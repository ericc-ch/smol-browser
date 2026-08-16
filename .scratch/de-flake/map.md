# De-flake the verify loop

Destination: Every wall-clock budget assert removed from unit tests; the
known cross-thread delivery bug fixed; 5 consecutive green full-suite runs
(+ 1 stress run) on this 4-core machine.

Notes:
- AGENTS.md verify: workspace nextest (exclude obscura-render) + `e2e/run.sh`.
- 4-core box; `--test-threads 8` oversubscription surfaced 3 flakes (kept as stress probe).
- Code-conventions skill: tests through public boundaries; assert observable behavior, not implementation timing.
- No "seam" jargon; no fake-clock engine surgery unless a case needs it.
- Wayfinder effort; decisions below are one question each.

Decisions so far:
- [01-verify-loop-destination](./tickets/01-verify-loop-destination.md): fix all Class A (cross-thread delivery race) and Class B (wall-clock budget asserts); observer-intersection e2e stage is a feature gap, out of scope; proof = 5 green runs + stress run.

Not yet specified (deferred — noted, no ticket yet):
- 02: approach shape — channel-driven fixtures + ordering asserts + generous caps (recommended) vs fake clock (rejected: wrong tool for I/O tests, invasive).
- 04: Class D (page.rs 200ms negative window) — leave (can only false-pass) vs poll-with-cap.
- 05: watchdog/times-out tests — audit margins, keep with generous caps.

Out of scope:
- observer-intersection e2e stage (consistent feature-gap fail, not a flake).
- WPT / web-conformance harness (future; Tier 3 unit tests migrate then).
- e2e stage runtime (~118s, scale later if wanted).
