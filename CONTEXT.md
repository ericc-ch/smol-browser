# smol-browser Domain Terms

## Terms

**the shim**:
bootstrap.js, the handwritten JavaScript that makes the engine look like a browser to page code.
_Avoid_: the DOM shim, bootstrap

**op**:
A function page JavaScript can call that runs in Rust. Ops take strings in and return strings out (for example, `Deno.core.ops.op_dom`).

**Boundary A**:
The seam between page JavaScript and Rust ops. Both sides live in one process; calls are direct.

**Boundary B**:
The seam between an agent's code and the browser process. Messages are JSON-RPC over stdio, one per user action.
_Avoid_: the SDK boundary

**the swap**:
Replacing v8/deno_core with quickjs-ng/rquickjs inside obscura-js while keeping the shim unmodified.

**the spike**:
A throwaway prototype that proves the risky parts of the swap before committing weeks to it.

**the obstacle course**:
The 33-stage end-to-end suite. It drives `obscura fetch` against local fixtures and checks results. It is the daily gate (`e2e/run.sh`).

**the gate**:
The pass condition for the swap: obstacle course results at or above the baseline, with the IO stage re-verified.

**baseline**:
Measurements taken at the pinned commit before the swap: 32/33 obstacle course, 74 MB binary (no-render), about 21.8 MB peak RSS.

**scoreboard**:
Binary size and RSS recorded before and after the swap.

**stealth**:
Presenting a normal browser fingerprint (user agent, timezone, TLS) so automation traffic is not singled out.
