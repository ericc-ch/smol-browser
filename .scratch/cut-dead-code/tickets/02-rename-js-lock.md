# 02: Rename js lock

Type: grilling

Question: Rename `v8_lock` / `is_v8_free_method` to JS names in this effort, or leave the names?

Answer: Rename in this effort. `v8_lock` → `js_lock`, `is_v8_free_method` → `is_js_free_method`, plus matching locals (`_v8_guard`, `nav_v8_lock`) and comments in the CDP crates/tests. The mutex stays. Docs rewrite stays out of scope.
