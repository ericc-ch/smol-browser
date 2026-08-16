# 02: Archive mechanics

Type: grilling

Question: How do we preserve render/mcp/scrape code when cutting it from the tree?

Answer: Plain delete. The code stays in git history (`git show` restores any file);
no archive branches or tags. Bookkeeping-free; the code is already recoverable.

Justification: all three are fully committed already; the archive is git itself.
Revisiting render/mcp later means checking out old paths, not a special mechanism.
