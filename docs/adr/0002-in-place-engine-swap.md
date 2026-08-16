# Engine swap happens in place inside tinybrowser-js

The swap replaces deno_core/v8 inside crates/tinybrowser-js. The crate keeps its name and its public seam (Page/JsRuntime). tinybrowser-cdp, tinybrowser-core, tinybrowser-dom, and tinybrowser-net stay untouched. The diff stays inside the two files being replaced (runtime.rs and ops.rs).

Status: accepted

Options Considered:
- A new crate with the same API and the old crate deleted. Cleaner history, much bigger diff.

Consequences:
- The fork stays a fork of Obscura; upstream merges stay possible for the untouched crates.
