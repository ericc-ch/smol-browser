# 07 — Intl gap accepted

Type: task

Question: quickjs-ng has no Intl. The shim uses native Intl only for timezone consistency. Do we cover it?

Answer: No, not in the prototype. Accept the gap and document it as a known surface. Add a small Intl.DateTimeFormat shim later only if a test or a target site needs it. The gate is unaffected. See ADR-0003.
