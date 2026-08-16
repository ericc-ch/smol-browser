# 15 — Cut ObscuraJsRuntime over to QuickJS

What to build: ObscuraJsRuntime is the rquickjs runtime. Page and CDP keep calling the same methods. deno_core, the V8 snapshot, and the V8 isolate are gone from obscura-js. Classic scripts, ES modules, and event-loop pumping that Page already uses all work. The V8 path is deleted, not left behind a flag.

Blocked by: 13 Async fetch over a channel, 14 Watchdog interrupt

Status: done

- [x] `ObscuraJsRuntime::with_base_url_and_proxy`, `set_dom`, `run_page_init`, `evaluate`, `execute_script`, `execute_script_guarded`, `prepare_module` / `evaluate_prepared_module`, `run_event_loop_bounded`, `arm_watchdog` / `disarm_watchdog` still exist
- [x] `set_v8_flags` still compiles for callers and is a no-op
- [x] obscura-js no longer depends on deno_core; the snapshot `build.rs` path is gone
- [x] `obscura fetch` of a local HTML file with an inline classic script evaluates that script
- [x] An ES module `<script type="module">` on a local page loads and runs (same cookie jar / proxy / SSRF as `op_fetch_url`)
- [x] `cargo nextest run --release --no-default-features -p obscura-js -p obscura-browser -p obscura-cdp` passes
- [x] bootstrap.js is not modified
- [x] obscura-dom, obscura-net, and CDP domain handlers are not rewritten
