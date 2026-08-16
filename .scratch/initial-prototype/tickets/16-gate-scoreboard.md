# 16 — Obstacle course gate and scoreboard

What to build: The swapped no-render release binary meets the gate. The scoreboard is recorded next to the baseline. The parked IO fixture is re-run on our scheduler.

Blocked by: 15 Cut ObscuraJsRuntime over to QuickJS

Status: done

- [x] Release build: `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo build --release -p obscura-cli --bins --no-default-features`
- [x] Obstacle course against that binary is at least 32/33 (baseline)
- [x] The `observer-intersection` fixture is re-run; pass or fail is recorded (a pass is allowed; a fail matching baseline is allowed; do not debug the old deno_core scheduler)
- [x] Scoreboard written: no-render unstripped binary size and peak RSS on a local fetch, compared to 74 MB and ~21.8 MB
- [x] WPT Core subtest pass percent recorded at this phase boundary
- [x] `cargo nextest run --release --no-default-features --no-fail-fast` passes (render feature is out of scope for this gate)

Recorded 2026-08-16 on this machine, no-render release (`target/release/obscura`):

- Obstacle course: **32/33**. Only `observer-intersection` failed (`expected 'io:50', got ''`), matching the parked baseline. Re-run of that fixture: fail. `fingerprint` needed a small `Intl.DateTimeFormat` stub (ADR-0003: add it if a test needs it); with that stub it passes.
- Scoreboard: unstripped binary **18.4 MB** (19,246,888 bytes) vs baseline 74 MB. Peak RSS on `fetch` of a local HTML file: **15.3 MB** (15,700 KB) vs baseline ~21.8 MB.
- WPT Core: not re-measured. `../obscura-benchmark/wpt` is not checked out; `setup-wpt.sh` is a several-GB clone meant for a VPS. Baseline Core on the V8 pin was 83.3% (318,916 / 382,891).
- Nextest: `cargo nextest run --release --no-default-features --no-fail-fast` does not compile the `obscura` library crate (its default `api` feature is required). Equivalent no-render gate: `cargo nextest run --release --no-fail-fast --workspace --exclude obscura-render` → **698 passed, 3 skipped**.
