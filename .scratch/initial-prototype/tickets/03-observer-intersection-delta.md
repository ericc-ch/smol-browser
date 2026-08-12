# 03 — observer-intersection delta: park

Type: research

Question: The pinned commit scores 32/33 (observer-intersection fails) while upstream claims 33/33. Investigate now, or later?

Answer: Park it. The failure is in the shim's task-delivery scheduler (observe → rendering opportunity → queueUserTimer, gated by op_async_runtime_available); the host side of that scheduler is replaced by the swap anyway (ticket 01 must prove timer delivery on quickjs-ng). Swap gate = at least baseline 32/33, with the IO stage re-verified via the same fixture once our own scheduler exists. Do not debug the deno_core/tokio interplay of code slated for deletion.
