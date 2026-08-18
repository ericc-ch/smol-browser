use std::sync::Arc;
use std::time::Instant;

use clap::{Parser, Subcommand};
use tinybrowser_core::{page_actor_channel, run_page_actor, BrowserContext, Page};
use tokio::io::AsyncWriteExt;
use tokio::time::{timeout, Duration};

#[derive(Parser)]
#[command(
    name = "tinybrowser",
    version = env!("TINYBROWSER_BUILD_VERSION"),
    about = "tinybrowser - A lightweight headless browser for web scraping and automation",
)]
struct Args {
    #[arg(short, long, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Command>,

    #[arg(short, long, default_value_t = 9222)]
    port: u16,

    #[arg(long, global = true)]
    proxy: Option<String>,

    #[arg(long)]
    obey_robots: bool,

    #[arg(long)]
    user_agent: Option<String>,

    #[arg(long)]
    storage_dir: Option<std::path::PathBuf>,

    /// Permit fetches to loopback, RFC1918, and link-local addresses.
    /// Default is to block them (SSRF fix from #4). Use this for local
    /// development against http://localhost:N or http://192.168.x.y.
    /// Equivalent to `TINYBROWSER_ALLOW_PRIVATE_NETWORK=1` but per-process
    /// and survives in command pipelines.
    #[arg(long, global = true)]
    allow_private_network: bool,
}

#[derive(Subcommand)]
enum Command {
    Serve {
        #[arg(short, long, default_value_t = 9222)]
        port: u16,

        // Bind address. Defaults to 127.0.0.1 (loopback only) for safety.
        // Set to 0.0.0.0 to listen on all interfaces (e.g. inside a Docker
        // container where you want the port to be reachable from the host
        // via -p mapping).
        #[arg(long, default_value = "127.0.0.1")]
        host: String,

        #[arg(long)]
        proxy: Option<String>,

        #[arg(long)]
        user_agent: Option<String>,

        /// Maximum live CDP connections. Connections beyond the limit are
        /// refused with a 503 rather than queued.
        #[arg(long, default_value_t = tinybrowser_cdp::DEFAULT_MAX_CONNECTIONS)]
        max_connections: usize,

        /// Allow CDP clients to navigate to file:// URLs. Off by
        /// default so a CDP connection cannot read arbitrary local
        /// files. Enable only when serving local HTML for testing
        /// and the port is on a trusted network.
        #[arg(long)]
        allow_file_access: bool,

        #[arg(long)]
        storage_dir: Option<std::path::PathBuf>,

        /// Suppress all logs (same as on `fetch`). Useful when scraping pages
        /// that flood the console with per-page script warnings (issue #264).
        #[arg(long)]
        quiet: bool,
    },

    Fetch {
        // Optional so a batch run can pass URLs via --file instead. A single
        // positional URL keeps the original one-shot behaviour.
        url: Option<String>,

        // Default is html. Kept as Option so we can tell whether --dump was
        // explicitly passed: a bare --eval returns its own value, while --eval
        // combined with --dump (or --selector) runs the eval, lets its async
        // work settle, then reads the page (issue #248).
        #[arg(long)]
        dump: Option<DumpFormat>,

        /// Read newline-delimited URLs from a file (one per line; blank lines
        /// and lines starting with `#` are skipped). Use `-` for stdin. Enables
        /// batch mode: every URL is fetched raw (--dump original) and one JSON
        /// status line is printed per URL.
        #[arg(long)]
        file: Option<std::path::PathBuf>,

        /// Number of URLs fetched concurrently in batch mode. Ignored without
        /// --file.
        #[arg(long, default_value_t = std::num::NonZeroUsize::new(1).unwrap())]
        concurrency: std::num::NonZeroUsize,

        #[arg(long)]
        selector: Option<String>,

        /// Maximum adaptive post-load settle time in seconds. When supplied
        /// explicitly, this is a fixed delay; the default is a 5-second cap
        /// that returns once the page is quiescent.
        #[arg(long)]
        wait: Option<u64>,

        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,

        #[arg(long, default_value = "load")]
        wait_until: String,

        #[arg(long)]
        user_agent: Option<String>,

        #[arg(long, short)]
        eval: Option<String>,

        #[arg(long, short = 'o')]
        output: Option<std::path::PathBuf>,

        #[arg(long, short)]
        quiet: bool,

        #[arg(long)]
        storage_dir: Option<std::path::PathBuf>,
    },
}

#[derive(Clone, Debug, clap::ValueEnum, PartialEq, Eq)]
enum DumpFormat {
    Html,
    Text,
    Links,
    Markdown,
    /// Stream the raw HTTP response body verbatim (binary-safe).
    /// Bypasses the browser/JS layer — useful for fetching images,
    /// JSON, JS, CSS, or any non-HTML resource (cf. issue #117).
    Original,
    /// One JSON object per line listing every sub-resource URL the
    /// rendered page references (script src, link href, img src,
    /// iframe src, media sources, embed/object data). Lets callers
    /// replay the asset graph with their own HTTP client when they
    /// need the originals alongside the page (cf. issue 124).
    Assets,
    /// Dump all cookies in the browser jar as a JSON array, including
    /// HttpOnly cookies that are inaccessible via document.cookie.
    /// Useful for extracting session tokens set by anti-bot challenges.
    Cookies,
}

fn print_banner(port: u16) {
    println!(
        "tinybrowser v{}\nCDP server: ws://127.0.0.1:{}/devtools/browser",
        env!("TINYBROWSER_BUILD_VERSION"),
        port
    );
}

fn select_log_filter(verbose: bool, quiet: bool) -> &'static str {
    if verbose {
        "debug"
    } else if quiet {
        "off"
    } else {
        "warn"
    }
}

fn is_quiet_command(cmd: &Option<Command>) -> bool {
    matches!(
        cmd,
        Some(Command::Fetch { quiet: true, .. } | Command::Serve { quiet: true, .. })
    )
}

fn merge_proxy(global_proxy: Option<String>, command_proxy: Option<String>) -> Option<String> {
    command_proxy.or(global_proxy)
}

#[expect(
    unsafe_code,
    reason = "Initialize process TZ environment variable at startup before threads spawn"
)]
fn init_process_timezone() {
    // Pin the process timezone before JS Date/Intl reads it. QuickJS sources
    // the zone for both Date (getTimezoneOffset, toString) and Intl.DateTimeFormat
    // from TZ; left unset it defaults to UTC for Date while the page layer
    // advertised a different zone, a cross-surface mismatch fingerprinting
    // scripts flag. Default to Europe/Berlin; set TINYBROWSER_TIMEZONE to match
    // the exit IP's region. An existing TZ from the host is respected.
    if let Some(tz) = std::env::var("TINYBROWSER_TIMEZONE")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        // SAFETY: runs synchronously at program entry before any worker thread or runtime starts.
        unsafe {
            std::env::set_var("TZ", tz);
        }
    } else if std::env::var_os("TZ").is_none() {
        // SAFETY: runs synchronously at program entry before any worker thread or runtime starts.
        unsafe {
            std::env::set_var("TZ", "Europe/Berlin");
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    init_process_timezone();

    let quiet = is_quiet_command(&args.command);
    let filter = select_log_filter(args.verbose, quiet);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
        )
        .with_writer(std::io::stderr)
        .init();

    let global_proxy = args.proxy.clone();

    match args.command {
        Some(Command::Serve {
            port,
            host,
            proxy,
            user_agent,
            max_connections,
            allow_file_access,
            storage_dir,
            quiet: _,
        }) => {
            // Fall back to TINYBROWSER_PROXY so a proxy can be supplied without
            // putting credentials on the command line.
            let proxy = merge_proxy(global_proxy.clone(), proxy).or_else(|| {
                std::env::var("TINYBROWSER_PROXY")
                    .ok()
                    .filter(|s| !s.is_empty())
            });
            print_banner(port);
            if let Some(ref dir) = storage_dir {
                tracing::info!("Storage dir: {}", dir.display());
            }
            if let Some(ref proxy) = proxy {
                tracing::info!("Using proxy: {}", proxy);
            }
            if let Some(ref ua) = user_agent {
                tracing::info!("User-Agent: {}", ua);
            }

            tinybrowser_cdp::start_with_serve_options_and_limit(
                port,
                &host,
                proxy,
                user_agent,
                allow_file_access,
                storage_dir,
                args.allow_private_network,
                max_connections,
            )
            .await?;
        }
        Some(Command::Fetch {
            url,
            dump,
            selector,
            wait,
            timeout,
            wait_until,
            user_agent,
            eval,
            output,
            quiet,
            storage_dir,
            file,
            concurrency,
        }) => {
            if let Some(file) = file {
                if url.is_some() {
                    anyhow::bail!("Pass URLs via a positional argument or --file, not both.");
                }
                // Batch mode is raw HTTP only.
                match dump {
                    None | Some(DumpFormat::Original) => {}
                    Some(_) => anyhow::bail!("batch mode (--file) only supports --dump original."),
                }
                let urls = read_urls_from_file(&file)?;
                run_batch_fetch(
                    urls,
                    concurrency.get(),
                    timeout,
                    user_agent,
                    global_proxy,
                    output,
                    quiet,
                )
                .await?;
            } else {
                let url = url.ok_or_else(|| {
                    anyhow::anyhow!(
                        "No URL provided. Pass a URL, or a list of URLs with --file <path>."
                    )
                })?;
                let wait_is_fixed = wait.is_some();
                run_fetch(
                    &url,
                    dump,
                    selector,
                    wait.unwrap_or(5),
                    wait_is_fixed,
                    timeout,
                    &wait_until,
                    user_agent,
                    eval,
                    output,
                    quiet,
                    global_proxy,
                    storage_dir,
                    args.allow_private_network,
                )
                .await?;
            }
        }
        None => {
            print_banner(args.port);
            if let Some(ref proxy) = args.proxy {
                tracing::info!("Using proxy: {}", proxy);
            }
            tinybrowser_cdp::start_with_options(args.port, args.proxy).await?;
        }
    }

    Ok(())
}

fn configure_fetch_navigation_timeout(page: &mut Page, timeout_secs: u64) {
    page.set_navigation_timeout(Duration::from_secs(timeout_secs));
}

async fn run_fetch(
    url_str: &str,
    dump: Option<DumpFormat>,
    selector: Option<String>,
    wait_secs: u64,
    wait_is_fixed: bool,
    timeout_secs: u64,
    wait_until: &str,
    user_agent: Option<String>,
    eval: Option<String>,
    output: Option<std::path::PathBuf>,
    quiet: bool,
    proxy: Option<String>,
    storage_dir: Option<std::path::PathBuf>,
    allow_private_network: bool,
) -> anyhow::Result<()> {
    // Whether the user explicitly passed --dump. With --eval also present this
    // decides whether we return the eval value or read the page after the
    // eval's async work settles (issue #248).
    let dump_specified = dump.is_some();
    let dump = dump.unwrap_or(DumpFormat::Html);

    // --dump original short-circuits the browser stack entirely: fetch the raw
    // response body via HTTP and stream the bytes verbatim. Useful for binary
    // payloads (images, fonts, …) and any non-HTML resource where parsing the
    // body through the DOM/JS layer would corrupt or discard data.
    if dump == DumpFormat::Original {
        let bytes = fetch_original_bytes(url_str, proxy, user_agent.clone(), timeout_secs).await?;
        write_or_print_bytes(&bytes, output.as_ref()).await?;
        return Ok(());
    }

    let context = Arc::new(BrowserContext::with_storage_and_network(
        "fetch".to_string(),
        proxy,
        user_agent.clone(),
        storage_dir.clone(),
        allow_private_network,
    ));
    let wait_condition = tinybrowser_core::lifecycle::WaitUntil::from_str(wait_until);
    let url_owned = url_str.to_string();
    let eval = eval.clone();
    let selector = selector.clone();
    let output = output.clone();
    let user_agent = user_agent.clone();

    if !quiet {
        eprintln!("Fetching {url_str}...");
    }

    {
        let settle_passes = if eval.is_some() && (selector.is_some() || dump_specified) {
            2
        } else {
            1
        };
        let hard = Duration::from_secs(
            timeout_secs
                .saturating_add(wait_secs.saturating_mul(settle_passes))
                .saturating_add(10),
        );
        std::thread::spawn(move || {
            std::thread::sleep(hard);
            eprintln!(
                "tinybrowser: hard timeout exceeded ({}s); forcing exit",
                hard.as_secs()
            );
            std::process::exit(124);
        });
    }

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let mut page = Page::new("fetch-page".to_string(), context.clone());
            configure_fetch_navigation_timeout(&mut page, timeout_secs);
            if let Some(ref ua) = user_agent {
                page.http_client.set_user_agent(ua).await;
            }
            let (handle, rx) = page_actor_channel();
            tokio::task::spawn_local(run_page_actor(page, rx));

            match timeout(
                Duration::from_secs(timeout_secs),
                handle.navigate_with_wait(&url_owned, wait_condition),
            )
            .await
            {
                Ok(result) => {
                    result.map_err(|e| anyhow::anyhow!("Failed to navigate to {url_owned}: {e}"))?
                }
                Err(_) => {
                    anyhow::bail!("Timed out navigating to {url_owned} after {timeout_secs}s")
                }
            }

            if !quiet {
                let (url, title) = handle
                    .with_page(|p| (p.url_string(), p.title.clone()))
                    .await
                    .unwrap_or_default();
                eprintln!("Page loaded: {url} - \"{title}\"");
            }

            let wait_ms = wait_secs.saturating_mul(1000);
            if wait_is_fixed {
                handle.settle_for_duration(wait_ms).await;
            } else {
                handle.settle(wait_ms).await;
            }

            if let Some(ref expr) = eval {
                let timeout = Duration::from_secs(timeout_secs);
                let expr = expr.clone();
                let result = handle
                    .with_page(move |p| p.evaluate_with_timeout(&expr, timeout))
                    .await
                    .unwrap_or(serde_json::Value::Null);

                if !dump_specified && selector.is_none() {
                    let rendered = match result {
                        serde_json::Value::String(s) => s,
                        serde_json::Value::Null => "null".to_string(),
                        other => other.to_string(),
                    };
                    write_or_print(rendered, output.as_ref()).await?;
                    context.save_cookies();
                    handle.shutdown();
                    return Ok(());
                }

                if wait_is_fixed {
                    handle.settle_for_duration(wait_ms).await;
                } else {
                    handle.settle(wait_ms).await;
                }
            }

            if let Some(ref sel) = selector {
                let found = wait_for_selector_actor(&handle, sel, wait_secs).await;
                if !found {
                    eprintln!("Warning: selector '{sel}' not found after {wait_secs}s");
                }
            }

            let rendered = handle
                .with_page(move |page| match dump {
                    DumpFormat::Html => dump_html(page),
                    DumpFormat::Text => dump_text(page),
                    DumpFormat::Links => dump_links(page),
                    DumpFormat::Markdown => dump_markdown(page),
                    DumpFormat::Assets => dump_assets(page),
                    DumpFormat::Cookies => dump_cookies(page),
                    DumpFormat::Original => {
                        unreachable!("Original dump handled before page navigation")
                    }
                })
                .await
                .unwrap_or_default();
            write_or_print(rendered, output.as_ref()).await?;
            context.save_cookies();
            handle.shutdown();
            Ok(())
        })
        .await
}

async fn fetch_original_response(
    url_str: &str,
    proxy: Option<String>,
    user_agent: Option<String>,
    timeout_secs: u64,
) -> anyhow::Result<tinybrowser_net::Response> {
    let url =
        url::Url::parse(url_str).map_err(|e| anyhow::anyhow!("Invalid URL '{url_str}': {e}"))?;

    let client = tinybrowser_net::HttpClient::with_options(
        Arc::new(tinybrowser_net::CookieJar::new()),
        proxy.as_deref(),
    );
    if let Some(ua) = user_agent {
        client.set_user_agent(&ua).await;
    }

    match timeout(Duration::from_secs(timeout_secs), client.fetch(&url)).await {
        Ok(Ok(resp)) => Ok(resp),
        Ok(Err(e)) => anyhow::bail!("Failed to fetch {url_str}: {e}"),
        Err(_) => anyhow::bail!("Timed out fetching {url_str} after {timeout_secs}s"),
    }
}

async fn fetch_original_bytes(
    url_str: &str,
    proxy: Option<String>,
    user_agent: Option<String>,
    timeout_secs: u64,
) -> anyhow::Result<Vec<u8>> {
    Ok(
        fetch_original_response(url_str, proxy, user_agent, timeout_secs)
            .await?
            .body,
    )
}

/// Read newline-delimited URLs from `path` (or stdin when `path` is `-`).
/// Blank lines and `#` comments are dropped, and surrounding whitespace is
/// trimmed so a list copy-pasted with indentation still works.
fn read_urls_from_file(path: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let content = if path == std::path::Path::new("-") {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| anyhow::anyhow!("Failed to read URLs from stdin: {e}"))?;
        s
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?
    };

    Ok(content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect())
}

/// Batch raw fetch: run `--dump original` over many URLs concurrently and print
/// one JSON status line per URL (issue #349). It never renders, so there is no
/// browser/JS cost per URL. Output stays in input order regardless of
/// completion order.
async fn run_batch_fetch(
    urls: Vec<String>,
    concurrency: usize,
    timeout_secs: u64,
    user_agent: Option<String>,
    proxy: Option<String>,
    output: Option<std::path::PathBuf>,
    quiet: bool,
) -> anyhow::Result<()> {
    let total = urls.len();
    if total == 0 {
        anyhow::bail!("No URLs to fetch (--file was empty).");
    }

    if !quiet {
        eprintln!(
            "Fetching {total} URLs with {concurrency} concurrent request(s) (per-fetch timeout: {timeout_secs}s)..."
        );
    }

    let start = Instant::now();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let user_agent = Arc::new(user_agent);
    let proxy = Arc::new(proxy);

    let mut handles = Vec::with_capacity(total);
    for (i, url) in urls.into_iter().enumerate() {
        let sem = semaphore.clone();
        let user_agent = user_agent.clone();
        let proxy = proxy.clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let task_start = Instant::now();
            let result = fetch_original_response(
                &url,
                (*proxy).clone(),
                (*user_agent).clone(),
                timeout_secs,
            )
            .await;
            let elapsed_ms = task_start.elapsed().as_millis();

            let line = match result {
                Ok(resp) => serde_json::json!({
                    "url": url,
                    "ok": (200..400).contains(&resp.status),
                    "status": resp.status,
                    "content_type": resp.headers.get("content-type").cloned().unwrap_or_default(),
                    "bytes": resp.body.len(),
                    "elapsed_ms": elapsed_ms,
                }),
                Err(e) => serde_json::json!({
                    "url": url,
                    "ok": false,
                    "error": e.to_string(),
                    "elapsed_ms": elapsed_ms,
                }),
            };
            (i, line)
        }));
    }

    let mut results: Vec<Option<serde_json::Value>> = vec![None; total];
    let mut failures = 0usize;
    for handle in handles {
        if let Ok((i, line)) = handle.await {
            if !line["ok"].as_bool().unwrap_or(false) {
                failures += 1;
            }
            results[i] = Some(line);
        } else {
            failures += 1;
        }
    }

    let mut out = String::new();
    for line in results.into_iter().flatten() {
        out.push_str(&serde_json::to_string(&line).unwrap_or_default());
        out.push('\n');
    }

    if let Some(path) = output {
        tokio::fs::write(&path, out.as_bytes())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path.display(), e))?;
    } else {
        let mut stdout = tokio::io::stdout();
        stdout
            .write_all(out.as_bytes())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write to stdout: {e}"))?;
        stdout
            .flush()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to flush stdout: {e}"))?;
    }

    if !quiet {
        eprintln!(
            "Done: {} URLs in {:.1}s ({} ok, {} failed).",
            total,
            start.elapsed().as_secs_f64(),
            total - failures,
            failures
        );
    }

    Ok(())
}

async fn write_or_print(
    content: String,
    output: Option<&std::path::PathBuf>,
) -> anyhow::Result<()> {
    if let Some(path) = output {
        tokio::fs::write(path, content)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path.display(), e))?;
    } else {
        println!("{content}");
    }
    Ok(())
}

async fn write_or_print_bytes(
    bytes: &[u8],
    output: Option<&std::path::PathBuf>,
) -> anyhow::Result<()> {
    if let Some(path) = output {
        tokio::fs::write(path, bytes)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write {}: {}", path.display(), e))?;
    } else {
        // Write raw bytes to stdout — never println! (would append a newline
        // and break binary payloads like JPEG/PNG).
        let mut stdout = tokio::io::stdout();
        stdout
            .write_all(bytes)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write to stdout: {e}"))?;
        stdout
            .flush()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to flush stdout: {e}"))?;
    }
    Ok(())
}

async fn wait_for_selector_actor(
    handle: &tinybrowser_core::PageActorHandle,
    selector: &str,
    timeout_secs: u64,
) -> bool {
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    loop {
        let sel = selector.to_string();
        let found = handle
            .with_page(move |page| {
                page.with_dom(|dom| dom.query_selector(&sel).ok().flatten().is_some())
                    .unwrap_or(false)
            })
            .await
            .unwrap_or(false);
        if found {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        let slice_started = tokio::time::Instant::now();
        handle.settle(100).await;
        let spent = slice_started.elapsed();
        let cadence = tokio::time::Duration::from_millis(100);
        if spent < cadence {
            tokio::time::sleep(cadence.checked_sub(spent).unwrap()).await;
        }
    }
}

fn dump_cookies(page: &Page) -> String {
    let cookies = page.context.cookie_jar.get_all_cookies();
    serde_json::to_string_pretty(&cookies).unwrap_or_else(|_| "[]".to_string())
}

fn dump_html(page: &Page) -> String {
    page.with_dom(|dom| {
        if let Ok(Some(html_node)) = dom.query_selector("html") {
            let html = dom.outer_html(html_node);
            format!("<!DOCTYPE html>\n{html}")
        } else {
            let doc = dom.document();
            dom.inner_html(doc)
        }
    })
    .unwrap_or_default()
}

fn dump_text(page: &mut Page) -> String {
    page.with_dom(|dom| {
        if let Ok(Some(body)) = dom.query_selector("body") {
            let text = extract_readable_text(dom, body);
            text.trim().to_string()
        } else {
            String::new()
        }
    })
    .unwrap_or_default()
}

fn dump_markdown(page: &mut Page) -> String {
    let result = page.evaluate(tinybrowser_core::HTML_TO_MARKDOWN_JS);
    result.as_str().unwrap_or_default().to_string()
}

fn extract_readable_text(
    dom: &tinybrowser_dom::DomTree,
    node_id: tinybrowser_dom::NodeId,
) -> String {
    use tinybrowser_dom::NodeData;

    // Iterative DFS over an explicit work stack. A recursive walk overflowed the
    // call stack (a hard abort, not a catchable panic) on deeply nested pages,
    // taking down the process on `--dump text` (issue #362, the CLI counterpart
    // of the serialize/textContent paths made iterative in tinybrowser-dom). A
    // `Newline` work item emits a block element's trailing newline after its
    // children, matching the old pre/post-recursion output exactly.
    enum Work {
        Visit(tinybrowser_dom::NodeId),
        Newline,
    }

    // Defense-in-depth cap mirroring DomTree::descendants; never reached on a
    // valid tree since append_child / insert_before reject cycles.
    const MAX_NODES: usize = 5_000_000;

    let mut result = String::new();
    let mut stack: Vec<Work> = vec![Work::Visit(node_id)];
    let mut visited = 0usize;

    while let Some(work) = stack.pop() {
        let id = match work {
            Work::Newline => {
                result.push('\n');
                continue;
            }
            Work::Visit(id) => id,
        };

        visited += 1;
        if visited > MAX_NODES {
            break;
        }

        let node = match dom.get_node(id) {
            Some(n) => n,
            None => continue,
        };

        match &node.data {
            NodeData::Text { contents } => {
                let trimmed = contents.trim();
                if !trimmed.is_empty() {
                    result.push_str(trimmed);
                }
            }
            NodeData::Element { name, .. } => {
                let tag = name.local.as_ref();

                // Boilerplate elements rarely contain content the user wants to
                // extract — strip them so `--dump text` returns the article body
                // instead of menus, footers, and cookie banners.
                if matches!(
                    tag,
                    "script" | "style" | "nav" | "header" | "footer" | "aside"
                ) {
                    continue;
                }

                let is_block = matches!(
                    tag,
                    "div"
                        | "p"
                        | "h1"
                        | "h2"
                        | "h3"
                        | "h4"
                        | "h5"
                        | "h6"
                        | "li"
                        | "tr"
                        | "br"
                        | "hr"
                        | "blockquote"
                        | "pre"
                        | "section"
                        | "article"
                        | "header"
                        | "footer"
                        | "nav"
                        | "main"
                        | "aside"
                        | "figure"
                        | "figcaption"
                        | "table"
                        | "thead"
                        | "tbody"
                        | "tfoot"
                        | "dl"
                        | "dt"
                        | "dd"
                        | "ul"
                        | "ol"
                );

                if is_block {
                    result.push('\n');
                    // Processed after all children (stack is LIFO): the trailing newline.
                    stack.push(Work::Newline);
                }
                // Push children in reverse so they pop in document order.
                for child_id in dom.children(id).into_iter().rev() {
                    stack.push(Work::Visit(child_id));
                }
            }
            _ => {
                for child_id in dom.children(id).into_iter().rev() {
                    stack.push(Work::Visit(child_id));
                }
            }
        }
    }

    result
}

fn dump_links(page: &Page) -> String {
    let base_url = page.url.clone();
    page.with_dom(|dom| {
        let mut rendered = Vec::new();
        let links = dom.query_selector_all("a").unwrap_or_default();
        for link_id in links {
            if let Some(node) = dom.get_node(link_id) {
                let href = node.get_attribute("href").unwrap_or_default().to_string();
                let text = dom.text_content(link_id);
                let text = text.trim();

                let full_url = if href.starts_with("http://") || href.starts_with("https://") {
                    href.clone()
                } else if let Some(ref base) = base_url {
                    base.join(&href)
                        .map(|u| u.to_string())
                        .unwrap_or(href.clone())
                } else {
                    href.clone()
                };

                if !full_url.is_empty() {
                    if text.is_empty() {
                        rendered.push(full_url);
                    } else {
                        rendered.push(format!("{full_url}\t{text}"));
                    }
                }
            }
        }
        rendered.join("\n")
    })
    .unwrap_or_default()
}

/// Selectors paired with the attribute whose URL we extract and the
/// asset kind we surface. Order is stable so the output of
/// `--dump assets` is deterministic across runs.
const ASSET_SELECTORS: &[(&str, &str, &str)] = &[
    ("script[src]", "src", "script"),
    ("link[href]", "href", "link"),
    ("img[src]", "src", "image"),
    ("iframe[src]", "src", "iframe"),
    ("source[src]", "src", "media"),
    ("video[src]", "src", "video"),
    ("audio[src]", "src", "audio"),
    ("embed[src]", "src", "embed"),
    ("object[data]", "data", "object"),
];

/// Map a `<link>` element's `rel` token to a more specific asset
/// kind so consumers can filter (e.g. just stylesheets, just icons).
/// Unknown / missing `rel` falls back to a generic "link" so the
/// caller still sees the URL.
fn link_kind_from_rel(rel: &str) -> &'static str {
    match rel
        .split_ascii_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "stylesheet" => "stylesheet",
        "icon" | "shortcut" => "icon",
        "manifest" => "manifest",
        "preload" => "preload",
        "prefetch" => "prefetch",
        "modulepreload" => "modulepreload",
        "dns-prefetch" => "dns-prefetch",
        "preconnect" => "preconnect",
        "alternate" => "alternate",
        _ => "link",
    }
}

/// Resolve a raw `src`/`href`/`data` attribute against the page's
/// base URL. Mirrors `dump_links`'s logic so `--dump assets` and
/// `--dump links` agree on absolute-URL semantics.
fn resolve_asset_url(raw: &str, base_url: Option<&url::Url>) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Some(trimmed.to_string());
    }
    if let Some(base) = base_url {
        if let Ok(joined) = base.join(trimmed) {
            return Some(joined.to_string());
        }
    }
    Some(trimmed.to_string())
}

/// Walk the rendered DOM and emit one NDJSON line per discoverable
/// sub-resource. Pure over `DomTree`/`Url` so unit tests can drive
/// it from a fixture HTML without standing up a browser.
fn extract_assets(dom: &tinybrowser_dom::DomTree, base_url: Option<&url::Url>) -> String {
    let mut out: Vec<String> = Vec::new();
    for (selector, attr, default_kind) in ASSET_SELECTORS {
        let nodes = dom.query_selector_all(selector).unwrap_or_default();
        for node_id in nodes {
            let Some(node) = dom.get_node(node_id) else {
                continue;
            };
            let raw = node.get_attribute(attr).unwrap_or_default().to_string();
            let Some(url) = resolve_asset_url(&raw, base_url) else {
                continue;
            };

            let kind = if *default_kind == "link" {
                let rel = node.get_attribute("rel").unwrap_or_default().to_string();
                link_kind_from_rel(&rel)
            } else {
                *default_kind
            };

            let record = serde_json::json!({
                "url": url,
                "type": kind,
            });
            out.push(record.to_string());
        }
    }
    out.join("\n")
}

fn dump_assets(page: &Page) -> String {
    let base_url = page.url.clone();
    let dom_ndjson = page
        .with_dom(|dom| extract_assets(dom, base_url.as_ref()))
        .unwrap_or_default();

    let mut lines: Vec<String> = dom_ndjson
        .lines()
        .filter(|l| !l.is_empty())
        .map(std::string::ToString::to_string)
        .collect();

    // URLs already listed from static DOM attributes, so a resource the script
    // fetches that the markup also references is not emitted twice.
    let mut seen: std::collections::HashSet<String> = lines
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            v.get("url")
                .and_then(|u| u.as_str())
                .map(std::string::ToString::to_string)
        })
        .collect();

    // Resources pulled in by JS fetch()/XHR, which leave no static DOM tag
    // (issue #301).
    for url in page.fetched_urls() {
        if seen.insert(url.clone()) {
            lines.push(serde_json::json!({ "url": url, "type": "fetch" }).to_string());
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{
        configure_fetch_navigation_timeout, extract_assets, extract_readable_text,
        fetch_original_bytes, is_quiet_command, link_kind_from_rel, merge_proxy,
        read_urls_from_file, resolve_asset_url, select_log_filter, write_or_print,
        write_or_print_bytes, Args, Command, DumpFormat,
    };
    use clap::Parser;
    use tinybrowser_dom::parse_html;

    // Issue #117 — `--dump original` short-circuits the browser stack and
    // streams the raw response body verbatim, including for binary payloads.
    //
    // Two tests below pin the behaviour:
    //   1. clap accepts `--dump original` as a valid DumpFormat variant.
    //   2. `fetch_original_bytes` returns the exact bytes a `file://` URL
    //      points at (binary-safe round-trip — no UTF-8 decode, no trailing
    //      newline, no DOM mutation).
    //   3. `write_or_print_bytes` writes raw bytes to a file without the
    //      trailing newline that `println!` would add.
    #[test]
    fn parsed_fetch_dump_original_is_accepted_by_clap() {
        let args = Args::try_parse_from([
            "tinybrowser",
            "fetch",
            "--dump",
            "original",
            "https://example.com/image.jpg",
        ])
        .expect("clap should accept --dump original");
        match args.command {
            Some(Command::Fetch { dump, .. }) => {
                assert_eq!(dump, Some(DumpFormat::Original));
            }
            _ => panic!("expected Fetch command"),
        }
    }

    // Issue #349 — batch mode: `fetch --file urls.txt --dump original
    // --concurrency N` with no positional URL.
    #[test]
    fn parsed_fetch_file_and_concurrency() {
        let args = Args::try_parse_from([
            "tinybrowser",
            "fetch",
            "--file",
            "urls.txt",
            "--dump",
            "original",
            "--concurrency",
            "25",
        ])
        .expect("clap should accept --file with --concurrency and no positional URL");
        match args.command {
            Some(Command::Fetch {
                url,
                file,
                concurrency,
                dump,
                ..
            }) => {
                assert!(url.is_none());
                assert_eq!(file, Some(std::path::PathBuf::from("urls.txt")));
                assert_eq!(concurrency.get(), 25);
                assert_eq!(dump, Some(DumpFormat::Original));
            }
            _ => panic!("expected Fetch command"),
        }
    }

    #[test]
    fn concurrency_rejects_zero() {
        // NonZeroUsize means --concurrency 0 is a parse error, not a silent hang
        // on a zero-permit semaphore.
        let err = Args::try_parse_from([
            "tinybrowser",
            "fetch",
            "--file",
            "u.txt",
            "--concurrency",
            "0",
        ]);
        assert!(err.is_err());
    }

    #[test]
    fn read_urls_skips_blanks_and_comments() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tinybrowser_urls_{}.txt", std::process::id()));
        std::fs::write(
            &path,
            "https://a.example/one.js\n\n  # a comment\n   https://b.example/two.css  \nhttps://c.example/three.json\n",
        )
        .unwrap();
        let urls = read_urls_from_file(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            urls,
            vec![
                "https://a.example/one.js".to_string(),
                "https://b.example/two.css".to_string(),
                "https://c.example/three.json".to_string(),
            ]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fetch_original_bytes_returns_file_contents_verbatim() {
        // A real binary payload: a 1×1 transparent PNG (89 50 4E 47 …) —
        // exactly the kind of resource #117 wants to stream without HTML/
        // JS rendering.
        const PNG_BYTES: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];

        let path = std::env::temp_dir().join(format!(
            "tinybrowser-fetch-original-test-{}.png",
            std::process::id()
        ));
        let _ = tokio::fs::remove_file(&path).await;
        tokio::fs::write(&path, PNG_BYTES)
            .await
            .expect("seed temp PNG fixture");

        let file_url = format!("file://{}", path.display());
        let bytes = fetch_original_bytes(&file_url, None, None, 5)
            .await
            .expect("fetch_original_bytes should round-trip the file body");

        let _ = tokio::fs::remove_file(&path).await;

        assert_eq!(
            bytes, PNG_BYTES,
            "raw response body must match the file byte-for-byte"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_or_print_bytes_writes_without_trailing_newline() {
        // Regression guard for #117: stdout must receive raw bytes. The file
        // path used here exercises the file-output branch — println!-style
        // output (used by write_or_print) would append a 0x0A byte and
        // corrupt binary payloads. write_or_print_bytes must not.
        let payload: &[u8] = &[0x00, 0xFF, b'h', b'i', 0x00];
        let path = std::env::temp_dir().join(format!(
            "tinybrowser-write-bytes-test-{}.bin",
            std::process::id()
        ));
        let _ = tokio::fs::remove_file(&path).await;

        write_or_print_bytes(payload, Some(&path))
            .await
            .expect("write_or_print_bytes should write the file");

        let read_back = tokio::fs::read(&path).await.expect("read back");
        let _ = tokio::fs::remove_file(&path).await;

        assert_eq!(
            read_back, payload,
            "file bytes must match the payload exactly"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_or_print_writes_output_file_with_tokio_fs() {
        let path = std::env::temp_dir().join(format!(
            "tinybrowser-fetch-output-test-{}.txt",
            std::process::id()
        ));
        let _ = tokio::fs::remove_file(&path).await;

        write_or_print("rendered output".to_string(), Some(&path))
            .await
            .expect("write output file");

        let content = tokio::fs::read_to_string(&path)
            .await
            .expect("read output file");
        let _ = tokio::fs::remove_file(&path).await;

        assert_eq!(content, "rendered output");
    }

    #[test]
    fn default_filter_is_warn() {
        assert_eq!(select_log_filter(false, false), "warn");
    }

    #[test]
    fn verbose_filter_is_debug() {
        assert_eq!(select_log_filter(true, false), "debug");
    }

    #[test]
    fn quiet_filter_is_off() {
        assert_eq!(select_log_filter(false, true), "off");
    }

    #[test]
    fn verbose_wins_over_quiet() {
        assert_eq!(select_log_filter(true, true), "debug");
    }

    #[test]
    fn parsed_fetch_with_quiet_flag_is_detected() {
        let args = Args::try_parse_from(["tinybrowser", "fetch", "--quiet", "https://example.com"])
            .expect("clap should accept --quiet on fetch");
        assert!(is_quiet_command(&args.command));
    }

    #[test]
    fn parsed_fetch_without_quiet_is_not_detected() {
        let args = Args::try_parse_from(["tinybrowser", "fetch", "https://example.com"])
            .expect("clap should accept fetch without --quiet");
        assert!(!is_quiet_command(&args.command));
    }

    #[test]
    fn parsed_serve_command_is_not_quiet() {
        let args =
            Args::try_parse_from(["tinybrowser", "serve"]).expect("clap should accept serve");
        assert!(!is_quiet_command(&args.command));
    }

    #[test]
    fn no_subcommand_is_not_quiet() {
        assert!(!is_quiet_command(&None));
    }

    #[test]
    fn parsed_fetch_quiet_resolves_to_off_filter() {
        let args = Args::try_parse_from(["tinybrowser", "fetch", "--quiet", "https://example.com"])
            .unwrap();
        let filter = select_log_filter(args.verbose, is_quiet_command(&args.command));
        assert_eq!(filter, "off");
    }

    #[test]
    fn fetch_wait_distinguishes_adaptive_default_from_fixed_delay() {
        let default =
            Args::try_parse_from(["tinybrowser", "fetch", "https://example.com"]).unwrap();
        match default.command {
            Some(Command::Fetch { wait, .. }) => assert_eq!(wait, None),
            _ => panic!("expected Fetch command"),
        }

        let fixed =
            Args::try_parse_from(["tinybrowser", "fetch", "https://example.com", "--wait", "0"])
                .unwrap();
        match fixed.command {
            Some(Command::Fetch { wait, .. }) => assert_eq!(wait, Some(0)),
            _ => panic!("expected Fetch command"),
        }
    }

    fn configured_fetch_timeout(args: Args) -> std::time::Duration {
        let timeout = match args.command {
            Some(Command::Fetch { timeout, .. }) => timeout,
            _ => panic!("expected Fetch command"),
        };
        let context =
            std::sync::Arc::new(tinybrowser_core::BrowserContext::with_storage_and_network(
                "cli-timeout-test".to_string(),
                None,
                None,
                None,
                true,
            ));
        let mut page = tinybrowser_core::Page::new("cli-timeout-test".to_string(), context);
        configure_fetch_navigation_timeout(&mut page, timeout);
        page.navigation_timeout()
    }

    #[test]
    fn fetch_timeout_sets_the_page_navigation_budget() {
        let args = Args::try_parse_from([
            "tinybrowser",
            "fetch",
            "https://example.com",
            "--timeout",
            "50",
        ])
        .unwrap();
        assert_eq!(
            configured_fetch_timeout(args),
            std::time::Duration::from_secs(50)
        );
    }

    #[test]
    fn fetch_default_navigation_budget_remains_thirty_seconds() {
        let args = Args::try_parse_from(["tinybrowser", "fetch", "https://example.com"]).unwrap();
        assert_eq!(
            configured_fetch_timeout(args),
            std::time::Duration::from_secs(30)
        );
    }

    #[test]
    fn matcher_still_uses_fetch_variant() {
        let cmd = Some(Command::Fetch {
            url: Some("https://x".to_string()),
            dump: Some(super::DumpFormat::Html),
            selector: None,
            file: None,
            concurrency: std::num::NonZeroUsize::new(1).unwrap(),
            wait: Some(5),
            timeout: 30,
            wait_until: "load".to_string(),
            user_agent: None,
            eval: None,
            quiet: true,
            output: None,
            storage_dir: None,
        });
        assert!(is_quiet_command(&cmd));
    }

    fn body_text(html: &str) -> String {
        let dom = parse_html(html);
        let body = dom
            .query_selector("body")
            .ok()
            .flatten()
            .expect("body must exist");
        extract_readable_text(&dom, body)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn skips_nav_header_footer_aside() {
        let text = body_text(
            r"<html><body>
                <header>SITE HEADER</header>
                <nav>NAV LINKS</nav>
                <aside>SIDEBAR</aside>
                <main><p>Article body.</p></main>
                <footer>FOOTER</footer>
            </body></html>",
        );
        assert!(text.contains("Article body."), "main content kept: {text}");
        for boilerplate in ["SITE HEADER", "NAV LINKS", "SIDEBAR", "FOOTER"] {
            assert!(
                !text.contains(boilerplate),
                "boilerplate '{boilerplate}' leaked through: {text}"
            );
        }
    }

    #[test]
    fn still_skips_script_and_style() {
        // Regression guard for the original skip list.
        let text = body_text(
            r#"<html><body>
                <p>Hello.</p>
                <script>console.log("nope")</script>
                <style>.x { color: red }</style>
            </body></html>"#,
        );
        assert!(text.contains("Hello."));
        assert!(!text.contains("console.log"));
        assert!(!text.contains("color: red"));
    }

    #[test]
    fn command_proxy_overrides_global_proxy() {
        let proxy = merge_proxy(
            Some("http://global.example:8080".to_string()),
            Some("socks5://127.0.0.1:1080".to_string()),
        );

        assert_eq!(proxy.as_deref(), Some("socks5://127.0.0.1:1080"));
    }

    #[test]
    fn global_proxy_is_used_when_command_proxy_is_absent() {
        let proxy = merge_proxy(Some("http://global.example:8080".to_string()), None);

        assert_eq!(proxy.as_deref(), Some("http://global.example:8080"));
    }

    #[test]
    fn parsed_fetch_dump_assets_is_accepted_by_clap() {
        let args = Args::try_parse_from([
            "tinybrowser",
            "fetch",
            "--dump",
            "assets",
            "https://example.com",
        ])
        .expect("clap should accept --dump assets");
        match args.command {
            Some(Command::Fetch { dump, .. }) => {
                assert_eq!(dump, Some(DumpFormat::Assets));
            }
            _ => panic!("expected Fetch command"),
        }
    }

    #[test]
    fn resolve_asset_url_keeps_absolute_unchanged() {
        let base = url::Url::parse("https://page.test/a/b").unwrap();
        let abs = "https://cdn.test/x.js";
        assert_eq!(resolve_asset_url(abs, Some(&base)).as_deref(), Some(abs));
    }

    #[test]
    fn resolve_asset_url_joins_relative_against_base() {
        let base = url::Url::parse("https://page.test/a/b").unwrap();
        let rel = "/static/x.js";
        assert_eq!(
            resolve_asset_url(rel, Some(&base)).as_deref(),
            Some("https://page.test/static/x.js"),
        );
    }

    #[test]
    fn resolve_asset_url_drops_empty() {
        let base = url::Url::parse("https://page.test/").unwrap();
        assert!(resolve_asset_url("", Some(&base)).is_none());
        assert!(resolve_asset_url("   ", Some(&base)).is_none());
    }

    #[test]
    fn link_kind_from_rel_handles_common_values() {
        assert_eq!(link_kind_from_rel("stylesheet"), "stylesheet");
        assert_eq!(link_kind_from_rel("icon"), "icon");
        // First token wins for multi-token rel (e.g. "shortcut icon").
        assert_eq!(link_kind_from_rel("shortcut icon"), "icon");
        assert_eq!(link_kind_from_rel("manifest"), "manifest");
        assert_eq!(link_kind_from_rel("preload"), "preload");
        assert_eq!(link_kind_from_rel("prefetch"), "prefetch");
        assert_eq!(link_kind_from_rel("modulepreload"), "modulepreload");
        assert_eq!(link_kind_from_rel("dns-prefetch"), "dns-prefetch");
        assert_eq!(link_kind_from_rel("preconnect"), "preconnect");
        assert_eq!(link_kind_from_rel("alternate"), "alternate");
        // Empty / unknown falls back to generic "link" so URL is still emitted.
        assert_eq!(link_kind_from_rel(""), "link");
        assert_eq!(link_kind_from_rel("noopener"), "link");
    }

    #[test]
    fn extract_assets_covers_every_resource_tag() {
        let html = r#"<html><head>
            <link rel="stylesheet" href="/site.css">
            <link rel="icon" href="/favicon.ico">
            <link rel="preload" href="/font.woff2">
            <link href="/no-rel.css">
            <script src="/app.js"></script>
        </head><body>
            <img src="/logo.png">
            <iframe src="/frame.html"></iframe>
            <video src="/clip.mp4"><source src="/clip.webm"></video>
            <audio src="/track.mp3"></audio>
            <embed src="/widget.swf">
            <object data="/doc.pdf"></object>
        </body></html>"#;
        let dom = tinybrowser_dom::parse_html(html);
        let base = url::Url::parse("https://example.test/page").unwrap();
        let ndjson = extract_assets(&dom, Some(&base));
        let records: Vec<serde_json::Value> = ndjson
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line must be valid JSON"))
            .collect();

        // Every emitted record must have an absolute URL on example.test
        // and a non-empty type string. Pin specific entries so a regression
        // in selectors or kind mapping fails loudly.
        for r in &records {
            let url = r["url"].as_str().unwrap();
            assert!(
                url.starts_with("https://example.test/"),
                "url not absolute: {url}",
            );
            assert!(!r["type"].as_str().unwrap().is_empty());
        }

        let pairs: Vec<(String, String)> = records
            .iter()
            .map(|r| {
                (
                    r["url"].as_str().unwrap().to_string(),
                    r["type"].as_str().unwrap().to_string(),
                )
            })
            .collect();

        assert!(pairs.contains(&(
            "https://example.test/app.js".to_string(),
            "script".to_string(),
        )));
        assert!(pairs.contains(&(
            "https://example.test/site.css".to_string(),
            "stylesheet".to_string(),
        )));
        assert!(pairs.contains(&(
            "https://example.test/favicon.ico".to_string(),
            "icon".to_string(),
        )));
        assert!(pairs.contains(&(
            "https://example.test/font.woff2".to_string(),
            "preload".to_string(),
        )));
        assert!(pairs.contains(&(
            "https://example.test/no-rel.css".to_string(),
            "link".to_string(),
        )));
        assert!(pairs.contains(&(
            "https://example.test/logo.png".to_string(),
            "image".to_string(),
        )));
        assert!(pairs.contains(&(
            "https://example.test/frame.html".to_string(),
            "iframe".to_string(),
        )));
        assert!(pairs.contains(&(
            "https://example.test/clip.mp4".to_string(),
            "video".to_string(),
        )));
        assert!(pairs.contains(&(
            "https://example.test/clip.webm".to_string(),
            "media".to_string(),
        )));
        assert!(pairs.contains(&(
            "https://example.test/track.mp3".to_string(),
            "audio".to_string(),
        )));
        assert!(pairs.contains(&(
            "https://example.test/widget.swf".to_string(),
            "embed".to_string(),
        )));
        assert!(pairs.contains(&(
            "https://example.test/doc.pdf".to_string(),
            "object".to_string(),
        )));
    }

    #[test]
    fn extract_assets_skips_empty_attributes() {
        let html = r#"<html><body>
            <script src=""></script>
            <img src="   ">
            <iframe src="/ok.html"></iframe>
        </body></html>"#;
        let dom = tinybrowser_dom::parse_html(html);
        let base = url::Url::parse("https://example.test/").unwrap();
        let ndjson = extract_assets(&dom, Some(&base));
        let lines: Vec<&str> = ndjson.lines().collect();
        // Only the iframe with a non-empty src survives.
        assert_eq!(lines.len(), 1, "got {lines:?}");
        assert!(lines[0].contains("\"https://example.test/ok.html\""));
        assert!(lines[0].contains("\"iframe\""));
    }
}
