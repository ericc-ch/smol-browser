# 14 — Watchdog interrupt

What to build: `arm_watchdog` / `disarm_watchdog` / `cancel_termination` on the QuickJS runtime interrupt hung JavaScript via the QuickJS interrupt handler. Control returns to Rust. The network thread is not killed.

Blocked by: 12 Hand-rolled loop, timers, posted tasks

Status: open

- [ ] A nextest `evaluate` of a tight `while (true) {}` with a short watchdog budget returns an error instead of hanging
- [ ] After the watchdog fires, `cancel_termination` (or equivalent) makes the runtime usable for a later `evaluate`
- [ ] `disarm_watchdog` before the budget elapses leaves a normal `evaluate` succeeding
- [ ] An in-flight `op_fetch_url` on the network thread is not aborted by a JS interrupt
- [ ] `IsolateHandle` / `spawn_watchdog` still exist as names CDP can call; the V8 type may be replaced
- [ ] bootstrap.js is not modified
