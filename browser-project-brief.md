# Smallest Stealth Browser for AI Agents — Project Brief

Status: planning (Aug 2026). Owner: ericc_ch. Build on his own iron, not on Eous boxes.

## Goal

Smallest headless browser engine for AI agents:
- stealth BY DEFAULT (JS-layer + TLS-layer)
- playwright-SHAPED SDK (simulated subset, not real compat)
- codemode-style code control (v2)
- no layout/rendering/screenshots in v1
- target: 5-10 MB binary, single-digit MB RSS (baseline: obscura 70 MB / 30 MB RSS, headless chrome 300+ MB)

## Mental model (the whole stack, once)

```
page bytes → [network] → [HTML parse] → [DOM + JS]  ← the engine stops here
                          CSS engine → layout → renderer   ← cut (agents read text)
```

- **CSS engine** answers "what" (red, bold, 16px). **Layout** answers "where" (x, y, size). **Renderer** draws. The first pixel appears only after layout. We keep zero of the last three.
- **HTML parsing is incremental and blocking**: chunks feed in as they arrive; a `<script>` tag freezes parsing, runs JS, resumes. The engine loop mediates chunk → parser → quickjs.
- **WebIDL** = the W3C-written contract for every DOM API ("button has `disabled` + `click()`"). Every browser implements the same contract: native data + a WebIDL face. Chromium's face = generated C++, ours = handwritten JS (bootstrap.js).
- **Two boundaries, never confuse them**:
  - **Boundary A** (page JS ↔ Rust ops): direct function calls, same process/thread. Rust registers closures into quickjs at `Deno.core.ops`; the engine converts values at the call. This is the shim's whole mechanism.
  - **Boundary B** (SDK ↔ browser process): JSON-RPC over stdio. Serialized messages, one per user action. Only for the ~40 SDK methods.

## Why this shape (research grounding)

- **obscura** (h4ckf0r0day/obscura, Rust, Apache-2.0, 20k stars): real JS, real DOM, no Chrome process. 42k lines / 8 crates. v8 via deno_core is THE size story. Strip the README proxy ads + AGENTS.md affiliate.
- **kitesurf** (Cloudflare, Aug 2026): started as an obscura port, rebuilt for Workers. Confirms the bet — BUT it does render: Blitz (stylo+taffy+parley+vello) gives it screenshots. "DOM + text, not pixels" is a bet, not a settled fact. Open-sourcing planned; watch it.
- **lightpanda** (Zig, AGPL-3.0, 34k stars): agent browser at scale, own layout engine (karmel). READ for shape, DO NOT copy code (AGPL).
- **agentos reframe**: html5ever + quickjs + cssparser are libraries, not OS programs — nothing to emulate. Untrusted code handled: codemode tree-walker = zero ambient authority; engine gives page code nothing by design.
- **interface convergence**: models already know playwright syntax from training; code compresses intent vs serial tool calls.
- **Language is ecosystem**: the browser crates (html5ever, cssparser, rquickjs, rquest) exist ONLY in Rust. Go/C/Zig/JS/JVM are all dead ends for this project — every other language means writing layers from scratch. The only genuine challenger is Zig (lightpanda proof), and it's AGPL + young.

## Architecture

```
agent's code (playwright-shaped TS)
  → codemode-style tree-walker (v2) — no engine, no authority
      → browser SDK tools (30-50 methods)           [boundary B: JSON-RPC over stdio]
          → thin rust surface
              → quickjs-ng runs the PAGE's code (bootstrap.js shim)  [boundary A: Deno.core.ops]
                  → obscura-dom: html5ever arena tree + cssparser/selectors (Rust)
                  → rquest networking (chrome TLS + HTTP/2 impersonation)
                  → synthetic events, no layout
```

The only genuinely novel piece: the DOM shim serving the playwright contract. Everything else is reused.

## Component decisions

| part | pick | notes |
|---|---|---|
| JS engine | **quickjs-ng via rquickjs, pin >= 0.16** | 367 KiB hello-world, ES2025, MIT. rquickjs = safe Rust wrapper over the C engine — never call the C API directly (unsafe + manual refcounting = the RCE class we don't write). test262 ~98%. no Intl, fine |
| HTML parser | html5ever + rcdom (servo crates) | already in obscura, keep |
| CSS | cssparser + selectors | keep. selector matching only (no cascade) |
| DOM shim | obscura's bootstrap.js (~9k lines) | keep, runs unmodified behind a `Deno.core.ops` object we register via rquickjs (`Function::new` closures hung at `ctx.globals().set("Deno", ...)`) |
| networking | **rquest** | rustls-based chrome TLS+HTTP2 impersonation. tokio lives INSIDE it — keep. feature flag: wreq/boringssl upgrade path for exotic servers |
| engine loop | **hand-rolled: threads + channels** | watchdog thread + bounded queues (obscura's shape). NOT tokio. Also the system-programming classroom — write this by hand, no AI |
| watchdog | JS_SetInterruptHandler | hung page must not hang process |
| cookies | obscura's jar + persist to file | ~50 lines, non-negotiable |
| CDP/MCP | strip | interface is the SDK, not CDP |

## Size budget (70 MB breakdown)

| part | cost | lever |
|---|---|---|
| v8 via deno_core | tens of MB | **THE lever** → quickjs-ng ~1-2 MB |
| boringssl (wreq) | 10-20 MB + cmake | 2nd lever → rquest/rustls |
| rustls + tokio + reqwest | few MB | keep, unavoidable — stripping tokio = losing rquest's impersonation = rewriting TLS (months + CVEs). no |
| html5ever + selectors + cssparser | few MB | keep, this IS the product |
| cdp + mcp + cli | code weight | strip |

The "smallest" game is two moves: drop v8, drop boringssl. Free wins after that: `cargo-bloat` to measure, LTO, `strip = true`, `panic = abort`, `opt-level = "z"`, rquest feature-minimization.

## Stealth by default

1. **JS-layer (free, in bootstrap.js)**: internals non-enumerable (Object.keys(window) doesn't leak); Function.prototype.toString → `[native code]`; own globals filtered from Object.getOwnPropertyNames / Reflect.ownKeys; consistent UA/sec-ch-ua/platform. Also: keep `Deno.core.ops` out of page code's reach — a nosy page must not probe internals.
2. **TLS-layer**: rquest chrome impersonation as DEFAULT (obscura made it opt-in). Known caveat (webclaw-tls): patched rustls occasionally rejects valid server configs — wreq/boringssl behind a feature flag as upgrade path.

## Playwright: simulate, not implement

Full compat is kitesurf-scale (geometry-based Input, Emulation, frames, workers). NOT happening. Simulate the SHAPE: model already knows playwright, so it writes `page.goto`, `locator().click()`, `waitForSelector` untrained. Target 30-50 methods: goto, click, fill, locator, textContent, innerText, title, url, waitForSelector, waitForLoadState, press, cookies, $$eval. Document the subset honestly. Adding a method = adding a tool definition, never touching rust.

**Accessibility snapshot — the one cheap add-on.** The a11y tree is CSSOM-level, not layout-level: DOM + computed `display`/`visibility` (cascade bits only) + HTML-AAM role mapping + accname computation (~2-4k lines of shim JS). Emit as JSON, skip bounds/offscreen geometry. Cheapest feature on the menu — candidate for v1 or v1.5, and a differentiator over kitesurf. (Correction to earlier thinking: the brief's "Accessibility domain leans on layout" is true only for full geometry compat.)

**codemode execute = v2, not v1.** `@opencode-ai/codemode` npm package, host-neutral, built on Effect. Pin or vendor — 0.x beta.

## Screenshots — the honest menu (deferred)

1. **v1: no pixels.** Design the seam: `snapshot()` = DOM + computed styles + viewport as serializable state. External rasterizer as fallback tool for visual tasks.
2. **Cheap screenshots**: pipe snapshot to any external renderer (headless chrome screenshot, etc.). Binary stays 5-10 MB.
3. **Embedded rendering = Blitz** (stylo/taffy/parley/vello, MIT, ~12 MB of binary by itself) — the kitesurf move, weeks not months, but blows the size goal. Only if screenshots become the product.

## Strip list / keep list

GO: deno_core+v8 (→rquickjs), wreq+boringssl (→rquest), obscura-cdp, obscura-mcp, cli serve/scrape modes, robots cache, tracker blocklist, README ads, AGENTS.md affiliate.
KEEP: obscura-dom, html5ever, cssparser/selectors, cookie jar, SSRF gate, bootstrap.js core, watchdog concept, benchmark suite. License Apache-2.0, fork freely.

## Roadmap

**Phase 0 — baseline (week 1)**
- fork obscura, PIN a commit; clean ads/affiliate
- build green on companion box or CI (v8 compile = 5 min + a few GB; NOT the tiny box)
- obstacle course 33/33 baseline green
- pick 5 target sites (sets the shim floor)
- rust strategy: read the book for CONCEPTS, never write Rust. Loop = intent → AI writes → read output + compiler errors (the compiler explains fixes in plain terms) → tests approve. Obstacle course is the referee.

**Phase 1 — the swap (3-6 weeks, the big one)**
- rquickjs spike: tiny crate, eval `1+1`, prove the binding (krishn03id blueprint: 22 ops inventoried, deno_core→rquickjs mapping, 8-step plan, skeleton crate)
- register `Deno.core.ops` shim so bootstrap.js runs unmodified
- port runtime.rs (~3,964 lines) + ops.rs (~1,977)
- hand-roll the engine loop (threads + channels, watchdog via JS_SetInterruptHandler) — written by hand, no AI: the learning track
- GATE: obstacle course green. scoreboard: binary size + RSS

**Phase 2 — strip (1-2 weeks)**
- cut cdp, mcp, scrape modes, robots cache, tracker blocklist
- network = rquest, stealth ON by default
- trim bootstrap.js against the 5 target sites ONLY, obstacle course after every cut

**Phase 3 — the SDK (2-3 weeks, home turf)**
- rust surface minimal: JSON-RPC over stdio
- TS playwright-shaped SDK: 30-50 methods, npm package, examples
- optional: a11y snapshot method (if target sites want it)
- codemode-style execute = v2

Total: 2-3 months part-time.

## Test rig (inherited, zero invention)

obscura-benchmark has 7 tracks; fork inherits them:
- **obstacle course**: 33/33, the daily gate, deterministic offline fixtures. green through every strip
- **WPT conformance**: Core tier 83.3% baseline, run at every phase boundary. tiers in `crates/triage/src/tiers.list`
- **realworld/** + **reliability/**: on the 5 target sites; catches watchdog regressions
- **stealth-bench**: fingerprint consistency — perfect for the rquest swap
- skip **compare/** and perf: performance doesn't matter
- test262 = quickjs-ng's problem; html5lib-tests = html5ever's. Pattern: whoever wrote the library owns the tests; we only test the glue

## Pitfalls / decisions locked

1. cookie persistence to a file — a jar that dies with the process can never stay logged in. ~50 lines, non-negotiable
2. target sites FIRST, before trimming the shim — otherwise you strip blind
3. watchdog stays: hung page ≠ hung process
4. codemode is 0.x beta: pin or vendor
5. no screenshots by design — fallback tool for visual tasks; `snapshot()` seam reserved
6. input without layout: click(selector) = synthetic event on matched element. ~90% of JS apps, breaks on canvas/drag-drop/geometry. honest limitation
7. dynamic content machinery (waitForSelector, timers, mutation observers, event loop ↔ quickjs) is real work — obscura's watchdog thread + bounded queues were earned
8. lightpanda = AGPL. read, don't copy
9. build box: companion for compiles, not the tiny box
10. keep the fork public, write it like a product — portfolio piece
11. tokio: not our enemy. it's ~2-3 MB, lives inside rquest, we're not maintaining it. hand-rolled core loop = classroom + boring/safe, not a size win
12. never hand-roll security code: TLS state machines, quickjs C-API value management. borrow those
13. promote shim interfaces to Rust ONLY on measured evidence (cargo-bloat / profiler vs target sites). hot paths are textContent/innerText/getAttribute/classList/style, not click/disabled. likely never worth it

## References

- obscura: github.com/h4ckf0r0day/obscura (clone: ~/throwaway/obscura)
- obscura-benchmark: ~/throwaway/obscura-benchmark
- krishn03id/obscura-android-port: v8→quickjs-ng blueprint (start.md, feedable to opencode)
- lightpanda: github.com/lightpanda-io/browser (read-only)
- rquest: github.com/StarSparse/rquest
- quickjs-ng: github.com/quickjs-ng/quickjs (v0.16.1)
- rquickjs: github.com/DelSkayn/rquickjs
- blitz (rendering path if ever needed): github.com/DioxusLabs/blitz
- kitesurf: blog.cloudflare.com/kitesurf
- opencode codemode: `@opencode-ai/codemode` npm package
- webclaw-tls (rustls-impersonation caveats): github.com/0xMassi/webclaw-tls
