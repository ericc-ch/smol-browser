use serde_json::{json, Value};

pub async fn handle(method: &str, _params: &Value) -> Result<Value, String> {
    match method {
        "getVersion" => Ok(json!({
            "protocolVersion": "1.3",
            "product": "Chrome/145.0.0.0",
            "revision": "@0000000000000000000000000000000000000000",
            "userAgent": "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36",
            "jsVersion": "14.5.0.0",
        })),
        "close" => Ok(json!({})),
        "getWindowForTarget" => Err(crate::util::cdp_unimplemented("Browser.getWindowForTarget")),
        "setDownloadBehavior" => Err(crate::util::cdp_unimplemented(
            "Browser.setDownloadBehavior",
        )),
        "getWindowBounds" => Err(crate::util::cdp_unimplemented("Browser.getWindowBounds")),
        "setWindowBounds" => Err(crate::util::cdp_unimplemented("Browser.setWindowBounds")),
        _ => Err(format!("Unknown Browser method: {}", method)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn window_and_download_stubs_are_explicit_unimplemented() {
        for method in [
            "getWindowForTarget",
            "getWindowBounds",
            "setWindowBounds",
            "setDownloadBehavior",
        ] {
            let err = handle(method, &json!({}))
                .await
                .expect_err("no-op stubs must error");
            assert!(
                err.contains("not implemented by tinybrowser"),
                "{method} must say unimplemented: {err}"
            );
            assert!(
                err.contains(&format!("Browser.{method}")),
                "{method} error must name the CDP method: {err}"
            );
        }
    }
}
