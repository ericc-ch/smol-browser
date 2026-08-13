# Initial prototype

Destination: quickjs-ng/rquickjs engine swap green on the inherited obstacle course; bootstrap.js running unmodified behind a registered Deno.core.ops; hand-rolled thread/channel loop + watchdog; CDP kept (all domains); binary size + RSS scoreboard recorded.

Notes:
- Brief: browser-project-brief.md (planning doc, untracked). Fork: ericc-ch/smol-browser, upstream h4ckf0r0day/obscura.
- Pinned at baseline-pin (6750d7d); never pull past it.
- Build on this machine only; ignore brief's companion-box and ~/throwaway layout. This machine's layout: ~/projects (benchmark at ../obscura-benchmark).
- Decision style: spike first, then the swap. Gate = existing test suite (obstacle course 33/33, WPT at phase boundaries). No picking 5 new target sites.
- Keep CDP entirely for now; revisit at Phase 2/3 when the SDK exists.
- Long-term stretch (out of scope for v1): sub-5MB binary; <1MB only if networking moves to a sidecar process. Embeddable Rust library API + C ABI.
- Per AGENTS.md: test with `cargo nextest`, panic-safe ops, keep robustness invariants.

Decisions so far:
- [01 rquickjs spike](./tickets/01-rquickjs-spike.md) — run the throwaway spike before the swap; prove ops closures, async op promises, queueUserTimer.
- [02 baseline pin and gate](./tickets/02-baseline-pin.md) — pinned at baseline-pin (6750d7d); build on this machine; obstacle course 33/33 baseline + size/RSS scoreboard before the swap.
- [03 observer-intersection delta](./tickets/03-observer-intersection-delta.md) — baseline is 32/33 (IO callback never fires on this pin, both builds); parked; swap gate = at least baseline, IO re-verified on our scheduler.
- [04 swap slicing](./tickets/04-swap-slicing.md) — follow krishn03id's 8-step order after the spike; keep the course green at every step.
- [05 engine loop](./tickets/05-engine-loop-network.md) — hand-rolled loop; rquest on its own thread with its own tokio; results via channel; ADR-0001.
- [06 crate shape](./tickets/06-crate-shape.md) — swap in place inside obscura-js, keep the Page/JsRuntime seam; ADR-0002.
- [07 Intl gap](./tickets/07-intl-gap.md) — accept no-Intl in the prototype; ADR-0003.
- [08 cookie persistence](./tickets/08-cookie-persistence.md) — already exists in obscura-net (persist/load to file); keep, zero work.
- [09 accessibility tree](./tickets/09-accessibility-tree.md) — already exists as CDP Accessibility.getFullAXTree in Rust; keep, swap does not touch it.

Build tickets (spec: ./spec.md):
- [10 rquickjs skeleton](./tickets/10-rquickjs-skeleton.md) — QuickJS beside V8; shim loads; Page still on V8
- [11 sync ops](./tickets/11-sync-ops.md) — required ops as closures; createElement works
- [12 engine loop](./tickets/12-engine-loop-timers.md) — hand-rolled pump, timers, posted tasks
- [13 async fetch](./tickets/13-async-fetch.md) — op_fetch_url via network thread + channel
- [14 watchdog](./tickets/14-watchdog.md) — interrupt hung JS; can run in parallel with 13
- [15 cutover](./tickets/15-cutover.md) — ObscuraJsRuntime is QuickJS; V8 gone
- [16 gate](./tickets/16-gate-scoreboard.md) — obstacle course, IO re-check, size/RSS, WPT

Not yet specified:
- None for this effort. Phase 2+3 (rquest, SDK, CDP strip) stay out of scope.

Out of scope:
- Rendering/layout/screenshots (brief's honest menu, deferred).
- codemode execute (v2).
- <1MB binary (sidecar-network v2 idea; record, don't build).
- Stripping CDP/MCP beyond CDP's current keep.
- 5 new target sites (inherited suite is the gate).
- Engine loop details: watchdog via JS_SetInterruptHandler equivalent, bounded queues.
- Cookie persistence to file (brief says non-negotiable; scope for this effort?).
- a11y snapshot (v1 or v1.5).
- Network swap to rquest + stealth-by-default (Phase 2).
- SDK shape over JSON-RPC stdio (Phase 3).

Out of scope:
- Rendering/layout/screenshots (brief's honest menu, deferred).
- codemode execute (v2).
- <1MB binary (sidecar-network v2 idea; record, don't build).
- Stripping CDP/MCP beyond CDP's current keep.
