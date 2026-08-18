use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::types::{RequestCredentials, RequestMode, ResourceType, Response};

pub(crate) const RESOURCE_CACHE_MAX_ENTRIES: usize = 256;
pub(crate) const RESOURCE_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct ResourceCacheKey {
    pub url: String,
    pub resource_type: ResourceType,
    pub mode: RequestMode,
    pub credentials: RequestCredentials,
    pub initiator: Option<String>,
    pub referrer: Option<String>,
    pub user_agent: String,
    pub extra_headers: Vec<(String, String)>,
    pub max_response_bytes: usize,
}

#[derive(Clone)]
struct ResourceCacheEntry {
    response: Response,
    expires_at: Instant,
}

#[derive(Default)]
pub(crate) struct ResourceCache {
    entries: HashMap<ResourceCacheKey, ResourceCacheEntry>,
    insertion_order: VecDeque<ResourceCacheKey>,
    body_bytes: usize,
}

#[derive(Default)]
pub(crate) struct ResourceLoaderState {
    pub cache: ResourceCache,
    pub shared_fetches: HashMap<ResourceCacheKey, SharedFetchSender>,
}

#[derive(Clone)]
pub(crate) enum SharedFetchOutcome {
    Cacheable(Response),
    RetryUncoalesced,
}

pub(crate) type SharedFetchSender = watch::Sender<Option<SharedFetchOutcome>>;

pub(crate) struct SharedFetchLeader<'a> {
    pub loader: &'a Mutex<ResourceLoaderState>,
    pub key: ResourceCacheKey,
    pub sender: SharedFetchSender,
    pub finished: bool,
}

impl SharedFetchLeader<'_> {
    pub fn finish(mut self, outcome: SharedFetchOutcome) {
        self.loader.lock().unwrap().shared_fetches.remove(&self.key);
        let _ = self.sender.send(Some(outcome));
        self.finished = true;
    }
}

impl Drop for SharedFetchLeader<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.loader.lock().unwrap().shared_fetches.remove(&self.key);
        let _ = self.sender.send(Some(SharedFetchOutcome::RetryUncoalesced));
    }
}

impl ResourceCache {
    pub fn get(&mut self, key: &ResourceCacheKey) -> Option<Response> {
        let entry = self.entries.get(key)?;
        if entry.expires_at <= Instant::now() {
            let expired = self.entries.remove(key)?;
            self.body_bytes = self.body_bytes.saturating_sub(expired.response.body.len());
            return None;
        }
        Some(entry.response.clone())
    }

    pub fn insert(&mut self, key: ResourceCacheKey, response: Response, lifetime: Duration) {
        let response_bytes = response.body.len();
        if response_bytes > RESOURCE_CACHE_MAX_BYTES {
            return;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.body_bytes = self.body_bytes.saturating_sub(previous.response.body.len());
            self.insertion_order.retain(|queued| queued != &key);
        }
        while self.entries.len() >= RESOURCE_CACHE_MAX_ENTRIES
            || self.body_bytes.saturating_add(response_bytes) > RESOURCE_CACHE_MAX_BYTES
        {
            let Some(oldest) = self.insertion_order.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.body_bytes = self.body_bytes.saturating_sub(entry.response.body.len());
            }
        }
        self.body_bytes = self.body_bytes.saturating_add(response_bytes);
        self.insertion_order.push_back(key.clone());
        self.entries.insert(
            key,
            ResourceCacheEntry {
                response,
                expires_at: Instant::now() + lifetime,
            },
        );
    }
}

pub(crate) fn response_cache_lifetime(response: &Response) -> Option<Duration> {
    if !(200..300).contains(&response.status)
        || !response.redirected_from.is_empty()
        || response.header("set-cookie").is_some()
        || response
            .header("vary")
            .is_some_and(|vary| vary.split(',').any(|name| name.trim() == "*"))
    {
        return None;
    }
    let cache_control = response.header("cache-control")?;
    let mut max_age = None;
    for directive in cache_control.split(',').map(str::trim) {
        let lower = directive.to_ascii_lowercase();
        if lower == "no-store" || lower == "no-cache" {
            return None;
        }
        if let Some(value) = lower.strip_prefix("max-age=") {
            max_age = value.trim_matches('"').parse::<u64>().ok();
        }
    }
    max_age
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::response_cache_lifetime;
    use crate::types::Response;
    use std::time::Duration;
    use url::Url;

    fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Response {
        Response {
            url: Url::parse("https://app.example/asset.js").unwrap(),
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_vec(),
            redirected_from: Vec::new(),
        }
    }

    #[test]
    fn cache_lifetime_requires_fresh_success_without_cookies_or_star_vary() {
        assert_eq!(
            response_cache_lifetime(&response(200, &[("cache-control", "max-age=60")], b"ok")),
            Some(Duration::from_mins(1))
        );
        assert_eq!(
            response_cache_lifetime(&response(404, &[("cache-control", "max-age=60")], b"no")),
            None
        );
        assert_eq!(
            response_cache_lifetime(&response(200, &[("cache-control", "no-store")], b"ok")),
            None
        );
        let mut redirected = response(200, &[("cache-control", "max-age=60")], b"ok");
        redirected
            .redirected_from
            .push(Url::parse("https://app.example/old.js").unwrap());
        assert_eq!(response_cache_lifetime(&redirected), None);
        assert_eq!(
            response_cache_lifetime(&response(
                200,
                &[("cache-control", "max-age=60"), ("set-cookie", "a=1")],
                b"ok"
            )),
            None
        );
        assert_eq!(
            response_cache_lifetime(&response(
                200,
                &[("cache-control", "max-age=60"), ("vary", "*")],
                b"ok"
            )),
            None
        );
    }
}
