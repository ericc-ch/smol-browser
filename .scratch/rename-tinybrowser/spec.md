# Rename to tinybrowser

Problem: The product is smol-browser, every crate and the binary are still named obscura, and three crates all sound like “the browser” (`obscura`, `obscura-browser`, `obscura-cli`). Maintainers cannot tell the public lib from the engine from the CLI.

Solution: Rename the product, crates, binary, env vars, and Obscura-prefixed types to tinybrowser, with role suffixes so lib / core / cli cannot be mixed up. Keep Obscura only as the upstream fork name.

User stories:
1. As a maintainer, I want crate names that match what each crate does, so that I know whether to edit the public API, the engine, or the CLI.
2. As an agent or user, I want one binary named `tinybrowser`, so that commands and docs match the product.
3. As a fork maintainer, I want NOTICE and upstream GitHub links to keep saying Obscura, so that Apache-2.0 attribution stays correct.

Implementation decisions:
- Product / binary: `tinybrowser`. No Cargo package named `tinybrowser`.
- Crate map (locked):

| Today | New | Role |
|---|---|---|
| `obscura` | `tinybrowser-lib` | Public in-process API (`use tinybrowser_lib::Browser`). Thin wrapper around core. CLI does not depend on it. |
| `obscura-browser` | `tinybrowser-core` | Engine: Page, BrowserContext, navigation, lifecycle. |
| `obscura-cli` | `tinybrowser-cli` | CLI crate. `[[bin]]` name is `tinybrowser`. |
| `obscura-js` | `tinybrowser-js` | QuickJS runtime, bootstrap.js, ops. |
| `obscura-cdp` | `tinybrowser-cdp` | CDP WebSocket + domain handlers. |
| `obscura-net` | `tinybrowser-net` | HTTP, cookies, stealth, robots, blocklist. |
| `obscura-dom` | `tinybrowser-dom` | HTML parse, tree, selectors. |

- Do not name the engine `tinybrowser-browser`. That recreates the collision.
- Env vars: `OBSCURA_*` → `TINYBROWSER_*` (including `TINYBROWSER_BIN`, `TINYBROWSER_VERSION`, `TINYBROWSER_BUILD_VERSION`).
- Types: drop the Obscura prefix rather than stamping Tinybrowser on them (`ObscuraHttpClient` → `HttpClient`, `ObscuraJsRuntime` → `JsRuntime`, `ObscuraState` → `RuntimeState`, `ObscuraNetError` → `NetError`, `ObscuraModuleLoader` → `ModuleLoader`).
- Repo docs / `NOTICE` / `CONTEXT.md` heading / skill path `skills/obscura/` → tinybrowser. GitHub remote rename is a human step; do not change `git remote` in this effort.
- Keep `https://github.com/h4ckf0r0day/obscura` (and the word Obscura) wherever it means the upstream project.
- Replacement order: longest crate names first (`obscura-browser` before `obscura`, `obscura_browser` before `obscura`) so the engine does not become `tinybrowser-lib-browser`.
- Do not merge `tinybrowser-lib` into `tinybrowser-core`. The wrapper stays.

Testing decisions:
- `cargo nextest run -p <crate>` for every renamed crate.
- `cargo nextest run --no-fail-fast --workspace` (drop `--exclude obscura-render`; that crate is already gone).
- `e2e/run.sh` exits 0 (33 required stages; `observer-intersection` may fail). Update the script to build `-p tinybrowser-cli` and default `TINYBROWSER_BIN` to `target/debug/tinybrowser`.
- Smoke: `tinybrowser fetch` still works; `use tinybrowser_lib::Browser` still compiles.

Out of scope:
- Deleting leftover `#[cfg(feature = "render")]` / V8-named internals (`v8_flags`, comments).
- Publishing to crates.io or renaming the GitHub repo / local folder.
- Setting `[lib] name = "tinybrowser"` on the lib crate. Package name and rust path stay `tinybrowser-lib` / `tinybrowser_lib`.

Notes:
- Workspace today: 7 crates under `crates/obscura*`. Docs still mention deleted `obscura-render` / `obscura-mcp`; rewrite those lines to the seven-crate map when touching docs.
- CLI talks to `tinybrowser-core` directly, not to `tinybrowser-lib`.
- Verify steps in `AGENTS.md` must be updated as part of the rename.
