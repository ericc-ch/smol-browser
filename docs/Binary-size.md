# Minimum binary size

Reference for shrinking the `tinybrowser` ELF. Engine work lives in Rust; the JS-visible Web API should stay a thin host seam plus JS (source, minified, or QuickJS bytecode). Growing `#[rquickjs::methods]` / `rquickjs::class` as the default way to add APIs grows `.text`.

Sub-1MB is aspirational (`AGENTS.md`). This document records what was measured, why Rust bindings cost more than JS wrappers, and which levers actually move the needle.

Re-measure after any size work. Cargo and rustc both warn that `"s"` vs `"z"` vs `3` can invert.

## Rule for bindings

| Put in Rust | Put in JS (minified or bytecode) |
|---|---|
| `DomTree`, HTML parser, cookies, HTTP, crypto primitives, anything that must be unforgeable | WebIDL wrappers, prototype chains, polyfills with no host hooks |
| One (or a few) typed host calls | Hundreds of methods that only forward to those calls |

Native `rquickjs::class` is the right tool for **identity** (`Node` as an unforgeable host object), not for every getter. One dispatcher (`op_dom(cmd, …)`) plus thin JS is the size-optimal FFI: one trampoline, many JS names.

ADR 0004 chose native DOM *and* promised `bootstrap.js` under 500 lines of pure-JS polyfills. That shrink has not landed. Until JS wrappers or native methods are deleted, adding more Rust bindings is additive.

When adding a Web API, prefer the op + thin shim recipe in [Adding a CDP method or Web API](Adding-a-CDP-method-or-Web-API.md). See that doc’s tips.

## Why a Rust method is fatter than a JS method

A JS getter is source (or bytecode) run by **one** interpreter. A Rust `#[rquickjs::methods]` item is a **new native function**:

- `rquickjs-macro` generates an `IntoJsFunc` impl per function.
- Each call does `FromJs` / `IntoJs`, `JsResult`, and often `Class<'js, T>` wrapping.
- LLVM emits a distinct `.text` body plus unwind records (`panic = "unwind"`).

Generic copies add up the same way on x86_64 as on wasm: fewer monomorphized surfaces win. See [Shrinking .wasm code size](https://rustwasm.github.io/book/reference/code-size.html).

The `DomTree` implementation in Rust is not the waste. The waste is one native export per WebIDL member.

QuickJS is built for a small C interpreter plus bytecode, not a large set of native exports:

- ~210 KiB of x86 for hello-world ([QuickJS](https://bellard.org/quickjs/quickjs.html)).
- `qjsc` embeds bytecode; `-s` strips source from that blob.
- rquickjs `embed` compiles JS modules to bytecode at build time ([rquickjs README](https://github.com/DelSkayn/rquickjs)).

Raw `include_str!` never compresses. The shim is stored as UTF-8 in `.rodata`.

## Snapshot (2026-08-21)

Source on this tree:

| Artifact | Size |
|---|---|
| `crates/tinybrowser-js/js/bootstrap.js` | 14,652 lines / **643,733** bytes UTF-8 |
| Same file, `gzip -9` | **173,160** bytes (~3.7× smaller than `.rodata`) |
| `dom_bindings.rs` | 781 lines / 24,428 bytes |
| `ops.rs` (includes `op_dom` multiplexer) | 2,508 lines / 96,564 bytes |
| `psl-2.1.223/src/list.rs` (crates.io) | **2,504,336** bytes, **11,383** `lookup_*` functions |

Existing stripped release ELF `target/release/obscura` (dated 2026-08-16; order of magnitude still holds if the current binary is named `tinybrowser`):

| Piece | Bytes | Share |
|---|---|---|
| File | **14,449,200** (~13.8 MiB) | 100% |
| `.text` | **9,456,663** | ~65% |
| `.rodata` | **2,065,008** | ~14% |
| `.eh_frame` + `.gcc_except_table` | **1,517,220** | ~11% |
| `gzip -9` of that ELF | **6,057,534** | — |

The marker `Pre-declare all internal globals as non-enumerable` is in that ELF, so ~644 KiB of shim source sits uncompressed in `.rodata`.

`bootstrap.js` still implements `class Node` via `_dom("node_type", …)` **and** Rust `JsNode` still exports native `nodeType` / `appendChild` / …. JS then copies methods onto `_NativeNode.prototype`. Both implementations are in the binary.

Sub-1MB vs this ELF: 13.8 MiB stripped, ~6 MiB gzipped. QuickJS hello-world is not TLS + HTML + CDP + PSL + clap + tokio. Getting under 1 MiB means replacing or dropping those stacks, not moving `nodeType` into Rust.

## Ranked levers

### 1. Stop paying twice

Pick one:

- JS surface + one op; delete native class methods, or
- native classes; delete the JS `class Node` / `_dom` wrappers and minify what remains.

Then minify or bytecode-embed the shim (`rquickjs::embed` or `qjsc -s`). Gzip of the source file is ~3.7× smaller than the bytes currently in `.rodata`.

### 2. Cargo release profile

Workspace `Cargo.toml` today only sets `panic = "unwind"`. Cargo defaults for release are `opt-level = 3`, `lto` off, `codegen-units = 16`, `strip` none ([Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html)).

Try, then A/B:

```toml
[profile.release]
opt-level = "s"      # then compare "z" and 3
lto = "fat"
codegen-units = 1
strip = "symbols"
panic = "unwind"     # required; see next section
```

- rustc: `"z"` often produces a **larger** binary than `"s"` ([`-C opt-level`](https://doc.rust-lang.org/rustc/codegen-options/index.html#opt-level)).
- Checklists: [min-sized-rust](https://github.com/johnthagen/min-sized-rust), [Rust Performance Book — build configuration](https://nnethercote.github.io/perf-book/build-configuration.html).

### 3. Keep `panic = "unwind"`

Ops wrap work in `std::panic::catch_unwind` so a panic becomes an error instead of unwinding into QuickJS and aborting. `panic = "abort"` would drop most of the ~1.5 MiB unwind metadata and would break that protocol.

Shrink panicking paths (`unwrap`, `format!`); those pull `core::fmt`.

### 4. Do not compile huge tables into `.text`

`psl = "2"` (for `document.domain`) compiles the Public Suffix List to native `match` code. The crate README states that is intentional (“compiles the list down to native Rust code for ultimate speed”). Comparisons of that approach vs a compressed trie are on the order of **~876 KiB codegen vs ~35 KiB blob** (`structured-public-domains` / `psl2`). Lookup speed is not the size-critical path.

### 5. Cut crate features

Usually larger than any one DOM method:

| Dependency | Fact |
|---|---|
| `tokio` `features = ["full"]` | Tokio docs: full pulls APIs and deps you may not need. Enable only `rt` / `rt-multi-thread`, `net`, `time`, `sync`, `macros`, `io-util` as used. |
| `clap` + `derive` | argparse-rosetta: **574–596 KiB** overhead vs **24 KiB** `pico-args` ([benchmarks](https://github.com/rust-cli/argparse-benchmarks-rs)). |
| `wreq` + gzip/brotli/deflate/zstd | Stealth TLS. Dominant `.text`. |
| `encoding_rs` | Already size-tuned. Do not enable `fast-*-encode` (up to +176 KiB). |
| `html5ever` / `selectors` | Load-bearing parser. |

Nightly-only if you go further: `-Zlocation-detail=none`, `-Zfmt-debug=none`, `-Z build-std` + `optimize_for_size` ([min-sized-rust](https://github.com/johnthagen/min-sized-rust)). UPX can shrink on-disk size and trips antivirus; skip for this binary.

## How to re-measure

```bash
cargo build --release -p tinybrowser-cli --bins
BIN=target/release/tinybrowser
stat -c '%s' "$BIN"
size -A "$BIN" | sort -k2 -n -r | head -20
strip -o /tmp/tinybrowser.stripped "$BIN"
stat -c '%s' /tmp/tinybrowser.stripped
gzip -9 -c /tmp/tinybrowser.stripped | wc -c
cargo bloat --release -p tinybrowser-cli --crates
```

Confirm the shim is still embedded:

```bash
strings -a "$BIN" | grep -c 'Pre-declare all internal globals as non-enumerable'
```

A size change is done when those numbers are recorded next to the commit, workspace tests are green, and `e2e/run.sh` still meets the gate (`AGENTS.md`).

## Sources

- [Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html)
- [rustc codegen: opt-level, lto, strip, panic](https://doc.rust-lang.org/rustc/codegen-options/index.html)
- [min-sized-rust](https://github.com/johnthagen/min-sized-rust)
- [Rust Performance Book — build configuration](https://nnethercote.github.io/perf-book/build-configuration.html)
- [QuickJS](https://bellard.org/quickjs/quickjs.html)
- [rquickjs](https://github.com/DelSkayn/rquickjs)
- [Shrinking .wasm code size](https://rustwasm.github.io/book/reference/code-size.html)
- [tokio feature flags](https://docs.rs/tokio/latest/tokio/#feature-flags)
- [encoding_rs](https://github.com/hsivonen/encoding_rs/)
- [clap argparse overhead](https://github.com/rust-cli/argparse-benchmarks-rs)
- `docs/adr/0004-native-dom-and-actor-architecture.md`
