# 01 — rquickjs spike

Type: prototype

Question: Can rquickjs/quickjs-ng serve bootstrap.js's three Deno.core surfaces — sync ops closures (string-in/string-out), async op promises (op_fetch_url shape), and queueUserTimer/cancelTimer — from a hand-rolled loop, so bootstrap.js runs unmodified?

Answer: YES, proven 2026-08-12 (throwaway crate at /tmp/opencode/rquickjs-spike, rquickjs 0.12.2 with vendored quickjs-ng):
1. Sync ops: `Function::new` closures with String/u32/bool params work exactly as designed (op_dom, op_async_runtime_available, op_console_msg proven).
2. Async ops: closure creates a Promise (`Promise::new`), stores the resolve Function via `Persistent::save`, a thread sends the result over a channel, the hand-rolled loop resolves and pumps quickjs pending jobs with `ctx.execute_pending_job()`. The `await` continuation runs.
3. queueUserTimer/cancelTimer: reimplemented as closures over a timer map; fired and cancelled correctly.
4. bootstrap.js (14,618 lines, unmodified) evaluates and initializes on quickjs-ng: window, document, document.createElement, getElementById, navigator, location, EventTarget all present. GATE PASS.
5. Op inventory: 46 referenced -> ~28 required, 18 optional (the whole render family: layout/scroll/computed-style/IO/resize/canvas/image/waapi is typeof- or variable-guarded and can be omitted).

Port-relevant API findings:
- Plain closures: up to 8 params. Ctx-as-first-param closures need explicit `Ctx<'js>`/`Value<'js>` lifetime annotations (compiler variance errors otherwise). 7-arg op_fetch_url did NOT compile as a plain closure even with MutFn; use `Rest<String>` variadic tail (proven) or a JS adapter.
- `rt.execute_pending_job()` panics with "RefCell already borrowed" from a closure/loop context; `ctx.execute_pending_job()` is lock-free and works.
- `Persistent::save` erases the lifetime to 'static, so timer/resolver maps can live in plain structs.

Next: the swap follows krishn03id's 8-step order (ticket 04), starting from this inventory.
