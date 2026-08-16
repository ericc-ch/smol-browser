# 01: Destination

Type: grilling

Question: What is in this dead-code cut, and what stays?

Answer: In: every `#[cfg(feature = "render")]` and `obscura_render` path in the seven live crates; the no-op `set_v8_flags` / CLI `--v8-flags` surface; finished scratch (`.scratch/strip-obscura/` now; `.scratch/rename-tinybrowser/` after the rename commit). Out: docs rewrite, NOTICE/AGENTS attribution, the e2e GitHub link fixture, `.scratch/de-flake/`. The per-connection `v8_lock` is live (QuickJS isolate affinity on one CDP thread) and is not deleted.
