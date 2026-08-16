# Initial prototype (engine swap)

Problem: Obscura already fetches pages, runs real JavaScript, and speaks CDP. V8 via deno_core is most of the binary. The first product move is a smaller engine. The rest of the browser (DOM, network, CDP, the shim) must keep working.

Solution: Replace V8/deno_core with quickjs-ng through rquickjs, inside obscura-js. Leave bootstrap.js unmodified. Keep the Page / ObscuraJsRuntime seam so fetch, eval, navigation, and CDP still call the same methods. Gate the work on the inherited obstacle course and record binary size plus RSS before and after.

User stories:
1. As someone running `obscura fetch` or a CDP client, I want pages to still load and run JavaScript after the engine change, so my existing workflows keep working.
2. As the person doing the swap, I want bootstrap.js left unmodified, so we do not maintain a forked shim.
3. As the person doing the swap, I want the obstacle course to stay at or above the pinned baseline after every slice, so a broken step is visible the day it lands.
4. As someone watching size, I want a recorded scoreboard of release binary size and peak RSS before and after the swap, so dropping V8 is a measured win.

Implementation decisions:
- Pin the fork at `6750d7d` (`baseline-pin`). Never pull past it. Build on this machine. The benchmark repo lives at `../obscura-benchmark`.
- The swap lives in place inside obscura-js. The crate keeps its name. ObscuraJsRuntime keeps the methods Page and CDP already call: construct, set DOM/URL/cookies/client, `run_page_init`, `evaluate`, `execute_script`, bounded event-loop pumping, `arm_watchdog` / `disarm_watchdog`. CDP, browser, DOM, and net crates stay untouched (ADR-0002).
- V8-only helpers (`set_v8_flags`, isolate snapshot, `IsolateHandle` as a V8 type) may become no-ops or a QuickJS-shaped handle. Callers keep compiling against the same method names.
- Register `Deno.core.ops` from Rust with rquickjs `Function::new` closures so bootstrap.js runs unmodified. Ops stay string-in / string-out. The spike proved this on rquickjs 0.12.2 with vendored quickjs-ng: unmodified bootstrap.js evaluated; `window`, `document`, `createElement`, `getElementById`, `navigator`, `location`, and `EventTarget` were present.
- Bind the ops the shim actually needs (~28). The render family (~18: layout, scroll, computed style, IO, resize, canvas, image, WAAPI) is typeof-guarded in the shim and can be omitted from the no-render build.
- `op_fetch_url` is too many arguments for a plain rquickjs closure. Use a `Rest<String>` tail or a small JS adapter. Pump pending jobs with `ctx.execute_pending_job()`, not `rt.execute_pending_job()`. Hold timer and promise resolvers with `Persistent::save`.
- Engine loop is hand-rolled threads and channels, not tokio (ADR-0001). HTTP work (today's obscura-net client; rquest is a later phase) runs on its own thread with its own tokio runtime. Results cross back on a channel. The isolate thread wakes, resolves the QuickJS promise, and pumps jobs. The watchdog interrupts JavaScript only, never the network thread. Use the QuickJS interrupt handler as the V8 `terminate_execution` stand-in. A hung page must not hang the process.
- Ops must not unwind into the JS engine. Keep the catch-unwind wrapper so a panic becomes an error return.
- Follow the proven deno_core-to-rquickjs slice order after the spike: ops inventoried (done), skeleton inside obscura-js, ops bound, runtime ported, gate. Keep the course green at every step. Do not land the whole port as one diff.
- Accept no `Intl` in this prototype (ADR-0003). Add a small `Intl.DateTimeFormat` shim later only if a test or a site needs it.
- Cookie persist/load already exists in obscura-net. Keep it. No new work.
- Accessibility already exists as CDP `Accessibility.getFullAXTree` in Rust. Keep it. The swap does not touch it.
- Keep every CDP domain for this effort. Revisit stripping CDP when the SDK exists.

Testing decisions:
- Daily gate is the obstacle course in obscura-benchmark, driven against this repo's release binary. Baseline on this pin: 32/33 (`observer-intersection` fails on both render and no-render; expected `io:50`, got empty). Swap pass condition: at least 32/33. Once our timer scheduler exists, re-run that IO fixture the same way. Do not debug the deno_core/tokio scheduler we are deleting.
- Record a scoreboard before the swap and after it is green: release binary size (no-render, unstripped) and peak RSS on a local fetch. Baseline: 74 MB no-render (90 MB render), ~21.8 MB peak RSS.
- Run WPT Core at phase boundaries. Report subtest pass percent, not whole-file pass.
- Prefer checks at the ObscuraJsRuntime seam, not inside the engine: `evaluate` / `execute_script` against the unmodified shim; sync ops (`op_dom` shape); async `op_fetch_url` promises that resolve after a channel wakeup; `queueUserTimer` / `cancelTimer`; watchdog interrupt of a tight loop that returns control to Rust.
- Run tests with `cargo nextest`, not `cargo test`. One V8 (and later QuickJS) isolate per process.
- Do not pick five new target sites. The inherited suite is the gate.

Out of scope:
- Rendering, layout, and screenshots.
- Network swap to rquest and stealth-by-default (Phase 2).
- Playwright-shaped SDK over JSON-RPC stdio (Phase 3).
- Stripping CDP or MCP.
- `Intl` polyfill unless a test forces it.
- Sidecar networking and a sub-1 MB binary.
- Codemode execute.
- Five new target sites.
- Changing cookie persistence or the accessibility tree.

Notes:
- Glossary: `CONTEXT.md`. ADRs: `docs/adr/0001-hand-rolled-loop-network-thread.md`, `docs/adr/0002-in-place-engine-swap.md`, `docs/adr/0003-intl-gap-accepted.md`.
- Decisions: `.scratch/initial-prototype/tickets/` (spike, pin, IO park, slice order, loop, crate shape, Intl, cookies, a11y).
- Spike crate was throwaway (`/tmp/opencode/rquickjs-spike`). Do not import it. Rebuild the binding in obscura-js.
- Product map: `AGENTS.md`. This spec is Phase 1 only.
