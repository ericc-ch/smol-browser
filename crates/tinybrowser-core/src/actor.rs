use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::lifecycle::WaitUntil;
use crate::page::{Page, PageError};

pub enum PageCommand {
    Evaluate {
        source: String,
        reply: oneshot::Sender<Value>,
    },
    Navigate {
        url: String,
        wait: WaitUntil,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Settle {
        budget_ms: u64,
        fixed: bool,
        reply: oneshot::Sender<()>,
    },
    Access(Box<dyn FnOnce(&mut Page) + Send>),
    Shutdown,
}

#[derive(Clone)]
pub struct PageActorHandle {
    tx: mpsc::UnboundedSender<PageCommand>,
}

impl PageActorHandle {
    pub async fn evaluate(&self, source: impl Into<String>) -> Value {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(PageCommand::Evaluate {
                source: source.into(),
                reply,
            })
            .is_err()
        {
            return Value::Null;
        }
        rx.await.unwrap_or(Value::Null)
    }

    pub async fn navigate(&self, url: impl Into<String>) -> Result<(), String> {
        self.navigate_with_wait(url, WaitUntil::Load).await
    }

    pub async fn navigate_with_wait(
        &self,
        url: impl Into<String>,
        wait: WaitUntil,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(PageCommand::Navigate {
                url: url.into(),
                wait,
                reply,
            })
            .is_err()
        {
            return Err("page actor stopped".into());
        }
        rx.await
            .unwrap_or_else(|_| Err("page actor stopped".into()))
    }

    pub async fn settle(&self, budget_ms: u64) {
        self.settle_inner(budget_ms, false).await;
    }

    pub async fn settle_for_duration(&self, budget_ms: u64) {
        self.settle_inner(budget_ms, true).await;
    }

    async fn settle_inner(&self, budget_ms: u64, fixed: bool) {
        let (reply, rx) = oneshot::channel();
        if self
            .tx
            .send(PageCommand::Settle {
                budget_ms,
                fixed,
                reply,
            })
            .is_ok()
        {
            let _ = rx.await;
        }
    }

    pub async fn with_page<R, F>(&self, f: F) -> Option<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Page) -> R + Send + 'static,
    {
        let (reply, rx) = oneshot::channel();
        let sent = self.tx.send(PageCommand::Access(Box::new(move |page| {
            let _ = reply.send(f(page));
        })));
        if sent.is_err() {
            return None;
        }
        rx.await.ok()
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(PageCommand::Shutdown);
    }
}

/// Run one page's command loop on the current task. Spawn this on a `LocalSet`
/// so `QuickJsRuntime` (`!Send`) stays on that task.
pub async fn run_page_actor(mut page: Page, mut rx: mpsc::UnboundedReceiver<PageCommand>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            PageCommand::Evaluate { source, reply } => {
                let value = page.evaluate(&source);
                let _ = reply.send(value);
            }
            PageCommand::Navigate { url, wait, reply } => {
                let result = page
                    .navigate_with_wait(&url, wait)
                    .await
                    .map_err(|e: PageError| e.to_string());
                let _ = reply.send(result);
            }
            PageCommand::Settle {
                budget_ms,
                fixed,
                reply,
            } => {
                if fixed {
                    page.settle_for_duration(budget_ms).await;
                } else {
                    page.settle(budget_ms).await;
                }
                let _ = reply.send(());
            }
            PageCommand::Access(run) => run(&mut page),
            PageCommand::Shutdown => break,
        }
    }
}

pub fn page_actor_channel() -> (PageActorHandle, mpsc::UnboundedReceiver<PageCommand>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (PageActorHandle { tx }, rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn blank_page(name: &str) -> Page {
        let context = std::sync::Arc::new(crate::BrowserContext::with_storage_and_network(
            name.to_string(),
            None,
            None,
            None,
            true,
        ));
        let mut page = Page::new(name.to_string(), context);
        page.navigate_blank();
        page.init_js();
        page
    }

    #[tokio::test]
    async fn actor_evaluates_sequentially_on_one_page() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let page = blank_page("actor-eval");
                let (handle, rx) = page_actor_channel();
                tokio::task::spawn_local(run_page_actor(page, rx));
                assert_eq!(handle.evaluate("1 + 1").await, json!(2));
                handle.shutdown();
            })
            .await;
    }

    #[tokio::test]
    async fn sibling_pages_keep_independent_js_state() {
        let a = blank_page("tab-a");
        let b = blank_page("tab-b");
        let (handle_a, rx_a) = page_actor_channel();
        let (handle_b, rx_b) = page_actor_channel();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                tokio::task::spawn_local(run_page_actor(a, rx_a));
                tokio::task::spawn_local(run_page_actor(b, rx_b));
                handle_a.evaluate("globalThis.marker = 'A'").await;
                handle_b.evaluate("globalThis.marker = 'B'").await;
                assert_eq!(handle_a.evaluate("marker").await, json!("A"));
                assert_eq!(handle_b.evaluate("marker").await, json!("B"));
                handle_a.shutdown();
                handle_b.shutdown();
            })
            .await;
    }
}
