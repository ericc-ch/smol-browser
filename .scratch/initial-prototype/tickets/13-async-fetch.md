# 13 — Async fetch over a channel

What to build: `op_fetch_url` returns a Promise. HTTP runs on a dedicated thread with its own tokio runtime, using today's obscura-net client. The isolate thread wakes on a channel, resolves the Promise, and pumps jobs. `fetch()` in the shim works.

Blocked by: 12 Hand-rolled loop, timers, posted tasks

Status: open

- [ ] `op_fetch_url` is bound with a `Rest<String>` tail or a small JS adapter (7-arg closures do not compile)
- [ ] A nextest `evaluate` of `fetch(url).then(r => r.text())` against a local fixture URL returns the body
- [ ] The network thread is not the isolate thread
- [ ] SSRF still blocks loopback / private ranges unless the existing allow-private-network policy is on
- [ ] Request interception still works: a Continue / Fulfill / Fail resolution reaches the Promise
- [ ] bootstrap.js is not modified
- [ ] No rquest; obscura-net stays as-is
