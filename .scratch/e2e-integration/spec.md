# E2E integration

Problem: The daily behavioral gate lives in another repo. Agents cannot run it from this tree, and a 32/33 result still exits 1 because one parked stage fails.

Solution: Copy the obstacle course into this repo. Drive it with a debug, no-render CLI. Treat `observer-intersection` as expected fail so the runner exits 0 when every required stage passes. One script is the daily command.

User stories:
1. As someone changing the engine, I want to run the course in this repo, so I do not need `obscura-benchmark` checked out.
2. As someone iterating, I want a debug build and a single timed fetch per stage, so the gate is fast to compile and run.
3. As an agent finishing a change, I want `e2e/run.sh` to exit 0 at 32/33, so I do not have to remember a human exception.

Implementation decisions:
- Copy `obstacle-course/` from `h4ckf0r0day/obscura-benchmark` into `e2e/obstacle-course/`. Own it here. No submodule, no subtree, no auto-sync.
- Copy the whole tree, including `vendor/` (React / Preact / Vue UMD). Those are fixtures for the framework stages, not a product dependency.
- Record upstream repo URL and commit SHA in `e2e/obstacle-course/README.md`. Apache-2.0 via the existing repo license. No ORIGIN file.
- Mark `observer-intersection` `expected_fail` in the manifest. Patch the runner so exit 0 means every required stage passed. Keep running that stage; print FAIL or PASS. A later PASS does not raise the gate to 33/33. Do not change `expect` to empty.
- Daily wrapper `e2e/run.sh`: `cargo build -p obscura-cli --bins --no-default-features`, then the course with `--runs 1 --warmup 0`. `AGENTS.md` step 3 is: that script exits 0.
- Raw `run.py` stays for `--filter` and `--json`.

Testing decisions:
- After the copy, `e2e/run.sh` exits 0 with 32 required stages passing and `observer-intersection` allowed to fail.
- Do not treat stage timings as a pass/fail.

Out of scope:
- WPT, realworld, reliability, stealth-bench, compare, perf-bench
- Fixing `observer-intersection`
- Speed numbers as a gate
- CI
- Render-repros

Notes:
- Decisions: `.scratch/e2e-integration/tickets/`
- Product map: `AGENTS.md`
