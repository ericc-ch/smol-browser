# Native Rust DOM, Generational Indexing, and Actor-Based Tab Isolation

The engine architecture is overhauled to eliminate the ~14,600-line JavaScript bootstrap shim (`bootstrap.js`), untyped string-based `op_dom` RPC, thread-per-isolate runtime sprawl, and tab state destruction (`suspend_js`/`resume_js`).

Status: accepted

Context:
The previous architecture attempted to implement full DOM, CSSOM, EventTarget, and Web APIs in a 14.6k-line handwritten JavaScript shim on top of an untyped string multiplexer (`op_dom`). This introduced severe memory leaks (unbounded JS wrapper cache `_cache`), ABA node corruption from non-generational arena reuse, protocol truncation on null bytes, synchronous head-of-line blocking on CDP navigations (256-message deferred queue), thread explosion (over 60 Tokio runtimes and 100+ OS threads under minor concurrency), and global `window.Deno` exposure.

Decision:
1. **Native Rust DOM with rquickjs::class Bindings:**
   `Document`, `Element`, `Node`, and `EventTarget` are implemented natively in Rust within `tinybrowser-dom` and bound to QuickJS classes via `rquickjs::class`. `bootstrap.js` is reduced from 14,600+ lines to a minimal (<500 line) set of pure-JS polyfills.
2. **Generational DOM Arena (slotmap):**
   Nodes in `DomTree` are indexed by generational keys (`SlotKey` / `NodeId { index, generation }`). Stale JS wrappers referencing deleted nodes are instantly detected as detached, eliminating the ABA wrapper recycling bug.
3. **True Tab Isolation (1 QuickJS Runtime & Context per Page):**
   Each `Page` owns its own `QuickJsRuntime` and `DomTree`. `suspend_js` and `resume_js` (which destroyed the JS heap on tab switches) are eliminated. Background tabs remain alive with their event listeners, component state, and timers intact.
4. **Single Global Tokio Runtime & Actor Model:**
   The thread zoo (`spawn_network_thread`, `tinybrowser-js-net-recv`, and per-connection OS threads) is removed. The process has one multi-threaded Tokio runtime for I/O and CDP routing. Each page is a `PageActor`: one Tokio task (or `LocalSet`) that keeps `QuickJsRuntime` and `DomTree` local (`!Send`, sequential JS per page). Network (reqwest/wreq) runs on the shared runtime (`Send` + `Sync`). External callers send `PageCommand` over `mpsc` only.
5. **Zero-Global Host Seam:**
   `globalThis.Deno` and any exposed host ops are deleted from the global scope. Internal host capabilities are injected exclusively into a private IIFE closure parameter.
6. **Committed-Origin Security:**
   Document URL and origin are immutable from page script until navigation response headers commit, eliminating cross-origin cookie theft via `op_navigate`.

Options Considered:
- **Incremental JS shim optimization:** Keep `bootstrap.js` but fix `op_dom` FFI with binary tuples. Rejected: preserves 14,000 lines of brittle JS maintenance burden, wrapper memory leaks, and high FFI overhead.
- **Shared QuickJS runtime across all pages with multiple Contexts (Realms):** Rejected: cross-tab JS evaluation or heavy scripts in one page could still starve the single QuickJS runtime instance.

Consequences:
- `bootstrap.js` shrinks by >95%, drastically reducing engine startup time and memory footprint.
- DOM mutations and traversals execute 10x-50x faster directly in compiled Rust.
- Zero bot-detection fingerprints (`window.Deno` gone).
- CDP sessions support true concurrent multi-tab scraping and automation.
