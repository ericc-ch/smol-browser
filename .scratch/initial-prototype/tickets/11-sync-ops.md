# 11 — Bind sync ops

What to build: The ~28 required sync ops are registered as rquickjs closures on `Deno.core.ops`. The shim can create elements and read the Rust DOM through Boundary A. Render-family ops stay omitted. V8 remains what Page uses.

Blocked by: 10 rquickjs skeleton beside V8

Status: open

- [ ] `op_dom` and the other required sync ops are bound as `Function::new` closures, string-in / string-out
- [ ] A panic in an op returns an error string and does not unwind into QuickJS
- [ ] A nextest against the QuickJS runtime: set a DomTree, `run_page_init`, `evaluate` `document.createElement('div').tagName` and get `"DIV"`
- [ ] `getElementById` / a simple `querySelector` round-trip the same way
- [ ] Render ops are not registered on the no-render build; the shim still boots (`typeof` guards)
- [ ] bootstrap.js is not modified
- [ ] Page still uses V8; the obstacle course is unchanged
