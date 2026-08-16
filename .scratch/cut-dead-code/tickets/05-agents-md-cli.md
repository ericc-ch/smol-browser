# 05: Patch AGENTS.md CLI line

Type: grilling

Question: Does this cut patch AGENTS.md agent-facing drift (`scrape`/`mcp` still listed) or leave all markdown out?

Answer: Patch AGENTS.md only. Drop `scrape`/`mcp` from the CLI line; keep `serve`/`fetch` as load-bearing. Leave the fork attribution line. Do not rewrite README or `docs/`.
