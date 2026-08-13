# 16 — Obstacle course gate and scoreboard

What to build: The swapped no-render release binary meets the gate. The scoreboard is recorded next to the baseline. The parked IO fixture is re-run on our scheduler.

Blocked by: 15 Cut ObscuraJsRuntime over to QuickJS

Status: open

- [ ] Release build: `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 cargo build --release -p obscura-cli --bins --no-default-features`
- [ ] Obstacle course against that binary is at least 32/33 (baseline)
- [ ] The `observer-intersection` fixture is re-run; pass or fail is recorded (a pass is allowed; a fail matching baseline is allowed; do not debug the old deno_core scheduler)
- [ ] Scoreboard written: no-render unstripped binary size and peak RSS on a local fetch, compared to 74 MB and ~21.8 MB
- [ ] WPT Core subtest pass percent recorded at this phase boundary
- [ ] `cargo nextest run --release --no-default-features --no-fail-fast` passes (render feature is out of scope for this gate)
