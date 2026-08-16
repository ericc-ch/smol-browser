# E2E integration

Destination: The 33-stage obstacle course lives in this repo. Daily gate is a debug, no-render `obscura` binary. Pass is 32/33 (`observer-intersection` may fail). WPT, real-site, stealth-bench, and vs-Chrome stay out.

Notes:
- Product map: `AGENTS.md`. Verify step 3 names this gate but has no command yet.
- Standalone fork; no merge-clean constraint with upstream Obscura.
- Course upstream: `h4ckf0r0day/obscura-benchmark` `obstacle-course/` (Apache-2.0). Layout: fixtures, vendor, data, modules, manifest.json, run.py.
- Debug CLI: `cargo build -p obscura-cli --bins --no-default-features` then `OBSCURA_BIN=./target/debug/obscura`.
- Skills: wayfinder, writing-for-agents. Do not build until the map is empty and `/to-spec` has run.

Decisions so far:
- [Course lands as a copy](./tickets/01-course-lands-as-copy.md) — copy `obstacle-course/` into `e2e/obstacle-course/`; own it here; no submodule.
- [observer-intersection is expected_fail](./tickets/02-observer-intersection-expected-fail.md) — keep the stage; `expected_fail` in the manifest; exit 0 when required stages pass; a later IO PASS does not raise the gate.
- [Daily command is e2e/run.sh](./tickets/03-daily-command-run-sh.md) — wrapper builds debug CLI and runs `--runs 1 --warmup 0`; AGENTS.md names that script.
- [Pin and license in the course README](./tickets/04-pin-in-readme.md) — upstream URL + commit SHA in `e2e/obstacle-course/README.md`; no ORIGIN file.
- [Keep vendor UMD fixtures](./tickets/05-keep-vendor-umd.md) — copy `vendor/` with the course; framework stages stay; not a product dependency.

Not yet specified:
- None. Spec: `./spec.md`.

Out of scope:
- WPT, realworld, reliability, stealth-bench, compare, perf-bench
- Fixing `observer-intersection`
- Speed numbers as a gate
- CI unless we decide it later
- Render-repros (already in this repo)
