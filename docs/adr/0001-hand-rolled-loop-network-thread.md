# Hand-rolled engine loop; network on its own thread

The engine loop is hand-rolled threads and channels, not tokio. rquest needs tokio, so rquest runs on its own thread with its own tokio runtime. Network results cross back via a channel; the loop wakes and resolves the promise on the isolate thread. The watchdog terminates JavaScript only, never the network thread.

Status: accepted

Options Considered:
- rquickjs async runtime driven by tokio inside the engine loop. Simpler promises, but the loop becomes tokio and the brief's hand-rolled goal dies.

Consequences:
- `op_fetch_url` promise resolution crosses a channel; the spike must prove it.
