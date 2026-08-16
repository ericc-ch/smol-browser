# 01: Extension structure — three-tier mod hierarchy

Type: grilling

Question: How does "everything is an extension" work in a compiled, single-binary
Rust world without killing the size goal?

Answer: Mods are not one mechanism — they're a speed hierarchy:

- **JS mods (runtime, free):** a `.js` file dropped next to the binary, loaded by
  the engine at startup, registers itself via a host API (ops, CDP domains, lifecycle
  hooks). Zero binary cost — the engine is already shipped; mods are data files.
  Good for behavior: a11y, scraping logic, automation, a JS renderer (pdf.js-style).
- **Build-time native mods (fast, rebuild):** cargo feature + optional crate.
  The engine, net backends, the DOM parser, and **rendering** live here. Render
  stays archived on a branch; `--features render` re-adds it when screenshots are
  wanted. Size control is exactly this lever: default = gate-minimal deps, never
  feature-creep the default.
- **WASM (rare, expensive):** exists only if a real runtime-native mod need appears
  (third-party native renderer, hot-swappable engine). Costs a runtime (~2MB) and a
  serialization boundary. The mod contract (registration of ops/CDP domains) stays
  transport-agnostic so wasm can slot in later without rearchitecting.

Rules: mods see only core (never each other); core = traits + registration registry +
minimal DOM/net; runtime mods are never linked code, so they cannot grow the binary.

Why not a single mechanism: JS-only mods are too slow for pixel-pushing (render),
wasm-over-wire for everything is friction and binary weight, and runtime native
loading (.so) violates single-binary + no-C-ABI. The hierarchy matches each mod type
to the smallest lever that satisfies it.

Out of scope: runtime native .so loading (C ABI, single-binary conflict); bundling
mods into the default build.
