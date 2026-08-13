use rquickjs::{CatchResultExt, Context, Runtime};

const SHIM: &str = include_str!("../js/bootstrap.js");

/// QuickJS runtime that will replace V8 inside obscura-js.
///
/// Lives beside the existing V8 path. Page still uses
/// [`crate::runtime::ObscuraJsRuntime`].
pub struct QuickJsRuntime {
    context: Context,
}

impl QuickJsRuntime {
    pub fn new() -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|e| e.to_string())?;
        let context = Context::full(&runtime).map_err(|e| e.to_string())?;
        Self::install_stub_ops(&context)?;
        Ok(Self { context })
    }

    fn install_stub_ops(context: &Context) -> Result<(), String> {
        context.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
                globalThis.Deno = {
                  core: {
                    ops: new Proxy({}, {
                      get: function() { return function() { return ""; }; }
                    }),
                    queueUserTimer: function() { return 0; },
                    cancelTimer: function() {}
                  }
                };
                "#,
            )
            .catch(&ctx)
            .map_err(|e| e.to_string())
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
            ctx.eval::<(), _>("globalThis.__obscura_init();")
                .catch(&ctx)
                .map_err(|e| format!("__obscura_init failed: {e}"))?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut rt = QuickJsRuntime::new().expect("runtime constructs");
        rt.load_shim().expect("shim loads");
        assert_eq!(
            rt.evaluate("typeof window").expect("eval"),
            serde_json::json!("object")
        );
        assert_eq!(
            rt.evaluate("typeof document").expect("eval"),
            serde_json::json!("object")
        );
    }
}
