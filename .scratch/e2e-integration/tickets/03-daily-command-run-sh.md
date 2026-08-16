# 03: Daily command is e2e/run.sh

Type: grilling

Question: Wrapper vs raw `run.py`, and what `AGENTS.md` names?

Answer: Daily gate is `e2e/run.sh`: build debug no-render CLI (`cargo build -p obscura-cli --bins --no-default-features`), then `run.py --runs 1 --warmup 0`. Iteration speed over speed numbers. `AGENTS.md` step 3 is: `e2e/run.sh` exits 0. Raw `run.py` stays for `--filter` and `--json`.
