use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use tinybrowser_dom::{DomTree, NodeData, NodeId};
use tinybrowser_dom::tree::{AttachShadowError, ShadowRootMode};
#[cfg(feature = "stealth")]
use tinybrowser_net::StealthHttpClient;
use tinybrowser_net::{
    CallbackRegistry, CookieJar, HttpClient, RequestInfo, ResourceType, Response,
};
use tokio::sync::Mutex;

use crate::import_map::ImportMap;

pub type InterceptCallback = Arc<
    Mutex<
        Option<Box<dyn Fn(String, String, String) -> Option<(u16, String, String)> + Send + Sync>>,
    >,
>;

#[derive(Debug)]
pub enum InterceptResolution {
    Continue {
        url: Option<String>,
        method: Option<String>,
        headers: Option<HashMap<String, String>>,
        body: Option<String>,
    },
    Fulfill {
        status: u16,
        headers: HashMap<String, String>,
        body: String,
    },
    Fail {
        reason: String,
    },
}

pub struct InterceptedRequest {
    pub request_id: String,
    pub url: String,
    pub method: String,
    pub headers: HashMap<String, String>,
    pub resource_type: String,
    pub resolver: tokio::sync::oneshot::Sender<InterceptResolution>,
}

#[derive(Debug, Clone)]
pub struct StoredNetworkResponseBody {
    pub body: String,
    pub base64_encoded: bool,
}

/// A network request made from page JS (fetch()/XHR/dynamic resource) recorded
/// so the CDP layer can emit Network.requestWillBeSent / responseReceived for
/// it. Static navigation subresources go through Page::record_network_event;
/// this is the parallel channel for script-initiated requests, which run in the
/// V8 op layer and would otherwise never surface as CDP Network events (#406).
#[derive(Debug, Clone)]
pub struct JsNetworkEvent {
    /// Matches the `fetch-{N}` id under which the body is stored, so CDP
    /// Network.getResponseBody resolves for the same request.
    pub request_id: String,
    pub url: String,
    pub method: String,
    pub status: u16,
    pub response_headers: HashMap<String, String>,
    pub body_size: usize,
    pub timestamp: f64,
}

pub struct RuntimeState {
    pub dom: Option<DomTree>,
    pub url: String,
    /// WHATWG canonical name of the document's character encoding (e.g.
    /// "UTF-8", "EUC-JP"). Backs `document.characterSet` and the URL query
    /// encoding override for `<a>`/`<area>` hrefs in legacy-charset documents.
    pub encoding: String,
    pub title: String,
    /// URL of the document that initiated this document's navigation. Direct
    /// browser/API navigations leave this empty; document-initiated
    /// navigations set it to the source document URL.
    pub referrer: String,
    pub blocked_urls: Vec<String>,
    pub cookie_jar: Option<Arc<CookieJar>>,
    pub http_client: Option<Arc<HttpClient>>,
    /// The owning page's passive on_request/on_response callbacks (issue
    /// #408). Page-scoped, so scripted fetch()/XHR observation stays local to
    /// the page that registered it.
    pub callbacks: Option<Arc<CallbackRegistry>>,
    /// When set (stealth mode), scripted fetch()/XHR is routed through the wreq
    /// client so the request carries the Chrome TLS fingerprint and client
    /// hints instead of the rustls ClientHello op_fetch_url would otherwise send.
    #[cfg(feature = "stealth")]
    pub stealth_client: Option<Arc<StealthHttpClient>>,
    pub pending_navigation: Option<(String, String, String)>,
    pub intercept_tx: Option<tokio::sync::mpsc::UnboundedSender<InterceptedRequest>>,
    pub intercept_counter: u64,
    pub intercept_enabled: bool,
    // Queue of (binding_name, payload) calls made by page JS via the
    // `op_binding_called` op. Drained by the CDP layer after each dispatch
    // and emitted as `Runtime.bindingCalled` events.
    pub pending_binding_calls: Vec<(String, String)>,
    pub network_response_bodies: HashMap<String, StoredNetworkResponseBody>,
    pub network_response_body_order: VecDeque<String>,
    pub network_response_body_counter: u64,
    // Absolute URLs requested via JS fetch() / XHR (op_fetch_url), in request
    // order. Surfaced by `--dump assets` so resources pulled in by script, not
    // just static DOM attributes, are listed (issue #301).
    pub fetched_urls: Vec<String>,
    // Network events for script-initiated requests (fetch/XHR/dynamic resource),
    // drained by the Page into its network_events so the CDP layer emits
    // Network.requestWillBeSent / responseReceived for them (issue #406).
    pub js_network_events: Vec<JsNetworkEvent>,
    /// Requests initiated by this runtime only. Browser contexts share their
    /// transport client across pages, so the client's aggregate counter cannot
    /// be used as a page-readiness signal.
    pub page_in_flight: Arc<std::sync::atomic::AtomicU32>,
    /// Monotonic generation for observable changes to the connected document.
    /// The browser settle policy samples this to distinguish useful deferred
    /// rendering work from unrelated long-lived timers.
    pub activity_generation: u64,
    /// Monotonic identity of the currently installed document. Async resource
    /// completions use this to discard bytes and lifecycle results belonging
    /// to a navigation that has already been replaced.
    pub document_generation: u64,
    /// Window-global import-map state shared by parser-discovered scripts,
    /// dynamically inserted import maps, and the module loader.
    pub(crate) import_map: Rc<RefCell<ImportMap>>,
    /// HTML's per-script "already started" flag.  This is native page state,
    /// rather than wrapper state, because it must survive moves and clones and
    /// because fragment parsing can create nodes before a JS wrapper exists.
    pub(crate) already_started_scripts: RefCell<HashSet<NodeId>>,
}

impl RuntimeState {
    pub fn new() -> Self {
        RuntimeState {
            dom: None,
            url: "about:blank".to_string(),
            encoding: "UTF-8".to_string(),
            title: String::new(),
            referrer: String::new(),
            blocked_urls: Vec::new(),
            cookie_jar: None,
            http_client: None,
            callbacks: None,
            #[cfg(feature = "stealth")]
            stealth_client: None,
            pending_navigation: None,
            intercept_tx: None,
            intercept_counter: 0,
            intercept_enabled: false,
            pending_binding_calls: Vec::new(),
            network_response_bodies: HashMap::new(),
            network_response_body_order: VecDeque::new(),
            network_response_body_counter: 0,
            fetched_urls: Vec::new(),
            js_network_events: Vec::new(),
            page_in_flight: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            activity_generation: 0,
            document_generation: 0,
            import_map: Rc::new(RefCell::new(ImportMap::default())),
            already_started_scripts: RefCell::new(HashSet::new()),
        }
    }
}

pub(crate) fn node_is_script(dom: &DomTree, node_id: NodeId) -> bool {
    dom.with_node(node_id, |node| {
        node.as_element()
            .map(|name| name.local.as_ref().eq_ignore_ascii_case("script"))
            .unwrap_or(false)
    })
    .unwrap_or(false)
}

fn script_nodes_including_template_contents(dom: &DomTree, root: NodeId) -> Vec<NodeId> {
    let mut scripts = Vec::new();
    let mut stack = vec![root];
    while let Some(node_id) = stack.pop() {
        if node_is_script(dom, node_id) {
            scripts.push(node_id);
        }
        let template_contents = dom
            .with_node(node_id, |node| match &node.data {
                NodeData::Element {
                    template_contents, ..
                } => *template_contents,
                _ => None,
            })
            .flatten();
        if let Some(contents) = template_contents {
            stack.push(contents);
        }
        let children = dom.children(node_id);
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    scripts
}

pub(crate) fn mark_script_subtree_started(state: &RuntimeState, root: NodeId) {
    let Some(dom) = state.dom.as_ref() else {
        return;
    };
    let scripts = script_nodes_including_template_contents(dom, root);
    state.already_started_scripts.borrow_mut().extend(scripts);
}

fn propagate_script_start_state(
    dom: &DomTree,
    source_root: NodeId,
    cloned_root: NodeId,
    started: &RefCell<HashSet<NodeId>>,
) {
    let mut pairs = vec![(source_root, cloned_root)];
    let mut additions = Vec::new();
    let current = started.borrow();
    while let Some((source, cloned)) = pairs.pop() {
        if current.contains(&source) {
            additions.push(cloned);
        }

        let source_template = dom
            .with_node(source, |node| match &node.data {
                NodeData::Element {
                    template_contents, ..
                } => *template_contents,
                _ => None,
            })
            .flatten();
        let cloned_template = dom
            .with_node(cloned, |node| match &node.data {
                NodeData::Element {
                    template_contents, ..
                } => *template_contents,
                _ => None,
            })
            .flatten();
        if let (Some(source_contents), Some(cloned_contents)) = (source_template, cloned_template) {
            pairs.push((source_contents, cloned_contents));
        }

        let source_children = dom.children(source);
        let cloned_children = dom.children(cloned);
        for pair in source_children.into_iter().zip(cloned_children).rev() {
            pairs.push(pair);
        }
    }
    drop(current);
    started.borrow_mut().extend(additions);
}

fn response_body_entry_limit() -> usize {
    std::env::var("TINYBROWSER_NETWORK_BODY_BUFFER_ENTRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(128)
}

fn response_body_byte_limit() -> usize {
    std::env::var("TINYBROWSER_NETWORK_BODY_BUFFER_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2 * 1024 * 1024)
}

pub type SharedState = Rc<RefCell<RuntimeState>>;

#[derive(Clone, Copy, Debug, Default)]
struct RenderMutationImpact {
    connected: bool,
    actual_change: bool,
}

fn node_is_connected(dom: &DomTree, node: NodeId) -> bool {
    dom.is_connected(node)
}

/// Classify whether a DOM command can make the retained document layout
/// stale. DOM construction is commonly performed in detached subtrees, and
/// frameworks also assign an attribute its current value. Neither operation
/// changes the rendered document. Chromium dirties layout when the mutation
/// reaches a connected style/layout owner, not merely because a mutating API
/// was entered.
fn render_mutation_impact(
    dom: &DomTree,
    cmd: &str,
    arg1: &str,
    arg2: &str,
) -> RenderMutationImpact {
    let node = |value: &str| value.parse::<u32>().ok().map(NodeId::new);
    match cmd {
        "set_attribute" => {
            let Some(target) = node(arg1) else {
                return RenderMutationImpact::default();
            };
            let Some((name, value)) = arg2.split_once('\0') else {
                return RenderMutationImpact::default();
            };
            let old = dom
                .with_node(target, |node| node.get_attribute(name).map(str::to_owned))
                .flatten();
            RenderMutationImpact {
                connected: node_is_connected(dom, target),
                actual_change: old.as_deref() != Some(value),
            }
        }
        "set_attribute_ns" => {
            let Some(target) = node(arg1) else {
                return RenderMutationImpact::default();
            };
            let mut parts = arg2.splitn(3, '\0');
            let namespace = parts.next().unwrap_or("");
            let qualified = parts.next().unwrap_or("");
            let value = parts.next().unwrap_or("");
            let local = qualified
                .split_once(':')
                .map(|(_, local)| local)
                .unwrap_or(qualified);
            let old = dom
                .with_node(target, |node| {
                    node.get_attribute_ns(namespace, local).map(str::to_owned)
                })
                .flatten();
            RenderMutationImpact {
                connected: node_is_connected(dom, target),
                actual_change: old.as_deref() != Some(value),
            }
        }
        "remove_attribute" => {
            let Some(target) = node(arg1) else {
                return RenderMutationImpact::default();
            };
            let existed = dom
                .with_node(target, |node| node.get_attribute(arg2).is_some())
                .unwrap_or(false);
            RenderMutationImpact {
                connected: node_is_connected(dom, target),
                actual_change: existed,
            }
        }
        "remove_attribute_ns" => {
            let Some(target) = node(arg1) else {
                return RenderMutationImpact::default();
            };
            let (namespace, local) = arg2.split_once('\0').unwrap_or(("", arg2));
            let existed = dom
                .with_node(target, |node| {
                    node.get_attribute_ns(namespace, local).is_some()
                })
                .unwrap_or(false);
            RenderMutationImpact {
                connected: node_is_connected(dom, target),
                actual_change: existed,
            }
        }
        "append_child" => {
            let (Some(parent), Some(child)) = (node(arg1), node(arg2)) else {
                return RenderMutationImpact::default();
            };
            if dom.get_node(parent).is_none() || dom.get_node(child).is_none() {
                return RenderMutationImpact::default();
            }
            let old_parent = dom.get_node(child).and_then(|node| node.parent);
            let already_last =
                old_parent == Some(parent) && dom.children(parent).last().copied() == Some(child);
            RenderMutationImpact {
                // Moving a connected node into a detached subtree removes its
                // old box, while attaching a detached node creates a new one.
                connected: node_is_connected(dom, parent) || node_is_connected(dom, child),
                actual_change: !already_last,
            }
        }
        "remove_child" => {
            let Some(child) = node(arg1) else {
                return RenderMutationImpact::default();
            };
            RenderMutationImpact {
                connected: node_is_connected(dom, child),
                actual_change: dom.get_node(child).and_then(|node| node.parent).is_some(),
            }
        }
        "insert_before" => {
            let (Some(new_node), Some(reference)) = (node(arg1), node(arg2)) else {
                return RenderMutationImpact::default();
            };
            if dom.get_node(new_node).is_none() {
                return RenderMutationImpact::default();
            }
            let Some(reference_parent) = dom.get_node(reference).and_then(|node| node.parent)
            else {
                return RenderMutationImpact::default();
            };
            let new_was_connected = node_is_connected(dom, new_node);
            let already_immediately_before =
                dom.get_node(reference).and_then(|node| node.prev_sibling) == Some(new_node);
            RenderMutationImpact {
                connected: node_is_connected(dom, reference_parent) || new_was_connected,
                actual_change: new_node != reference && !already_immediately_before,
            }
        }
        "set_inner_html" | "set_inner_html_context" => {
            let Some(target) = node(arg1) else {
                return RenderMutationImpact::default();
            };
            RenderMutationImpact {
                connected: node_is_connected(dom, target),
                // Parsing normalizes source text, so a cheap string comparison
                // cannot prove equality. Connected replacement remains dirty.
                actual_change: dom.get_node(target).is_some(),
            }
        }
        "set_text_content" => {
            let Some(target) = node(arg1) else {
                return RenderMutationImpact::default();
            };
            let changed = dom
                .with_node(target, |node| match &node.data {
                    NodeData::Text { contents } | NodeData::Comment { contents } => {
                        contents.as_str() != arg2
                    }
                    NodeData::ProcessingInstruction { data, .. } => data.as_str() != arg2,
                    // Element/DocumentFragment textContent replaces their
                    // child structure, which can change style even when the
                    // flattened text is equal (for example `<b>x</b>` -> `x`).
                    _ => {
                        let children = dom.children(target);
                        match children.as_slice() {
                            [] => !arg2.is_empty(),
                            [child] => dom
                                .with_node(*child, |child| match &child.data {
                                    NodeData::Text { contents } => contents.as_str() != arg2,
                                    _ => true,
                                })
                                .unwrap_or(true),
                            _ => true,
                        }
                    }
                })
                .unwrap_or(false);
            RenderMutationImpact {
                connected: node_is_connected(dom, target),
                actual_change: changed,
            }
        }
        _ => RenderMutationImpact::default(),
    }
}

fn is_mutating_dom_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "set_attribute"
            | "remove_attribute"
            | "set_attribute_ns"
            | "remove_attribute_ns"
            | "append_child"
            | "remove_child"
            | "insert_before"
            | "set_inner_html"
            | "set_inner_html_context"
            | "set_text_content"
            | "set_fragment_html_executable"
    )
}

fn fragment_context_and_html(arg: &str) -> (html5ever::QualName, &str) {
    let mut parts = arg.splitn(3, '\0');
    let first = parts.next().unwrap_or("body");
    let second = parts.next();
    let third = parts.next();
    let (namespace, qualified, html) = match (second, third) {
        // Namespace-aware encoding used by the current bootstrap.
        (Some(qualified), Some(html)) => (first, qualified, html),
        // Backward-compatible encoding for older snapshots: `local\0html`.
        (Some(html), None) => ("http://www.w3.org/1999/xhtml", first, html),
        (None, None) => ("http://www.w3.org/1999/xhtml", "body", first),
        (None, Some(_)) => unreachable!(),
    };
    let (prefix, local) = match qualified.split_once(':') {
        Some((prefix, local)) if !prefix.is_empty() && !local.is_empty() => {
            (Some(html5ever::Prefix::from(prefix)), local)
        }
        _ => (None, if qualified.is_empty() { "body" } else { qualified }),
    };
    (
        html5ever::QualName::new(
            prefix,
            html5ever::Namespace::from(namespace),
            html5ever::LocalName::from(local),
        ),
        html,
    )
}
pub(crate) fn op_script_mark_started_inner(shared: &SharedState, nid: u32) -> bool {
    let state = shared.borrow();
    let Some(dom) = state.dom.as_ref() else {
        return false;
    };
    let node_id = NodeId::new(nid);
    if !node_is_script(dom, node_id) {
        return false;
    }
    state.already_started_scripts.borrow_mut().insert(node_id);
    true
}

/// Atomically claim an executable script.  A false result means the node was
/// created inert by an HTML-string API or has already been prepared once.
pub(crate) fn op_script_try_start_inner(shared: &SharedState, nid: u32) -> bool {
    let state = shared.borrow();
    let Some(dom) = state.dom.as_ref() else {
        return false;
    };
    let node_id = NodeId::new(nid);
    if !node_is_script(dom, node_id) {
        return false;
    }
    let newly_started = state.already_started_scripts.borrow_mut().insert(node_id);
    newly_started
}

/// Attach one native shadow-tree scope without making it part of the light
/// tree. Layout intentionally remains unaware of the detached root until
/// scoped style, slot assignment, and composed-tree paint are implemented.
pub(crate) fn op_shadow_attach_inner(shared: &SharedState, host_nid: u32, mode: String) -> i32 {
    let mode = match mode.as_str() {
        "open" => ShadowRootMode::Open,
        "closed" => ShadowRootMode::Closed,
        _ => return -1,
    };
    let state = shared.borrow();
    let Some(dom) = state.dom.as_ref() else {
        return -1;
    };
    match dom.attach_shadow_root(NodeId::new(host_nid), mode) {
        Ok(root) => root.raw() as i32,
        Err(AttachShadowError::HostAlreadyHasShadowRoot) => -2,
        Err(_) => -1,
    }
}

/// Return native host-owned shadow identity as `root-id\0mode`. Closed roots
/// are included here; the Web-facing `Element.shadowRoot` getter applies mode
/// visibility in bootstrap.js.
pub(crate) fn op_shadow_root_info_inner(shared: &SharedState, host_nid: u32) -> String {
    let state = shared.borrow();
    let Some(dom) = state.dom.as_ref() else {
        return String::new();
    };
    dom.shadow_root(NodeId::new(host_nid))
        .and_then(|root| dom.shadow_root_info(root))
        .map(|shadow| {
            let mode = match shadow.mode {
                ShadowRootMode::Open => "open",
                ShadowRootMode::Closed => "closed",
            };
            format!("{}\0{mode}", shadow.id.raw())
        })
        .unwrap_or_default()
}
/// Read-only parent/sibling walks used by MutationObserver ancestor checks.
/// Returns the neighbor node id, or -1 when the edge is empty. `None` means
/// `cmd` is not a tree-edge read.
pub(crate) fn op_dom_tree_query(shared: &SharedState, cmd: &str, nid: u32) -> Option<i32> {
    if !matches!(
        cmd,
        "parent_node" | "first_child" | "last_child" | "next_sibling" | "prev_sibling"
    ) {
        return None;
    }
    let gs = shared.borrow();
    let Some(dom) = gs.dom.as_ref() else {
        return Some(-1);
    };
    let id = dom
        .with_node(NodeId::new(nid), |n| match cmd {
            "parent_node" => n.parent,
            "first_child" => n.first_child,
            "last_child" => n.last_child,
            "next_sibling" => n.next_sibling,
            "prev_sibling" => n.prev_sibling,
            _ => None,
        })
        .flatten();
    Some(id.map(|id| id.index() as i32).unwrap_or(-1))
}

pub(crate) fn op_dom_inner(
    shared: &SharedState,
    cmd: String,
    arg1: String,
    arg2: String,
) -> String {
    if let Some(fast) = arg1
        .parse::<u32>()
        .ok()
        .and_then(|nid| op_dom_tree_query(shared, cmd.as_str(), nid))
    {
        return fast.to_string();
    }
    if is_mutating_dom_command(cmd.as_str()) {
        // Scroll offsets belong to a node at its current tree position.
        // Temporary box/style loss keeps that latent state, but DOM removal,
        // reparenting, and subtree replacement reset the affected identities,
        // matching Chromium's lifecycle behavior.
        // Any changed attribute on a connected node can participate in an
        // author selector. Detached subtree construction, failed operations,
        // and no-op value assignments cannot change live layout and preserve
        // the prepared render. The next relevant mutation invalidates once;
        // subsequent writes are coalesced until geometry is read again.
        let mut state = shared.borrow_mut();
        let impact = state
            .dom
            .as_ref()
            .map(|dom| render_mutation_impact(dom, &cmd, &arg1, &arg2))
            .unwrap_or_default();
        let invalidate = impact.connected && impact.actual_change;
        if invalidate {
            state.activity_generation = state.activity_generation.wrapping_add(1);
        }
    }
    let gs = shared.borrow();
    let dom = match &gs.dom {
        Some(d) => d,
        None => return "null".to_string(),
    };

    match cmd.as_str() {
        "document_node_id" => dom.document().index().to_string(),
        "document_title" => {
            // The DOM is authoritative after parsing. In particular, script
            // changes through title.textContent must be reflected by
            // document.title, not hidden behind the navigation-time snapshot.
            let title = dom
                .query_selector("title")
                .ok()
                .flatten()
                .map(|title_id| {
                    dom.text_content(title_id)
                        .split(|ch| matches!(ch, '\t' | '\n' | '\u{000C}' | '\r' | ' '))
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            serde_json::to_string(&title).unwrap_or("\"\"".into())
        }
        "document_url" => serde_json::to_string(&gs.url).unwrap_or("\"\"".into()),
        "document_referrer" => serde_json::to_string(&gs.referrer).unwrap_or("\"\"".into()),
        "document_encoding" => serde_json::to_string(&gs.encoding).unwrap_or("\"UTF-8\"".into()),
        "document_element" => {
            for cid in dom.children(dom.document()) {
                if let Some(n) = dom.get_node(cid) {
                    if n.as_element()
                        .map(|name| name.local.as_ref() == "html")
                        .unwrap_or(false)
                    {
                        return cid.index().to_string();
                    }
                }
            }
            "-1".into()
        }
        "document_doctype" => {
            for cid in dom.children(dom.document()) {
                if let Some(n) = dom.get_node(cid) {
                    if let tinybrowser_dom::NodeData::Doctype {
                        name,
                        public_id,
                        system_id,
                    } = &n.data
                    {
                        return serde_json::json!({
                            "name": name,
                            "publicId": public_id,
                            "systemId": system_id,
                            "nodeId": cid.index(),
                        })
                        .to_string();
                    }
                }
            }
            "null".into()
        }
        "get_element_by_id" => {
            // Verify the indexed node is in the live document. The id_index is best-effort:
            // it only registers nodes at creation time and doesn't update on reparent, so
            // it can point to a detached clone while the live node is elsewhere in the tree.
            let doc = dom.document();
            let nid = dom.get_element_by_id(&arg1);
            let live = nid.filter(|&n| dom.ancestors(n).contains(&doc));
            match live {
                Some(n) => n.index().to_string(),
                None => {
                    // Fall back to full scan for the live document.
                    let sel = format!(
                        "[id=\"{}\"]",
                        arg1.replace('\\', "\\\\").replace('"', "\\\"")
                    );
                    dom.query_selector(&sel)
                        .ok()
                        .flatten()
                        .map(|id| id.index().to_string())
                        .unwrap_or("-1".into())
                }
            }
        }
        "query_selector" => dom
            .query_selector(&arg1)
            .ok()
            .flatten()
            .map(|id| id.index().to_string())
            .unwrap_or("-1".into()),
        "query_selector_all" => {
            let ids: Vec<i32> = dom
                .query_selector_all(&arg1)
                .ok()
                .map(|ids| ids.iter().map(|id| id.index() as i32).collect())
                .unwrap_or_default();
            serde_json::to_string(&ids).unwrap_or("[]".into())
        }
        "query_selector_scoped" => {
            let root_nid = arg1.parse::<u32>().unwrap_or(0);
            dom.query_selector_from(NodeId::new(root_nid), &arg2)
                .ok()
                .flatten()
                .map(|id| id.index().to_string())
                .unwrap_or("-1".into())
        }
        "query_selector_all_scoped" => {
            let root_nid = arg1.parse::<u32>().unwrap_or(0);
            let ids: Vec<i32> = dom
                .query_selector_all_from(NodeId::new(root_nid), &arg2)
                .ok()
                .map(|ids| ids.iter().map(|id| id.index() as i32).collect())
                .unwrap_or_default();
            serde_json::to_string(&ids).unwrap_or("[]".into())
        }
        "matches_selector" => {
            let nid = NodeId::new(arg1.parse::<u32>().unwrap_or(0));
            dom.matches_selector(nid, &arg2)
                .unwrap_or(false)
                .to_string()
        }
        "node_type" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.with_node(NodeId::new(nid), |n| match &n.data {
                NodeData::Document => "9",
                NodeData::Element { .. } => "1",
                NodeData::Text { .. } => "3",
                NodeData::Comment { .. } => "8",
                NodeData::Doctype { .. } => "10",
                NodeData::ProcessingInstruction { .. } => "7",
            })
            .unwrap_or("0")
            .into()
        }
        "node_name" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let name: String = dom
                .with_node(NodeId::new(nid), |n| match &n.data {
                    NodeData::Document => "#document".to_string(),
                    NodeData::Element { name, .. } => name.local.as_ref().to_ascii_uppercase(),
                    NodeData::Text { .. } => "#text".to_string(),
                    NodeData::Comment { .. } => "#comment".to_string(),
                    NodeData::Doctype { name, .. } => name.clone(),
                    NodeData::ProcessingInstruction { target, .. } => target.clone(),
                })
                .unwrap_or_default();
            serde_json::to_string(&name).unwrap_or("\"\"".into())
        }
        "text_content" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            serde_json::to_string(&dom.text_content(NodeId::new(nid))).unwrap_or("\"\"".into())
        }
        "parent_node" | "first_child" | "last_child" | "next_sibling" | "prev_sibling" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.with_node(NodeId::new(nid), |n| match cmd.as_str() {
                "parent_node" => n.parent,
                "first_child" => n.first_child,
                "last_child" => n.last_child,
                "next_sibling" => n.next_sibling,
                "prev_sibling" => n.prev_sibling,
                _ => None,
            })
            .flatten()
            .map(|id| id.index().to_string())
            .unwrap_or("-1".into())
        }
        "next_in_subtree" => {
            let root = NodeId::new(arg1.parse::<u32>().unwrap_or(0));
            let current = NodeId::new(arg2.parse::<u32>().unwrap_or(0));
            dom.next_in_subtree(root, current)
                .map(|id| id.index().to_string())
                .unwrap_or("-1".into())
        }
        // Reverse document order within a subtree, for NodeIterator's backward
        // walk (which prunes nothing, so the whole step fits in the DOM layer).
        "prev_in_subtree" => {
            let root = NodeId::new(arg1.parse::<u32>().unwrap_or(0));
            let current = NodeId::new(arg2.parse::<u32>().unwrap_or(0));
            dom.prev_in_subtree(root, current)
                .map(|id| id.index().to_string())
                .unwrap_or("-1".into())
        }
        // Step past a whole subtree rather than into it: NodeFilter.FILTER_REJECT
        // prunes the rejected node's descendants, unlike FILTER_SKIP.
        "next_after_subtree" => {
            let root = NodeId::new(arg1.parse::<u32>().unwrap_or(0));
            let current = NodeId::new(arg2.parse::<u32>().unwrap_or(0));
            dom.next_after_subtree(root, current)
                .map(|id| id.index().to_string())
                .unwrap_or("-1".into())
        }
        "child_nodes" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let ids: Vec<i32> = dom
                .children(NodeId::new(nid))
                .iter()
                .map(|id| id.index() as i32)
                .collect();
            serde_json::to_string(&ids).unwrap_or("[]".into())
        }
        "tag_name" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let name = dom
                .with_node(NodeId::new(nid), |n| {
                    n.as_element().map(|name| {
                        if name.ns == html5ever::ns!(html) {
                            name.local.as_ref().to_ascii_uppercase()
                        } else {
                            match &name.prefix {
                                Some(prefix) => format!("{}:{}", prefix, name.local),
                                None => name.local.to_string(),
                            }
                        }
                    })
                })
                .flatten()
                .unwrap_or_default();
            serde_json::to_string(&name).unwrap_or("\"\"".into())
        }
        "local_name" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let name = dom
                .with_node(NodeId::new(nid), |n| {
                    n.as_element().map(|name| name.local.to_string())
                })
                .flatten()
                .unwrap_or_default();
            serde_json::to_string(&name).unwrap_or("\"\"".into())
        }
        // The tree builder already assigns foreign content (an <svg>/<math>
        // subtree) its own namespace; expose it so JS does not have to guess
        // the namespace from the tag name.
        "namespace_uri" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let ns = dom
                .with_node(NodeId::new(nid), |n| {
                    n.as_element().map(|name| name.ns.as_ref().to_string())
                })
                .flatten()
                .unwrap_or_default();
            serde_json::to_string(&ns).unwrap_or("\"\"".into())
        }
        "get_attribute" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let val = dom
                .with_node(NodeId::new(nid), |n| {
                    n.get_attribute(&arg2).map(|s| s.to_string())
                })
                .flatten();
            serde_json::to_string(&val).unwrap_or("null".into())
        }
        "attribute_names" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let names: Vec<String> = dom
                .with_node(NodeId::new(nid), |n| {
                    n.attrs()
                        .map(|a| a.iter().map(|x| x.qualified_name()).collect())
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            serde_json::to_string(&names).unwrap_or("[]".into())
        }
        "set_attribute" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let node_id = NodeId::new(nid);
            if let Some((name, value)) = arg2.split_once('\0') {
                if name == "id" {
                    let old_id = dom
                        .with_node(node_id, |n| n.get_attribute("id").map(|s| s.to_string()))
                        .flatten();
                    dom.with_node_mut(node_id, |n| n.set_attribute(name, value.to_string()));
                    dom.update_id_index(node_id, old_id.as_deref(), Some(value));
                } else {
                    dom.with_node_mut(node_id, |n| n.set_attribute(name, value.to_string()));
                }
            }
            "true".into()
        }
        "inner_html" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            serde_json::to_string(&dom.inner_html(NodeId::new(nid))).unwrap_or("\"\"".into())
        }
        "outer_html" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            serde_json::to_string(&dom.outer_html(NodeId::new(nid))).unwrap_or("\"\"".into())
        }
        "append_child" => {
            // Reject if either nid failed to parse (was "undefined"/empty) — those
            // default to 0 which is the document root, and silently operating on it
            // corrupts the tree. Require both args to be valid positive integers.
            let parent = match arg1.parse::<u32>() {
                Ok(n) => n,
                Err(_) => return "false".into(),
            };
            let child = match arg2.parse::<u32>() {
                Ok(n) => n,
                Err(_) => return "false".into(),
            };
            let parent = NodeId::new(parent);
            let child = NodeId::new(child);
            dom.append_child(parent, child);
            (dom.get_node(child).and_then(|node| node.parent) == Some(parent)).to_string()
        }
        "remove_child" => {
            let child = match arg1.parse::<u32>() {
                Ok(n) => n,
                Err(_) => return "false".into(),
            };
            let child = NodeId::new(child);
            let had_parent = dom.get_node(child).is_some_and(|node| node.parent.is_some());
            dom.remove_child(child);
            (had_parent && dom.get_node(child).is_some_and(|node| node.parent.is_none())).to_string()
        }
        "insert_before" => {
            let new_node = match arg1.parse::<u32>() {
                Ok(n) => n,
                Err(_) => return "false".into(),
            };
            let ref_node = match arg2.parse::<u32>() {
                Ok(n) => n,
                Err(_) => return "false".into(),
            };
            let ref_node = NodeId::new(ref_node);
            let new_node = NodeId::new(new_node);
            let expected_parent = dom.get_node(ref_node).and_then(|node| node.parent);
            dom.insert_before(ref_node, new_node);
            (expected_parent.is_some()
                && dom.get_node(new_node).and_then(|node| node.parent) == expected_parent)
                .to_string()
        }
        "remove_attribute" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.with_node_mut(NodeId::new(nid), |n| {
                if let NodeData::Element { attrs, .. } = &mut n.data {
                    attrs.retain(|a| !a.qualified_name_eq(&arg2));
                }
            });
            "true".into()
        }
        // Namespace-aware attribute ops. arg2 packs the pieces with a NUL:
        //   get/remove: "<namespace>\0<localName>"
        //   set:        "<namespace>\0<qualifiedName>\0<value>"
        "get_attribute_ns" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let (ns, local) = arg2.split_once('\0').unwrap_or(("", arg2.as_str()));
            let val = dom
                .with_node(NodeId::new(nid), |n| n.get_attribute_ns(ns, local).map(|s| s.to_string()))
                .flatten();
            serde_json::to_string(&val).unwrap_or("null".into())
        }
        "set_attribute_ns" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let node_id = NodeId::new(nid);
            let mut parts = arg2.splitn(3, '\0');
            let ns = parts.next().unwrap_or("");
            let qualified = parts.next().unwrap_or("");
            let value = parts.next().unwrap_or("");
            if !qualified.is_empty() {
                let local = qualified
                    .split_once(':')
                    .map(|(_, local)| local)
                    .unwrap_or(qualified);
                if ns.is_empty() && local == "id" {
                    let old_id = dom
                        .with_node(node_id, |n| n.get_attribute("id").map(str::to_owned))
                        .flatten();
                    dom.with_node_mut(node_id, |n| {
                        n.set_attribute_ns(ns, qualified, value.to_string())
                    });
                    dom.update_id_index(node_id, old_id.as_deref(), Some(value));
                } else {
                    dom.with_node_mut(node_id, |n| {
                        n.set_attribute_ns(ns, qualified, value.to_string())
                    });
                }
            }
            "true".into()
        }
        "remove_attribute_ns" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let node_id = NodeId::new(nid);
            let (ns, local) = arg2.split_once('\0').unwrap_or(("", arg2.as_str()));
            if ns.is_empty() && local == "id" {
                let old_id = dom
                    .with_node(node_id, |n| n.get_attribute("id").map(str::to_owned))
                    .flatten();
                dom.with_node_mut(node_id, |n| n.remove_attribute_ns(ns, local));
                dom.update_id_index(node_id, old_id.as_deref(), None);
            } else {
                dom.with_node_mut(node_id, |n| n.remove_attribute_ns(ns, local));
            }
            "true".into()
        }
        "set_inner_html" => {
            let nid = match arg1.parse::<u32>() {
                Ok(n) if n > 0 => n,
                // nid=0 is the document root; never allow innerHTML to clear it.
                // nid parse failure (e.g. "undefined") also falls here.
                _ => return "false".into(),
            };
            let target = NodeId::new(nid);
            let children = dom.children(target);
            for child in children {
                dom.detach(child);
            }
            if !arg2.is_empty() {
                let context_name = dom
                    .with_node(target, |node| match &node.data {
                        NodeData::Element { name, .. } => Some(name.clone()),
                        _ => None,
                    })
                    .flatten();
                let fragment = match context_name {
                    Some(name) => tinybrowser_dom::parse_fragment_with_context(&arg2, name),
                    None => tinybrowser_dom::parse_fragment(&arg2),
                };
                let import_root = fragment.fragment_root();
                dom.import_children_from(target, &fragment, import_root);
                for child in dom.children(target) {
                    mark_script_subtree_started(&gs, child);
                }
            }
            "true".into()
        }
        "set_inner_html_context" => {
            let nid = match arg1.parse::<u32>() {
                Ok(n) if n > 0 => n,
                _ => return "false".into(),
            };
            let target = NodeId::new(nid);
            let (context_name, html) = fragment_context_and_html(&arg2);
            for child in dom.children(target) {
                dom.detach(child);
            }
            if !html.is_empty() {
                let fragment = tinybrowser_dom::parse_fragment_with_context(html, context_name);
                let import_root = fragment.fragment_root();
                dom.import_children_from(target, &fragment, import_root);
                for child in dom.children(target) {
                    mark_script_subtree_started(&gs, child);
                }
            }
            "true".into()
        }
        // Range.createContextualFragment has a deliberately different script
        // policy from innerHTML: scripts remain eligible and are prepared when
        // the returned fragment is inserted into a connected document.
        "set_fragment_html_executable" => {
            let nid = match arg1.parse::<u32>() {
                Ok(n) if n > 0 => n,
                _ => return "false".into(),
            };
            let target = NodeId::new(nid);
            let (context_name, html) = fragment_context_and_html(&arg2);
            for child in dom.children(target) {
                dom.detach(child);
            }
            if !html.is_empty() {
                let fragment = tinybrowser_dom::parse_fragment_with_context(html, context_name);
                let import_root = fragment.fragment_root();
                dom.import_children_from(target, &fragment, import_root);
            }
            "true".into()
        }
        "set_text_content" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.with_node_mut(NodeId::new(nid), |n| match &mut n.data {
                NodeData::Text { contents } => {
                    *contents = arg2.clone();
                }
                NodeData::Comment { contents } => {
                    *contents = arg2.clone();
                }
                NodeData::ProcessingInstruction { data, .. } => {
                    *data = arg2.clone();
                }
                _ => {}
            });
            "true".into()
        }
        // A <template>'s children live in a separate contents document, so this
        // is the only route to them from JS. Allocates one on demand for
        // templates built via createElement.
        "template_contents" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.template_contents(NodeId::new(nid))
                .map(|id| id.index().to_string())
                .unwrap_or("-1".into())
        }
        "create_document_fragment" => dom.new_node(NodeData::Document).index().to_string(),
        "clone_node" => {
            let nid = match arg1.parse::<u32>() {
                Ok(n) => n,
                Err(_) => return "-1".into(),
            };
            let source = NodeId::new(nid);
            match dom.clone_node(source, arg2 == "true") {
                Some(cloned) => {
                    propagate_script_start_state(dom, source, cloned, &gs.already_started_scripts);
                    cloned.index().to_string()
                }
                None => "-1".into(),
            }
        }
        "create_element" => dom
            .new_node(NodeData::Element {
                name: html5ever::QualName::new(
                    None,
                    html5ever::ns!(html),
                    html5ever::LocalName::from(arg1.as_str()),
                ),
                attrs: vec![],
                template_contents: None,
                mathml_annotation_xml_integration_point: false,
            })
            .index()
            .to_string(),
        "create_element_ns" => {
            let (namespace, qualified) = arg1.split_once('\0').unwrap_or(("", arg1.as_str()));
            let (prefix, local) = match qualified.split_once(':') {
                Some((prefix, local)) if !prefix.is_empty() && !local.is_empty() => {
                    (Some(html5ever::Prefix::from(prefix)), local)
                }
                None if !qualified.is_empty() => (None, qualified),
                _ => return "-1".into(),
            };
            dom.new_node(NodeData::Element {
                name: html5ever::QualName::new(
                    prefix,
                    html5ever::Namespace::from(namespace),
                    html5ever::LocalName::from(local),
                ),
                attrs: vec![],
                template_contents: None,
                mathml_annotation_xml_integration_point: false,
            })
            .index()
            .to_string()
        }
        "create_text_node" => dom
            .new_node(NodeData::Text {
                contents: arg1.clone(),
            })
            .index()
            .to_string(),
        "create_comment_node" => dom
            .new_node(NodeData::Comment {
                contents: arg1.clone(),
            })
            .index()
            .to_string(),
        "create_processing_instruction" => {
            // arg1 = target, arg2 = data
            dom.new_node(NodeData::ProcessingInstruction {
                target: arg1.clone(),
                data: arg2.clone(),
            })
            .index()
            .to_string()
        }
        "create_doctype" => {
            // arg1 = name, arg2 = public_id. system_id stored only in the
            // JS wrapper since neither current WPT test reads it back from
            // the underlying tree.
            dom.new_node(NodeData::Doctype {
                name: arg1.clone(),
                public_id: arg2.clone(),
                system_id: String::new(),
            })
            .index()
            .to_string()
        }
        "pi_target" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let val = dom
                .with_node(NodeId::new(nid), |n| match &n.data {
                    NodeData::ProcessingInstruction { target, .. } => Some(target.clone()),
                    _ => None,
                })
                .flatten()
                .unwrap_or_default();
            serde_json::to_string(&val).unwrap_or("\"\"".into())
        }
        "doctype_name" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let val = dom
                .with_node(NodeId::new(nid), |n| match &n.data {
                    NodeData::Doctype { name, .. } => Some(name.clone()),
                    _ => None,
                })
                .flatten()
                .unwrap_or_default();
            serde_json::to_string(&val).unwrap_or("\"\"".into())
        }
        "doctype_public_id" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let val = dom
                .with_node(NodeId::new(nid), |n| match &n.data {
                    NodeData::Doctype { public_id, .. } => Some(public_id.clone()),
                    _ => None,
                })
                .flatten()
                .unwrap_or_default();
            serde_json::to_string(&val).unwrap_or("\"\"".into())
        }
        "element_children" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let ids: Vec<i32> = dom
                .children(NodeId::new(nid))
                .iter()
                .filter(|&&id| dom.get_node(id).map(|n| n.is_element()).unwrap_or(false))
                .map(|id| id.index() as i32)
                .collect();
            serde_json::to_string(&ids).unwrap_or("[]".into())
        }
        "has_child_nodes" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.with_node(NodeId::new(nid), |n| n.first_child.is_some())
                .unwrap_or(false)
                .to_string()
        }
        "contains" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            let other = arg2.parse::<u32>().unwrap_or(0);
            dom.descendants(NodeId::new(nid))
                .contains(&NodeId::new(other))
                .to_string()
        }
        // Connectivity is maintained incrementally by DomTree. Exposing the
        // cached bit avoids an ancestor op crossing for every level when JS
        // builds a deep detached subtree.
        "is_connected" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            dom.is_connected(NodeId::new(nid)).to_string()
        }
        // Index of a node among its parent's children. Walks prev siblings in
        // Rust, avoiding the per-step JS->op round trips a Range comparison
        // would otherwise make.
        "node_index" => {
            let nid = arg1.parse::<u32>().unwrap_or(0);
            node_child_index(dom, NodeId::new(nid)).to_string()
        }
        // Document (preorder) tree order of two nodes: -1 if a precedes b, 1 if
        // a follows b, 0 if equal. Used by the Range boundary-point algorithms.
        "compare_order" => {
            let a = NodeId::new(arg1.parse::<u32>().unwrap_or(0));
            let b = NodeId::new(arg2.parse::<u32>().unwrap_or(0));
            compare_node_order(dom, a, b).to_string()
        }
        // Root (topmost ancestor) of a node, in one op rather than an O(depth)
        // walk of parentNode ops from JS.
        "node_root" => {
            let mut cur = NodeId::new(arg1.parse::<u32>().unwrap_or(0));
            while let Some(p) = dom.with_node(cur, |x| x.parent).flatten() {
                cur = p;
            }
            cur.index().to_string()
        }
        // Inclusive ancestor test in one op so MutationObserver subtree
        // matching does not walk parentNode in JS (quadratic on deep trees).
        "is_inclusive_ancestor" => {
            let ancestor = NodeId::new(arg1.parse::<u32>().unwrap_or(0));
            let mut cur = NodeId::new(arg2.parse::<u32>().unwrap_or(0));
            let mut hops = 0u32;
            loop {
                if cur == ancestor {
                    break "true".into();
                }
                hops += 1;
                if hops > 1_000_000 {
                    break "false".into();
                }
                match dom.with_node(cur, |x| x.parent).flatten() {
                    Some(parent) => cur = parent,
                    None => break "false".into(),
                }
            }
        }
        _ => "null".into(),
    }
}

/// Index of `n` among its parent's children (0-based).
fn node_child_index(dom: &DomTree, n: NodeId) -> usize {
    let mut i = 0usize;
    let mut cur = dom.with_node(n, |x| x.prev_sibling).flatten();
    while let Some(p) = cur {
        i += 1;
        cur = dom.with_node(p, |x| x.prev_sibling).flatten();
    }
    i
}

/// Ancestor chain of `n` from the root down to `n` (root first).
fn node_ancestors_root_first(dom: &DomTree, n: NodeId) -> Vec<NodeId> {
    let mut v = vec![n];
    let mut cur = n;
    while let Some(p) = dom.with_node(cur, |x| x.parent).flatten() {
        v.push(p);
        cur = p;
    }
    v.reverse();
    v
}

/// Preorder (document) order comparison of two nodes: -1 before, 1 after, 0 same.
fn compare_node_order(dom: &DomTree, a: NodeId, b: NodeId) -> i32 {
    if a == b {
        return 0;
    }
    let aa = node_ancestors_root_first(dom, a);
    let bb = node_ancestors_root_first(dom, b);
    // Different roots: order is undefined per spec; keep it stable by node id.
    if aa[0] != bb[0] {
        return if a.index() < b.index() { -1 } else { 1 };
    }
    let mut i = 0usize;
    while i < aa.len() && i < bb.len() && aa[i] == bb[i] {
        i += 1;
    }
    if i >= aa.len() {
        return -1; // a is an ancestor of b -> a precedes
    }
    if i >= bb.len() {
        return 1; // b is an ancestor of a -> a follows
    }
    if node_child_index(dom, aa[i]) < node_child_index(dom, bb[i]) {
        -1
    } else {
        1
    }
}
// Fallback cache for runtimes that have no owning HttpClient, such as
// a standalone module loader. Browser pages use their context-scoped client
// below so sequential V8 runtimes never share an async network pool (#453).
static FETCH_CLIENT_CACHE: std::sync::OnceLock<
    std::sync::RwLock<std::collections::HashMap<String, reqwest::Client>>,
> = std::sync::OnceLock::new();

/// Shared HTTP client cache for any code in obscura-js that needs a
/// reqwest::Client (op_fetch_url for JS-side fetch/XHR, the ES module
/// loader for dynamic imports). Keyed by proxy URL ("" = direct).
/// One client per distinct proxy, reused for every request, so the
/// connection pool actually warms up.
pub fn cached_request_client(proxy_url: Option<&str>) -> Result<reqwest::Client, String> {
    let key = proxy_url.unwrap_or("").to_string();
    let cache =
        FETCH_CLIENT_CACHE.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    if let Ok(read) = cache.read() {
        if let Some(client) = read.get(&key) {
            return Ok(client.clone());
        }
    }
    let client = build_request_client(proxy_url)?;
    if let Ok(mut write) = cache.write() {
        write.entry(key).or_insert_with(|| client.clone());
    }
    Ok(client)
}

fn build_request_client(proxy_url: Option<&str>) -> Result<reqwest::Client, String> {
    // Redirects are followed manually below so each hop can be re-validated
    // against the same SSRF policy as the initial URL (GHSA-8v6v-g4rh-jmcm).
    // With reqwest's default auto-follow, an attacker-controlled origin can
    // 302 to http://127.0.0.1 and read the internal-service body.
    // Per-request timeout so a scripted fetch()/XHR, or a CORS preflight OPTIONS
    // (issue #251), to a server that accepts the connection but never responds
    // cannot hang forever. Without it op_fetch_url never returns, the fetch
    // promise never settles, and the JS XHR is stuck at readyState 1 with no
    // completion event (which stranded Angular HttpClient). On timeout reqwest's
    // send().await errors, which op_fetch_url propagates and the fetch shim turns
    // into an XHR `error`/`loadend`. 30s matches the other clients in the
    // workspace; TINYBROWSER_FETCH_TIMEOUT_MS overrides it for tighter cloud limits.
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(fetch_timeout())
        // SSRF guard: also reject hostnames that resolve to a private/loopback IP.
        .dns_resolver(std::sync::Arc::new(tinybrowser_net::SsrfGuardResolver::new(
            false,
        )))
        // Be explicit about pool size: default is unbounded which is fine,
        // but pool_idle_timeout default (90s) is short for SPA-heavy
        // workloads where the same origin is hit dozens of times across
        // a navigation. Keep connections warm longer.
        .pool_idle_timeout(std::time::Duration::from_secs(300))
        .tcp_keepalive(std::time::Duration::from_secs(60));
    if let Some(proxy) = proxy_url {
        let p = reqwest::Proxy::all(proxy)
            .map_err(|e| format!("Invalid op_fetch_url proxy '{}': {}", proxy, e))?;
        builder = builder.proxy(p);
    }
    builder
        .build()
        .map_err(|e| format!("failed to build reqwest::Client: {}", e))
}

pub(crate) fn fetch_timeout() -> std::time::Duration {
    let timeout_ms = std::env::var("TINYBROWSER_FETCH_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);
    std::time::Duration::from_millis(timeout_ms)
}

/// Cap on the number of redirect hops op_fetch_url will follow.
/// Matches reqwest's default policy of 10.
const FETCH_REDIRECT_LIMIT: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FetchCredentials {
    Omit,
    SameOrigin,
    Include,
}

impl FetchCredentials {
    fn parse(value: &str) -> Self {
        match value {
            "omit" => Self::Omit,
            "include" => Self::Include,
            _ => Self::SameOrigin,
        }
    }

    fn allows(self, page_origin: &str, request_url: &str) -> bool {
        match self {
            Self::Omit => false,
            Self::Include => true,
            Self::SameOrigin => request_origin(request_url)
                .map(|origin| origin == page_origin)
                .unwrap_or(false),
        }
    }
}

fn request_origin(request_url: &str) -> Option<String> {
    url::Url::parse(request_url)
        .ok()
        .map(|url| url.origin().ascii_serialization())
}

fn cors_response_allows(
    credentials: FetchCredentials,
    page_origin: &str,
    allowed_origin: &str,
    allow_credentials: &str,
) -> bool {
    if credentials == FetchCredentials::Include {
        allowed_origin == page_origin && allow_credentials == "true"
    } else {
        allowed_origin == "*" || allowed_origin == page_origin
    }
}

/// Snapshot of page state a fetch needs on the network thread. `RuntimeState`
/// is `!Send` (`Rc<RefCell<_>>`); this owned bundle is.
pub(crate) struct FetchJob {
    pub url: String,
    pub method: String,
    pub headers_json: String,
    pub body: String,
    pub origin: String,
    pub mode: String,
    pub credentials: String,
    pub cookie_jar: Option<Arc<CookieJar>>,
    pub http_client: Option<Arc<HttpClient>>,
    pub intercept: Option<(tokio::sync::mpsc::UnboundedSender<InterceptedRequest>, String)>,
    pub callbacks: Option<Arc<CallbackRegistry>>,
    pub in_flight: Option<Arc<std::sync::atomic::AtomicU32>>,
    pub page_in_flight: Arc<std::sync::atomic::AtomicU32>,
    #[cfg(feature = "stealth")]
    pub stealth_client: Option<Arc<StealthHttpClient>>,
}

pub(crate) struct FetchStore {
    pub body: String,
    pub body_len: usize,
    pub url: String,
    pub method: String,
    pub status: u16,
    pub response_headers: HashMap<String, String>,
}

pub(crate) struct FetchOutcome {
    pub json: serde_json::Value,
    pub store: Option<FetchStore>,
}

impl FetchOutcome {
    fn blocked(url: &str, error: Option<String>) -> Self {
        let mut json = serde_json::json!({
            "status": 0,
            "body": "",
            "url": url,
            "headers": {},
            "blocked": true,
        });
        if let Some(e) = error {
            json["error"] = serde_json::Value::String(e);
        }
        Self { json, store: None }
    }
}

fn json_outcome(json: serde_json::Value) -> FetchOutcome {
    FetchOutcome { json, store: None }
}

pub(crate) enum FetchStart {
    Immediate(FetchOutcome),
    Pending(FetchJob),
}

pub(crate) fn start_fetch(
    gs: &mut RuntimeState,
    url: String,
    method: String,
    headers_json: String,
    body: String,
    origin: String,
    mode: String,
    credentials: String,
) -> FetchStart {
    for pattern in &gs.blocked_urls {
        if pattern == "*" || url.contains(pattern) || glob_match(pattern, &url) {
            return FetchStart::Immediate(FetchOutcome::blocked(&url, None));
        }
    }
    gs.fetched_urls.push(url.clone());
    let allow_private_network = gs
        .http_client
        .as_ref()
        .is_some_and(|client| client.allow_private_network);
    if let Ok(parsed_url) = url::Url::parse(&url) {
        if let Err(e) = validate_fetch_url(&parsed_url, allow_private_network) {
            return FetchStart::Immediate(FetchOutcome::blocked(&url, Some(e)));
        }
    }
    tracing::debug!(
        "op_fetch_url: intercept_enabled={}, has_tx={}",
        gs.intercept_enabled,
        gs.intercept_tx.is_some()
    );
    let intercept = if gs.intercept_enabled {
        gs.intercept_counter += 1;
        gs.intercept_tx
            .clone()
            .map(|tx| (tx, format!("intercept-{}", gs.intercept_counter)))
    } else {
        None
    };
    FetchStart::Pending(FetchJob {
        cookie_jar: gs.cookie_jar.clone(),
        in_flight: gs.http_client.as_ref().map(|c| c.in_flight.clone()),
        page_in_flight: Arc::clone(&gs.page_in_flight),
        intercept,
        callbacks: gs.callbacks.clone(),
        http_client: gs.http_client.clone(),
        #[cfg(feature = "stealth")]
        stealth_client: gs.stealth_client.clone(),
        url,
        method,
        headers_json,
        body,
        origin,
        mode,
        credentials,
    })
}

pub(crate) fn apply_fetch_outcome(gs: &mut RuntimeState, mut outcome: FetchOutcome) -> String {
    if let Some(store) = outcome.store.take() {
        gs.network_response_body_counter += 1;
        let request_id = format!("fetch-{}", gs.network_response_body_counter);
        let max_entries = response_body_entry_limit();
        let max_bytes = response_body_byte_limit();
        if max_entries > 0 && max_bytes > 0 && store.body_len <= max_bytes {
            gs.network_response_bodies.insert(
                request_id.clone(),
                StoredNetworkResponseBody {
                    body: store.body.clone(),
                    base64_encoded: false,
                },
            );
            gs.network_response_body_order.push_back(request_id.clone());
            while gs.network_response_body_order.len() > max_entries {
                if let Some(oldest) = gs.network_response_body_order.pop_front() {
                    gs.network_response_bodies.remove(&oldest);
                }
            }
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();
        gs.js_network_events.push(JsNetworkEvent {
            request_id: request_id.clone(),
            url: store.url,
            method: store.method,
            status: store.status,
            response_headers: store.response_headers,
            body_size: store.body_len,
            timestamp,
        });
        const MAX_JS_NETWORK_EVENTS: usize = 4096;
        if gs.js_network_events.len() > MAX_JS_NETWORK_EVENTS {
            let overflow = gs.js_network_events.len() - MAX_JS_NETWORK_EVENTS;
            gs.js_network_events.drain(0..overflow);
        }
        outcome.json["requestId"] = serde_json::Value::String(request_id);
    }
    outcome.json.to_string()
}
pub(crate) async fn run_fetch_job(job: FetchJob) -> Result<FetchOutcome, String> {
    let FetchJob {
        url,
        method,
        headers_json,
        body,
        origin,
        mode,
        credentials,
        cookie_jar,
        http_client,
        intercept: intercept_tx,
        callbacks,
        in_flight,
        page_in_flight,
        #[cfg(feature = "stealth")]
        stealth_client,
    } = job;
    let proxy_url = http_client
        .as_ref()
        .and_then(|c| c.proxy_url().map(|s| s.to_string()));
    let allow_private_network = http_client
        .as_ref()
        .is_some_and(|client| client.allow_private_network);
    struct PageInFlightGuard(Arc<std::sync::atomic::AtomicU32>);
    impl Drop for PageInFlightGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    page_in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _page_in_flight = PageInFlightGuard(page_in_flight);

    // Slots the interception channel can override via Continue so a consumer
    // can rewrite url/method/headers/body before the request goes out.
    let mut override_url: Option<String> = None;
    let mut override_method: Option<String> = None;
    let mut override_headers: Option<HashMap<String, String>> = None;
    let mut override_body: Option<String> = None;

    if let Some((tx, request_id)) = intercept_tx {
        let custom_headers: HashMap<String, String> =
            serde_json::from_str(&headers_json).unwrap_or_default();
        let (resolve_tx, resolve_rx) = tokio::sync::oneshot::channel();
        let intercepted = InterceptedRequest {
            request_id: request_id.clone(),
            url: url.clone(),
            method: method.clone(),
            headers: custom_headers.clone(),
            resource_type: "Fetch".to_string(),
            resolver: resolve_tx,
        };
        if tx.send(intercepted).is_ok() {
            match resolve_rx.await {
                Ok(InterceptResolution::Fulfill {
                    status,
                    headers: h,
                    body: b,
                }) => {
                    let resp_headers: HashMap<String, String> = h;
                    return Ok(json_outcome(serde_json::json!({
                        "status": status,
                        "body": b,
                        "url": url,
                        "headers": resp_headers,
                    })));
                }
                Ok(InterceptResolution::Fail { reason }) => {
                    return Ok(json_outcome(serde_json::json!({
                        "status": 0,
                        "body": "",
                        "url": url,
                        "headers": {},
                        "blocked": true,
                        "error": reason,
                    })));
                }
                Ok(InterceptResolution::Continue {
                    url,
                    method,
                    headers,
                    body,
                }) => {
                    override_url = url;
                    override_method = method;
                    override_headers = headers;
                    override_body = body;
                    tracing::debug!(
                        "Interception: continue (overrides url={} method={} headers={} body={})",
                        override_url.is_some(),
                        override_method.is_some(),
                        override_headers.is_some(),
                        override_body.is_some()
                    );
                }
                Err(_) => {}
            }
        }
    }

    // Apply interception overrides (shadow the params for the rest of the op).
    // A Continue rewrite of the URL must pass the same SSRF / private-network
    // gate as the original request (checked above) and as redirects (checked
    // below). Without this re-validation a rewrite to an internal address would
    // bypass validate_fetch_url entirely.
    let url = if let Some(new_url) = override_url {
        if let Ok(parsed) = url::Url::parse(&new_url) {
            if let Err(reason) = validate_fetch_url(&parsed, allow_private_network) {
                return Ok(json_outcome(serde_json::json!({
                    "status": 0,
                    "body": "",
                    "url": new_url,
                    "blocked": true,
                    "error": format!("Intercept rewrite to forbidden URL blocked: {}", reason),
                })));
            }
        }
        new_url
    } else {
        url
    };
    let method = override_method.unwrap_or(method);
    let body = override_body.unwrap_or(body);

    let client = match &http_client {
        Some(client) => client.request_client().await,
        None => {
            cached_request_client(proxy_url.as_deref())?
        }
    };

    let initial_request_origin = request_origin(&url).unwrap_or_default();
    let page_origin = if origin.is_empty() {
        initial_request_origin.clone()
    } else {
        origin.clone()
    };
    let is_cross_origin = !page_origin.is_empty() && initial_request_origin != page_origin;
    let credentials = FetchCredentials::parse(&credentials);

    let req_method: reqwest::Method = method.parse().unwrap_or(reqwest::Method::GET);

    let custom_headers: std::collections::HashMap<String, String> =
        override_headers.unwrap_or_else(|| serde_json::from_str(&headers_json).unwrap_or_default());

    // Passive request observation (non-blocking). Fires for every request that
    // reaches the network (Fulfill/Fail from the interception channel short-
    // circuit earlier). on_request/on_response previously fired only for
    // navigation; this wires them for JS fetch()/XHR too.
    if let Some(ref cbs) = callbacks {
        if cbs.has_request_callbacks().await {
            if let Ok(parsed) = url::Url::parse(&url) {
                let info = RequestInfo {
                    url: parsed,
                    method: method.clone(),
                    headers: custom_headers.clone(),
                    resource_type: ResourceType::Fetch,
                };
                cbs.fire_request(&info).await;
            }
        }
    }

    let needs_preflight = is_cross_origin
        && mode == "cors"
        && (req_method != reqwest::Method::GET
            && req_method != reqwest::Method::HEAD
            && req_method != reqwest::Method::POST
            || custom_headers.keys().any(|k| {
                let kl = k.to_lowercase();
                kl != "accept"
                    && kl != "accept-language"
                    && kl != "content-language"
                    && kl != "content-type"
            }));

    if needs_preflight {
        let preflight = client
            .request(reqwest::Method::OPTIONS, &url)
            .timeout(fetch_timeout())
            .header("Origin", &page_origin)
            .header("Access-Control-Request-Method", method.as_str())
            .header(
                "Access-Control-Request-Headers",
                custom_headers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            )
            .send()
            .await
            .map_err(|e| format!("CORS preflight failed: {}", e))?;

        let allowed_origin = preflight
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let allow_credentials = preflight
            .headers()
            .get("access-control-allow-credentials")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !cors_response_allows(credentials, &page_origin, allowed_origin, allow_credentials) {
            return Err(format!(
                "CORS preflight: Origin '{}' not allowed by Access-Control-Allow-Origin '{}'",
                page_origin, allowed_origin
            ));
        }
    }

    // Stealth mode: route scripted requests through wreq after the CORS
    // preflight. stealth_fetch_all applies the credentials decision to each
    // redirect hop without losing the Chrome TLS/client-hint transport.
    #[cfg(feature = "stealth")]
    if let Some(stealth) = stealth_client {
        let json = stealth_fetch_all(
            stealth,
            url.clone(),
            req_method.as_str().to_string(),
            custom_headers.clone(),
            body.clone(),
            page_origin.clone(),
            mode.clone(),
            credentials,
            callbacks.clone(),
            allow_private_network,
        )
        .await
        .map_err(|e| e.to_string())?;
        let parsed: serde_json::Value =
            serde_json::from_str(&json).unwrap_or_else(|_| serde_json::Value::String(json));
        return Ok(json_outcome(parsed));
    }

    // Follow redirects manually so the SSRF policy applies to every hop.
    // reqwest's auto-follow would bypass validate_fetch_url on the redirect
    // target and let an attacker-allowed origin 302 to http://127.0.0.1
    // (GHSA-8v6v-g4rh-jmcm).
    let mut current_url = url.clone();
    let mut current_method = req_method;
    let mut current_body = body;
    let mut redirects_followed: usize = 0;
    let response = loop {
        let mut req = client
            .request(current_method.clone(), &current_url)
            .timeout(fetch_timeout());

        let current_is_cross_origin = request_origin(&current_url)
            .map(|request_origin| request_origin != page_origin)
            .unwrap_or(false);
        if current_is_cross_origin {
            req = req.header("Origin", &page_origin);
        }

        let credentials_allowed = credentials.allows(&page_origin, &current_url);
        if credentials_allowed {
            if let Some(ref jar) = cookie_jar {
                if let Ok(parsed_url) = url::Url::parse(&current_url) {
                    let cookie_header = jar.get_cookie_header(&parsed_url);
                    if !cookie_header.is_empty() {
                        req = req.header("Cookie", &cookie_header);
                    }
                }
            }
        }

        // Send a default User-Agent on fetch()/XHR requests (the navigation path
        // sets one, but this op did not, so scripted requests went out with no UA
        // and UA-gated servers rejected them). Honor an explicit override.
        if !custom_headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("user-agent"))
        {
            req = req.header(
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
            );
        }

        for (k, v) in &custom_headers {
            req = req.header(k.as_str(), v.as_str());
        }

        if !current_body.is_empty() {
            req = req.body(current_body.clone());
        }

        if let Some(ref counter) = in_flight {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        let resp = req.send().await.map_err(|e| {
            if let Some(ref counter) = in_flight {
                counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            e.to_string()
        })?;

        if let Some(ref counter) = in_flight {
            counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }

        if credentials_allowed {
            if let Some(ref jar) = cookie_jar {
                if let Ok(parsed_url) = url::Url::parse(&current_url) {
                    for val in resp.headers().get_all(reqwest::header::SET_COOKIE) {
                        if let Ok(s) = val.to_str() {
                            jar.set_cookie(s, &parsed_url);
                        }
                    }
                }
            }
        }

        if !resp.status().is_redirection() {
            break resp;
        }

        let location_header = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let Some(location) = location_header else {
            // 3xx without a Location header is not actually a redirect.
            break resp;
        };

        let base = match url::Url::parse(&current_url) {
            Ok(b) => b,
            Err(_) => break resp,
        };
        let next_url = match base.join(&location) {
            Ok(u) => u,
            Err(_) => break resp,
        };

        // Re-validate every redirect target against the SSRF policy.
        if let Err(reason) = validate_fetch_url(&next_url, allow_private_network) {
            return Ok(json_outcome(serde_json::json!({
                "status": 0,
                "body": "",
                "url": next_url.to_string(),
                "headers": {},
                "blocked": true,
                "error": format!("Redirect to forbidden URL blocked: {}", reason),
            })));
        }

        redirects_followed += 1;
        if redirects_followed > FETCH_REDIRECT_LIMIT {
            return Ok(json_outcome(serde_json::json!({
                "status": 0,
                "body": "",
                "url": next_url.to_string(),
                "headers": {},
                "blocked": true,
                "error": format!("Too many redirects (>{})", FETCH_REDIRECT_LIMIT),
            })));
        }

        // Browser semantics: 301/302/303 downgrade to GET with no body.
        // 307/308 preserve method and body.
        let status_code = resp.status().as_u16();
        if status_code == 301 || status_code == 302 || status_code == 303 {
            current_method = reqwest::Method::GET;
            current_body.clear();
        }

        current_url = next_url.to_string();
    };

    let status = response.status().as_u16();

    let resp_headers: std::collections::HashMap<String, String> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    let final_is_cross_origin = request_origin(&current_url)
        .map(|request_origin| request_origin != page_origin)
        .unwrap_or(false);
    if final_is_cross_origin && mode == "cors" {
        let allowed = resp_headers
            .get("access-control-allow-origin")
            .map(|s| s.as_str())
            .unwrap_or("");

        let allow_credentials = resp_headers
            .get("access-control-allow-credentials")
            .map(|s| s.as_str())
            .unwrap_or("");
        if !cors_response_allows(credentials, &page_origin, allowed, allow_credentials) {
            return Ok(json_outcome(serde_json::json!({
                "status": 0,
                "body": "",
                "url": url,
                "headers": {},
                "corsBlocked": true,
                "corsError": if credentials == FetchCredentials::Include {
                    format!(
                        "CORS error: credentialed request requires Access-Control-Allow-Origin '{}' and Access-Control-Allow-Credentials 'true'",
                        page_origin
                    )
                } else {
                    format!("CORS error: Origin '{}' not in Access-Control-Allow-Origin '{}'", page_origin, allowed)
                },
            })));
        }
    }

    let resp_bytes = response
        .bytes()
        .await
        .map_err(|e| e.to_string())?;
    let resp_body = String::from_utf8_lossy(&resp_bytes).to_string();
    let resp_body_base64 = BASE64.encode(&resp_bytes);
    if let Some(ref cbs) = callbacks {
        if cbs.has_response_callbacks().await {
            let resp = fetch_response(&url, status, resp_headers.clone(), resp_bytes.to_vec());
            let info = RequestInfo {
                url: resp.url.clone(),
                method: method.clone(),
                headers: resp_headers.clone(),
                resource_type: ResourceType::Fetch,
            };
            cbs.fire_response(&info, &resp).await;
        }
    }

    tracing::debug!(
        "op_fetch_url completed: {} {} ({} bytes)",
        method,
        url,
        resp_body.len()
    );

    Ok(FetchOutcome {
        json: serde_json::json!({
            "status": status,
            "body": resp_body,
            "bodyBase64": resp_body_base64,
            "url": url,
            "headers": resp_headers,
        }),
        store: Some(FetchStore {
            body: resp_body,
            body_len: resp_bytes.len(),
            url,
            method,
            status,
            response_headers: resp_headers,
        }),
    })
}

/// Assemble a `Response` for the on_response interception callbacks from the
/// parts op_fetch_url already holds. Navigation gets a Response straight from
/// the http client, but the JS fetch path builds the pieces itself.
fn fetch_response(
    url: &str,
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
) -> Response {
    Response {
        url: url::Url::parse(url).unwrap_or_else(|_| url::Url::parse("http://0.0.0.0/").unwrap()),
        status,
        headers,
        body,
        redirected_from: Vec::new(),
    }
}

/// Stealth-mode scripted fetch()/XHR: mirrors op_fetch_url's redirect, SSRF,
/// and CORS semantics but sends every hop through the wreq stealth client so
/// the request carries the Chrome TLS fingerprint and client hints. Cookie
/// handling lives inside StealthHttpClient::send_single, which shares the
/// context jar. Response bodies are not mirrored into the CDP
/// Network.getResponseBody buffer here; that is a follow-up for stealth fetches.
#[cfg(feature = "stealth")]
async fn stealth_fetch_all(
    stealth: Arc<StealthHttpClient>,
    url: String,
    method: String,
    custom_headers: HashMap<String, String>,
    body: String,
    page_origin: String,
    mode: String,
    credentials: FetchCredentials,
    callbacks: Option<Arc<CallbackRegistry>>,
    allow_private_network: bool,
) -> Result<String, deno_error::JsErrorBox> {
    let mut current_url = url.clone();
    let mut current_method = method;
    let mut current_body = body;
    let mut redirects_followed: usize = 0;

    let (status, resp_headers, resp_bytes): (u16, HashMap<String, String>, Vec<u8>) = loop {
        let parsed_current = match url::Url::parse(&current_url) {
            Ok(u) => u,
            Err(_) => {
                return Ok(serde_json::json!({
                    "status": 0, "body": "", "url": current_url, "headers": {},
                })
                .to_string());
            }
        };

        let mut req_headers: HashMap<String, String> = HashMap::new();
        let current_is_cross_origin = parsed_current.origin().ascii_serialization() != page_origin;
        if current_is_cross_origin {
            req_headers.insert("origin".to_string(), page_origin.clone());
        }
        for (k, v) in &custom_headers {
            req_headers.insert(k.to_lowercase(), v.clone());
        }

        let credentials_allowed = credentials.allows(&page_origin, &current_url);
        let r = stealth
            .send_single(
                &current_method,
                &parsed_current,
                &req_headers,
                &current_body,
                credentials_allowed,
                credentials_allowed,
            )
            .await
            .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;

        if !(300..400).contains(&r.status) {
            break (r.status, r.headers, r.body);
        }
        let Some(location) = r.headers.get("location").cloned() else {
            break (r.status, r.headers, r.body);
        };
        let next_url = match parsed_current.join(&location) {
            Ok(u) => u,
            Err(_) => break (r.status, r.headers, r.body),
        };
        // Re-validate every redirect target against the SSRF policy, matching
        // op_fetch_url (GHSA-8v6v-g4rh-jmcm).
        if let Err(reason) = validate_fetch_url(&next_url, allow_private_network) {
            return Ok(serde_json::json!({
                "status": 0, "body": "", "url": next_url.to_string(), "headers": {},
                "blocked": true,
                "error": format!("Redirect to forbidden URL blocked: {}", reason),
            })
            .to_string());
        }
        redirects_followed += 1;
        if redirects_followed > FETCH_REDIRECT_LIMIT {
            return Ok(serde_json::json!({
                "status": 0, "body": "", "url": next_url.to_string(), "headers": {},
                "blocked": true,
                "error": format!("Too many redirects (>{})", FETCH_REDIRECT_LIMIT),
            })
            .to_string());
        }
        // Browser semantics: 301/302/303 downgrade to GET with no body.
        if r.status == 301 || r.status == 302 || r.status == 303 {
            current_method = "GET".to_string();
            current_body.clear();
        }
        current_url = next_url.to_string();
    };

    let final_is_cross_origin = request_origin(&current_url)
        .map(|request_origin| request_origin != page_origin)
        .unwrap_or(false);
    if final_is_cross_origin && mode == "cors" {
        let allowed = resp_headers
            .get("access-control-allow-origin")
            .map(|s| s.as_str())
            .unwrap_or("");
        let allow_credentials = resp_headers
            .get("access-control-allow-credentials")
            .map(|s| s.as_str())
            .unwrap_or("");
        if !cors_response_allows(credentials, &page_origin, allowed, allow_credentials) {
            return Ok(serde_json::json!({
                "status": 0, "body": "", "url": url, "headers": {},
                "corsBlocked": true,
                "corsError": if credentials == FetchCredentials::Include {
                    format!(
                        "CORS error: credentialed request requires Access-Control-Allow-Origin '{}' and Access-Control-Allow-Credentials 'true'",
                        page_origin
                    )
                } else {
                    format!(
                        "CORS error: Origin '{}' not in Access-Control-Allow-Origin '{}'",
                        page_origin, allowed
                    )
                },
            })
            .to_string());
        }
    }

    let resp_body = String::from_utf8_lossy(&resp_bytes).to_string();
    let resp_body_base64 = BASE64.encode(&resp_bytes);
    if let Some(ref cbs) = callbacks {
        if cbs.has_response_callbacks().await {
            let resp = fetch_response(&url, status, resp_headers.clone(), resp_bytes.clone());
            let info = RequestInfo {
                url: resp.url.clone(),
                method: current_method.clone(),
                headers: resp_headers.clone(),
                resource_type: ResourceType::Fetch,
            };
            cbs.fire_response(&info, &resp).await;
        }
    }

    Ok(serde_json::json!({
        "status": status,
        "body": resp_body,
        "bodyBase64": resp_body_base64,
        "url": url,
        "headers": resp_headers,
    })
    .to_string())
}

fn glob_match(pattern: &str, url: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    let mut remainder = url;
    let mut first = true;
    for part in pattern.split('*') {
        if part.is_empty() {
            continue;
        }

        let Some(index) = remainder.find(part) else {
            return false;
        };

        if first && !pattern.starts_with('*') && index != 0 {
            return false;
        }

        remainder = &remainder[index + part.len()..];
        first = false;
    }

    pattern.ends_with('*') || remainder.is_empty()
}

#[cfg(test)]
mod tests {
    use super::{cors_response_allows, glob_match, validate_fetch_url, FetchCredentials};
    use crate::runtime::JsRuntime;
    use tinybrowser_dom::parse_html;

    #[test]
    fn glob_match_handles_cdp_blocked_url_patterns() {
        assert!(glob_match(
            "*://*.google.com/maps/vt/*",
            "https://www.google.com/maps/vt/pb=!1m4!1m3",
        ));
        assert!(glob_match(
            "*://*.gstatic.com/*.woff2",
            "https://fonts.gstatic.com/s/inter/v18/font.woff2",
        ));
        assert!(glob_match(
            "https://example.com/assets/*",
            "https://example.com/assets/app.js",
        ));
        assert!(!glob_match(
            "https://example.com/assets/*",
            "https://cdn.example.com/assets/app.js",
        ));
        assert!(!glob_match(
            "*://*.gstatic.com/*.woff2",
            "https://fonts.gstatic.com/s/inter/v18/font.woff",
        ));
    }

    #[test]
    fn fetch_credentials_gate_cookie_send_and_storage_per_request_origin() {
        let page_origin = "https://www.example.com";
        let same_origin_url = "https://www.example.com/api";
        let explicit_default_port = "https://www.example.com:443/api";
        let cross_origin_url = "https://api.example.com/data";

        assert!(!FetchCredentials::Omit.allows(page_origin, same_origin_url));
        assert!(!FetchCredentials::Omit.allows(page_origin, cross_origin_url));

        assert!(FetchCredentials::SameOrigin.allows(page_origin, same_origin_url));
        assert!(FetchCredentials::SameOrigin.allows(page_origin, explicit_default_port));
        assert!(!FetchCredentials::SameOrigin.allows(page_origin, cross_origin_url));

        assert!(FetchCredentials::Include.allows(page_origin, same_origin_url));
        assert!(FetchCredentials::Include.allows(page_origin, cross_origin_url));
    }

    #[test]
    fn credentialed_cors_requires_exact_origin_and_allow_credentials() {
        let page_origin = "https://www.example.com";

        assert!(cors_response_allows(
            FetchCredentials::SameOrigin,
            page_origin,
            "*",
            "",
        ));
        assert!(!cors_response_allows(
            FetchCredentials::Include,
            page_origin,
            "*",
            "true",
        ));
        assert!(!cors_response_allows(
            FetchCredentials::Include,
            page_origin,
            page_origin,
            "",
        ));
        assert!(cors_response_allows(
            FetchCredentials::Include,
            page_origin,
            page_origin,
            "true",
        ));
    }

    #[test]
    fn fetch_url_validation_honors_per_context_private_network_opt_in() {
        let loopback = url::Url::parse("http://127.0.0.1:8080/resource").unwrap();
        assert!(validate_fetch_url(&loopback, true).is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn posted_task_chains_complete_without_zero_delay_timer_floor() {
        let mut runtime = JsRuntime::new();
        runtime.set_dom(parse_html("<html><body></body></html>"));
        runtime.set_url("http://example.com/posted-task-test");
        runtime.run_page_init();
        runtime
            .execute_script(
                "posted-task-throughput",
                r#"
                    globalThis.__postedTaskBench = {
                        message: 0,
                        postTask: 0,
                        yields: 0,
                        started: performance.now(),
                        finished: 0,
                    };
                    const markFinished = () => {
                        if (__postedTaskBench.message === 100 &&
                            __postedTaskBench.postTask === 100 &&
                            __postedTaskBench.yields === 100) {
                            __postedTaskBench.finished = performance.now();
                        }
                    };

                    const channel = new MessageChannel();
                    channel.port2.onmessage = () => {
                        __postedTaskBench.message++;
                        if (__postedTaskBench.message < 100) channel.port1.postMessage(null);
                        else markFinished();
                    };
                    channel.port1.postMessage(null);

                    const postNext = () => scheduler.postTask(() => {
                        __postedTaskBench.postTask++;
                        if (__postedTaskBench.postTask < 100) postNext();
                        else markFinished();
                    });
                    postNext();

                    scheduler.postTask(async () => {
                        while (__postedTaskBench.yields < 100) {
                            await scheduler.yield();
                            __postedTaskBench.yields++;
                        }
                        markFinished();
                    });
                "#,
            )
            .unwrap();

        runtime.run_event_loop_bounded(100).await.unwrap();
        let result = runtime
            .evaluate(
                r#"[
                    __postedTaskBench.message,
                    __postedTaskBench.postTask,
                    __postedTaskBench.yields,
                    __postedTaskBench.finished - __postedTaskBench.started,
                ]"#,
            )
            .unwrap();
        let values = result.as_array().unwrap();
        assert!(
            values[..3].iter().all(|value| value.as_f64() == Some(100.0)),
            "posted-task chains did not finish inside the 100ms pump: {result}",
        );
        assert!(
            values[3].as_f64().is_some_and(|elapsed| elapsed >= 0.0 && elapsed < 75.0),
            "300 chained posted-task deliveries retained timer-wheel latency: {result}",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shared_posted_task_queue_preserves_priority_fifo_and_microtasks() {
        let mut runtime = JsRuntime::new();
        runtime.set_dom(parse_html("<html><body></body></html>"));
        runtime.set_url("http://example.com/posted-task-order");
        runtime.run_page_init();
        runtime
            .execute_script(
                "shared-posted-task-order",
                r#"
                    globalThis.__sharedPostedOrder = ["sync"];
                    const channel = new MessageChannel();
                    channel.port2.onmessage = event => {
                        __sharedPostedOrder.push("message-" + event.data);
                        Promise.resolve().then(() => {
                            __sharedPostedOrder.push("message-" + event.data + "-microtask");
                        });
                    };
                    channel.port1.postMessage(1);
                    scheduler.postTask(() => {
                        __sharedPostedOrder.push("visible");
                        Promise.resolve().then(() => __sharedPostedOrder.push("visible-microtask"));
                    });
                    channel.port1.postMessage(2);
                    scheduler.postTask(() => {
                        __sharedPostedOrder.push("background");
                    }, { priority: "background" });
                    scheduler.postTask(() => {
                        __sharedPostedOrder.push("blocking");
                        Promise.resolve().then(() => __sharedPostedOrder.push("blocking-microtask"));
                    }, { priority: "user-blocking" });
                    Promise.resolve().then(() => __sharedPostedOrder.push("initial-microtask"));
                "#,
            )
            .unwrap();

        runtime.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            runtime.evaluate("__sharedPostedOrder").unwrap(),
            serde_json::json!([
                "sync",
                "initial-microtask",
                "blocking",
                "blocking-microtask",
                "message-1",
                "message-1-microtask",
                "visible",
                "visible-microtask",
                "message-2",
                "message-2-microtask",
                "background",
            ]),
        );
    }

}

fn validate_fetch_url(url: &url::Url, allow_private_network: bool) -> Result<(), String> {
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" && scheme != "file" {
        return Err(format!(
            "Forbidden URL scheme '{}' - only http, https, and file are allowed",
            scheme
        ));
    }

    if scheme == "file"
        || allow_private_network
        || tinybrowser_net::env_allows_private_network()
    {
        return Ok(());
    }

    if let Some(host) = url.host() {
        match host {
            url::Host::Ipv4(ip) => {
                if tinybrowser_net::is_forbidden_ip(std::net::IpAddr::V4(ip)) {
                    return Err(format!(
                        "Access to private/internal IP address {} is not allowed",
                        ip
                    ));
                }
            }
            url::Host::Ipv6(ip) => {
                if tinybrowser_net::is_forbidden_ip(std::net::IpAddr::V6(ip)) {
                    return Err(format!(
                        "Access to private/internal IPv6 address {} is not allowed",
                        ip
                    ));
                }
            }
            url::Host::Domain(domain) => {
                let lower_domain = domain.to_lowercase();
                if lower_domain == "localhost"
                    || lower_domain.ends_with(".localhost")
                    || lower_domain == "127.0.0.1"
                    || lower_domain == "::1"
                {
                    return Err(format!(
                        "Access to localhost domain '{}' is not allowed",
                        domain
                    ));
                }
            }
        }
    }

    Ok(())
}
pub(crate) fn op_get_cookies_inner(shared: &SharedState) -> String {
    let gs = shared.borrow();
    let jar = match &gs.cookie_jar {
        Some(j) => j,
        None => return String::new(),
    };
    let url = match url::Url::parse(&gs.url) {
        Ok(u) => u,
        Err(_) => return String::new(),
    };
    jar.get_js_visible_cookies(&url)
}
pub(crate) fn op_set_cookie_inner(shared: &SharedState, cookie_str: &str) {
    let gs = shared.borrow();
    let jar = match &gs.cookie_jar {
        Some(j) => j,
        None => return,
    };
    let url = match url::Url::parse(&gs.url) {
        Ok(u) => u,
        Err(_) => return,
    };
    jar.set_cookie_from_js(cookie_str, &url);
}
pub(crate) fn op_navigate_inner(shared: &SharedState, url: &str, method: &str, body: &str) {
    let mut gs = shared.borrow_mut();
    gs.url = url.to_string();
    gs.pending_navigation = Some((url.to_string(), method.to_string(), body.to_string()));
}

// Records a binding call from page JS. The CDP layer drains this queue
// after every dispatch and emits one `Runtime.bindingCalled` event per
// entry, that's how puppeteer's `page.exposeFunction` callbacks fire.
pub(crate) fn op_binding_called_inner(shared: &SharedState, name: &str, payload: &str) {
    let mut gs = shared.borrow_mut();
    gs.pending_binding_calls
        .push((name.to_string(), payload.to_string()));
}

/// Real WebCrypto `crypto.subtle.digest`. `algorithm` is the SubtleCrypto
/// algorithm name (`SHA-1` / `SHA-256` / `SHA-384` / `SHA-512`, plus the
/// FIPS 180-4 truncated variants `SHA-512/224` and `SHA-512/256`). The JS
/// shim validates the name; any other value is unreachable.
/// Returns the raw digest bytes so the JS shim can hand them back as an ArrayBuffer.
pub(crate) fn subtle_digest(algorithm: &str, data: &[u8]) -> Vec<u8> {
    use sha1::Digest as _;
    let alg = algorithm.to_ascii_uppercase();
    match alg.as_str() {
        "SHA-1" => sha1::Sha1::digest(data).to_vec(),
        "SHA-256" => sha2::Sha256::digest(data).to_vec(),
        "SHA-384" => sha2::Sha384::digest(data).to_vec(),
        "SHA-512" => sha2::Sha512::digest(data).to_vec(),
        "SHA-512/224" => sha2::Sha512_224::digest(data).to_vec(),
        "SHA-512/256" => sha2::Sha512_256::digest(data).to_vec(),
        _ => vec![],
    }
}

// ---------------------------------------------------------------------------
// WebCrypto (crypto.subtle) secret-key primitives.
//
// These ops are stateless. The JS shim in bootstrap.js owns the CryptoKey
// objects and their raw key bytes; it hands the bytes plus normalized algorithm
// parameters to these ops for each operation. Only secret-key algorithms live
// here (HMAC, AES-GCM/CBC/CTR, PBKDF2, HKDF); public-key algorithms are rejected
// in the shim. A fallible op returns a JsErrorBox that the shim turns into the
// appropriate DOMException (OperationError for a bad tag or padding, etc.).
// ---------------------------------------------------------------------------

fn crypto_err(msg: impl std::fmt::Display) -> deno_error::JsErrorBox {
    deno_error::JsErrorBox::generic(msg.to_string())
}

/// HMAC sign. `hash` is a normalized SubtleCrypto hash name; any key length is
/// accepted (HMAC pads or hashes the key per RFC 2104). Returns the MAC bytes;
/// the shim does the constant-time-insensitive compare for `verify`.
pub(crate) fn subtle_hmac(
    hash: &str,
    key: &[u8],
    data: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use hmac::{Hmac, Mac};
    macro_rules! run {
        ($d:ty) => {{
            let mut mac = Hmac::<$d>::new_from_slice(key).map_err(crypto_err)?;
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }};
    }
    Ok(match hash {
        "SHA-1" => run!(sha1::Sha1),
        "SHA-256" => run!(sha2::Sha256),
        "SHA-384" => run!(sha2::Sha384),
        "SHA-512" => run!(sha2::Sha512),
        _ => return Err(crypto_err("unsupported HMAC hash")),
    })
}

/// AES-GCM encrypt/decrypt. WebCrypto's ciphertext carries the auth tag
/// appended, which is exactly RustCrypto's combined form, so this maps 1:1.
/// Restricted to a 96-bit IV and 128-bit tag (the WebCrypto defaults and the
/// overwhelming majority of real usage); the shim rejects other tag lengths.
pub(crate) fn subtle_aes_gcm(
    encrypt: bool,
    key: &[u8],
    iv: &[u8],
    aad: &[u8],
    data: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::aes::{Aes192, Aes256};
    use aes_gcm::{AesGcm, Nonce};
    type Aes192Gcm = AesGcm<Aes192, aes_gcm::aead::consts::U12>;
    type Aes256Gcm = AesGcm<Aes256, aes_gcm::aead::consts::U12>;

    if iv.len() != 12 {
        return Err(crypto_err("AES-GCM requires a 96-bit (12-byte) IV"));
    }
    let nonce = Nonce::from_slice(iv);
    macro_rules! run {
        ($ty:ty) => {{
            let cipher = <$ty>::new_from_slice(key).map_err(crypto_err)?;
            if encrypt {
                cipher
                    .encrypt(nonce, Payload { msg: data, aad })
                    .map_err(|_| crypto_err("AES-GCM encryption failed"))?
            } else {
                cipher
                    .decrypt(nonce, Payload { msg: data, aad })
                    .map_err(|_| {
                        crypto_err("AES-GCM decryption failed: authentication tag mismatch")
                    })?
            }
        }};
    }
    Ok(match key.len() {
        16 => run!(aes_gcm::Aes128Gcm),
        24 => run!(Aes192Gcm),
        32 => run!(Aes256Gcm),
        _ => return Err(crypto_err("AES-GCM key must be 128, 192, or 256 bits")),
    })
}

/// AES-CBC encrypt/decrypt with PKCS#7 padding (the only padding WebCrypto
/// AES-CBC uses) and a 16-byte IV.
pub(crate) fn subtle_aes_cbc(
    encrypt: bool,
    key: &[u8],
    iv: &[u8],
    data: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use cbc::cipher::block_padding::Pkcs7;
    use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};
    use cbc::{Decryptor, Encryptor};

    if iv.len() != 16 {
        return Err(crypto_err("AES-CBC requires a 16-byte IV"));
    }
    macro_rules! run {
        ($cipher:ty) => {{
            if encrypt {
                Encryptor::<$cipher>::new_from_slices(key, iv)
                    .map_err(crypto_err)?
                    .encrypt_padded_vec_mut::<Pkcs7>(data)
            } else {
                Decryptor::<$cipher>::new_from_slices(key, iv)
                    .map_err(crypto_err)?
                    .decrypt_padded_vec_mut::<Pkcs7>(data)
                    .map_err(|_| crypto_err("AES-CBC decryption failed: invalid padding"))?
            }
        }};
    }
    Ok(match key.len() {
        16 => run!(aes::Aes128),
        24 => run!(aes::Aes192),
        32 => run!(aes::Aes256),
        _ => return Err(crypto_err("AES-CBC key must be 128, 192, or 256 bits")),
    })
}

/// AES-CTR. Encrypt and decrypt are the same keystream XOR. `counter_length` is
/// the WebCrypto counter width in bits; it selects the RustCrypto CTR flavor so
/// only the low `counter_length` bits of the 16-byte block increment.
pub(crate) fn subtle_aes_ctr(
    key: &[u8],
    counter: &[u8],
    counter_length: u32,
    data: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use ctr::cipher::{KeyIvInit, StreamCipher};

    if counter.len() != 16 {
        return Err(crypto_err("AES-CTR requires a 16-byte counter block"));
    }
    let mut buf = data.to_vec();
    macro_rules! run {
        ($ty:ty) => {{
            <$ty>::new_from_slices(key, counter)
                .map_err(crypto_err)?
                .apply_keystream(&mut buf);
        }};
    }
    macro_rules! by_key {
        ($flavor:ident) => {
            match key.len() {
                16 => run!(ctr::$flavor<aes::Aes128>),
                24 => run!(ctr::$flavor<aes::Aes192>),
                32 => run!(ctr::$flavor<aes::Aes256>),
                _ => return Err(crypto_err("AES-CTR key must be 128, 192, or 256 bits")),
            }
        };
    }
    match counter_length {
        128 => by_key!(Ctr128BE),
        64 => by_key!(Ctr64BE),
        32 => by_key!(Ctr32BE),
        _ => {
            return Err(crypto_err(
                "AES-CTR supports counter lengths of 32, 64, or 128 bits",
            ))
        }
    }
    Ok(buf)
}

/// PBKDF2 key derivation. `length` is the derived-bits output in bytes.
pub(crate) fn subtle_pbkdf2(
    hash: &str,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    length: u32,
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use pbkdf2::pbkdf2_hmac;
    let mut dk = vec![0u8; length as usize];
    match hash {
        "SHA-1" => pbkdf2_hmac::<sha1::Sha1>(password, salt, iterations, &mut dk),
        "SHA-256" => pbkdf2_hmac::<sha2::Sha256>(password, salt, iterations, &mut dk),
        "SHA-384" => pbkdf2_hmac::<sha2::Sha384>(password, salt, iterations, &mut dk),
        "SHA-512" => pbkdf2_hmac::<sha2::Sha512>(password, salt, iterations, &mut dk),
        _ => return Err(crypto_err("unsupported PBKDF2 hash")),
    }
    Ok(dk)
}

/// HKDF key derivation. `length` is the output length in bytes. An empty salt
/// behaves as RFC 5869 specifies (HMAC zero-pads it to the block size, which is
/// what browsers do).
pub(crate) fn subtle_hkdf(
    hash: &str,
    ikm: &[u8],
    salt: &[u8],
    info: &[u8],
    length: u32,
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use hkdf::Hkdf;
    let mut okm = vec![0u8; length as usize];
    macro_rules! run {
        ($d:ty) => {
            Hkdf::<$d>::new(Some(salt), ikm)
                .expand(info, &mut okm)
                .map_err(|_| crypto_err("HKDF: requested key length is too long"))?
        };
    }
    match hash {
        "SHA-1" => run!(sha1::Sha1),
        "SHA-256" => run!(sha2::Sha256),
        "SHA-384" => run!(sha2::Sha384),
        "SHA-512" => run!(sha2::Sha512),
        _ => return Err(crypto_err("unsupported HKDF hash")),
    }
    Ok(okm)
}

/// Fill `len` bytes from the OS CSPRNG. Backs `crypto.getRandomValues`,
/// `crypto.randomUUID`, and `generateKey`, replacing the old Math.random shim
/// (which was neither uniform across typed-array widths nor cryptographically
/// random, and was a fingerprinting tell).
pub(crate) fn random_bytes(len: u32) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    let mut buf = vec![0u8; len as usize];
    getrandom::getrandom(&mut buf).map_err(|e| crypto_err(format!("getrandom failed: {e}")))?;
    Ok(buf)
}

/// Serialize a parsed URL into the WHATWG IDL component shape consumed by the
/// `URL` class in bootstrap.js. Getters read these fields directly so no op
/// call happens per property access.
fn url_components(u: &url::Url) -> serde_json::Value {
    let port = u.port().map(|p| p.to_string()).unwrap_or_default();
    let hostname = u.host_str().unwrap_or("").to_string();
    let host = if hostname.is_empty() {
        String::new()
    } else if port.is_empty() {
        hostname.clone()
    } else {
        format!("{hostname}:{port}")
    };
    // WHATWG search/hash getters return "" for a null OR empty component.
    let search = match u.query() {
        Some(q) if !q.is_empty() => format!("?{q}"),
        _ => String::new(),
    };
    let hash = match u.fragment() {
        Some(f) if !f.is_empty() => format!("#{f}"),
        _ => String::new(),
    };
    serde_json::json!({
        "ok": true,
        "href": u.as_str(),
        "protocol": format!("{}:", u.scheme()),
        "username": u.username(),
        "password": u.password().unwrap_or(""),
        "host": host,
        "hostname": hostname,
        "port": port,
        "pathname": u.path(),
        "search": search,
        "hash": hash,
        "origin": u.origin().ascii_serialization(),
    })
}

/// Parse `href` (optionally resolved against `base`) with the WHATWG-compliant
/// `url` crate. Returns the component JSON, or `{"ok":false}` when the input is
/// not a valid URL (the JS side turns that into a TypeError, per spec).
pub(crate) fn url_parse(href: &str, base: &str) -> String {
    // The url crate can panic on a few pathological inputs (internal range
    // slicing); catch it so a bad URL never aborts the process.
    std::panic::catch_unwind(|| {
        let parsed = if base.is_empty() {
            url::Url::parse(href)
        } else {
            url::Url::parse(base).and_then(|b| b.join(href))
        };
        match parsed {
            Ok(u) => url_components(&u).to_string(),
            Err(_) => "{\"ok\":false}".to_string(),
        }
    })
    .unwrap_or_else(|_| "{\"ok\":false}".to_string())
}

/// Apply a WHATWG URL setter (`part` = href/protocol/username/password/host/
/// hostname/port/pathname/search/hash) to `href` and return the new components.
fn url_set_inner(href: &str, part: &str, value: &str) -> Option<serde_json::Value> {
    let mut u = url::Url::parse(href).ok()?;
    match part {
        "href" => {
            let nu = url::Url::parse(value).ok()?;
            return Some(url_components(&nu));
        }
        "protocol" => {
            let _ = u.set_scheme(value.trim_end_matches(':'));
        }
        "username" => {
            let _ = u.set_username(value);
        }
        "password" => {
            let _ = u.set_password(if value.is_empty() { None } else { Some(value) });
        }
        "host" => set_host_port(&mut u, value),
        "hostname" => {
            if !value.is_empty() {
                let _ = u.set_host(Some(value));
            }
        }
        "port" => {
            if value.is_empty() {
                let _ = u.set_port(None);
            } else if let Ok(p) = value.parse::<u16>() {
                let _ = u.set_port(Some(p));
            }
        }
        "pathname" => u.set_path(value),
        "search" => {
            let q = value.strip_prefix('?').unwrap_or(value);
            u.set_query(if q.is_empty() { None } else { Some(q) });
        }
        "hash" => {
            let f = value.strip_prefix('#').unwrap_or(value);
            u.set_fragment(if f.is_empty() { None } else { Some(f) });
        }
        _ => {}
    }
    Some(url_components(&u))
}

pub(crate) fn url_set(href: &str, part: &str, value: &str) -> String {
    // Some url-crate setters panic on pathological inputs (the url-setters WPT
    // tests exercise these). Catch the unwind and treat it as a no-op setter,
    // returning the URL unchanged, which matches WHATWG "do nothing on invalid".
    match std::panic::catch_unwind(|| url_set_inner(href, part, value)) {
        Ok(Some(v)) => v.to_string(),
        _ => match url::Url::parse(href) {
            Ok(u) => url_components(&u).to_string(),
            Err(_) => "{\"ok\":false}".to_string(),
        },
    }
}

/// Best-effort `host` setter: split `host[:port]` (handling bracketed IPv6) and
/// apply hostname and port separately, since `url::Url::set_host` rejects a port.
fn set_host_port(u: &mut url::Url, value: &str) {
    // IPv6 literals are bracketed; never split inside the brackets.
    if value.starts_with('[') {
        if let Some(close) = value.find(']') {
            let host = &value[..=close];
            let rest = &value[close + 1..];
            if u.set_host(Some(host)).is_ok() {
                if let Some(p) = rest.strip_prefix(':') {
                    if let Ok(pn) = p.parse::<u16>() {
                        let _ = u.set_port(Some(pn));
                    }
                }
            }
            return;
        }
    }
    if let Some(idx) = value.rfind(':') {
        let (h, p) = (&value[..idx], &value[idx + 1..]);
        if p.is_empty() || p.chars().all(|c| c.is_ascii_digit()) {
            if u.set_host(Some(h)).is_ok() {
                if p.is_empty() {
                    let _ = u.set_port(None);
                } else if let Ok(pn) = p.parse::<u16>() {
                    let _ = u.set_port(Some(pn));
                }
            }
            return;
        }
    }
    let _ = u.set_host(Some(value));
}

/// Resolve `href` against optional `base` and return only the serialized
/// absolute URL (no component breakdown). Used by the hot `a.href`/`area.href`
/// getter, which only needs the resolved string, so it avoids building and
/// re-parsing the full component JSON. Returns "" when the input is invalid.
pub(crate) fn url_resolve(href: &str, base: &str) -> String {
    std::panic::catch_unwind(|| {
        let parsed = if base.is_empty() {
            url::Url::parse(href)
        } else {
            url::Url::parse(base).and_then(|b| b.join(href))
        };
        parsed.map(|u| u.as_str().to_string()).unwrap_or_default()
    })
    .unwrap_or_default()
}

/// Canonicalize and validate a `document.domain` assignment.
///
/// Gecko's `Document::IsValidDomain` accepts the current effective host or a
/// dot-delimited suffix no shorter than its registrable domain.  The latter
/// check is important: a plain `ends_with` would let `foo.example.co.uk`
/// relax all the way to `co.uk`, and would incorrectly treat private suffixes
/// such as `github.io` as shared registrable domains.
///
/// An empty return value means SecurityError on the JS side.  The current host
/// is supplied by the Document rather than read from op state because repeated
/// assignments operate on the already-relaxed effective domain.
pub(crate) fn document_domain_candidate(current: &str, input: &str) -> String {
    let canonical = match url::Host::parse(input) {
        Ok(host) => host.to_string().to_ascii_lowercase(),
        Err(_) => return String::new(),
    };
    let current = current.to_ascii_lowercase();

    // Gecko permits assigning the exact current host, including IP literals
    // and single-label hosts.  Neither can be relaxed to a parent.
    if canonical == current {
        return canonical;
    }
    if current.parse::<std::net::IpAddr>().is_ok()
        || canonical.parse::<std::net::IpAddr>().is_ok()
        || !current.ends_with(&format!(".{canonical}"))
    {
        return String::new();
    }

    // `domain_str` is the eTLD+1.  A candidate shorter than it is a public
    // suffix and must not become an effective domain.
    match psl::domain_str(&current) {
        Some(registrable) if canonical.len() >= registrable.len() => canonical,
        _ => String::new(),
    }
}
pub(crate) fn op_add_import_map_inner(
    shared: &SharedState,
    source: String,
    base_url: String,
) -> String {
    let import_map = shared.borrow().import_map.clone();
    let parsed = match ImportMap::parse(&source, &base_url) {
        Ok(map) => map,
        Err(error) => return error,
    };
    let result = match import_map.try_borrow_mut() {
        Ok(mut current) => {
            current.merge(parsed);
            String::new()
        }
        Err(_) => "Import map is already borrowed".to_string(),
    };
    result
}

/// Canonical (lowercased) WHATWG name for a TextDecoder label, or "" if the
/// label is unknown (the JS constructor turns "" into a RangeError).
pub(crate) fn encoding_for_label(label: &str) -> String {
    tinybrowser_net::label_name(label).unwrap_or_default()
}

/// Decode bytes with a legacy/explicit encoding via encoding_rs. Returns
/// {"ok":true,"v":<string>} or {"ok":false} (unknown label, or a fatal decode
/// error). The UTF-8 non-fatal common case is handled in JS without this op.
pub(crate) fn text_decode(
    label: &str,
    bytes: &[u8],
    fatal: bool,
    ignore_bom: bool,
) -> String {
    match tinybrowser_net::decode_with_label(label, bytes, fatal, ignore_bom) {
        Some(s) => serde_json::json!({ "ok": true, "v": s }).to_string(),
        None => "{\"ok\":false}".to_string(),
    }
}

/// Re-encode a URL query component using a non-UTF-8 document encoding override
/// (the WHATWG "encoding override"). `query` is the already-UTF-8-decoded query
/// string; `label` the target charset; `special` whether the URL has a special
/// scheme (adds `'` to the percent-encode set). Returns the encoded query, or
/// the input unchanged if the label is unknown. Only called by the JS anchor
/// path when the document is non-UTF-8, so the UTF-8 hot path never reaches it.
pub(crate) fn url_encode_query(query: &str, label: &str, special: bool) -> String {
    tinybrowser_net::url_encode_query(query, label, special).unwrap_or_else(|| query.to_string())
}

