# 05 — Engine loop: hand-rolled, network on its own thread

Type: task

Question: The shim's fetch needs rquest, and rquest needs tokio. How does the hand-rolled engine loop talk to it?

Answer: rquest runs on its own thread with its own tokio runtime. Results cross back through a channel. The loop thread wakes on the channel and resolves the quickjs promise on the isolate thread. The watchdog terminates JavaScript only, never the network thread. The loop stays hand-rolled threads and channels, no tokio. See ADR-0001.
