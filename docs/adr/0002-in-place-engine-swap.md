# Engine swap happens in place inside obscura-js

The swap replaces deno_core/v8 inside crates/obscura-js. The crate keeps its name and its public seam (Page/JsRuntime). obscura-cdp, obscura-browser, obscura-dom, and obscura-net stay untouched. The diff stays inside the two files being replaced (runtime.rs and ops.rs).

Status: accepted

Options Considered:
- A new crate with the same API and the old crate deleted. Cleaner history, much bigger diff.

Consequences:
- The fork stays a fork of obscura; upstream merges stay possible for the untouched crates.
