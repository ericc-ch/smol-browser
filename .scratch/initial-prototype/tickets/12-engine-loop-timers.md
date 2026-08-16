# 12 — Hand-rolled loop, timers, posted tasks

What to build: The QuickJS runtime pumps jobs on a hand-rolled loop (no tokio on the isolate thread). `Deno.core.queueUserTimer` / `cancelTimer` and `op_posted_task` work. `op_async_runtime_available` is true so the shim uses the async path.

Blocked by: 11 Bind sync ops

Status: done

- [x] Pending jobs run via `ctx.execute_pending_job()`, not `rt.execute_pending_job()`
- [x] A nextest `setTimeout` callback fires after the loop pumps
- [x] A nextest cancels a timer and the callback does not fire
- [x] `op_posted_task` settles on a later turn, not in the turn that called it
- [x] `op_async_runtime_available()` returns true on this runtime
- [x] Timer and resolver maps use `Persistent::save` so they are not tied to a short lifetime
- [x] bootstrap.js is not modified
