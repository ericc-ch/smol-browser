# 10 — rquickjs skeleton beside V8

What to build: A QuickJS runtime lives inside obscura-js next to the existing V8 path. It constructs, evaluates `1+1`, and loads the unmodified shim with a stub `Deno.core.ops` object. Page still uses V8. The obstacle course stays at baseline.

Blocked by: None

Status: done

- [x] obscura-js depends on rquickjs wrapping quickjs-ng (rebuild the binding; do not import the throwaway spike crate)
- [x] A nextest in obscura-js evaluates `1+1` on the new runtime and gets `2`
- [x] The same nextest loads the unmodified shim and sees `window` and `document`
- [x] `Deno.core.ops` exists as an object (ops may return null / no-op)
- [x] `cargo nextest run --release --features render -p obscura-js` still passes on the V8 path
- [x] bootstrap.js is not modified
