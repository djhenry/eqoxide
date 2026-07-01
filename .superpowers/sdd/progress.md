# Progress ledger — quest-tracking-complete

Plan: docs/superpowers/plans/2026-06-30-quest-tracking-complete.md
Worktree: .claude/worktrees/quest-tracking-complete (branch worktree-quest-tracking-complete)
Baseline: cargo build OK; cargo test --lib 329 passed, 0 failed (pre-existing unrelated failure: tests/asset_sync_live.rs, not in scope)

## Tasks
- [x] Task 1: complete (commits b1c8ed1..6fd6119, review clean)
- [x] Task 2: complete (commits 6fd6119..8dc60a3, review clean)
- [x] Task 3: complete (commits 8dc60a3..f1faa20, review clean)
- [x] Task 4: complete (commits f1faa20..9dc6afb, fixed unbounded Vec::with_capacity clamp, review clean)
- [x] Task 5: complete (commits 9dc6afb..02a5dd7, review clean, minor note re: silent zero-fallback on missing offer not fixed)
- [x] Task 6: complete (commits 02a5dd7..f2fb628, review clean)
- [x] Task 7: complete (commit 2cb40a6; .claude/skills/build-run/SKILL.md edited directly in main checkout since .claude/ is gitignored/untracked in the worktree)
- [x] Task 8: complete. cargo test --lib: 349 passed, 0 failed. cargo build --release: clean.
  Live GM validation (durgan+aiquestbot on shared EQEmu server): task assignment verified
  end-to-end correct (GET /v1/quests/log showed full title/description/coin_reward/xp_reward/
  status=Active for task_id=2). Completion-flow (/v1/quests/log -> /v1/quests/completed) was
  NOT successfully observed live — blocked by environment issues, not a demonstrated client bug:
  (1) a duplicate aiquestbot process caused a double-login kick that made further GM commands
  silently no-op, (2) after a clean relaunch, the shared server's zone capacity was exhausted
  (all 20 dynamic zone slots in use), preventing re-zone-in to retry. Code review of
  apply_completed_tasks (src/eq_net/packet_handler.rs) confirms it exactly matches
  TaskManager::SendCompletedTasksToClient's wire format read directly from EQEmu source
  (zone/task_manager.cpp:912-970: count u32, then task_id u32 + title cstr + completed_time u32
  per entry) — no known defect, just unverified live behavior for this specific path.
  Separately noted (pre-existing, OUT OF SCOPE for this plan): task_id=2's activity showed an
  anomalous done_count=1766195200 vs goal_count=1 even before completion was requested,
  suggesting a bug in apply_task_activity (untouched by Tasks 1-8), not the OP_CompletedTasks
  path this plan implements.
  Task offer/accept/decline flow not reachable live either (matches design spec's known finding
  that no live content on this server uses tasksetselector) — validated by unit tests only
  (Tasks 4 & 5).

## Final whole-branch review (Opus, range b1c8ed1..2cb40a6)

Verdict: Ready to merge, with fixes. Architecture/threading/OP_CompletedTasks fix/untrusted-input
hardening all solid; 7 of 8 tasks production-ready as-is.

Important finding (fixed): `extract_saylink_text` (packet_handler.rs) assumed a 3-`\x12`-delimiter
saylink format that EQEmu's real `SayLinkEngine::GenerateLink()` (common/say_link.cpp) does not
emit — it emits exactly 2 delimiters, `\x12<56-char hex body><Name>\x12` (RoF2
`SAY_LINK_BODY_SIZE`=56, rof2_limits.h:304), body and name concatenated in one segment. The old
parser returned real reward-item links unchanged (raw control bytes + hex body), silently failing
gap #2 ("item rewards aren't parsed") the feature exists to close. The existing test used a
fabricated 3-delimiter fixture that masked the bug. Fixed: parser now splits on 2 delimiters and
strips the fixed 56-char body; added `SAY_LINK_BODY_SIZE` const; rewrote both the unit test and
`apply_task_description`'s reward-item test to use the real 2-delimiter format; added a
short-body-no-panic test. Verified against EQEmu source directly. 350 lib tests pass (0 failed),
release build clean.

Minor (fixed): stale `OP_COMPLETED_TASKS` comment in protocol.rs still said "list of completed
task ids" — corrected to describe the actual full-record format.

Minor (accepted, not fixed — low impact, no live content triggers this path): stale task_offers
on `element_count != 0` bail-out in `apply_task_select_window`; one-tick staleness window on
`/v1/quests/offers` after accept (consistent with every other snapshot-slot in this codebase).
