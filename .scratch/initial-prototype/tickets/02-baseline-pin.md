# 02 — Baseline pin and gate

Type: task

Question: Pin upstream and establish the baseline gate before the swap — which commit, where does it build, what numbers do we record?

Answer: Pin current HEAD (6750d7d, tagged `baseline-pin`), never pull past it. Work from here on out. Build on this machine (this box; no companion machine, ignore the brief's companion/throwaway layout — this machine's layout is ~/projects). Gate = inherited obstacle course 33/33 from obscura-benchmark (cloned to ../obscura-benchmark, i.e. /home/erickc/projects/obscura-benchmark). Record scoreboard: release binary size + RSS + obstacle course result before the swap.

Baseline measured 2026-08-12 on this machine (release, this repo at baseline-pin):
- Binary size: 74 MB no-render (`--no-default-features`), 90 MB render build (both unstripped)
- Peak RSS on a local fetch: ~21.8 MB (no-render build)
- Obstacle course: 32/33 — `observer-intersection` fails consistently on both build variants (expected 'io:50', got ''). No-render shim path exists (compatibility geometry) but the IO callback never fires on this pin. Upstream claims 33/33; delta open: runner-invocation mismatch vs real regression. Decide separately whether to fix in the swap.
