# Cut dead code

Problem: The render crate and V8 flag plumbing are already gone, but the tree still carries `#[cfg(feature = "render")]` paint paths that never compile, a no-op `--v8-flags` surface, V8 names on a live JS lock, and finished scratch from earlier cuts. Agents still read a CLI line that lists deleted `scrape`/`mcp` commands.

Solution: Delete the dead render cfg and V8-flags API, rename the per-connection lock to `js_lock`, keep paint CDP methods as explicit failures, patch the AGENTS.md CLI line, and delete finished scratch once the rename commit exists. Verify stays green.

User stories:
1. As a maintainer, I want leftover render cfg gone, so that a deleted crate does not still own thousands of lines in the live tree.
2. As an agent client, I want `Page.captureScreenshot` / `printToPDF` / screencast to fail with a clear "no paint" error, so that Playwright does not see "Unknown Page method" or a fake image.
3. As a CLI user, I want `--v8-flags` gone, so that a QuickJS engine does not advertise a V8 switch that does nothing.
4. As an agent, I want AGENTS.md to list only `serve` and `fetch`, so that I do not try to build deleted subcommands.

Implementation decisions:
- Delete every `#[cfg(feature = "render")]` / `#[cfg(not(feature = "render"))]` / `obscura_render` path in the seven live crates. The `render` Cargo feature is already absent.
- Keep named CDP arms for `Page.captureScreenshot`, `Page.printToPDF`, and screencast start/stop/ack. They return JSON-RPC `-32601` with a `Page.captureSnapshot`-style message: the method exists, there is no layout or paint engine. Do not mention a render feature. Do not return `{ data }`.
- Delete raster PDF and screenshot implementation (including the core `pdf` module and JS re-exports of the old render crate). Screencast session state and the server tick go with the cfg.
- Delete `set_v8_flags`, the `v8_flags` module, CLI `--v8-flags`, defaults, and the clap tests that only exist for that flag. Callers should stop compiling against the name.
- Rename `v8_lock` → `js_lock`, `is_v8_free_method` → `is_js_free_method`, plus matching locals and comments in CDP crates/tests. The mutex stays. Do not rewrite `docs/`.
- AGENTS.md: CLI line is `serve` and `fetch` only. Keep `Forked from h4ckf0r0day/obscura`.
- Delete `.scratch/strip-obscura/` in this cut. Delete `.scratch/rename-tinybrowser/` in this cut after the crate-rename commit exists.

Testing decisions:
- Keep the Page-domain tests that require captureScreenshot / printToPDF not to fall through as unknown methods; point them at the new "no paint" message.
- Delete tests that only compiled under `feature = "render"` (screenshot, PDF raster, screencast pumps).
- Drop CLI tests whose only job is `--v8-flags` parsing.
- Gate: `cargo nextest run -p <crate>` for every changed crate; `cargo nextest run --no-fail-fast --workspace`; `e2e/run.sh` (33 required stages; `observer-intersection` may fail).

Out of scope:
- README / `docs/` rewrite (V8, `--features render`, `--v8-flags` text stays stale)
- NOTICE / AGENTS.md upstream attribution
- e2e fixture expected GitHub `h4ckf0r0day/obscura` link
- `.scratch/de-flake/`
- Implementing a renderer
- Fake screenshot/PDF success payloads

Notes:
- Decisions: tickets/01 through 05 under `.scratch/cut-dead-code/tickets/`
- The crate rename (`obscura*` → `tinybrowser-*`) is a separate uncommitted change; this cut sits on top of it and only deletes rename scratch after that commit exists.
