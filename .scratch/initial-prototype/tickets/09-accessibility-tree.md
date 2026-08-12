# 09 — Accessibility tree: already exists, keep

Type: research

Question: Does obscura have an accessibility snapshot, or is it the brief's "cheapest feature on the menu" (new work)?

Answer: It already exists. obscura-cdp serves Accessibility.getFullAXTree, built in pure Rust over the DOM tree (role mapping, name, value, properties, AX ids). The engine swap does not touch it. Keep as-is. The brief's a11y-snapshot item is already delivered by the inherited CDP domain.
