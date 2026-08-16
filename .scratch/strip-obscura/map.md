# Strip obscura

Destination: Render, mcp, and scrape cut from the tree; deps trimmed; no-render binary
measured and pushed toward ~10MB; e2e gate green (33 required stages).

Notes:
- AGENTS.md verify: workspace nextest (exclude obscura-render) + `e2e/run.sh`.
- Baseline: 74MB no-render binary (pre-QuickJS), 32/33 obstacle course, ~21.8MB peak RSS.
- Measured after cuts (release, no-render): 14MB stripped (18.3MB unstripped). Trim target: ~10MB.
- Size is dep-driven: wreq/BoringSSL (stealth), rquickjs, reqwest, html5ever, tokio.
- Extension/mod architecture is deferred (see ticket 01) — do not design mod host-APIs or
  extension layouts in this effort. No future-renderer planning.
- Scrape spawns a sidecar worker binary (obscura-worker) — cut restores single-binary.
- Wayfinder effort; one decision per ticket.

Status:
- [x] Cut obscura-render + obscura-mcp + scrape/worker + render-repros (c969a20 + this commit)
- [x] Repair workspace: feature chains, vendored patches, CLI subcommands, Dockerfile, release.yml
- [x] Measure no-render size: 14MB stripped
- [ ] Trim deps toward ~10MB (next step)

Decisions so far:
- [01-extension-structure](./tickets/01-extension-structure.md): decided, then deferred —
  three-tier mod hierarchy (JS mods / build-time features / wasm) is the future shape;
  not built here.
- [02-archive-mechanics](./tickets/02-archive-mechanics.md): plain delete; git history
  is the archive, no branches or tags.

Not yet specified:
- Order of work: cut crates → fix workspace (features, patches, e2e) → measure → trim deps → verify
- Size target after measurement (10MB aspirational vs what the dep stack allows)
- Dep trim list (depends on measurement)

Out of scope:
- Extension/mod system implementation (deferred)
- Future renderer planning
- Sub-1MB (aspirational, later)
- `observer-intersection` e2e stage (feature gap)
