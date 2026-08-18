use std::collections::HashMap;
use std::error::Error;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{watch, RwLock};
use url::Url;

use crate::cache::{
    response_cache_lifetime, ResourceCacheKey, ResourceLoaderState, SharedFetchLeader,
    SharedFetchOutcome,
};
use crate::callbacks::CallbackRegistry;
use crate::cookies::CookieJar;
use crate::cors::{
    cors_required, is_cors_simple_method, is_cors_unsafe_request_header, redirect_taints_origin,
    request_fetch_site, request_referrer, serialized_request_origin, validate_cors_response,
    validate_request_mode,
};
use crate::interceptor::{InterceptAction, RequestInterceptor};
use crate::ssrf::{custom_cert_store_requested, fetch_file_url, validate_url};
use crate::types::{
    response_too_large, NetError, RequestInfo, RequestMode, ResourceRequest, ResourceType, Response,
};

pub const STEALTH_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/145.0.0.0 Safari/537.36";

pub const STEALTH_NAVIGATOR_PLATFORM: &str = "Win32";
pub const STEALTH_UA_PLATFORM: &str = "Windows";
pub const STEALTH_UA_PLATFORM_VERSION: &str = "15.0.0";

pub(crate) struct InFlightGuard {
    counter: Arc<AtomicU32>,
}

impl InFlightGuard {
    pub(crate) fn new(counter: &Arc<AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self {
            counter: counter.clone(),
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

fn wreq_response_header_value<'a>(
    headers: &'a wreq::header::HeaderMap,
    name: &'static str,
    url: &Url,
) -> Result<Option<&'a str>, NetError> {
    let mut values = headers.get_all(name).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(NetError::Cors(format!(
            "{} returned multiple {} headers",
            url, name
        )));
    }
    first
        .to_str()
        .map(Some)
        .map_err(|_| NetError::Cors(format!("{} returned an invalid {} header", url, name)))
}

fn validate_wreq_cors_response(
    request: &ResourceRequest,
    target: &Url,
    serialized_origin: &str,
    headers: &wreq::header::HeaderMap,
) -> Result<(), NetError> {
    if !cors_required(request, target) {
        return Ok(());
    }
    let allow_origin = wreq_response_header_value(headers, "access-control-allow-origin", target)?;
    let allow_credentials =
        wreq_response_header_value(headers, "access-control-allow-credentials", target)?;
    validate_cors_response(
        request,
        target,
        serialized_origin,
        allow_origin,
        allow_credentials,
    )
}

async fn read_wreq_body_limited(
    response: wreq::Response,
    url: &Url,
    limit: usize,
) -> Result<Vec<u8>, NetError> {
    if response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .is_some_and(|length| length > limit as u64)
    {
        return Err(response_too_large(url, limit));
    }

    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0)
        .min(limit);
    let stream = response.bytes_stream();
    futures_util::pin_mut!(stream);
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| NetError::Network(format!("Failed to read body: {}", error)))?;
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(response_too_large(url, limit));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn proxy_is_socks(proxy: &str) -> bool {
    let scheme = proxy.split(':').next().unwrap_or("").to_ascii_lowercase();
    matches!(scheme.as_str(), "socks" | "socks4" | "socks5" | "socks5h")
}

fn load_custom_cert_store() -> Result<wreq::tls::trust::CertStore, String> {
    let mut pems = Vec::new();
    if let Some(path) = std::env::var_os("SSL_CERT_FILE").filter(|path| !path.is_empty()) {
        pems.push(std::fs::read(&path).map_err(|error| {
            format!("failed to read SSL_CERT_FILE {}: {error}", path.display())
        })?);
    }
    if let Some(directory) = std::env::var_os("SSL_CERT_DIR").filter(|path| !path.is_empty()) {
        let entries = std::fs::read_dir(&directory).map_err(|error| {
            format!(
                "failed to read SSL_CERT_DIR {}: {error}",
                std::path::Path::new(&directory).display()
            )
        })?;
        for entry in entries.filter_map(Result::ok) {
            if let Ok(bytes) = std::fs::read(entry.path()) {
                pems.push(bytes);
            }
        }
    }
    if pems.is_empty() {
        return Err("SSL_CERT_FILE/SSL_CERT_DIR set but no certificates were loaded".into());
    }
    let mut store = wreq::tls::trust::CertStore::builder();
    for pem in &pems {
        store = store.add_pem_cert(pem.as_slice());
    }
    store
        .build()
        .map_err(|error| format!("failed to build custom certificate store: {error}"))
}

fn default_timeout() -> Duration {
    let timeout_ms = std::env::var("TINYBROWSER_FETCH_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);
    Duration::from_millis(timeout_ms)
}

pub struct HttpClient {
    client: wreq::Client,
    proxy_url: Option<String>,
    unsupported_proxy: Option<String>,
    pub cookie_jar: Arc<CookieJar>,
    pub user_agent: RwLock<String>,
    pub extra_headers: RwLock<HashMap<String, String>>,
    pub interceptor: RwLock<Option<Box<dyn RequestInterceptor + Send + Sync>>>,
    pub timeout: Duration,
    pub in_flight: Arc<AtomicU32>,
    pub block_trackers: bool,
    resource_loader: std::sync::Mutex<ResourceLoaderState>,
    pub allow_private_network: bool,
}

impl HttpClient {
    pub fn new() -> Self {
        Self::with_cookie_jar(Arc::new(CookieJar::new()))
    }

    pub fn with_cookie_jar(cookie_jar: Arc<CookieJar>) -> Self {
        Self::with_options(cookie_jar, None)
    }

    pub fn with_options(cookie_jar: Arc<CookieJar>, proxy_url: Option<&str>) -> Self {
        Self::with_full_options(cookie_jar, proxy_url, false)
    }

    pub fn with_full_options(
        cookie_jar: Arc<CookieJar>,
        proxy_url: Option<&str>,
        allow_private_network: bool,
    ) -> Self {
        let unsupported_proxy = proxy_url
            .filter(|proxy| proxy_is_socks(proxy))
            .map(str::to_string);

        let emulation_opts = wreq_util::Emulation::builder()
            .profile(wreq_util::Profile::Chrome145)
            .platform(wreq_util::Platform::Windows)
            .build();

        let mut builder = wreq::Client::builder()
            .emulation(emulation_opts)
            .timeout(default_timeout())
            .redirect(wreq::redirect::Policy::none());

        if custom_cert_store_requested(
            std::env::var_os("SSL_CERT_FILE").as_deref(),
            std::env::var_os("SSL_CERT_DIR").as_deref(),
        ) {
            match load_custom_cert_store() {
                Ok(store) => builder = builder.tls_cert_store(store),
                Err(error) => tracing::warn!(
                    %error,
                    "SSL_CERT_FILE/SSL_CERT_DIR set but the certificate store failed to build; \
                     continuing with the default roots"
                ),
            }
        }

        if unsupported_proxy.is_none() {
            if let Some(proxy) = proxy_url {
                match wreq::Proxy::all(proxy) {
                    Ok(p) => builder = builder.proxy(p),
                    Err(error) => tracing::warn!(%error, proxy, "invalid HTTP(S) proxy URL"),
                }
            }
        }

        let client = builder.build().expect("failed to build wreq HTTP client");

        HttpClient {
            client,
            proxy_url: proxy_url.map(|s| s.to_string()),
            unsupported_proxy,
            cookie_jar,
            user_agent: RwLock::new(STEALTH_USER_AGENT.to_string()),
            extra_headers: RwLock::new(HashMap::new()),
            interceptor: RwLock::new(None),
            in_flight: Arc::new(AtomicU32::new(0)),
            timeout: default_timeout(),
            block_trackers: false,
            resource_loader: std::sync::Mutex::new(ResourceLoaderState::default()),
            allow_private_network,
        }
    }

    pub fn proxy_url(&self) -> Option<&str> {
        self.proxy_url.as_deref()
    }

    pub async fn fetch(&self, url: &Url) -> Result<Response, NetError> {
        self.fetch_with_callbacks(url, None).await
    }

    pub async fn fetch_with_callbacks(
        &self,
        url: &Url,
        callbacks: Option<&CallbackRegistry>,
    ) -> Result<Response, NetError> {
        self.fetch_with_profile(url, callbacks, ResourceRequest::navigation())
            .await
    }

    pub async fn post_form(&self, url: &Url, body: &str) -> Result<Response, NetError> {
        self.post_form_with_callbacks(url, body, None).await
    }

    pub async fn post_form_with_callbacks(
        &self,
        url: &Url,
        body: &str,
        callbacks: Option<&CallbackRegistry>,
    ) -> Result<Response, NetError> {
        let mut request = ResourceRequest::navigation();
        request.method = "POST".to_string();
        request.body = Some(body.as_bytes().to_vec());
        request.headers.insert(
            "content-type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        );
        self.fetch_with_profile(url, callbacks, request).await
    }

    pub async fn fetch_resource_with_callbacks(
        &self,
        url: &Url,
        request: ResourceRequest,
        callbacks: Option<&CallbackRegistry>,
    ) -> Result<Response, NetError> {
        self.fetch_with_profile(url, callbacks, request).await
    }

    async fn resource_cache_key(
        &self,
        url: &Url,
        request: &ResourceRequest,
    ) -> Option<ResourceCacheKey> {
        if !request.method.eq_ignore_ascii_case("GET")
            || request.body.is_some()
            || request.resource_type == ResourceType::Document
            || !matches!(url.scheme(), "http" | "https")
            || self.interceptor.read().await.is_some()
        {
            return None;
        }
        let mut extra_headers = self
            .extra_headers
            .read()
            .await
            .iter()
            .chain(request.headers.iter())
            .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
            .collect::<Vec<_>>();
        extra_headers.sort();
        extra_headers.dedup_by(|a, b| a.0 == b.0);
        if extra_headers.iter().any(|(name, value)| {
            name == "authorization"
                || name == "cookie"
                || (name == "cache-control"
                    && (value.to_ascii_lowercase().contains("no-cache")
                        || value.to_ascii_lowercase().contains("no-store")))
        }) {
            return None;
        }
        if request.sends_credentials_to(url) && !self.cookie_jar.get_cookie_header(url).is_empty() {
            return None;
        }
        Some(ResourceCacheKey {
            url: url.to_string(),
            resource_type: request.resource_type,
            mode: request.mode,
            credentials: request.credentials,
            initiator: request.initiator.as_ref().map(ToString::to_string),
            referrer: request.referrer.as_ref().map(ToString::to_string),
            user_agent: self.user_agent.read().await.clone(),
            extra_headers,
            max_response_bytes: request.max_response_bytes,
        })
    }

    async fn fetch_with_profile(
        &self,
        url: &Url,
        callbacks: Option<&CallbackRegistry>,
        request: ResourceRequest,
    ) -> Result<Response, NetError> {
        if let Some(proxy) = &self.unsupported_proxy {
            return Err(NetError::UnsupportedProxy {
                proxy: proxy.clone(),
            });
        }

        let Some(cache_key) = self.resource_cache_key(url, &request).await else {
            return self
                .fetch_with_profile_uncached(url, callbacks, request)
                .await;
        };

        enum Acquisition {
            Cached(Response),
            Follower(watch::Receiver<Option<SharedFetchOutcome>>),
            Leader(crate::cache::SharedFetchSender),
        }

        let acquisition = {
            let mut loader = self.resource_loader.lock().unwrap();
            if let Some(response) = loader.cache.get(&cache_key) {
                Acquisition::Cached(response)
            } else if let Some(sender) = loader.shared_fetches.get(&cache_key) {
                Acquisition::Follower(sender.subscribe())
            } else {
                let (sender, _receiver) = watch::channel(None);
                loader
                    .shared_fetches
                    .insert(cache_key.clone(), sender.clone());
                Acquisition::Leader(sender)
            }
        };

        match acquisition {
            Acquisition::Cached(response) => {
                self.fire_logical_resource_callbacks(callbacks, url, &request, &response)
                    .await;
                Ok(response)
            }
            Acquisition::Follower(mut receiver) => loop {
                let outcome = { receiver.borrow().clone() };
                if let Some(outcome) = outcome {
                    break match outcome {
                        SharedFetchOutcome::Cacheable(response) => {
                            self.fire_logical_resource_callbacks(
                                callbacks, url, &request, &response,
                            )
                            .await;
                            Ok(response)
                        }
                        SharedFetchOutcome::RetryUncoalesced => {
                            self.fetch_with_profile_uncached(url, callbacks, request)
                                .await
                        }
                    };
                }
                if receiver.changed().await.is_err() {
                    break self
                        .fetch_with_profile_uncached(url, callbacks, request)
                        .await;
                }
            },
            Acquisition::Leader(sender) => {
                let leader = SharedFetchLeader {
                    loader: &self.resource_loader,
                    key: cache_key.clone(),
                    sender,
                    finished: false,
                };
                let result = self
                    .fetch_with_profile_uncached(url, callbacks, request)
                    .await;
                let shared_outcome = match &result {
                    Ok(response) => match response_cache_lifetime(response) {
                        Some(lifetime) => {
                            self.resource_loader.lock().unwrap().cache.insert(
                                cache_key,
                                response.clone(),
                                lifetime,
                            );
                            SharedFetchOutcome::Cacheable(response.clone())
                        }
                        None => SharedFetchOutcome::RetryUncoalesced,
                    },
                    Err(_) => SharedFetchOutcome::RetryUncoalesced,
                };
                leader.finish(shared_outcome);
                result
            }
        }
    }

    async fn fire_logical_resource_callbacks(
        &self,
        callbacks: Option<&CallbackRegistry>,
        url: &Url,
        request: &ResourceRequest,
        response: &Response,
    ) {
        let Some(callbacks) = callbacks else {
            return;
        };
        let request_info = RequestInfo {
            url: url.clone(),
            method: request.method.clone(),
            headers: self.extra_headers.read().await.clone(),
            resource_type: request.resource_type,
        };
        callbacks.fire_request(&request_info).await;
        callbacks.fire_response(&request_info, response).await;
    }

    async fn fetch_with_profile_uncached(
        &self,
        url: &Url,
        callbacks: Option<&CallbackRegistry>,
        request: ResourceRequest,
    ) -> Result<Response, NetError> {
        validate_url(url, self.allow_private_network)?;
        validate_request_mode(&request, url)?;

        if url.scheme() == "file" {
            return fetch_file_url(url, request.max_response_bytes).await;
        }

        let mut method = request.method.clone();
        let mut body = request.body.clone();
        if self.block_trackers {
            if let Some(host) = url.host_str() {
                if crate::blocklist::is_blocked(host) {
                    tracing::debug!("Blocked tracker: {}", url);
                    return Ok(Response {
                        status: 0,
                        url: url.clone(),
                        headers: HashMap::new(),
                        body: Vec::new(),
                        redirected_from: Vec::new(),
                    });
                }
            }
        }

        let mut current_url = url.clone();
        let mut redirects = Vec::new();
        let mut redirect_tainted = false;
        let mut request_callback_fired = false;

        for _ in 0..20 {
            validate_request_mode(&request, &current_url)?;
            let mut headers = self.extra_headers.read().await.clone();
            for (k, v) in &request.headers {
                headers.insert(k.to_lowercase(), v.clone());
            }
            let request_info = RequestInfo {
                url: current_url.clone(),
                method: method.clone(),
                headers: headers.clone(),
                resource_type: request.resource_type,
            };

            if let Some(interceptor) = self.interceptor.read().await.as_ref() {
                match interceptor.intercept(&request_info).await {
                    InterceptAction::Continue => {}
                    InterceptAction::Block => {
                        return Err(NetError::Blocked(current_url.to_string()));
                    }
                    InterceptAction::Fulfill(response) => {
                        return Ok(response);
                    }
                    InterceptAction::ModifyHeaders(modified) => {
                        let mut extra = self.extra_headers.write().await;
                        extra.extend(modified);
                    }
                }
            }

            if !request_callback_fired {
                if let Some(cbs) = callbacks {
                    cbs.fire_request(&request_info).await;
                }
                request_callback_fired = true;
            }

            let request_origin = serialized_request_origin(&request, redirect_tainted);
            if cors_required(&request, &current_url)
                && (!is_cors_simple_method(&method)
                    || headers.keys().any(|k| is_cors_unsafe_request_header(k)))
            {
                self.send_cors_preflight(&current_url, &method, &headers, &request_origin)
                    .await?;
            }

            let req_method = method
                .parse::<wreq::Method>()
                .map_err(|e| NetError::Network(format!("invalid method '{method}': {e}")))?;
            let mut req = self
                .client
                .request(req_method, current_url.as_str())
                .timeout(self.timeout)
                .header("accept", request.accept())
                .header("sec-fetch-site", request_fetch_site(&request, &current_url))
                .header("sec-fetch-mode", request.mode.header_value())
                .header("sec-fetch-dest", request.destination());
            if request.mode == RequestMode::Navigate {
                req = req
                    .header("upgrade-insecure-requests", "1")
                    .header("sec-fetch-user", "?1");
            }
            if let Some(referer) = request_referrer(&request, &current_url) {
                req = req.header("referer", referer);
            }

            let ua = self.user_agent.read().await.clone();
            if !headers.keys().any(|k| k.eq_ignore_ascii_case("user-agent")) {
                req = req.header("user-agent", ua);
            }

            let cookie_header = if request.sends_credentials_to(&current_url) {
                self.cookie_jar.get_cookie_header(&current_url)
            } else {
                String::new()
            };
            if !cookie_header.is_empty() {
                req = req.header("Cookie", &cookie_header);
            }

            for (k, v) in &headers {
                if k.eq_ignore_ascii_case("origin") {
                    continue;
                }
                req = req.header(k.as_str(), v.as_str());
            }
            if cors_required(&request, &current_url) {
                req = req.header("origin", &request_origin);
            }

            if let Some(ref b) = body {
                req = req.body(b.clone());
            }

            let in_flight = InFlightGuard::new(&self.in_flight);
            let resp = req.send().await.map_err(|e| {
                NetError::Network(format!("{}: {} (source: {:?})", current_url, e, e.source()))
            })?;

            let status = resp.status();
            validate_wreq_cors_response(&request, &current_url, &request_origin, resp.headers())?;

            if request.sends_credentials_to(&current_url) {
                for val in resp.headers().get_all("set-cookie") {
                    if let Ok(s) = val.to_str() {
                        self.cookie_jar.set_cookie(s, &current_url);
                    }
                }
            }

            let response_headers: HashMap<String, String> = resp
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.as_str().to_lowercase(),
                        v.to_str().unwrap_or("").to_string(),
                    )
                })
                .collect();

            if status.is_redirection() {
                if let Some(location) = resp.headers().get("location") {
                    let location_str = location
                        .to_str()
                        .map_err(|_| NetError::Network("Invalid redirect Location".into()))?;
                    let next_url = current_url
                        .join(location_str)
                        .map_err(|e| NetError::Network(format!("Invalid redirect URL: {}", e)))?;
                    validate_url(&next_url, self.allow_private_network)?;
                    validate_request_mode(&request, &next_url)?;
                    redirect_tainted |= redirect_taints_origin(&request, &current_url, &next_url);
                    redirects.push(current_url.clone());
                    current_url = next_url;
                    if status.as_u16() == 301 || status.as_u16() == 302 || status.as_u16() == 303 {
                        method = "GET".to_string();
                        body = None;
                    }
                    continue;
                }
            }

            let body_bytes =
                read_wreq_body_limited(resp, &current_url, request.max_response_bytes).await?;
            drop(in_flight);

            let response = Response {
                url: current_url,
                status: status.as_u16(),
                headers: response_headers,
                body: body_bytes,
                redirected_from: redirects,
            };
            if let Some(cbs) = callbacks {
                cbs.fire_response(&request_info, &response).await;
            }
            return Ok(response);
        }

        Err(NetError::TooManyRedirects(url.to_string()))
    }

    async fn send_cors_preflight(
        &self,
        url: &Url,
        method: &str,
        headers: &HashMap<String, String>,
        origin: &str,
    ) -> Result<(), NetError> {
        let requested_headers = headers.keys().cloned().collect::<Vec<_>>().join(", ");
        let mut req = self
            .client
            .request(wreq::Method::OPTIONS, url.as_str())
            .timeout(self.timeout)
            .header("origin", origin)
            .header("access-control-request-method", method);
        if !requested_headers.is_empty() {
            req = req.header("access-control-request-headers", requested_headers);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| NetError::Cors(format!("CORS preflight failed: {e}")))?;
        let allow_origin =
            wreq_response_header_value(resp.headers(), "access-control-allow-origin", url)?
                .unwrap_or("");
        let allow_credentials =
            wreq_response_header_value(resp.headers(), "access-control-allow-credentials", url)?
                .unwrap_or("");
        if origin != "null" && allow_origin != "*" && allow_origin != origin {
            return Err(NetError::Cors(format!(
                "CORS preflight: Origin '{origin}' not allowed by Access-Control-Allow-Origin '{allow_origin}'"
            )));
        }
        if allow_credentials == "true" && allow_origin == "*" {
            return Err(NetError::Cors(
                "CORS preflight: credentialed response cannot use Access-Control-Allow-Origin *"
                    .into(),
            ));
        }
        Ok(())
    }

    pub async fn set_user_agent(&self, ua: &str) {
        *self.user_agent.write().await = ua.to_string();
    }

    pub async fn set_extra_headers(&self, headers: HashMap<String, String>) {
        *self.extra_headers.write().await = headers;
    }

    pub fn active_requests(&self) -> u32 {
        self.in_flight.load(Ordering::Relaxed)
    }

    pub fn is_network_idle(&self) -> bool {
        self.active_requests() == 0
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::HttpClient;
    use crate::callbacks::CallbackRegistry;
    use crate::cookies::CookieJar;
    use crate::ssrf::custom_cert_store_requested;
    use crate::types::{NetError, RequestCredentials, RequestMode, ResourceRequest, ResourceType};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use url::Url;

    async fn http_fixture(
        responses: Vec<String>,
    ) -> (Url, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                loop {
                    let Ok(read) = stream.read(&mut buffer).await else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = request_tx.send(String::from_utf8_lossy(&request).into_owned());
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (
            Url::parse(&format!("http://{address}/resource")).unwrap(),
            request_rx,
        )
    }

    fn ok_response(headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn cross_origin_font_sends_origin_and_omits_cross_origin_cookies() {
        let (target, mut received) = http_fixture(vec![ok_response(
            "Access-Control-Allow-Origin: *\r\nSet-Cookie: rejected=1; Path=/\r\n",
            "font",
        )])
        .await;
        let initiator = Url::parse("http://127.0.0.1:1/page").unwrap();
        let jar = Arc::new(CookieJar::new());
        jar.set_cookie("seed=1; Path=/", &target);
        let client = HttpClient::with_full_options(jar.clone(), None, true);

        let response = client
            .fetch_resource_with_callbacks(
                &target,
                ResourceRequest::subresource(ResourceType::Font, &initiator),
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.body, b"font");
        let request = received.recv().await.unwrap().to_ascii_lowercase();
        assert!(request.contains("origin: http://127.0.0.1:1\r\n"));
        assert!(request.contains("sec-fetch-mode: cors\r\n"));
        assert!(request.contains("sec-fetch-dest: font\r\n"));
        assert!(!request.contains("cookie:"));
        assert_eq!(jar.get_cookie_header(&target), "seed=1");
    }

    #[tokio::test]
    async fn cross_origin_module_uses_cors_script_profile_without_credentials() {
        let (target, mut received) = http_fixture(vec![ok_response(
            "Access-Control-Allow-Origin: *\r\nSet-Cookie: rejected=1; Path=/\r\n",
            "export default 1;",
        )])
        .await;
        let initiator = Url::parse("http://127.0.0.1:1/page").unwrap();
        let importing_module = target.join("/parent.js").unwrap();
        let jar = Arc::new(CookieJar::new());
        jar.set_cookie("seed=1; Path=/", &target);
        let client = HttpClient::with_full_options(jar.clone(), None, true);

        let response = client
            .fetch_resource_with_callbacks(
                &target,
                ResourceRequest::module_script(&initiator, &importing_module),
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.body, b"export default 1;");
        let request = received.recv().await.unwrap().to_ascii_lowercase();
        assert!(request.contains("origin: http://127.0.0.1:1\r\n"));
        assert!(request.contains("sec-fetch-mode: cors\r\n"));
        assert!(request.contains("sec-fetch-dest: script\r\n"));
        assert!(request.contains(&format!("referer: {}\r\n", importing_module)));
        assert!(!request.contains("cookie:"));
        assert_eq!(jar.get_cookie_header(&target), "seed=1");
    }

    #[tokio::test]
    async fn credentialed_cors_rejects_wildcard_and_accepts_exact_origin() {
        let initiator = Url::parse("http://127.0.0.1:1/page").unwrap();
        let wildcard = ok_response(
            "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Credentials: true\r\n",
            "blocked",
        );
        let exact = ok_response(
            "Access-Control-Allow-Origin: http://127.0.0.1:1\r\nAccess-Control-Allow-Credentials: true\r\nSet-Cookie: accepted=1; Path=/\r\n",
            "allowed",
        );
        let (target, mut received) = http_fixture(vec![wildcard, exact]).await;
        let jar = Arc::new(CookieJar::new());
        jar.set_cookie("seed=1; Path=/", &target);
        let client = HttpClient::with_full_options(jar.clone(), None, true);
        let mut request = ResourceRequest::subresource(ResourceType::Image, &initiator);
        request.mode = RequestMode::Cors;
        request.credentials = RequestCredentials::Include;

        let error = client
            .fetch_resource_with_callbacks(&target, request.clone(), None)
            .await
            .unwrap_err();
        assert!(matches!(error, NetError::Cors(_)));
        client
            .fetch_resource_with_callbacks(&target, request, None)
            .await
            .unwrap();

        let first = received.recv().await.unwrap().to_ascii_lowercase();
        let second = received.recv().await.unwrap().to_ascii_lowercase();
        assert!(first.contains("cookie: seed=1\r\n"));
        assert!(second.contains("cookie: seed=1\r\n"));
        let cookies = jar.get_cookie_header(&target);
        assert!(cookies.contains("seed=1"));
        assert!(cookies.contains("accepted=1"));
    }

    #[tokio::test]
    async fn same_origin_font_needs_no_cors_header_and_sends_cookies() {
        let (target, mut received) = http_fixture(vec![ok_response("", "same")]).await;
        let mut initiator = target.clone();
        initiator.set_path("/page");
        let jar = Arc::new(CookieJar::new());
        jar.set_cookie("same=1; Path=/", &target);
        let client = HttpClient::with_full_options(jar, None, true);
        client
            .fetch_resource_with_callbacks(
                &target,
                ResourceRequest::subresource(ResourceType::Font, &initiator),
                None,
            )
            .await
            .unwrap();
        let request = received.recv().await.unwrap().to_ascii_lowercase();
        assert!(!request.contains("origin:"));
        assert!(request.contains("cookie: same=1\r\n"));
    }

    #[tokio::test]
    async fn response_limits_reject_content_length_and_streamed_overflow() {
        let advertised = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n";
        let chunked = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n4\r\nabcd\r\n4\r\nefgh\r\n0\r\n\r\n";
        let (target, _) = http_fixture(vec![advertised.to_string(), chunked.to_string()]).await;
        let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
        let initiator = target.clone();
        let request = ResourceRequest::subresource(ResourceType::Image, &initiator)
            .with_max_response_bytes(6);

        for _ in 0..2 {
            let error = client
                .fetch_resource_with_callbacks(&target, request.clone(), None)
                .await
                .unwrap_err();
            assert!(matches!(error, NetError::ResponseTooLarge { limit: 6, .. }));
            assert_eq!(client.active_requests(), 0);
        }
    }

    async fn hanging_fixture() -> (Url, tokio::sync::oneshot::Receiver<()>) {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0u8; 2048];
            let _ = stream.read(&mut buffer).await;
            let _ = started_tx.send(());
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        (
            Url::parse(&format!("http://{address}/hang")).unwrap(),
            started_rx,
        )
    }

    async fn cancelled_shared_fetch_fixture(
    ) -> (Url, tokio::sync::oneshot::Receiver<()>, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut first_stream = None;
            let mut started_tx = Some(started_tx);
            for index in 0..3 {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0u8; 2048];
                let _ = stream.read(&mut request).await;
                observed.fetch_add(1, Ordering::SeqCst);
                if index == 0 {
                    if let Some(started_tx) = started_tx.take() {
                        let _ = started_tx.send(());
                    }
                    // Hold the transport open until the leader task is
                    // cancelled. The next two connections prove both the
                    // waiting follower retry and a fresh cache leader work.
                    first_stream = Some(stream);
                    continue;
                }
                let body = "shared";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nCache-Control: public, max-age=3600\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
            drop(first_stream);
        });
        (
            Url::parse(&format!("http://{address}/shared.js")).unwrap(),
            started_rx,
            requests,
        )
    }

    #[tokio::test]
    async fn cancellation_returns_active_requests_to_zero() {
        let (target, started) = hanging_fixture().await;
        let client = Arc::new(HttpClient::with_full_options(
            Arc::new(CookieJar::new()),
            None,
            true,
        ));
        let task = tokio::spawn({
            let client = client.clone();
            async move { client.fetch(&target).await }
        });
        started.await.unwrap();
        assert_eq!(client.active_requests(), 1);
        task.abort();
        let _ = task.await;
        assert_eq!(client.active_requests(), 0);
    }

    #[tokio::test]
    async fn cancelled_shared_subresource_leader_wakes_follower_and_clears_slot() {
        let (target, started, network_requests) = cancelled_shared_fetch_fixture().await;
        let initiator = target.join("/page.html").unwrap();
        let request = ResourceRequest::subresource(ResourceType::Script, &initiator);
        let client = Arc::new(HttpClient::with_full_options(
            Arc::new(CookieJar::new()),
            None,
            true,
        ));

        let leader = tokio::spawn({
            let client = client.clone();
            let target = target.clone();
            let request = request.clone();
            async move {
                client
                    .fetch_resource_with_callbacks(&target, request, None)
                    .await
            }
        });
        started.await.unwrap();

        let follower = tokio::spawn({
            let client = client.clone();
            let target = target.clone();
            let request = request.clone();
            async move {
                client
                    .fetch_resource_with_callbacks(&target, request, None)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let follower_is_waiting = client
                    .resource_loader
                    .lock()
                    .unwrap()
                    .shared_fetches
                    .values()
                    .next()
                    .is_some_and(|sender| sender.receiver_count() > 0);
                if follower_is_waiting {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("follower did not join the shared fetch");

        leader.abort();
        let _ = leader.await;
        let response = tokio::time::timeout(Duration::from_secs(2), follower)
            .await
            .expect("follower remained blocked after leader cancellation")
            .unwrap()
            .unwrap();
        assert_eq!(response.body, b"shared");

        // The follower intentionally retried without populating the cache.
        // A subsequent request must be able to install a fresh leader, and
        // its successful response is then reusable.
        client
            .fetch_resource_with_callbacks(&target, request.clone(), None)
            .await
            .unwrap();
        client
            .fetch_resource_with_callbacks(&target, request, None)
            .await
            .unwrap();
        assert_eq!(network_requests.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn transport_timeout_returns_active_requests_to_zero() {
        let (target, started) = hanging_fixture().await;
        let mut client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
        client.timeout = std::time::Duration::from_millis(25);
        let fetch = client.fetch(&target);
        let (_, result) = tokio::join!(started, fetch);
        assert!(result.is_err());
        assert_eq!(client.active_requests(), 0);
    }

    #[tokio::test]
    async fn callbacks_fire_once_across_redirects() {
        let redirect = "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (target, _) = http_fixture(vec![redirect.to_string(), ok_response("", "done")]).await;
        let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
        let callbacks = CallbackRegistry::new();
        let requests = Arc::new(AtomicUsize::new(0));
        let responses = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        callbacks.add_request(Arc::new(move |_| {
            request_count.fetch_add(1, Ordering::SeqCst);
        }));
        let response_count = responses.clone();
        callbacks.add_response(Arc::new(move |_, _| {
            response_count.fetch_add(1, Ordering::SeqCst);
        }));

        client
            .fetch_with_callbacks(&target, Some(&callbacks))
            .await
            .unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert_eq!(responses.load(Ordering::SeqCst), 1);
    }

    async fn cacheable_resource_fixture(
        status: u16,
        headers: &'static str,
    ) -> (Url, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let observed = observed.clone();
                tokio::spawn(async move {
                    let mut request = [0u8; 2048];
                    let _ = stream.read(&mut request).await;
                    observed.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    let body = "globalThis.__sharedRuns=(globalThis.__sharedRuns||0)+1;";
                    let response = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
                        body.len(),
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (
            Url::parse(&format!("http://{address}/shared.js")).unwrap(),
            requests,
        )
    }

    #[tokio::test]
    async fn cacheable_identical_subresources_share_one_in_flight_request() {
        let (url, network_requests) = cacheable_resource_fixture(
            200,
            "Cache-Control: public, max-age=3600\r\nVary: Accept-Language\r\n",
        )
        .await;
        let initiator = url.join("/page.html").unwrap();
        let client = Arc::new(HttpClient::with_full_options(
            Arc::new(CookieJar::new()),
            None,
            true,
        ));
        let callbacks = Arc::new(CallbackRegistry::new());
        let callback_requests = Arc::new(AtomicUsize::new(0));
        let callback_responses = Arc::new(AtomicUsize::new(0));
        let observed_requests = callback_requests.clone();
        callbacks.add_request(Arc::new(move |_| {
            observed_requests.fetch_add(1, Ordering::SeqCst);
        }));
        let observed_responses = callback_responses.clone();
        callbacks.add_response(Arc::new(move |_, _| {
            observed_responses.fetch_add(1, Ordering::SeqCst);
        }));

        let mut fetches = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let client = client.clone();
            let callbacks = callbacks.clone();
            let url = url.clone();
            let request = ResourceRequest::subresource(ResourceType::Script, &initiator);
            fetches.spawn(async move {
                client
                    .fetch_resource_with_callbacks(&url, request, Some(&callbacks))
                    .await
                    .unwrap()
            });
        }
        let mut responses = Vec::new();
        while let Some(response) = fetches.join_next().await {
            responses.push(response.unwrap());
        }

        assert_eq!(responses.len(), 32);
        assert!(responses.iter().all(|response| response.status == 200));
        assert_eq!(network_requests.load(Ordering::SeqCst), 1);
        assert_eq!(callback_requests.load(Ordering::SeqCst), 32);
        assert_eq!(callback_responses.load(Ordering::SeqCst), 32);
    }

    #[tokio::test]
    async fn cacheable_identical_module_scripts_share_one_in_flight_request() {
        let (url, network_requests) =
            cacheable_resource_fixture(200, "Cache-Control: public, max-age=3600\r\n").await;
        let initiator = url.join("/app.js").unwrap();
        let client = Arc::new(HttpClient::with_full_options(
            Arc::new(CookieJar::new()),
            None,
            true,
        ));

        let mut fetches = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let client = client.clone();
            let url = url.clone();
            let request = ResourceRequest::module_script(&initiator, &initiator);
            fetches.spawn(async move {
                client
                    .fetch_resource_with_callbacks(&url, request, None)
                    .await
                    .unwrap()
            });
        }
        let mut responses = Vec::new();
        while let Some(response) = fetches.join_next().await {
            responses.push(response.unwrap());
        }

        assert_eq!(responses.len(), 16);
        assert!(responses.iter().all(|response| response.status == 200));
        assert_eq!(network_requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn distinct_subresource_urls_do_not_coalesce() {
        let (url, network_requests) =
            cacheable_resource_fixture(200, "Cache-Control: public, max-age=3600\r\n").await;
        let initiator = url.join("/page.html").unwrap();
        let client = Arc::new(HttpClient::with_full_options(
            Arc::new(CookieJar::new()),
            None,
            true,
        ));

        let mut fetches = tokio::task::JoinSet::new();
        for index in 0..24 {
            let client = client.clone();
            let url = url.join(&format!("/distinct/{index}.js")).unwrap();
            let request = ResourceRequest::subresource(ResourceType::Script, &initiator);
            fetches.spawn(async move {
                client
                    .fetch_resource_with_callbacks(&url, request, None)
                    .await
                    .unwrap()
            });
        }
        let mut responses = Vec::new();
        while let Some(response) = fetches.join_next().await {
            responses.push(response.unwrap());
        }

        assert_eq!(responses.len(), 24);
        assert_eq!(network_requests.load(Ordering::SeqCst), 24);
    }

    #[tokio::test]
    async fn no_store_vary_star_and_error_responses_are_not_reused() {
        for (status, headers) in [
            (200, "Cache-Control: no-store\r\n"),
            (200, "Cache-Control: public, max-age=3600\r\nVary: *\r\n"),
            (500, "Cache-Control: public, max-age=3600\r\n"),
        ] {
            let (url, network_requests) = cacheable_resource_fixture(status, headers).await;
            let initiator = url.join("/page.html").unwrap();
            let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
            let request = ResourceRequest::subresource(ResourceType::Script, &initiator);
            client
                .fetch_resource_with_callbacks(&url, request.clone(), None)
                .await
                .unwrap();
            client
                .fetch_resource_with_callbacks(&url, request, None)
                .await
                .unwrap();
            assert_eq!(
                network_requests.load(Ordering::SeqCst),
                2,
                "status={status} headers={headers:?}",
            );
        }
    }

    #[tokio::test]
    async fn authorization_and_cookie_bearing_requests_bypass_resource_cache() {
        for header in [
            ("Authorization", "Bearer secret"),
            ("Cookie", "session=secret"),
        ] {
            let (url, network_requests) =
                cacheable_resource_fixture(200, "Cache-Control: public, max-age=3600\r\n").await;
            let initiator = url.join("/page.html").unwrap();
            let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
            client
                .set_extra_headers(HashMap::from([(
                    header.0.to_string(),
                    header.1.to_string(),
                )]))
                .await;
            let request = ResourceRequest::subresource(ResourceType::Script, &initiator);
            client
                .fetch_resource_with_callbacks(&url, request.clone(), None)
                .await
                .unwrap();
            client
                .fetch_resource_with_callbacks(&url, request, None)
                .await
                .unwrap();
            assert_eq!(
                network_requests.load(Ordering::SeqCst),
                2,
                "header={header:?}"
            );
        }
    }

    #[tokio::test]
    async fn socks_proxy_fails_loudly_on_fetch() {
        let client = HttpClient::with_full_options(
            Arc::new(CookieJar::new()),
            Some("socks5://127.0.0.1:1080"),
            true,
        );
        let url = Url::parse("http://example.com/").unwrap();
        let err = client.fetch(&url).await.unwrap_err();
        assert!(
            matches!(err, NetError::UnsupportedProxy { ref proxy } if proxy == "socks5://127.0.0.1:1080"),
            "expected UnsupportedProxy, got {err}"
        );
    }
    const PLAIN_BODY: &str = "<!DOCTYPE html><html><body><p id=\"mark\">gzip ok</p></body></html>";

    // gzip (level 9) of PLAIN_BODY, hardcoded so the fixture needs no
    // compression dependency. A wrong byte fails the assert below.
    const GZIP_BODY: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0xb3, 0x51, 0x74, 0xf1, 0x77,
        0x0e, 0x89, 0x0c, 0x70, 0x55, 0xc8, 0x28, 0xc9, 0xcd, 0xb1, 0xb3, 0x81, 0x90, 0x49, 0xf9,
        0x29, 0x95, 0x76, 0x36, 0x05, 0x0a, 0x99, 0x29, 0xb6, 0x4a, 0xb9, 0x89, 0x45, 0xd9, 0x4a,
        0x76, 0xe9, 0x55, 0x99, 0x05, 0x0a, 0xf9, 0xd9, 0x36, 0xfa, 0x05, 0x76, 0x36, 0xfa, 0x10,
        0x69, 0x7d, 0xb0, 0x5a, 0x00, 0x80, 0x3d, 0x1c, 0x5f, 0x41, 0x00, 0x00, 0x00,
    ];

    /// Serve one `Content-Encoding: gzip` response on an ephemeral port.
    async fn gzip_fixture() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-encoding: gzip\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        GZIP_BODY.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(GZIP_BODY).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        port
    }

    // The emulation profile advertises gzip, so origins compress. Without the
    // decoder the raw gzip bytes reach the HTML parser as document text.
    #[tokio::test]
    async fn http_client_decodes_gzip_response() {
        let port = gzip_fixture().await;
        let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();

        let resp = client.fetch(&url).await.expect("fixture must be reachable");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.text(), PLAIN_BODY, "gzip body must be decompressed");
    }

    #[tokio::test]
    async fn http_client_blocks_loopback_unless_private_network_is_allowed() {
        if crate::ssrf::env_allows_private_network() {
            return;
        }
        let port = gzip_fixture().await;
        let url = Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();

        let blocked = HttpClient::new();
        let err = blocked
            .fetch(&url)
            .await
            .expect_err("loopback must be SSRF-blocked by default");
        assert!(
            err.to_string().contains("not allowed"),
            "default HTTP client must refuse 127.0.0.1: {err}"
        );

        let allowed = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
        let resp = allowed
            .fetch(&url)
            .await
            .expect("allow_private_network must reach loopback");
        assert_eq!(resp.status, 200);
    }
    /// Mint a throwaway CA plus a 127.0.0.1 leaf it signed, and serve one
    /// canned HTTPS response with the leaf on an ephemeral port. Returns the
    /// port and the CA certificate as PEM.
    async fn https_fixture_with_private_ca() -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf_params = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();

        let certs = vec![tokio_rustls::rustls::pki_types::CertificateDer::from(
            leaf_cert.der().to_vec(),
        )];
        let key = tokio_rustls::rustls::pki_types::PrivateKeyDer::Pkcs8(
            tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer::from(leaf_key.serialize_der()),
        );
        let config = tokio_rustls::rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(stream).await else {
                        return; // Handshake rejection is the point of one test.
                    };
                    let mut buf = [0u8; 1024];
                    let _ = tls.read(&mut buf).await;
                    let body = "private ca ok";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = tls.write_all(resp.as_bytes()).await;
                    let _ = tls.shutdown().await;
                });
            }
        });

        (port, ca_cert.pem())
    }

    // The two configured-roots tests set/rely on SSL_CERT_FILE, which is
    // cached once per process at client build. They are only correct under
    // `cargo nextest` (one process per test), the same constraint the whole
    // workspace already has.

    #[test]
    fn ssl_cert_file_loads_into_the_custom_store() {
        let ca_file = tempfile::NamedTempFile::new().unwrap();
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        std::fs::write(ca_file.path(), ca_cert.pem()).unwrap();
        std::env::set_var("SSL_CERT_FILE", ca_file.path());
        super::load_custom_cert_store().expect("SSL_CERT_FILE PEM must parse into a CertStore");
    }

    #[test]
    fn ssl_cert_dir_loads_into_the_custom_store() {
        let ca_dir = tempfile::tempdir().unwrap();
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        std::fs::write(ca_dir.path().join("private-ca.pem"), ca_cert.pem()).unwrap();
        std::env::set_var("SSL_CERT_DIR", ca_dir.path());
        super::load_custom_cert_store().expect("SSL_CERT_DIR PEM must parse into a CertStore");
    }

    #[tokio::test]
    async fn private_ca_is_still_rejected_without_ssl_cert_file() {
        // The same fixture that the SSL_CERT_FILE test trusts must fail here. The
        // listener is reachable (same setup), so an Err can only be TLS.
        let (port, _ca_pem) = https_fixture_with_private_ca().await;
        let client = HttpClient::with_full_options(Arc::new(CookieJar::new()), None, true);
        let url = Url::parse(&format!("https://localhost:{port}/")).unwrap();
        assert!(
            client.fetch(&url).await.is_err(),
            "unknown CA must be rejected"
        );
    }

    #[test]
    fn empty_ssl_cert_env_is_treated_as_unset() {
        use std::ffi::OsStr;
        assert!(!custom_cert_store_requested(Some(OsStr::new("")), None));
        assert!(!custom_cert_store_requested(None, Some(OsStr::new(""))));
        assert!(!custom_cert_store_requested(
            Some(OsStr::new("")),
            Some(OsStr::new(""))
        ));
        assert!(!custom_cert_store_requested(None, None));
        assert!(custom_cert_store_requested(
            Some(OsStr::new("/etc/corp/ca.pem")),
            None
        ));
        assert!(custom_cert_store_requested(
            None,
            Some(OsStr::new("/etc/ssl/certs"))
        ));
    }
}
