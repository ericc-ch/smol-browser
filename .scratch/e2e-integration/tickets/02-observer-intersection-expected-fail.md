# 02: observer-intersection is expected_fail

Type: grilling

Question: How is `observer-intersection` encoded so 32/33 is the runner exit, not a human exception?

Answer: Keep the stage. Mark it `expected_fail` in `manifest.json`. Patch `run.py` so exit 0 means every required stage passed. The parked IO fail still prints. A later PASS is extra; it does not raise the gate to 33/33. Do not change `expect` to empty.
