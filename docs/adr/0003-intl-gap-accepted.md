# No Intl in the prototype

quickjs-ng has no Intl. The shim uses native Intl only to keep timezone consistent with Date (bootstrap.js line 12369). The prototype accepts this gap and documents it as a known surface. A small Intl.DateTimeFormat shim gets added later only if a test or a target site needs it.

Status: accepted

Consequences:
- Fingerprinting scripts that probe `Intl.DateTimeFormat().resolvedOptions().timeZone` will not match Chrome until the shim lands.
- The obstacle course fingerprint stage does not use Intl, so the gate is unaffected.
