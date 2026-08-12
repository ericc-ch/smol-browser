# 04 — Swap slicing: follow krishn03id's 8-step order

Type: task

Question: How do we eat the 3-6 week port after the spike?

Answer: Follow krishn03id's 8-step order (ops inventoried, skeleton crate, ops bound, runtime ported, gate). It is a proven deno_core to rquickjs path. Our spike extends its inventory (46 ops referenced, not 22) and adds the async and timer proofs it lacks. In plain terms: wire up the small pieces first, keep the course green at every step.
