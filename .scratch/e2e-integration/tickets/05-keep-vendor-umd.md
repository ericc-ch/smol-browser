# 05: Keep vendor UMD fixtures

Type: grilling

Question: Keep `vendor/` (pinned React / Preact / Vue UMD) or drop the framework stages?

Answer: Keep `vendor/` and the framework stages. Those files are local page fixtures so `react`, `preact`, `vue`, `ssr-hydrate`, and `spa-mini-app` stay offline and version-pinned. They are not a product dependency. Copy the whole `obstacle-course/` tree.
