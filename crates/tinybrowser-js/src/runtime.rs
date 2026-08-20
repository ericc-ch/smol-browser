use std::collections::HashMap;
use std::sync::Arc;

use tinybrowser_dom::{DomTree, NodeId};

/// Cloneable handle the CDP watchdog uses to interrupt hung JavaScript.
/// QuickJS interrupt flag; same method names as the old V8 isolate handle.
pub use crate::quickjs::QuickJsIsolateHandle as IsolateHandle;

use crate::import_map::ImportMap;
use crate::module_loader::ModuleLoadActivity;
use crate::ops::{node_is_script, StoredNetworkResponseBody};
use crate::quickjs::{spawn_quickjs_watchdog, QuickJsRuntime, QuickJsWatchdogToken};

const DEFAULT_CDP_AWAIT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone)]
pub struct RemoteObjectInfo {
    pub js_type: String,
    pub subtype: Option<String>,
    pub class_name: String,
    pub description: String,
    pub object_id: Option<String>,
    pub value: Option<serde_json::Value>,
}

pub struct JsRuntime {
    qjs: QuickJsRuntime,
    object_store: HashMap<String, String>,
    object_counter: u64,
    /// Loader-owned signal for pending dynamic-import graph fetches. This is
    /// intentionally separate from page fetch/XHR activity so analytics does
    /// not hold screenshot readiness open.
    module_load_activity: Arc<ModuleLoadActivity>,
}

/// A fetched module graph whose evaluation is delayed until the HTML script
/// scheduler reaches its post-parse turn.
pub struct PreparedModule {
    specifier: String,
    source: String,
    description: String,
}

fn remaining_deadline_ms(deadline: tokio::time::Instant) -> Option<u64> {
    let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;
    if remaining.is_zero() {
        return None;
    }
    let millis = remaining
        .as_millis()
        .saturating_add(u128::from(remaining.subsec_nanos() % 1_000_000 != 0));
    Some(millis.min(u128::from(u64::MAX)) as u64)
}

pub type WatchdogToken = QuickJsWatchdogToken;

pub fn spawn_watchdog(handle: IsolateHandle, budget: std::time::Duration) -> WatchdogToken {
    spawn_quickjs_watchdog(handle, budget)
}

// Observation deadlines are checked between browser tasks. A task which has
// already started receives this bounded completion allowance, matching the
// fixed-wait path while retaining an absolute backstop for infinite script.
const SYNCHRONOUS_TASK_FLOOR_MS: u64 = 5_000;
const WATCHDOG_SCHEDULING_MARGIN_MS: u64 = 500;

impl JsRuntime {
    /// Freeze the document timeline for one JavaScript task. Browser timelines
    /// update at task/rendering boundaries, not on each forced style or layout
    /// read. Keeping one sample across the task also lets repeated CSSOM reads
    /// share the retained layout on pages with running animations.
    fn begin_javascript_task(&mut self) {}
    pub fn new() -> Self {
        Self::with_base_url("about:blank")
    }

    pub fn with_base_url(base_url: &str) -> Self {
        Self::with_base_url_and_proxy(base_url, None)
    }

    /// Construct a runtime. `proxy_url` is kept so Page can keep calling this
    /// ctor; the ES-module loader and `op_fetch_url` honour the proxy on the
    /// HTTP client installed later via [`Self::set_http_client`].
    pub fn with_base_url_and_proxy(base_url: &str, _proxy_url: Option<String>) -> Self {
        let mut qjs = QuickJsRuntime::new().expect("QuickJS runtime constructs");
        qjs.load_shim().expect("shim loads");
        qjs.set_url(base_url);
        qjs.execute_script(
            "<tinybrowser:init>",
            "globalThis.__tinybrowser_objects = {}; globalThis.__tinybrowser_oid = 0;",
        )
        .expect("init should not fail");
        let module_load_activity = qjs.module_load_activity();
        JsRuntime {
            qjs,
            object_store: HashMap::new(),
            object_counter: 0,
            module_load_activity,
        }
    }

    /// Parse and merge an inline document import map. Rules which would alter
    /// already-observed module resolutions are discarded while unrelated new
    /// rules remain available, matching Chromium's multiple-map model.
    pub fn add_import_map(&self, source: &str, base_url: &str) -> Result<(), String> {
        let map = ImportMap::parse(source, base_url)?;
        self.qjs
            .shared_state()
            .borrow()
            .import_map
            .try_borrow_mut()
            .map_err(|_| "Import map is already borrowed".to_string())?
            .merge(map);
        Ok(())
    }

    pub fn set_cookie_jar(&self, jar: std::sync::Arc<tinybrowser_net::CookieJar>) {
        self.qjs.shared_state().borrow_mut().cookie_jar = Some(jar);
    }

    pub fn set_http_client(&self, client: std::sync::Arc<tinybrowser_net::HttpClient>) {
        self.qjs.shared_state().borrow_mut().http_client = Some(client);
    }

    /// Install the owning page's passive on_request/on_response callback
    /// registry so scripted fetch()/XHR observation is page-scoped (issue #408).
    pub fn set_callbacks(&self, callbacks: std::sync::Arc<tinybrowser_net::CallbackRegistry>) {
        self.qjs.shared_state().borrow_mut().callbacks = Some(callbacks);
    }

    pub fn set_dom(&self, dom: DomTree) {
        let mut gs = self.qjs.shared_state().borrow_mut();
        gs.dom = Some(dom);
        gs.document_generation = gs.document_generation.wrapping_add(1);
        gs.activity_generation = 0;
        gs.page_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        gs.already_started_scripts.borrow_mut().clear();
        // A new document owns a fresh retained scene and resource cache.
    }

    pub fn set_url(&self, url: &str) {
        let mut state = self.qjs.shared_state().borrow_mut();
        if state.url != url {
            state.url = url.to_string();
        }
    }

    /// Set the document's character encoding (WHATWG canonical name). Backs
    /// `document.characterSet` and the `<a>`/`<area>` URL query encoding
    /// override for legacy-charset documents.
    pub fn set_encoding(&self, encoding: &str) {
        self.qjs.shared_state().borrow_mut().encoding = encoding.to_string();
    }

    pub fn set_title(&self, title: &str) {
        self.qjs.shared_state().borrow_mut().title = title.to_string();
    }

    /// Set the source document URL exposed as `document.referrer`. Navigation
    /// owns this value; it is not derived from the current URL because direct
    /// navigations and document-initiated navigations have different
    /// referrer semantics.
    pub fn set_referrer(&self, referrer: &str) {
        self.qjs.shared_state().borrow_mut().referrer = referrer.to_string();
    }

    pub fn set_blocked_urls(&self, patterns: Vec<String>) {
        self.qjs.shared_state().borrow_mut().blocked_urls = patterns;
    }

    pub fn take_pending_navigation(&self) -> Option<(String, String, String)> {
        self.qjs
            .shared_state()
            .borrow_mut()
            .pending_navigation
            .take()
    }

    pub fn take_pending_binding_calls(&self) -> Vec<(String, String)> {
        std::mem::take(&mut self.qjs.shared_state().borrow_mut().pending_binding_calls)
    }

    pub fn get_network_response_body(&self, request_id: &str) -> Option<StoredNetworkResponseBody> {
        self.qjs
            .shared_state()
            .borrow()
            .network_response_bodies
            .get(request_id)
            .cloned()
    }

    pub fn clear_network_response_bodies(&self) {
        let mut state = self.qjs.shared_state().borrow_mut();
        state.network_response_bodies.clear();
        state.network_response_body_order.clear();
    }

    /// Wire up the interception channel without enabling interception.
    /// Use set_intercept_enabled separately. The two were entangled before
    /// and every navigation auto-enabled interception, which made
    /// `fetch()` from page JS hang forever waiting for a CDP client to
    /// answer Fetch.requestPaused events that the client never asked for.
    pub fn set_intercept_tx(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::ops::InterceptedRequest>,
    ) {
        let mut state = self.qjs.shared_state().borrow_mut();
        state.intercept_tx = Some(tx);
    }

    pub fn set_intercept_enabled(&self, enabled: bool) {
        let mut state = self.qjs.shared_state().borrow_mut();
        state.intercept_enabled = enabled;
    }

    pub fn set_user_agent(&mut self, ua: &str) {
        let escaped = ua.replace('\\', "\\\\").replace('\'', "\\'");
        let _ = self.qjs.execute_script(
            "<set-ua>",
            format!("globalThis.__tinybrowser_ua = '{escaped}';"),
        );
    }

    pub fn set_platform(&mut self, platform: &str, ua_platform: &str, ua_platform_version: &str) {
        let p = platform.replace('\'', "\\'");
        let uap = ua_platform.replace('\'', "\\'");
        let uapv = ua_platform_version.replace('\'', "\\'");
        let _ = self.qjs.execute_script(
            "<set-platform>",
            format!(
                "globalThis.__tinybrowser_platform='{p}';globalThis.__tinybrowser_ua_platform='{uap}';globalThis.__tinybrowser_ua_platform_version='{uapv}';"
            ),
        );
    }

    pub fn set_stealth(&mut self, enabled: bool) {
        let _ = self.qjs.execute_script(
            "<set-stealth>",
            format!("globalThis.__tinybrowser_stealth = {enabled};"),
        );
    }

    /// Set the CSS viewport exposed to page JavaScript. This must run before
    /// `run_page_init` for navigation-time responsive code; it may also be
    /// called later by CDP emulation to update the live window surfaces.
    pub fn set_viewport(&mut self, width: f64, height: f64) {
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return;
        }
        let _ = self.qjs.execute_script(
            "<set-viewport>",
            format!(
                "globalThis.__tinybrowser_viewport_w={width};\
                 globalThis.__tinybrowser_viewport_h={height};\
                 globalThis.innerWidth={width};globalThis.innerHeight={height};\
                 if(globalThis.visualViewport){{\
                   globalThis.visualViewport.width={width};\
                   globalThis.visualViewport.height={height};\
                 }}\
                 if(typeof globalThis.__tinybrowser_recompute_intersections==='function'){{\
                   globalThis.__tinybrowser_recompute_intersections();\
                 }}\
                 if(typeof globalThis.__tinybrowser_recompute_resizes==='function'){{\
                   globalThis.__tinybrowser_recompute_resizes();\
                 }}",
            ),
        );
    }

    /// Override the physical screen metrics exposed to page JavaScript.
    /// Unlike the CSS viewport, CDP only changes these when both optional
    /// screen dimensions are supplied. Passing `None` restores the native
    /// screen surface while keeping the viewport override intact.
    pub fn set_screen_size_override(&mut self, size: Option<(f64, f64)>, emulated: bool) {
        let script = match size {
            Some((width, height))
                if width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 =>
            {
                format!(
                    "globalThis.__tinybrowser_set_screen_override({width},{height},{emulated});"
                )
            }
            _ => format!("globalThis.__tinybrowser_set_screen_override(null,null,{emulated});"),
        };
        let _ = self.qjs.execute_script("<set-screen-size>", script);
    }

    /// Run __tinybrowser_init() after all per-page properties (UA, platform, stealth, etc.)
    /// have been set. Must be called once per page setup, after all set_* methods.
    pub fn run_page_init(&mut self) {
        let _ = self.qjs.execute_script(
            "<tinybrowser:page-init>",
            "globalThis.__tinybrowser_init();",
        );
    }

    /// Override the coordinates the navigator.geolocation shim reports. The
    /// values are injected as numeric globals the bootstrap reads; when unset it
    /// keeps the built-in default. Callers validate the range before calling.
    pub fn set_geolocation(&mut self, latitude: f64, longitude: f64) {
        let _ = self.qjs.execute_script(
            "<set-geo>",
            format!(
                "globalThis.__tinybrowser_geo_lat={latitude};globalThis.__tinybrowser_geo_lon={longitude};"
            ),
        );
    }

    pub fn evaluate(&mut self, expression: &str) -> Result<serde_json::Value, String> {
        self.begin_javascript_task();
        self.qjs.evaluate(expression)
    }

    pub async fn evaluate_for_cdp(
        &mut self,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
    ) -> Result<RemoteObjectInfo, String> {
        self.evaluate_for_cdp_with_timeout(
            expression,
            return_by_value,
            await_promise,
            DEFAULT_CDP_AWAIT_TIMEOUT_MS,
        )
        .await
    }

    pub async fn evaluate_for_cdp_with_timeout(
        &mut self,
        expression: &str,
        return_by_value: bool,
        await_promise: bool,
        await_timeout_ms: u64,
    ) -> Result<RemoteObjectInfo, String> {
        if !await_promise && return_by_value {
            let val = self.evaluate(expression)?;
            return Ok(Self::info_from_json(&val));
        }
        self.begin_javascript_task();

        self.object_counter += 1;
        let oid = self.make_oid(self.object_counter);

        // Same trailing-semicolon trim as QuickJsRuntime::wrap_expression —
        // Playwright's utility-script eval ends with `})();`, and `({expr})` would
        // otherwise become `(...;)` which is a parse-time SyntaxError.
        let cleaned_expr = expression
            .trim()
            .trim_end_matches(|c: char| c == ';' || c.is_whitespace());

        // Puppeteer / Playwright bundles end with a `//# sourceURL=...`
        // line comment. If we put `{expr})` on a single line the comment
        // swallows the closing paren and our wrapper breaks. A newline
        // before the `)` terminates any trailing line comment so the
        // parens close on their own line.
        let done_counter = self.object_counter;
        let meta_code = if await_promise {
            format!(
                "(async function() {{\n\
                    try {{\n\
                        var __result = await (\n{expr}\n);\n\
                        globalThis.__tinybrowser_objects['{oid}'] = __result;\n\
                        globalThis.__tinybrowser_await_meta = {meta_fn};\n\
                        globalThis.__tinybrowser_await_rejected = false;\n\
                    }} catch(e) {{\n\
                        globalThis.__tinybrowser_objects['{oid}'] = e;\n\
                        globalThis.__tinybrowser_await_meta = {err_meta_fn};\n\
                        globalThis.__tinybrowser_await_rejected = true;\n\
                    }}\n\
                    globalThis.__tinybrowser_done_{done_counter} = true;\n\
                }})()",
                expr = cleaned_expr,
                oid = oid,
                meta_fn = Self::meta_extract_js("__result"),
                err_meta_fn = Self::meta_extract_js("e"),
                done_counter = done_counter,
            )
        } else {
            format!(
                "(function() {{\n\
                    var __result;\n\
                    try {{ __result = (\n{expr}\n); }} catch(e) {{ __result = undefined; }}\n\
                    globalThis.__tinybrowser_objects['{oid}'] = __result;\n\
                    return {meta_fn};\n\
                }})()",
                expr = cleaned_expr,
                oid = oid,
                meta_fn = Self::meta_extract_js("__result"),
            )
        };

        let meta_str = if await_promise {
            self.qjs
                .execute_script("<eval-remote>", &meta_code)
                .map_err(|e| format!("JS error: {e}"))?;
            let __t0 = std::time::Instant::now();
            let sentinel = format!("globalThis.__tinybrowser_done_{done_counter} === true");
            let settled = self
                .resolve_promises_until(
                    |rt| {
                        rt.qjs
                            .evaluate(&sentinel)
                            .ok()
                            .and_then(|j| j.as_bool())
                            .unwrap_or(false)
                    },
                    await_timeout_ms,
                )
                .await;
            if !settled {
                return Err(format!(
                    "Runtime.evaluate promise did not settle within {await_timeout_ms}ms"
                ));
            }
            let __dt = __t0.elapsed();
            if __dt > std::time::Duration::from_secs(1) {
                let preview: String = expression
                    .chars()
                    .take(200)
                    .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
                    .collect();
                tracing::debug!(
                    "Runtime.evaluate awaitPromise took {}ms; expr={}",
                    __dt.as_millis(),
                    preview,
                );
            }
            let rejected = self
                .qjs
                .evaluate("globalThis.__tinybrowser_await_rejected")
                .map_err(|e| format!("JS error: {e}"))?;
            if rejected.as_bool().unwrap_or(false) {
                let err = self.qjs.evaluate(&format!(
                    "String(globalThis.__tinybrowser_objects['{oid}'] && (globalThis.__tinybrowser_objects['{oid}'].message || globalThis.__tinybrowser_objects['{oid}']))"
                ))
                .map_err(|e| format!("JS error: {e}"))?;
                return Err(format!("Promise rejected: {}", err.as_str().unwrap_or("")));
            }
            self.qjs
                .evaluate("globalThis.__tinybrowser_await_meta")
                .map_err(|e| format!("JS error: {e}"))?
        } else {
            self.qjs
                .evaluate(&meta_code)
                .map_err(|e| format!("JS error: {e}"))?
        };
        let meta_json = if let serde_json::Value::String(s) = &meta_str {
            serde_json::from_str(s).unwrap_or(meta_str)
        } else {
            meta_str
        };
        self.object_store.insert(
            oid.clone(),
            format!("globalThis.__tinybrowser_objects['{oid}']"),
        );

        if await_promise && return_by_value {
            let json_val = self
                .qjs
                .evaluate(&format!("globalThis.__tinybrowser_objects['{oid}']"))
                .map_err(|e| format!("JS error: {e}"))?;
            return Ok(Self::info_from_json(&json_val));
        }

        Ok(Self::info_from_meta(&meta_json, Some(oid)))
    }

    pub async fn call_function_on_for_cdp(
        &mut self,
        function_declaration: &str,
        object_id: Option<&str>,
        arguments: &[serde_json::Value],
        return_by_value: bool,
        await_promise: bool,
    ) -> Result<RemoteObjectInfo, String> {
        self.call_function_on_for_cdp_with_timeout(
            function_declaration,
            object_id,
            arguments,
            return_by_value,
            await_promise,
            DEFAULT_CDP_AWAIT_TIMEOUT_MS,
        )
        .await
    }

    pub async fn call_function_on_for_cdp_with_timeout(
        &mut self,
        function_declaration: &str,
        object_id: Option<&str>,
        arguments: &[serde_json::Value],
        return_by_value: bool,
        await_promise: bool,
        await_timeout_ms: u64,
    ) -> Result<RemoteObjectInfo, String> {
        self.begin_javascript_task();
        let this_expr = self.resolve_this(object_id);
        let (setup, args_list) = self.build_args(arguments);

        self.object_counter += 1;
        let oid = self.make_oid(self.object_counter);

        if await_promise {
            let done_counter = self.object_counter;
            let err_meta_fn = Self::meta_extract_js("__result");
            let code = format!(
                "(async function() {{\n\
                    {setup}\n\
                    var __fn = ({fn_decl});\n\
                    var __this = ({this_expr});\n\
                    var __result;\n\
                    try {{\n\
                        __result = await __fn.call(__this, {args});\n\
                        globalThis.__tinybrowser_objects['{oid}'] = __result;\n\
                        globalThis.__tinybrowser_await_meta = {meta_fn};\n\
                    }} catch(e) {{\n\
                        __result = e;\n\
                        globalThis.__tinybrowser_objects['{oid}'] = e;\n\
                        globalThis.__tinybrowser_await_meta = {err_meta_fn};\n\
                    }} finally {{\n\
                        globalThis.__tinybrowser_done_{done_counter} = true;\n\
                    }}\n\
                }})()",
                setup = setup,
                fn_decl = function_declaration,
                this_expr = this_expr,
                args = args_list,
                oid = oid,
                meta_fn = Self::meta_extract_js("__result"),
                err_meta_fn = err_meta_fn,
                done_counter = done_counter,
            );

            self.qjs
                .execute_script("<callFnAsync>", code)
                .map_err(|e| format!("JS error: {e}"))?;

            let __t0 = std::time::Instant::now();
            let sentinel = format!("globalThis.__tinybrowser_done_{done_counter} === true");
            let settled = self
                .resolve_promises_until(
                    |rt| {
                        rt.qjs
                            .evaluate(&sentinel)
                            .ok()
                            .and_then(|j| j.as_bool())
                            .unwrap_or(false)
                    },
                    await_timeout_ms,
                )
                .await;
            if !settled {
                return Err(format!(
                    "Runtime.callFunctionOn promise did not settle within {await_timeout_ms}ms"
                ));
            }
            let __dt = __t0.elapsed();
            if __dt > std::time::Duration::from_secs(1) {
                let preview: String = function_declaration
                    .chars()
                    .take(300)
                    .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
                    .collect();
                tracing::debug!(
                    "Runtime.callFunctionOn awaitPromise took {}ms; fn={}",
                    __dt.as_millis(),
                    preview,
                );
            }

            if return_by_value {
                let json_val = self
                    .qjs
                    .evaluate(&format!("globalThis.__tinybrowser_objects['{oid}']"))
                    .map_err(|e| format!("JS error: {e}"))?;
                return Ok(Self::info_from_json(&json_val));
            }

            let meta_str = self
                .qjs
                .evaluate("globalThis.__tinybrowser_await_meta")
                .map_err(|e| format!("JS error: {e}"))?;
            let meta_json = if let serde_json::Value::String(s) = &meta_str {
                serde_json::from_str(s).unwrap_or(meta_str.clone())
            } else {
                meta_str
            };
            self.object_store.insert(
                oid.clone(),
                format!("globalThis.__tinybrowser_objects['{oid}']"),
            );
            return Ok(Self::info_from_meta(&meta_json, Some(oid)));
        }

        if return_by_value {
            let code = format!(
                "(function() {{\n\
                    {setup}\n\
                    var __fn = ({function_declaration});\n\
                    var __this = ({this_expr});\n\
                    return __fn.call(__this, {args_list});\n\
                }})()",
            );
            let json_val = self
                .qjs
                .evaluate(&code)
                .map_err(|e| format!("JS error: {e}"))?;
            return Ok(Self::info_from_json(&json_val));
        }

        let code = format!(
            "(function() {{\n\
                {setup}\n\
                var __fn = ({fn_decl});\n\
                var __this = ({this_expr});\n\
                var __result = __fn.call(__this, {args});\n\
                globalThis.__tinybrowser_objects['{oid}'] = __result;\n\
                return {meta_fn};\n\
            }})()",
            setup = setup,
            fn_decl = function_declaration,
            this_expr = this_expr,
            args = args_list,
            oid = oid,
            meta_fn = Self::meta_extract_js("__result"),
        );
        let meta_str = self
            .qjs
            .evaluate(&code)
            .map_err(|e| format!("JS error: {e}"))?;
        let meta_json = if let serde_json::Value::String(s) = &meta_str {
            serde_json::from_str(s).unwrap_or(meta_str.clone())
        } else {
            meta_str
        };
        self.object_store.insert(
            oid.clone(),
            format!("globalThis.__tinybrowser_objects['{oid}']"),
        );
        Ok(Self::info_from_meta(&meta_json, Some(oid)))
    }
    pub async fn call_function_on(
        &mut self,
        function_declaration: &str,
        object_id: Option<&str>,
        arguments: &[serde_json::Value],
        return_by_value: bool,
    ) -> Result<RemoteObjectInfo, String> {
        self.call_function_on_for_cdp(
            function_declaration,
            object_id,
            arguments,
            return_by_value,
            false,
        )
        .await
    }
    pub fn store_object(&mut self, js_expression: &str) -> Result<String, String> {
        self.begin_javascript_task();
        self.object_counter += 1;
        let oid = self.make_oid(self.object_counter);
        let code = format!("globalThis.__tinybrowser_objects['{oid}'] = ({js_expression});",);
        self.qjs
            .execute_script("<store>", code)
            .map_err(|e| format!("Store error: {e}"))?;
        self.object_store.insert(
            oid.clone(),
            format!("globalThis.__tinybrowser_objects['{oid}']"),
        );
        Ok(oid)
    }

    pub fn store_object_with_meta(
        &mut self,
        js_expression: &str,
    ) -> Result<RemoteObjectInfo, String> {
        self.begin_javascript_task();
        self.object_counter += 1;
        let oid = self.make_oid(self.object_counter);
        let code = format!(
            "(function() {{\n\
                var __result = (\n{expr}\n);\n\
                globalThis.__tinybrowser_objects['{oid}'] = __result;\n\
                return {meta_fn};\n\
            }})()",
            expr = js_expression,
            oid = oid,
            meta_fn = Self::meta_extract_js("__result"),
        );
        let meta_str = self
            .qjs
            .evaluate(&code)
            .map_err(|e| format!("Store error: {e}"))?;
        let meta_json = if let serde_json::Value::String(s) = &meta_str {
            serde_json::from_str(s).unwrap_or(meta_str.clone())
        } else {
            meta_str
        };
        self.object_store.insert(
            oid.clone(),
            format!("globalThis.__tinybrowser_objects['{oid}']"),
        );
        Ok(Self::info_from_meta(&meta_json, Some(oid)))
    }

    pub fn release_object(&mut self, object_id: &str) {
        if self.object_store.remove(object_id).is_some() {
            let code = format!("delete globalThis.__tinybrowser_objects['{object_id}'];");
            let _ = self.qjs.execute_script("<release>", code);
        }
    }

    pub fn release_object_group(&mut self) {
        let _ = self
            .qjs
            .execute_script("<releaseGroup>", "globalThis.__tinybrowser_objects = {};");
        self.object_store.clear();
    }
    pub async fn load_module(&mut self, url: &str, budget_ms: u64) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(budget_ms);
        let prepared = self.prepare_module(url, budget_ms).await?;
        let remaining_ms = remaining_deadline_ms(deadline).ok_or_else(|| {
            format!("Module {url} exhausted its {budget_ms}ms load+evaluation budget")
        })?;
        self.evaluate_prepared_module(prepared, remaining_ms).await
    }

    pub async fn prepare_module(
        &mut self,
        url: &str,
        budget_ms: u64,
    ) -> Result<PreparedModule, String> {
        let source = self.fetch_module_source(url, budget_ms)?;
        Ok(PreparedModule {
            specifier: url.to_string(),
            source,
            description: format!("Module {url}"),
        })
    }

    fn fetch_module_source(&mut self, url: &str, budget_ms: u64) -> Result<String, String> {
        self.qjs
            .fetch_module_source(url, std::time::Duration::from_millis(budget_ms.max(1)))
    }

    pub async fn prepare_inline_module(
        &mut self,
        code: &str,
        base_url: &str,
        _budget_ms: u64,
    ) -> Result<PreparedModule, String> {
        Ok(PreparedModule {
            specifier: base_url.to_string(),
            source: code.to_string(),
            description: "Inline module".to_string(),
        })
    }

    pub async fn evaluate_prepared_module(
        &mut self,
        prepared: PreparedModule,
        budget_ms: u64,
    ) -> Result<(), String> {
        let PreparedModule {
            specifier,
            source,
            description,
        } = prepared;
        let watchdog = self.arm_watchdog(std::time::Duration::from_millis(budget_ms));
        let import_meta = (description == "Inline module").then_some(specifier.as_str());
        let result = self
            .qjs
            .eval_module(&specifier, &source, import_meta, budget_ms);
        let watchdog_fired = self.disarm_watchdog(watchdog);
        if watchdog_fired || result.as_ref().is_err_and(|err| err == "timeout") {
            Err(format!(
                "{description} evaluation timed out after {budget_ms}ms"
            ))
        } else {
            match result {
                Ok(()) => Ok(()),
                Err(err) if err.starts_with("load:") => Err(format!(
                    "{} load error: {}",
                    description,
                    err.trim_start_matches("load:").trim()
                )),
                Err(err) if err.starts_with("eval:") => Err(format!(
                    "{} eval error: {}",
                    description,
                    err.trim_start_matches("eval:").trim()
                )),
                Err(err) => Err(format!("{description}: {err}")),
            }
        }
    }

    pub async fn load_inline_module(
        &mut self,
        code: &str,
        base_url: &str,
        budget_ms: u64,
    ) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(budget_ms);
        let prepared = self
            .prepare_inline_module(code, base_url, budget_ms)
            .await?;
        let remaining_ms = remaining_deadline_ms(deadline).ok_or_else(|| {
            format!("Inline module exhausted its {budget_ms}ms load+evaluation budget")
        })?;
        self.evaluate_prepared_module(prepared, remaining_ms).await
    }

    fn execute_classic_script(&mut self, name: &str, source: &str) -> Result<(), String> {
        self.begin_javascript_task();
        self.qjs.execute_script(name, source)?;
        self.qjs.drain_pending_jobs()
    }

    /// Parser/lifecycle scripts must not drain `import()` jobs: the QuickJS
    /// module loader fetches synchronously, so a checkpoint would turn a
    /// dynamic import into an implicit navigation settle.
    pub fn execute_script_no_checkpoint(&mut self, name: &str, source: &str) -> Result<(), String> {
        self.begin_javascript_task();
        self.qjs.execute_script(name, source)
    }

    pub fn execute_script(&mut self, name: &str, source: &str) -> Result<(), String> {
        self.execute_classic_script(name, source)
    }

    pub fn execute_script_guarded(&mut self, name: &str, source: &str) -> Result<(), String> {
        let result = self.qjs.execute_script_guarded(name, source);
        let _ = self.qjs.drain_pending_jobs();
        result
    }

    pub fn execute_script_guarded_no_checkpoint(
        &mut self,
        name: &str,
        source: &str,
    ) -> Result<(), String> {
        self.qjs.execute_script_guarded(name, source)
    }

    pub fn drain_pending_jobs(&mut self) -> Result<(), String> {
        self.qjs.drain_pending_jobs()
    }

    pub fn execute_script_with_timeout(
        &mut self,
        name: &str,
        source: &str,
        timeout: std::time::Duration,
    ) -> Result<(), String> {
        if timeout.is_zero() {
            return self.execute_classic_script(name, source);
        }
        let token = self.arm_watchdog(timeout);
        let result = self.execute_classic_script(name, source);
        let fired = self.disarm_watchdog(token);
        match result {
            Ok(()) => Ok(()),
            Err(msg) => {
                if fired || msg.contains("interrupted") || msg.contains("execution terminated") {
                    tracing::warn!("Script killed after {}s timeout", timeout.as_secs());
                    Ok(())
                } else {
                    Err(msg)
                }
            }
        }
    }

    pub async fn run_event_loop(&mut self) -> Result<(), String> {
        self.begin_javascript_task();
        self.run_cooperative_event_loop_tick().await.map(|_| ())
    }

    /// Whether the serialized dynamic-script queue is still fetching or
    /// evaluating a script. The queue stays private to the bootstrap closure;
    /// Rust reads it through a hidden status function so page declarations
    /// cannot collide with or overwrite the queue itself.
    pub fn has_pending_dynamic_scripts(&mut self) -> bool {
        let pending_dom_script = self
            .evaluate("globalThis.__tinybrowser_hasPendingDynamicScripts?.() === true")
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        pending_dom_script
            || self
                .module_load_activity
                .is_pending_or_recent(std::time::Duration::from_millis(100))
    }

    /// Whether a connected dynamic script prepared before the document load
    /// event still has fetch/evaluation/load-or-error work outstanding.
    ///
    /// This intentionally excludes `import()` and scripts created by a load
    /// handler. Those are ordinary post-load enhancement work and should only
    /// be driven when an automation caller explicitly asks the page to settle.
    pub fn has_pending_load_delaying_scripts(&mut self) -> bool {
        self.evaluate("globalThis.__tinybrowser_hasPendingLoadDelayingScripts?.() === true")
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
    }

    /// Generation of observable connected-document mutations. This excludes
    /// detached-tree construction and no-op writes, which cannot affect a
    /// screenshot or DOM dump.
    pub fn activity_generation(&self) -> u64 {
        self.qjs.shared_state().borrow().activity_generation
    }

    fn has_pending_network_requests(&self) -> bool {
        let state = self.qjs.shared_state().borrow();
        state
            .page_in_flight
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0
    }

    fn next_pending_timeout_delay_ms(&mut self) -> Option<f64> {
        self.evaluate("globalThis.__tinybrowser_nextPendingTimeoutDelay?.() ?? -1")
            .ok()
            .and_then(|value| value.as_f64())
            .filter(|delay| *delay >= 0.0)
    }

    /// Arm a hard wall-clock backstop on synchronous JavaScript. A page stuck
    /// in a tight loop pins the OS thread inside QuickJS, so
    /// `tokio::time::timeout` (which can only cancel at await points) never
    /// fires. This spawns a watchdog thread that sets the isolate interrupt
    /// flag once `budget` elapses, forcing QuickJS to throw and hand control
    /// back. Always balance with [`Self::disarm_watchdog`].
    pub fn arm_watchdog(&mut self, budget: std::time::Duration) -> WatchdogToken {
        self.qjs.arm_watchdog(budget)
    }

    /// Stop a watchdog armed by [`Self::arm_watchdog`]. If it had already fired
    /// (terminated the isolate), clear the interrupt flag so the isolate is
    /// usable again, and return `true`.
    pub fn disarm_watchdog(&mut self, token: WatchdogToken) -> bool {
        self.qjs.disarm_watchdog(token)
    }

    /// This runtime's interrupt handle (captured at construction, stable for
    /// the isolate's life). Lets the CDP dispatcher arm a per-command watchdog
    /// from `&self`.
    pub fn isolate_handle(&self) -> IsolateHandle {
        self.qjs.isolate_handle()
    }

    /// Clear the interrupt flag after a watchdog armed externally (via the
    /// isolate handle) fired, so the isolate is usable for the next command.
    pub fn cancel_termination(&mut self) {
        self.qjs.cancel_termination();
    }

    /// Drive the event loop for at most `budget_ms`, bounded against BOTH async
    /// idle (Tokio deadline) and synchronous hangs (QuickJS interrupt). The
    /// deadline is observed between browser tasks; a task already running there
    /// gets a five-second completion allowance plus a 500ms scheduling margin
    /// before the interrupt fires. A well-behaved page returns as soon as the
    /// loop goes idle.
    pub async fn run_event_loop_bounded(&mut self, budget_ms: u64) -> Result<(), String> {
        if budget_ms == 0 {
            return self.run_event_loop().await;
        }
        self.begin_javascript_task();
        let token = self.arm_watchdog(
            std::time::Duration::from_millis(budget_ms)
                + std::time::Duration::from_millis(SYNCHRONOUS_TASK_FLOOR_MS)
                + std::time::Duration::from_millis(WATCHDOG_SCHEDULING_MARGIN_MS),
        );
        let result = self.qjs.run_event_loop_bounded(budget_ms);
        let fired = self.disarm_watchdog(token);
        match result {
            Err(error)
                if fired
                    || error.contains("interrupted")
                    || error.contains("execution terminated") =>
            {
                Ok(())
            }
            other => other,
        }
    }

    /// Drive page tasks for a fixed observation interval without a run-to-idle
    /// pump owning that entire interval.
    ///
    /// Modern schedulers commonly keep the event loop continuously ready with
    /// animation frames, zero-delay tasks, or streaming work. A single
    /// `run_event_loop()` poll then never yields to Tokio, so the fixed-delay
    /// deadline can only be enforced by terminating otherwise valid page JS.
    /// Cooperative turns preserve the requested wall interval while returning
    /// to the embedder between task-queue wakes. The watchdog remains solely as
    /// a backstop for one genuinely synchronous, unyielding turn.
    pub async fn run_event_loop_for_duration(&mut self, budget_ms: u64) -> Result<(), String> {
        if budget_ms == 0 {
            return Ok(());
        }
        self.run_event_loop_bounded(budget_ms).await
    }

    /// Drive one cooperative event-loop tick. When work is due, pump it; when
    /// the next wait is a timer or fetch, sleep until that instant, then pump
    /// again. Yields even if that tick schedules more work so Tokio can observe
    /// timeouts and readiness policy.
    async fn run_cooperative_event_loop_tick(&mut self) -> Result<bool, String> {
        self.begin_javascript_task();
        let _ = self.qjs.pump_ready()?;
        if self.qjs.is_idle() {
            return Ok(true);
        }
        if let Some(deadline) = self.qjs.next_wait_instant() {
            let wait = deadline.saturating_duration_since(std::time::Instant::now());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            let _ = self.qjs.pump_ready()?;
        }
        Ok(self.qjs.is_idle())
    }

    /// Drive one browser task while allowing the future to remain parked on
    /// the next timer or fetch. This is the long-lived browser server
    /// counterpart to bounded screenshot settling: the owner selects this
    /// future alongside incoming protocol commands, so a page continues to make
    /// progress while the automation client is idle without polling at a fixed
    /// frequency.
    ///
    /// The shared CDP watchdog is armed only around synchronous JavaScript.
    /// It is deliberately disarmed while `Poll::Pending`; a legitimate distant
    /// timer must not look like a hung JavaScript task merely because the
    /// runtime is asleep waiting for it.
    #[doc(hidden)]
    pub async fn run_autonomous_event_loop_turn(&mut self) -> Result<bool, String> {
        self.run_cooperative_event_loop_tick().await
    }

    /// Drive one cooperative event-loop turn for browser lifecycle code that
    /// must re-check an external readiness predicate after every wake. The
    /// boolean is true only when host queues and pending JS jobs are idle.
    #[doc(hidden)]
    pub async fn run_load_delaying_event_loop_tick(&mut self) -> Result<bool, String> {
        self.run_cooperative_event_loop_tick().await
    }

    /// Pump deferred work until host queues and pending JS jobs are idle, or
    /// until the page has had no connected-document mutation, relevant
    /// request/dynamic-script work, or near-term one-shot timeout for
    /// `quiet_ms`. Network and script work gets a bounded post-load grace
    /// period: this retains ordinary app hydration without allowing analytics,
    /// telemetry, or a hung endpoint to consume the caller's complete budget.
    /// Long timers and perpetual visual mutations are bounded separately for
    /// the same reason. `budget_ms` remains an absolute wall-clock bound.
    pub async fn run_event_loop_until_quiescent(
        &mut self,
        budget_ms: u64,
        quiet_ms: u64,
    ) -> Result<(), String> {
        if budget_ms == 0 {
            return Ok(());
        }

        let budget = std::time::Duration::from_millis(budget_ms);
        let quiet = std::time::Duration::from_millis(quiet_ms.max(1).min(budget_ms));
        let started = tokio::time::Instant::now();
        let deadline = started + budget;
        // A one-second grace covers the common load -> fetch -> framework
        // commit path (and matches the CLI's established one-second useful
        // hydration window), but it is intentionally independent of a larger
        // caller budget. Requests which remain pending after this point are no
        // longer readiness evidence by themselves. Their eventual connected
        // DOM mutation is still observed during the bounded activity tail.
        const EXTERNAL_WORK_GRACE_MS: u64 = 1_000;
        const OBSERVABLE_ACTIVITY_TAIL_MS: u64 = 500;
        let external_work_grace =
            std::time::Duration::from_millis(EXTERNAL_WORK_GRACE_MS).min(budget);
        let external_work_deadline = started + external_work_grace;
        let activity_tail = std::time::Duration::from_millis(OBSERVABLE_ACTIVITY_TAIL_MS);
        let mut activity_deadline = deadline.min(started + activity_tail);
        let token = self.arm_watchdog(
            budget.saturating_add(std::time::Duration::from_millis(SYNCHRONOUS_TASK_FLOOR_MS))
                + std::time::Duration::from_millis(WATCHDOG_SCHEDULING_MARGIN_MS),
        );
        let mut generation = self.activity_generation();
        let mut quiet_since: Option<tokio::time::Instant> = None;
        let result = loop {
            let now = tokio::time::Instant::now();
            let Some(_remaining) = deadline.checked_duration_since(now) else {
                break Ok(());
            };
            let next_generation = self.activity_generation();
            // One-shot timers up to two quiet windows away are commonly app
            // hydration/debounce work (`setTimeout(render, 200)`). Intervals
            // are intentionally excluded, and distant one-shots are treated
            // like Chromium after `load`: callers needing an arbitrary fixed
            // delay can request strict settle.
            let near_timeout = self
                .next_pending_timeout_delay_ms()
                .is_some_and(|delay| delay <= quiet.as_secs_f64() * 2_000.0);
            let external_work_pending = now < external_work_deadline
                && (self.has_pending_network_requests() || self.has_pending_dynamic_scripts());
            if external_work_pending {
                activity_deadline = deadline.min(external_work_deadline + activity_tail);
                generation = next_generation;
                quiet_since = None;
            } else if now < activity_deadline && near_timeout {
                generation = next_generation;
                quiet_since = None;
            } else {
                if now < activity_deadline && next_generation != generation {
                    // A mutation starts a fresh quiet interval at its observed
                    // delivery time. There is no need for a fixed-rate poll to
                    // discover that the interval has begun.
                    quiet_since = Some(now);
                }
                generation = next_generation;
                let since = quiet_since.get_or_insert(now);
                if now.duration_since(*since) >= quiet {
                    break Ok(());
                }
            }

            // Park on the runtime's actual waker. The policy deadline is only
            // a fallback for a hung request, a quiet-window expiry, or the
            // caller's absolute budget; it is not a periodic polling quantum.
            let policy_deadline = if external_work_pending {
                external_work_deadline
            } else if now < activity_deadline && near_timeout {
                activity_deadline
            } else {
                quiet_since.map_or(deadline, |since| since + quiet)
            }
            .min(deadline);
            // One cooperative tick may still run a long chain of timers and
            // microtasks. Tokio's deadline cannot preempt that native call.
            // Bound the individual turn beyond the readiness horizon by the
            // same bounded task allowance as fixed waits.
            let tick_watchdog = self.arm_watchdog(
                policy_deadline.saturating_duration_since(now)
                    + std::time::Duration::from_millis(SYNCHRONOUS_TASK_FLOOR_MS)
                    + std::time::Duration::from_millis(WATCHDOG_SCHEDULING_MARGIN_MS),
            );
            let tick =
                tokio::time::timeout_at(policy_deadline, self.run_cooperative_event_loop_tick())
                    .await;
            let tick_fired = self.disarm_watchdog(tick_watchdog);
            if tick_fired {
                break Ok(());
            }
            self.qjs.run_event_loop_bounded(1)?;
            match tick {
                Ok(Ok(true)) => break Ok(()),
                Ok(Ok(false)) | Err(_) => {}
                Ok(Err(error)) => break Err(error),
            }
        };
        let fired = self.disarm_watchdog(token);
        match result {
            Err(error) if fired || error.contains("execution terminated") => Ok(()),
            other => other,
        }
    }

    /// Like [`Self::evaluate`] but bounded by the isolate watchdog, so a
    /// `--eval` expression that loops forever cannot hang the process.
    pub fn evaluate_with_timeout(
        &mut self,
        expression: &str,
        timeout: std::time::Duration,
    ) -> Result<serde_json::Value, String> {
        if timeout.is_zero() {
            return self.evaluate(expression);
        }
        self.begin_javascript_task();
        let token = self.arm_watchdog(timeout);
        let result = self.qjs.evaluate(expression);
        let fired = self.disarm_watchdog(token);
        match result {
            Ok(v) if !fired => Ok(v),
            Ok(_) => Err("eval timed out".to_string()),
            Err(e) => {
                let msg = e;
                if fired || msg.contains("interrupted") || msg.contains("execution terminated") {
                    Err("eval timed out".to_string())
                } else {
                    Err(format!("JS error: {msg}"))
                }
            }
        }
    }

    pub async fn resolve_promises(&mut self) {
        self.begin_javascript_task();
        let _ = self.qjs.run_event_loop_bounded(5_000);
    }

    /// Pump the event loop until `done_check` returns true (e.g. an IIFE
    /// has written its result sentinel), or `max_total_ms` elapses. Returns
    /// whether the predicate completed before the deadline.
    ///
    /// Why this exists: a run-to-idle pump only returns when there is
    /// no pending work. Page JS routinely schedules long setTimeouts
    /// (IntersectionObserver re-fires at 7s, requestIdleCallback, etc.) that
    /// the caller does not care about. With the plain timeout we waited 5s
    /// even when the IIFE we cared about resolved in <1ms — the click flow
    /// added ~7s per click because Puppeteer's `isIntersectingViewport`
    /// disconnects its observer in the callback, but our scheduled
    /// re-fires keep the event loop "busy" until they all fire.
    pub async fn resolve_promises_until<F>(&mut self, mut done_check: F, max_total_ms: u64) -> bool
    where
        F: FnMut(&mut Self) -> bool,
    {
        let deadline =
            tokio::time::Instant::now() + tokio::time::Duration::from_millis(max_total_ms);
        let mut tick_ms: u64 = 1;
        loop {
            self.begin_javascript_task();
            if done_check(self) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            // Pump for a short slice. If the loop returns idle in <tick_ms,
            // run_event_loop returns Ok and we check the predicate again.
            let _ = tokio::time::timeout(
                tokio::time::Duration::from_millis(tick_ms.max(1)),
                self.run_cooperative_event_loop_tick(),
            )
            .await;
            // Backoff so a hung promise doesn't burn CPU. Caps at 50ms;
            // worst case we miss the result by <50ms.
            if tick_ms < 50 {
                tick_ms = (tick_ms * 2).min(50);
            }
        }
    }
    pub fn take_dom(&self) -> Option<DomTree> {
        let mut state = self.qjs.shared_state().borrow_mut();
        state.dom.take()
    }

    /// Export document-owned script preparation state before the runtime realm
    /// is temporarily destroyed.  Page suspension keeps the DOM alive, so the
    /// HTML "already started" flags must travel with it rather than resetting
    /// like window-global JavaScript state.
    pub fn started_script_ids(&self) -> Vec<u64> {
        let state = self.qjs.shared_state().borrow();
        let mut ids = state
            .already_started_scripts
            .borrow()
            .iter()
            .map(|node_id| node_id.raw())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    /// Restore script preparation state only onto script nodes in the current
    /// DOM.  Callers use this exclusively for the same DomTree surviving a
    /// suspend/resume cycle; normal set_dom navigation starts from an empty set.
    pub fn restore_started_script_ids(&self, ids: &[u64]) {
        let state = self.qjs.shared_state().borrow();
        let Some(dom) = state.dom.as_ref() else {
            return;
        };
        let valid = ids
            .iter()
            .copied()
            .map(NodeId::new)
            .filter(|node_id| node_is_script(dom, *node_id))
            .collect::<Vec<_>>();
        state.already_started_scripts.borrow_mut().extend(valid);
    }

    pub fn with_dom<R>(&self, f: impl FnOnce(&DomTree) -> R) -> Option<R> {
        let state = self.qjs.shared_state().borrow();
        state.dom.as_ref().map(f)
    }

    /// Absolute URLs the page requested via fetch()/XHR, in request order
    /// (issue #301). Backs `--dump assets`.
    pub fn fetched_urls(&self) -> Vec<String> {
        self.qjs.shared_state().borrow().fetched_urls.clone()
    }

    /// Drain the network events recorded for script-initiated requests
    /// (fetch/XHR/dynamic resource). The Page moves these into its own
    /// network_events so the CDP layer emits Network events for them (#406).
    pub fn take_js_network_events(&self) -> Vec<crate::ops::JsNetworkEvent> {
        std::mem::take(&mut self.qjs.shared_state().borrow_mut().js_network_events)
    }

    pub fn dom_ref(&self) -> Option<std::cell::Ref<'_, Option<DomTree>>> {
        let r = self.qjs.shared_state().borrow();
        if r.dom.is_some() {
            Some(std::cell::Ref::map(r, |s| &s.dom))
        } else {
            None
        }
    }
    fn make_oid(&self, counter: u64) -> String {
        format!("{{\"injectedScriptId\":1,\"id\":{counter}}}")
    }

    fn meta_extract_js(var_name: &str) -> String {
        format!(
            r"(function(v) {{
                var t = typeof v;
                var st = null, cn = '', desc = '';
                if (v === null) {{ t = 'object'; st = 'null'; }}
                else if (v === undefined) {{ t = 'undefined'; }}
                else if (Array.isArray(v)) {{
                    st = 'array'; cn = 'Array';
                    desc = 'Array(' + v.length + ')';
                }}
                else if (t === 'object' && typeof v._nid === 'number') {{
                    st = 'node';
                    cn = v.constructor ? v.constructor.name : 'Node';
                    if (v.nodeType === 9) cn = 'HTMLDocument';
                    else if (v.nodeType === 1) cn = 'HTML' + (v.tagName || 'Element').charAt(0) + (v.tagName || 'Element').slice(1).toLowerCase() + 'Element';
                    desc = v.tagName ? v.tagName.toLowerCase() : (v.nodeName || 'node');
                }}
                else if (t === 'function') {{
                    cn = 'Function';
                    desc = v.name ? 'function ' + v.name + '()' : 'function()';
                }}
                else if (t === 'object') {{
                    cn = (v.constructor && v.constructor.name) || 'Object';
                    desc = cn;
                }}
                else {{ desc = String(v); }}
                return JSON.stringify({{type:t,subtype:st,className:cn,description:desc}});
            }})({var_name})",
        )
    }

    fn resolve_this(&self, object_id: Option<&str>) -> String {
        match object_id {
            Some(oid) => {
                if let Some(retrieval) = self.object_store.get(oid) {
                    retrieval.clone()
                } else if oid.starts_with("node-") {
                    let nid = oid.strip_prefix("node-").unwrap_or("0");
                    format!(
                        "(function() {{ \
                            var nid = {nid}; \
                            var cache = globalThis._cache || new Map(); \
                            if (cache.has(nid)) return cache.get(nid); \
                            return null; \
                        }})()"
                    )
                } else {
                    "globalThis".to_string()
                }
            }
            None => "globalThis".to_string(),
        }
    }

    fn build_args(&self, arguments: &[serde_json::Value]) -> (String, String) {
        let mut setup_lines = Vec::new();
        let mut arg_names = Vec::new();

        for (i, arg) in arguments.iter().enumerate() {
            let arg_name = format!("__arg{i}");
            if let Some(value) = arg.get("value") {
                let json_str =
                    serde_json::to_string(value).unwrap_or_else(|_| "undefined".to_string());
                setup_lines.push(format!("var {arg_name} = {json_str};"));
            } else if let Some(oid) = arg.get("objectId").and_then(|v| v.as_str()) {
                if let Some(retrieval) = self.object_store.get(oid) {
                    setup_lines.push(format!("var {arg_name} = {retrieval};"));
                } else {
                    setup_lines.push(format!("var {arg_name} = undefined;"));
                }
            } else if let Some(unser) = arg.get("unserializableValue").and_then(|v| v.as_str()) {
                setup_lines.push(format!("var {arg_name} = {unser};"));
            } else {
                setup_lines.push(format!("var {arg_name} = undefined;"));
            }
            arg_names.push(arg_name);
        }

        (setup_lines.join("\n"), arg_names.join(", "))
    }

    fn info_from_json(value: &serde_json::Value) -> RemoteObjectInfo {
        match value {
            serde_json::Value::Null => RemoteObjectInfo {
                js_type: "object".into(),
                subtype: Some("null".into()),
                class_name: String::new(),
                description: "null".into(),
                object_id: None,
                value: Some(serde_json::Value::Null),
            },
            serde_json::Value::Bool(b) => RemoteObjectInfo {
                js_type: "boolean".into(),
                subtype: None,
                class_name: String::new(),
                description: b.to_string(),
                object_id: None,
                value: Some(value.clone()),
            },
            serde_json::Value::Number(n) => RemoteObjectInfo {
                js_type: "number".into(),
                subtype: None,
                class_name: String::new(),
                description: n.to_string(),
                object_id: None,
                value: Some(value.clone()),
            },
            serde_json::Value::String(s) => RemoteObjectInfo {
                js_type: "string".into(),
                subtype: None,
                class_name: String::new(),
                description: s.clone(),
                object_id: None,
                value: Some(value.clone()),
            },
            serde_json::Value::Array(arr) => RemoteObjectInfo {
                js_type: "object".into(),
                subtype: Some("array".into()),
                class_name: "Array".into(),
                description: format!("Array({})", arr.len()),
                object_id: None,
                value: Some(value.clone()),
            },
            serde_json::Value::Object(_) => RemoteObjectInfo {
                js_type: "object".into(),
                subtype: None,
                class_name: "Object".into(),
                description: "Object".into(),
                object_id: None,
                value: Some(value.clone()),
            },
        }
    }

    fn info_from_meta(meta: &serde_json::Value, object_id: Option<String>) -> RemoteObjectInfo {
        let js_type = meta
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("undefined")
            .to_string();
        let subtype = meta
            .get("subtype")
            .and_then(|v| v.as_str())
            .map(std::string::ToString::to_string);
        let class_name = meta
            .get("className")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let description = meta
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let value = if js_type != "object" && js_type != "function" {
            meta.get("description")
                .and_then(|v| v.as_str())
                .map(|s| serde_json::Value::String(s.to_string()))
        } else {
            None
        };

        RemoteObjectInfo {
            js_type,
            subtype,
            class_name,
            description,
            object_id,
            value,
        }
    }
}

impl Default for JsRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tinybrowser_dom::parse_html;

    fn setup_runtime(html: &str) -> JsRuntime {
        let dom = parse_html(html);
        let mut rt = JsRuntime::new();
        rt.set_dom(dom);
        rt.set_url("http://example.com/test");
        rt.set_title("Test Page");
        rt.run_page_init();
        rt
    }

    #[test]
    fn iframe_content_window_exposes_realm_globals() {
        let mut rt = setup_runtime("<html><body></body></html>");

        assert_eq!(
            rt.evaluate(
                r#"(() => {
                    const iframe = document.createElement("iframe");
                    document.body.appendChild(iframe);
                    const child = iframe.contentWindow;
                    const names = [
                        "Object", "Function", "Error", "Promise", "Proxy",
                        "XMLHttpRequest", "Worker", "Blob", "FormData",
                        "WebSocket", "MutationObserver",
                    ];
                    return {
                        types: names.map(name => typeof child[name]),
                        separate: [
                            child.Object !== Object,
                            child.Promise !== Promise,
                            child.XMLHttpRequest !== XMLHttpRequest,
                            child.Math !== Math,
                        ],
                        constructible: [
                            new child.Object() instanceof child.Object,
                            new child.Promise(resolve => resolve()) instanceof child.Promise,
                            new child.XMLHttpRequest() instanceof child.XMLHttpRequest,
                            new child.Blob([]) instanceof child.Blob,
                            new child.FormData() instanceof child.FormData,
                            new child.MutationObserver(() => {}) instanceof child.MutationObserver,
                        ],
                        utilities: [
                            child.Object.keys({ first: 1 })[0] === "first",
                            child.Array.isArray([]),
                            child.Promise.resolve(1) instanceof child.Promise,
                            child.Function("return 7")() === 7,
                            Object.getOwnPropertyNames(child).includes("XMLHttpRequest"),
                            child.globalThis === child,
                        ],
                    };
                })()"#,
            )
            .unwrap(),
            serde_json::json!({
                "types": vec!["function"; 11],
                "separate": vec![true; 4],
                "constructible": vec![true; 6],
                "utilities": vec![true; 6],
            })
        );
    }

    #[test]
    fn document_domain_getter_and_valid_relaxation_match_effective_host() {
        let dom = parse_html("<html><body></body></html>");
        let mut rt = JsRuntime::new();
        rt.set_dom(dom);
        rt.set_url("https://deep.assets.example.co.uk:8443/page");
        rt.run_page_init();

        assert_eq!(
            rt.evaluate(
                r#"(() => {
                    const initial = document.domain;
                    document.domain = "ASSETS.EXAMPLE.CO.UK";
                    const first = document.domain;
                    document.domain = "example.co.uk";
                    return [initial, first, document.domain, location.hostname,
                            (new Document()).domain,
                            new DOMParser().parseFromString("", "text/html").domain];
                })()"#,
            )
            .unwrap(),
            serde_json::json!([
                "deep.assets.example.co.uk",
                "assets.example.co.uk",
                "example.co.uk",
                "deep.assets.example.co.uk",
                "example.co.uk",
                "example.co.uk"
            ])
        );
    }

    #[test]
    fn document_domain_rejects_unrelated_child_and_public_suffix_hosts() {
        let dom = parse_html("<html><body></body></html>");
        let mut rt = JsRuntime::new();
        rt.set_dom(dom);
        rt.set_url("https://app.user.github.io/page");
        rt.run_page_init();

        assert_eq!(
            rt.evaluate(
                r#"(() => {
                    const attempts = ["", ".github.io", "github.io", "evilgithub.io",
                                      "other.github.io", "child.app.user.github.io"];
                    const rejected = attempts.map(value => {
                        try { document.domain = value; return "accepted"; }
                        catch (error) { return error.name; }
                    });
                    document.domain = "user.github.io";
                    return rejected.concat(document.domain);
                })()"#,
            )
            .unwrap(),
            serde_json::json!([
                "SecurityError",
                "SecurityError",
                "SecurityError",
                "SecurityError",
                "SecurityError",
                "SecurityError",
                "user.github.io"
            ])
        );
    }

    #[test]
    fn document_domain_detached_and_hostless_setters_throw_security_error() {
        let mut rt = setup_runtime("<html><body></body></html>");
        assert_eq!(
            rt.evaluate(
                r#"(() => {
                    const detached = [new Document(),
                        document.implementation.createHTMLDocument("x"),
                        document.implementation.createDocument(null, "root")];
                    const errors = detached.map(doc => {
                        try { doc.domain = "example.com"; return "accepted"; }
                        catch (error) { return error.name; }
                    });
                    return [typeof document.domain, document.domain].concat(errors);
                })()"#,
            )
            .unwrap(),
            serde_json::json!([
                "string",
                "example.com",
                "SecurityError",
                "SecurityError",
                "SecurityError"
            ])
        );

        let dom = parse_html("<html><body></body></html>");
        let mut hostless = JsRuntime::new();
        hostless.set_dom(dom);
        hostless.set_url("about:blank");
        hostless.run_page_init();
        assert_eq!(
            hostless
                .evaluate(
                    r#"(() => {
                        let error = "";
                        try { document.domain = "example.com"; }
                        catch (caught) { error = caught.name; }
                        return [document.domain, error];
                    })()"#,
                )
                .unwrap(),
            serde_json::json!(["", "SecurityError"])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scheduler_post_task_observes_priority_fifo_and_task_boundaries() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "scheduler-priority-order",
            r#"
                globalThis.__schedulerOrder = ["sync"];
                const schedule = (name, priority) => scheduler.postTask(() => {
                    __schedulerOrder.push(name);
                    Promise.resolve().then(() => __schedulerOrder.push(name + "-microtask"));
                    return name + "-result";
                }, { priority });
                globalThis.__schedulerResults = Promise.all([
                    schedule("background-1", "background"),
                    schedule("background-2", "background"),
                    schedule("visible", "user-visible"),
                    schedule("blocking-1", "user-blocking"),
                    schedule("blocking-2", "user-blocking"),
                ]).then(values => { globalThis.__schedulerValues = values; });
                Promise.resolve().then(() => __schedulerOrder.push("initial-microtask"));
            "#,
        )
        .unwrap();

        rt.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            rt.evaluate("__schedulerOrder").unwrap(),
            serde_json::json!([
                "sync",
                "initial-microtask",
                "blocking-1",
                "blocking-1-microtask",
                "blocking-2",
                "blocking-2-microtask",
                "visible",
                "visible-microtask",
                "background-1",
                "background-1-microtask",
                "background-2",
                "background-2-microtask",
            ])
        );
        assert_eq!(
            rt.evaluate("__schedulerValues").unwrap(),
            serde_json::json!([
                "background-1-result",
                "background-2-result",
                "visible-result",
                "blocking-1-result",
                "blocking-2-result",
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scheduler_abort_delay_and_yield_follow_task_state() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "scheduler-abort-delay-yield",
            r#"
                globalThis.__schedulerState = {
                    order: [],
                    canceledCallbackRan: false,
                    exactAbortReason: false,
                    selfAbortCallbackRan: false,
                    exactSelfAbortReason: false,
                };
                const abortReason = { reason: "stop" };
                const canceled = new AbortController();
                scheduler.postTask(() => {
                    __schedulerState.canceledCallbackRan = true;
                }, { signal: canceled.signal, delay: 20 }).catch(error => {
                    __schedulerState.exactAbortReason = error === abortReason;
                });
                canceled.abort(abortReason);

                const selfAbortReason = { reason: "inside callback" };
                const selfCanceled = new AbortController();
                scheduler.postTask(() => {
                    __schedulerState.selfAbortCallbackRan = true;
                    selfCanceled.abort(selfAbortReason);
                    return "ignored result";
                }, { signal: selfCanceled.signal }).catch(error => {
                    __schedulerState.exactSelfAbortReason = error === selfAbortReason;
                });

                scheduler.postTask(async () => {
                    __schedulerState.order.push("blocking-start");
                    await scheduler.yield();
                    __schedulerState.order.push("blocking-continuation");
                }, { priority: "user-blocking" });
                scheduler.postTask(() => {
                    __schedulerState.order.push("background");
                }, { priority: "background" });
            "#,
        )
        .unwrap();

        rt.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            rt.evaluate(
                r"[
                    __schedulerState.order,
                    __schedulerState.canceledCallbackRan,
                    __schedulerState.exactAbortReason,
                    __schedulerState.selfAbortCallbackRan,
                    __schedulerState.exactSelfAbortReason,
                    scheduler instanceof Scheduler,
                    Object.prototype.toString.call(scheduler),
                    Scheduler.prototype.postTask.length,
                    Scheduler.prototype.yield.length,
                ]",
            )
            .unwrap(),
            serde_json::json!([
                ["blocking-start", "blocking-continuation", "background"],
                false,
                true,
                true,
                true,
                true,
                "[object Scheduler]",
                1,
                0,
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn self_requeueing_message_channel_yields_to_timers() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "message-channel-task-yield",
            r"
                globalThis.__messageCount = 0;
                globalThis.__timerObserved = false;
                const channel = new MessageChannel();
                channel.port2.onmessage = () => {
                    __messageCount++;
                    if (!__timerObserved) channel.port1.postMessage(null);
                };
                channel.port1.postMessage(null);
                setTimeout(() => { __timerObserved = true; }, 1);
            ",
        )
        .unwrap();

        rt.run_event_loop_bounded(100).await.unwrap();
        let result = rt.evaluate("[__messageCount, __timerObserved]").unwrap();
        let values = result.as_array().unwrap();
        assert!(
            values[0]
                .as_u64()
                .is_some_and(|count| count > 0 && count < 10_000),
            "message task did not yield: {result}"
        );
        assert_eq!(values[1], serde_json::json!(true));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn message_port_queues_until_start_and_clones_at_post_time() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "message-port-start-and-clone",
            r#"
                const channel = new MessageChannel();
                const payload = { nested: { value: 7 } };
                globalThis.__messagePortResult = {
                    portInstance: channel.port1 instanceof MessagePort,
                    channelInstance: channel instanceof MessageChannel,
                    deliveredBeforeStart: false,
                    delivered: false,
                };
                channel.port2.addEventListener("message", function(event) {
                    __messagePortResult.delivered = true;
                    __messagePortResult.value = event.data.nested.value;
                    __messagePortResult.targetIsPort = event.target === channel.port2;
                    __messagePortResult.thisIsPort = this === channel.port2;
                    __messagePortResult.origin = event.origin;
                    __messagePortResult.portCount = event.ports.length;
                });
                channel.port1.postMessage(payload);
                payload.nested.value = 99;
                setTimeout(() => {
                    __messagePortResult.deliveredBeforeStart = __messagePortResult.delivered;
                    channel.port2.start();
                }, 0);
            "#,
        )
        .unwrap();

        rt.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            rt.evaluate("__messagePortResult").unwrap(),
            serde_json::json!({
                "portInstance": true,
                "channelInstance": true,
                "deliveredBeforeStart": false,
                "delivered": true,
                "value": 7,
                "targetIsPort": true,
                "thisIsPort": true,
                "origin": "",
                "portCount": 0,
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn message_port_onmessage_starts_and_yields_between_messages() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "message-port-task-boundaries",
            r#"
                globalThis.__messagePortOrder = [];
                const channel = new MessageChannel();
                channel.port1.postMessage(1);
                channel.port1.postMessage(2);
                channel.port2.onmessage = (event) => {
                    __messagePortOrder.push("message-" + event.data);
                    if (event.currentTarget !== channel.port2) __messagePortOrder.push("bad-current-target");
                    Promise.resolve().then(() => __messagePortOrder.push("microtask-" + event.data));
                };
            "#,
        )
        .unwrap();

        rt.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            rt.evaluate("__messagePortOrder").unwrap(),
            serde_json::json!(["message-1", "microtask-1", "message-2", "microtask-2",])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn message_port_close_discards_delivery_already_queued_for_a_task() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "message-port-close-cancels-queued-delivery",
            r#"
                globalThis.__closedPortDeliveries = 0;
                const channel = new MessageChannel();
                channel.port2.onmessage = () => { __closedPortDeliveries++; };
                channel.port1.postMessage("queued");
                channel.port2.close();
            "#,
        )
        .unwrap();

        rt.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            rt.evaluate("__closedPortDeliveries").unwrap(),
            serde_json::json!(0)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn message_port_handler_and_listener_follow_registration_order() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "message-port-mixed-registration-order",
            r#"
                globalThis.__messagePortRegistrationOrder = [];

                const handlerFirst = new MessageChannel();
                handlerFirst.port2.onmessage = () => __messagePortRegistrationOrder.push("handler-first:handler");
                handlerFirst.port2.addEventListener("message", () => __messagePortRegistrationOrder.push("handler-first:listener"));
                handlerFirst.port1.postMessage(null);

                const listenerFirst = new MessageChannel();
                listenerFirst.port2.addEventListener("message", () => __messagePortRegistrationOrder.push("listener-first:listener"));
                listenerFirst.port2.onmessage = () => __messagePortRegistrationOrder.push("listener-first:handler");
                listenerFirst.port1.postMessage(null);
            "#,
        )
        .unwrap();

        rt.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            rt.evaluate("__messagePortRegistrationOrder").unwrap(),
            serde_json::json!([
                "handler-first:handler",
                "handler-first:listener",
                "listener-first:listener",
                "listener-first:handler",
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn message_port_internal_state_is_hidden_and_ignores_own_property_tampering() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "message-port-hidden-state",
            r#"
                const channel = new MessageChannel();
                globalThis.__messagePortOwnKeys = Object.keys(channel.port2);
                globalThis.__messagePortOwnNames = Object.getOwnPropertyNames(channel.port2);
                globalThis.__messagePortTamperResult = [];
                channel.port2.onmessage = (event) => __messagePortTamperResult.push(event.data);

                // These names used to be the actual implementation state. An
                // expando with any of them must not alter delivery now.
                channel.port1._closed = true;
                channel.port1._entangled = null;
                channel.port2._closed = true;
                channel.port2._messageQueue = [];
                channel.port2._messageQueueEnabled = false;
                channel.port2._messageDeliveryPending = true;
                channel.port2._onmessage = null;
                channel.port2._scheduleMessageDelivery = () => {};
                channel.port2.dispatchEvent = () => { throw new Error("tampered dispatchEvent called"); };
                channel.port1.postMessage("delivered");
            "#,
        )
        .unwrap();

        rt.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            rt.evaluate("[__messagePortOwnKeys, __messagePortOwnNames, __messagePortTamperResult]")
                .unwrap(),
            serde_json::json!([[], [], ["delivered"]])
        );
    }

    #[test]
    fn message_port_has_browser_shaped_construction_and_clone_errors() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"(() => {
                    let constructorError = "";
                    let cloneError = "";
                    try { new MessagePort(); } catch (error) { constructorError = error.name; }
                    try { new MessageChannel().port1.postMessage(() => {}); }
                    catch (error) { cloneError = error.name; }
                    return [constructorError, cloneError, Object.prototype.toString.call(new MessageChannel().port1)];
                })()"#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!(["TypeError", "DataCloneError", "[object MessagePort]"])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broadcast_channel_delivers_independent_post_time_clones_to_matching_peers() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "broadcast-channel-clone-delivery",
            r#"
                globalThis.__broadcastResults = { sender: 0, otherName: 0, peers: [] };
                const sender = new BroadcastChannel("session-sync");
                const first = new BroadcastChannel("session-sync");
                const second = new BroadcastChannel("session-sync");
                const other = new BroadcastChannel("other-name");
                sender.onmessage = () => { __broadcastResults.sender++; };
                other.onmessage = () => { __broadcastResults.otherName++; };
                first.onmessage = (event) => {
                    __broadcastResults.peers.push({
                        peer: "first",
                        value: event.data.nested.value,
                        bytes: Array.from(event.data.bytes),
                        source: event.source,
                        ports: event.ports.length,
                    });
                    event.data.nested.value = 500;
                    event.data.bytes[0] = 99;
                };
                second.onmessage = (event) => {
                    __broadcastResults.peers.push({
                        peer: "second",
                        value: event.data.nested.value,
                        bytes: Array.from(event.data.bytes),
                        source: event.source,
                        ports: event.ports.length,
                    });
                };
                const payload = { nested: { value: 7 }, bytes: new Uint8Array([1, 2, 3]) };
                sender.postMessage(payload);
                payload.nested.value = 42;
                payload.bytes[0] = 88;
            "#,
        )
        .unwrap();

        rt.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            rt.evaluate("__broadcastResults").unwrap(),
            serde_json::json!({
                "sender": 0,
                "otherName": 0,
                "peers": [
                    { "peer": "first", "value": 7, "bytes": [1, 2, 3], "source": null, "ports": 0 },
                    { "peer": "second", "value": 7, "bytes": [1, 2, 3], "source": null, "ports": 0 },
                ],
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broadcast_channel_handlers_follow_registration_order_and_task_timing() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "broadcast-channel-registration-order",
            r#"
                globalThis.__broadcastOrder = ["sync"];
                const sender = new BroadcastChannel("ordering");
                const handlerFirst = new BroadcastChannel("ordering");
                const listenerFirst = new BroadcastChannel("ordering");
                handlerFirst.onmessage = () => __broadcastOrder.push("handler-first:handler");
                handlerFirst.addEventListener("message", () => __broadcastOrder.push("handler-first:listener"));
                listenerFirst.addEventListener("message", () => __broadcastOrder.push("listener-first:listener"));
                listenerFirst.onmessage = () => __broadcastOrder.push("listener-first:handler");
                sender.postMessage(null);
                Promise.resolve().then(() => __broadcastOrder.push("microtask"));
            "#,
        )
        .unwrap();

        rt.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            rt.evaluate("__broadcastOrder").unwrap(),
            serde_json::json!([
                "sync",
                "microtask",
                "handler-first:handler",
                "handler-first:listener",
                "listener-first:listener",
                "listener-first:handler",
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn broadcast_channel_close_cancels_delivery_and_closed_post_throws() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "broadcast-channel-close",
            r#"
                globalThis.__broadcastCloseResult = { deliveries: 0 };
                const sender = new BroadcastChannel("close-test");
                const recipient = new BroadcastChannel("close-test");
                recipient.onmessage = () => { __broadcastCloseResult.deliveries++; };
                sender.postMessage("queued");
                recipient.close();
                sender.close();
                try { sender.postMessage("closed"); }
                catch (error) { __broadcastCloseResult.closedError = error.name; }
                try { new BroadcastChannel(); }
                catch (error) { __broadcastCloseResult.constructorError = error.name; }
                try { new BroadcastChannel("no-peers").postMessage(() => {}); }
                catch (error) { __broadcastCloseResult.cloneError = error.name; }
                __broadcastCloseResult.ownKeys = Object.keys(new BroadcastChannel("shape"));
                __broadcastCloseResult.tag = Object.prototype.toString.call(new BroadcastChannel("shape"));
                __broadcastCloseResult.eventTarget = new BroadcastChannel("shape") instanceof EventTarget;
            "#,
        )
        .unwrap();

        rt.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            rt.evaluate("__broadcastCloseResult").unwrap(),
            serde_json::json!({
                "deliveries": 0,
                "closedError": "InvalidStateError",
                "constructorError": "TypeError",
                "cloneError": "DataCloneError",
                "ownKeys": [],
                "tag": "[object BroadcastChannel]",
                "eventTarget": true,
            })
        );
    }

    #[test]
    fn performance_now_is_monotonic_under_bursty_calls() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // Hammer performance.now() so many calls land in the same millisecond and
        // the wall clock rolls over repeatedly; the value must never go backwards.
        let violations = rt
            .evaluate(
                "(function(){var prev=-Infinity, bad=0; for(var i=0;i<500000;i++){var t=performance.now(); if(t<prev) bad++; prev=t;} return bad;})()",
            )
            .unwrap();
        assert_eq!(
            violations.as_f64(),
            Some(0.0),
            "performance.now() went backwards"
        );
    }

    #[test]
    fn performance_now_does_not_outrun_elapsed_time() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let lead = rt
            .evaluate(
                "(function(){for(var i=0;i<500000;i++)performance.now(); return performance.now()-(Date.now()-performance.timeOrigin);})()",
            )
            .unwrap();
        assert!(
            lead.as_f64().unwrap() <= 1.0,
            "performance.now() advanced ahead of elapsed time: {lead}"
        );
    }

    #[test]
    fn childnode_helpers_coerce_non_string_primitives_to_text() {
        let mut rt =
            setup_runtime(r#"<html><body><div id="p"><span id="t">x</span></div></body></html>"#);
        let before = rt
            .evaluate("(function(){var t=document.getElementById('t'); t.before(5); return t.previousSibling ? t.previousSibling.textContent : 'NULL';})()")
            .unwrap();
        assert_eq!(before, serde_json::json!("5"));
        let after = rt
            .evaluate("(function(){var t=document.getElementById('t'); t.after(true); return t.nextSibling ? t.nextSibling.textContent : 'NULL';})()")
            .unwrap();
        assert_eq!(after, serde_json::json!("true"));
        let replaced = rt
            .evaluate("(function(){var t=document.getElementById('t'); t.replaceWith(42); return document.getElementById('p').textContent;})()")
            .unwrap();
        assert!(
            replaced.as_str().unwrap().contains("42"),
            "replaceWith(42) should leave text '42': {replaced}"
        );
    }

    #[test]
    fn replace_state_without_url_preserves_current_location() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let path = rt
            .evaluate(
                "(function(){history.pushState({}, '', '/dashboard'); history.replaceState({scroll:1}); return location.pathname;})()",
            )
            .unwrap();
        assert_eq!(path, serde_json::json!("/dashboard"));
    }

    #[test]
    fn push_state_without_url_preserves_current_location() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let path = rt
            .evaluate(
                "(function(){history.pushState({}, '', '/a'); history.pushState({b:1}); return location.pathname;})()",
            )
            .unwrap();
        assert_eq!(path, serde_json::json!("/a"));
    }

    #[test]
    fn history_exposes_the_web_platform_constructor_and_prototype() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"(function(){
                    const original = history.replaceState;
                    History.prototype.replaceState.call(history, {ok:true}, "", "/prototype");
                    let illegal = false;
                    try { new History(); } catch (error) { illegal = error instanceof TypeError; }
                    return {
                        instance: history instanceof History,
                        prototype: Object.getPrototypeOf(history) === History.prototype,
                        method: original === History.prototype.replaceState,
                        tag: Object.prototype.toString.call(history),
                        path: location.pathname,
                        illegal,
                    };
                })()"#,
            )
            .unwrap();
        assert_eq!(result["instance"], serde_json::json!(true));
        assert_eq!(result["prototype"], serde_json::json!(true));
        assert_eq!(result["method"], serde_json::json!(true));
        assert_eq!(result["tag"], serde_json::json!("[object History]"));
        assert_eq!(result["path"], serde_json::json!("/prototype"));
        assert_eq!(result["illegal"], serde_json::json!(true));
    }

    #[test]
    fn style_attribute_parses_into_style_object() {
        // Inline styles present in the parsed HTML must be visible via el.style.*
        let mut rt = setup_runtime(
            r#"<html><body><div id="d" style="color: red; display: none">hi</div></body></html>"#,
        );
        assert_eq!(
            rt.evaluate("document.getElementById('d').style.color")
                .unwrap(),
            serde_json::json!("red")
        );
        assert_eq!(
            rt.evaluate("document.getElementById('d').style.display")
                .unwrap(),
            serde_json::json!("none")
        );
    }

    #[test]
    fn set_style_attribute_updates_style_object() {
        let mut rt = setup_runtime(r#"<html><body><div id="d">hi</div></body></html>"#);
        let margin = rt
            .evaluate(
                "(function(){var e=document.getElementById('d'); e.setAttribute('style','margin: 5px'); return e.style.margin;})()",
            )
            .unwrap();
        assert_eq!(margin, serde_json::json!("5px"));
    }

    #[test]
    fn null_namespace_style_attribute_stays_in_sync() {
        let mut rt = setup_runtime(r#"<html><body><div id="d">hi</div></body></html>"#);
        let result = rt
            .evaluate(
                "(function(){var e=document.getElementById('d'); e.setAttributeNS(null,'style','color: green'); var before=e.style.color; e.removeAttributeNS(null,'style'); return before+'|'+e.style.color+'|'+String(e.getAttribute('style'));})()",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("green||null"));
    }

    #[test]
    fn setting_style_property_updates_the_attribute_and_serialization() {
        let mut rt = setup_runtime(r#"<html><body><div id="d">hi</div></body></html>"#);
        let attr = rt
            .evaluate(
                "(function(){var e=document.getElementById('d'); e.style.color='blue'; return e.getAttribute('style');})()",
            )
            .unwrap();
        assert_eq!(attr, serde_json::json!("color: blue;"));
        let html = rt
            .evaluate("document.getElementById('d').outerHTML")
            .unwrap();
        assert!(
            html.as_str().unwrap().contains("color: blue"),
            "outerHTML should carry the style set via el.style: {html}"
        );
    }

    #[test]
    fn style_object_reflects_external_attribute_change() {
        // A later setAttribute('style', …) must supersede an earlier value read
        // through el.style (the declaration re-syncs from the attribute).
        let mut rt =
            setup_runtime(r#"<html><body><div id="d" style="color: red">hi</div></body></html>"#);
        let color = rt
            .evaluate(
                "(function(){var e=document.getElementById('d'); e.style.color; e.setAttribute('style','color: green'); return e.style.color;})()",
            )
            .unwrap();
        assert_eq!(color, serde_json::json!("green"));
    }

    #[test]
    fn clone_node_deep_preserves_context_sensitive_elements() {
        // A <tr> is not a valid child of <div>, so cloning through a throwaway
        // <div>.innerHTML dropped it and returned null. A structural clone keeps it.
        let mut rt = setup_runtime("<html><body></body></html>");
        let tag = rt
            .evaluate("(document.createElement('tr').cloneNode(true) || {}).tagName || 'NULL'")
            .unwrap();
        assert_eq!(tag, serde_json::json!("TR"));
        let td = rt
            .evaluate("(document.createElement('td').cloneNode(true) || {}).tagName || 'NULL'")
            .unwrap();
        assert_eq!(td, serde_json::json!("TD"));
    }

    #[test]
    fn clone_node_deep_copies_children_and_attributes() {
        let mut rt = setup_runtime(
            r#"<html><body><ul id="l"><li class="a">one</li><li class="b">two</li></ul></body></html>"#,
        );
        let out = rt
            .evaluate(
                "(function(){var c=document.getElementById('l').cloneNode(true); return c.children.length + '|' + c.children[0].className + '|' + c.children[1].textContent;})()",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("2|a|two"));
    }

    #[test]
    fn clone_node_deep_preserves_table_rows() {
        let mut rt = setup_runtime(
            r#"<html><body><table id="t"><tbody><tr><td>1</td><td>2</td></tr></tbody></table></body></html>"#,
        );
        // Navigate the detached clone directly (querySelector does not traverse
        // detached subtrees). tbody > tr > (td, td).
        let out = rt
            .evaluate(
                "(function(){var tb=document.querySelector('#t tbody').cloneNode(true); var tr=tb.children[0]; return tr.tagName + '|' + tr.children.length + '|' + tr.children[1].textContent;})()",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("TR|2|2"));
    }

    #[test]
    fn clone_node_shallow_copies_attributes_without_children() {
        let mut rt = setup_runtime(
            r#"<html><body><div id="d" data-x="7"><span>kid</span></div></body></html>"#,
        );
        let out = rt
            .evaluate(
                "(function(){var c=document.getElementById('d').cloneNode(false); return c.getAttribute('data-x') + '|' + c.childNodes.length;})()",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("7|0"));
    }

    #[test]
    fn clone_node_copies_js_assigned_inline_styles() {
        let mut rt = setup_runtime("<html><body><div id='d'></div></body></html>");
        let out = rt
            .evaluate(
                "(function(){var d=document.getElementById('d');d.style.color='red';d.style.fontSize='12px';var c=d.cloneNode(false);return c.style.color+'|'+c.style.fontSize+'|'+c.style.cssText;})()",
            )
            .unwrap();
        assert_eq!(
            out,
            serde_json::json!("red|12px|color: red; font-size: 12px;")
        );
    }

    #[test]
    fn clone_node_deep_copies_template_content() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let out = rt
            .evaluate(
                "(function(){var t=document.createElement('template');t.content.appendChild(document.createElement('option')).textContent='choice';var c=t.cloneNode(true);return c.content.childNodes.length+'|'+c.content.firstChild.tagName+'|'+c.content.firstChild.textContent;})()",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("1|OPTION|choice"));
    }

    #[test]
    fn insert_adjacent_html_parses_table_fragments() {
        let mut rt = setup_runtime(
            r#"<html><body><table id="t"><tbody id="tb"></tbody></table></body></html>"#,
        );
        let out = rt
            .evaluate("(function(){var tb=document.getElementById('tb'); tb.insertAdjacentHTML('beforeend','<tr><td>1</td><td>2</td></tr>'); var tr=tb.firstElementChild; return tr ? (tr.tagName+':'+tr.children.length) : 'NULL';})()")
            .unwrap();
        assert_eq!(out, serde_json::json!("TR:2"));
    }

    #[test]
    fn insert_adjacent_html_position_is_case_insensitive() {
        let mut rt =
            setup_runtime(r#"<html><body><div id="host"><span>base</span></div></body></html>"#);
        let out = rt
            .evaluate("(function(){var h=document.getElementById('host'); h.insertAdjacentHTML('BeforeEnd','<b>x</b>'); return h.lastElementChild ? h.lastElementChild.tagName : 'NULL';})()")
            .unwrap();
        assert_eq!(out, serde_json::json!("B"));
    }

    #[test]
    fn insert_adjacent_html_rejects_invalid_position() {
        let mut rt = setup_runtime(r#"<html><body><div id="host"></div></body></html>"#);
        let out = rt
            .evaluate("(function(){var h=document.getElementById('host'); try { h.insertAdjacentHTML('nope','<b>x</b>'); return 'no-throw'; } catch(e){ return e.name; }})()")
            .unwrap();
        assert_eq!(out, serde_json::json!("SyntaxError"));
    }

    #[test]
    fn insert_adjacent_html_keeps_leading_comments_in_table_contexts() {
        let mut rt = setup_runtime(
            r#"<html><body><table><tbody id="tb"><tr id="row"></tr></tbody></table></body></html>"#,
        );
        let out = rt
            .evaluate(
                "(function(){var tb=document.getElementById('tb');tb.insertAdjacentHTML('beforeend','<!--m--><tr><td>v</td></tr>');var row=document.getElementById('row');row.insertAdjacentHTML('beforeend','<!--n--><td>x</td>');return Array.from(tb.childNodes).map(function(n){return n.nodeName}).join('|')+';'+Array.from(row.childNodes).map(function(n){return n.nodeName}).join('|');})()",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("TR|#comment|TR;#comment|TD"));
    }

    #[test]
    fn insert_adjacent_html_uses_the_insertion_element_as_context() {
        let mut rt = setup_runtime(
            r#"<html><body><div id="d"></div><table id="table"><tbody id="tb"></tbody></table></body></html>"#,
        );
        let out = rt
            .evaluate(
                "(function(){var d=document.getElementById('d');d.insertAdjacentHTML('beforeend','<tr><td>v</td></tr>');var table=document.getElementById('table');table.insertAdjacentHTML('beforeend','<tr><td>x</td></tr>');var tb=document.getElementById('tb');tb.insertAdjacentHTML('beforeend','<tr><td>y</td></tr>tail');return d.firstChild.nodeName+':'+d.textContent+';'+table.lastElementChild.tagName+';'+Array.from(tb.childNodes).map(function(n){return n.nodeName+(n.data?':'+n.data:'')}).join('|');})()",
            )
            .unwrap();
        assert_eq!(out, serde_json::json!("#text:v;TBODY;TR|#text:tail"));
    }

    #[test]
    fn set_attribute_ns_is_retrievable_by_namespace_and_local_name() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate("(function(){var s=document.createElementNS('http://www.w3.org/2000/svg','svg'); s.setAttributeNS('http://www.w3.org/1999/xlink','xlink:href','#g'); return s.getAttributeNS('http://www.w3.org/1999/xlink','href');})()")
            .unwrap();
        assert_eq!(v, serde_json::json!("#g"));
    }

    #[test]
    fn remove_attribute_ns_removes_by_namespace() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate("(function(){var s=document.createElementNS('http://www.w3.org/2000/svg','svg'); s.setAttributeNS('http://www.w3.org/1999/xlink','xlink:href','#g'); s.removeAttributeNS('http://www.w3.org/1999/xlink','href'); return s.getAttributeNS('http://www.w3.org/1999/xlink','href');})()")
            .unwrap();
        assert_eq!(v, serde_json::json!(null));
    }

    #[test]
    fn get_attribute_ns_reads_plain_attributes_with_null_namespace() {
        // Backward-compat: getAttributeNS(null, name) still reads a plain attr.
        let mut rt = setup_runtime(r#"<html><body><div id="d" title="hi"></div></body></html>"#);
        let v = rt
            .evaluate("document.getElementById('d').getAttributeNS(null,'title')")
            .unwrap();
        assert_eq!(v, serde_json::json!("hi"));
    }

    #[test]
    fn namespaced_attribute_keeps_its_qualified_name() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate("(function(){var s=document.createElementNS('http://www.w3.org/2000/svg','svg');s.setAttributeNS('http://www.w3.org/1999/xlink','xlink:href','#g');return s.getAttribute('xlink:href')+'|'+s.getAttributeNames()[0]+'|'+s.outerHTML;})()")
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!("#g|xlink:href|<svg xlink:href=\"#g\"></svg>")
        );
    }

    #[test]
    fn parsed_xlink_attribute_is_available_through_both_apis() {
        let mut rt = setup_runtime(
            r##"<html><body><svg><use id="u" xlink:href="#icon"></use></svg></body></html>"##,
        );
        let v = rt
            .evaluate("(function(){var u=document.getElementById('u');return u.getAttribute('xlink:href')+'|'+u.getAttributeNS('http://www.w3.org/1999/xlink','href')+'|'+u.getAttributeNames().join(',');})()")
            .unwrap();
        assert_eq!(v, serde_json::json!("#icon|#icon|id,xlink:href"));
    }

    #[test]
    fn set_attribute_updates_a_parsed_namespaced_attribute_in_place() {
        // setAttribute matched the stored attribute by local name only, so a
        // parsed `xlink:href` (prefix=xlink, local=href) was never found by the
        // qualified name "xlink:href": the update was pushed as a *second*
        // attribute, getAttribute kept returning the stale original, and the
        // element serialized `xlink:href` twice.
        let mut rt = setup_runtime(
            r##"<html><body><svg><use id="u" xlink:href="#a"></use></svg></body></html>"##,
        );
        let v = rt
            .evaluate("(function(){var u=document.getElementById('u');u.setAttribute('xlink:href','#b');var dup=(u.outerHTML.match(/xlink:href/g)||[]).length;return u.getAttribute('xlink:href')+'|'+u.getAttributeNS('http://www.w3.org/1999/xlink','href')+'|'+u.getAttributeNames().join(',')+'|'+dup;})()")
            .unwrap();
        assert_eq!(v, serde_json::json!("#b|#b|id,xlink:href|1"));
    }

    #[test]
    fn set_attribute_ns_validates_namespace_constraints() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate("(function(){var e=document.createElement('div'),out=[];for(const args of [[null,'x:y'],['urn:test','a:b:c'],['urn:test','xml:lang'],['urn:test','xmlns:x']]){try{e.setAttributeNS(args[0],args[1],'v');out.push('none')}catch(err){out.push(err.name)}}return out.join('|');})()")
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!("NamespaceError|InvalidCharacterError|NamespaceError|NamespaceError")
        );
    }

    #[test]
    fn dom_parser_flags_malformed_xml_with_parsererror() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let has_err = rt
            .evaluate("(function(){var d=new DOMParser().parseFromString('<a><b></a>','application/xml'); return d.querySelector('parsererror') ? true : false;})()")
            .unwrap();
        assert_eq!(has_err, serde_json::json!(true));
    }

    #[test]
    fn dom_parser_accepts_well_formed_xml() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let ok = rt
            .evaluate("(function(){var d=new DOMParser().parseFromString('<root><child>x</child></root>','application/xml'); return d.querySelector('parsererror') ? 'ERR' : 'OK';})()")
            .unwrap();
        assert_eq!(ok, serde_json::json!("OK"));
    }

    #[test]
    fn dom_parser_html_never_gets_parsererror() {
        // HTML parsing is tolerant and must never synthesize a parsererror.
        let mut rt = setup_runtime("<html><body></body></html>");
        let ok = rt
            .evaluate("(function(){var d=new DOMParser().parseFromString('<div><p>hi</a>','text/html'); return d.querySelector('parsererror') ? 'ERR' : 'OK';})()")
            .unwrap();
        assert_eq!(ok, serde_json::json!("OK"));
    }

    #[test]
    fn custom_element_upgrade_runs_class_constructor_on_existing_element() {
        let mut rt = setup_runtime(
            r#"<html><body><svelte-like id="component"></svelte-like></body></html>"#,
        );
        let result = rt
            .evaluate(
                r#"
                const before = document.getElementById("component");
                class SvelteLike extends HTMLElement {
                    constructor() {
                        super();
                        this.$$s = [];
                        this.attachShadow({ mode: "open" });
                    }
                    connectedCallback() {
                        for (const subscription of this.$$s) subscription();
                        this.$$s.push(() => {});
                        this.shadowRoot.textContent = "ready";
                    }
                }
                customElements.define("svelte-like", SvelteLike);
                return [
                    document.getElementById("component") === before,
                    before instanceof SvelteLike,
                    before.constructor === SvelteLike,
                    before.$$s.length,
                    before.shadowRoot && before.shadowRoot.textContent
                ];
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!([true, true, true, 1, "ready"]));
    }

    #[test]
    fn shadow_root_children_expose_parent_siblings_and_composed_root() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                const host = document.createElement("lit-host");
                document.body.appendChild(host);
                const root = host.attachShadow({ mode: "open" });
                const start = document.createComment("start");
                const end = document.createComment("end");
                root.appendChild(start);
                root.appendChild(end);

                const text = document.createTextNode("rendered");
                start.parentNode.insertBefore(text, end);
                const inserted = [
                    start.parentNode === root,
                    start.nextSibling === text,
                    text.previousSibling === start,
                    text.nextSibling === end,
                    end.previousSibling === text,
                    root.contains(text),
                    text.getRootNode() === root,
                    text.getRootNode({ composed: true }) === document,
                    root.getRootNode({ composed: true }) === document,
                    root.isConnected,
                    text.isConnected,
                    root.textContent
                ];

                root.removeChild(text);
                const removed = [
                    text.parentNode === null,
                    start.nextSibling === end,
                    end.previousSibling === start
                ];

                document.body.appendChild(start);
                const moved = [
                    start.parentNode === document.body,
                    start.getRootNode() === document,
                    root.firstChild === end
                ];

                root.innerHTML = "<span id='inside'>inside</span>";
                const inside = root.firstChild;
                const parsed = [
                    inside.parentNode === root,
                    inside.getRootNode() === root,
                    root.textContent,
                    inside.matches("span#inside"),
                    root.querySelector("span#inside") === inside,
                    root.querySelectorAll("span#inside").length === 1
                ];

                const a = document.createElement("a");
                const b = document.createElement("b");
                const c = document.createElement("i");
                root.replaceChildren(a, b, c);
                root.insertBefore(a, c);
                const movedWithin = Array.from(root.children, el => el.localName);

                const fragment = document.createDocumentFragment();
                const x = document.createElement("x-one");
                const y = document.createElement("x-two");
                fragment.append(x, y);
                root.insertBefore(fragment, c);
                const flattened = [
                    Array.from(root.children, el => el.localName),
                    fragment.childNodes.length,
                    x.parentNode === root,
                    y.parentNode === root
                ];

                root.replaceChild(b, c);
                const replaced = [
                    Array.from(root.children, el => el.localName),
                    c.parentNode === null,
                    b.parentNode === root
                ];

                const detached = document.createElement("detached-node");
                const errors = [];
                for (const operation of [
                    () => root.insertBefore(detached, c),
                    () => root.removeChild(c),
                    () => root.replaceChild(detached, c),
                    () => root.appendChild(root),
                    () => root.appendChild(host)
                ]) {
                    try {
                        operation();
                        errors.push("none");
                    } catch (error) {
                        errors.push(error.name);
                    }
                }
                return [inserted, removed, moved, parsed, movedWithin, flattened, replaced, errors];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                [true, true, true, true, true, true, true, true, true, true, true, "rendered"],
                [true, true, true],
                [true, true, true],
                [true, true, "inside", true, true, true],
                ["b", "a", "i"],
                [["b", "a", "x-one", "x-two", "i"], 0, true, true],
                [["a", "x-one", "x-two", "b"], true, true],
                [
                    "NotFoundError",
                    "NotFoundError",
                    "NotFoundError",
                    "HierarchyRequestError",
                    "HierarchyRequestError"
                ]
            ])
        );
    }

    #[test]
    fn shadow_root_identity_and_children_are_native_tree_backed() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r##"
                const host = document.createElement("native-shadow-host");
                document.body.appendChild(host);
                const root = host.attachShadow({ mode: "open", delegatesFocus: true });
                root.innerHTML = "<section id='inside'><span>native</span></section>";
                const inside = root.querySelector("#inside");
                const records = [];
                const observer = new MutationObserver(batch => records.push(...batch));
                observer.observe(root, { childList: true, subtree: true });
                const added = document.createElement("strong");
                inside.appendChild(added);
                records.push(...observer.takeRecords());

                host._shadowRoot = { mode: "closed" };
                let duplicateError = "none";
                try { host.attachShadow({ mode: "open" }); }
                catch (error) { duplicateError = error.name; }

                class ClosedShadowHost extends HTMLElement {
                    constructor() {
                        super();
                        this.closedRoot = this.attachShadow({ mode: "closed" });
                        this.internals = this.attachInternals();
                    }
                }
                customElements.define("closed-shadow-host", ClosedShadowHost);
                const closedHost = document.createElement("closed-shadow-host");

                return [
                    root instanceof ShadowRoot,
                    root.nodeType,
                    root.nodeName,
                    root.host === host,
                    root.mode,
                    root.delegatesFocus,
                    host.shadowRoot === root,
                    duplicateError,
                    inside.parentNode === root,
                    inside.getRootNode() === root,
                    inside.getRootNode({ composed: true }) === document,
                    root.isConnected,
                    inside.isConnected,
                    host.contains(inside),
                    document.querySelector("#inside") === null,
                    root.querySelector("#inside") === inside,
                    records.length,
                    records[0] && records[0].target === inside,
                    records[0] && records[0].addedNodes[0] === added,
                    closedHost.shadowRoot,
                    closedHost.internals.shadowRoot === closedHost.closedRoot
                ];
                "##,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true,
                11,
                "#document-fragment",
                true,
                "open",
                true,
                true,
                "NotSupportedError",
                true,
                true,
                true,
                true,
                true,
                false,
                true,
                true,
                1,
                true,
                true,
                null,
                true
            ])
        );
    }

    #[test]
    fn create_element_synchronously_constructs_an_existing_definition() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                const testStart = true;
                class CreatedLater extends HTMLElement {
                    constructor() {
                        super();
                        this.constructorState = ["initialized"];
                        this.attachShadow({ mode: "open" });
                        this.shadowRoot.textContent = "constructed";
                    }
                    connectedCallback() {
                        this.constructorState.push("connected");
                    }
                }
                customElements.define("created-later", CreatedLater);
                const element = document.createElement("created-later");
                const foreign = document.createElementNS(
                    "http://www.w3.org/2000/svg", "created-later"
                );
                return [
                    element instanceof CreatedLater,
                    element.constructor === CreatedLater,
                    element.localName,
                    element.constructorState,
                    element.shadowRoot && element.shadowRoot.textContent,
                    element.isConnected,
                    foreign instanceof CreatedLater
                ];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true,
                true,
                "created-later",
                ["initialized"],
                "constructed",
                false,
                false
            ])
        );
    }

    #[test]
    fn created_foreign_element_keeps_native_qualified_name_through_clone() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                "(function(){const ns='http://www.w3.org/2000/svg';const el=document.createElementNS(ns,'linearGradient');const clone=el.cloneNode(true);return [el.namespaceURI,el.localName,el.tagName,el.nodeName,clone.namespaceURI,clone.localName,clone.outerHTML].join('|');})()",
            )
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!(
                "http://www.w3.org/2000/svg|linearGradient|linearGradient|linearGradient|http://www.w3.org/2000/svg|linearGradient|<linearGradient></linearGradient>"
            )
        );
    }

    #[test]
    fn svg_path_uses_the_standard_interface_chain() {
        let mut rt = setup_runtime(
            r#"<html><body><svg><path id="shape" d="M0 0L1 1"></path></svg></body></html>"#,
        );
        let result = rt
            .evaluate(
                r#"
                const parsed = document.getElementById("shape");
                SVGPathElement.prototype.polyfillProbe = () => "path";
                const created = document.createElementNS(
                    "http://www.w3.org/2000/svg", "path"
                );
                const div = document.createElement("div");
                return [
                    parsed.constructor.name,
                    parsed instanceof SVGPathElement,
                    parsed instanceof SVGGeometryElement,
                    parsed instanceof SVGGraphicsElement,
                    parsed instanceof SVGElement,
                    parsed instanceof Element,
                    created instanceof SVGPathElement,
                    Object.getPrototypeOf(SVGPathElement.prototype) === SVGGeometryElement.prototype,
                    Object.getPrototypeOf(SVGGeometryElement.prototype) === SVGGraphicsElement.prototype,
                    Object.getPrototypeOf(SVGGraphicsElement.prototype) === SVGElement.prototype,
                    Object.getPrototypeOf(SVGElement.prototype) === Element.prototype,
                    parsed.polyfillProbe(),
                    typeof div.polyfillProbe
                ];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                "SVGPathElement",
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                "path",
                "undefined"
            ])
        );
    }

    #[test]
    fn foreign_inner_html_and_contextual_fragments_keep_svg_namespace() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                "(function(){const ns='http://www.w3.org/2000/svg';const svg=document.createElementNS(ns,'svg');svg.innerHTML='<linearGradient id=paint></linearGradient>';const range=document.createRange();range.selectNodeContents(svg);const fragment=range.createContextualFragment('<circle></circle>');const circle=fragment.firstElementChild;return [svg.firstElementChild.namespaceURI,svg.firstElementChild.localName,circle.namespaceURI,circle.localName].join('|');})()",
            )
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!(
                "http://www.w3.org/2000/svg|linearGradient|http://www.w3.org/2000/svg|circle"
            )
        );
    }

    #[test]
    fn throwing_custom_element_constructor_marks_upgrade_failed_without_connecting() {
        let mut rt = setup_runtime(
            r#"<html><body><throws-during-upgrade id="target"></throws-during-upgrade></body></html>"#,
        );
        let result = rt
            .evaluate(
                r#"
                let constructorCalls = 0;
                let connectedCalls = 0;
                class ThrowsDuringUpgrade extends HTMLElement {
                    constructor() {
                        super();
                        constructorCalls++;
                        throw new Error("expected constructor failure");
                    }
                    connectedCallback() {
                        connectedCalls++;
                    }
                }
                customElements.define("throws-during-upgrade", ThrowsDuringUpgrade);
                const element = document.getElementById("target");
                customElements.upgrade(document);
                return [
                    constructorCalls,
                    connectedCalls,
                    element.__customUpgradeFailed === true
                ];
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!([1, 0, true]));
    }

    #[test]
    fn document_title_setter_creates_missing_title_element() {
        let mut rt = setup_runtime("<html><body><main>content</main></body></html>");
        let result = rt
            .evaluate(
                r#"
                (function() {
                  document.title = "Created";
                  return [
                    document.title,
                    document.head.tagName,
                    document.head.firstElementChild.tagName,
                    document.head.firstElementChild.textContent,
                    document.documentElement.firstElementChild === document.head
                  ];
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!(["Created", "HEAD", "TITLE", "Created", true])
        );

        let detached = rt
            .evaluate(
                r#"
                (function() {
                  const doc = document.implementation.createHTMLDocument();
                  doc.title = "  Detached   title  ";
                  return [doc.title, doc.querySelector("title").textContent, doc.referrer];
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            detached,
            serde_json::json!(["Detached title", "  Detached   title  ", ""])
        );
    }

    #[test]
    fn document_referrer_has_explicit_navigation_state() {
        let mut rt = setup_runtime("<html><body></body></html>");
        assert_eq!(
            rt.evaluate("document.referrer").unwrap(),
            serde_json::json!("")
        );

        rt.set_referrer("https://source.example/path?q=1");
        assert_eq!(
            rt.evaluate("document.referrer").unwrap(),
            serde_json::json!("https://source.example/path?q=1")
        );
    }

    #[test]
    fn global_window_has_browser_constructor_identity() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                "return [window === self, self.constructor === Window,\
                         window instanceof Window, self.document === document,\
                         self.location === location, self.history === history,\
                         self.navigator === navigator];",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, true, true, true, true, true, true])
        );
    }

    #[test]
    fn window_named_access_exposes_ids_and_eligible_names() {
        let mut rt = setup_runtime(
            r#"<html><body>
                <script id="payload" type="application/json">{"ready":true}</script>
                <div id="duplicate"></div><span id="duplicate"></span>
                <form name="login"></form><img name="hero">
                <div name="not-exposed"></div>
            </body></html>"#,
        );
        let result = rt
            .evaluate(
                r#"
                return [
                    window.payload === document.getElementById("payload"),
                    window.payload.text,
                    window.duplicate instanceof HTMLCollection,
                    window.duplicate.length,
                    window.login === document.querySelector("form"),
                    window.hero === document.querySelector("img"),
                    typeof window["not-exposed"]
                ];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, "{\"ready\":true}", true, 2, true, true, "undefined"])
        );
    }

    #[test]
    fn window_named_access_tracks_dynamic_ids_and_fragment_parsing() {
        let mut rt = setup_runtime("<html><body><div id='host'></div></body></html>");
        let result = rt
            .evaluate(
                r#"
                const made = document.createElement("section");
                made.id = "dynamicName";
                const detachedIdAbsent = !("dynamicName" in window);
                document.body.appendChild(made);
                const first = window.dynamicName === made;
                made.id = "renamedDynamic";
                const renamed = !("dynamicName" in window)
                    && window.renamedDynamic === made;
                document.body.removeChild(made);
                const removed = !("renamedDynamic" in window);
                document.body.appendChild(made);
                const reattached = window.renamedDynamic === made;
                document.getElementById("host").innerHTML =
                    "<script id='parsedName'>payload</script>";
                const parsed = window.parsedName === document.getElementById("parsedName")
                    && window.parsedName.text === "payload";
                document.getElementById("host").innerHTML = "";
                const subtree = document.createElement("div");
                subtree.innerHTML = "<svg><path id='nestedSvg' name='svgName'></path></svg>";
                const detachedNestedAbsent = !("nestedSvg" in window);
                document.body.appendChild(subtree);
                const nested = window.nestedSvg === subtree.querySelector("path")
                    && typeof window.svgName === "undefined";
                document.body.removeChild(subtree);
                const nestedRemoved = !("nestedSvg" in window);
                document.body.appendChild(subtree);
                const shadowHost = document.createElement("div");
                const shadowRoot = shadowHost.attachShadow({ mode: "open" });
                const shadowChild = document.createElement("span");
                shadowChild.id = "shadowOnly";
                shadowRoot.appendChild(shadowChild);
                document.body.appendChild(shadowHost);
                const originalFetch = window.fetch;
                const collision = document.createElement("div");
                collision.id = "fetch";
                document.body.appendChild(collision);
                document.body.removeChild(collision);
                return [
                    detachedIdAbsent,
                    first,
                    renamed,
                    removed,
                    reattached,
                    parsed,
                    !("parsedName" in window),
                    detachedNestedAbsent,
                    nested,
                    nestedRemoved,
                    window.nestedSvg === subtree.querySelector("path"),
                    !("shadowOnly" in window),
                    window.fetch === originalFetch
                ];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true, true, true, true, true, true, true, true, true, true, true, true, true
            ])
        );
    }

    #[test]
    fn explicit_viewport_is_distinct_from_fingerprinted_screen() {
        let dom = parse_html("<html><body></body></html>");
        let mut rt = JsRuntime::new();
        rt.set_dom(dom);
        rt.set_viewport(1024.0, 768.0);
        rt.run_page_init();
        let result = rt
            .evaluate(
                "return [innerWidth, innerHeight, visualViewport.width,\
                         visualViewport.height, screen.width > 0, screen.height > 0];",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([1024, 768, 1024, 768, true, true])
        );
    }

    #[test]
    fn screen_override_is_independent_live_and_preserves_screen_identity() {
        let dom = parse_html("<html><body></body></html>");
        let mut rt = JsRuntime::new();
        rt.set_dom(dom);
        rt.set_viewport(1024.0, 768.0);
        rt.run_page_init();
        rt.execute_script(
            "remember-screen",
            "globalThis.__screenBefore = screen;\
             globalThis.__screenSizeBefore = [screen.width, screen.height];",
        )
        .unwrap();

        rt.set_screen_size_override(Some((1440.0, 900.0)), true);
        assert_eq!(
            rt.evaluate(
                "[innerWidth, innerHeight, screen.width, screen.height,\
                  screen.availWidth, screen.availHeight, screen === __screenBefore]"
            )
            .unwrap(),
            serde_json::json!([1024, 768, 1440, 900, 1440, 900, true])
        );

        rt.set_screen_size_override(None, false);
        assert_eq!(
            rt.evaluate(
                "[innerWidth, innerHeight, screen.width === __screenSizeBefore[0],\
                  screen.height === __screenSizeBefore[1],\
                  screen.availHeight === screen.height - 40,\
                  screen === __screenBefore]"
            )
            .unwrap(),
            serde_json::json!([1024, 768, true, true, true, true])
        );
    }

    #[test]
    fn match_media_evaluates_query_lists_conjunctions_ranges_and_orientation() {
        let dom = parse_html("<html><body></body></html>");
        let mut rt = JsRuntime::new();
        rt.set_dom(dom);
        rt.set_viewport(1280.0, 720.0);
        rt.run_page_init();

        let result = rt
            .evaluate(
                r#"
                return [
                    matchMedia("(min-width: 1024px) and (min-height: 700px)").matches,
                    matchMedia("(min-width: 1024px) and (min-height: 900px)").matches,
                    matchMedia("(max-width: 600px), screen and (orientation: landscape)").matches,
                    matchMedia("not print").matches,
                    matchMedia("not screen").matches,
                    matchMedia("only screen and (width: 1280px) and (height = 720px)").matches,
                    matchMedia("(1000px <= width < 1400px) and (height > 700px)").matches,
                    matchMedia("(orientation: portrait)").matches,
                    matchMedia("(prefers-color-scheme: light) and (pointer: fine) and (hover: hover)").matches,
                    matchMedia("(tinybrowser-unknown-feature: yes)").matches
                ];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, false, true, true, false, true, true, false, true, false])
        );
    }

    #[test]
    fn match_media_matches_are_live_across_viewport_resizes() {
        let dom = parse_html("<html><body></body></html>");
        let mut rt = JsRuntime::new();
        rt.set_dom(dom);
        rt.set_viewport(900.0, 600.0);
        rt.run_page_init();
        assert_eq!(
            rt.evaluate(
                r#"
                return [
                    (globalThis.__wideAndShort = matchMedia(
                        "(min-width: 800px) and (max-height: 700px)"
                    )).matches,
                    (globalThis.__portrait = matchMedia(
                        "(orientation: portrait)"
                    )).matches
                ];
                "#,
            )
            .unwrap(),
            serde_json::json!([true, false])
        );

        rt.set_viewport(600.0, 900.0);
        assert_eq!(
            rt.evaluate(
                "return [__wideAndShort.matches, __portrait.matches,\
                         matchMedia('(max-width: 600px), print').matches];",
            )
            .unwrap(),
            serde_json::json!([false, true, true])
        );
    }

    #[test]
    fn computed_style_access_does_not_get_shadowed_by_inline_style_proxy() {
        let mut rt = setup_runtime(
            r#"<html><body><div id="box" style="opacity:.5;width:40px"></div></body></html>"#,
        );
        let result = rt
            .evaluate(
                r#"
                const box = document.getElementById("box");
                const computed = getComputedStyle(box);
                return [
                    computed.display,
                    computed.visibility,
                    computed.opacity,
                    computed.width,
                    computed.getPropertyValue("display"),
                    computed.getPropertyValue("background-color")
                ];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                "block",
                "visible",
                "0.5",
                "40px",
                "block",
                "rgba(0, 0, 0, 0)"
            ])
        );
    }

    #[test]
    fn hyperlink_content_attributes_reflect_through_the_idl_surface() {
        let mut rt = setup_runtime(
            r#"<html><body>
                <a id="locale" hreflang="en-US" rel="alternate"
                   target="_blank" download="guide.pdf"
                   ping="/audit" referrerpolicy="no-referrer">English</a>
            </body></html>"#,
        );
        let result = rt
            .evaluate(
                r#"
                const link = document.getElementById("locale");
                const initial = [
                    link.hreflang, link.rel, link.target, link.download,
                    link.ping, link.referrerPolicy,
                    link.hreflang.split("-")[1]
                ];
                link.hreflang = "de-DE";
                link.referrerPolicy = "origin";
                return [
                    initial,
                    link.getAttribute("hreflang"),
                    link.getAttribute("referrerpolicy")
                ];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                [
                    "en-US",
                    "alternate",
                    "_blank",
                    "guide.pdf",
                    "/audit",
                    "no-referrer",
                    "US"
                ],
                "de-DE",
                "origin"
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn idle_event_loop_flushes_resolved_promise_continuations() {
        let mut rt = setup_runtime("<html><body><div id='state'>pending</div></body></html>");
        rt.execute_script(
            "font-ready",
            "document.fonts.load('normal 1px Example').then(() => {\
                 document.getElementById('state').textContent = 'ready';\
             });",
        )
        .unwrap();
        rt.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            rt.evaluate("document.getElementById('state').textContent")
                .unwrap(),
            serde_json::json!("ready")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quiescent_event_loop_does_not_wait_for_analytics_interval() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "quiescent-long-interval",
            "setInterval(() => { globalThis.__analyticsTicks = (globalThis.__analyticsTicks || 0) + 1; }, 1000);",
        )
        .unwrap();

        let started = std::time::Instant::now();
        rt.run_event_loop_until_quiescent(1_000, 50).await.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_millis(400),
            "a future analytics interval must not consume the full settle budget"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fixed_duration_event_loop_yields_from_continuously_ready_tasks() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "fixed-duration-continuously-ready",
            "globalThis.__fixedTicks = 0;\
             setInterval(() => { __fixedTicks++; }, 0);",
        )
        .unwrap();

        let started = std::time::Instant::now();
        rt.run_event_loop_bounded(40).await.unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(300),
            "a continuously-ready queue must return between tasks instead of waiting for the watchdog: {elapsed:?}",
        );
        assert!(
            rt.evaluate("globalThis.__fixedTicks > 0")
                .unwrap()
                .as_bool()
                .unwrap_or(false),
            "the cooperative fixed wait must still execute queued tasks",
        );
        assert_eq!(
            rt.evaluate(
                "(document.body.setAttribute('data-after-fixed-wait', 'usable'), \
                 document.body.getAttribute('data-after-fixed-wait'))",
            )
            .unwrap(),
            serde_json::json!("usable"),
            "the isolate must remain usable after the fixed wait",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn short_observation_deadline_does_not_terminate_the_active_task() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "task-crossing-observation-deadline",
            "globalThis.__longTaskCompleted = false;\
             setTimeout(() => {\
               const end = performance.now() + 600;\
               while (performance.now() < end) {}\
               __longTaskCompleted = true;\
             }, 0);",
        )
        .unwrap();

        let started = std::time::Instant::now();
        rt.run_event_loop_bounded(20).await.unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed >= std::time::Duration::from_millis(500)
                && elapsed < std::time::Duration::from_millis(1_500),
            "capture must wait for the active task boundary without becoming unbounded: {elapsed:?}",
        );
        assert_eq!(
            rt.evaluate("globalThis.__longTaskCompleted").unwrap(),
            serde_json::json!(true),
            "a screenshot/readiness deadline must not terminate valid page work",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn adaptive_observation_deadline_does_not_terminate_the_active_task() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "adaptive-task-crossing-observation-deadline",
            "globalThis.__adaptiveLongTaskCompleted = false;\
             setTimeout(() => {\
               const end = performance.now() + 600;\
               while (performance.now() < end) {}\
               __adaptiveLongTaskCompleted = true;\
             }, 0);",
        )
        .unwrap();

        let started = std::time::Instant::now();
        rt.run_event_loop_until_quiescent(20, 10).await.unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed >= std::time::Duration::from_millis(500)
                && elapsed < std::time::Duration::from_millis(1_500),
            "adaptive settle must wait for the active task boundary: {elapsed:?}",
        );
        assert_eq!(
            rt.evaluate("globalThis.__adaptiveLongTaskCompleted")
                .unwrap(),
            serde_json::json!(true),
            "adaptive readiness must not terminate valid page work",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quiescent_event_loop_yields_from_continuously_ready_non_visual_work() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "quiescent-continuously-ready",
            "setInterval(() => {\
                 globalThis.__schedulerTicks = (globalThis.__schedulerTicks || 0) + 1;\
             }, 0);",
        )
        .unwrap();

        let started = std::time::Instant::now();
        rt.run_event_loop_until_quiescent(2_000, 150).await.unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "a continuously-ready non-visual scheduler pinned adaptive settle: {elapsed:?}"
        );
        assert!(
            rt.evaluate("globalThis.__schedulerTicks > 0")
                .unwrap()
                .as_bool()
                .unwrap_or(false),
            "the cooperative policy must still drive scheduler work"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quiescent_event_loop_bounds_a_single_unyielding_callback_drain() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "quiescent-unyielding-task",
            "setTimeout(() => { while (true) {} }, 0);",
        )
        .unwrap();

        let started = std::time::Instant::now();
        rt.run_event_loop_until_quiescent(2_000, 150).await.unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed >= std::time::Duration::from_millis(SYNCHRONOUS_TASK_FLOOR_MS)
                && elapsed < std::time::Duration::from_millis(SYNCHRONOUS_TASK_FLOOR_MS + 1_500,),
            "one synchronous callback drain escaped the bounded task allowance: {elapsed:?}"
        );
        assert_eq!(
            rt.evaluate(
                "(document.body.setAttribute('data-after-watchdog', 'usable'), \
                  document.body.getAttribute('data-after-watchdog'))",
            )
            .unwrap(),
            serde_json::json!("usable"),
            "the per-turn watchdog must leave the isolate reusable",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quiescent_event_loop_retains_delayed_network_and_dom_update() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let client = std::sync::Arc::new(tinybrowser_net::HttpClient::with_full_options(
            std::sync::Arc::new(tinybrowser_net::CookieJar::new()),
            None,
            true,
        ));
        let in_flight = rt.qjs.shared_state().borrow().page_in_flight.clone();
        in_flight.store(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(80));
            in_flight.store(0, std::sync::atomic::Ordering::SeqCst);
        });
        rt.set_http_client(client);
        rt.execute_script(
            "quiescent-delayed-work",
            "setInterval(() => {}, 1000);\
             setTimeout(() => document.body.setAttribute('data-ready', 'ready'), 40);",
        )
        .unwrap();

        rt.run_event_loop_until_quiescent(1_000, 150).await.unwrap();
        assert_eq!(
            rt.evaluate("document.body.getAttribute('data-ready')")
                .unwrap(),
            serde_json::json!("ready"),
        );
    }

    fn delayed_fetch_runtime(
        response_delay: std::time::Duration,
    ) -> (JsRuntime, std::sync::mpsc::Receiver<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            accepted_tx.send(()).unwrap();
            std::thread::sleep(response_delay);
            let body = "hydrated";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes());
        });

        let origin = format!("http://{address}");
        let mut rt = JsRuntime::new();
        rt.set_dom(parse_html("<html><body></body></html>"));
        rt.set_url(&format!("{origin}/page"));
        rt.set_http_client(std::sync::Arc::new(
            tinybrowser_net::HttpClient::with_full_options(
                std::sync::Arc::new(tinybrowser_net::CookieJar::new()),
                None,
                true,
            ),
        ));
        rt.run_page_init();
        (rt, accepted_rx)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quiescent_event_loop_allows_fetch_hydration_within_network_grace() {
        let (mut rt, accepted) = delayed_fetch_runtime(std::time::Duration::from_millis(700));
        rt.execute_script(
            "quiescent-fetch-hydration",
            "fetch('/hydrate').then(response => response.text()).then(text => {\
                 document.body.setAttribute('data-ready', text);\
             });",
        )
        .unwrap();

        let started = std::time::Instant::now();
        rt.run_event_loop_until_quiescent(3_000, 150).await.unwrap();
        let elapsed = started.elapsed();

        accepted
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("fixture fetch was not issued");
        assert!(
            elapsed >= std::time::Duration::from_millis(650),
            "settle returned before the delayed response: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(1_500),
            "completed hydration should only pay its following quiet window: {elapsed:?}"
        );
        assert_eq!(
            rt.evaluate("document.body.getAttribute('data-ready')")
                .unwrap(),
            serde_json::json!("hydrated"),
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quiescent_event_loop_bounds_a_hanging_page_request() {
        let (mut rt, accepted) = delayed_fetch_runtime(std::time::Duration::from_secs(3));
        rt.execute_script(
            "quiescent-hanging-fetch",
            "fetch('/analytics').catch(() => {});",
        )
        .unwrap();

        let started = std::time::Instant::now();
        rt.run_event_loop_until_quiescent(4_000, 150).await.unwrap();
        let elapsed = started.elapsed();

        accepted
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("fixture fetch was not issued");
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "pending page work must receive the network grace: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(1_700),
            "a hanging request consumed more than its bounded grace: {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quiescent_event_loop_gives_post_grace_dom_activity_a_quiet_window() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.qjs
            .shared_state()
            .borrow()
            .page_in_flight
            .store(1, std::sync::atomic::Ordering::SeqCst);
        rt.execute_script(
            "quiescent-post-grace-commit",
            "setInterval(() => {}, 1000);\
             setTimeout(() => document.body.setAttribute('data-ready', 'late'), 1100);",
        )
        .unwrap();

        let started = std::time::Instant::now();
        rt.run_event_loop_until_quiescent(4_000, 150).await.unwrap();
        let elapsed = started.elapsed();

        assert_eq!(
            rt.evaluate("document.body.getAttribute('data-ready')")
                .unwrap(),
            serde_json::json!("late"),
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(1_200),
            "the late commit did not receive a following quiet window: {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(1_800),
            "late observable work escaped the bounded activity tail: {elapsed:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quiescence_ignores_another_pages_shared_client_request() {
        let client = std::sync::Arc::new(tinybrowser_net::HttpClient::with_full_options(
            std::sync::Arc::new(tinybrowser_net::CookieJar::new()),
            None,
            true,
        ));
        client
            .in_flight
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.set_http_client(client);
        rt.execute_script("quiescent-shared-client", "setInterval(() => {}, 1000);")
            .unwrap();

        let started = std::time::Instant::now();
        rt.run_event_loop_until_quiescent(1_000, 50).await.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_millis(400),
            "an unrelated page request on the shared client must not pin settle"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quiescent_event_loop_retains_near_term_render_timeout() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "quiescent-render-timeout",
            "setInterval(() => {}, 1000);\
             setTimeout(() => document.body.setAttribute('data-ready', 'ready'), 200);",
        )
        .unwrap();

        let started = std::time::Instant::now();
        rt.run_event_loop_until_quiescent(1_000, 150).await.unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(180));
        assert_eq!(
            rt.evaluate("document.body.getAttribute('data-ready')")
                .unwrap(),
            serde_json::json!("ready"),
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quiescent_event_loop_bounds_continuous_visual_mutations() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "quiescent-animated-page",
            "let tick=0;setInterval(() =>\
               document.body.setAttribute('data-frame', String(++tick)), 10);",
        )
        .unwrap();

        let started = std::time::Instant::now();
        rt.run_event_loop_until_quiescent(2_000, 150).await.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "an animated document must not consume the complete settle budget"
        );
        assert!(
            rt.evaluate("Number(document.body.getAttribute('data-frame')) > 0")
                .unwrap()
                .as_bool()
                .unwrap_or(false),
            "the policy must still pump animation work before capture"
        );
    }

    #[test]
    fn font_face_set_tracks_authored_and_script_created_faces() {
        let mut rt = setup_runtime(
            r#"<html><head><style>
                @font-face {
                    font-family: "Authored One";
                    src: url("https://assets.test/one.woff2") format("woff2");
                    font-weight: 350 650;
                }
                @font-face {
                    font-family: AuthoredTwo;
                    src: url(data:font/woff2;base64,d09GMg==);
                    font-style: italic;
                }
            </style></head><body></body></html>"#,
        );
        let result = rt
            .evaluate(
                r#"(() => {
                    const authored = Array.from(document.fonts);
                    const cssDelete = document.fonts.delete(authored[0]);
                    const dynamic = new FontFace("Dynamic", "url('/dynamic.ttf')", {
                        style: "oblique 12deg",
                        weight: "700",
                        stretch: "condensed",
                        unicodeRange: "U+20-7E",
                        display: "swap"
                    });
                    const addResult = document.fonts.add(dynamic);
                    const visited = [];
                    document.fonts.forEach((value, key, set) => {
                        visited.push(value === key && set === document.fonts);
                    });
                    const afterAdd = [
                        document.fonts.size,
                        document.fonts.has(dynamic),
                        addResult === document.fonts,
                        dynamic.family,
                        dynamic.style,
                        dynamic.weight,
                        dynamic.stretch,
                        dynamic.unicodeRange,
                        dynamic.display,
                        visited.every(Boolean)
                    ];
                    const deleted = document.fonts.delete(dynamic);
                    document.fonts.clear();
                    const bytes = new Uint8Array([0, 1, 2, 253, 254, 255]);
                    const binary = new FontFace("Binary", bytes, { weight: 600 });
                    return [
                        authored.length,
                        authored.map(face => face.family),
                        cssDelete,
                        afterAdd,
                        deleted,
                        document.fonts.size,
                        binary.status,
                        binary.loaded === binary.load()
                    ];
                })()"#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                2,
                ["Authored One", "AuthoredTwo"],
                false,
                [
                    3,
                    true,
                    true,
                    "Dynamic",
                    "oblique 12deg",
                    "700",
                    "condensed",
                    "U+20-7E",
                    "swap",
                    true
                ],
                true,
                2,
                "loaded",
                true
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn font_face_load_updates_status_set_readiness_and_matching() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "font-face-lifecycle",
            r#"
                globalThis.__fontEvents = [];
                const face = new FontFace("Lifecycle", "url('/lifecycle.woff2')", {
                    weight: "700"
                });
                document.fonts.onloading = event => __fontEvents.push([event.type, event.fontfaces.length]);
                document.fonts.onloadingdone = event => __fontEvents.push([event.type, event.fontfaces.length]);
                document.fonts.add(face);
                globalThis.__fontBefore = [
                    face.status,
                    document.fonts.status,
                    document.fonts.check("700 16px Lifecycle")
                ];
                globalThis.__fontLoadResult = "pending";
                document.fonts.load("700 16px Lifecycle").then(faces => {
                    __fontLoadResult = [faces.length, faces[0] === face, face.status,
                        document.fonts.check("700 16px Lifecycle")];
                });
                document.fonts.ready.then(set => {
                    globalThis.__fontReady = set === document.fonts;
                });
            "#,
        )
        .unwrap();
        rt.run_event_loop_bounded(100).await.unwrap();
        let result = rt
            .evaluate("return [__fontBefore, __fontLoadResult, __fontReady, __fontEvents];")
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                ["unloaded", "loaded", false],
                [1, true, "loaded", true],
                true,
                [["loading", 1], ["loadingdone", 1]]
            ])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rendering_opportunity_orders_raf_resize_and_intersection_phases() {
        let mut rt = setup_runtime(
            "<html><body><div id='target' style='width:20px;height:20px'></div></body></html>",
        );
        rt.execute_script(
            "rendering-opportunity-order",
            r#"
                globalThis.__renderPhaseOrder = [];
                const target = document.getElementById("target");
                new ResizeObserver(() => __renderPhaseOrder.push("resize")).observe(target);
                new IntersectionObserver(() => __renderPhaseOrder.push("intersection")).observe(target);
                requestAnimationFrame(() => {
                    __renderPhaseOrder.push("raf");
                    target.style.width = "40px";
                });
            "#,
        )
        .unwrap();

        rt.run_event_loop_bounded(100).await.unwrap();
        assert_eq!(
            rt.evaluate("__renderPhaseOrder.slice(0, 3)").unwrap(),
            serde_json::json!(["raf", "resize", "intersection"]),
        );
    }

    #[test]
    fn css_supports_matches_capabilities_and_boolean_conditions() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"JSON.stringify([
                    CSS.supports("-webkit-hyphens", "none"),
                    CSS.supports("margin-trim", "inline"),
                    CSS.supports("-moz-orient", "inline"),
                    CSS.supports("color", "rgb(from red r g b)"),
                    CSS.supports("(((-webkit-hyphens:none)) and (not (margin-trim:inline))) or ((-moz-orient:inline) and (not (color:rgb(from red r g b))))"),
                    CSS.supports("display", "grid"),
                    CSS.supports("(display:grid) and (selector(.card > *))"),
                    CSS.supports("not (unknown-engine-prop:value)"),
                    CSS.supports("selector(.card >)"),
                    CSS.supports("selector(:tinybrowser-unknown)"),
                    CSS.supports("selector(.card,)"),
                    CSS.supports("scrollbar-gutter", "stable"),
                    CSS.supports("scrollbar-gutter", "floating"),
                    CSS.supports("color", "light-dark(rgb(1, 2, 3), color-mix(in srgb, white 50%, black))"),
                    CSS.supports("(color:light-dark(red, light-dark(white, black)))"),
                    CSS.supports("color", "light-dark(red)"),
                    CSS.supports("color", "light-dark(red, rgb(1, 2, 3)"),
                    CSS.supports("border", "2px dashed red"),
                    CSS.supports("border-width", "10%"),
                    CSS.supports("word-break", "break-all"),
                    CSS.supports("filter", "blur(2px)"),
                    CSS.supports("content", "attr(data-label)"),
                    CSS.supports("display", "grid;"),
                    CSS.supports("flex-flow", "column"),
                    CSS.supports("flex-flow", "wrap column"),
                    CSS.supports("flex-flow", "column wrap"),
                    CSS.supports("flex-flow", "row column"),
                    CSS.supports("flex-flow", "nowrap wrap-reverse"),
                    CSS.supports("(flex-flow:column)")
                ])"#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!("[false,false,false,false,false,true,true,true,false,false,false,true,false,true,true,false,false,true,false,true,false,true,false,true,true,true,false,false,true]")
        );
    }

    #[test]
    fn attributes_named_node_map_is_live() {
        let mut rt = setup_runtime(r#"<div id="test" class="card" data-state="ready"></div>"#);
        let result = rt
            .evaluate(
                r#"
                const element = document.getElementById("test");
                const attributes = element.attributes;
                const sameObject = attributes === element.attributes;
                const firstName = attributes[0].name;
                let removed = 0;
                while (attributes.length) {
                    element.removeAttributeNode(attributes[0]);
                    removed++;
                    if (removed > 10) throw new Error("NamedNodeMap is not live");
                }
                return {
                    sameObject,
                    namedNodeMap: attributes instanceof NamedNodeMap,
                    firstName,
                    removed,
                    length: attributes.length,
                    hasAttributes: element.hasAttributes(),
                };
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!({
                "sameObject": true,
                "namedNodeMap": true,
                "firstName": "id",
                "removed": 3,
                "length": 0,
                "hasAttributes": false,
            })
        );
    }

    #[test]
    fn script_created_attribute_reads_stay_coherent_across_mutation_apis() {
        let mut rt = setup_runtime(r"<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                (() => {
                    const element = document.createElement("DIV");
                    const initial = element.getAttribute("data-state");
                    element.setAttribute("DATA-STATE", "ready");
                    const ordinary = [
                        element.getAttribute("data-state"),
                        element.getAttribute("DATA-STATE"),
                    ];
                    element.setAttributeNS(null, "data-state", "namespaced");
                    const namespaced = element.getAttribute("data-state");
                    element.removeAttributeNS(null, "data-state");
                    const removed = element.getAttribute("data-state");
                    return { initial, ordinary, namespaced, removed };
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!({
                "initial": null,
                "ordinary": ["ready", "ready"],
                "namespaced": "namespaced",
                "removed": null,
            })
        );
    }

    #[test]
    fn structural_cache_tracks_detach_reparent_and_rejected_mutations() {
        let mut rt = setup_runtime(r"<html><body></body></html>");
        let result = rt
            .evaluate(
                r#"
                (() => {
                    const host = document.createElement("div");
                    const child = document.createElement("span");
                    const text = document.createTextNode("hello");
                    const fresh = [host.parentNode, host.isConnected, text.parentNode, text.isConnected];

                    host.appendChild(child);
                    const detachedTree = [child.parentNode === host, host.isConnected, child.isConnected];
                    document.body.appendChild(host);
                    const connectedTree = [host.isConnected, child.isConnected, child.parentNode === host];

                    const other = document.createElement("section");
                    document.body.appendChild(other);
                    const afterUnrelatedMutation = [host.parentNode === document.body, child.isConnected];

                    other.appendChild(child);
                    const reparented = [child.parentNode === other, child.isConnected, host.firstChild === null];

                    let wrongReference = "";
                    try { other.insertBefore(document.createElement("b"), host); }
                    catch (error) { wrongReference = error.name; }
                    let wrongReplacement = "";
                    try { other.replaceChild(document.createElement("i"), host); }
                    catch (error) { wrongReplacement = error.name; }
                    let cycle = "";
                    try { child.appendChild(other); }
                    catch (error) { cycle = error.name; }

                    document.body.removeChild(other);
                    const removedTree = [other.parentNode, other.isConnected, child.isConnected, child.parentNode === other];
                    document.body.appendChild(other);
                    const reattachedTree = [other.isConnected, child.isConnected];
                    return {
                        fresh,
                        detachedTree,
                        connectedTree,
                        afterUnrelatedMutation,
                        reparented,
                        wrongReference,
                        wrongReplacement,
                        cycle,
                        removedTree,
                        reattachedTree,
                    };
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!({
                "fresh": [null, false, null, false],
                "detachedTree": [true, false, false],
                "connectedTree": [true, true, true],
                "afterUnrelatedMutation": [true, true],
                "reparented": [true, true, true],
                "wrongReference": "NotFoundError",
                "wrongReplacement": "NotFoundError",
                "cycle": "HierarchyRequestError",
                "removedTree": [null, false, false, true],
                "reattachedTree": [true, true],
            })
        );
    }

    #[test]
    fn element_scroll_methods_update_scroll_offsets() {
        let mut rt = setup_runtime(
            r#"<div id="scroller" style="width:100px;height:100px;overflow:auto">
                   <div style="width:300px;height:300px"></div>
               </div>"#,
        );
        let result = rt
            .evaluate(
                r#"
                const element = document.getElementById("scroller");
                element.scrollTo({left: 12, top: 20, behavior: "smooth"});
                element.scrollBy(3, -5);
                element.scroll({left: 7});
                return {
                    left: element.scrollLeft,
                    top: element.scrollTop,
                    methods: [
                        typeof element.scroll,
                        typeof element.scrollTo,
                        typeof element.scrollBy,
                    ],
                };
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!({
                "left": 7,
                "top": 15,
                "methods": ["function", "function", "function"],
            })
        );
    }

    #[test]
    fn document_fragment_get_element_by_id_searches_descendants() {
        let mut rt = setup_runtime(r#"<div id="target">document</div>"#);
        let result = rt
            .evaluate(
                r#"
                (() => {
                    const frag = document.createDocumentFragment();
                    const section = document.createElement('section');
                    section.innerHTML = '<div><span id="target">fragment</span></div><p id="a.b">literal</p>';
                    frag.appendChild(section);

                    const dup = document.createDocumentFragment();
                    const deepParent = document.createElement('div');
                    deepParent.innerHTML = '<span id="dup">deep</span>';
                    const shallow = document.createElement('p');
                    shallow.id = 'dup';
                    shallow.textContent = 'shallow';
                    dup.appendChild(deepParent);
                    dup.appendChild(shallow);

                    return [
                        frag.getElementById('target').textContent,
                        frag.getElementById('missing') === null,
                        frag.getElementById('a.b').textContent,
                        frag.getElementById(123) === null,
                        dup.getElementById('dup').textContent,
                    ];
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!(["fragment", true, "literal", true, "deep"])
        );
    }

    /// Issue #461: FILTER_REJECT must prune the rejected node's whole subtree,
    /// while FILTER_SKIP only skips the node and leaves descendants eligible.
    /// Collapsing both into "not accepted" let a TreeWalker yield nodes from
    /// inside a subtree the page explicitly rejected.
    #[test]
    fn tree_walker_filter_reject_prunes_the_whole_subtree() {
        let mut rt = setup_runtime(r#"<div id="root"><section><p>deep</p></section><a></a></div>"#);
        rt.run_page_init();
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                function walk(verdict) {
                    const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                        acceptNode(node) {
                            return node.tagName === 'SECTION' ? verdict : NodeFilter.FILTER_ACCEPT;
                        }
                    });
                    const seen = [];
                    let node;
                    while ((node = w.nextNode())) seen.push(node.tagName);
                    return seen;
                }
                return [walk(NodeFilter.FILTER_REJECT), walk(NodeFilter.FILTER_SKIP)];
                ",
            )
            .unwrap();
        // REJECT drops <p> with its <section> parent; SKIP drops only <section>.
        assert_eq!(result, serde_json::json!([["A"], ["P", "A"]]));
    }

    /// Issue #462: previousNode() must walk reverse document order until a node
    /// is accepted, not give up as soon as the first candidate is filtered out.
    #[test]
    fn previous_node_walks_reverse_document_order() {
        let mut rt = setup_runtime(r#"<div id="root"><a><b></b></a><c></c></div>"#);
        rt.run_page_init();
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                    acceptNode(node) {
                        return node.tagName === 'B'
                            ? NodeFilter.FILTER_SKIP
                            : NodeFilter.FILTER_ACCEPT;
                    }
                });
                const forward = [];
                let node;
                while ((node = w.nextNode())) forward.push(node.tagName);
                const backward = [];
                while ((node = w.previousNode())) backward.push(node.tagName);
                return [forward, backward];
                ",
            )
            .unwrap();
        // From <c>, the previous sibling's deepest last child <b> is skipped, so
        // the walk must keep going up to <a> instead of returning null.
        assert_eq!(result, serde_json::json!([["A", "C"], ["A"]]));
    }

    /// Issue #462: a backward walk must retrace a forward walk exactly, and stop
    /// at the root without ever returning it.
    #[test]
    fn previous_node_retraces_a_full_forward_walk() {
        let mut rt = setup_runtime(
            r#"<div id="root"><section><p>one</p><span></span></section><a><b></b></a></div>"#,
        );
        rt.run_page_init();
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
                const forward = [];
                let node;
                while ((node = w.nextNode())) forward.push(node.tagName);
                const backward = [];
                while ((node = w.previousNode())) backward.push(node.tagName);
                backward.reverse();
                // previousNode never yields root, and never yields the node the
                // forward walk ended on, so compare against forward minus its last.
                // A failed traversal leaves currentNode untouched (DOM 6.1), so
                // it stays on the last node previousNode did return.
                return [forward, backward, w.currentNode.tagName];
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                ["SECTION", "P", "SPAN", "A", "B"],
                ["SECTION", "P", "SPAN", "A"],
                "SECTION"
            ])
        );
    }

    /// Issue #462: FILTER_REJECT prunes a subtree in the backward direction too
    /// — the descent into a rejected node's last children must stop.
    #[test]
    fn previous_node_honours_filter_reject_subtree_pruning() {
        let mut rt =
            setup_runtime(r#"<div id="root"><a></a><section><p>deep</p></section><c></c></div>"#);
        rt.run_page_init();
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                    acceptNode(node) {
                        return node.tagName === 'SECTION'
                            ? NodeFilter.FILTER_REJECT
                            : NodeFilter.FILTER_ACCEPT;
                    }
                });
                while (w.nextNode()) { /* advance to the last accepted node */ }
                const backward = [];
                let node;
                while ((node = w.previousNode())) backward.push(node.tagName);
                return backward;
                ",
            )
            .unwrap();
        // <p> lives inside the rejected <section>, so the backward walk from <c>
        // must jump straight to <a>.
        assert_eq!(result, serde_json::json!(["A"]));
    }

    /// Issue #461: NodeIterator has no subtree pruning — DOM 6.2 says
    /// FILTER_REJECT behaves as FILTER_SKIP there. The shared walker must not
    /// Issue #475: parentNode() must never surface a node above `root`. With
    /// currentNode at root, the old guard stepped to root's own parent and
    /// returned it — escaping the walker's subtree entirely.
    #[test]
    fn tree_walker_parent_node_does_not_escape_above_root() {
        let mut rt = setup_runtime(r#"<div id="root"><a></a></div>"#);
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
                const escaped = w.parentNode();
                return [escaped, w.currentNode.id];
                ",
            )
            .unwrap();
        // No parent within the subtree, and currentNode stays put at root.
        assert_eq!(result, serde_json::json!([null, "root"]));
    }

    /// Issue #475: when the accepted ancestor is `root` itself, parentNode()
    /// returns it and moves currentNode there — the old `parent !== root` guard
    /// wrongly excluded it.
    #[test]
    fn tree_walker_parent_node_can_return_the_root() {
        let mut rt = setup_runtime(r#"<div id="root"><a></a></div>"#);
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
                w.currentNode = root.querySelector('a');
                const p = w.parentNode();
                return [p ? p.id : null, w.currentNode === root];
                ",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["root", true]));
    }

    /// Issue #475: parentNode() climbs past a skipped ancestor to the first
    /// accepted one, instead of stopping at the immediate parent.
    #[test]
    fn tree_walker_parent_node_climbs_past_skipped_ancestors() {
        let mut rt =
            setup_runtime(r#"<div id="root"><main id="m"><section><a></a></section></main></div>"#);
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                    acceptNode(n) {
                        return n.tagName === 'SECTION'
                            ? NodeFilter.FILTER_SKIP
                            : NodeFilter.FILTER_ACCEPT;
                    }
                });
                w.currentNode = root.querySelector('a');
                const p = w.parentNode();
                return p ? p.id : null;
                ",
            )
            .unwrap();
        // <a>'s parent <section> is skipped, so <main> is the first accepted
        // ancestor — not null, and not the immediate <section>.
        assert_eq!(result, serde_json::json!("m"));
    }

    /// leak TreeWalker's pruning into it.
    #[test]
    fn node_iterator_treats_filter_reject_as_skip() {
        let mut rt = setup_runtime(r#"<div id="root"><section><p>deep</p></section><a></a></div>"#);
        rt.run_page_init();
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT, {
                    acceptNode(node) {
                        return node.tagName === 'SECTION'
                            ? NodeFilter.FILTER_REJECT
                            : NodeFilter.FILTER_ACCEPT;
                    }
                });
                const seen = [];
                let node;
                while ((node = it.nextNode())) seen.push(node.tagName);
                return seen;
                ",
            )
            .unwrap();
        // The rejected <section> is skipped but not pruned, so <p> still shows.
        // The leading root is #467: an iterator yields the node it is rooted at.
        assert_eq!(result, serde_json::json!(["DIV", "P", "A"]));
    }

    /// Issue #467: a NodeIterator starts *before* its root, so the first
    /// nextNode() returns the root itself. Aliasing createTreeWalker silently
    /// dropped exactly the element the iterator was rooted at.
    #[test]
    fn node_iterator_yields_the_root_node_first() {
        let mut rt = setup_runtime(r#"<div id="root"><a></a></div>"#);
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT);
                const seen = [];
                let node;
                while ((node = it.nextNode())) seen.push(node.tagName);
                return seen;
                ",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["DIV", "A"]));
    }

    /// Issue #467: the NodeIterator interface surface, and that TreeWalker-only
    /// members are not exposed on it.
    #[test]
    fn node_iterator_exposes_its_own_interface() {
        let mut rt = setup_runtime(r#"<div id="root"><a></a></div>"#);
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT);
                const before = [it.referenceNode === root, it.pointerBeforeReferenceNode];
                it.nextNode();
                return [
                    before,
                    typeof it.detach,
                    it.detach() === undefined,
                    typeof it.previousNode,
                    it.root === root,
                    it.whatToShow,
                    // TreeWalker-only members must not leak onto a NodeIterator.
                    typeof it.currentNode,
                    typeof it.firstChild,
                    typeof it.parentNode,
                    // The pointer advanced past the root it just returned.
                    [it.referenceNode.tagName, it.pointerBeforeReferenceNode],
                ];
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                [true, true],
                "function",
                true,
                "function",
                true,
                1,
                "undefined",
                "undefined",
                "undefined",
                ["DIV", false]
            ])
        );
    }

    /// Issue #467: previousNode() retraces the iterator, and the root is the
    /// last node it yields going backwards.
    #[test]
    fn node_iterator_previous_node_retraces_the_walk() {
        let mut rt = setup_runtime(r#"<div id="root"><a><b></b></a><c></c></div>"#);
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                const it = document.createNodeIterator(root, NodeFilter.SHOW_ELEMENT);
                const forward = [];
                let node;
                while ((node = it.nextNode())) forward.push(node.tagName);
                const backward = [];
                while ((node = it.previousNode())) backward.push(node.tagName);
                return [forward, backward];
                ",
            )
            .unwrap();
        // Forward ends on <c>; going back re-yields <c> (the pointer sits after
        // it), then the rest in reverse, root included.
        assert_eq!(
            result,
            serde_json::json!([["DIV", "A", "B", "C"], ["C", "B", "A", "DIV"]])
        );
    }

    /// Issue #463: `<template>` contents are parsed into the node's
    /// `template_contents` document, but no op exposed it, so `.content` handed
    /// back a fabricated empty fragment and the parsed markup was unreachable.
    #[test]
    fn template_content_exposes_parsed_markup() {
        let mut rt = setup_runtime(
            r#"<body><template id="t"><p class="row">a</p><p class="row">b</p></template></body>"#,
        );
        let result = rt
            .evaluate(
                r"
                const t = document.getElementById('t');
                return [
                    t.content.childNodes.length,
                    t.content.querySelectorAll('.row').length,
                    t.content.firstElementChild.textContent,
                    t.innerHTML,
                    t.content.nodeType,
                    t.content instanceof DocumentFragment,
                    // Identity is stable: frameworks stash `.content` and reuse it.
                    t.content === t.content,
                    // The children stay off the element itself, per the HTML spec.
                    t.childNodes.length,
                ];
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                2,
                2,
                "a",
                r#"<p class="row">a</p><p class="row">b</p>"#,
                11,
                true,
                true,
                0
            ])
        );
    }

    /// Setting innerHTML on the <html> element parses in the "before head"
    /// insertion mode, which synthesizes head and body. The importer must keep
    /// both; it previously returned the synthesized body and dropped the head
    /// (so a <title>/<meta> assigned this way vanished).
    #[test]
    fn documentelement_inner_html_keeps_head_and_body() {
        let mut rt = setup_runtime("<html><head></head><body></body></html>");
        let v = rt
            .evaluate(
                "(function(){ document.documentElement.innerHTML = '<head><title>T</title></head><body><p>hi</p></body>'; \
                 var t = document.querySelector('title'); var p = document.querySelector('p'); \
                 return (t ? t.textContent : 'no-title') + '|' + (p ? p.textContent : 'no-p'); })()",
            )
            .unwrap();
        assert_eq!(v, serde_json::json!("T|hi"));
    }

    /// Regression guard: innerHTML on an ordinary element still imports the
    /// parsed nodes directly (no head/body is synthesized for a div context),
    /// so the fix above must not change the common case.
    #[test]
    fn ordinary_element_inner_html_imports_content_directly() {
        let mut rt = setup_runtime("<html><body><div id=\"d\"></div></body></html>");
        let v = rt
            .evaluate(
                "(function(){ var d=document.getElementById('d'); d.innerHTML='<span>a</span><span>b</span>'; \
                 return d.children.length + '|' + d.textContent; })()",
            )
            .unwrap();
        assert_eq!(v, serde_json::json!("2|ab"));
    }

    /// Issue #463: the same must hold for a template that arrives via innerHTML
    /// rather than the initial document parse — that is how most frameworks
    /// inject templates.
    #[test]
    fn template_content_works_for_templates_added_via_inner_html() {
        let mut rt = setup_runtime(r#"<body><div id="host"></div></body>"#);
        let result = rt
            .evaluate(
                r#"
                const host = document.getElementById('host');
                host.innerHTML = '<template id="t2"><li class="item">x</li></template>';
                const t = document.getElementById('t2');
                const stamped = t.content.cloneNode(true);
                host.appendChild(stamped);
                return [
                    t.content.childNodes.length,
                    t.content.querySelector('.item').textContent,
                    host.querySelectorAll('li.item').length,
                ];
                "#,
            )
            .unwrap();
        // cloneNode(true) of the content is the canonical stamping idiom.
        assert_eq!(result, serde_json::json!([1, "x", 1]));
    }

    /// Issue #463: a template built with createElement has no parsed contents,
    /// so `.content` must allocate a backing fragment on demand and round-trip
    /// through innerHTML.
    #[test]
    fn template_content_round_trips_for_created_templates() {
        let mut rt = setup_runtime(r"<body></body>");
        let result = rt
            .evaluate(
                r#"
                const t = document.createElement('template');
                t.innerHTML = '<span class="s">hi</span>';
                return [
                    t.content.childNodes.length,
                    t.content.querySelector('.s').textContent,
                    t.innerHTML,
                    t.childNodes.length,
                ];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([1, "hi", r#"<span class="s">hi</span>"#, 0])
        );
    }

    /// Issue #463: serializing a `<template>` must emit its contents, or the
    /// markup silently disappears from outerHTML/innerHTML round-trips — and
    /// `cloneNode(true)`, which round-trips through outer_html, yields an empty
    /// template.
    #[test]
    fn template_contents_survive_serialization_and_clone() {
        let mut rt =
            setup_runtime(r#"<body><template id="t"><li class="item">x</li></template></body>"#);
        let result = rt
            .evaluate(
                r"
                const t = document.getElementById('t');
                const clone = t.cloneNode(true);
                return [
                    t.outerHTML,
                    document.body.innerHTML,
                    clone.content.childNodes.length,
                    clone.content.querySelector('.item').textContent,
                    // The clone's contents are its own, not shared with the original.
                    (clone.content.firstElementChild === t.content.firstElementChild),
                ];
                ",
            )
            .unwrap();
        let expected = r#"<template id="t"><li class="item">x</li></template>"#;
        assert_eq!(
            result,
            serde_json::json!([expected, expected, 1, "x", false])
        );
    }

    /// Issue #468: window.scrollTo/scrollBy/scroll were no-op stubs, so the
    /// dominant infinite-scroll idiom never advanced the page offset.
    #[test]
    fn window_scroll_methods_move_the_page_offset() {
        let mut rt = setup_runtime(r#"<html><body><div id="d"></div></body></html>"#);
        let result = rt
            .evaluate(
                r"
                const scrolled = window.scrollTo(0, 500);
                const afterTo = [window.scrollX, window.scrollY];
                window.scrollBy(0, 200);
                const afterBy = [window.pageXOffset, window.pageYOffset];
                window.scrollTo({ left: 10, top: 40 });
                const afterOptions = [window.scrollX, window.scrollY];
                window.scroll(5, 5);
                const afterScroll = [window.scrollX, window.scrollY];
                // Negative offsets clamp to 0, as they do for elements.
                window.scrollTo(0, -100);
                return [afterTo, afterBy, afterOptions, afterScroll, window.scrollY];
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([[0, 500], [0, 700], [10, 40], [5, 5], 0])
        );
    }

    /// Issue #468: the page offset is one value, readable and writable through
    /// either `window.scrollY` or `document.scrollingElement.scrollTop`.
    #[test]
    fn window_scroll_offset_is_shared_with_the_scrolling_element() {
        let mut rt = setup_runtime(r#"<html><body><div id="d"></div></body></html>"#);
        let result = rt
            .evaluate(
                r"
                const isDocEl = document.scrollingElement === document.documentElement;
                window.scrollTo(0, 300);
                // Written through the window, read through the element...
                const viaElement = document.scrollingElement.scrollTop;
                // ...and the reverse.
                document.scrollingElement.scrollTop = 90;
                return [isDocEl, viaElement, window.scrollY, window.pageYOffset];
                ",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!([true, 300, 90, 90]));
    }

    /// Issue #468: a scroll event must reach listeners on both the window and
    /// the document — that is the signal lazy loaders wait for.
    #[tokio::test(flavor = "current_thread")]
    async fn window_scroll_fires_a_scroll_event() {
        let mut rt = setup_runtime(r#"<html><body><div id="d"></div></body></html>"#);
        let result = rt
            .evaluate_for_cdp(
                r"
                new Promise(resolve => {
                    let win = 0, doc = 0;
                    window.addEventListener('scroll', () => win++);
                    document.addEventListener('scroll', () => doc++);
                    window.scrollBy(0, 400);
                    setTimeout(() => resolve([win, doc, window.scrollY]), 5);
                })
                ",
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!([1, 1, 400]));
    }

    #[test]
    fn non_render_cssom_rects_keep_compatibility_geometry() {
        let mut rt = setup_runtime(r#"<html><body><div id="box"></div></body></html>"#);
        let result = rt
            .evaluate(
                r#"
                const box = document.getElementById("box");
                const detached = document.createElement("div");
                return [box, detached].map(element => {
                    const rect = element.getBoundingClientRect();
                    return [rect.width, rect.height, element.getClientRects().length];
                });
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!([[100, 20, 1], [100, 20, 1]]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn intersection_observer_initial_geometry_waits_for_one_render_checkpoint() {
        let mut rt = setup_runtime(
            r#"<html><body><div id="first"></div><div id="second"></div></body></html>"#,
        );
        rt.execute_script(
            "intersection-render-checkpoint",
            r#"
                globalThis.__ioOrder = ["sync"];
                globalThis.__ioReads = 0;
                const first = document.getElementById("first");
                const second = document.getElementById("second");
                for (const element of [first, second]) {
                    const nativeRect = element.getBoundingClientRect.bind(element);
                    element.getBoundingClientRect = () => {
                        __ioReads++;
                        return nativeRect();
                    };
                }
                const observer = new IntersectionObserver(
                    () => __ioOrder.push("observer")
                );
                observer.observe(first);
                observer.observe(second);
                Promise.resolve().then(() => __ioOrder.push("microtask"));
                __ioOrder.push("after-observe-" + __ioReads);
            "#,
        )
        .unwrap();

        assert_eq!(
            rt.evaluate("[__ioOrder, __ioReads]").unwrap(),
            serde_json::json!([["sync", "after-observe-0", "microtask"], 0])
        );
        rt.run_event_loop_bounded(100).await.unwrap();
        let expected_geometry_reads = 2;
        assert_eq!(
            rt.evaluate("[__ioOrder, __ioReads]").unwrap(),
            serde_json::json!([
                ["sync", "after-observe-0", "microtask", "observer"],
                expected_geometry_reads,
            ])
        );
    }

    /// Issue #469: FILTER_SKIP leaves a skipped node's children eligible, so
    /// firstChild()/lastChild() must descend into them. FILTER_REJECT must not.
    #[test]
    fn tree_walker_child_movers_descend_on_skip_but_not_on_reject() {
        let mut rt = setup_runtime(r#"<div id="root"><section><a></a><b></b></section></div>"#);
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                function mover(verdict, method) {
                    const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                        acceptNode(node) {
                            return node.tagName === 'SECTION' ? verdict : NodeFilter.FILTER_ACCEPT;
                        }
                    });
                    const found = w[method]();
                    return found ? found.tagName : null;
                }
                return [
                    mover(NodeFilter.FILTER_SKIP, 'firstChild'),
                    mover(NodeFilter.FILTER_SKIP, 'lastChild'),
                    mover(NodeFilter.FILTER_REJECT, 'firstChild'),
                    mover(NodeFilter.FILTER_REJECT, 'lastChild'),
                ];
                ",
            )
            .unwrap();
        // SKIP descends into <section>; REJECT prunes it and finds nothing else.
        assert_eq!(result, serde_json::json!(["A", "B", null, null]));
    }

    /// Issue #469: nextSibling()/previousSibling() must descend into a skipped
    /// sibling's subtree rather than stepping straight over it.
    #[test]
    fn tree_walker_sibling_movers_descend_into_skipped_siblings() {
        let mut rt = setup_runtime(
            r#"<div id="root"><p id="start"></p><section><a></a></section><q></q></div>"#,
        );
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                function mover(verdict, method, from) {
                    const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                        acceptNode(node) {
                            return node.tagName === 'SECTION' ? verdict : NodeFilter.FILTER_ACCEPT;
                        }
                    });
                    w.currentNode = document.getElementById(from);
                    const found = w[method]();
                    return found ? found.tagName : null;
                }
                return [
                    // <section> is skipped, so its child <a> is the next sibling.
                    mover(NodeFilter.FILTER_SKIP, 'nextSibling', 'start'),
                    // Rejected: the subtree is off-limits, so skip past to <q>.
                    mover(NodeFilter.FILTER_REJECT, 'nextSibling', 'start'),
                ];
                ",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["A", "Q"]));
    }

    /// Issue #469: the backward sibling mover descends to *last* children.
    #[test]
    fn tree_walker_previous_sibling_descends_to_last_child() {
        let mut rt = setup_runtime(
            r#"<div id="root"><section><a></a><b></b></section><p id="start"></p></div>"#,
        );
        let result = rt
            .evaluate(
                r"
                const root = document.getElementById('root');
                const w = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT, {
                    acceptNode(node) {
                        return node.tagName === 'SECTION'
                            ? NodeFilter.FILTER_SKIP
                            : NodeFilter.FILTER_ACCEPT;
                    }
                });
                w.currentNode = document.getElementById('start');
                const found = w.previousSibling();
                return found ? found.tagName : null;
                ",
            )
            .unwrap();
        // Reverse order descends to <section>'s last child, not its first.
        assert_eq!(result, serde_json::json!("B"));
    }

    #[test]
    fn append_child_flattens_document_fragment() {
        let mut rt = setup_runtime(r#"<main id="host"></main>"#);
        let result = rt
            .evaluate(
                r"
                const host = document.getElementById('host');
                const fragment = document.createDocumentFragment();
                const first = document.createElement('article');
                const second = document.createElement('article');
                first.id = 'first';
                second.id = 'second';
                first.className = second.className = 'quote';
                fragment.appendChild(first);
                fragment.appendChild(second);

                const returned = host.appendChild(fragment);
                return [
                    returned === fragment,
                    Array.from(host.children).map(node => node.id),
                    host.querySelectorAll('.quote').length,
                    fragment.childNodes.length,
                    first.parentNode === host,
                    first.parentElement === host,
                ];
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, ["first", "second"], 2, 0, true, true])
        );
    }

    #[test]
    fn insert_before_flattens_document_fragment_in_order() {
        let mut rt = setup_runtime(r#"<main id="host"><article id="last"></article></main>"#);
        let result = rt
            .evaluate(
                r"
                const host = document.getElementById('host');
                const last = document.getElementById('last');
                const fragment = document.createDocumentFragment();
                const first = document.createElement('article');
                const second = document.createElement('article');
                first.id = 'first';
                second.id = 'second';
                fragment.appendChild(first);
                fragment.appendChild(second);

                const returned = host.insertBefore(fragment, last);
                return [
                    returned === fragment,
                    Array.from(host.children).map(node => node.id),
                    fragment.childNodes.length,
                    first.parentElement === host,
                    second.parentElement === host,
                ];
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, ["first", "second", "last"], 0, true, true])
        );
    }

    #[test]
    fn replace_child_flattens_document_fragment_and_removes_old_child() {
        let mut rt = setup_runtime(
            r#"<main id="host"><article id="old"></article><article id="tail"></article></main>"#,
        );
        let result = rt
            .evaluate(
                r"
                const host = document.getElementById('host');
                const old = document.getElementById('old');
                const fragment = document.createDocumentFragment();
                const first = document.createElement('article');
                const second = document.createElement('article');
                first.id = 'first';
                second.id = 'second';
                fragment.appendChild(first);
                fragment.appendChild(second);

                const returned = host.replaceChild(fragment, old);
                return [
                    returned === old,
                    Array.from(host.children).map(node => node.id),
                    fragment.childNodes.length,
                    old.parentNode === null,
                    first.parentElement === host,
                    second.parentElement === host,
                ];
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, ["first", "second", "tail"], 0, true, true, true])
        );
    }

    #[test]
    fn template_inner_html_preserves_table_fragments() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r"
                const template = document.createElement('template');
                template.innerHTML = '<tr><td>first</td><td>second</td></tr>';
                const clone = template.content.firstChild.cloneNode(true);
                return [
                    clone.tagName,
                    clone.firstElementChild.tagName,
                    clone.firstElementChild.children.length,
                    clone.textContent,
                ];
                ",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["TR", "TD", 0, "firstsecond"]));
    }

    #[test]
    fn document_exposes_parent_node_element_children_api() {
        let mut rt = setup_runtime("<html><head></head><body></body></html>");
        let result = rt
            .evaluate(
                "return [document.firstElementChild === document.documentElement,\
                         document.lastElementChild === document.documentElement,\
                         document.children.length, document.childElementCount];",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!([true, true, 1, 1]));
    }

    #[test]
    fn atob_decodes_large_payload_without_argument_stack_overflow() {
        let mut rt = setup_runtime("<html><body></body></html>");
        // 60k four-character groups decode to 180k bytes, comfortably above
        // V8's maximum argument count for a single fromCharCode(...bytes).
        let encoded = "QUFB".repeat(60_000);
        let result = rt.evaluate(&format!("atob('{encoded}').length")).unwrap();
        assert_eq!(result.as_f64().unwrap() as usize, 180_000);
    }

    #[test]
    fn navigation_api_updates_current_entry_state() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r"
                (() => {
                    navigation.updateCurrentEntry({state: {route: 'home'}});
                    const first = navigation.currentEntry;
                    navigation.navigate('/docs', {state: {route: 'docs'}});
                    return [
                        typeof navigation.updateCurrentEntry,
                        first.getState().route,
                        navigation.currentEntry.getState().route,
                        navigation.currentEntry.url,
                    ];
                })()
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!(["function", "home", "docs", "http://example.com/docs"])
        );
    }

    #[test]
    fn inline_stylesheet_cssom_lists_and_rules_are_live_same_objects() {
        let mut rt = setup_runtime(
            r#"<html><head>
                <style id="first">.one { color:red } .two { width:20px }</style>
                <style id="second">.three { display:block }</style>
            </head><body></body></html>"#,
        );
        let result = rt
            .evaluate(
                r"
                (() => {
                    const list = document.styleSheets;
                    const style = document.getElementById('first');
                    const sheet = style.sheet;
                    const rules = sheet.cssRules;
                    const firstRule = rules[0];
                    const initial = [
                        list === document.styleSheets,
                        list.length,
                        list[0] === sheet,
                        list.item(0) === sheet,
                        list.item(9),
                        sheet.ownerNode === style,
                        rules === sheet.cssRules,
                        rules.length,
                        rules.item(0) === firstRule,
                        firstRule instanceof CSSRule,
                        firstRule instanceof CSSStyleRule,
                        firstRule.type,
                        firstRule.selectorText,
                        firstRule.style.color,
                        firstRule.parentStyleSheet === sheet,
                    ];

                    sheet.insertRule('.middle { height: 30px; }', 1);
                    const inserted = [
                        rules.length,
                        rules[0] === firstRule,
                        rules[1].selectorText,
                        style.textContent.includes('.middle'),
                    ];
                    sheet.deleteRule(1);

                    const extra = document.createElement('style');
                    document.head.appendChild(extra);
                    const afterAppend = list.length;
                    const emptySheet = extra.sheet;
                    const emptyIdentity = emptySheet === list[2]
                        && emptySheet.cssRules.length === 0;
                    extra.textContent = '.extra { opacity:.5 }';
                    const emptyBecameLive = emptySheet.cssRules.length === 1;
                    extra.remove();
                    const afterRemove = list.length;

                    style.textContent = '.replacement { padding: 4px; }';
                    const reparsed = [
                        style.sheet === sheet,
                        sheet.cssRules === rules,
                        rules.length,
                        rules[0].selectorText,
                        rules[0].style.padding,
                    ];
                    style.remove();
                    const disconnected = style.sheet === null
                        && sheet.ownerNode === null
                        && list.length === 1;
                    document.head.appendChild(style);
                    const reconnected = style.sheet !== sheet
                        && style.sheet.ownerNode === style
                        && list.length === 2;

                    const left = document.createElement('div');
                    const right = document.createElement('div');
                    document.body.append(left, right);
                    const moving = document.createElement('style');
                    moving.textContent = '.moving { color: red }';
                    left.appendChild(moving);
                    const beforeMove = moving.sheet;
                    right.appendChild(moving);
                    const reparented = beforeMove.ownerNode === null
                        && moving.sheet !== beforeMove
                        && moving.sheet.ownerNode === moving;
                    right.remove();
                    left.remove();

                    const bulk = document.createElement('div');
                    document.body.appendChild(bulk);
                    bulk.innerHTML = '<style>.bulk { color: blue }</style><span></span>';
                    const bulkSheet = bulk.querySelector('style').sheet;
                    bulk.innerHTML = '';
                    const innerHTMLDetached = bulkSheet.ownerNode === null;
                    bulk.innerHTML = '<section><style>.text { color: green }</style></section>';
                    const textSheet = bulk.querySelector('style').sheet;
                    bulk.textContent = '';
                    const textContentDetached = textSheet.ownerNode === null;
                    bulk.remove();
                    return {
                        initial, inserted, afterAppend, emptyIdentity, emptyBecameLive,
                        afterRemove, reparsed, disconnected, reconnected, reparented,
                        innerHTMLDetached, textContentDetached,
                    };
                })()
                ",
            )
            .unwrap();

        assert_eq!(
            result,
            serde_json::json!({
                "initial": [true, 2, true, true, null, true, true, 2, true,
                    true, true, 1, ".one", "red", true],
                "inserted": [3, true, ".middle", true],
                "afterAppend": 3,
                "emptyIdentity": true,
                "emptyBecameLive": true,
                "afterRemove": 2,
                "reparsed": [true, true, 1, ".replacement", "4px"],
                "disconnected": true,
                "reconnected": true,
                "reparented": true,
                "innerHTMLDetached": true,
                "textContentDetached": true,
            })
        );
    }

    #[test]
    fn adopted_stylesheets_materialize_into_the_document() {
        let mut rt =
            setup_runtime("<html><head></head><body><div class=\"card\"></div></body></html>");
        let result = rt
            .evaluate(
                r#"
                (() => {
                    const sheet = new CSSStyleSheet();
                    document.adoptedStyleSheets.push(sheet);
                    sheet.insertRule('.card { display: flex; color: red; }', 0);
                    const node = document.querySelector('style[data-tinybrowser-adopted]');
                    const inserted = node.textContent;
                    sheet.replaceSync('.card { content: "a;b"; background-image: url("data:image/svg+xml;utf8,<svg/>"); }');
                    const preserved = [
                        sheet.cssRules[0].style.content,
                        sheet.cssRules[0].style.backgroundImage,
                        node.textContent.includes('a;b'),
                        node.textContent.includes('svg+xml;utf8'),
                    ];
                    sheet.deleteRule(0);
                    return [
                        document.adoptedStyleSheets.length,
                        document.querySelectorAll('style[data-tinybrowser-adopted]').length,
                        inserted.includes('display: flex'),
                        preserved,
                        node.textContent,
                    ];
                })()
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                1,
                1,
                true,
                [
                    "\"a;b\"",
                    "url(\"data:image/svg+xml;utf8,<svg/>\")",
                    true,
                    true
                ],
                ""
            ])
        );
    }

    #[test]
    fn shadow_stylesheet_lists_and_adoption_are_live_across_roots() {
        let mut rt = setup_runtime(
            "<html><head></head><body><div id='one'></div><div id='two'></div></body></html>",
        );
        let result = rt
            .evaluate(
                r"
                (() => {
                    const first = document.getElementById('one').attachShadow({ mode: 'open' });
                    const second = document.getElementById('two').attachShadow({ mode: 'open' });
                    const inline = document.createElement('style');
                    inline.textContent = '.local { width: 17px }';
                    first.appendChild(inline);
                    const inlineSheet = inline.sheet;
                    const firstList = first.styleSheets;
                    const firstAdopted = first.adoptedStyleSheets;
                    const secondAdopted = second.adoptedStyleSheets;
                    const documentAdopted = document.adoptedStyleSheets;

                    const shared = new CSSStyleSheet();
                    first.adoptedStyleSheets = [shared];
                    second.adoptedStyleSheets.push(shared);
                    document.adoptedStyleSheets = [shared];

                    const firstNode = first.querySelector('style[data-tinybrowser-adopted]');
                    const secondNode = second.querySelector('style[data-tinybrowser-adopted]');
                    const documentNode = document.querySelector('style[data-tinybrowser-adopted]');
                    const initial = [
                        first.styleSheets === firstList,
                        firstList.length,
                        firstList[0] === inlineSheet,
                        firstList.item(0) === inlineSheet,
                        second.styleSheets === second.styleSheets,
                        second.styleSheets.length,
                        first.adoptedStyleSheets === firstAdopted,
                        second.adoptedStyleSheets === secondAdopted,
                        document.adoptedStyleSheets === documentAdopted,
                        firstAdopted.length,
                        secondAdopted.length,
                        documentAdopted.length,
                        firstNode.parentNode === first,
                        secondNode.parentNode === second,
                        documentNode.parentNode === document.head,
                    ];

                    shared.insertRule('.shared { width: 31px }', 0);
                    const synchronized = [firstNode, secondNode, documentNode]
                        .map(node => node.textContent.includes('width: 31px'));

                    second.adoptedStyleSheets = [];
                    shared.replaceSync('.shared { width: 47px }');
                    const afterRemoval = [
                        second.adoptedStyleSheets === secondAdopted,
                        secondAdopted.length,
                        secondNode.parentNode,
                        second.querySelectorAll('style[data-tinybrowser-adopted]').length,
                        firstNode.textContent.includes('width: 47px'),
                        documentNode.textContent.includes('width: 47px'),
                        secondNode.textContent.includes('width: 31px'),
                    ];

                    inline.remove();
                    const inlineRemoval = [
                        first.styleSheets === firstList,
                        firstList.length,
                        inlineSheet.ownerNode,
                    ];
                    return { initial, synchronized, afterRemoval, inlineRemoval };
                })()
                ",
            )
            .unwrap();

        assert_eq!(
            result,
            serde_json::json!({
                "initial": [
                    true, 1, true, true, true, 0,
                    true, true, true, 1, 1, 1,
                    true, true, true
                ],
                "synchronized": [true, true, true],
                "afterRemoval": [true, 0, null, 0, true, true, true],
                "inlineRemoval": [true, 0, null],
            })
        );
    }

    #[test]
    fn unavailable_webgl_context_does_not_claim_success() {
        let mut rt = setup_runtime("<html><body><canvas></canvas></body></html>");
        let result = rt
            .evaluate(
                r"
                (() => {
                    const canvas = document.querySelector('canvas');
                    const fallback = document.createElement('p');
                    if (!canvas.getContext('webgl')) {
                        fallback.textContent = 'static fallback';
                        document.body.appendChild(fallback);
                    }
                    return [
                        canvas.getContext('webgl'),
                        canvas.getContext('webgl2'),
                        canvas.getContext('experimental-webgl'),
                        fallback.isConnected,
                        fallback.textContent,
                    ];
                })()
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([null, null, null, true, "static fallback"])
        );
    }

    #[test]
    fn page_var_declarations_do_not_collide_with_dom_interfaces() {
        let mut rt = setup_runtime("<html><body></body></html>");

        rt.execute_script(
            "legacy-node-guard",
            "if (!window.Node) { var Node = {}; } globalThis.__legacyNodeRan = true;",
        )
        .unwrap();
        assert_eq!(
            rt.evaluate("globalThis.__legacyNodeRan").unwrap(),
            serde_json::json!(true)
        );

        rt.execute_script(
            "page-element",
            "var Element = function PageElement() {}; globalThis.__createdTag = document.createElement('div').tagName;",
        )
        .unwrap();
        assert_eq!(
            rt.evaluate("globalThis.__createdTag").unwrap(),
            serde_json::json!("DIV")
        );
    }

    #[test]
    fn dynamic_script_status_bridge_is_hidden_and_idle() {
        let mut rt = setup_runtime("<html><body></body></html>");
        assert!(!rt.has_pending_dynamic_scripts());
        assert!(!rt.has_pending_load_delaying_scripts());
        assert_eq!(rt.next_pending_timeout_delay_ms(), None);
        assert_eq!(
            rt.evaluate("typeof __dynScriptBusy").unwrap(),
            serde_json::json!("undefined")
        );
        assert_eq!(
            rt.evaluate(
                "Object.getOwnPropertyNames(globalThis).includes('__tinybrowser_hasPendingDynamicScripts')"
            )
            .unwrap(),
            serde_json::json!(false)
        );
        assert_eq!(
            rt.evaluate(
                "Reflect.ownKeys(globalThis).includes('__tinybrowser_hasPendingDynamicScripts')"
            )
            .unwrap(),
            serde_json::json!(false)
        );
        assert_eq!(
            rt.evaluate(
                "Reflect.ownKeys(globalThis).includes('__tinybrowser_hasPendingLoadDelayingScripts')"
            )
            .unwrap(),
            serde_json::json!(false)
        );
        assert_eq!(
            rt.evaluate(
                "Object.getOwnPropertyNames(globalThis).includes('__tinybrowser_nextPendingTimeoutDelay')"
            )
            .unwrap(),
            serde_json::json!(false)
        );
        assert_eq!(
            rt.evaluate(
                "Reflect.ownKeys(globalThis).includes('__tinybrowser_nextPendingTimeoutDelay')"
            )
            .unwrap(),
            serde_json::json!(false)
        );
    }

    /// Regression test for #147: a TypeError in one script must not poison
    /// the runtime so that subsequent scripts (or DOM queries) collapse to
    /// empty. The reporter saw `--dump text` return 1 byte after offside.js
    /// crashed; that cascade should never happen.
    #[test]
    fn script_typeerror_does_not_poison_subsequent_execution() {
        let mut rt = setup_runtime("<html><body><p id=hit>BODY_TEXT</p></body></html>");

        // 1. First script throws the same flavor of error offside.js produced
        //    (`Cannot read properties of undefined (reading 'classList')`).
        let err = rt
            .execute_script("buggy", "var x; x.classList.add('y');")
            .unwrap_err();
        assert!(
            err.contains("classList") || err.contains("undefined"),
            "expected classList/undefined error, got: {err}"
        );

        // 2. The runtime must still be usable: a follow-up script runs.
        rt.execute_script("ok", "globalThis.__after_error = 'still alive';")
            .unwrap();
        let result = rt.evaluate("globalThis.__after_error").unwrap();
        assert_eq!(result, serde_json::json!("still alive"));

        // 3. DOM queries still work after the script error.
        let text = rt
            .evaluate("document.querySelector('#hit').textContent")
            .unwrap();
        assert_eq!(text, serde_json::json!("BODY_TEXT"));
    }

    /// Regression test for #355: an explicit `throw` in one inline <script> must
    /// not stop later independent <script>s from running. Each <script> executes
    /// as its own `execute_script` call, mirroring how page.rs runs them, so a
    /// thrown error is reported but the next script still runs.
    #[test]
    fn thrown_error_in_one_script_does_not_stop_later_scripts() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script("s1", "globalThis.__ran1 = true;")
            .unwrap();
        let err = rt
            .execute_script(
                "s2",
                "throw new Error('only one instance of babel-polyfill is allowed');",
            )
            .unwrap_err();
        assert!(
            err.contains("babel-polyfill"),
            "expected the thrown message, got: {err}"
        );
        rt.execute_script("s3", "globalThis.__ran3 = true;")
            .unwrap();
        let ran = rt
            .evaluate("JSON.stringify([globalThis.__ran1 === true, globalThis.__ran3 === true])")
            .unwrap();
        assert_eq!(ran, serde_json::json!("[true,true]"));
    }

    /// Regression test for #356: the `in` operator and `Object.keys` must work on
    /// `el.style` (CSSStyleDeclaration) and `el.dataset` (DOMStringMap), `_props`
    /// must not leak, and cssText must serialize dashed names with a trailing
    /// semicolon.
    #[test]
    fn style_and_dataset_support_in_operator_and_keys() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r"(() => {
                    const el = document.createElement('div');
                    el.style.color = 'red';
                    el.style.fontSize = '14px';
                    el.dataset.foo = 'bar';
                    const keys = Object.keys(el.style);
                    return JSON.stringify({
                        colorInStyle: 'color' in el.style,
                        objectFitInStyle: 'object-fit' in el.style,
                        keysHasSet: keys.includes('color') && keys.includes('fontSize'),
                        noPropsLeak: !keys.includes('_props'),
                        fooInDataset: 'foo' in el.dataset,
                        datasetKeys: Object.keys(el.dataset),
                        cssText: el.style.cssText,
                        length: el.style.length,
                        getByDash: el.style.getPropertyValue('font-size'),
                        reflectedAttribute: el.getAttribute('style')
                    });
                })()",
            )
            .unwrap();
        let p: serde_json::Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
        assert_eq!(p["colorInStyle"], true);
        assert_eq!(p["objectFitInStyle"], true);
        assert_eq!(p["keysHasSet"], true);
        assert_eq!(p["noPropsLeak"], true);
        assert_eq!(p["fooInDataset"], true);
        assert_eq!(p["datasetKeys"], serde_json::json!(["foo"]));
        assert_eq!(p["cssText"], "color: red; font-size: 14px;");
        assert_eq!(p["length"], 2);
        assert_eq!(p["getByDash"], "14px");
        assert_eq!(p["reflectedAttribute"], "color: red; font-size: 14px;");
    }

    #[test]
    fn style_declaration_reflects_and_removes_parsed_attributes() {
        let mut rt = setup_runtime(
            "<html><body><div id='icon' style='font-size: 0px; color: red'></div></body></html>",
        );
        let result = rt
            .evaluate(
                r"(() => {
                    const el = document.getElementById('icon');
                    const before = [el.style.fontSize, el.style.color, el.style.length];
                    const removed = el.style.removeProperty('font-size');
                    return JSON.stringify({
                        before,
                        removed,
                        after: el.style.cssText,
                        attribute: el.getAttribute('style')
                    });
                })()",
            )
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
        assert_eq!(value["before"], serde_json::json!(["0px", "red", 2]));
        assert_eq!(value["removed"], "0px");
        assert_eq!(value["after"], "color: red;");
        assert_eq!(value["attribute"], "color: red;");
    }

    #[test]
    fn select_add_and_option_text_update_the_live_dom() {
        let mut rt = setup_runtime("<html><body><select id='language'></select></body></html>");
        let result = rt
            .evaluate(
                r"(() => {
                    const select = document.getElementById('language');
                    const english = document.createElement('option');
                    english.value = 'en';
                    english.text = 'English';
                    english.selected = true;
                    select.add(english);
                    const greek = document.createElement('option');
                    greek.value = 'el';
                    greek.text = 'Greek';
                    select.add(greek, 0);
                    return JSON.stringify({
                        labels: [...select.options].map(option => option.textContent),
                        selectedIndex: select.selectedIndex,
                        value: select.value,
                        html: select.outerHTML
                    });
                })()",
            )
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
        assert_eq!(value["labels"], serde_json::json!(["Greek", "English"]));
        assert_eq!(value["selectedIndex"], 1);
        assert_eq!(value["value"], "en");
        assert!(value["html"]
            .as_str()
            .unwrap()
            .contains(r#"<option value="en" selected="">English</option>"#));
    }

    /// Regression for #105: `element.querySelector` and `querySelectorAll`
    /// must scope to the receiver's subtree, not the whole document.
    #[test]
    fn element_query_selector_is_scoped_to_subtree() {
        let mut rt = setup_runtime(
            r#"<div id="a"><span class="x">in a</span></div><div id="b"><span class="x">in b</span></div>"#,
        );
        let text = rt
            .evaluate("document.getElementById('a').querySelector('.x').textContent")
            .unwrap();
        assert_eq!(text, serde_json::json!("in a"));

        let count_in_a = rt
            .evaluate("document.getElementById('a').querySelectorAll('.x').length")
            .unwrap();
        assert_eq!(count_in_a.as_f64().unwrap() as i64, 1);

        // Document-scoped query still sees both.
        let count_doc = rt
            .evaluate("document.querySelectorAll('.x').length")
            .unwrap();
        assert_eq!(count_doc.as_f64().unwrap() as i64, 2);
    }

    #[test]
    fn document_evaluate_exposes_basic_xpath_result() {
        let mut rt = setup_runtime("");

        let exposed = rt
            .evaluate("`${typeof XPathResult}:${typeof Document.prototype.evaluate}:${XPathResult.FIRST_ORDERED_NODE_TYPE}`")
            .unwrap();
        assert_eq!(exposed, serde_json::json!("function:function:9"));
    }

    /// Regression for #105: `document.forms` / `images` / `links` must be
    /// live, not hardcoded `[]`. jQuery 1.x's submit-event setup iterates
    /// `document.forms` and crashes when it's empty for pages that have forms.
    #[test]
    fn document_forms_images_links_are_live() {
        let mut rt =
            setup_runtime(r#"<form></form><form></form><img><a href="x">l</a><a>no-href</a>"#);
        assert_eq!(
            rt.evaluate("document.forms.length")
                .unwrap()
                .as_f64()
                .unwrap() as i64,
            2
        );
        assert_eq!(
            rt.evaluate("document.images.length")
                .unwrap()
                .as_f64()
                .unwrap() as i64,
            1
        );
        assert_eq!(
            rt.evaluate("document.links.length")
                .unwrap()
                .as_f64()
                .unwrap() as i64,
            1
        );
    }

    /// Regression for #105: `HTMLFormElement` must expose `.elements` so
    /// frameworks that probe form field collections work.
    #[test]
    fn html_form_element_exposes_elements_collection() {
        let mut rt = setup_runtime(
            r#"<form id="f"><input name=a><input name=b><textarea></textarea></form>"#,
        );
        let n = rt
            .evaluate("document.getElementById('f').elements.length")
            .unwrap();
        assert_eq!(n.as_f64().unwrap() as i64, 3);
        let is_form = rt
            .evaluate("document.getElementById('f') instanceof HTMLFormElement")
            .unwrap();
        assert_eq!(is_form, serde_json::json!(true));
    }

    /// Regression for #105: `Element.prepend` must actually insert at the
    /// start, not silently no-op.
    #[test]
    fn element_prepend_inserts_at_start() {
        let mut rt = setup_runtime(r#"<div id="c"><span>existing</span></div>"#);
        rt.evaluate(
            r"
            const c = document.getElementById('c');
            const n = document.createElement('span');
            n.id = 'first';
            c.prepend(n);
            ",
        )
        .unwrap();
        let first_id = rt
            .evaluate("document.getElementById('c').firstChild.id")
            .unwrap();
        assert_eq!(first_id, serde_json::json!("first"));
        let count = rt
            .evaluate("document.getElementById('c').childNodes.length")
            .unwrap();
        assert_eq!(count.as_f64().unwrap() as i64, 2);
    }

    /// Regression for #105: `isEqualNode` compares structure, not identity.
    /// Framework diff algorithms rely on this.
    #[test]
    fn is_equal_node_does_structural_compare() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r"
                const a = document.createElement('div'); a.setAttribute('class', 'x'); a.innerHTML = '<span>hi</span>';
                const b = document.createElement('div'); b.setAttribute('class', 'x'); b.innerHTML = '<span>hi</span>';
                const c = document.createElement('div'); c.innerHTML = '<span>bye</span>';
                return [a.isEqualNode(b), a.isEqualNode(c), a.isSameNode(b)];
                ",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!([true, false, false]));
    }

    /// Regression for the long-standing insert_before arg-order bug noted
    /// in CLAUDE.md: bootstrap.js was passing (parent, new, ref) but `_dom`
    /// forwards only two args, silently dropping `ref`. With the fix,
    /// `insertBefore` actually inserts.
    #[test]
    fn insert_before_inserts_node_at_correct_position() {
        let mut rt =
            setup_runtime(r#"<div id="p"><span id="b">b</span><span id="c">c</span></div>"#);
        let order = rt
            .evaluate(
                r"
                const p = document.getElementById('p');
                const a = document.createElement('span');
                a.id = 'a';
                p.insertBefore(a, document.getElementById('b'));
                return Array.from(p.children).map(e => e.id).join(',');
                ",
            )
            .unwrap();
        assert_eq!(order, serde_json::json!("a,b,c"));
    }

    #[test]
    fn test_button_click_dispatches_listener() {
        let mut rt = setup_runtime(r#"<button id="go">Go</button>"#);
        let result = rt
            .evaluate(
                r"
            const button = document.getElementById('go');
            button.addEventListener('click', () => { button.dataset.clicked = 'yes'; });
            button.click();
            return button.dataset.clicked;
        ",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!("yes"));
    }

    #[test]
    fn test_dispatch_mouse_event_runs_listener() {
        let mut rt = setup_runtime(r#"<button id="go">Go</button>"#);
        let result = rt
            .evaluate(
                r"
            const button = document.getElementById('go');
            let count = 0;
            button.addEventListener('click', () => { count += 1; });
            button.dispatchEvent(new MouseEvent('click', { bubbles: true }));
            return count;
        ",
            )
            .unwrap();
        assert_eq!(result.as_f64().unwrap() as i64, 1);
    }

    #[test]
    fn test_submit_button_click_handler_can_prevent_default_and_navigate() {
        let mut rt =
            setup_runtime(r#"<form><button type="submit" id="submit">Submit</button></form>"#);
        let href = rt
            .evaluate(
                r"
            const form = document.querySelector('form');
            form.addEventListener('submit', (event) => {
                event.preventDefault();
                location.href = '/submitted';
            });
            document.getElementById('submit').click();
            return location.href;
        ",
            )
            .unwrap();
        assert_eq!(href, serde_json::json!("http://example.com/submitted"));
        assert_eq!(
            rt.take_pending_navigation(),
            Some((
                "http://example.com/submitted".to_string(),
                "GET".to_string(),
                String::new()
            ))
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_no_args() {
        let mut rt = setup_runtime("<html><head><title>Test</title></head><body></body></html>");
        let result = rt
            .call_function_on("() => document.title", None, &[], true)
            .await
            .unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("Test"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_with_args() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let args = vec![
            serde_json::json!({"value": 10}),
            serde_json::json!({"value": 20}),
        ];
        let result = rt
            .call_function_on("(a, b) => a + b", None, &args, true)
            .await
            .unwrap();
        assert_eq!(result.value.unwrap().as_f64().unwrap() as i64, 30);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_with_string_args() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let args = vec![
            serde_json::json!({"value": "hello"}),
            serde_json::json!({"value": " world"}),
        ];
        let result = rt
            .call_function_on("(a, b) => a + b", None, &args, true)
            .await
            .unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("hello world"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_with_object_args() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let args = vec![serde_json::json!({"value": {"name": "test", "count": 5}})];
        let result = rt
            .call_function_on("(obj) => obj.name + ':' + obj.count", None, &args, true)
            .await
            .unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!("test:5"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_return_object() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on("() => ({a: 1, b: 2})", None, &[], true)
            .await
            .unwrap();
        assert_eq!(result.value.unwrap(), serde_json::json!({"a": 1, "b": 2}));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_object_ref_preserves_methods() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on(
                "() => ({ items: [1,2,3], getLen: function() { return this.items.length; } })",
                None,
                &[],
                false,
            )
            .await
            .unwrap();
        let oid = result.object_id.unwrap();

        let result2 = rt
            .call_function_on(
                "function() { return this.getLen(); }",
                Some(&oid),
                &[],
                true,
            )
            .await
            .unwrap();
        assert_eq!(result2.value.unwrap().as_f64().unwrap() as i64, 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_detects_node() {
        let mut rt = setup_runtime("<html><body><h1>Hello</h1></body></html>");
        let result = rt
            .evaluate_for_cdp("document.querySelector('h1')", false, false)
            .await
            .unwrap();
        assert_eq!(result.subtype.as_deref(), Some("node"));
        assert_eq!(result.js_type, "object");
        assert!(result.object_id.is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_detects_document() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate_for_cdp("document", false, false).await.unwrap();
        assert_eq!(result.subtype.as_deref(), Some("node"));
        assert_eq!(result.class_name, "HTMLDocument");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_awaits_resolved_promise() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate_for_cdp("Promise.resolve(42)", true, true)
            .await
            .unwrap();
        assert_eq!(result.value.unwrap().as_f64().unwrap() as i64, 42);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_awaits_timer_promise() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate_for_cdp(
                "new Promise(resolve => setTimeout(() => resolve('done'), 1))",
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(result.value.unwrap().as_str().unwrap(), "done");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_can_await_beyond_legacy_five_second_cap() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let started = std::time::Instant::now();
        let result = rt
            .evaluate_for_cdp_with_timeout(
                "new Promise(resolve => setTimeout(() => resolve('after-five'), 5100))",
                true,
                true,
                6000,
            )
            .await
            .unwrap();
        assert_eq!(result.value.unwrap().as_str(), Some("after-five"));
        assert!(
            started.elapsed() >= std::time::Duration::from_secs(5),
            "long promise resolved before its timer deadline"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_for_cdp_reports_unsettled_promise_timeout() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let error = rt
            .call_function_on_for_cdp_with_timeout(
                "() => new Promise(() => {})",
                None,
                &[],
                true,
                true,
                25,
            )
            .await
            .unwrap_err();
        assert!(
            error.contains("did not settle within 25ms"),
            "unexpected timeout error: {error}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_awaits_async_function() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate_for_cdp("(async () => 'async-ok')()", true, true)
            .await
            .unwrap();
        assert_eq!(result.value.unwrap().as_str().unwrap(), "async-ok");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_evaluate_for_cdp_reports_promise_rejection() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let err = rt
            .evaluate_for_cdp("Promise.reject(new Error('boom'))", true, true)
            .await
            .unwrap_err();
        assert!(err.contains("boom"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_call_function_on_dom_interaction() {
        let mut rt = setup_runtime(r#"<div id="items"><span>A</span><span>B</span></div>"#);
        let args = vec![serde_json::json!({"value": "span"})];
        let result = rt
            .call_function_on(
                "(sel) => document.querySelectorAll(sel).length",
                None,
                &args,
                true,
            )
            .await
            .unwrap();
        assert_eq!(result.value.unwrap().as_f64().unwrap() as i64, 2);
    }

    #[test]
    fn test_sequential_runtime_swap() {
        let mut rt1 = setup_runtime("<html><body><h1>Page1</h1></body></html>");
        let title1 = rt1
            .evaluate("document.querySelector('h1').textContent")
            .unwrap();
        assert_eq!(title1, serde_json::json!("Page1"));

        let dom1 = rt1.take_dom();
        drop(rt1);

        let mut rt2 = setup_runtime("<html><body><h1>Page2</h1></body></html>");
        let title2 = rt2
            .evaluate("document.querySelector('h1').textContent")
            .unwrap();
        assert_eq!(title2, serde_json::json!("Page2"));
        drop(rt2);

        if let Some(dom) = dom1 {
            let mut rt1b = JsRuntime::new();
            rt1b.set_dom(dom);
            rt1b.set_url("http://example.com");
            rt1b.set_title("Page1");
            rt1b.run_page_init();
            let title1b = rt1b
                .evaluate("document.querySelector('h1').textContent")
                .unwrap();
            assert_eq!(title1b, serde_json::json!("Page1"));
        }
    }

    // Issue #324: React/Preact/Vue install a value tracker by redefining `value`
    // on the element instance so they can tell a real edit from their own
    // controlled write. __tinybrowser_setFieldValue must write through the prototype
    // setter, leaving that per-instance tracker stale, so the following input
    // event reads as a genuine change and onChange fires. A plain assignment
    // keeps the tracker in sync and suppresses onChange.
    #[test]
    fn set_field_value_bypasses_instance_value_wrapper() {
        let mut rt = setup_runtime(r#"<input id="i">"#);
        let result = rt
            .evaluate(
                r"
                (function(){
                    var el = document.getElementById('i');
                    var d = Object.getOwnPropertyDescriptor(el.constructor.prototype, 'value');
                    var set = d.set, get = d.get, tracked = '' + el.value;
                    Object.defineProperty(el, 'value', {
                        configurable: true,
                        get: function(){ return get.call(this); },
                        set: function(v){ tracked = '' + v; set.call(this, v); },
                    });
                    el.value = 'wrapped';
                    var afterDirect = { value: el.value, tracked: tracked };
                    globalThis.__tinybrowser_setFieldValue(el, 'value', 'native');
                    var afterHelper = { value: el.value, tracked: tracked };
                    return JSON.stringify({ afterDirect: afterDirect, afterHelper: afterHelper });
                })()
                ",
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
        // Direct assignment keeps tracker == value (the change that suppresses onChange).
        assert_eq!(parsed["afterDirect"]["value"], "wrapped");
        assert_eq!(parsed["afterDirect"]["tracked"], "wrapped");
        // The helper updates the value but leaves the tracker stale, so onChange fires.
        assert_eq!(parsed["afterHelper"]["value"], "native");
        assert_eq!(parsed["afterHelper"]["tracked"], "wrapped");
    }

    // Issue #324: React feature-detects the modern input-event path with
    // `('oninput' in document)`. If the GlobalEventHandlers on* attributes are
    // only on window (not Document/Element), that check fails and React falls
    // back to a legacy change-detection path, so controlled-input onChange never
    // fires. These must be present on document and Element.prototype too.
    #[test]
    fn global_event_handlers_present_on_document_and_element() {
        let mut rt = setup_runtime("<div></div>");
        let result = rt
            .evaluate(
                r"JSON.stringify({
                    docInput: ('oninput' in document),
                    docChange: ('onchange' in document),
                    docClick: ('onclick' in document),
                    elProtoInput: ('oninput' in Element.prototype),
                    winInput: ('oninput' in window)
                })",
            )
            .unwrap();
        let p: serde_json::Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
        assert_eq!(p["docInput"], true);
        assert_eq!(p["docChange"], true);
        assert_eq!(p["docClick"], true);
        assert_eq!(p["elProtoInput"], true);
        assert_eq!(p["winInput"], true);
    }

    #[test]
    fn shallow_element_clone_preserves_interface_attributes_and_isolation() {
        let mut rt = setup_runtime(
            r#"<section id="src" class="source" data-token="original"><span>child</span></section>"#,
        );
        let result = rt
            .evaluate(
                r"
                const source = document.getElementById('src');
                const clone = source.cloneNode(false);
                clone.className = 'clone';
                source.setAttribute('data-token', 'changed');
                return [
                    clone instanceof Node,
                    clone instanceof Element,
                    clone instanceof HTMLElement,
                    typeof clone.outerHTML,
                    typeof clone.querySelectorAll,
                    clone.tagName,
                    clone.id,
                    clone.className,
                    clone.getAttribute('data-token'),
                    clone.childNodes.length,
                    clone.ownerDocument === document,
                    clone.parentNode === null,
                    clone !== source,
                    source.className,
                    source.getAttribute('data-token'),
                    source.childNodes.length,
                ];
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true, true, true, "string", "function", "SECTION", "src", "clone", "original", 0,
                true, true, true, "source", "changed", 1
            ])
        );
    }

    #[test]
    fn deep_document_element_clone_stays_an_independent_html_element() {
        let mut rt = setup_runtime(
            r#"<html lang="en" data-root="original"><head><title>Clone</title></head><body><main id="app" data-state="source"><p class="item">original text</p></main></body></html>"#,
        );
        let result = rt
            .evaluate(
                r"
                const source = document.documentElement;
                const clone = source.cloneNode(true);
                const cloneItem = clone.querySelector('.item');
                const sourceItem = source.querySelector('.item');
                cloneItem.textContent = 'clone text';
                source.querySelector('#app').setAttribute('data-state', 'changed');
                clone.setAttribute('lang', 'fr');
                return [
                    clone instanceof Element,
                    clone instanceof HTMLElement,
                    clone.tagName,
                    typeof clone.outerHTML,
                    typeof clone.querySelectorAll,
                    clone.querySelectorAll('head, body, main, p').length,
                    clone.ownerDocument === document,
                    clone.parentNode === null,
                    clone !== source,
                    clone.querySelector('body') !== document.body,
                    clone.getAttribute('data-root'),
                    clone.getAttribute('lang'),
                    source.getAttribute('lang'),
                    cloneItem.textContent,
                    sourceItem.textContent,
                    clone.querySelector('#app').getAttribute('data-state'),
                    source.querySelector('#app').getAttribute('data-state'),
                ];
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true,
                true,
                "HTML",
                "string",
                "function",
                4,
                true,
                true,
                true,
                true,
                "original",
                "fr",
                "en",
                "clone text",
                "original text",
                "source",
                "changed"
            ])
        );
    }

    #[test]
    fn test_evaluate_multistatement() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt.evaluate("var x = 5; var y = 10; return x + y;").unwrap();
        assert_eq!(result.as_f64().unwrap() as i64, 15);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_object_ref_as_argument() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let obj = rt
            .call_function_on("() => ({ x: 42 })", None, &[], false)
            .await
            .unwrap();
        let oid = obj.object_id.unwrap();

        let args = vec![serde_json::json!({"objectId": oid})];
        let result = rt
            .call_function_on("(obj) => obj.x * 2", None, &args, true)
            .await
            .unwrap();
        assert_eq!(result.value.unwrap().as_f64().unwrap() as i64, 84);
    }

    fn setup_runtime_with_cookies(
        html: &str,
    ) -> (JsRuntime, std::sync::Arc<tinybrowser_net::CookieJar>) {
        let dom = tinybrowser_dom::parse_html(html);
        let jar = std::sync::Arc::new(tinybrowser_net::CookieJar::new());
        let mut rt = JsRuntime::new();
        rt.set_dom(dom);
        rt.set_url("http://example.com/test");
        rt.set_title("Test Page");
        rt.set_cookie_jar(jar.clone());
        rt.run_page_init();
        (rt, jar)
    }

    #[test]
    fn test_document_cookie_reads_http_cookies() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let url = url::Url::parse("http://example.com/test").unwrap();
        jar.set_cookie("session=abc123; Path=/", &url);
        jar.set_cookie("theme=dark; Path=/", &url);
        let result = rt.evaluate("document.cookie").unwrap();
        let cookie_str = result.as_str().unwrap();
        assert!(
            cookie_str.contains("session=abc123"),
            "expected session cookie, got: {cookie_str}"
        );
        assert!(
            cookie_str.contains("theme=dark"),
            "expected theme cookie, got: {cookie_str}"
        );
    }

    #[test]
    fn test_document_cookie_excludes_httponly() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let url = url::Url::parse("http://example.com/test").unwrap();
        jar.set_cookie("visible=yes; Path=/", &url);
        jar.set_cookie("secret=token; Path=/; HttpOnly", &url);
        let result = rt.evaluate("document.cookie").unwrap();
        let cookie_str = result.as_str().unwrap();
        assert!(
            cookie_str.contains("visible=yes"),
            "expected visible cookie, got: {cookie_str}"
        );
        assert!(
            !cookie_str.contains("secret"),
            "httpOnly cookie should not be visible to JS, got: {cookie_str}"
        );
    }

    #[test]
    fn test_document_cookie_delete_via_max_age() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let url = url::Url::parse("http://example.com/test").unwrap();
        rt.evaluate("document.cookie = 'temp=val; Path=/'").unwrap();
        assert!(rt
            .evaluate("document.cookie")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("temp=val"));
        rt.evaluate("document.cookie = 'temp=; Max-Age=0'").unwrap();
        let result = rt.evaluate("document.cookie").unwrap();
        assert!(
            !result.as_str().unwrap().contains("temp="),
            "cookie should be deleted, got: {result}"
        );
        assert!(!jar.get_cookie_header(&url).contains("temp="));
    }

    #[test]
    fn test_document_cookie_js_and_http_merge() {
        let (mut rt, jar) = setup_runtime_with_cookies("<html><body></body></html>");
        let url = url::Url::parse("http://example.com/test").unwrap();
        jar.set_cookie("server_sid=xyz; Path=/", &url);
        rt.evaluate("document.cookie = 'client_pref=light'")
            .unwrap();
        let result = rt.evaluate("document.cookie").unwrap();
        let cookie_str = result.as_str().unwrap();
        assert!(
            cookie_str.contains("server_sid=xyz"),
            "expected server cookie, got: {cookie_str}"
        );
        assert!(
            cookie_str.contains("client_pref=light"),
            "expected client cookie, got: {cookie_str}"
        );
    }

    #[test]
    fn test_document_write_appends_to_body() {
        let mut rt = setup_runtime("<html><body><p>Existing</p></body></html>");
        rt.evaluate("document.write('<div>Added</div>')").unwrap();
        let html = rt.evaluate("document.body.innerHTML").unwrap();
        let body = html.as_str().unwrap();
        assert!(
            body.contains("Existing"),
            "existing content should remain, got: {body}"
        );
        assert!(
            body.contains("Added"),
            "written content should appear, got: {body}"
        );
    }

    #[test]
    fn test_document_writeln() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.evaluate("document.writeln('Hello')").unwrap();
        let html = rt.evaluate("document.body.innerHTML").unwrap();
        assert!(html.as_str().unwrap().contains("Hello"));
    }

    #[test]
    fn test_document_write_multiple_args() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.evaluate("document.write('Hello', ' ', 'World')")
            .unwrap();
        let text = rt.evaluate("document.body.textContent").unwrap();
        assert_eq!(text.as_str().unwrap().trim(), "Hello World");
    }

    #[test]
    fn test_document_open_clears_body() {
        let mut rt = setup_runtime("<html><body><p>Old content</p></body></html>");
        rt.evaluate("document.open()").unwrap();
        let html = rt.evaluate("document.body.innerHTML").unwrap();
        assert_eq!(html.as_str().unwrap(), "");
    }

    #[test]
    fn test_document_write_html_elements() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.evaluate(r#"document.write('<h1 id="title">Test</h1><p>Para</p>')"#)
            .unwrap();
        let h1 = rt
            .evaluate("document.querySelector('h1').textContent")
            .unwrap();
        assert_eq!(h1.as_str().unwrap(), "Test");
        let p = rt
            .evaluate("document.querySelector('p').textContent")
            .unwrap();
        assert_eq!(p.as_str().unwrap(), "Para");
    }

    #[test]
    fn test_url_relative_resolution() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate("new URL('data.json', 'http://example.com/path/page.html').href")
            .unwrap();
        assert_eq!(
            result.as_str().unwrap(),
            "http://example.com/path/data.json"
        );

        let result = rt
            .evaluate("new URL('/api/data', 'http://example.com/path/page.html').href")
            .unwrap();
        assert_eq!(result.as_str().unwrap(), "http://example.com/api/data");

        let result = rt
            .evaluate("new URL('https://other.com/foo', 'http://example.com/bar').href")
            .unwrap();
        assert_eq!(result.as_str().unwrap(), "https://other.com/foo");

        let result = rt
            .evaluate("new URL('sub/file.js', 'http://example.com/a/b/c.html').href")
            .unwrap();
        assert_eq!(
            result.as_str().unwrap(),
            "http://example.com/a/b/sub/file.js"
        );

        let result = rt
            .evaluate("new URL('api.json', 'http://localhost:8080/dir/index.html').href")
            .unwrap();
        assert_eq!(
            result.as_str().unwrap(),
            "http://localhost:8080/dir/api.json"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_fetch_url_input_decodes_binary_body_base64() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                const originalFetchOp = __tbHost.core.ops.op_fetch_url;
                try {
                    __tbHost.core.ops.op_fetch_url = (url) => {
                        globalThis.__capturedFetchUrl = url;
                        return JSON.stringify({
                            status: 200,
                            headers: { "content-type": "application/wasm" },
                            bodyBase64: "AGFzbQEAAAA=",
                            url,
                        });
                    };
                    const response = await fetch(new URL("/pkg/app_bg.wasm", document.URL));
                    const bytes = Array.from(new Uint8Array(await response.arrayBuffer()));
                    return { url: globalThis.__capturedFetchUrl, bytes };
                } finally {
                    __tbHost.core.ops.op_fetch_url = originalFetchOp;
                }
            }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "url": "http://example.com/pkg/app_bg.wasm",
                "bytes": [0, 97, 115, 109, 1, 0, 0, 0],
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_and_xhr_forward_browser_credentials_modes() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const originalFetchOp = __tbHost.core.ops.op_fetch_url;
                    const calls = [];
                    try {
                        __tbHost.core.ops.op_fetch_url =
                            (url, method, headers, body, origin, mode, credentials) => {
                                calls.push({ url, credentials });
                                return JSON.stringify({
                                    status: 200,
                                    headers: {},
                                    body: "ok",
                                    url,
                                });
                            };

                        await fetch("/default");
                        await fetch("/omit", { credentials: "omit" });
                        const request = new Request("/included", { credentials: "include" });
                        await fetch(request);
                        await fetch(request.clone());
                        await fetch(request, { credentials: "same-origin" });

                        const sendXhr = (path, withCredentials) => new Promise((resolve, reject) => {
                            const xhr = new XMLHttpRequest();
                            xhr.open("GET", path);
                            xhr.withCredentials = withCredentials;
                            xhr.onload = resolve;
                            xhr.onerror = reject;
                            xhr.send();
                        });
                        await sendXhr("/xhr-default", false);
                        await sendXhr("/xhr-credentialed", true);

                        let invalidFetchRejected = false;
                        try {
                            await fetch("/bad", { credentials: "invalid" });
                        } catch (error) {
                            invalidFetchRejected = error instanceof TypeError;
                        }

                        return { calls, invalidFetchRejected };
                    } finally {
                        __tbHost.core.ops.op_fetch_url = originalFetchOp;
                    }
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "calls": [
                    { "url": "http://example.com/default", "credentials": "same-origin" },
                    { "url": "http://example.com/omit", "credentials": "omit" },
                    { "url": "http://example.com/included", "credentials": "include" },
                    { "url": "http://example.com/included", "credentials": "include" },
                    { "url": "http://example.com/included", "credentials": "same-origin" },
                    { "url": "http://example.com/xhr-default", "credentials": "same-origin" },
                    { "url": "http://example.com/xhr-credentialed", "credentials": "include" },
                ],
                "invalidFetchRejected": true,
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dynamic_linked_stylesheet_enters_the_live_dom_with_imports_rebased() {
        let mut rt =
            setup_runtime("<html><head></head><body><div class=\"card\"></div></body></html>");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const originalFetchOp = __tbHost.core.ops.op_fetch_url;
                    try {
                        __tbHost.core.ops.op_fetch_url = (url) => JSON.stringify({
                            status: 200,
                            headers: { "content-type": "text/css" },
                            body: url.endsWith("/assets/route.css")
                                ? '@import "./theme/base.css"; .card { display:grid; background-image:url("../img/card.png") }'
                                : '.card { color:red; background-image:url("./grain.png") }',
                            url,
                        });
                        const link = document.createElement("link");
                        link.setAttribute("rel", "stylesheet");
                        link.setAttribute("href", "/assets/route.css");
                        const loaded = new Promise(resolve => {
                            link.onload = () => resolve();
                        });
                        document.head.appendChild(link);
                        await loaded;
                        const style = document.querySelector("style[data-tinybrowser-linked]");
                        const css = style.textContent;
                        const afterLink = link.nextSibling === style;
                        const list = document.styleSheets;
                        const sheet = link.sheet;
                        const rules = sheet.cssRules;
                        const cssom = {
                            listed: list.length === 1 && list[0] === sheet,
                            stable: link.sheet === sheet && sheet.cssRules === rules,
                            owner: sheet.ownerNode === link,
                            href: sheet.href,
                            selectors: Array.from(rules, rule => rule.selectorText),
                        };
                        link.remove();
                        return {
                            afterLink,
                            importedBeforeRoute:
                                css.indexOf("color:red") < css.indexOf("display:grid"),
                            importedUrl:
                                css.includes("http://example.com/assets/theme/grain.png"),
                            routeUrl:
                                css.includes("http://example.com/img/card.png"),
                            removedWithLink:
                                !document.querySelector("style[data-tinybrowser-linked]"),
                            cssom,
                            detachedCssom: sheet.ownerNode === null
                                && link.sheet === null
                                && list.length === 0,
                        };
                    } finally {
                        __tbHost.core.ops.op_fetch_url = originalFetchOp;
                    }
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "afterLink": true,
                "importedBeforeRoute": true,
                "importedUrl": true,
                "routeUrl": true,
                "removedWithLink": true,
                "cssom": {
                    "listed": true,
                    "stable": true,
                    "owner": true,
                    "href": "http://example.com/assets/route.css",
                    "selectors": [".card", ".card"],
                },
                "detachedCssom": true,
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unsuccessful_dynamic_script_response_fires_error_without_evaluating_body() {
        let mut rt = setup_runtime("<html><head></head><body></body></html>");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const originalFetchOp = __tbHost.core.ops.op_fetch_url;
                    try {
                        __tbHost.core.ops.op_fetch_url = (url) => JSON.stringify({
                            status: 401,
                            headers: { "content-type": "application/json" },
                            body: "globalThis.__executedFailedScript = true",
                            url,
                        });
                        const script = document.createElement("script");
                        script.src = "/unauthorized.js";
                        const outcome = await new Promise(resolve => {
                            script.onload = () => resolve("load");
                            script.onerror = () => resolve("error");
                            document.head.appendChild(script);
                        });
                        return {
                            outcome,
                            executed: globalThis.__executedFailedScript === true,
                        };
                    } finally {
                        __tbHost.core.ops.op_fetch_url = originalFetchOp;
                    }
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "outcome": "error",
                "executed": false,
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dynamic_classic_scripts_are_async_by_default_but_honor_async_false_order() {
        let mut rt = setup_runtime("<html><head></head><body></body></html>");
        let result = rt
            .call_function_on_for_cdp(
                r#"async () => {
                    const originalFetchOp = __tbHost.core.ops.op_fetch_url;
                    const runPair = async (explicitlyInOrder) => {
                        globalThis.__dynamicOrder = [];
                        __tbHost.core.ops.op_fetch_url = (url) => new Promise(resolve => {
                            const slow = url.includes("slow");
                            setTimeout(() => resolve(JSON.stringify({
                                status: 200,
                                headers: {"content-type": "text/javascript"},
                                body: `globalThis.__dynamicOrder.push("${slow ? "slow" : "fast"}")`,
                                url,
                            })), slow ? 30 : 1);
                        });
                        const load = name => new Promise(resolve => {
                            const script = document.createElement("script");
                            if (explicitlyInOrder) script.async = false;
                            script.src = `/${name}.js`;
                            script.onload = resolve;
                            document.head.appendChild(script);
                        });
                        await Promise.all([load("slow"), load("fast")]);
                        return globalThis.__dynamicOrder.slice();
                    };
                    try {
                        const asyncOrder = await runPair(false);
                        const inOrder = await runPair(true);
                        return {
                            asyncOrder,
                            inOrder,
                            pending: globalThis.__tinybrowser_hasPendingDynamicScripts(),
                        };
                    } finally {
                        __tbHost.core.ops.op_fetch_url = originalFetchOp;
                    }
                }"#,
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!({
                "asyncOrder": ["fast", "slow"],
                "inOrder": ["slow", "fast"],
                "pending": false,
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_response_array_buffer_preserves_typed_array_view() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .call_function_on_for_cdp(
                r"async () => {
                const bytes = new Uint8Array([9, 0, 97, 115, 109, 1, 8]);
                const response = new Response(bytes.subarray(1, 6));
                return Array.from(new Uint8Array(await response.arrayBuffer()));
            }",
                None,
                &[],
                true,
                true,
            )
            .await
            .unwrap();

        assert_eq!(
            result.value.unwrap(),
            serde_json::json!([0, 97, 115, 109, 1])
        );
    }

    #[test]
    fn test_text_decoder_respects_typed_array_view() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate("new TextDecoder().decode(new Uint8Array([65, 66, 67]).subarray(1, 2))")
            .unwrap();
        assert_eq!(result.as_str().unwrap(), "B");
    }

    #[test]
    fn test_xml_serializer_doctype() {
        let mut rt = setup_runtime("<!DOCTYPE html><html><body></body></html>");
        let result = rt
            .evaluate("new XMLSerializer().serializeToString(document.doctype)")
            .unwrap();
        assert_eq!(result.as_str().unwrap(), "<!DOCTYPE html>");
    }

    #[test]
    fn test_xml_serializer_element() {
        let mut rt = setup_runtime(r#"<html><body><div id="x">Hello</div></body></html>"#);
        let result = rt
            .evaluate("new XMLSerializer().serializeToString(document.getElementById('x'))")
            .unwrap();
        let html = result.as_str().unwrap();
        assert!(html.contains("<div"));
        assert!(html.contains("Hello"));
    }

    #[test]
    fn test_create_event_custom_event_has_init_method() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let kind = rt
            .evaluate("typeof document.createEvent('CustomEvent').initCustomEvent")
            .unwrap();
        assert_eq!(kind, serde_json::json!("function"));
    }

    #[test]
    fn test_init_custom_event_sets_fields() {
        let mut rt = setup_runtime("<html><body></body></html>");
        rt.execute_script(
            "test",
            r"
            globalThis.__e = document.createEvent('CustomEvent');
            globalThis.__e.initCustomEvent('myevent', true, false, {hello: 'world'});
        ",
        )
        .unwrap();
        let t = rt.evaluate("globalThis.__e.type").unwrap();
        assert_eq!(t, serde_json::json!("myevent"));
        let b = rt.evaluate("globalThis.__e.bubbles").unwrap();
        assert_eq!(b, serde_json::json!(true));
        let c = rt.evaluate("globalThis.__e.cancelable").unwrap();
        assert_eq!(c, serde_json::json!(false));
        let d = rt.evaluate("globalThis.__e.detail.hello").unwrap();
        assert_eq!(d, serde_json::json!("world"));
    }

    #[test]
    fn test_create_event_returns_correct_class() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let cust = rt
            .evaluate("document.createEvent('CustomEvent') instanceof CustomEvent")
            .unwrap();
        assert_eq!(cust, serde_json::json!(true));
        let mouse = rt
            .evaluate("document.createEvent('MouseEvent') instanceof MouseEvent")
            .unwrap();
        assert_eq!(mouse, serde_json::json!(true));
        let mouses = rt
            .evaluate("document.createEvent('MouseEvents') instanceof MouseEvent")
            .unwrap();
        assert_eq!(mouses, serde_json::json!(true));
        let kb = rt
            .evaluate("document.createEvent('KeyboardEvent') instanceof KeyboardEvent")
            .unwrap();
        assert_eq!(kb, serde_json::json!(true));
    }

    #[test]
    fn cssstyledeclaration_is_a_usable_global_interface() {
        // CSSStyleDeclaration was pre-declared non-enumerable but never assigned
        // a value (the only WebIDL interface missing its globalThis.X = X line),
        // so it was `undefined` while `'CSSStyleDeclaration' in window` was true,
        // and `el.style instanceof CSSStyleDeclaration` threw. It must be a real
        // constructor, non-enumerable like a browser, and the type of .style.
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate("(function(){var d=Object.getOwnPropertyDescriptor(window,'CSSStyleDeclaration');return (typeof window.CSSStyleDeclaration)+'|'+(document.body.style instanceof CSSStyleDeclaration)+'|'+(d?d.enumerable:'missing');})()")
            .unwrap();
        assert_eq!(v, serde_json::json!("function|true|false"));
    }

    #[test]
    fn test_create_event_unknown_type_returns_event() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let kind = rt
            .evaluate("document.createEvent('NotARealType') instanceof Event")
            .unwrap();
        assert_eq!(kind, serde_json::json!(true));
    }

    #[test]
    fn event_constructor_matches_webidl_conformance() {
        // new Event()/new CustomEvent() must throw (type is a required arg),
        // the type argument must be coerced to a string, CustomEvent.detail must
        // default to null (not undefined), createEvent must still build a
        // type-"" event, and an explicit detail must be preserved.
        let mut rt = setup_runtime("<html><body></body></html>");
        let v = rt
            .evaluate(
                "(function(){\
                 var out=[];\
                 try{new Event();out.push('no-throw')}catch(e){out.push(e.name)}\
                 try{new CustomEvent();out.push('no-throw')}catch(e){out.push(e.name)}\
                 out.push(new Event(123).type+':'+typeof new Event(123).type);\
                 out.push(String(new CustomEvent('x').detail));\
                 out.push(String(new CustomEvent('x',{detail:7}).detail));\
                 out.push(new Event('click').type);\
                 out.push(JSON.stringify(document.createEvent('Event').type));\
                 return out.join('|');\
                 })()",
            )
            .unwrap();
        assert_eq!(
            v,
            serde_json::json!("TypeError|TypeError|123:string|null|7|click|\"\"")
        );
    }

    #[test]
    fn test_promise_rejection_event_requires_promise() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r"(() => {
                    const promise = Promise.resolve(1);
                    const event = new PromiseRejectionEvent('unhandledrejection', {
                        promise,
                        reason: 'failed'
                    });
                    let missingPromiseThrows = false;
                    try {
                        new PromiseRejectionEvent('unhandledrejection');
                    } catch (error) {
                        missingPromiseThrows = error instanceof TypeError;
                    }
                    return [
                        event instanceof Event,
                        event.promise === promise,
                        event.reason === 'failed',
                        missingPromiseThrows
                    ];
                })()",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!([true, true, true, true]));
    }

    #[test]
    fn test_create_event_rejects_promise_rejection_event() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r"(() => {
                    try {
                        document.createEvent('PromiseRejectionEvent');
                        return null;
                    } catch (error) {
                        return [error.name, error instanceof DOMException];
                    }
                })()",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["NotSupportedError", true]));
    }

    #[test]
    fn test_storage_event_constructor_and_legacy_factory() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let result = rt
            .evaluate(
                r"(() => {
                    const event = new StorageEvent('storage', {
                        key: 'theme',
                        oldValue: 'light',
                        newValue: 'dark',
                        url: 'https://example.test/'
                    });
                    const legacy = document.createEvent('StorageEvent');
                    legacy.initStorageEvent(
                        'storage', false, false, 'count', '1', '2',
                        'https://example.test/', null
                    );
                    return [
                        event instanceof Event,
                        event.key,
                        event.oldValue,
                        event.newValue,
                        event.url,
                        legacy instanceof StorageEvent,
                        legacy.key,
                        legacy.newValue
                    ];
                })()",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true,
                "theme",
                "light",
                "dark",
                "https://example.test/",
                true,
                "count",
                "2"
            ])
        );
    }

    #[test]
    fn test_page_content_puppeteer_pattern() {
        let mut rt =
            setup_runtime("<!DOCTYPE html><html><head></head><body><p>Test</p></body></html>");
        let result = rt.evaluate(
            "(function() { let retVal = ''; if (document.doctype) retVal = new XMLSerializer().serializeToString(document.doctype); if (document.documentElement) retVal += document.documentElement.outerHTML; return retVal; })()"
        ).unwrap();
        let html = result.as_str().unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<html>"));
        assert!(html.contains("<p>Test</p>"));
    }

    #[test]
    fn test_element_from_point_is_function() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let kind = rt.evaluate("typeof document.elementFromPoint").unwrap();
        assert_eq!(kind, serde_json::json!("function"));
        let kind2 = rt.evaluate("typeof document.elementsFromPoint").unwrap();
        assert_eq!(kind2, serde_json::json!("function"));
    }

    #[test]
    fn test_element_from_point_in_viewport_returns_body() {
        let mut rt = setup_runtime("<html><body><h1>Hi</h1></body></html>");
        let tag = rt
            .evaluate("document.elementFromPoint(10, 10)?.tagName")
            .unwrap();
        assert_eq!(tag, serde_json::json!("BODY"));
    }

    #[test]
    fn test_element_from_point_out_of_viewport_returns_null() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let neg_x = rt.evaluate("document.elementFromPoint(-1, 10)").unwrap();
        assert_eq!(neg_x, serde_json::Value::Null);
        let neg_y = rt.evaluate("document.elementFromPoint(10, -1)").unwrap();
        assert_eq!(neg_y, serde_json::Value::Null);
        let huge = rt
            .evaluate("document.elementFromPoint(99999, 99999)")
            .unwrap();
        assert_eq!(huge, serde_json::Value::Null);
    }

    #[test]
    fn test_elements_from_point_returns_array() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let len_in = rt
            .evaluate("document.elementsFromPoint(10, 10).length")
            .unwrap();
        assert_eq!(len_in.as_f64().unwrap() as i64, 1);
        let len_out = rt
            .evaluate("document.elementsFromPoint(-1, -1).length")
            .unwrap();
        assert_eq!(len_out.as_f64().unwrap() as i64, 0);
    }

    #[test]
    fn test_element_from_point_non_numeric_returns_null() {
        let mut rt = setup_runtime("<html><body></body></html>");
        let nan = rt.evaluate("document.elementFromPoint(NaN, 10)").unwrap();
        assert_eq!(nan, serde_json::Value::Null);
        let inf = rt
            .evaluate("document.elementFromPoint(Infinity, 10)")
            .unwrap();
        assert_eq!(inf, serde_json::Value::Null);
    }

    fn spawn_one_response_server(status: &str, body: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.to_string();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 {}\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status,
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        format!("http://{address}")
    }

    #[derive(Clone, Copy)]
    enum ModuleGraphFixture {
        CookieProtected,
        RedirectedChild,
    }

    fn spawn_module_graph_server(
        fixture: ModuleGraphFixture,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (requests_tx, requests_rx) = std::sync::mpsc::channel();
        let request_count = match fixture {
            ModuleGraphFixture::CookieProtected => 2,
            ModuleGraphFixture::RedirectedChild => 3,
        };
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = vec![0u8; 8192];
                let length = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..length]).to_string();
                let lower_request = request.to_ascii_lowercase();
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_ascii_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                requests_tx.send(request.clone()).unwrap();

                let (status, extra_headers, body) = match (fixture, path.as_str()) {
                    (ModuleGraphFixture::CookieProtected, "/entry.js") => (
                        "200 OK",
                        "",
                        "import { value } from './child.js'; \
                         globalThis.__module_graph_value = value;",
                    ),
                    (ModuleGraphFixture::CookieProtected, "/child.js")
                        if lower_request.contains("\r\ncookie: session=ok\r\n")
                            && lower_request
                                .contains("\r\nuser-agent: modulegraphtest/1.0\r\n")
                            && lower_request.contains("\r\nx-module-test: shared\r\n") =>
                    {
                        ("200 OK", "", "export const value = 'cookie-child';")
                    }
                    (ModuleGraphFixture::CookieProtected, "/child.js") => (
                        "401 Unauthorized",
                        "",
                        "throw new Error('page request context missing');",
                    ),
                    (ModuleGraphFixture::RedirectedChild, "/entry.js") => (
                        "200 OK",
                        "",
                        "import { value } from './redirect.js'; \
                         globalThis.__module_graph_value = value;",
                    ),
                    (ModuleGraphFixture::RedirectedChild, "/redirect.js") => {
                        ("302 Found", "Location: /child.js\r\n", "")
                    }
                    (ModuleGraphFixture::RedirectedChild, "/child.js") => {
                        ("200 OK", "", "export const value = 'redirect-child';")
                    }
                    _ => ("404 Not Found", "", "not found"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\n\
                     Content-Type: application/javascript\r\n\
                     {extra_headers}Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{address}"), requests_rx)
    }

    fn spawn_import_map_server() -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (requests_tx, requests_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 2048];
                let length = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..length]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_ascii_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                requests_tx.send(path.clone()).unwrap();
                let (status, body) = match path.as_str() {
                    "/vendor/pkg/feature.js" => ("200 OK", "export const value = 'prefix-static';"),
                    "/vendor/dynamic.js" => ("200 OK", "export const value = 'exact-dynamic';"),
                    _ => ("404 Not Found", "not found"),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\n\
                     Content-Type: application/javascript\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (format!("http://{address}"), requests_rx)
    }

    fn spawn_root_module_import_map_server() -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (requests_tx, requests_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let length = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..length]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_ascii_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            requests_tx.send(path.clone()).unwrap();
            let body = if path == "/entry.js" {
                "globalThis.__root_module_identity = 'entry';"
            } else {
                "globalThis.__root_module_identity = 'remapped';"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/javascript\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len(),
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        (format!("http://{address}"), requests_rx)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn entry_module_http_failure_is_not_evaluated_as_empty_source() {
        let base = spawn_one_response_server("404 Not Found", "not found");
        let jar = std::sync::Arc::new(tinybrowser_net::CookieJar::new());
        let client = std::sync::Arc::new(tinybrowser_net::HttpClient::with_full_options(
            jar, None, true,
        ));
        let mut rt = JsRuntime::with_base_url(&format!("{base}/"));
        rt.set_http_client(client);

        let error = rt
            .load_module(&format!("{base}/entry.js"), 1_000)
            .await
            .unwrap_err();
        assert!(
            error.contains("HTTP 404"),
            "expected entry fetch status in error, got: {error}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn module_graph_load_times_out() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::Read;
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        });
        let base = format!("http://{address}");
        let client = std::sync::Arc::new(tinybrowser_net::HttpClient::with_full_options(
            std::sync::Arc::new(tinybrowser_net::CookieJar::new()),
            None,
            true,
        ));
        let mut rt = JsRuntime::with_base_url(&format!("{base}/"));
        rt.set_http_client(client);

        let started = std::time::Instant::now();
        let error = rt
            .load_module(&format!("{base}/entry.js"), 80)
            .await
            .unwrap_err();
        assert!(
            error.contains("timed out"),
            "expected graph load timeout, got: {error}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "timeout took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn descendant_module_uses_page_cookie_identity_and_headers() {
        let (base, requests) = spawn_module_graph_server(ModuleGraphFixture::CookieProtected);
        let page_url = url::Url::parse(&format!("{base}/")).unwrap();
        let jar = std::sync::Arc::new(tinybrowser_net::CookieJar::new());
        jar.set_cookie("session=ok; Path=/", &page_url);
        let client = std::sync::Arc::new(tinybrowser_net::HttpClient::with_full_options(
            jar.clone(),
            None,
            true,
        ));
        client.set_user_agent("ModuleGraphTest/1.0").await;
        client
            .set_extra_headers(std::collections::HashMap::from([(
                "x-module-test".to_string(),
                "shared".to_string(),
            )]))
            .await;
        let callback_urls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let callbacks = std::sync::Arc::new(tinybrowser_net::CallbackRegistry::new());
        let callback_urls_capture = callback_urls.clone();
        callbacks.add_request(std::sync::Arc::new(move |request| {
            callback_urls_capture
                .lock()
                .unwrap()
                .push(request.url.path().to_string());
        }));

        let mut rt = JsRuntime::with_base_url(&format!("{base}/"));
        rt.set_cookie_jar(jar);
        rt.set_http_client(client);
        rt.set_callbacks(callbacks);
        rt.load_module(&format!("{base}/entry.js"), 1_000)
            .await
            .unwrap();

        assert_eq!(
            rt.evaluate("globalThis.__module_graph_value").unwrap(),
            serde_json::json!("cookie-child"),
        );
        let requests = (0..2)
            .map(|_| {
                requests
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(requests
            .iter()
            .any(|request| request.starts_with("GET /entry.js ")));
        let child = requests
            .iter()
            .find(|request| request.starts_with("GET /child.js "))
            .expect("descendant request");
        let child_lower = child.to_ascii_lowercase();
        assert!(
            child_lower.contains("\r\ncookie: session=ok\r\n"),
            "{child}"
        );
        assert!(
            child_lower.contains("\r\nuser-agent: modulegraphtest/1.0\r\n"),
            "{child}"
        );
        assert!(
            child_lower.contains("\r\nx-module-test: shared\r\n"),
            "{child}"
        );
        assert_eq!(
            *callback_urls.lock().unwrap(),
            vec!["/entry.js", "/child.js"],
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cross_origin_module_descendant_does_not_gain_module_origin_cookies() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let module_base = format!("http://{address}");
        let document_url = "http://127.0.0.1:1/page";
        let document_origin = "http://127.0.0.1:1";
        let (requests_tx, requests_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};

            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 4096];
                let length = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..length]).to_string();
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_ascii_whitespace().nth(1))
                    .unwrap_or("/");
                let body = match path {
                    "/entry.js" => {
                        "import { value } from './child.js'; globalThis.__cors_value = value;"
                    }
                    "/child.js" => "export const value = 'safe';",
                    _ => "throw new Error('unexpected module path');",
                };
                requests_tx.send(request).unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: application/javascript\r\n\
                     Access-Control-Allow-Origin: {document_origin}\r\n\
                     Cache-Control: public, max-age=3600\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });

        let module_origin = url::Url::parse(&module_base).unwrap();
        let jar = std::sync::Arc::new(tinybrowser_net::CookieJar::new());
        jar.set_cookie("cdn_session=secret; Path=/", &module_origin);
        let client = std::sync::Arc::new(tinybrowser_net::HttpClient::with_full_options(
            jar, None, true,
        ));
        let mut rt = JsRuntime::with_base_url(document_url);
        rt.set_http_client(client);
        rt.load_module(&format!("{module_base}/entry.js"), 1_000)
            .await
            .unwrap();

        assert_eq!(
            rt.evaluate("globalThis.__cors_value").unwrap(),
            serde_json::json!("safe"),
        );
        let requests = (0..2)
            .map(|_| {
                requests_rx
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        for request in &requests {
            let lower = request.to_ascii_lowercase();
            assert!(
                lower.contains("\r\norigin: http://127.0.0.1:1\r\n"),
                "{request}"
            );
            assert!(!lower.contains("\r\ncookie:"), "{request}");
        }
        let child = requests
            .iter()
            .find(|request| request.starts_with("GET /child.js "))
            .expect("child module request")
            .to_ascii_lowercase();
        assert!(
            child.contains(&format!("\r\nreferer: {module_base}/entry.js\r\n")),
            "{child}",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn descendant_module_follows_page_client_redirects() {
        let (base, requests) = spawn_module_graph_server(ModuleGraphFixture::RedirectedChild);
        let jar = std::sync::Arc::new(tinybrowser_net::CookieJar::new());
        let client = std::sync::Arc::new(tinybrowser_net::HttpClient::with_full_options(
            jar, None, true,
        ));
        let mut rt = JsRuntime::with_base_url(&format!("{base}/"));
        rt.set_http_client(client);
        rt.load_module(&format!("{base}/entry.js"), 1_000)
            .await
            .unwrap();

        assert_eq!(
            rt.evaluate("globalThis.__module_graph_value").unwrap(),
            serde_json::json!("redirect-child"),
        );
        let paths = (0..3)
            .map(|_| {
                requests
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                "GET /entry.js HTTP/1.1",
                "GET /redirect.js HTTP/1.1",
                "GET /child.js HTTP/1.1",
            ],
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn import_map_resolves_prefix_static_and_exact_dynamic_imports() {
        let (base, requests) = spawn_import_map_server();
        let jar = std::sync::Arc::new(tinybrowser_net::CookieJar::new());
        let client = std::sync::Arc::new(tinybrowser_net::HttpClient::with_full_options(
            jar, None, true,
        ));
        let mut rt = JsRuntime::with_base_url(&format!("{base}/app/index.html"));
        rt.set_http_client(client);
        rt.add_import_map(
            r#"{
                "imports": {
                    "pkg/": "../vendor/pkg/",
                    "dynamic-pkg": "../vendor/dynamic.js"
                }
            }"#,
            &format!("{base}/config/import-map.json"),
        )
        .unwrap();

        rt.load_inline_module(
            "import { value as prefix } from 'pkg/feature.js'; \
             const dynamic = (await import('dynamic-pkg')).value; \
             globalThis.__import_map_values = [prefix, dynamic];",
            &format!("{base}/app/index.html"),
            1_000,
        )
        .await
        .unwrap();

        assert_eq!(
            rt.evaluate("globalThis.__import_map_values").unwrap(),
            serde_json::json!(["prefix-static", "exact-dynamic"]),
        );
        let paths = (0..2)
            .map(|_| {
                requests
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(
            paths.contains(&"/vendor/pkg/feature.js".to_string()),
            "{paths:?}"
        );
        assert!(
            paths.contains(&"/vendor/dynamic.js".to_string()),
            "{paths:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn import_map_does_not_remap_external_root_module_url() {
        let (base, requests) = spawn_root_module_import_map_server();
        let jar = std::sync::Arc::new(tinybrowser_net::CookieJar::new());
        let client = std::sync::Arc::new(tinybrowser_net::HttpClient::with_full_options(
            jar, None, true,
        ));
        let mut rt = JsRuntime::with_base_url(&format!("{base}/index.html"));
        rt.set_http_client(client);
        rt.add_import_map(
            &format!(r#"{{"imports":{{"{base}/entry.js":"{base}/remapped.js"}}}}"#),
            &format!("{base}/index.html"),
        )
        .unwrap();

        rt.load_module(&format!("{base}/entry.js"), 1_000)
            .await
            .unwrap();
        assert_eq!(
            rt.evaluate("globalThis.__root_module_identity").unwrap(),
            serde_json::json!("entry"),
        );
        assert_eq!(
            requests
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            "/entry.js",
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inline_modules_expose_document_base_as_import_meta_url() {
        let mut rt = JsRuntime::with_base_url("https://example.com/page/index.html");
        rt.load_inline_module(
            "globalThis.__first_inline_url = import.meta.url;",
            "https://example.com/base/",
            1_000,
        )
        .await
        .unwrap();
        rt.load_inline_module(
            "globalThis.__second_inline_url = import.meta.url;",
            "https://example.com/base/",
            1_000,
        )
        .await
        .unwrap();

        assert_eq!(
            rt.evaluate("[globalThis.__first_inline_url, globalThis.__second_inline_url]")
                .unwrap(),
            serde_json::json!(["https://example.com/base/", "https://example.com/base/"])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn classic_script_url_is_dynamic_import_referrer() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let request_thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let length = stream.read(&mut request).unwrap();
            let path = String::from_utf8_lossy(&request[..length])
                .lines()
                .next()
                .and_then(|line| line.split_ascii_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            let body = "export const value = 'scoped-classic';";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            stream.write_all(response.as_bytes()).unwrap();
            path
        });
        let base = format!("http://{address}");
        let jar = std::sync::Arc::new(tinybrowser_net::CookieJar::new());
        let client = std::sync::Arc::new(tinybrowser_net::HttpClient::with_full_options(
            jar, None, true,
        ));
        let mut rt = JsRuntime::with_base_url(&format!("{base}/page/index.html"));
        rt.set_http_client(client);
        rt.set_dom(parse_html("<html><body></body></html>"));
        rt.run_page_init();
        rt.add_import_map(
            &format!(r#"{{"scopes":{{"{base}/classic/":{{"pkg":"{base}/scoped.js"}}}}}}"#),
            &format!("{base}/page/index.html"),
        )
        .unwrap();

        rt.execute_script(
            &format!("{base}/classic/entry.js"),
            "document.documentElement.setAttribute('data-classic-op', 'ran'); \
             import('pkg').then(module => { globalThis.__classic_import = module.value; });",
        )
        .unwrap();
        rt.run_event_loop().await.unwrap();

        assert_eq!(
            rt.evaluate("globalThis.__classic_import").unwrap(),
            serde_json::json!("scoped-classic")
        );
        assert_eq!(
            rt.evaluate("document.documentElement.getAttribute('data-classic-op')")
                .unwrap(),
            serde_json::json!("ran")
        );
        assert_eq!(request_thread.join().unwrap(), "/scoped.js");
    }

    #[test]
    fn timed_out_classic_script_leaves_runtime_reusable() {
        let mut rt = JsRuntime::new();
        rt.execute_script_with_timeout(
            "https://example.test/hang.js",
            "while (true) {}",
            std::time::Duration::from_millis(20),
        )
        .unwrap();
        rt.execute_script(
            "https://example.test/after-timeout.js",
            "globalThis.__after_timeout = true;",
        )
        .unwrap();
        assert_eq!(
            rt.evaluate("globalThis.__after_timeout").unwrap(),
            serde_json::json!(true)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inline_module_graph_error_propagates() {
        let mut rt = JsRuntime::with_base_url("https://example.com/");
        let error = rt
            .load_inline_module("import 'bare-specifier';", "https://example.com/", 1_000)
            .await
            .unwrap_err();
        assert!(
            error.contains("Inline module load error"),
            "expected graph load error, got: {error}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inline_module_evaluation_error_propagates() {
        let mut rt = JsRuntime::with_base_url("https://example.com/");
        let error = rt
            .load_inline_module(
                "throw new Error('module-evaluation-boom');",
                "https://example.com/",
                1_000,
            )
            .await
            .unwrap_err();
        assert!(
            error.contains("Inline module eval error") && error.contains("module-evaluation-boom"),
            "expected evaluation error, got: {error}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inline_module_evaluation_timeout_propagates() {
        let mut rt = JsRuntime::with_base_url("https://example.com/");
        let error = rt
            .load_inline_module(
                "await new Promise(resolve => setTimeout(resolve, 10000));",
                "https://example.com/",
                20,
            )
            .await
            .unwrap_err();
        assert!(
            error.contains("Inline module evaluation timed out after"),
            "expected evaluation timeout, got: {error}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn successful_inline_module_does_not_wait_for_interval_idle() {
        let mut rt = JsRuntime::with_base_url("https://example.com/");
        rt.load_inline_module(
            "globalThis.__module_loaded = true; setInterval(() => {}, 10000);",
            "https://example.com/",
            500,
        )
        .await
        .unwrap();
        assert_eq!(
            rt.evaluate("globalThis.__module_loaded").unwrap(),
            serde_json::json!(true)
        );
    }

    // Issue #139 — proxy_url must thread through to both the ES-module
    // loader (module_loader.rs) and op_fetch_url's reqwest client
    // (ops.rs::build_request_client). Pre-fix both built clients with
    // `Client::builder().build()` — no proxy — so JS fetch/XHR and
    // dynamic imports silently bypassed BrowserContext.proxy_url.
    //
    // Phase 5.5 RED check: each test references a symbol that does NOT
    // exist on main (proxy_url() accessor, with_proxy ctor,
    // with_base_url_and_proxy ctor), so the tests fail to compile without
    // the prod fix.
    #[test]
    fn http_client_round_trips_proxy_url() {
        use tinybrowser_net::{CookieJar, HttpClient};
        let jar = std::sync::Arc::new(CookieJar::new());
        let configured = HttpClient::with_options(jar.clone(), Some("http://proxy.test:8080"));
        assert_eq!(
            configured.proxy_url(),
            Some("http://proxy.test:8080"),
            "proxy_url() must expose the value passed to with_options"
        );

        let direct = HttpClient::with_options(jar, None);
        assert_eq!(
            direct.proxy_url(),
            None,
            "proxy_url() must return None when no proxy was configured"
        );
    }

    #[test]
    fn module_loader_stores_proxy_for_dynamic_imports() {
        use crate::module_loader::ModuleLoader;
        let loader = ModuleLoader::with_proxy(
            "https://example.com/",
            Some("http://proxy.test:8080".to_string()),
        );
        assert_eq!(loader.proxy_url.as_deref(), Some("http://proxy.test:8080"));
        assert_eq!(loader.base_url, "https://example.com/");

        // Default constructor must keep the historical "no proxy" behaviour.
        let direct = ModuleLoader::new("https://example.com/");
        assert_eq!(direct.proxy_url, None);
    }

    #[test]
    fn runtime_with_base_url_and_proxy_constructs_successfully() {
        // Sanity-check the public ctor that page.rs uses to thread proxy
        // through to the module loader. Direct (None) and proxied paths
        // must both initialise the JS environment.
        let _direct = JsRuntime::with_base_url_and_proxy("https://example.com/", None);
        let _proxied = JsRuntime::with_base_url_and_proxy(
            "https://example.com/",
            Some("http://proxy.test:8080".to_string()),
        );
    }

    // ── Issue #45 (Playwright actionability) regression tests ────────────────
    // Kept at the end of the module so they don't share textual context with
    // unrelated test additions in other branches (avoids spurious merge
    // conflicts when both this branch and an unrelated bootstrap.js change
    // add tests near the start of `mod tests`).

    /// Playwright >= 1.25 calls `element.checkVisibility(...)` before every
    /// input event. If the method isn't defined Playwright retries until its
    /// action timeout fires. Without a layout engine we can't compute it
    /// properly, so the stub always returns true — still strictly better
    /// than the undefined path.
    #[test]
    fn element_check_visibility_is_callable() {
        let mut rt = setup_runtime(r#"<div id="x">x</div>"#);
        let result = rt
            .evaluate("document.getElementById('x').checkVisibility({checkOpacity: true})")
            .unwrap();
        assert_eq!(result, serde_json::json!(true));

        let typeof_method = rt
            .evaluate("typeof document.getElementById('x').checkVisibility")
            .unwrap();
        assert_eq!(typeof_method, serde_json::json!("function"));
    }

    /// Playwright's `getByRole` / `getByLabel` locators resolve via ARIA
    /// reflection properties. Without the getters those locators always
    /// fail. Reflect the underlying aria-* attributes.
    #[test]
    fn element_aria_reflection_properties_read_aria_attrs() {
        let mut rt = setup_runtime(
            r#"<button id="b" role="tab" aria-label="Settings" aria-selected="true">x</button>"#,
        );
        let result = rt
            .evaluate(
                r"
                const el = document.getElementById('b');
                return [el.role, el.ariaLabel, el.ariaSelected];
                ",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["tab", "Settings", "true"]));
    }

    /// Setting an ARIA reflection property must write through to the
    /// underlying attribute so frameworks that toggle state via
    /// `el.ariaExpanded = 'true'` actually update the DOM.
    /// Regression: React 18 / mobile SPAs (e.g. goofish.com) call
    /// addEventListener on navigator.connection (NetworkInformation) and
    /// navigator.serviceWorker (ServiceWorkerContainer). Both are EventTargets
    /// in real browsers; missing the method crashed the app bundle with
    /// "addEventListener is not a function".
    #[test]
    fn navigator_eventtarget_stubs_expose_add_event_listener() {
        let mut rt = setup_runtime("<div></div>");
        let result = rt
            .evaluate(
                r"
                const connection = navigator.connection;
                let calls = 0;
                let receiverMatches = false;
                function listener(event) {
                    calls += 1;
                    receiverMatches = this === connection && event.type === 'change';
                }
                connection.addEventListener('change', listener);
                const dispatchResult = connection.dispatchEvent(new Event('change'));
                connection.removeEventListener('change', listener);
                connection.dispatchEvent(new Event('change'));
                return [
                    typeof connection.addEventListener,
                    typeof connection.removeEventListener,
                    typeof connection.dispatchEvent,
                    typeof navigator.serviceWorker.addEventListener,
                    dispatchResult,
                    calls,
                    receiverMatches,
                ];
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!(["function", "function", "function", "function", true, 1, true])
        );
    }

    #[test]
    fn text_codec_streams_expose_browser_shape() {
        let mut rt = setup_runtime("<div></div>");
        let result = rt
            .evaluate(
                r"
                const encoder = new TextEncoderStream();
                const decoder = new TextDecoderStream();
                return {
                    encoder: encoder.encoding,
                    encoderReadable: typeof encoder.readable.getReader,
                    encoderWritable: typeof encoder.writable.getWriter,
                    decoder: decoder.encoding,
                    decoderReadable: typeof decoder.readable.getReader,
                    decoderWritable: typeof decoder.writable.getWriter,
                };
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!({
                "encoder": "utf-8",
                "encoderReadable": "function",
                "encoderWritable": "function",
                "decoder": "utf-8",
                "decoderReadable": "function",
                "decoderWritable": "function",
            })
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn text_encoder_stream_pipe_through_delivers_hydration_data() {
        let mut rt = setup_runtime("<div></div>");
        let result = rt
            .evaluate_for_cdp(
                r#"
                (async () => {
                    let sourceController;
                    const source = new ReadableStream({
                        start(controller) { sourceController = controller; },
                    });
                    const encoded = source.pipeThrough(new TextEncoderStream());
                    sourceController.enqueue('["server",{"hydrated":true}]\n');
                    sourceController.close();

                    const decoder = new TextDecoder();
                    let tail = "";
                    const lines = encoded.pipeThrough(new TransformStream({
                        transform(chunk, controller) {
                            const complete = (tail + decoder.decode(chunk, {stream: true})).split("\n");
                            tail = complete.pop() || "";
                            for (const line of complete) controller.enqueue(line);
                        },
                        flush(controller) { if (tail) controller.enqueue(tail); },
                    }));
                    const first = await lines.getReader().read();
                    return JSON.parse(first.value);
                })()
                "#,
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(
            result.value.unwrap(),
            serde_json::json!(["server", {"hydrated": true}])
        );
    }

    #[test]
    fn test_text_encoding_wpt_matrix() {
        let mut rt = setup_runtime("<div></div>");
        // Table-driven WPT-style matrix: 20 rows covering utf-8/gbk/big5/shift_jis/euc-* × fatal/BOM/RangeError
        let cases: &[(&str, &str, &str)] = &[
            // utf-8 basics
            ("new TextDecoder().decode(new Uint8Array([65,66,67]))", "\"ABC\"", "utf-8 default"),
            ("new TextDecoder().decode(new Uint8Array([65, 66, 67]).subarray(1, 2))", "\"B\"", "typed array view"),
            ("new TextDecoder().decode(new DataView(new Uint8Array([65,66,67]).buffer))", "\"ABC\"", "DataView"),
            ("new TextDecoder().decode(new Uint8Array([65,66,67]).buffer)", "\"ABC\"", "ArrayBuffer"),
            ("new TextDecoder().decode()", "\"\"", "empty"),
            // BOM handling
            ("new TextDecoder().decode(new Uint8Array([0xEF,0xBB,0xBF,65]))", "\"A\"", "BOM stripped by default"),
            ("new TextDecoder('utf-8', {ignoreBOM:true}).decode(new Uint8Array([0xEF,0xBB,0xBF,65])).charCodeAt(0) === 0xFEFF", "true", "ignoreBOM true"),
            // fatal
            ("(function(){ try { new TextDecoder('utf-8', {fatal:true}).decode(new Uint8Array([0xFF])); return 'no-throw'; } catch(e){ return e.name; }})()", "\"TypeError\"", "fatal throws"),
            ("new TextDecoder('utf-8').decode(new Uint8Array([0xFF])).charCodeAt(0) === 0xFFFD", "true", "non-fatal replacement"),
            // bad label
            ("(function(){ try { new TextDecoder('bad-label'); return 'no-throw'; } catch(e){ return e.name; }})()", "\"RangeError\"", "bad label"),
            // legacy encodings
            ("new TextDecoder('gbk').decode(new Uint8Array([0xC4, 0xE3, 0xBA, 0xC3]))", "\"你好\"", "gbk hello"),
            ("new TextDecoder('gb2312').decode(new Uint8Array([0xC4, 0xE3]))", "\"你\"", "gbk alias"),
            ("new TextDecoder('big5').decode(new Uint8Array([0xA4,0x40]))", "\"一\"", "big5"),
            ("new TextDecoder('shift_jis').decode(new Uint8Array([0x82,0xA0]))", "\"あ\"", "shift_jis"),
            ("new TextDecoder('euc-jp').decode(new Uint8Array([0xA4,0xA2]))", "\"あ\"", "euc-jp"),
            ("new TextDecoder('euc-kr').decode(new Uint8Array([0xB0,0xA1]))", "\"가\"", "euc-kr"),
            // TextEncoder
            ("JSON.stringify(Array.from(new TextEncoder().encode('ABC')))", "\"[65,66,67]\"", "TextEncoder encode"),
            ("new TextEncoder().encoding", "\"utf-8\"", "TextEncoder encoding"),
            ("(function(){ const e=new TextEncoder(); const d=new Uint8Array(10); const r=e.encodeInto('hello', d); return r.read===5 && r.written===5; })()", "true", "encodeInto full"),
            ("(function(){ const e=new TextEncoder(); const d=new Uint8Array(2); const r=e.encodeInto('hello', d); return r.read===2 && r.written===2; })()", "true", "encodeInto truncated"),
        ];
        for (expr, expected_json, name) in cases {
            let got = rt.evaluate(expr).unwrap().to_string();
            assert_eq!(&got, expected_json, "case {name}: {expr}");
        }
        // Also verify native toString
        let native = rt.evaluate("TextDecoder.toString().includes('[native code]')").unwrap();
        assert_eq!(native, serde_json::Value::Bool(true), "TextDecoder should be native");
    }

    /// Regression test for #285: DDoS-Guard's challenge calls
    /// `t.insertAdjacentText(...)` and dies with `TypeError: ... is not a
    /// function` because `Element.prototype.insertAdjacentText` was missing.
    /// Verify all four positions place a Text node (NOT parsed HTML) at the
    /// right spot. Tests `insertAdjacentText` exists, is callable, and that
    /// inserted content remains literal text — angle brackets must not be
    /// parsed as markup, which is the whole point of the API.
    #[test]
    fn element_insert_adjacent_text_polyfill() {
        let mut rt = setup_runtime(r#"<div id="p"><span id="t">X</span></div>"#);
        let result = rt
            .evaluate(
                r"
                const t = document.getElementById('t');
                t.insertAdjacentText('afterbegin', 'AB');
                t.insertAdjacentText('beforeend', 'BE');
                t.insertAdjacentText('beforebegin', 'BB');
                t.insertAdjacentText('afterend', 'AE');
                t.insertAdjacentText('beforeend', '<b>raw</b>');
                return [
                    typeof Element.prototype.insertAdjacentText,
                    document.getElementById('p').textContent,
                    t.getElementsByTagName('b').length,
                ];
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!(["function", "BBABXBE<b>raw</b>AE", 0])
        );
    }

    /// Regression test for #285: `Element.prototype.insertAdjacentElement`
    /// was missing alongside `insertAdjacentText`. Verify all four positions
    /// place the given element correctly and that the inserted element is
    /// returned (per spec — that's the contract callers rely on for chaining).
    #[test]
    fn element_insert_adjacent_element_polyfill() {
        let mut rt = setup_runtime(r#"<div id="p"><span id="t">X</span></div>"#);
        let result = rt
            .evaluate(
                r"
                const t = document.getElementById('t');
                const before = document.createElement('b');  before.id = 'before';
                const after  = document.createElement('i');  after.id  = 'after';
                const inside = document.createElement('em'); inside.id = 'inside';
                const last   = document.createElement('u');  last.id   = 'last';
                const r1 = t.insertAdjacentElement('beforebegin', before);
                const r2 = t.insertAdjacentElement('afterend',    after);
                const r3 = t.insertAdjacentElement('afterbegin',  inside);
                const r4 = t.insertAdjacentElement('beforeend',   last);
                const siblings = Array.from(document.getElementById('p').children).map(c => c.id);
                const inT = Array.from(t.children).map(c => c.id);
                return [
                    typeof Element.prototype.insertAdjacentElement,
                    r1 === before && r2 === after && r3 === inside && r4 === last,
                    siblings,
                    inT,
                ];
                ",
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                "function",
                true,
                ["before", "t", "after"],
                ["inside", "last"]
            ])
        );
    }

    #[test]
    fn console_log_error_does_not_trigger_prepare_stack_trace() {
        let mut rt = setup_runtime("<div></div>");
        let result = rt
            .evaluate(
                r#"
            let called = false;
            const saved = Error.prepareStackTrace;
            Error.prepareStackTrace = function() { called = true; return saved; };
            const e = new Error("test");
            console.log(e);
            Error.prepareStackTrace = saved;
            return called;
        "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(false));
    }

    #[test]
    fn element_aria_reflection_setters_write_through() {
        let mut rt = setup_runtime(r#"<div id="d"></div>"#);
        let result = rt
            .evaluate(
                r"
                const el = document.getElementById('d');
                el.role = 'menu';
                el.ariaExpanded = 'true';
                return [el.getAttribute('role'), el.getAttribute('aria-expanded')];
                ",
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["menu", "true"]));
    }

    /// Framework schedulers commonly subclass EventTarget for their own
    /// lifecycle events. These targets have no backing DOM node, but must
    /// still deliver callbacks (including object, once, and signal listeners).
    #[test]
    fn standalone_event_target_delivers_framework_lifecycle_events() {
        let mut rt = setup_runtime("<div></div>");
        let result = rt
            .evaluate(
                r#"
                const TypedEventTarget = class extends EventTarget {};
                const target = new TypedEventTarget();
                const calls = [];
                const removed = () => calls.push("removed");
                const controller = new AbortController();
                const node = { id: "canvas-ref" };

                target.addEventListener("insert", (event) => calls.push(event.node.id));
                target.addEventListener("insert", { handleEvent() { calls.push("object"); } });
                target.addEventListener("insert", () => calls.push("once"), { once: true });
                target.addEventListener("insert", removed);
                target.removeEventListener("insert", removed);
                target.addEventListener("insert", () => calls.push("aborted"), {
                    signal: controller.signal,
                });
                controller.abort();

                const first = new Event("insert", { cancelable: true });
                first.node = node;
                const firstResult = target.dispatchEvent(first);
                const second = new Event("insert");
                second.node = node;
                const secondResult = target.dispatchEvent(second);
                return [
                    target instanceof EventTarget,
                    calls,
                    firstResult,
                    secondResult,
                    first.target === target,
                    first.currentTarget === null,
                ];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                true,
                ["canvas-ref", "object", "once", "canvas-ref", "object"],
                true,
                true,
                true,
                true
            ])
        );
    }

    #[test]
    fn media_text_tracks_expose_loaded_webvtt_cues() {
        let mut rt = setup_runtime(
            r#"<video><track id="captions" kind="captions" srclang="en" default
                src="data:text/vtt,WEBVTT%0A%0A00%3A00%3A01.000%20--%3E%2000%3A00%3A03.000%0AHello"></video>"#,
        );
        let result = rt
            .evaluate(
                r#"
                const element = document.getElementById("captions");
                const video = document.querySelector("video");
                const cue = element.track.cues[0];
                const added = video.addTextTrack("metadata", "Data", "en");
                cue.line = -2;
                cue.size = 80;
                return [
                    element instanceof HTMLTrackElement,
                    element.readyState === HTMLTrackElement.LOADED,
                    element.track instanceof TextTrack,
                    element.track.cues.length,
                    cue.startTime,
                    cue.endTime,
                    cue.text,
                    cue.line,
                    cue.size,
                    video.textTracks.length,
                    video.textTracks.getTrackById("captions") === element.track,
                    added.kind,
                ];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([true, true, true, 1, 1, 3, "Hello", -2, 80, 1, true, "metadata"])
        );
    }

    #[test]
    fn unsupported_media_capabilities_and_readiness_are_honest() {
        let mut rt = setup_runtime(
            r#"<video id="media" src="https://example.test/movie.mp4"
                poster="https://example.test/poster.png"></video>"#,
        );
        let result = rt
            .evaluate(
                r#"
                const media = document.getElementById("media");
                return [
                    media.canPlayType("video/mp4"),
                    media.canPlayType('video/webm; codecs="vp9"'),
                    media.readyState,
                    media.currentTime,
                    media.videoWidth,
                    media.videoHeight,
                    media.paused,
                    media.currentSrc,
                    media.poster,
                ];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([
                "",
                "",
                0,
                0,
                0,
                0,
                true,
                "",
                "https://example.test/poster.png"
            ])
        );
    }

    #[test]
    fn html_string_scripts_remain_inert_when_connected() {
        let mut rt = setup_runtime("<html><head></head><body><div id=target></div></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                globalThis.__fragmentScriptRuns = 0;

                const direct = document.createElement("div");
                direct.innerHTML = "<script>globalThis.__fragmentScriptRuns++<\/script>";
                document.body.appendChild(direct.firstChild);

                const nested = document.createElement("div");
                nested.innerHTML = "<section><script>globalThis.__fragmentScriptRuns++<\/script></section>";
                document.body.appendChild(nested.firstChild);

                const template = document.createElement("template");
                template.innerHTML = "<script>globalThis.__fragmentScriptRuns++<\/script>";
                document.body.appendChild(template.content.firstChild);

                const nestedTemplateHolder = document.createElement("div");
                nestedTemplateHolder.innerHTML =
                    "<template><script>globalThis.__fragmentScriptRuns++<\/script></template>";
                document.body.appendChild(
                    nestedTemplateHolder.firstChild.content.firstChild
                );

                document.getElementById("target").insertAdjacentHTML(
                    "beforeend",
                    "<script>globalThis.__fragmentScriptRuns++<\/script>"
                );

                const parsed = new DOMParser().parseFromString(
                    "<body><script>globalThis.__fragmentScriptRuns++<\/script></body>",
                    "text/html"
                );
                document.body.appendChild(parsed.querySelector("script"));

                let externalFetches = 0;
                const originalFetchOp = __tbHost.core.ops.op_fetch_url;
                try {
                    __tbHost.core.ops.op_fetch_url = () => {
                        externalFetches++;
                        return JSON.stringify({
                            status: 200,
                            headers: {"content-type": "text/javascript"},
                            body: "globalThis.__fragmentScriptRuns++",
                            url: "http://example.com/inert.js"
                        });
                    };
                    const external = document.createElement("div");
                    external.innerHTML = "<script src=/inert.js><\/script>";
                    document.head.appendChild(external.firstChild);
                } finally {
                    __tbHost.core.ops.op_fetch_url = originalFetchOp;
                }
                return [globalThis.__fragmentScriptRuns, externalFetches];
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!([0, 0]));
    }

    #[test]
    fn connected_insertion_prepares_dynamic_script_subtrees_once() {
        let mut rt = setup_runtime("<html><head></head><body><i id=anchor></i></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                globalThis.__dynamicScriptRuns = [];

                const detached = document.createElement("div");
                const delayed = document.createElement("script");
                delayed.textContent = "globalThis.__dynamicScriptRuns.push('delayed')";
                detached.appendChild(delayed);
                const beforeConnection = globalThis.__dynamicScriptRuns.length;
                document.body.appendChild(detached);
                document.head.appendChild(delayed);

                const before = document.createElement("script");
                before.textContent = "globalThis.__dynamicScriptRuns.push('before')";
                document.body.insertBefore(before, document.getElementById("anchor"));

                const replacement = document.createElement("script");
                replacement.textContent = "globalThis.__dynamicScriptRuns.push('replace')";
                document.body.replaceChild(replacement, document.getElementById("anchor"));

                const subtree = document.createElement("section");
                const nested = document.createElement("script");
                nested.textContent = "globalThis.__dynamicScriptRuns.push('nested')";
                subtree.appendChild(nested);
                document.body.appendChild(subtree);

                return [beforeConnection, globalThis.__dynamicScriptRuns];
                "#,
            )
            .unwrap();
        assert_eq!(
            result,
            serde_json::json!([0, ["delayed", "before", "replace", "nested"]])
        );
    }

    #[test]
    fn script_clone_preserves_started_state() {
        let mut rt = setup_runtime(
            "<html><head></head><body><script id=parser>globalThis.__cloneScriptRuns++</script></body></html>",
        );
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                globalThis.__cloneScriptRuns = 0;
                const parser = document.getElementById("parser");
                globalThis.__markParserScripts([parser._nid]);
                document.head.appendChild(parser);
                document.body.appendChild(parser.cloneNode(true));

                const dynamic = document.createElement("script");
                dynamic.textContent = "globalThis.__cloneScriptRuns++";
                document.body.appendChild(dynamic);
                document.body.appendChild(dynamic.cloneNode(true));

                const holder = document.createElement("div");
                holder.innerHTML = "<script>globalThis.__cloneScriptRuns++<\/script>";
                document.body.appendChild(holder.firstChild.cloneNode(true));

                const fragment = document.createDocumentFragment();
                fragment.appendChild(dynamic.cloneNode(true));
                document.body.appendChild(fragment.cloneNode(true));
                return globalThis.__cloneScriptRuns;
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(1));
    }

    #[test]
    fn contextual_fragment_and_document_write_keep_executable_script_policy() {
        let mut rt = setup_runtime("<html><head></head><body><div id=context></div></body></html>");
        let result = rt
            .evaluate(
                r#"
                var scriptTestSetup = true;
                globalThis.__executableFragmentRuns = [];
                const range = document.createRange();
                range.selectNode(document.getElementById("context"));
                const fragment = range.createContextualFragment(
                    "<template><script>globalThis.__executableFragmentRuns.push('template')<\/script></template>" +
                    "<script>globalThis.__executableFragmentRuns.push('range')<\/script>"
                );
                document.body.appendChild(fragment);
                document.write(
                    "<script>globalThis.__executableFragmentRuns.push('write')<\/script>"
                );
                return globalThis.__executableFragmentRuns;
                "#,
            )
            .unwrap();
        assert_eq!(result, serde_json::json!(["range", "write"]));
    }
}
