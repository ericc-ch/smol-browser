# tinybrowser Domain Terms

## Terms

**the native DOM**:
The Rust-native `DomTree` in `tinybrowser-dom` backed by a generational arena (`slotmap`) and bound directly to QuickJS via `rquickjs::class` as `Node`, `Element`, `Document`, and `EventTarget`.
_Avoid_: the JS DOM, bootstrap DOM

**the shim**:
The minimal (<500 line) JavaScript standard library polyfills loaded in a private IIFE closure to provide Web APIs that require no Rust hooks (e.g. `URLSearchParams`, simple standard utilities).
_Avoid_: the 14k-line bootstrap, bootstrap.js

**Boundary A**:
The seam between page JavaScript and native Rust host capabilities. Unforgeable and private; never exposed as globals (`window.Deno` is forbidden). Calls are direct typed FFI through `rquickjs::class` bindings and closure-passed host capabilities.

**Boundary B**:
The seam between an agent's automation code and the browser process. CDP over WebSocket, routed asynchronously to `PageActor`s without blocking other tabs.

**PageActor**:
One Tokio task (or `LocalSet`) per tab. Owns that page's `QuickJsRuntime` and `DomTree` on the local task (QuickJS is `!Send`; JS on a page stays sequential). Network I/O runs on the shared multi-threaded Tokio runtime. CDP and other callers talk to the actor only via `Send` `mpsc` messages (`PageCommand`).

**the obstacle course**:
The 33-stage end-to-end suite. It drives `tinybrowser fetch` against local fixtures and checks results. It is the daily gate (`e2e/run.sh`).

**the gate**:
The pass condition for any architectural change: obstacle course results at or above the baseline (32/33 or 33/33 pass), with zero regressions.

**generational DOM index**:
A `NodeId` combining a 32-bit slot index and a 32-bit generation counter (`slotmap`). Prevents the ABA wrapper reuse bug when nodes are removed and newly created nodes occupy recycled arena slots.

**stealth**:
Presenting a clean, real Chrome browser fingerprint (TLS ClientHello, cipher suites, ALPN, navigator properties, timezone, and header ordering) with zero engine-leaking global identifiers (`window.Deno`, `__tinybrowser_*`).
