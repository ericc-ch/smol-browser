# Strip obscura

Problem: The binary is ~74MB (pre-QuickJS baseline) and the tree carries crates the
core vision doesn't need: a 66.5k-line renderer, an MCP server, and a scrape command
that spawns a sidecar worker binary. We want the smallest binary that still passes the
e2e gate, with the tree holding only what shipping needs.

Solution: Delete obscura-render, obscura-mcp, and the scrape command (+ its worker
binary) from the tree — git history is the archive. Repair the workspace (feature
chains, vendored patches, bins), measure the real no-render size, then trim
dependencies toward ~10MB while keeping the e2e gate green (33 required stages).

User stories:
1. As a maintainer, I want `cargo build -p obscura-cli` to produce one binary with no
   sidecar, so that packaging is trivial.
2. As a maintainer, I want the no-render binary as small as the dep stack allows, so
   that the product stays lean (10MB is the aspiration).
3. As a maintainer, I want the daily gate (`e2e/run.sh`, 33 stages) green after the
   cuts, so that stripping never breaks the obstacle course.
4. As a maintainer, I want render/mcp/scrape recoverable from git history, so that
   deleting them is reversible.

Implementation decisions:
- Delete `crates/obscura-render/` (66.5k lines; deps: taffy, tiny-skia, ab_glyph,
  cosmic-text, image, resvg, usvg, ureq, wuff) and its `paint` feature.
- Delete `crates/obscura-mcp/` (2.6k lines; CLI `mcp` subcommand) and the CLI `scrape`
  subcommand + `worker.rs` (obscura-worker bin).
- Remove the `render` feature chain everywhere (workspace members, feature lists) and
  the vendored `taffy`/`cosmic-text` patches.
- Keep 7 crates: obscura-dom, obscura-net, obscura-js, obscura-browser, obscura-cdp,
  obscura-cli, obscura (lib wrapper). CLI keeps `serve` + `fetch` (e2e gate drives
  fetch; CDP boundary needs serve).
- No extension/mod architecture work in this effort (deferred).

Testing decisions:
- `cargo nextest run -p <crate>` per changed crate; workspace run excluding render.
- `e2e/run.sh` green (33 required stages; `observer-intersection` documented fail).
- Record binary size (release, no-render) and peak RSS before and after trimming;
  size target is set from measurement, 10MB is the aspiration.
- cli unit tests for removed subcommands are deleted with them.

Out of scope:
- Extension/mod system (deferred; see tickets/01-extension-structure.md)
- Future renderer planning
- Sub-1MB binary
- `observer-intersection` e2e stage (feature gap)

Notes:
- Decisions: tickets/01-extension-structure.md, tickets/02-archive-mechanics.md
- AGENTS.md verify steps are the gate for this effort.
