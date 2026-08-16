# 04: Delete rename scratch after commit

Type: grilling

Question: When the uncommitted rename lands, delete `.scratch/rename-tinybrowser/` in the same cut or in a follow-up?

Answer: Same cut, after the rename is committed. Delete `.scratch/strip-obscura/` in this effort now; delete `.scratch/rename-tinybrowser/` once that commit exists, in the same dead-code cut rather than a later follow-up.
