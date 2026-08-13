use std::cell::RefCell;
use std::panic::AssertUnwindSafe;
use std::rc::Rc;

use obscura_dom::DomTree;
use rquickjs::function::Rest;
use rquickjs::{CatchResultExt, Context, Function, Object, Runtime, Value};

use crate::ops::{ObscuraState, SharedState};

const SHIM: &str = include_str!("../js/bootstrap.js");

const REQUIRED_STRING_OPS: &[&str] = &[
    "op_shadow_root_info",
    "op_get_cookies",
    "op_url_parse",
    "op_url_set",
    "op_url_resolve",
    "op_document_domain_candidate",
    "op_add_import_map",
    "op_encoding_for_label",
    "op_text_decode",
    "op_url_encode_query",
];

const REQUIRED_BOOL_OPS: &[&str] = &[
    "op_script_mark_started",
    "op_script_try_start",
    "op_async_runtime_available",
];

const REQUIRED_VOID_OPS: &[&str] = &[
    "op_console_msg",
    "op_set_cookie",
    "op_navigate",
    "op_binding_called",
];

const REQUIRED_BUFFER_OPS: &[&str] = &[
    "op_subtle_digest",
    "op_subtle_hmac",
    "op_subtle_aes_gcm",
    "op_subtle_aes_cbc",
    "op_subtle_aes_ctr",
    "op_subtle_pbkdf2",
    "op_subtle_hkdf",
    "op_random_bytes",
];

/// QuickJS runtime that will replace V8 inside obscura-js.
///
/// Lives beside the existing V8 path. Page still uses
/// [`crate::runtime::ObscuraJsRuntime`].
pub struct QuickJsRuntime {
    context: Context,
    state: SharedState,
}

fn catch_op<T: Default>(f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| T::default())
}

fn catch_op_string(f: impl FnOnce() -> String) -> String {
    std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        tracing::error!("op panicked; returning null");
        "null".to_string()
    })
}

impl QuickJsRuntime {
    pub fn new() -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|e| e.to_string())?;
        let context = Context::full(&runtime).map_err(|e| e.to_string())?;
        let state = Rc::new(RefCell::new(ObscuraState::new()));
        Self::install_ops(&context, &state)?;
        Ok(Self { context, state })
    }

    pub fn set_dom(&self, dom: DomTree) {
        self.state.borrow_mut().dom = Some(dom);
    }

    fn install_ops(context: &Context, state: &SharedState) -> Result<(), String> {
        let dom_state = state.clone();
        context.with(|ctx| {
            let ops = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
            ops.set(
                "op_dom",
                Function::new(ctx.clone(), move |cmd: String, a1: String, a2: String| {
                    catch_op_string(|| crate::ops::op_dom_inner(&dom_state, cmd, a1, a2))
                })
                .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;

            ops.set(
                "op_shadow_attach",
                Function::new(ctx.clone(), |_: Rest<Value>| catch_op(|| -1i32))
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;

            for name in REQUIRED_STRING_OPS {
                ops.set(
                    *name,
                    Function::new(ctx.clone(), |_: Rest<Value>| catch_op_string(|| String::new()))
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
            }
            for name in REQUIRED_BOOL_OPS {
                ops.set(
                    *name,
                    Function::new(ctx.clone(), |_: Rest<Value>| catch_op(|| false))
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
            }
            for name in REQUIRED_VOID_OPS {
                ops.set(
                    *name,
                    Function::new(ctx.clone(), |_: Rest<Value>| catch_op(|| ()))
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
            }
            for name in REQUIRED_BUFFER_OPS {
                ops.set(
                    *name,
                    Function::new(ctx.clone(), |_: Rest<Value>| catch_op_string(|| String::new()))
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
            }

            let core = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
            core.set("ops", ops).map_err(|e| e.to_string())?;
            core.set(
                "queueUserTimer",
                Function::new(ctx.clone(), |_: Rest<Value>| 0i32).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            core.set(
                "cancelTimer",
                Function::new(ctx.clone(), |_: Rest<Value>| ()).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;

            let deno = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
            deno.set("core", core).map_err(|e| e.to_string())?;
            ctx.globals()
                .set("Deno", deno)
                .map_err(|e| e.to_string())?;
            Ok(())
        })
    }

    pub fn evaluate(&mut self, expression: &str) -> Result<serde_json::Value, String> {
        self.context.with(|ctx| {
            let script = format!("JSON.stringify({expression})");
            let raw: String = ctx
                .eval(script.as_str())
                .catch(&ctx)
                .map_err(|e| e.to_string())?;
            serde_json::from_str(&raw).map_err(|e| e.to_string())
        })
    }

    pub fn load_shim(&mut self) -> Result<(), String> {
        self.context.with(|ctx| {
            // QuickJS-ng ships a native Performance whose timeOrigin is
            // read-only. The shim keeps that object (`performance || { ... }`)
            // and __obscura_init then assigns timeOrigin. Drop the native
            // object so the shim's writable fallback is used. Do not edit the shim.
            ctx.eval::<(), _>("delete globalThis.performance;")
                .catch(&ctx)
                .map_err(|e| format!("clear native performance: {e}"))?;
            ctx.eval::<(), _>(SHIM)
                .catch(&ctx)
                .map_err(|e| format!("shim eval failed: {e}"))?;
            Ok(())
        })
    }

    pub fn run_page_init(&mut self) -> Result<(), String> {
        self.context.with(|ctx| {
            ctx.eval::<(), _>("globalThis.__obscura_init();")
                .catch(&ctx)
                .map_err(|e| format!("__obscura_init failed: {e}"))
        })
    }

    #[cfg(test)]
    fn install_panicking_op(&self) -> Result<(), String> {
        self.context.with(|ctx| {
            let ops: Object = ctx
                .eval("Deno.core.ops")
                .catch(&ctx)
                .map_err(|e| e.to_string())?;
            ops.set(
                "op_test_panic",
                Function::new(ctx.clone(), || catch_op_string(|| panic!("test panic")))
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use obscura_dom::parse_html;

    fn page(html: &str) -> QuickJsRuntime {
        let mut rt = QuickJsRuntime::new().expect("runtime constructs");
        rt.set_dom(parse_html(html));
        rt.load_shim().expect("shim loads");
        rt.run_page_init().expect("page init");
        rt
    }

    #[test]
    fn evaluate_one_plus_one() {
        let mut rt = QuickJsRuntime::new().expect("runtime constructs");
        assert_eq!(rt.evaluate("1+1").expect("eval"), serde_json::json!(2));
    }

    #[test]
    fn deno_core_ops_is_an_object() {
        let mut rt = QuickJsRuntime::new().expect("runtime constructs");
        assert_eq!(
            rt.evaluate("typeof Deno.core.ops").expect("eval"),
            serde_json::json!("object")
        );
    }

    #[test]
    fn load_shim_exposes_window_and_document() {
        let mut rt = page("<html><body></body></html>");
        assert_eq!(
            rt.evaluate("typeof window").expect("eval"),
            serde_json::json!("object")
        );
        assert_eq!(
            rt.evaluate("typeof document").expect("eval"),
            serde_json::json!("object")
        );
    }

    #[test]
    fn create_element_tag_name_is_div() {
        let mut rt = page("<html><body></body></html>");
        assert_eq!(
            rt.evaluate("document.createElement('div').tagName")
                .expect("eval"),
            serde_json::json!("DIV")
        );
    }

    #[test]
    fn get_element_by_id_round_trip() {
        let mut rt = page("<html><body><p id=\"hello\">hi</p></body></html>");
        assert_eq!(
            rt.evaluate("document.getElementById('hello').id")
                .expect("eval"),
            serde_json::json!("hello")
        );
    }

    #[test]
    fn query_selector_round_trip() {
        let mut rt = page("<html><body><p id=\"hello\">hi</p></body></html>");
        assert_eq!(
            rt.evaluate("document.querySelector('#hello').tagName")
                .expect("eval"),
            serde_json::json!("P")
        );
    }

    #[test]
    fn op_panic_returns_error_without_unwinding() {
        let mut rt = QuickJsRuntime::new().expect("runtime constructs");
        rt.install_panicking_op().expect("bind panicking op");
        assert_eq!(
            rt.evaluate("Deno.core.ops.op_test_panic()")
                .expect("panic is caught"),
            serde_json::json!("null")
        );
        assert_eq!(
            rt.evaluate("1+1").expect("runtime still runs"),
            serde_json::json!(2)
        );
    }

    #[test]
    fn render_ops_are_not_registered() {
        let mut rt = QuickJsRuntime::new().expect("runtime constructs");
        assert_eq!(
            rt.evaluate("typeof Deno.core.ops.op_layout_geometry")
                .expect("eval"),
            serde_json::json!("undefined")
        );
        assert_eq!(
            rt.evaluate("typeof Deno.core.ops.op_computed_style")
                .expect("eval"),
            serde_json::json!("undefined")
        );
    }
}
