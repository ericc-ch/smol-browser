# 06 — Crate shape: swap in place

Type: task

Question: Where does the swap live?

Answer: Inside crates/obscura-js, replacing deno_core/v8. The crate keeps its name and its public seam (Page/JsRuntime). CDP, browser, dom, and net crates stay untouched. See ADR-0002.
