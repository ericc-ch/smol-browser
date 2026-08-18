use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rquickjs::{
    function::Rest,
    loader::{ImportAttributes, Loader, Resolver},
    CatchResultExt, Context, Ctx, Function, Module, Object, Persistent, Promise, Runtime,
    TypedArray,
};

use tinybrowser_dom::DomTree;

use crate::module_loader::{self, ModuleLoadActivity};
use crate::ops::{self, SharedState};

const SHIM: &str = include_str!("../js/bootstrap.js");

fn delay_duration(delay_ms: f64) -> Duration {
    if !delay_ms.is_finite() || delay_ms <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64((delay_ms / 1000.0).min(86_400.0))
}

struct ScheduledTimer {
    deadline: Instant,
    callback: Persistent<Function<'static>>,
}

/// Host-side timer and posted-task queues. Callbacks and promise resolvers
/// are held with `Persistent::save` so the maps are not tied to a `Ctx`
/// lifetime. Dropped before the `Context` (struct field order).
struct HostLoop {
    next_timer_id: u32,
    timers: HashMap<u32, ScheduledTimer>,
    posted: VecDeque<Persistent<Function<'static>>>,
    next_fetch_id: u64,
    fetch_resolvers: HashMap<u64, (Persistent<Function<'static>>, Persistent<Function<'static>>)>,
}

impl HostLoop {
    fn new() -> Self {
        Self {
            next_timer_id: 0,
            timers: HashMap::new(),
            posted: VecDeque::new(),
            next_fetch_id: 0,
            fetch_resolvers: HashMap::new(),
        }
    }

    fn queue_timer<'js>(&mut self, ctx: &Ctx<'js>, delay_ms: f64, callback: Function<'js>) -> u32 {
        self.next_timer_id = self.next_timer_id.wrapping_add(1);
        if self.next_timer_id == 0 {
            self.next_timer_id = 1;
        }
        let id = self.next_timer_id;
        self.timers.insert(
            id,
            ScheduledTimer {
                deadline: Instant::now() + delay_duration(delay_ms),
                callback: Persistent::save(ctx, callback),
            },
        );
        id
    }

    fn cancel_timer(&mut self, id: u32) {
        self.timers.remove(&id);
    }

    fn take_due(&mut self, now: Instant) -> Vec<Persistent<Function<'static>>> {
        let mut ids: Vec<u32> = self
            .timers
            .iter()
            .filter(|(_, timer)| timer.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(|id| self.timers.remove(&id).map(|t| t.callback))
            .collect()
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.timers.values().map(|t| t.deadline).min()
    }

    fn has_due(&self, now: Instant) -> bool {
        self.timers.values().any(|t| t.deadline <= now)
    }

    fn push_posted(&mut self, resolve: Persistent<Function<'static>>) {
        self.posted.push_back(resolve);
    }

    fn pop_posted(&mut self) -> Option<Persistent<Function<'static>>> {
        self.posted.pop_front()
    }

    fn has_posted(&self) -> bool {
        !self.posted.is_empty()
    }

    fn push_fetch(
        &mut self,
        resolve: Persistent<Function<'static>>,
        reject: Persistent<Function<'static>>,
    ) -> u64 {
        self.next_fetch_id = self.next_fetch_id.wrapping_add(1);
        if self.next_fetch_id == 0 {
            self.next_fetch_id = 1;
        }
        let id = self.next_fetch_id;
        self.fetch_resolvers.insert(id, (resolve, reject));
        id
    }

    fn take_fetch(
        &mut self,
        id: u64,
    ) -> Option<(Persistent<Function<'static>>, Persistent<Function<'static>>)> {
        self.fetch_resolvers.remove(&id)
    }

    fn has_fetch(&self) -> bool {
        !self.fetch_resolvers.is_empty()
    }
}

struct FetchWork {
    id: u64,
    job: ops::FetchJob,
}

struct FetchReply {
    id: u64,
    result: Result<ops::FetchOutcome, String>,
}

enum NetworkWork {
    Fetch(FetchWork),
    Module {
        url: String,
        document_url: String,
        referrer: String,
        client: std::sync::Arc<tinybrowser_net::HttpClient>,
        callbacks: Option<std::sync::Arc<tinybrowser_net::CallbackRegistry>>,
        reply: std::sync::mpsc::Sender<Result<(String, String), String>>,
    },
}

fn spawn_network_thread(
    work_rx: std::sync::mpsc::Receiver<NetworkWork>,
    reply_tx: std::sync::mpsc::Sender<FetchReply>,
) {
    std::thread::Builder::new()
        .name("tinybrowser-js-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("network tokio runtime");
            let (async_tx, mut async_rx) = tokio::sync::mpsc::unbounded_channel();
            std::thread::Builder::new()
                .name("tinybrowser-js-net-recv".into())
                .spawn(move || {
                    while let Ok(work) = work_rx.recv() {
                        if async_tx.send(work).is_err() {
                            break;
                        }
                    }
                })
                .expect("spawn tinybrowser-js network recv thread");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                while let Some(work) = async_rx.recv().await {
                    match work {
                        NetworkWork::Fetch(work) => {
                            let reply_tx = reply_tx.clone();
                            tokio::task::spawn_local(async move {
                                let result = ops::run_fetch_job(work.job).await;
                                let _ = reply_tx.send(FetchReply {
                                    id: work.id,
                                    result,
                                });
                            });
                        }
                        NetworkWork::Module {
                            url,
                            document_url,
                            referrer,
                            client,
                            callbacks,
                            reply,
                        } => {
                            tokio::task::spawn_local(async move {
                                let parsed = url::Url::parse(&url);
                                let document = url::Url::parse(&document_url);
                                let referrer_url = url::Url::parse(&referrer);
                                let result = match (parsed, document, referrer_url) {
                                    (Ok(url), Ok(document), Ok(referrer)) => {
                                        module_loader::fetch_module_bytes(
                                            &client,
                                            &url,
                                            &document,
                                            &referrer,
                                            callbacks.as_deref(),
                                        )
                                        .await
                                    }
                                    _ => Err(format!("Invalid module URL {url}")),
                                };
                                let _ = reply.send(result);
                            });
                        }
                    }
                }
            });
        })
        .expect("spawn tinybrowser-js network thread");
}

struct QjsModuleHost {
    state: std::rc::Weak<RefCell<ops::RuntimeState>>,
    fetch_tx: std::sync::mpsc::Sender<NetworkWork>,
    referrers: HashMap<String, String>,
    activity: Arc<ModuleLoadActivity>,
}

struct QjsModuleResolver(Rc<RefCell<QjsModuleHost>>);
struct QjsModuleSourceLoader(Rc<RefCell<QjsModuleHost>>);

impl Resolver for QjsModuleResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        let host = self.0.borrow();
        let state = host.state.upgrade().ok_or_else(|| {
            rquickjs::Error::new_resolving_message(
                base,
                name,
                "module loader page state was dropped",
            )
        })?;
        let document_base = state.borrow().url.clone();
        let import_map_cell = state.borrow().import_map.clone();
        let mut import_map = import_map_cell.try_borrow_mut().map_err(|_| {
            rquickjs::Error::new_resolving_message(base, name, "Import map is already borrowed")
        })?;
        let resolved =
            module_loader::resolve_module_specifier(&mut import_map, name, base, &document_base)
                .map_err(|e| rquickjs::Error::new_resolving_message(base, name, e))?;
        drop(import_map);
        drop(state);
        drop(host);
        let resolved = resolved.to_string();
        self.0
            .borrow_mut()
            .referrers
            .insert(resolved.clone(), base.to_string());
        Ok(resolved)
    }
}

impl Loader for QjsModuleSourceLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js>> {
        let source = self
            .load_source(name)
            .map_err(|e| rquickjs::Error::new_loading_message(name, e))?;
        Module::declare(ctx.clone(), name, source)
    }
}

impl QjsModuleSourceLoader {
    fn load_source(&self, name: &str) -> Result<String, String> {
        let host = self.0.borrow();
        let _activity = host.activity.begin();
        let state = host
            .state
            .upgrade()
            .ok_or_else(|| "Module loader page state was dropped".to_string())?;
        let (client, callbacks, document_url) = {
            let gs = state
                .try_borrow()
                .map_err(|_| "Module loader page state is already borrowed".to_string())?;
            let client = gs
                .http_client
                .clone()
                .ok_or_else(|| "No http_client wired to module loader".to_string())?;
            (client, gs.callbacks.clone(), gs.url.clone())
        };
        let referrer = host
            .referrers
            .get(name)
            .cloned()
            .unwrap_or_else(|| document_url.clone());
        let fetch_tx = host.fetch_tx.clone();
        drop(host);

        request_module_source(
            &fetch_tx,
            name,
            document_url,
            referrer,
            client,
            callbacks,
            ops::fetch_timeout(),
        )
    }
}

fn request_module_source(
    fetch_tx: &std::sync::mpsc::Sender<NetworkWork>,
    url: &str,
    document_url: String,
    referrer: String,
    client: std::sync::Arc<tinybrowser_net::HttpClient>,
    callbacks: Option<std::sync::Arc<tinybrowser_net::CallbackRegistry>>,
    budget: Duration,
) -> Result<String, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid module URL {url}: {e}"))?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    fetch_tx
        .send(NetworkWork::Module {
            url: parsed.to_string(),
            document_url,
            referrer,
            client,
            callbacks,
            reply: reply_tx,
        })
        .map_err(|_| "network thread closed".to_string())?;
    match reply_rx.recv_timeout(budget) {
        Ok(result) => result.map(|(_final_url, code)| code),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "Module graph load timed out after {}ms: {url}",
            budget.as_millis()
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err("module fetch reply closed".to_string())
        }
    }
}

/// Panic landing pad for every QuickJS op closure: a panic must not unwind
/// through QuickJS's C frames (mirror of the V8 `op_dom` catch in ops.rs).
/// The panic is logged and the caller gets a default value.
fn guarded<R: Default>(f: impl FnOnce() -> R) -> R {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
        tracing::error!("QuickJS op panicked; returning default");
        R::default()
    })
}

/// Fallible ops return a JS exception on panic instead of unwinding.
fn guarded_result<T>(f: impl FnOnce() -> Result<T, rquickjs::Error>) -> Result<T, rquickjs::Error> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(_) => {
            tracing::error!("QuickJS op panicked; returning error");
            Err(op_err("op panicked"))
        }
    }
}

/// Convert a fallible op's error into a JS exception, matching the V8 path
/// where `deno_error::JsErrorBox` becomes a thrown error the shim's `runOp`
/// turns into the appropriate DOMException.
fn op_err(e: impl std::fmt::Display) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("tinybrowser op", "operation", e.to_string())
}

/// QuickJS runtime behind [`crate::runtime::JsRuntime`].
pub struct QuickJsRuntime {
    // Persistents in `host` must drop while the context is still alive.
    // Context must drop before Runtime. Fields drop in declaration order.
    host: Rc<RefCell<HostLoop>>,
    context: Context,
    // Kept so Context drops before Runtime (struct field order).
    #[allow(dead_code)]
    runtime: Runtime,
    state: SharedState,
    fetch_tx: std::sync::mpsc::Sender<NetworkWork>,
    fetch_rx: std::sync::mpsc::Receiver<FetchReply>,
    interrupt: Arc<AtomicBool>,
    activity: Arc<ModuleLoadActivity>,
    inline_seq: std::cell::Cell<u64>,
}

impl QuickJsRuntime {
    pub fn new() -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|e| e.to_string())?;
        let interrupt = Arc::new(AtomicBool::new(false));
        {
            let interrupt = interrupt.clone();
            runtime.set_interrupt_handler(Some(Box::new(move || interrupt.load(Ordering::SeqCst))));
        }
        let context = Context::full(&runtime).map_err(|e| e.to_string())?;
        let state = Rc::new(RefCell::new(ops::RuntimeState::new()));
        let host = Rc::new(RefCell::new(HostLoop::new()));
        let (fetch_tx, work_rx) = std::sync::mpsc::channel();
        let (reply_tx, fetch_rx) = std::sync::mpsc::channel();
        spawn_network_thread(work_rx, reply_tx);
        let activity = Arc::new(ModuleLoadActivity::default());
        let module_host = Rc::new(RefCell::new(QjsModuleHost {
            state: Rc::downgrade(&state),
            fetch_tx: fetch_tx.clone(),
            referrers: HashMap::new(),
            activity: activity.clone(),
        }));
        runtime.set_loader(
            QjsModuleResolver(module_host.clone()),
            QjsModuleSourceLoader(module_host),
        );
        let this = Self {
            host,
            context,
            runtime,
            state,
            fetch_tx,
            fetch_rx,
            interrupt,
            activity,
            inline_seq: std::cell::Cell::new(0),
        };
        this.install_ops()?;
        Ok(this)
    }

    fn with_ctx<F, R>(&self, f: F) -> R
    where
        F: FnOnce(Ctx<'_>) -> R,
    {
        let state = self.state.clone();
        self.context
            .with(|ctx| crate::dom_bindings::enter_shared(state.clone(), || f(ctx)))
    }

    fn install_ops(&self) -> Result<(), String> {
        self.with_ctx(|ctx| self.install_ops_in(ctx))
    }

    fn install_ops_in<'js>(&self, ctx: Ctx<'js>) -> Result<(), String> {
        let ops = Object::new(ctx.clone()).map_err(|e| e.to_string())?;
        macro_rules! set_op {
            ($name:literal, $f:expr) => {
                ops.set(
                    $name,
                    Function::new(ctx.clone(), $f).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?
            };
        }

        let s = self.state.clone();
        set_op!("op_dom", move |cmd: String,
                                arg1: String,
                                arg2: String|
              -> String {
            guarded(|| ops::op_dom_inner(&s, cmd, arg1, arg2))
        });

        let s = self.state.clone();
        set_op!("op_script_mark_started", move |nid: f64| -> bool {
            guarded(|| ops::op_script_mark_started_inner(&s, nid as u64))
        });

        let s = self.state.clone();
        set_op!("op_script_try_start", move |nid: f64| -> bool {
            guarded(|| ops::op_script_try_start_inner(&s, nid as u64))
        });

        let s = self.state.clone();
        set_op!("op_shadow_attach", move |host_nid: f64,
                                          mode: String|
              -> i64 {
            guarded(|| ops::op_shadow_attach_inner(&s, host_nid as u64, mode))
        });

        let s = self.state.clone();
        set_op!("op_shadow_root_info", move |host_nid: f64| -> String {
            guarded(|| ops::op_shadow_root_info_inner(&s, host_nid as u64))
        });

        set_op!("op_console_msg", |level: String, msg: String| {
            guarded(|| match level.as_str() {
                "warn" => tracing::warn!(target: "tinybrowser::console", "{}", msg),
                "error" => tracing::error!(target: "tinybrowser::console", "{}", msg),
                _ => tracing::info!(target: "tinybrowser::console", "{}", msg),
            })
        });

        let s = self.state.clone();
        set_op!("op_get_cookies", move || -> String {
            guarded(|| ops::op_get_cookies_inner(&s))
        });

        let s = self.state.clone();
        set_op!("op_set_cookie", move |cookie_str: String| {
            guarded(|| ops::op_set_cookie_inner(&s, &cookie_str))
        });

        let s = self.state.clone();
        set_op!(
            "op_navigate",
            move |url: String, method: String, body: String| {
                guarded(|| ops::op_navigate_inner(&s, &url, &method, &body))
            }
        );

        set_op!("op_async_runtime_available", || -> bool { true });

        let host_posted = self.host.clone();
        set_op!("op_posted_task", move |ctx: Ctx<'js>| -> Result<
            Promise<'js>,
            rquickjs::Error,
        > {
            let (promise, resolve, _reject) = Promise::new(&ctx)?;
            host_posted
                .borrow_mut()
                .push_posted(Persistent::save(&ctx, resolve));
            Ok(promise)
        });

        let fetch_state = self.state.clone();
        let fetch_host = self.host.clone();
        let fetch_tx = self.fetch_tx.clone();
        set_op!("op_fetch_url", move |ctx: Ctx<'js>,
                                      args: Rest<String>|
              -> Result<
            Promise<'js>,
            rquickjs::Error,
        > {
            let get = |i: usize| args.0.get(i).cloned().unwrap_or_default();
            let (promise, resolve, reject) = Promise::new(&ctx)?;
            let mut gs = fetch_state.borrow_mut();
            match ops::start_fetch(
                &mut gs,
                get(0),
                get(1),
                get(2),
                get(3),
                get(4),
                get(5),
                get(6),
            ) {
                ops::FetchStart::Immediate(outcome) => {
                    let json = ops::apply_fetch_outcome(&mut gs, outcome);
                    drop(gs);
                    resolve
                        .call::<(String,), ()>((json,))
                        .catch(&ctx)
                        .map_err(|e| {
                            rquickjs::Error::new_from_js_message(
                                "tinybrowser op",
                                "operation",
                                e.to_string(),
                            )
                        })?;
                }
                ops::FetchStart::Pending(job) => {
                    drop(gs);
                    let id = fetch_host.borrow_mut().push_fetch(
                        Persistent::save(&ctx, resolve),
                        Persistent::save(&ctx, reject),
                    );
                    if fetch_tx
                        .send(NetworkWork::Fetch(FetchWork { id, job }))
                        .is_err()
                    {
                        let Some((_, reject)) = fetch_host.borrow_mut().take_fetch(id) else {
                            return Ok(promise);
                        };
                        let reject = reject.restore(&ctx).map_err(|e| {
                            rquickjs::Error::new_from_js_message(
                                "tinybrowser op",
                                "operation",
                                e.to_string(),
                            )
                        })?;
                        reject
                            .call::<(String,), ()>(("network thread closed".into(),))
                            .catch(&ctx)
                            .map_err(|e| {
                                rquickjs::Error::new_from_js_message(
                                    "tinybrowser op",
                                    "operation",
                                    e.to_string(),
                                )
                            })?;
                    }
                }
            }
            Ok(promise)
        });

        let s = self.state.clone();
        set_op!("op_binding_called", move |name: String, payload: String| {
            guarded(|| ops::op_binding_called_inner(&s, &name, &payload))
        });

        let s = self.state.clone();
        set_op!("op_add_import_map", move |source: String,
                                           base_url: String|
              -> String {
            guarded(|| ops::op_add_import_map_inner(&s, source, base_url))
        });

        set_op!("op_subtle_digest", |algorithm: String,
                                     data: TypedArray<'_, u8>|
         -> Vec<u8> {
            guarded(|| {
                let bytes = data.as_bytes().unwrap_or(&[]);
                ops::subtle_digest(&algorithm, bytes)
            })
        });

        set_op!(
            "op_subtle_hmac",
            |hash: String,
             key: TypedArray<'_, u8>,
             data: TypedArray<'_, u8>|
             -> Result<Vec<u8>, rquickjs::Error> {
                guarded_result(|| {
                    let key = key.as_bytes().unwrap_or(&[]);
                    let data = data.as_bytes().unwrap_or(&[]);
                    ops::subtle_hmac(&hash, key, data).map_err(op_err)
                })
            }
        );

        set_op!("op_subtle_aes_gcm", |encrypt: bool,
                                      key: TypedArray<'_, u8>,
                                      iv: TypedArray<'_, u8>,
                                      aad: TypedArray<'_, u8>,
                                      data: TypedArray<'_, u8>|
         -> Result<
            Vec<u8>,
            rquickjs::Error,
        > {
            guarded_result(|| {
                let key = key.as_bytes().unwrap_or(&[]);
                let iv = iv.as_bytes().unwrap_or(&[]);
                let aad = aad.as_bytes().unwrap_or(&[]);
                let data = data.as_bytes().unwrap_or(&[]);
                ops::subtle_aes_gcm(encrypt, key, iv, aad, data).map_err(op_err)
            })
        });

        set_op!("op_subtle_aes_cbc", |encrypt: bool,
                                      key: TypedArray<'_, u8>,
                                      iv: TypedArray<'_, u8>,
                                      data: TypedArray<'_, u8>|
         -> Result<
            Vec<u8>,
            rquickjs::Error,
        > {
            guarded_result(|| {
                let key = key.as_bytes().unwrap_or(&[]);
                let iv = iv.as_bytes().unwrap_or(&[]);
                let data = data.as_bytes().unwrap_or(&[]);
                ops::subtle_aes_cbc(encrypt, key, iv, data).map_err(op_err)
            })
        });

        set_op!("op_subtle_aes_ctr", |key: TypedArray<'_, u8>,
                                      counter: TypedArray<'_, u8>,
                                      counter_length: u32,
                                      data: TypedArray<'_, u8>|
         -> Result<
            Vec<u8>,
            rquickjs::Error,
        > {
            guarded_result(|| {
                let key = key.as_bytes().unwrap_or(&[]);
                let counter = counter.as_bytes().unwrap_or(&[]);
                let data = data.as_bytes().unwrap_or(&[]);
                ops::subtle_aes_ctr(key, counter, counter_length, data).map_err(op_err)
            })
        });

        set_op!(
            "op_subtle_pbkdf2",
            |hash: String,
             password: TypedArray<'_, u8>,
             salt: TypedArray<'_, u8>,
             iterations: u32,
             len: u32|
             -> Result<Vec<u8>, rquickjs::Error> {
                guarded_result(|| {
                    let password = password.as_bytes().unwrap_or(&[]);
                    let salt = salt.as_bytes().unwrap_or(&[]);
                    ops::subtle_pbkdf2(&hash, password, salt, iterations, len).map_err(op_err)
                })
            }
        );

        set_op!(
            "op_subtle_hkdf",
            |hash: String,
             ikm: TypedArray<'_, u8>,
             salt: TypedArray<'_, u8>,
             info: TypedArray<'_, u8>,
             len: u32|
             -> Result<Vec<u8>, rquickjs::Error> {
                guarded_result(|| {
                    let ikm = ikm.as_bytes().unwrap_or(&[]);
                    let salt = salt.as_bytes().unwrap_or(&[]);
                    let info = info.as_bytes().unwrap_or(&[]);
                    ops::subtle_hkdf(&hash, ikm, salt, info, len).map_err(op_err)
                })
            }
        );

        set_op!(
            "op_random_bytes",
            |len: u32| -> Result<Vec<u8>, rquickjs::Error> {
                guarded_result(|| ops::random_bytes(len).map_err(op_err))
            }
        );

        set_op!("op_url_parse", |href: String, base: String| -> String {
            guarded(|| ops::url_parse(&href, &base))
        });

        set_op!("op_url_set", |href: String,
                               part: String,
                               value: String|
         -> String {
            guarded(|| ops::url_set(&href, &part, &value))
        });

        set_op!("op_url_resolve", |href: String, base: String| -> String {
            guarded(|| ops::url_resolve(&href, &base))
        });

        set_op!("op_document_domain_candidate", |current: String,
                                                 input: String|
         -> String {
            guarded(|| ops::document_domain_candidate(&current, &input))
        });

        set_op!("op_encoding_for_label", |label: String| -> String {
            guarded(|| ops::encoding_for_label(&label))
        });

        set_op!("op_text_decode", |label: String,
                                   bytes: TypedArray<'_, u8>,
                                   fatal: bool,
                                   ignore_bom: bool|
         -> String {
            let bytes = bytes.as_bytes().unwrap_or(&[]);
            guarded(|| ops::text_decode(&label, bytes, fatal, ignore_bom))
        });

        set_op!("op_url_encode_query", |query: String,
                                        label: String,
                                        special: bool|
         -> String {
            guarded(|| ops::url_encode_query(&query, &label, special))
        });

        // Any op the shim probes that is not registered here (the whole
        // render family) falls back to the no-op stub, so bootstrap.js
        // boots unmodified on no-render builds.
        crate::dom_bindings::register_dom_classes(&ctx, self.state.clone())?;

        ctx.globals()
            .set("__tinybrowser_ops", ops)
            .map_err(|e| e.to_string())?;
        // Host stays off `window` after boot. The shim IIFE receives it as
        // `host` / local `Deno`; page script never sees `globalThis.Deno`.
        ctx.eval::<(), _>(
            r#"
            globalThis.__tbHost = { core: { ops: __tinybrowser_ops } };
            delete globalThis.__tinybrowser_ops;
            "#,
        )
        .catch(&ctx)
        .map_err(|e| e.to_string())?;

        let host: Object = ctx.globals().get("__tbHost").map_err(|e| e.to_string())?;
        let wrap_state = self.state.clone();
        host.set(
            "wrapNode",
            Function::new(ctx.clone(), move |ctx: Ctx<'js>, nid: f64| {
                crate::dom_bindings::wrap_node(
                    ctx,
                    wrap_state.clone(),
                    tinybrowser_dom::NodeId::from_raw(nid as u64),
                )
                .ok()
            }),
        )
        .map_err(|e| e.to_string())?;
        let core: Object = host.get("core").map_err(|e| e.to_string())?;

        let host_timer = self.host.clone();
        core.set(
            "queueUserTimer",
            Function::new(
                ctx.clone(),
                move |ctx: Ctx<'js>,
                      _depth: f64,
                      _repeat: bool,
                      delay: f64,
                      callback: Function<'js>|
                      -> u32 {
                    host_timer.borrow_mut().queue_timer(&ctx, delay, callback)
                },
            )
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

        let host_cancel = self.host.clone();
        core.set(
            "cancelTimer",
            Function::new(ctx.clone(), move |id: u32| {
                host_cancel.borrow_mut().cancel_timer(id);
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

        Ok(())
    }

    pub fn set_dom(&self, dom: DomTree) {
        let mut gs = self.state.borrow_mut();
        gs.dom = Some(dom);
        gs.document_generation = gs.document_generation.wrapping_add(1);
        gs.activity_generation = 0;
        gs.page_in_flight = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        gs.already_started_scripts.borrow_mut().clear();
    }

    pub fn set_url(&self, url: &str) {
        let mut state = self.state.borrow_mut();
        if state.url != url {
            state.url = url.to_string();
        }
    }

    pub fn set_title(&self, title: &str) {
        self.state.borrow_mut().title = title.to_string();
    }

    pub fn set_http_client(&self, client: std::sync::Arc<tinybrowser_net::HttpClient>) {
        self.state.borrow_mut().http_client = Some(client);
    }

    pub fn set_intercept_tx(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<ops::InterceptedRequest>,
    ) {
        self.state.borrow_mut().intercept_tx = Some(tx);
    }

    pub fn set_intercept_enabled(&self, enabled: bool) {
        self.state.borrow_mut().intercept_enabled = enabled;
    }

    pub(crate) fn shared_state(&self) -> &SharedState {
        &self.state
    }

    pub fn run_page_init(&mut self) -> Result<(), String> {
        self.with_ctx(|ctx| {
            ctx.eval::<(), _>("globalThis.__tinybrowser_init();")
                .catch(&ctx)
                .map_err(|e| format!("__tinybrowser_init failed: {e}"))?;
            while ctx.execute_pending_job() {}
            Ok(())
        })
    }

    pub fn evaluate(&mut self, expression: &str) -> Result<serde_json::Value, String> {
        self.eval_wrapped(&Self::wrap_expression(expression))
    }

    pub(crate) fn module_load_activity(&self) -> Arc<ModuleLoadActivity> {
        self.activity.clone()
    }

    pub fn eval_module(
        &mut self,
        name: &str,
        source: &str,
        import_meta_url: Option<&str>,
        budget_ms: u64,
    ) -> Result<(), String> {
        let module_name = if import_meta_url.is_some() {
            let id = self.inline_seq.get().wrapping_add(1);
            self.inline_seq.set(id);
            format!("{name}#inline-{id}")
        } else {
            name.to_string()
        };
        let import_meta = import_meta_url.unwrap_or(name).to_string();
        self.with_ctx(|ctx| {
            let module = Module::declare(ctx.clone(), module_name.as_str(), source)
                .catch(&ctx)
                .map_err(|e| format!("load: {e}"))?;
            if let Ok(meta) = module.meta() {
                let _ = meta.set("url", import_meta.as_str());
            }
            let (_evaluated, promise) = module.eval().catch(&ctx).map_err(|e| format!("eval: {e}"))?;
            ctx.globals()
                .set("__tinybrowser_mod_p", promise)
                .map_err(|e| e.to_string())?;
            ctx.eval::<(), _>(
                r#"
                globalThis.__tinybrowser_mod_state = 0;
                globalThis.__tinybrowser_mod_err = null;
                globalThis.__tinybrowser_mod_p.then(
                    function() { globalThis.__tinybrowser_mod_state = 1; },
                    function(e) {
                        globalThis.__tinybrowser_mod_state = 2;
                        globalThis.__tinybrowser_mod_err = (e && e.message) ? String(e.message) : String(e);
                    }
                );
                delete globalThis.__tinybrowser_mod_p;
                "#,
            )
            .catch(&ctx)
            .map_err(|e| format!("eval: {e}"))?;
            while ctx.execute_pending_job() {}
            Ok::<(), String>(())
        })?;

        let budget_end = Instant::now() + Duration::from_millis(budget_ms.max(1));
        loop {
            self.drain_jobs()?;
            let state = self.with_ctx(|ctx| {
                ctx.eval::<i32, _>("globalThis.__tinybrowser_mod_state|0")
                    .unwrap_or(0)
            });
            if state == 1 {
                return Ok(());
            }
            if state == 2 {
                let err = self.with_ctx(|ctx| {
                    ctx.eval::<String, _>(
                        "String(globalThis.__tinybrowser_mod_err || 'module error')",
                    )
                    .unwrap_or_else(|_| "module error".to_string())
                });
                return Err(format!("eval: {err}"));
            }
            let now = Instant::now();
            if now >= budget_end {
                return Err("timeout".into());
            }
            let slice = budget_end
                .saturating_duration_since(now)
                .as_millis()
                .min(20) as u64;
            self.run_event_loop_bounded(slice)?;
        }
    }

    pub fn fetch_module_source(&mut self, url: &str, budget: Duration) -> Result<String, String> {
        let _activity = self.activity.begin();
        let (client, callbacks, document_url) = {
            let gs = self.state.borrow();
            let client = gs
                .http_client
                .clone()
                .ok_or_else(|| "No http_client wired to module loader".to_string())?;
            (client, gs.callbacks.clone(), gs.url.clone())
        };
        request_module_source(
            &self.fetch_tx,
            url,
            document_url.clone(),
            document_url,
            client,
            callbacks,
            budget,
        )
    }

    pub fn execute_script(&mut self, name: &str, source: impl AsRef<str>) -> Result<(), String> {
        let source = source.as_ref();
        let filename = if name.is_empty() {
            "eval_script".to_string()
        } else {
            name.to_string()
        };
        self.with_ctx(|ctx| {
            let mut opts = rquickjs::context::EvalOptions::default();
            opts.global = true;
            opts.filename = Some(filename);
            ctx.eval_with_options::<(), _>(source, opts)
                .catch(&ctx)
                .map_err(|e| format!("JS error: {e}"))?;
            // Do not drain jobs here. `import()` is a pending job whose
            // loader fetch is synchronous; draining it would turn a dynamic
            // import into an implicit navigation settle.
            Ok(())
        })
    }

    pub fn execute_script_guarded(&mut self, name: &str, source: &str) -> Result<(), String> {
        if source.len() < 10_000 {
            self.execute_script(name, source)
        } else {
            let token = self.arm_watchdog(Duration::from_secs(5));
            let result = self.execute_script(name, source);
            let fired = self.disarm_watchdog(token);
            match result {
                Ok(()) => Ok(()),
                Err(_) if fired => Ok(()),
                other => other,
            }
        }
    }

    fn eval_wrapped(&mut self, wrapped: &str) -> Result<serde_json::Value, String> {
        self.with_ctx(|ctx| {
            // JSON.stringify(undefined) is undefined, not a string. Statements
            // like document.write() complete with undefined; coerce that to
            // null so the result is always JSON.
            let script = format!(
                "(function(){{ var __v = ({wrapped}); if (__v === undefined) return \"null\"; var __s = JSON.stringify(__v); return (__s === undefined) ? \"null\" : __s; }})()"
            );
            let raw: Result<String, _> = ctx.eval(script.as_str()).catch(&ctx);
            let raw = match raw {
                Ok(s) => s,
                Err(e) => return Err(e.to_string()),
            };
            match serde_json::from_str(&raw) {
                Ok(v) => Ok(v),
                Err(_) => Ok(serde_json::Value::String(raw)),
            }
        })
    }

    fn wrap_expression(expression: &str) -> String {
        let trimmed = expression.trim();
        let is_multi_statement = trimmed.starts_with("var ")
            || trimmed.starts_with("let ")
            || trimmed.starts_with("const ")
            || trimmed.starts_with("if ")
            || trimmed.starts_with("for ")
            || trimmed.starts_with("while ")
            || trimmed.starts_with("return ");
        if is_multi_statement {
            format!(
                "(function() {{ try {{\n{}\n}} catch(e) {{ return null; }} }})()",
                expression
            )
        } else {
            let cleaned = trimmed.trim_end_matches(|c: char| c == ';' || c.is_whitespace());
            format!(
                "(function() {{ try {{ return (\n{}\n); }} catch(e) {{ return null; }} }})()",
                cleaned
            )
        }
    }

    /// Drive already-due timers, posted-task wakes, and pending jobs for at
    /// most `budget_ms`. No tokio: the isolate thread sleeps with
    /// `thread::sleep` until the next timer or the budget ends.
    pub fn run_event_loop_bounded(&mut self, budget_ms: u64) -> Result<(), String> {
        let budget_end = Instant::now() + Duration::from_millis(budget_ms);
        loop {
            self.drain_jobs()?;
            let fetched = self.drain_fetch()?;
            let fired = self.fire_due_timers(Instant::now())?;
            let posted = self.resolve_one_posted()?;
            self.drain_jobs()?;

            let now = Instant::now();
            if now >= budget_end {
                return Ok(());
            }
            if fetched
                || fired
                || posted
                || self.host.borrow().has_posted()
                || self.host.borrow().has_due(now)
            {
                continue;
            }
            let fetch_wait = self.host.borrow().has_fetch();
            let timer_deadline = self.host.borrow().next_deadline();
            if !fetch_wait && timer_deadline.is_none() && !self.runtime.is_job_pending() {
                return Ok(());
            }
            let wait_until = match (fetch_wait, timer_deadline) {
                (true, Some(d)) => d.min(budget_end),
                (true, None) => budget_end,
                (false, Some(d)) => {
                    if d > budget_end {
                        return Ok(());
                    }
                    d
                }
                (false, None) => return Ok(()),
            };
            let now = Instant::now();
            if wait_until <= now {
                continue;
            }
            if fetch_wait {
                match self.fetch_rx.recv_timeout(wait_until - now) {
                    Ok(reply) => self.complete_fetch(reply)?,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                }
            } else {
                std::thread::sleep(wait_until - now);
            }
        }
    }

    fn drain_jobs(&mut self) -> Result<(), String> {
        self.with_ctx(|ctx| {
            while ctx.execute_pending_job() {}
            Ok::<(), String>(())
        })
    }

    pub fn drain_pending_jobs(&mut self) -> Result<(), String> {
        self.drain_jobs()
    }

    fn drain_fetch(&mut self) -> Result<bool, String> {
        let mut any = false;
        while let Ok(reply) = self.fetch_rx.try_recv() {
            any = true;
            self.complete_fetch(reply)?;
        }
        Ok(any)
    }

    fn complete_fetch(&mut self, reply: FetchReply) -> Result<(), String> {
        let Some((resolve, reject)) = self.host.borrow_mut().take_fetch(reply.id) else {
            return Ok(());
        };
        match reply.result {
            Ok(outcome) => {
                let json = ops::apply_fetch_outcome(&mut self.state.borrow_mut(), outcome);
                self.with_ctx(|ctx| {
                    let func = resolve.restore(&ctx).map_err(|e| e.to_string())?;
                    func.call::<(String,), ()>((json,))
                        .catch(&ctx)
                        .map_err(|e| e.to_string())?;
                    Ok::<(), String>(())
                })
            }
            Err(err) => self.with_ctx(|ctx| {
                let func = reject.restore(&ctx).map_err(|e| e.to_string())?;
                func.call::<(String,), ()>((err,))
                    .catch(&ctx)
                    .map_err(|e| e.to_string())?;
                Ok::<(), String>(())
            }),
        }
    }

    fn fire_due_timers(&mut self, now: Instant) -> Result<bool, String> {
        let due = self.host.borrow_mut().take_due(now);
        if due.is_empty() {
            return Ok(false);
        }
        self.with_ctx(|ctx| {
            for callback in due {
                let func = callback.restore(&ctx).map_err(|e| e.to_string())?;
                let _ = func.call::<(), ()>(()).catch(&ctx);
            }
            Ok::<(), String>(())
        })?;
        Ok(true)
    }

    fn resolve_one_posted(&mut self) -> Result<bool, String> {
        let Some(resolve) = self.host.borrow_mut().pop_posted() else {
            return Ok(false);
        };
        self.with_ctx(|ctx| {
            let func = resolve.restore(&ctx).map_err(|e| e.to_string())?;
            func.call::<(), ()>(())
                .catch(&ctx)
                .map_err(|e| e.to_string())?;
            Ok::<(), String>(())
        })?;
        Ok(true)
    }

    pub fn load_shim(&mut self) -> Result<(), String> {
        self.with_ctx(|ctx| {
            // QuickJS-ng ships a native Performance whose timeOrigin is
            // read-only. The shim keeps that object (`performance || { ... }`)
            // and __tinybrowser_init then assigns timeOrigin. Drop the native
            // object so the shim's writable fallback is used. Do not edit the shim.
            ctx.eval::<(), _>(
                r#"
                delete globalThis.performance;
                try { delete globalThis.BroadcastChannel; } catch (e) {}
                globalThis.BroadcastChannel = undefined;
                "#,
            )
            .catch(&ctx)
            .map_err(|e| format!("clear native globals: {e}"))?;
            // QuickJS-ng does not ship WebAssembly. Install a minimal instantiate
            // before the shim so its streaming fallback can use Response.arrayBuffer.
            ctx.eval::<(), _>(
                r#"
                if (typeof WebAssembly === 'undefined') {
                  function Instance() {}
                  function Module() {}
                  globalThis.WebAssembly = {
                    Instance: Instance,
                    Module: Module,
                    instantiate: function(buf, imports) {
                      return Promise.resolve({ instance: new Instance(), module: new Module() });
                    }
                  };
                }
                "#,
            )
            .catch(&ctx)
            .map_err(|e| format!("wasm stub: {e}"))?;
            // quickjs-ng has no Intl. The obstacle-course fingerprint stage
            // (and Chrome-like probes) call DateTimeFormat().resolvedOptions().
            // Match the CLI default TZ (Europe/Berlin) so Date and Intl agree.
            let tz = std::env::var("TZ")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Europe/Berlin".to_string());
            let tz_js = serde_json::to_string(&tz).unwrap_or_else(|_| "\"Europe/Berlin\"".into());
            ctx.eval::<(), _>(format!(
                r#"
                if (typeof Intl === 'undefined' || typeof Intl.DateTimeFormat === 'undefined') {{
                  function DateTimeFormat() {{
                    if (!(this instanceof DateTimeFormat)) return new DateTimeFormat();
                  }}
                  DateTimeFormat.prototype.resolvedOptions = function() {{
                    return {{ timeZone: {tz_js} }};
                  }};
                  globalThis.Intl = {{ DateTimeFormat: DateTimeFormat }};
                }}
                "#
            ))
            .catch(&ctx)
            .map_err(|e| format!("intl timezone stub: {e}"))?;
            let host: Object = ctx
                .globals()
                .get("__tbHost")
                .map_err(|e| format!("boot host: {e}"))?;
            let boot: Function = ctx
                .eval(SHIM)
                .catch(&ctx)
                .map_err(|e| format!("shim eval failed: {e}"))?;
            boot.call::<_, ()>((host.clone(),))
                .catch(&ctx)
                .map_err(|e| format!("shim boot failed: {e}"))?;
            let patch: Function = ctx
                .eval(
                    r#"
                (function(Deno) {
                globalThis.__notifyMutation = function(type, target_nid, addedNodes, removedNodes, attributeName, oldValue) {
                  if (!globalThis.__mutationObservers.length) return;
                  const target = globalThis._wrap(target_nid);
                  if (!target) return;
                  const record = {
                    type: type,
                    target: target,
                    addedNodes: (addedNodes || []).map(nid => globalThis._wrap(nid)).filter(Boolean),
                    removedNodes: (removedNodes || []).map(nid => globalThis._wrap(nid)).filter(Boolean),
                    attributeName: attributeName || null,
                    oldValue: oldValue ?? null,
                    previousSibling: null,
                    nextSibling: null,
                  };
                  for (const obs of globalThis.__mutationObservers) {
                    let matched = false;
                    for (const t of obs._targets) {
                      const root = t.target;
                      if (!root) continue;
                      const wantsType =
                        (type === 'attributes' && t.options.attributes) ||
                        (type === 'characterData' && t.options.characterData) ||
                        (type === 'childList' && t.options.childList);
                      if (!wantsType) continue;
                      if (root._nid === target_nid) { matched = true; break; }
                      if (t.options.subtree && Deno.core.ops.op_dom("is_inclusive_ancestor", String(root._nid), String(target_nid)) === "true") {
                        matched = true;
                        break;
                      }
                    }
                    if (matched) obs._notify([record]);
                  }
                };
                })
                "#,
                )
                .catch(&ctx)
                .map_err(|e| format!("mutation observer ancestor patch: {e}"))?;
            patch
                .call::<_, ()>((host,))
                .catch(&ctx)
                .map_err(|e| format!("mutation observer ancestor patch: {e}"))?;
            #[cfg(not(test))]
            {
                ctx.eval::<(), _>("delete globalThis.__tbHost;")
                    .catch(&ctx)
                    .map_err(|e| format!("clear boot host: {e}"))?;
            }
            // QuickJS-ng invokes Error.prepareStackTrace inside the Error
            // constructor (V8 is lazy on .stack). Shadow the native accessor
            // so assignment is a JS property the constructor never reads.
            // console.log in the shim already nulls the hook around .stack.
            ctx.eval::<(), _>(
                r#"
                (function() {
                  var pst;
                  Object.defineProperty(Error, 'prepareStackTrace', {
                    configurable: true,
                    enumerable: false,
                    get: function() { return pst; },
                    set: function(v) { pst = v; }
                  });
                })();
                "#,
            )
            .catch(&ctx)
            .map_err(|e| format!("prepareStackTrace shadow: {e}"))?;
            Ok(())
        })
    }

    pub fn pump_ready(&mut self) -> Result<bool, String> {
        self.drain_jobs()?;
        let fetched = self.drain_fetch()?;
        let fired = self.fire_due_timers(Instant::now())?;
        let posted = self.resolve_one_posted()?;
        self.drain_jobs()?;
        Ok(fetched || fired || posted)
    }

    pub fn is_idle(&self) -> bool {
        if self.runtime.is_job_pending() {
            return false;
        }
        let host = self.host.borrow();
        host.timers.is_empty() && !host.has_posted() && !host.has_fetch()
    }

    pub fn next_wait_instant(&self) -> Option<Instant> {
        let host = self.host.borrow();
        let timer = host.next_deadline();
        if host.has_fetch() {
            let soon = Instant::now() + Duration::from_millis(5);
            Some(timer.map(|t| t.min(soon)).unwrap_or(soon))
        } else {
            timer
        }
    }

    pub fn isolate_handle(&self) -> QuickJsIsolateHandle {
        QuickJsIsolateHandle {
            interrupt: self.interrupt.clone(),
        }
    }

    pub fn arm_watchdog(&mut self, budget: Duration) -> QuickJsWatchdogToken {
        spawn_quickjs_watchdog(self.isolate_handle(), budget)
    }

    pub fn disarm_watchdog(&mut self, token: QuickJsWatchdogToken) -> bool {
        let fired = token.stop();
        if fired {
            self.cancel_termination();
            tracing::warn!("QuickJS watchdog fired: interrupted a synchronous overrun");
        }
        fired
    }

    pub fn cancel_termination(&mut self) {
        self.interrupt.store(false, Ordering::SeqCst);
    }
}

impl Drop for QuickJsRuntime {
    fn drop(&mut self) {
        // Persistents must be released while Context/Runtime are still alive.
        let mut host = self.host.borrow_mut();
        host.timers.clear();
        host.posted.clear();
        host.fetch_resolvers.clear();
    }
}

/// Cloneable handle the CDP watchdog can fire without borrowing the runtime.
#[derive(Clone)]
pub struct QuickJsIsolateHandle {
    interrupt: Arc<AtomicBool>,
}

impl QuickJsIsolateHandle {
    pub fn terminate_execution(&self) {
        self.interrupt.store(true, Ordering::SeqCst);
    }
}

pub struct QuickJsWatchdogToken {
    pair: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    join: Option<std::thread::JoinHandle<()>>,
    fired: Arc<AtomicBool>,
}

pub fn spawn_quickjs_watchdog(
    handle: QuickJsIsolateHandle,
    budget: Duration,
) -> QuickJsWatchdogToken {
    let pair = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let fired = Arc::new(AtomicBool::new(false));
    let pair_c = pair.clone();
    let fired_c = fired.clone();
    let join = std::thread::spawn(move || {
        let (lock, cvar) = &*pair_c;
        let mut cancelled = lock.lock().unwrap();
        let deadline = Instant::now() + budget;
        loop {
            if *cancelled {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                fired_c.store(true, Ordering::SeqCst);
                handle.terminate_execution();
                return;
            }
            let (guard, _) = cvar.wait_timeout(cancelled, remaining).unwrap();
            cancelled = guard;
            if *cancelled {
                return;
            }
        }
    });
    QuickJsWatchdogToken {
        pair,
        join: Some(join),
        fired,
    }
}

impl QuickJsWatchdogToken {
    pub fn stop(mut self) -> bool {
        {
            let (lock, cvar) = &*self.pair;
            *lock.lock().unwrap() = true;
            cvar.notify_one();
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.fired.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_at(html: &str, page_url: &str) -> QuickJsRuntime {
        let mut rt = QuickJsRuntime::new().expect("runtime constructs");
        rt.load_shim().expect("shim loads");
        rt.set_url(page_url);
        rt.set_dom(tinybrowser_dom::parse_html(html));
        rt.run_page_init().expect("page init runs");
        rt
    }

    fn setup(html: &str) -> QuickJsRuntime {
        setup_at(html, "http://example.com/test")
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
            rt.evaluate("typeof __tbHost.core.ops").expect("eval"),
            serde_json::json!("object")
        );
    }

    #[test]
    fn load_shim_exposes_window_and_document() {
        let mut rt = QuickJsRuntime::new().expect("runtime constructs");
        rt.load_shim().expect("shim loads");
        rt.run_page_init().expect("page init runs");
        assert_eq!(
            rt.evaluate("typeof window").expect("eval"),
            serde_json::json!("object")
        );
        assert_eq!(
            rt.evaluate("typeof document").expect("eval"),
            serde_json::json!("object")
        );
        let tz = std::env::var("TZ")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Europe/Berlin".to_string());
        assert_eq!(
            rt.evaluate("Intl.DateTimeFormat().resolvedOptions().timeZone")
                .expect("eval"),
            serde_json::json!(tz)
        );
    }

    #[test]
    fn deep_detached_append_child_chain_completes() {
        let mut rt = setup("<html><body></body></html>");
        let started = Instant::now();
        let value = rt
            .evaluate(
                r#"(() => {
                    const root = document.createElement('div');
                    let parent = root;
                    for (let i = 0; i < 5000; i++) {
                      const child = document.createElement('span');
                      parent.appendChild(child);
                      parent = child;
                    }
                    document.body.appendChild(root);
                    const walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
                    let count = 0;
                    while (walker.nextNode()) count++;
                    return count;
                })()"#,
            )
            .expect("eval");
        assert_eq!(value, serde_json::json!(5000));
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "deep appendChild chain took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn sync_dom_ops_round_trip_through_rust_dom() {
        let mut rt = setup("<html><body><div id='x'>hi</div><p class='y'>yo</p></body></html>");
        assert_eq!(
            rt.evaluate("document.createElement('div').tagName")
                .expect("eval"),
            serde_json::json!("DIV")
        );
        assert_eq!(
            rt.evaluate("document.getElementById('x').tagName")
                .expect("eval"),
            serde_json::json!("DIV")
        );
        assert_eq!(
            rt.evaluate("document.getElementById('x').textContent")
                .expect("eval"),
            serde_json::json!("hi")
        );
        assert_eq!(
            rt.evaluate("document.querySelector('#x').textContent")
                .expect("eval"),
            serde_json::json!("hi")
        );
        assert_eq!(
            rt.evaluate("document.querySelector('.y').textContent")
                .expect("eval"),
            serde_json::json!("yo")
        );
        // The element created in JS lives in the Rust tree: creating it and
        // then appending it round-trips through op_dom("append_child").
        assert_eq!(
            rt.evaluate(
                "(function(){ const el = document.createElement('span'); document.body.appendChild(el); return document.body.lastChild.tagName; })()"
            )
            .expect("eval"),
            serde_json::json!("SPAN")
        );
    }

    #[test]
    fn page_script_does_not_see_host_deno() {
        let mut rt = QuickJsRuntime::new().expect("runtime constructs");
        rt.load_shim().expect("shim loads");
        assert_eq!(
            rt.evaluate("typeof Deno").expect("eval"),
            serde_json::json!("undefined")
        );
        assert_eq!(
            rt.evaluate("typeof fetch").expect("eval"),
            serde_json::json!("function")
        );
    }

    #[test]
    fn location_href_does_not_switch_cookie_origin_before_commit() {
        let jar = std::sync::Arc::new(tinybrowser_net::CookieJar::new());
        jar.set_cookie(
            "secret=from-evil",
            &url::Url::parse("https://evil.test/").unwrap(),
        );
        jar.set_cookie(
            "session=victim",
            &url::Url::parse("https://victim.test/").unwrap(),
        );
        let mut rt = setup_at("<html><body></body></html>", "https://victim.test/");
        rt.shared_state().borrow_mut().cookie_jar = Some(jar);
        rt.evaluate("location.href = 'https://evil.test/stolen'")
            .expect("eval");
        assert_eq!(
            rt.evaluate("document.cookie").expect("eval"),
            serde_json::json!("session=victim")
        );
        assert!(rt.shared_state().borrow().pending_navigation.is_some());
    }

    #[test]
    fn cancel_timer_prevents_the_callback() {
        let mut rt = setup("<html><body></body></html>");
        rt.evaluate("(function(){ globalThis.__fired = false; var id = setTimeout(function(){ globalThis.__fired = true; }, 0); clearTimeout(id); return true; })()")
            .expect("eval");
        rt.run_event_loop_bounded(20).expect("pump");
        assert_eq!(
            rt.evaluate("globalThis.__fired").expect("eval"),
            serde_json::json!(false)
        );
    }

    #[test]
    fn posted_task_settles_on_a_later_turn() {
        let mut rt = QuickJsRuntime::new().expect("runtime constructs");
        assert_eq!(
            rt.evaluate("(function(){ globalThis.__settled = false; __tbHost.core.ops.op_posted_task().then(function(){ globalThis.__settled = true; }); return globalThis.__settled; })()")
                .expect("eval"),
            serde_json::json!(false)
        );
        // evaluate drains microtasks. If the op resolved in the same turn,
        // the then-handler would already have run.
        assert_eq!(
            rt.evaluate("globalThis.__settled").expect("eval"),
            serde_json::json!(false)
        );
        rt.run_event_loop_bounded(20).expect("pump");
        assert_eq!(
            rt.evaluate("globalThis.__settled").expect("eval"),
            serde_json::json!(true)
        );
    }

    #[test]
    fn stateless_ops_round_trip() {
        let mut rt = QuickJsRuntime::new().expect("runtime constructs");
        assert_eq!(
            rt.evaluate(
                "JSON.parse(__tbHost.core.ops.op_url_parse('https://a.com/x?y=1#z', '')).ok"
            )
            .expect("eval"),
            serde_json::json!(true)
        );
        assert_eq!(
            rt.evaluate(
                "JSON.parse(__tbHost.core.ops.op_text_decode('utf-8', new Uint8Array([104, 105]), false, false)).v"
            )
            .expect("eval"),
            serde_json::json!("hi")
        );
        assert_eq!(
            rt.evaluate("Array.from(__tbHost.core.ops.op_subtle_digest('SHA-256', new Uint8Array(0))).length")
                .expect("eval"),
            serde_json::json!(32)
        );
        assert_eq!(
            rt.evaluate("Array.from(__tbHost.core.ops.op_random_bytes(8)).length")
                .expect("eval"),
            serde_json::json!(8)
        );
    }

    #[test]
    fn fallible_op_errors_throw_into_js() {
        let mut rt = QuickJsRuntime::new().expect("runtime constructs");
        assert_eq!(
            rt.evaluate(
                "(function(){ try { __tbHost.core.ops.op_subtle_aes_gcm(true, new Uint8Array(0), new Uint8Array(12), new Uint8Array(0), new Uint8Array(0)); return 'no-throw'; } catch (e) { return 'threw'; } })()"
            )
            .expect("eval"),
            serde_json::json!("threw")
        );
    }

    #[test]
    fn op_panic_landing_pad_returns_default() {
        let v = guarded(|| -> String { panic!("boom") });
        assert_eq!(v, "");
    }

    #[test]
    fn fallible_op_panic_landing_pad_returns_error() {
        let err = guarded_result(|| -> Result<Vec<u8>, rquickjs::Error> { panic!("boom") })
            .expect_err("panic must become an error");
        assert!(err.to_string().contains("op panicked"), "{err}");
    }

    #[test]
    fn http_document_cannot_read_file_module() {
        let mut rt = setup_at("<html><body></body></html>", "https://example.com/page");
        rt.set_http_client(allow_private_client());
        let err = rt
            .fetch_module_source("file:///etc/passwd", Duration::from_secs(1))
            .expect_err("file: module from an https document must fail");
        assert!(
            err.contains("cross-scheme") || err.contains("file"),
            "got {err}"
        );
    }

    #[test]
    fn absolute_path_is_not_a_filesystem_module() {
        let mut rt = setup_at("<html><body></body></html>", "https://example.com/page");
        rt.set_http_client(allow_private_client());
        let err = rt
            .fetch_module_source("/etc/passwd", Duration::from_secs(1))
            .expect_err("bare paths must not be read from disk");
        assert!(err.contains("Invalid module URL"), "got {err}");
    }

    #[test]
    fn file_document_can_load_file_module() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tinybrowser-qjs-mod-{}.js", std::process::id()));
        std::fs::write(&path, "export const value = 1;\n").unwrap();
        let file_url = url::Url::from_file_path(&path).expect("temp path is absolute");
        let page = url::Url::from_file_path(dir.join("page.html")).expect("temp path is absolute");
        let mut rt = setup_at("<html><body></body></html>", page.as_str());
        rt.set_http_client(allow_private_client());
        let source = rt
            .fetch_module_source(file_url.as_str(), Duration::from_secs(2))
            .expect("file: module from a file: document");
        let _ = std::fs::remove_file(&path);
        assert!(source.contains("export const value"), "{source}");
    }

    #[test]
    fn module_fetch_times_out_instead_of_hanging() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            use std::io::Read;
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                std::thread::sleep(Duration::from_secs(30));
            }
        });
        let mut rt = setup_at("<html><body></body></html>", &format!("http://{addr}/"));
        rt.set_http_client(allow_private_client());
        let started = Instant::now();
        let err = rt
            .fetch_module_source(&format!("http://{addr}/mod.js"), Duration::from_millis(80))
            .expect_err("hung module fetch must time out");
        assert!(err.contains("timed out"), "got {err}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout took {:?}",
            started.elapsed()
        );
        drop(server);
    }

    fn serve_once(body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (format!("http://{addr}/fixture"), handle)
    }

    fn allow_private_client() -> std::sync::Arc<tinybrowser_net::HttpClient> {
        std::sync::Arc::new(tinybrowser_net::HttpClient::with_full_options(
            std::sync::Arc::new(tinybrowser_net::CookieJar::new()),
            None,
            true,
        ))
    }

    fn pump_until(rt: &mut QuickJsRuntime, pred: &str, budget_ms: u64) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_millis(budget_ms);
        loop {
            let value = rt.evaluate(pred).expect("eval");
            if value != serde_json::Value::Null && value != serde_json::json!(false) {
                return value;
            }
            if Instant::now() >= deadline {
                return value;
            }
            rt.run_event_loop_bounded(20).expect("pump");
        }
    }

    #[test]
    fn fetch_against_local_fixture_returns_the_body() {
        let (url, server) = serve_once("hello-fixture");
        let page = url.replace("/fixture", "/page");
        let mut rt = setup_at("<html><body></body></html>", &page);
        rt.set_http_client(allow_private_client());
        rt.evaluate(&format!(
            "(function(){{ globalThis.__body = null; fetch('{url}').then(function(r){{ return r.text(); }}).then(function(t){{ globalThis.__body = t; }}).catch(function(e){{ globalThis.__body = String(e); }}); return true; }})()"
        ))
        .expect("start fetch");
        let body = pump_until(&mut rt, "globalThis.__body", 5_000);
        drop(server);
        assert_eq!(body, serde_json::json!("hello-fixture"));
    }

    #[test]
    fn fetch_http_runs_off_the_isolate_thread() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            started_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            let body = "slow";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes());
        });
        let url = format!("http://{addr}/slow");
        let mut rt = setup_at("<html><body></body></html>", &format!("http://{addr}/page"));
        rt.set_http_client(allow_private_client());
        rt.evaluate(&format!(
            "(function(){{ globalThis.__body = null; fetch('{url}').then(function(r){{ return r.text(); }}).then(function(t){{ globalThis.__body = t; }}).catch(function(e){{ globalThis.__body = String(e); }}); return true; }})()"
        ))
        .expect("start fetch");
        // The isolate must stay usable while HTTP is in flight. A blocking
        // fetch on this thread would not return from evaluate until the
        // server slept 200ms.
        let t0 = Instant::now();
        assert_eq!(rt.evaluate("1+1").expect("eval"), serde_json::json!(2));
        assert!(t0.elapsed() < Duration::from_millis(100));
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("network thread issued the request");
        let body = pump_until(&mut rt, "globalThis.__body", 5_000);
        drop(server);
        assert_eq!(body, serde_json::json!("slow"));
    }

    #[test]
    fn fetch_ssrf_blocks_loopback_without_allow_private_network() {
        let mut rt = setup("<html><body></body></html>");
        rt.evaluate(
            "(function(){{ globalThis.__err = null; fetch('http://127.0.0.1:1/secret').then(function(){{ globalThis.__err = 'resolved'; }}).catch(function(e){{ globalThis.__err = e.name; }}); return true; }})()"
        )
        .expect("start fetch");
        let err = pump_until(&mut rt, "globalThis.__err", 2_000);
        assert_eq!(err, serde_json::json!("AbortError"));
    }

    #[test]
    fn fetch_intercept_fulfill_fail_and_continue_reach_the_promise() {
        let (url, server) = serve_once("from-network");
        let page = url.replace("/fixture", "/page");
        let mut rt = setup_at("<html><body></body></html>", &page);
        rt.set_http_client(allow_private_client());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ops::InterceptedRequest>();
        rt.set_intercept_tx(tx);
        rt.set_intercept_enabled(true);
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let fulfill = rx.recv().await.expect("fulfill");
                let _ = fulfill.resolver.send(ops::InterceptResolution::Fulfill {
                    status: 200,
                    headers: Default::default(),
                    body: "fulfilled".into(),
                });
                let fail = rx.recv().await.expect("fail");
                let _ = fail.resolver.send(ops::InterceptResolution::Fail {
                    reason: "blocked-by-test".into(),
                });
                let cont = rx.recv().await.expect("continue");
                let _ = cont.resolver.send(ops::InterceptResolution::Continue {
                    url: None,
                    method: None,
                    headers: None,
                    body: None,
                });
            });
        });

        rt.evaluate(
            "(function(){ globalThis.__a = null; globalThis.__b = null; globalThis.__c = null; return true; })()"
        )
        .unwrap();
        rt.evaluate(&format!(
            "(function(){{ fetch('http://example.invalid/fulfill').then(function(r){{ return r.text(); }}).then(function(t){{ globalThis.__a = t; }}).catch(function(e){{ globalThis.__a = String(e); }}); return true; }})()"
        ))
        .unwrap();
        assert_eq!(
            pump_until(&mut rt, "globalThis.__a", 5_000),
            serde_json::json!("fulfilled")
        );

        rt.evaluate(
            "(function(){{ fetch('http://example.invalid/fail').then(function(){{ globalThis.__b = 'ok'; }}).catch(function(e){{ globalThis.__b = e.name; }}); return true; }})()"
        )
        .unwrap();
        assert_eq!(
            pump_until(&mut rt, "globalThis.__b", 5_000),
            serde_json::json!("AbortError")
        );

        rt.evaluate(&format!(
            "(function(){{ fetch('{url}').then(function(r){{ return r.text(); }}).then(function(t){{ globalThis.__c = t; }}).catch(function(e){{ globalThis.__c = String(e); }}); return true; }})()"
        ))
        .unwrap();
        assert_eq!(
            pump_until(&mut rt, "globalThis.__c", 5_000),
            serde_json::json!("from-network")
        );
        drop(server);
    }

    #[test]
    fn watchdog_interrupts_a_tight_loop_and_runtime_is_reusable() {
        let mut rt = setup("<html><body></body></html>");
        let token = rt.arm_watchdog(Duration::from_millis(50));
        let err = rt.evaluate("(function(){ while (true) {} })()");
        let fired = rt.disarm_watchdog(token);
        assert!(
            err.is_err() || fired,
            "tight loop must be interrupted: {err:?} fired={fired}"
        );
        rt.cancel_termination();
        assert_eq!(
            rt.evaluate("1+1").expect("eval after cancel"),
            serde_json::json!(2)
        );
    }

    #[test]
    fn disarm_watchdog_before_budget_leaves_evaluate_succeeding() {
        let mut rt = setup("<html><body></body></html>");
        let token = rt.arm_watchdog(Duration::from_secs(5));
        assert_eq!(rt.evaluate("1+1").expect("eval"), serde_json::json!(2));
        assert!(!rt.disarm_watchdog(token));
    }

    #[test]
    fn watchdog_does_not_abort_in_flight_fetch() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            std::thread::sleep(Duration::from_millis(300));
            let body = "still-here";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            let _ = stream.write_all(response.as_bytes());
        });
        let mut rt = setup_at("<html><body></body></html>", &format!("http://{addr}/page"));
        rt.set_http_client(allow_private_client());
        rt.evaluate(&format!(
            "(function(){{ globalThis.__body = null; fetch('http://{addr}/slow').then(function(r){{ return r.text(); }}).then(function(t){{ globalThis.__body = t; }}).catch(function(e){{ globalThis.__body = String(e); }}); return true; }})()"
        ))
        .expect("start fetch");
        let token = rt.arm_watchdog(Duration::from_millis(50));
        let _ = rt.evaluate("(function(){ while (true) {} })()");
        let _ = rt.disarm_watchdog(token);
        rt.cancel_termination();
        let body = pump_until(&mut rt, "globalThis.__body", 5_000);
        drop(server);
        assert_eq!(body, serde_json::json!("still-here"));
    }
}
