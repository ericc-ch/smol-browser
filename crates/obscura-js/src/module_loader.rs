use std::sync::Arc;

use url::Url;

use crate::import_map::ImportMap;

/// Observable network activity for ES-module graphs.
///
/// The browser lifecycle still needs to distinguish a genuinely idle page from
/// a graph whose fetch is in flight. A loader-owned counter provides that
/// signal without treating unrelated fetch/XHR analytics as render-blocking
/// work.
#[derive(Debug, Default)]
pub(crate) struct ModuleLoadActivity {
    pending: std::sync::atomic::AtomicUsize,
    last_activity: std::sync::Mutex<Option<std::time::Instant>>,
}

impl ModuleLoadActivity {
    pub(crate) fn begin(self: &Arc<Self>) -> ModuleLoadGuard {
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(std::time::Instant::now());
        ModuleLoadGuard(self.clone())
    }

    pub(crate) fn is_pending_or_recent(&self, grace: std::time::Duration) -> bool {
        if self.pending.load(std::sync::atomic::Ordering::Relaxed) != 0 {
            return true;
        }
        self.last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some_and(|last| last.elapsed() <= grace)
    }
}

pub(crate) struct ModuleLoadGuard(Arc<ModuleLoadActivity>);

impl Drop for ModuleLoadGuard {
    fn drop(&mut self) {
        let previous = self
            .0
            .pending
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        debug_assert!(previous > 0, "module load activity counter underflow");
        *self
            .0
            .last_activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(std::time::Instant::now());
    }
}

/// Standalone loader identity used by the proxy round-trip test. Network
/// fetches go through [`fetch_module_bytes`] on the QuickJS network thread.
pub struct ObscuraModuleLoader {
    pub base_url: String,
    /// Proxy URL threaded through to every dynamic ES-module fetch (#139).
    /// `None` keeps the pre-#139 direct-connection behaviour for callers
    /// that haven't been updated.
    pub proxy_url: Option<String>,
}

impl ObscuraModuleLoader {
    pub fn new(base_url: &str) -> Self {
        Self::with_proxy(base_url, None)
    }

    pub fn with_proxy(base_url: &str, proxy_url: Option<String>) -> Self {
        ObscuraModuleLoader {
            base_url: base_url.to_string(),
            proxy_url,
        }
    }
}

/// Resolve a module specifier against the document import map.
///
/// A synthetic `"."` referrer means this is the graph root (`<script type=module
/// src>`). The document import map must not remap that root URL.
pub(crate) fn resolve_module_specifier(
    import_map: &mut ImportMap,
    specifier: &str,
    referrer: &str,
    document_base: &str,
) -> Result<Url, String> {
    if referrer == "." {
        return resolve_import(specifier, document_base);
    }

    let base = if referrer.is_empty()
        || referrer.starts_with('<')
        || referrer == "about:blank"
    {
        document_base
    } else {
        referrer
    };

    let base = Url::parse(base)
        .map_err(|e| format!("Invalid module referrer {base}: {e}"))?;
    import_map.resolve(specifier, &base)
}

fn resolve_import(specifier: &str, base: &str) -> Result<Url, String> {
    if let Ok(url) = Url::parse(specifier) {
        return Ok(url);
    }
    let base = Url::parse(base).map_err(|e| format!("Invalid module base {base}: {e}"))?;
    base.join(specifier)
        .map_err(|e| format!("Failed to resolve {specifier} against {base}: {e}"))
}

/// Fetch one ES module with the page's cookie jar, proxy, SSRF, and callbacks.
pub(crate) async fn fetch_module_bytes(
    client: &obscura_net::ObscuraHttpClient,
    url: &Url,
    document_url: &Url,
    referrer: &Url,
    callbacks: Option<&obscura_net::CallbackRegistry>,
) -> Result<(String, String), String> {
    // Same cross-scheme rule as classic <script src>: a web document must
    // not pull file: modules, which would read the local filesystem.
    if url.scheme() == "file" && !document_url.scheme().eq_ignore_ascii_case("file") {
        return Err(format!(
            "blocking cross-scheme module load: page={document_url} src={url}"
        ));
    }
    tracing::debug!("Loading ES module: {}", url);
    let resp = client
        .fetch_resource_with_callbacks(
            url,
            obscura_net::ResourceRequest::module_script(document_url, referrer),
            callbacks,
        )
        .await
        .map_err(|e| format!("Failed to fetch module {url}: {e}"))?;
    if !(200..=299).contains(&resp.status) {
        return Err(format!("Module {url} returned HTTP {}", resp.status));
    }
    let code = obscura_net::decode_non_html(&resp.body, resp.content_type());
    Ok((resp.url.as_str().to_string(), code))
}
