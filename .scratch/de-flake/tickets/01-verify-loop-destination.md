# 01: Verify-loop destination

Type: grilling

Question: What does "de-flaked" mean for this effort — which tests, what proof?

Answer: Fix every Class A and Class B instance found in the suite sweep:

- Class A (cross-thread delivery race, real bug): `callbacks_do_not_bleed_across_pages` — `on_request` delivered after `drop(page_a)`.
- Class B (wall-clock budget asserts): `posted_task_chains_complete_without_zero_delay_timer_floor` (100ms pump), `quiescent_event_loop_allows_fetch_hydration_within_network_grace` (1500ms window), `quiescent_event_loop_bounds_a_hanging_page_request` (1700ms window), `fetch_http_runs_off_the_isolate_thread` (`< 100ms`), the `quiescent_event_loop_bounds_*` group, watchdog/times-out audits. Excluded: floor asserts (`>= 180ms`) kept only if load-safe.

Proof: 5 consecutive green `cargo nextest run --workspace` runs plus 1 stress run at `--test-threads 8` on this machine; `e2e/run.sh` green (33/34 required, `observer-intersection` documented fail).

Out of scope: `observer-intersection` e2e stage (feature gap, not a flake — track separately).
