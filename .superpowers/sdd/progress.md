# Progress ledger — group-management

Plan: docs/superpowers/plans/2026-07-01-group-management.md
Spec: docs/superpowers/specs/2026-07-01-group-management-design.md
Worktree: .claude/worktrees/group-management (branch worktree-group-management)
Baseline: cargo build OK; cargo test --lib 366 passed, 0 failed

## Tasks
- [x] Task 1: complete (commits 562c0f1..86d0ee6, review clean — minor: 2 unused-import warnings inherited from brief, one missing symmetric test on disband_other name2-match branch, not blocking)
- [x] Task 2: complete (commits 86d0ee6..a1014f7, review clean — minor: duplicated name-copy pattern across 4 builders, not blocking)
- [x] Task 3: complete (commits a1014f7..48af69b, review clean — minor: untrimmed name storage on invite/kick/makeleader, cosmetic field alignment, not blocking; main.rs intentionally left non-compiling pending Task 4)
- [x] Task 4: complete (commits 48af69b..0f4382b, review clean — argument-order trace for group_invite/group_kick/group_make_leader verified across all 9 call sites, no swaps; 2 pre-existing Task-1 compiler warnings noted, not blocking)
- [x] Task 5: complete (commits 0f4382b..dc24efb, review clean — no egui smoke test but matches existing convention, not blocking)
- [x] Task 6: complete. Live two-character validation (Korgath+Lyrica, qeynos) against a running
  EQEmu server. Found and fixed a real bug: `OP_GroupDisband` (0x4c10) client→server struct was
  128 bytes, but the live server requires 148 bytes — server logged
  `Wrong size on incoming [OP_GroupDisband] ... Got [128], expected [148]` and silently dropped
  leave/decline/kick packets (no crash, no roster change). Fixed `build_group_disband()` in
  `src/eq_net/navigation.rs` to emit 148 bytes; updated its unit test and
  `docs/eq-technical-knowledgebase/group-protocol.md` (which had wrongly inferred 128 bytes via
  static analysis). Re-verified live post-fix: invite/accept, decline, non-leader leave, kick,
  and leader-leave-with-<3-members-disbands all PASS on both sides' rosters. Makeleader handoff
  itself confirmed working (no disband at transfer); the narrower claim "no disband on a
  subsequent leave once handed off, with 3+ members remaining" could not be exercised — only 2
  live characters were available this session. Full detail: `.superpowers/sdd/task-6-report.md`.
