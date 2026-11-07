# Overnight log

One line per queue item: issue, PR, merge commit, tests. Nothing deployed, released, or armed.

- #8 (C19 GCS backup): ehgraph PR #12 merged 00e2d6b; ai-meta pin PR #527 merged 6f55cb1; wiki c4e2019; tests 78 passed / 0 failed, fmt+clippy clean.
- #5 (C23 idempotency window): ehgraph PR #13 merged d540e42; ai-meta pin PR #528 merged 3e0d2f6; wiki 4aa5d04; tests 84 passed / 0 failed, fmt+clippy clean.
- #6 (C11 memory budgets): ehgraph PR #14 merged 5694339; ai-meta pin PR #529 merged 96672d2; wiki 77034e5; tests 96 passed / 0 failed, fmt+clippy clean.
- #4 (C02 compaction): ehgraph PR #15 merged 42f3eba; ai-meta pin PR #530 merged 85e65f3; wiki 98d0841; tests 108 passed / 0 failed, fmt+clippy clean.
- #7 (C17 perf targets + harness): ehgraph PR #16 merged 9cb6314; ai-meta pin PR #531 merged 15b4545; wiki 4c95a50; tests 109 passed / 0 failed, fmt+clippy clean. CI bench (50k, Linux): all within target except neighbors-right-after-a-write 102 ms vs 100 ms (non-gating; noted on #7).

Queue done 2026-11-06. Nothing deployed, released, tagged, or armed; no live cluster touched. New behaviour that changes runtime defaults: embedded budgets are now derived from the host memory limit (C11). Compaction (C02) ships OFF.
