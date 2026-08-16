We are building the smallest headless browser for AI agents. Engine stops at DOM + JS. No layout or screenshots.

Forked from h4ckf0r0day/obscura

## Verify

A change is done when:

1. `cargo nextest run -p <crate>` is green for every crate you changed.
2. `cargo nextest run --no-fail-fast --workspace --exclude obscura-render` is green.
3. `e2e/run.sh` exits 0 (32 required stages; `observer-intersection` may fail).

## Further Reading

- `CONTEXT.md`
- `docs/adr/`
- `.scratch/initial-prototype/`
