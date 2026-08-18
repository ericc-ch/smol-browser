use std::cell::RefCell;

use rquickjs::{
    class::Trace,
    function::{Opt, This},
    Class, Ctx, Function, JsLifetime, Object, Result as JsResult, Value,
};
use tinybrowser_dom::{parse_fragment_with_context, DomTree, NodeData, NodeId};

use crate::ops::SharedState;

thread_local! {
    static CURRENT_SHARED: RefCell<Option<SharedState>> = const { RefCell::new(None) };
}

pub fn enter_shared<R>(state: SharedState, f: impl FnOnce() -> R) -> R {
    CURRENT_SHARED.with(|slot| {
        let prev = slot.replace(Some(state));
        let out = f();
        slot.replace(prev);
        out
    })
}

fn current_shared() -> Result<SharedState, String> {
    CURRENT_SHARED.with(|slot| {
        slot.borrow()
            .clone()
            .ok_or_else(|| "DOM bindings have no active runtime state".into())
    })
}

fn wrap<'js>(ctx: Ctx<'js>, state: SharedState, nid: NodeId) -> Result<Class<'js, JsNode>, String> {
    Class::instance(
        ctx,
        JsNode {
            nid: nid.raw(),
            state,
        },
    )
    .map_err(|e| e.to_string())
}

fn js_err(ctor: &'static str, method: &'static str, e: impl std::fmt::Display) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message(ctor, method, e.to_string())
}

#[derive(Trace, JsLifetime)]
#[rquickjs::class(rename = "Node")]
pub struct JsNode {
    nid: u64,
    #[qjs(skip_trace)]
    state: SharedState,
}

impl JsNode {
    fn id(&self) -> NodeId {
        NodeId::from_raw(self.nid)
    }

    fn with_dom<R>(&self, f: impl FnOnce(&DomTree) -> R) -> Option<R> {
        self.state.borrow().dom.as_ref().map(f)
    }

    fn wrap_id<'js>(&self, ctx: Ctx<'js>, nid: NodeId) -> Option<Class<'js, JsNode>> {
        wrap(ctx, self.state.clone(), nid).ok()
    }
}

#[rquickjs::methods]
impl JsNode {
    #[qjs(constructor)]
    pub fn new(nid: Opt<f64>) -> JsResult<Self> {
        let state = current_shared().map_err(|e| js_err("Node", "constructor", e))?;
        Ok(JsNode {
            nid: nid.0.unwrap_or(0.0) as u64,
            state,
        })
    }

    #[qjs(get, rename = "_nid")]
    pub fn nid(&self) -> f64 {
        self.nid as f64
    }

    #[qjs(get, rename = "nodeType")]
    pub fn node_type(&self) -> i32 {
        self.with_dom(|dom| {
            dom.with_node(self.id(), |n| match n.data {
                NodeData::Element { .. } => 1,
                NodeData::Text { .. } => 3,
                NodeData::Comment { .. } => 8,
                NodeData::Document => 9,
                NodeData::Doctype { .. } => 10,
                NodeData::ProcessingInstruction { .. } => 7,
            })
        })
        .flatten()
        .unwrap_or(0)
    }

    #[qjs(get, rename = "nodeName")]
    pub fn node_name(&self) -> String {
        self.with_dom(|dom| {
            dom.with_node(self.id(), |n| match &n.data {
                NodeData::Document => "#document".to_string(),
                NodeData::Element { name, .. } => name.local.as_ref().to_ascii_uppercase(),
                NodeData::Text { .. } => "#text".to_string(),
                NodeData::Comment { .. } => "#comment".to_string(),
                NodeData::Doctype { name, .. } => name.clone(),
                NodeData::ProcessingInstruction { target, .. } => target.clone(),
            })
        })
        .flatten()
        .unwrap_or_default()
    }

    #[qjs(rename = "appendChild")]
    pub fn append_child<'js>(&self, ctx: Ctx<'js>, child: &JsNode) -> JsResult<Class<'js, JsNode>> {
        if let Some(dom) = self.state.borrow().dom.as_ref() {
            dom.append_child(self.id(), child.id());
        }
        wrap(ctx, child.state.clone(), child.id()).map_err(|e| js_err("Node", "appendChild", e))
    }

    #[qjs(rename = "removeChild")]
    pub fn remove_child<'js>(&self, ctx: Ctx<'js>, child: &JsNode) -> JsResult<Class<'js, JsNode>> {
        if let Some(dom) = self.state.borrow().dom.as_ref() {
            let _ = self;
            dom.remove_child(child.id());
        }
        wrap(ctx, child.state.clone(), child.id()).map_err(|e| js_err("Node", "removeChild", e))
    }

    #[qjs(rename = "insertBefore")]
    pub fn insert_before<'js>(
        &self,
        ctx: Ctx<'js>,
        new_child: &JsNode,
        ref_child: Opt<Class<'js, JsNode>>,
    ) -> JsResult<Class<'js, JsNode>> {
        if let Some(dom) = self.state.borrow().dom.as_ref() {
            let _ = self;
            if let Some(r) = ref_child.0 {
                dom.insert_before(r.borrow().id(), new_child.id());
            } else {
                dom.append_child(self.id(), new_child.id());
            }
        }
        wrap(ctx, new_child.state.clone(), new_child.id())
            .map_err(|e| js_err("Node", "insertBefore", e))
    }

    #[qjs(rename = "replaceChild")]
    pub fn replace_child<'js>(
        &self,
        ctx: Ctx<'js>,
        new_child: &JsNode,
        old_child: &JsNode,
    ) -> JsResult<Class<'js, JsNode>> {
        if let Some(dom) = self.state.borrow().dom.as_ref() {
            let _ = self;
            dom.insert_before(old_child.id(), new_child.id());
            dom.remove_child(old_child.id());
        }
        wrap(ctx, new_child.state.clone(), new_child.id())
            .map_err(|e| js_err("Node", "replaceChild", e))
    }

    #[qjs(get, rename = "textContent")]
    pub fn text_content(&self) -> String {
        self.with_dom(|dom| dom.text_content(self.id()))
            .unwrap_or_default()
    }

    #[qjs(set, rename = "textContent")]
    pub fn set_text_content(&self, value: String) {
        let gs = self.state.borrow();
        let Some(dom) = gs.dom.as_ref() else {
            return;
        };
        let is_char_data = dom
            .with_node(self.id(), |n| {
                matches!(
                    n.data,
                    NodeData::Text { .. }
                        | NodeData::Comment { .. }
                        | NodeData::ProcessingInstruction { .. }
                )
            })
            .unwrap_or(false);
        if is_char_data {
            dom.with_node_mut(self.id(), |n| match &mut n.data {
                NodeData::Text { contents }
                | NodeData::Comment { contents }
                | NodeData::ProcessingInstruction { data: contents, .. } => {
                    *contents = value;
                }
                _ => {}
            });
            return;
        }
        for child in dom.children(self.id()) {
            dom.detach(child);
        }
        if !value.is_empty() {
            dom.append_text(self.id(), &value);
        }
    }

    #[qjs(get, rename = "parentNode")]
    pub fn parent_node<'js>(&self, ctx: Ctx<'js>) -> Option<Class<'js, JsNode>> {
        let nid = self.with_dom(|dom| dom.with_node(self.id(), |n| n.parent).flatten())??;
        self.wrap_id(ctx, nid)
    }

    #[qjs(get, rename = "firstChild")]
    pub fn first_child<'js>(&self, ctx: Ctx<'js>) -> Option<Class<'js, JsNode>> {
        let nid = self.with_dom(|dom| dom.with_node(self.id(), |n| n.first_child).flatten())??;
        self.wrap_id(ctx, nid)
    }

    #[qjs(get, rename = "lastChild")]
    pub fn last_child<'js>(&self, ctx: Ctx<'js>) -> Option<Class<'js, JsNode>> {
        let nid = self.with_dom(|dom| dom.with_node(self.id(), |n| n.last_child).flatten())??;
        self.wrap_id(ctx, nid)
    }

    #[qjs(get, rename = "nextSibling")]
    pub fn next_sibling<'js>(&self, ctx: Ctx<'js>) -> Option<Class<'js, JsNode>> {
        let nid = self.with_dom(|dom| dom.with_node(self.id(), |n| n.next_sibling).flatten())??;
        self.wrap_id(ctx, nid)
    }

    #[qjs(get, rename = "previousSibling")]
    pub fn previous_sibling<'js>(&self, ctx: Ctx<'js>) -> Option<Class<'js, JsNode>> {
        let nid = self.with_dom(|dom| dom.with_node(self.id(), |n| n.prev_sibling).flatten())??;
        self.wrap_id(ctx, nid)
    }

    #[qjs(get, rename = "isConnected")]
    pub fn is_connected(&self) -> bool {
        self.with_dom(|dom| dom.is_connected(self.id()))
            .unwrap_or(false)
    }

    #[qjs(rename = "hasChildNodes")]
    pub fn has_child_nodes(&self) -> bool {
        self.with_dom(|dom| {
            dom.with_node(self.id(), |n| n.first_child.is_some())
                .unwrap_or(false)
        })
        .unwrap_or(false)
    }

    #[qjs(rename = "contains")]
    pub fn contains(&self, other: Opt<Class<'_, JsNode>>) -> bool {
        let Some(other) = other.0 else {
            return false;
        };
        let other_id = other.borrow().id();
        self.with_dom(|dom| other_id == self.id() || dom.ancestors(other_id).contains(&self.id()))
            .unwrap_or(false)
    }

    #[qjs(rename = "getAttribute")]
    pub fn get_attribute(&self, name: String) -> Option<String> {
        self.with_dom(|dom| dom.with_node(self.id(), |n| n.get_attribute(&name).map(str::to_owned)))
            .flatten()
            .flatten()
    }

    #[qjs(rename = "setAttribute")]
    pub fn set_attribute(&self, name: String, value: String) {
        if let Some(dom) = self.state.borrow().dom.as_ref() {
            dom.with_node_mut(self.id(), |n| n.set_attribute(&name, value));
        }
    }

    #[qjs(rename = "hasAttribute")]
    pub fn has_attribute(&self, name: String) -> bool {
        self.get_attribute(name).is_some()
    }

    #[qjs(rename = "removeAttribute")]
    pub fn remove_attribute(&self, name: String) {
        if let Some(dom) = self.state.borrow().dom.as_ref() {
            dom.with_node_mut(self.id(), |n| {
                if let Some(attrs) = n.attrs_mut() {
                    attrs.retain(|a| !a.qualified_name_eq(&name));
                }
            });
        }
    }

    #[qjs(rename = "querySelector")]
    pub fn query_selector<'js>(
        &self,
        ctx: Ctx<'js>,
        selector: String,
    ) -> Option<Class<'js, JsNode>> {
        let nid =
            self.with_dom(|dom| dom.query_selector_from(self.id(), &selector).ok().flatten())??;
        self.wrap_id(ctx, nid)
    }

    #[qjs(rename = "querySelectorAll")]
    pub fn query_selector_all<'js>(
        &self,
        ctx: Ctx<'js>,
        selector: String,
    ) -> Vec<Class<'js, JsNode>> {
        let ids = self
            .with_dom(|dom| {
                dom.query_selector_all_from(self.id(), &selector)
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        ids.into_iter()
            .filter_map(|nid| self.wrap_id(ctx.clone(), nid))
            .collect()
    }

    #[qjs(rename = "matches")]
    pub fn matches(&self, selector: String) -> bool {
        self.with_dom(|dom| {
            dom.compile_rule_selector(&selector)
                .map(|compiled| dom.element_matches(self.id(), &compiled))
                .unwrap_or(false)
        })
        .unwrap_or(false)
    }

    #[qjs(rename = "closest")]
    pub fn closest<'js>(&self, ctx: Ctx<'js>, selector: String) -> Option<Class<'js, JsNode>> {
        let nid = {
            let gs = self.state.borrow();
            let dom = gs.dom.as_ref()?;
            let compiled = dom.compile_rule_selector(&selector)?;
            let mut current = Some(self.id());
            let mut found = None;
            while let Some(id) = current {
                if dom.element_matches(id, &compiled) {
                    found = Some(id);
                    break;
                }
                current = dom.with_node(id, |n| n.parent).flatten();
            }
            found
        }?;
        self.wrap_id(ctx, nid)
    }

    #[qjs(rename = "addEventListener")]
    pub fn add_event_listener<'js>(
        this: This<Class<'js, JsNode>>,
        ctx: Ctx<'js>,
        type_: String,
        callback: Value<'js>,
        options: Opt<Value<'js>>,
    ) {
        if let Ok(add) = ctx.globals().get::<_, Function<'_>>("_eventTargetAdd") {
            let _ = add.call::<_, ()>((this.0.clone(), type_, callback, options.0));
            return;
        }
        let _ = this;
        let _ = callback;
    }

    #[qjs(rename = "removeEventListener")]
    pub fn remove_event_listener<'js>(
        this: This<Class<'js, JsNode>>,
        ctx: Ctx<'js>,
        type_: String,
        callback: Value<'js>,
        options: Opt<Value<'js>>,
    ) {
        if let Ok(remove) = ctx.globals().get::<_, Function<'_>>("_eventTargetRemove") {
            let _ = remove.call::<_, ()>((this.0.clone(), type_, callback, options.0));
        }
    }

    #[qjs(rename = "dispatchEvent")]
    pub fn dispatch_event<'js>(
        this: This<Class<'js, JsNode>>,
        ctx: Ctx<'js>,
        event: Value<'js>,
    ) -> bool {
        if let Ok(dispatch) = ctx.globals().get::<_, Function<'_>>("_eventTargetDispatch") {
            return dispatch
                .call::<_, bool>((this.0.clone(), event))
                .unwrap_or(true);
        }
        true
    }

    #[qjs(get, rename = "innerHTML")]
    pub fn inner_html(&self) -> String {
        self.with_dom(|dom| dom.inner_html(self.id()))
            .unwrap_or_default()
    }

    #[qjs(set, rename = "innerHTML")]
    pub fn set_inner_html(&self, html: String) {
        let gs = self.state.borrow();
        let Some(dom) = gs.dom.as_ref() else {
            return;
        };
        if self.id() == dom.document() {
            return;
        }
        for child in dom.children(self.id()) {
            dom.detach(child);
        }
        if html.is_empty() {
            return;
        }
        let context_name = dom
            .with_node(self.id(), |node| match &node.data {
                NodeData::Element { name, .. } => Some(name.clone()),
                _ => None,
            })
            .flatten();
        let Some(name) = context_name else {
            return;
        };
        let fragment = parse_fragment_with_context(&html, name);
        let import_root = fragment.fragment_root();
        dom.import_children_from(self.id(), &fragment, import_root);
    }

    #[qjs(get, rename = "classList")]
    pub fn class_list<'js>(&self, ctx: Ctx<'js>) -> JsResult<Class<'js, JsDOMTokenList>> {
        Class::instance(
            ctx,
            JsDOMTokenList {
                nid: self.nid,
                state: self.state.clone(),
            },
        )
    }

    #[qjs(get)]
    pub fn style<'js>(&self, ctx: Ctx<'js>) -> JsResult<Class<'js, JsCSSStyleDeclaration>> {
        Class::instance(
            ctx,
            JsCSSStyleDeclaration {
                nid: self.nid,
                state: self.state.clone(),
            },
        )
    }
}

#[derive(Trace, JsLifetime)]
#[rquickjs::class(rename = "Event")]
pub struct JsEvent {
    type_: String,
    bubbles: bool,
    cancelable: bool,
}

#[rquickjs::methods]
impl JsEvent {
    #[qjs(constructor)]
    pub fn new(type_: String, init: Opt<Object<'_>>) -> Self {
        let mut bubbles = false;
        let mut cancelable = false;
        if let Some(init) = init.0 {
            bubbles = init.get::<_, bool>("bubbles").unwrap_or(false);
            cancelable = init.get::<_, bool>("cancelable").unwrap_or(false);
        }
        Self {
            type_,
            bubbles,
            cancelable,
        }
    }

    #[qjs(get, rename = "type")]
    pub fn type_(&self) -> String {
        self.type_.clone()
    }

    #[qjs(get)]
    pub fn bubbles(&self) -> bool {
        self.bubbles
    }

    #[qjs(get)]
    pub fn cancelable(&self) -> bool {
        self.cancelable
    }
}

#[derive(Trace, JsLifetime)]
#[rquickjs::class(rename = "CustomEvent")]
pub struct JsCustomEvent {
    type_: String,
    bubbles: bool,
    cancelable: bool,
}

#[rquickjs::methods]
impl JsCustomEvent {
    #[qjs(constructor)]
    pub fn new(type_: String, init: Opt<Object<'_>>) -> Self {
        let mut bubbles = false;
        let mut cancelable = false;
        if let Some(init) = init.0 {
            bubbles = init.get::<_, bool>("bubbles").unwrap_or(false);
            cancelable = init.get::<_, bool>("cancelable").unwrap_or(false);
        }
        Self {
            type_,
            bubbles,
            cancelable,
        }
    }

    #[qjs(get, rename = "type")]
    pub fn type_(&self) -> String {
        self.type_.clone()
    }
}

#[derive(Trace, JsLifetime)]
#[rquickjs::class(rename = "DOMTokenList")]
pub struct JsDOMTokenList {
    nid: u64,
    #[qjs(skip_trace)]
    state: SharedState,
}

impl JsDOMTokenList {
    fn tokens(&self) -> Vec<String> {
        let class = self
            .state
            .borrow()
            .dom
            .as_ref()
            .and_then(|dom| {
                dom.with_node(NodeId::from_raw(self.nid), |n| {
                    n.get_attribute("class").map(str::to_owned)
                })
            })
            .flatten()
            .unwrap_or_default();
        class
            .split_ascii_whitespace()
            .map(str::to_string)
            .filter(|t| !t.is_empty())
            .collect()
    }

    fn write(&self, tokens: &[String]) {
        if let Some(dom) = self.state.borrow().dom.as_ref() {
            dom.with_node_mut(NodeId::from_raw(self.nid), |n| {
                n.set_attribute("class", tokens.join(" "));
            });
        }
    }
}

#[rquickjs::methods]
impl JsDOMTokenList {
    pub fn add(&self, token: String) {
        let mut tokens = self.tokens();
        if !tokens.iter().any(|t| t == &token) {
            tokens.push(token);
            self.write(&tokens);
        }
    }

    pub fn remove(&self, token: String) {
        let mut tokens = self.tokens();
        tokens.retain(|t| t != &token);
        self.write(&tokens);
    }

    pub fn contains(&self, token: String) -> bool {
        self.tokens().iter().any(|t| t == &token)
    }

    pub fn toggle(&self, token: String) -> bool {
        let mut tokens = self.tokens();
        if let Some(idx) = tokens.iter().position(|t| t == &token) {
            tokens.remove(idx);
            self.write(&tokens);
            false
        } else {
            tokens.push(token);
            self.write(&tokens);
            true
        }
    }
}

#[derive(Trace, JsLifetime)]
#[rquickjs::class(rename = "CSSStyleDeclaration")]
pub struct JsCSSStyleDeclaration {
    nid: u64,
    #[qjs(skip_trace)]
    state: SharedState,
}

#[rquickjs::methods]
impl JsCSSStyleDeclaration {
    #[qjs(get, rename = "cssText")]
    pub fn css_text(&self) -> String {
        self.state
            .borrow()
            .dom
            .as_ref()
            .and_then(|dom| {
                dom.with_node(NodeId::from_raw(self.nid), |n| {
                    n.get_attribute("style").map(str::to_owned)
                })
            })
            .flatten()
            .unwrap_or_default()
    }

    #[qjs(set, rename = "cssText")]
    pub fn set_css_text(&self, value: String) {
        if let Some(dom) = self.state.borrow().dom.as_ref() {
            dom.with_node_mut(NodeId::from_raw(self.nid), |n| {
                n.set_attribute("style", value);
            });
        }
    }
}

pub fn register_dom_classes<'js>(ctx: &Ctx<'js>, state: SharedState) -> Result<(), String> {
    let _ = state;
    Class::<JsNode>::define(&ctx.globals()).map_err(|e| e.to_string())?;
    Class::<JsEvent>::define(&ctx.globals()).map_err(|e| e.to_string())?;
    Class::<JsCustomEvent>::define(&ctx.globals()).map_err(|e| e.to_string())?;
    Class::<JsDOMTokenList>::define(&ctx.globals()).map_err(|e| e.to_string())?;
    Class::<JsCSSStyleDeclaration>::define(&ctx.globals()).map_err(|e| e.to_string())?;
    ctx.eval::<(), _>(
        r#"
        Node.ELEMENT_NODE = 1;
        Node.ATTRIBUTE_NODE = 2;
        Node.TEXT_NODE = 3;
        Node.CDATA_SECTION_NODE = 4;
        Node.ENTITY_REFERENCE_NODE = 5;
        Node.ENTITY_NODE = 6;
        Node.PROCESSING_INSTRUCTION_NODE = 7;
        Node.COMMENT_NODE = 8;
        Node.DOCUMENT_NODE = 9;
        Node.DOCUMENT_TYPE_NODE = 10;
        Node.DOCUMENT_FRAGMENT_NODE = 11;
        Node.NOTATION_NODE = 12;
        Node.DOCUMENT_POSITION_DISCONNECTED = 1;
        Node.DOCUMENT_POSITION_PRECEDING = 2;
        Node.DOCUMENT_POSITION_FOLLOWING = 4;
        Node.DOCUMENT_POSITION_CONTAINS = 8;
        Node.DOCUMENT_POSITION_CONTAINED_BY = 16;
        Node.DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC = 32;
        "#,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn wrap_node<'js>(
    ctx: Ctx<'js>,
    state: SharedState,
    nid: NodeId,
) -> Result<Class<'js, JsNode>, String> {
    wrap(ctx, state, nid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::RuntimeState;
    use html5ever::{LocalName, Namespace, QualName};
    use rquickjs::{CatchResultExt, Context, Runtime};
    use std::rc::Rc;
    use tinybrowser_dom::NodeData;

    fn html_element(tree: &DomTree, local: &str) -> NodeId {
        tree.new_node(NodeData::Element {
            name: QualName::new(
                None,
                Namespace::from("http://www.w3.org/1999/xhtml"),
                LocalName::from(local),
            ),
            attrs: vec![],
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
        })
    }

    fn with_tree(tree: DomTree, f: impl FnOnce(Ctx<'_>, SharedState, NodeId, NodeId)) {
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let parent = tree
            .query_selector("div")
            .ok()
            .flatten()
            .unwrap_or_else(|| html_element(&tree, "div"));
        let child = tree
            .query_selector("span")
            .ok()
            .flatten()
            .unwrap_or_else(|| html_element(&tree, "span"));
        let mut runtime_state = RuntimeState::new();
        runtime_state.dom = Some(tree);
        let state: SharedState = Rc::new(RefCell::new(runtime_state));
        ctx.with(|ctx| enter_shared(state.clone(), || f(ctx, state, parent, child)));
    }

    #[test]
    fn native_node_class_reads_and_mutates_dom_without_string_ops() {
        let tree = DomTree::new();
        let doc = tree.document();
        let parent = html_element(&tree, "div");
        let child = html_element(&tree, "span");
        tree.append_child(doc, parent);
        tree.append_child(parent, child);
        tree.with_node_mut(child, |n| n.set_attribute("id", "x".into()));

        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        let mut runtime_state = RuntimeState::new();
        runtime_state.dom = Some(tree);
        let state: SharedState = Rc::new(RefCell::new(runtime_state));

        ctx.with(|ctx| {
            enter_shared(state.clone(), || {
                Class::<JsNode>::define(&ctx.globals()).unwrap();
                let wrapped = wrap_node(ctx.clone(), state.clone(), child).unwrap();
                ctx.globals().set("el", wrapped).unwrap();
                let id: String = ctx.eval(r#"el.getAttribute("id")"#).catch(&ctx).unwrap();
                assert_eq!(id, "x");
                ctx.eval::<(), _>(r#"el.setAttribute("id", "y")"#)
                    .catch(&ctx)
                    .unwrap();
            })
        });

        let id = state
            .borrow()
            .dom
            .as_ref()
            .unwrap()
            .with_node(child, |n| n.get_attribute("id").map(str::to_owned))
            .flatten();
        assert_eq!(id.as_deref(), Some("y"));
    }

    #[test]
    fn native_query_and_classlist_and_inner_html() {
        let tree = DomTree::new();
        let doc = tree.document();
        let parent = html_element(&tree, "div");
        let child = html_element(&tree, "span");
        tree.append_child(doc, parent);
        tree.append_child(parent, child);
        tree.with_node_mut(child, |n| n.set_attribute("id", "x".into()));

        with_tree(tree, |ctx, state, parent, _child| {
            register_dom_classes(&ctx, state.clone()).unwrap();
            let wrapped = wrap_node(ctx.clone(), state.clone(), parent).unwrap();
            ctx.globals().set("el", wrapped).unwrap();
            let found: bool = ctx
                .eval(r#"el.querySelector('#x') !== null"#)
                .catch(&ctx)
                .unwrap();
            assert!(found);
            ctx.eval::<(), _>(r#"el.classList.add("box")"#)
                .catch(&ctx)
                .unwrap();
            ctx.eval::<(), _>(r#"el.innerHTML = "<b>hi</b>""#)
                .catch(&ctx)
                .unwrap();
        });
    }
}
