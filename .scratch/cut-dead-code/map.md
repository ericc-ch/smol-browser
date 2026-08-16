# Cut dead code

Destination: Delete every `#[cfg(feature = "render")]` / `obscura_render` path from the live crates; delete the no-op `--v8-flags` / `set_v8_flags` surface; rename the per-connection `v8_lock` to `js_lock`; patch the AGENTS.md CLI line; delete finished scratch (`.scratch/strip-obscura/`; `.scratch/rename-tinybrowser/` after the rename commit). Verify loop stays green.

Notes:
- AGENTS.md verify: crate nextest, workspace nextest (no `obscura-render` exclude; that crate is gone), `e2e/run.sh` (33 required stages).
- `obscura-render` crate is already gone; leftover cfg never compiles.
- Wayfinder effort; one decision per ticket. Do not write feature code while wayfinding.

Decisions so far:
- [01-destination](./tickets/01-destination.md): render cfg, dead V8 flags API, finished scratch in; live lock stays; docs/attribution/de-flake out.
- [02-rename-js-lock](./tickets/02-rename-js-lock.md): `v8_lock` → `js_lock`, `is_v8_free_method` → `is_js_free_method`; mutex stays; docs rewrite still out.
- [03-paint-cdp-explicit-fail](./tickets/03-paint-cdp-explicit-fail.md): screenshot / PDF / screencast stay named arms; `-32601` with captureSnapshot-style "no paint" message; no fake success.
- [04-delete-rename-scratch](./tickets/04-delete-rename-scratch.md): `.scratch/rename-tinybrowser/` goes in this cut once the rename commit exists.
- [05-agents-md-cli](./tickets/05-agents-md-cli.md): AGENTS.md CLI line drops `scrape`/`mcp`; fork line stays; README/`docs/` stay.

Not yet specified:

Out of scope:
- Docs rewrite
- NOTICE / AGENTS.md upstream attribution (`Forked from h4ckf0r0day/obscura`)
- e2e fixture expected GitHub `h4ckf0r0day/obscura` link
- `.scratch/de-flake/` (still live)
- Implementing a renderer
