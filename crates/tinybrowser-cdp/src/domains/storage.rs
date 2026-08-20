use serde_json::{json, Value};

use crate::cookie_params::{parse_cdp_cookie, parse_delete_cookies_params};
use crate::dispatch::CdpContext;
use crate::domains::network::cookie_info_to_cdp_json;

fn cookie_jar_for(
    ctx: &CdpContext,
    params: &Value,
    session_id: &Option<String>,
) -> Result<std::sync::Arc<tinybrowser_net::CookieJar>, String> {
    match params
        .get("browserContextId")
        .and_then(|value| value.as_str())
    {
        Some(id) => ctx
            .browser_context(id)
            .map(|context| context.cookie_jar.clone())
            .ok_or_else(|| format!("Browser context not found: {id}")),
        None => Ok(ctx.get_session_page(session_id).map_or_else(
            || ctx.default_context.cookie_jar.clone(),
            |page| page.context.cookie_jar.clone(),
        )),
    }
}

pub async fn handle(
    method: &str,
    params: &Value,
    ctx: &mut CdpContext,
    session_id: &Option<String>,
) -> Result<Value, String> {
    match method {
        "getCookies" => {
            let cookies: Vec<tinybrowser_net::CookieInfo> = cookie_jar_for(ctx, params, session_id)?
                .all()
                .into_iter()
                .map(Into::into)
                .collect();
            let cdp_cookies: Vec<Value> = cookies.iter().map(cookie_info_to_cdp_json).collect();
            Ok(json!({ "cookies": cdp_cookies }))
        }
        "setCookies" => {
            if let Some(cookies) = params.get("cookies").and_then(|v| v.as_array()) {
                let parsed: Vec<_> = cookies.iter().filter_map(parse_cdp_cookie).collect();
                let jar = cookie_jar_for(ctx, params, session_id)?;
                for info in parsed {
                    if let Ok(c) = tinybrowser_net::Cookie::try_from(info) {
                        let _ = jar.store(c);
                    }
                }
            }
            Ok(json!({}))
        }
        "deleteCookies" => {
            if let Some(filter) = parse_delete_cookies_params(params) {
                let _ = cookie_jar_for(ctx, params, session_id)?.remove(&tinybrowser_net::CookieKey {
                    name: tinybrowser_net::CookieName(filter.name),
                    domain: tinybrowser_net::Domain(filter.domain),
                    path: filter.path.map(tinybrowser_net::CookiePath),
                });
            }
            Ok(json!({}))
        }
        _ => Err(format!("Unknown Storage method: {method}")),
    }
}
