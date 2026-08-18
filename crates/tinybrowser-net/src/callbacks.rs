use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::types::{RequestInfo, Response};

pub type RequestCallback = Arc<dyn Fn(&RequestInfo) + Send + Sync>;
pub type ResponseCallback = Arc<dyn Fn(&RequestInfo, &Response) + Send + Sync>;

/// Page-scoped store for the passive on_request/on_response callbacks (issue
/// #408). Each `Page` owns one, so a callback never fires for another page's
/// requests and dies with its page. The HTTP client itself stays
/// callback-free; page-driven fetches pass the page's registry in. Ids keep
/// the `u64` shape #416 established on `Page::on_request`/`on_response`.
///
/// `alive` is the page-lifetime tombstone: `Page::drop` clears it, so an
/// in-flight fetch task still holding the `Arc` after the page is dropped
/// stops delivering (issue #408 delivery-after-drop race).
pub struct CallbackRegistry {
    on_request: RwLock<Vec<(u64, RequestCallback)>>,
    on_response: RwLock<Vec<(u64, ResponseCallback)>>,
    id_counter: std::sync::atomic::AtomicU64,
    alive: AtomicBool,
}

impl CallbackRegistry {
    pub fn new() -> Self {
        CallbackRegistry {
            on_request: RwLock::new(Vec::new()),
            on_response: RwLock::new(Vec::new()),
            id_counter: std::sync::atomic::AtomicU64::new(1),
            alive: AtomicBool::new(true),
        }
    }

    fn next_id(&self) -> u64 {
        self.id_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    pub fn add_request(&self, cb: RequestCallback) -> u64 {
        let id = self.next_id();
        if let Ok(mut v) = self.on_request.try_write() {
            v.push((id, cb));
        }
        id
    }

    pub fn add_response(&self, cb: ResponseCallback) -> u64 {
        let id = self.next_id();
        if let Ok(mut v) = self.on_response.try_write() {
            v.push((id, cb));
        }
        id
    }

    pub fn remove_request(&self, id: u64) -> bool {
        match self.on_request.try_write() {
            Ok(mut v) => {
                let before = v.len();
                v.retain(|(cid, _)| *cid != id);
                v.len() != before
            }
            Err(_) => false,
        }
    }

    pub fn remove_response(&self, id: u64) -> bool {
        match self.on_response.try_write() {
            Ok(mut v) => {
                let before = v.len();
                v.retain(|(cid, _)| *cid != id);
                v.len() != before
            }
            Err(_) => false,
        }
    }

    pub async fn has_request_callbacks(&self) -> bool {
        !self.on_request.read().await.is_empty()
    }

    pub async fn has_response_callbacks(&self) -> bool {
        !self.on_response.read().await.is_empty()
    }

    pub fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }

    pub async fn fire_request(&self, info: &RequestInfo) {
        if !self.alive.load(Ordering::SeqCst) {
            return;
        }
        for (_, cb) in self.on_request.read().await.iter() {
            if !self.alive.load(Ordering::SeqCst) {
                return;
            }
            cb(info);
        }
    }

    pub async fn fire_response(&self, info: &RequestInfo, resp: &Response) {
        if !self.alive.load(Ordering::SeqCst) {
            return;
        }
        for (_, cb) in self.on_response.read().await.iter() {
            if !self.alive.load(Ordering::SeqCst) {
                return;
            }
            cb(info, resp);
        }
    }
}

impl Default for CallbackRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::CallbackRegistry;
    use crate::types::{RequestInfo, ResourceType};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use url::Url;

    fn info() -> RequestInfo {
        RequestInfo {
            url: Url::parse("https://app.example/").unwrap(),
            method: "GET".into(),
            headers: HashMap::new(),
            resource_type: ResourceType::Document,
        }
    }

    #[tokio::test]
    async fn add_remove_and_kill_gate_callback_delivery() {
        let registry = CallbackRegistry::new();
        let fired = Arc::new(AtomicUsize::new(0));
        let id = registry.add_request({
            let fired = Arc::clone(&fired);
            Arc::new(move |_| {
                fired.fetch_add(1, Ordering::SeqCst);
            })
        });

        registry.fire_request(&info()).await;
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        assert!(registry.remove_request(id));
        registry.fire_request(&info()).await;
        assert_eq!(fired.load(Ordering::SeqCst), 1);

        let _id = registry.add_request({
            let fired = Arc::clone(&fired);
            Arc::new(move |_| {
                fired.fetch_add(1, Ordering::SeqCst);
            })
        });
        registry.kill();
        registry.fire_request(&info()).await;
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }
}
