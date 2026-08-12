# 08 — Cookie persistence: already exists, keep

Type: research

Question: Does obscura already persist cookies to a file, or is it ~50 lines of new work (brief item)?

Answer: It already exists. obscura-net/cookies.rs has `persist_to_file` and `load_from_file`. Keep as-is; no new work. The prototype swaps the engine, not features.
