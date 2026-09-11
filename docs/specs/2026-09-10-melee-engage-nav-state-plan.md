# Melee-Engage nav_state Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give an agent an honest `nav_state` word (`engaging`) while auto-attack is steering the body at a target, replacing the silent `idle`/`goto_superseded` lie, and make the goto-supersede one-shot per melee episode.

**Architecture:** A new per-tick reconciler in `ActionLoop::tick` — inserted *above* the 150 ms planner gate and every early return, peering with `nav_halt_if_dead` — owns a transient `engaging` word: it publishes `engaging`/`melee_engaged` while `auto_attack` has a live target inside the ~200u engage radius and retires it to `idle`/`melee_disengaged` the first tick that predicate is false. `drive_auto_engage_melee` keeps doing steering and facing but no longer touches nav command state (its per-tick `request_cancel_goto()` is deleted). `CommandState::stamp_new_goal`'s idle-branch word-preservation widens from life-halt-only to a new `nav_state_is_suspended` predicate so a `/stop` or a WASD cancel can't relabel `engaging`. Three `/move/*` handlers (`goto`, `follow`, `zone_cross`) auto-disengage `auto_attack` on accept ("newest explicit command wins"), and `/v1/observe/debug` gains an always-present `auto_attack` player field.

**Tech Stack:** Rust, Cargo workspace. Build/test with `~/bin/rbuild <worktree-dir> <cargo-args>` (remote builder). axum + tokio for HTTP router tests. **Every `rbuild` call passes `timeout: 420000`** — the workspace suite runs ~3 min and the Bash default 120 s backgrounds it.

**Spec:** `docs/specs/2026-09-10-melee-engage-nav-state-design.md`

## Global Constraints

Every task's requirements implicitly include this section.

- **Test baseline:** `main` = **1145 passed / 0 failed / 29 ignored** (`~/bin/rbuild . test --workspace --locked`). This plan adds ~15 tests. The suite must end **0 failed** with no newly-`#[ignore]`d tests — expect **≥ 1158 passed / 0 failed / 29 ignored**.
- **`engaging` is TRANSIENT — NEVER add it to `TERMINAL_NAV_STATES`** (`crates/eqoxide-nav/src/walker.rs`). That array's doc is a standing `#1007` trap warning: a word listed there has its `!nav_state_is_terminal`-guarded retirement turned into dead code and the state becomes permanent. Task 2 adds a test locking the array at length 5 without `"engaging"`.
- **`goal_id` bumps at most once per melee-engage episode.** The supersede of an in-flight `/goto` is edge-triggered by the `engage_active` latch; `enter_engaging` never bumps `goal_id`.
- **The `NavStatus` exhaustive-writer trio stays `..`-free.** `retire_to_idle`, `stamp_fresh_goal`, `transition_within_goal` (`crates/eqoxide-ipc/src/lib.rs`) each destructure `NavStatus` with **no `..`**, so a new field is `error[E0027]` on every route. This plan adds **no** `NavStatus` field — `engaging` is a value of the existing `state: String` — so the trio is untouched. Do not "tidy" it.
- **`walker.rs` has exactly ONE top-level `#[cfg(test)]`.** The lint `the_driving_nav_state_word_is_only_ever_written_through_the_verdict_851` asserts `SRC.match_indices("\n#[cfg(test)]").count() == 1`. All new walker tests go **inside the existing `mod tests`**, never in a new `#[cfg(test)] mod`.
- **`guild.rs` refusal-lint canonical shape.** Every `s.command.request_*` call in an HTTP handler must be exactly `if let Some(busy) = s.command.request_<verb>(<args>).refused(<MSG>) { return busy; }` (or `.refused_json(<json>)`). The `refusal_sites` span scanner cuts the statement at the first `;` after its opening `{` — **so a `refused_json` message string must contain NO semicolon**, or conjunct 3 (`{ return busy;`) is lost and the lint goes RED. Task 6's three new sites require bumping `guild.rs::CANONICAL_SITES` **38 → 41**.
- **Attribution trailers on every commit:**
  ```
  Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01G8ggWADJdrxci4Q5MEhqJ6
  ```
- **Anchor by surrounding code, not raw line numbers.** `crates/eqoxide-net/src/action_loop.rs`'s `tick` body drifts between revisions. Line numbers here were correct at spec HEAD `8788f97a`; every insertion is *also* described by the code it sits between — trust the code context.
- **Worktree git:** simple commands only (no compound commands, heredocs, loops, or `$(...)`-computed options). Never bare `git stash`. Do not push `main`; push the feature branch (it has a remote); open a **draft** PR referencing `#1007` with no auto-closing keyword.

---

## File Structure

Nine files. The first eight are code; the ninth is the agent-facing contract doc.

| # | File | Responsibility in this change |
|---|------|-------------------------------|
| 1 | `crates/eqoxide-ipc/src/lib.rs` | Single source of truth for nav vocabulary. Adds `NAV_STATE_ENGAGING`, `NAV_REASON_MELEE_ENGAGED`, `NAV_REASON_MELEE_DISENGAGED`, and the `nav_state_is_suspended` predicate (life-halt ∪ `engaging`). |
| 2 | `crates/eqoxide-nav/src/walker.rs` | Re-exports the new vocab. Adds `enter_engaging()` — the idempotent publisher of `engaging`/`melee_engaged` that also nulls `goal`/`local` (#732 discipline). |
| 3 | `crates/eqoxide-command/src/nav.rs` | Adds `has_active_goto()` (un-gated goto-slot query, for a release build). Widens `stamp_new_goal`'s idle-branch word-preservation from `nav_state_is_life_halt` to `nav_state_is_suspended`. Corrects three stale rustdoc mentions of a per-tick melee cancel. |
| 4 | `crates/eqoxide-net/src/action_loop.rs` | The reconciler `reconcile_engage_nav_state()`, its `tick` call site, and the `engage_active` latch field. Deletes the per-tick `request_cancel_goto()` from `drive_auto_engage_melee` and rewrites its now-false `#1109` comment. |
| 5 | `crates/eqoxide-http/src/move_api.rs` | `should_disengage_for_new_move()` predicate + inline canonical auto-attack disengage in `post_goto` / `post_follow` / `post_zone_cross`. New `disengaged` response key. |
| 6 | `crates/eqoxide-http/src/guild.rs` | `CANONICAL_SITES` 38 → 41 (the three new refusal sites in `move_api.rs`) + reconciliation-comment update. |
| 7 | `crates/eqoxide-http/src/lib.rs` | `PlayerState.auto_attack: bool` field + `from_game_state` wiring (the single construction site). |
| 8 | `crates/eqoxide-http/src/observe.rs` | Surfaces `auto_attack` on the `GET /v1/observe/debug` player object, always present. |
| 9 | `docs/http-api.md` | Documents `engaging` (non-terminal), `melee_engaged`, `melee_disengaged`, the `disengaged` move-response key, and the `auto_attack` player field. |

Task→file map:

- **Task 1** → file 1
- **Task 2** → file 2 (consumes 1)
- **Task 3** → file 3 (`has_active_goto` only)
- **Task 4** → file 3 (`stamp_new_goal` widen + doc note; consumes 1)
- **Task 5** → file 4 + the three doc corrections in file 3 (consumes 1, 2, 3)
- **Task 6** → files 5 + 6
- **Task 7** → files 7 + 8
- **Task 8** → file 9

---

### Task 1: `eqoxide-ipc` melee-engage vocabulary

**Files:**
- Modify: `crates/eqoxide-ipc/src/lib.rs` — insert 3 consts + `nav_state_is_suspended` immediately after `nav_state_is_life_halt`'s closing `}` (was ~line 1993), which is the blank line before the `/// Live navigation state for the active` rustdoc that opens the `NavStatus` doc block.
- Test: same file, new `#[cfg(test)] mod nav_suspended_tests_1007` appended at EOF (after the final `}` of `mod world_roster_tests_643`, near `mod event_feed;`).

**Interfaces:**
- Consumes: `nav_state_is_life_halt(&str) -> bool`, `NAV_STATE_DEAD`, `NAV_STATE_HALTED_HP_ZERO` (all already in this file).
- Produces:
  - `pub const NAV_STATE_ENGAGING: &str` (`= "engaging"`)
  - `pub const NAV_REASON_MELEE_ENGAGED: &str` (`= "melee_engaged"`)
  - `pub const NAV_REASON_MELEE_DISENGAGED: &str` (`= "melee_disengaged"`)
  - `pub fn nav_state_is_suspended(s: &str) -> bool`

- [ ] **Step 1: Write the failing test**

Append at EOF of `crates/eqoxide-ipc/src/lib.rs`:

```rust
#[cfg(test)]
mod nav_suspended_tests_1007 {
    use super::*;

    #[test]
    fn engaging_is_a_suspended_state_alongside_the_life_halts() {
        assert!(nav_state_is_suspended(NAV_STATE_DEAD));
        assert!(nav_state_is_suspended(NAV_STATE_HALTED_HP_ZERO));
        assert!(nav_state_is_suspended(NAV_STATE_ENGAGING));
    }

    #[test]
    fn ordinary_goal_states_are_not_suspended() {
        assert!(!nav_state_is_suspended("idle"));
        assert!(!nav_state_is_suspended("navigating"));
        assert!(!nav_state_is_suspended("arrived"));
        assert!(!nav_state_is_suspended("blocked"));
    }

    #[test]
    fn the_melee_vocab_is_spelled_as_the_agent_contract_documents() {
        assert_eq!(NAV_STATE_ENGAGING, "engaging");
        assert_eq!(NAV_REASON_MELEE_ENGAGED, "melee_engaged");
        assert_eq!(NAV_REASON_MELEE_DISENGAGED, "melee_disengaged");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `~/bin/rbuild . test -p eqoxide-ipc --lib -- nav_suspended_tests_1007` (`timeout: 420000`)
Expected: FAIL to **compile** — `cannot find value NAV_STATE_ENGAGING` / `cannot find function nav_state_is_suspended`.

- [ ] **Step 3: Write minimal implementation**

In `crates/eqoxide-ipc/src/lib.rs`, immediately after the closing `}` of `pub fn nav_state_is_life_halt` (its body is `state == NAV_STATE_DEAD || state == NAV_STATE_HALTED_HP_ZERO`), before the `/// Live navigation state for the active `/move/goto`` rustdoc, insert:

```rust
/// `nav_state` while auto-attack is pursuing a live target into melee range (#1007).
/// The character IS navigating — `drive_auto_engage_melee` is steering the controller
/// toward `target_id` — so publishing `idle` (which `request_cancel_goto` used to do
/// every gated tick) is the #343 lie: a confident "ready for work" over a body walking
/// across the zone at a skeleton. TRANSIENT: retired to `idle`/`melee_disengaged` the
/// first tick the pursuit predicate is false. **Deliberately NOT in
/// `TERMINAL_NAV_STATES`** — see the trap doc at `walker.rs:101`.
pub const NAV_STATE_ENGAGING: &str = "engaging";

/// `nav_reason` accompanying `NAV_STATE_ENGAGING`: auto-attack has a live target within
/// the ~200u engage radius and is steering toward it. If a `/move/goto` was in flight
/// when the pursuit began, it was superseded ONCE (a single `goal_id` bump) — read the
/// `nav_goal_id` you got back from `POST /goto`: a higher current id means the melee
/// engage took the wheel.
pub const NAV_REASON_MELEE_ENGAGED: &str = "melee_engaged";

/// `nav_reason` on the `idle` that `NAV_STATE_ENGAGING` retires to when the pursuit
/// predicate goes false — the target died or despawned, moved beyond ~200u, `auto_attack`
/// was turned off, or a fresh `/move/{goto,follow,zone_cross}` disengaged it (§3.6).
/// Distinct from `stopped` (you asked via `/move/stop`) and `goto_superseded` (manual
/// movement took over): "your melee pursuit ended and nothing replaced it."
pub const NAV_REASON_MELEE_DISENGAGED: &str = "melee_disengaged";

/// Is `state` one where navigation is SUSPENDED by something outside the goal's own
/// lifecycle — a life halt (#1000/#1007) or an active melee pursuit (#1007 re-scope)?
/// Used by `CommandState::stamp_new_goal`'s idle branch to decide whether a goal-level
/// event (a `/stop`, a per-frame `request_cancel_goto` from WASD/`/manual`) may relabel
/// the published word. It may not: neither event is evidence about whether the character
/// is halted or in melee, and only the owning driver can know.
pub fn nav_state_is_suspended(s: &str) -> bool {
    nav_state_is_life_halt(s) || s == NAV_STATE_ENGAGING
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `~/bin/rbuild . test -p eqoxide-ipc --lib -- nav_suspended_tests_1007` (`timeout: 420000`)
Expected: PASS (3 tests).

- [ ] **Step 5: Run the crate's whole test suite (regression check)**

Run: `~/bin/rbuild . test -p eqoxide-ipc --lib` (`timeout: 420000`)
Expected: PASS, no regressions.

- [ ] **Step 6: Commit**

Write `/home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt`:

```
feat(#1007): ipc melee-engage nav vocabulary

NAV_STATE_ENGAGING / NAV_REASON_MELEE_ENGAGED / NAV_REASON_MELEE_DISENGAGED,
and nav_state_is_suspended (life-halt ∪ engaging) for the stamp_new_goal
idle-branch word-preservation widening in a later task.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01G8ggWADJdrxci4Q5MEhqJ6
```

```bash
git add crates/eqoxide-ipc/src/lib.rs
git commit -F /home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt
```

---

### Task 2: `walker.rs` — re-export vocab + `enter_engaging()`

**Files:**
- Modify: `crates/eqoxide-nav/src/walker.rs`
  - The `pub use eqoxide_ipc::{ ... };` re-export (was ~line 76, the one importing `NAV_STATE_DEAD, NAV_STATE_HALTED_HP_ZERO, nav_state_is_life_halt`) — grow to 7 items.
  - Add `enter_engaging` immediately after `set_nav_state_because`'s closing `}` (its body contains a `debug_assert!`), before the `/// ` rustdoc that opens `write_nav_state_locked`.
- Test: `crates/eqoxide-nav/src/walker.rs`, **inside the existing `mod tests`** (near the other `walker_with(...)`-using tests). **Do NOT add a new `#[cfg(test)] mod`** — see Global Constraints.

**Interfaces:**
- Consumes: `NAV_STATE_ENGAGING`, `NAV_REASON_MELEE_ENGAGED`, `NAV_REASON_MELEE_DISENGAGED`, `nav_state_is_suspended` (Task 1); `transition_within_goal` on the locked `NavStatus`; `walker_with(Default::default()) -> (Walker, NavSlots, NavIntent, NavDebugView)`; `TERMINAL_NAV_STATES`, `nav_state_is_terminal`.
- Produces: `pub fn Walker::enter_engaging(&self)` — idempotent publisher of `engaging`/`melee_engaged`; nulls `goal` and `local`; never bumps `goal_id`. Re-exported names `NAV_STATE_ENGAGING`, `NAV_REASON_MELEE_ENGAGED`, `NAV_REASON_MELEE_DISENGAGED`, `nav_state_is_suspended` now reachable as `eqoxide_nav::walker::<name>`.

- [ ] **Step 1: Write the failing tests**

Inside `crates/eqoxide-nav/src/walker.rs`'s `mod tests`, add:

```rust
    #[test]
    fn engaging_is_not_terminal() {
        assert!(!TERMINAL_NAV_STATES.contains(&"engaging"),
            "engaging is TRANSIENT — listing it makes its retirement dead code (#1007 trap)");
        assert_eq!(TERMINAL_NAV_STATES.len(), 5);
        assert!(!nav_state_is_terminal("engaging"));
    }

    #[test]
    fn enter_engaging_nulls_goal_and_local() {
        let (walker, nav, _intent, _dbg) = walker_with(Default::default());
        *nav.nav_state.lock().unwrap() = eqoxide_ipc::NavStatus {
            state: "arrived".into(),
            reason: Some("seeded".into()),
            goal: Some([9.0, 9.0, 9.0]),
            local: Some(eqoxide_ipc::NavLocal {
                state: "threading".into(),
                reason: "carrot".into(),
                stuck_ticks: 3,
                plan_us: 120,
            }),
            ..Default::default()
        };

        walker.enter_engaging();

        let ns = nav.nav_state.lock().unwrap().clone();
        assert_eq!(ns.state, "engaging");
        assert_eq!(ns.reason.as_deref(), Some("melee_engaged"));
        assert_eq!(ns.goal, None, "a melee pursuit has no fixed-point goal (#732)");
        assert_eq!(ns.local, None, "a melee pursuit does not use the fine planner (#382/#766)");
    }

    #[test]
    fn enter_engaging_is_idempotent() {
        let (walker, nav, _intent, _dbg) = walker_with(Default::default());
        walker.enter_engaging();
        let gid = nav.nav_state.lock().unwrap().goal_id;
        walker.enter_engaging();
        let ns = nav.nav_state.lock().unwrap().clone();
        assert_eq!(ns.goal_id, gid, "a re-publish of the same engaging word must not churn goal_id");
        assert_eq!(ns.state, "engaging");
        assert_eq!(ns.reason.as_deref(), Some("melee_engaged"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `~/bin/rbuild . test -p eqoxide-nav --lib -- enter_engaging engaging_is_not_terminal` (`timeout: 420000`)
Expected: FAIL to compile — `no method named enter_engaging`. (`engaging_is_not_terminal` alone would already pass — that is intentional; it is the mutation guard for "don't add `engaging` to `TERMINAL_NAV_STATES` later".)

- [ ] **Step 3: Grow the re-export**

Replace the `pub use eqoxide_ipc::{NAV_STATE_DEAD, NAV_STATE_HALTED_HP_ZERO, nav_state_is_life_halt};` line with:

```rust
pub use eqoxide_ipc::{
    NAV_STATE_DEAD, NAV_STATE_HALTED_HP_ZERO, NAV_STATE_ENGAGING,
    NAV_REASON_MELEE_ENGAGED, NAV_REASON_MELEE_DISENGAGED,
    nav_state_is_life_halt, nav_state_is_suspended,
};
```

- [ ] **Step 4: Add `enter_engaging`**

Immediately after `set_nav_state_because`'s closing `}`, before the `write_nav_state_locked` rustdoc:

```rust
/// Publish `engaging`/`melee_engaged` (#1007). Idempotent — the reconciler calls this
/// every tick a pursuit is active. Unlike a bare `set_nav_state_because("engaging", …)`
/// this also nulls `goal` and `local`: a melee pursuit has no fixed-point goal (the
/// target is a live entity) and does not use the fine planner, so leaving either set
/// would be a stale `nav_goal`/`nav_local` beside `engaging` — the #732 defect class.
/// `goal_id` is NOT bumped here: entering melee is not a `/move/*` accept, and the
/// one-shot supersede in the reconciler already bumped it once if there was a goto.
pub fn enter_engaging(&self) {
    let mut s = self.nav.nav_state.lock().unwrap();
    if s.state == NAV_STATE_ENGAGING
        && s.reason.as_deref() == Some(eqoxide_ipc::NAV_REASON_MELEE_ENGAGED) {
        return; // already there — no write, no churn
    }
    s.transition_within_goal(NAV_STATE_ENGAGING, Some(eqoxide_ipc::NAV_REASON_MELEE_ENGAGED));
    s.goal  = None;   // explicit narrowing of transition_within_goal's `_keep_goal`  (#732)
    s.local = None;   // explicit narrowing of transition_within_goal's `_keep_local` (#382/#766)
}
```

Place it in the same `impl Walker { ... }` block as `set_nav_state_because` (it uses `self.nav`).

- [ ] **Step 5: Run tests to verify they pass**

Run: `~/bin/rbuild . test -p eqoxide-nav --lib -- enter_engaging engaging_is_not_terminal` (`timeout: 420000`)
Expected: PASS (3 tests).

- [ ] **Step 6: Run the crate's whole suite (the driving-word lint lives here)**

Run: `~/bin/rbuild . test -p eqoxide-nav --lib` (`timeout: 420000`)
Expected: PASS. In particular `the_driving_nav_state_word_is_only_ever_written_through_the_verdict_851` must stay green — the new tests added **zero** top-level `#[cfg(test)]` lines and `enter_engaging` calls `transition_within_goal`, not `set_nav_state`/`set_nav_state_because` with a driving-word literal.

- [ ] **Step 7: Commit**

Write `/home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt`:

```
feat(#1007): walker enter_engaging() + re-export the melee vocab

enter_engaging publishes engaging/melee_engaged and nulls goal/local
(#732 discipline); an early-return guard makes a re-publish a no-op so
the reconciler can call it every tick. goal_id is never bumped here.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01G8ggWADJdrxci4Q5MEhqJ6
```

```bash
git add crates/eqoxide-nav/src/walker.rs
git commit -F /home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt
```

---

### Task 3: `nav.rs` — `has_active_goto()`

**Files:**
- Modify: `crates/eqoxide-command/src/nav.rs` — add `has_active_goto` right after `goto_target()`'s closing `}` and before the `impl` block's closing `}` (`goto_target` is `#[cfg(any(test, feature = "test-fixtures"))]`; `has_active_goto` is **not** gated).
- Test: same file, `mod tests` (near `request_goto_sets_target_and_clears_entity` / `request_stop_clears_both_slots`).

**Interfaces:**
- Consumes: `self.nav.goto_target: Mutex<Option<(f32,f32,f32)>>`; `CommandState::default()`, `request_goto`, `request_stop` (all existing).
- Produces: `pub fn CommandState::has_active_goto(&self) -> bool` — release-build-visible peek at the goto slot.

- [ ] **Step 1: Write the failing test**

In `crates/eqoxide-command/src/nav.rs`'s `mod tests`:

```rust
    #[test]
    fn has_active_goto_tracks_the_goto_slot() {
        let cs = CommandState::default();
        assert!(!cs.has_active_goto(), "no goto set at construction");
        cs.request_goto((1.0, 2.0, 3.0));
        assert!(cs.has_active_goto(), "a /goto target is now set");
        cs.request_stop();
        assert!(!cs.has_active_goto(), "stop clears the goto slot");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `~/bin/rbuild . test -p eqoxide-command --lib -- has_active_goto_tracks_the_goto_slot` (`timeout: 420000`)
Expected: FAIL to compile — `no method named has_active_goto`.

- [ ] **Step 3: Write minimal implementation**

After `goto_target()`'s closing `}`, before the `impl` block close:

```rust
    /// Is a `/move/goto` (or the goto half of a `/follow`) target currently set? Un-gated
    /// (unlike `goto_target()` above, which is `#[cfg(test)]`-only) — the #1007 reconciler
    /// needs it in a release build to decide whether entering melee must supersede a walk.
    pub fn has_active_goto(&self) -> bool {
        self.nav.goto_target.lock().unwrap().is_some()
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `~/bin/rbuild . test -p eqoxide-command --lib -- has_active_goto_tracks_the_goto_slot` (`timeout: 420000`)
Expected: PASS.

- [ ] **Step 5: Commit**

Write `/home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt`:

```
feat(#1007): CommandState::has_active_goto() (un-gated goto-slot peek)

The reconciler entering melee must, in a release build, know whether a
/move/goto is in flight to decide whether to supersede it once.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01G8ggWADJdrxci4Q5MEhqJ6
```

```bash
git add crates/eqoxide-command/src/nav.rs
git commit -F /home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt
```

---

### Task 4: `nav.rs` — widen the idle-branch word-preservation to `nav_state_is_suspended`

**Files:**
- Modify: `crates/eqoxide-command/src/nav.rs` — in `stamp_new_goal`, the `if new_state == "idle" {` branch: one identifier swap on the `let halted = eqoxide_ipc::nav_state_is_life_halt(&s.state)` line, plus one sentence added to the branch's comment block.
- Test: same file, `mod tests`.

**Interfaces:**
- Consumes: `eqoxide_ipc::nav_state_is_suspended` (Task 1); `NAV_REASON_MELEE_ENGAGED`; `CommandState::default()`, `request_goto`, `request_stop`, `request_cancel_goto`; direct access to `cs.nav.nav_state` / `cs.nav.goto_target`.
- Produces: `stamp_new_goal`'s idle branch now preserves the published `state`/`reason` words for **any** suspended state (life halt **or** `engaging`), not just life halts.

- [ ] **Step 1: Write the failing test**

In `crates/eqoxide-command/src/nav.rs`'s `mod tests`:

```rust
    #[test]
    fn stop_and_wasd_cancel_do_not_flip_engaging_to_idle() {
        for cancel in ["stop", "cancel_goto"] {
            let cs = CommandState::default();
            // Route through the real accept path first so `state` is a normal in-flight goal…
            let g0 = cs.request_goto((10.0, 0.0, 0.0));
            // …then seed `engaging` directly on the locked NavStatus, exactly as the reconciler would.
            {
                let mut s = cs.nav.nav_state.lock().unwrap();
                s.state = "engaging".into();
                s.reason = Some(eqoxide_ipc::NAV_REASON_MELEE_ENGAGED.into());
            }
            let g1 = match cancel {
                "stop" => cs.request_stop(),
                _ => cs.request_cancel_goto(),
            };
            let s = cs.nav.nav_state.lock().unwrap();
            assert_eq!(s.state, "engaging",
                "{cancel}: a goal-level cancel must not relabel the suspended word");
            assert_eq!(s.reason.as_deref(), Some("melee_engaged"), "{cancel}: reason preserved too");
            assert!(g1 > g0, "{cancel}: goal_id still bumps (the caller's landed-request signal)");
            assert!(cs.nav.goto_target.lock().unwrap().is_none(), "{cancel}: goto slot cleared");
        }
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `~/bin/rbuild . test -p eqoxide-command --lib -- stop_and_wasd_cancel_do_not_flip_engaging_to_idle` (`timeout: 420000`)
Expected: FAIL — `assertion left == right failed: state` is `"idle"`, not `"engaging"` (the pre-change code only preserves life-halt words).

- [ ] **Step 3: Make the swap**

In `stamp_new_goal`'s `if new_state == "idle" {` branch, change:

```rust
            let halted = eqoxide_ipc::nav_state_is_life_halt(&s.state)
                .then(|| (s.state.clone(), s.reason.clone()));
```

to:

```rust
            let halted = eqoxide_ipc::nav_state_is_suspended(&s.state)   // was: nav_state_is_life_halt
                .then(|| (s.state.clone(), s.reason.clone()));
```

- [ ] **Step 4: Add the widening rationale to the branch comment**

In the same branch's comment block (the one that begins `// #1007/#1000 — A LIFE-HALT WORD IS NOT A GOAL OUTCOME…` and ends before the `let halted = …` line), append one sentence after the existing life-halt argument and before the measured-flap paragraph:

```rust
            // The same argument covers `engaging` (#1007 re-scope): a `/stop` or a per-frame
            // `request_cancel_goto` from WASD/`/manual` says nothing about whether auto-attack is
            // still pursuing a live target — only the tick reconciler can know — so a goal-level
            // event may not relabel that word either. Hence `nav_state_is_suspended`, not
            // `nav_state_is_life_halt`, gates the preservation below.
```

- [ ] **Step 5: Run test to verify it passes**

Run: `~/bin/rbuild . test -p eqoxide-command --lib -- stop_and_wasd_cancel_do_not_flip_engaging_to_idle` (`timeout: 420000`)
Expected: PASS (both loop iterations).

- [ ] **Step 6: Run the crate's whole suite**

Run: `~/bin/rbuild . test -p eqoxide-command --lib` (`timeout: 420000`)
Expected: PASS — the existing life-halt preservation tests still hold (`nav_state_is_suspended` is a strict superset of `nav_state_is_life_halt`).

- [ ] **Step 7: Commit**

Write `/home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt`:

```
feat(#1007): preserve `engaging` across a goal-level cancel

stamp_new_goal's idle branch now gates its state/reason preservation on
nav_state_is_suspended (life-halt ∪ engaging), not nav_state_is_life_halt.
A /stop or a WASD cancel still bumps goal_id and clears the goto slot,
but no longer relabels a melee pursuit to idle/goto_superseded.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01G8ggWADJdrxci4Q5MEhqJ6
```

```bash
git add crates/eqoxide-command/src/nav.rs
git commit -F /home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt
```

---

### Task 5: `action_loop.rs` — the reconciler, and strip nav-command state from `drive_auto_engage_melee`

This is the keystone task. It consumes Tasks 1–3 and is coherent as one review unit: the reconciler *takes over* the responsibility the deleted `request_cancel_goto()` line held, so the delete, the reconciler, and the three now-false rustdoc corrections in `nav.rs` land together.

**Files:**
- Modify: `crates/eqoxide-net/src/action_loop.rs`
  - `struct ActionLoop` — new field `engage_active: bool` after `auto_attack: bool,`.
  - `ActionLoop::new` — init `engage_active: false,` after `auto_attack: false,` in the struct literal.
  - New method `reconcile_engage_nav_state` — insert at the blank line **after `drive_auto_pet_combat`'s closing `}` and before the `/// Returns true if this handled the tick…` rustdoc of `drive_auto_engage_melee`**.
  - `tick` — new call `self.reconcile_engage_nav_state(gs);` **between the `if self.walker.nav_halt_if_dead(gs) { return; }` block and `self.walker.apply_fast_steering(gs);`**.
  - `drive_auto_engage_melee` — **delete** the line `self.command.request_cancel_goto(); // cancel any stale walk` (immediately above `return true;`); keep `return true;`. Rewrite the `#1109` comment block inside this fn (begins `// #1109: a DEAD target is not engageable.`, ends `// Do not read the short case as the bound.`).
- Modify: `crates/eqoxide-command/src/nav.rs` — three rustdoc corrections (no behavior change):
  1. Module doc — the sentence naming "the melee-engage auto-cancel (`eq_net/action_loop.rs`)" as a `request_cancel_goto` caller.
  2. `NAV_REASON_GOTO_CANCELLED` const doc — "or the auto-melee-engage override takes over steering".
  3. `request_cancel_goto` fn doc + its body comment — "an auto-melee-engage override" / "or the melee-engage override".
- Test: `crates/eqoxide-net/src/action_loop.rs`, `mod tests`, using `shared_nav_action_loop()` (so `request_cancel_goto`'s `goal_id` bump and `enter_engaging`'s writes are visible on the mutex the test reads).

**Interfaces:**
- Consumes:
  - `CommandState::has_active_goto(&self) -> bool` (Task 3), `CommandState::request_cancel_goto(&self) -> u64` (existing).
  - `Walker::enter_engaging(&self)` (Task 2), `Walker::nav_state_is(&self, &str) -> bool` (existing), `Walker::set_nav_state_because(&self, &str, Option<&str>)` (existing).
  - `eqoxide_nav::walker::NAV_STATE_ENGAGING`, `eqoxide_nav::walker::NAV_REASON_MELEE_DISENGAGED` (Task 2 re-export).
  - `gs.auto_attack` field is NOT read here — the loop's own `self.auto_attack` is; `gs.target_id: Option<u32>`, `gs.world.entities: HashMap<u32, Entity>`, `gs.player_x`/`gs.player_y: f32`, `Entity.dead: bool`, `Entity.x`/`Entity.y: f32`.
  - `shared_nav_action_loop() -> (ActionLoop, eqoxide_ipc::NavSlots, eqoxide_command::CommandState, SharedCollision, ZoneAssetStateShared)`; `crate::transport::test_stream(0, 0).await`; `eqoxide_core::game_state::make_entity(id, name, x, y, z, is_npc) -> Entity` (gives `dead: false`); `std::time::Instant`.
- Produces: `fn ActionLoop::reconcile_engage_nav_state(&mut self, gs: &GameState)` — private; called once per `tick` above the 150 ms gate.

- [ ] **Step 1: Write the failing tests**

In `crates/eqoxide-net/src/action_loop.rs`'s `mod tests`:

```rust
    #[test]
    fn engaging_is_published_while_auto_attack_pursues_a_live_nearby_target() {
        let (mut al, nav, _command, _collision, _za) = shared_nav_action_loop();
        let mut gs = GameState::new();
        gs.player_id = 7;
        gs.player_x = 0.0;
        gs.player_y = 0.0;
        gs.player_z = 0.0;
        gs.upsert_entity(eqoxide_core::game_state::make_entity(42, "a skeleton", 150.0, 0.0, 0.0, true));
        gs.target_id = Some(42);
        al.auto_attack = true;

        al.reconcile_engage_nav_state(&gs);

        let ns = nav.nav_state.lock().unwrap().clone();
        assert_eq!(ns.state, eqoxide_ipc::NAV_STATE_ENGAGING);
        assert_eq!(ns.reason.as_deref(), Some(eqoxide_ipc::NAV_REASON_MELEE_ENGAGED));
        assert_eq!(ns.goal, None, "a melee pursuit has no fixed-point goal");
    }

    #[tokio::test]
    async fn the_reconciler_runs_above_the_150ms_gate() {
        let (mut stream, _rx) = crate::transport::test_stream(0, 0).await;
        let (mut al, nav, _command, _collision, _za) = shared_nav_action_loop();
        let mut gs = GameState::new();
        gs.player_id = 7;
        gs.player_x = 0.0;
        gs.player_y = 0.0;
        gs.player_z = 0.0;
        gs.upsert_entity(eqoxide_core::game_state::make_entity(42, "a skeleton", 150.0, 0.0, 0.0, true));
        gs.target_id = Some(42);
        al.auto_attack = true;

        // Gate CLOSED: last_tick fresh, so `tick` early-returns at the 150 ms gate…
        al.last_tick = Instant::now();
        al.tick(&mut stream, &mut gs);

        // …yet the reconciler, which sits ABOVE the gate, still published the word.
        assert_eq!(nav.nav_state.lock().unwrap().state, eqoxide_ipc::NAV_STATE_ENGAGING,
            "reconcile_engage_nav_state must run before the 150 ms gate's early return");
    }

    #[test]
    fn engaging_retires_to_idle_melee_disengaged_when_the_target_dies() {
        let (mut al, nav, _command, _collision, _za) = shared_nav_action_loop();
        let mut gs = GameState::new();
        gs.player_id = 7;
        gs.player_x = 0.0;
        gs.player_y = 0.0;
        gs.upsert_entity(eqoxide_core::game_state::make_entity(42, "a skeleton", 150.0, 0.0, 0.0, true));
        gs.target_id = Some(42);
        al.auto_attack = true;

        al.reconcile_engage_nav_state(&gs);
        let gid = nav.nav_state.lock().unwrap().goal_id;
        assert_eq!(nav.nav_state.lock().unwrap().state, eqoxide_ipc::NAV_STATE_ENGAGING);

        // The target dies but lingers in world.entities until the server deletes the spawn.
        let mut dead = eqoxide_core::game_state::make_entity(42, "a skeleton", 150.0, 0.0, 0.0, true);
        dead.dead = true;
        gs.upsert_entity(dead);

        al.reconcile_engage_nav_state(&gs);

        let ns = nav.nav_state.lock().unwrap().clone();
        assert_eq!(ns.state, "idle");
        assert_eq!(ns.reason.as_deref(), Some(eqoxide_ipc::NAV_REASON_MELEE_DISENGAGED));
        assert_eq!(ns.goal_id, gid, "retiring the melee word is not a goal accept — goal_id unchanged");
    }

    #[test]
    fn the_retire_guard_leaves_a_word_written_by_someone_else_alone() {
        let (mut al, nav, _command, _collision, _za) = shared_nav_action_loop();
        *nav.nav_state.lock().unwrap() = eqoxide_ipc::NavStatus {
            state: "navigating".into(),
            reason: Some("seeded".into()),
            ..Default::default()
        };
        let gs = GameState::new(); // predicate false: no target
        al.auto_attack = false;

        al.reconcile_engage_nav_state(&gs);

        assert_eq!(nav.nav_state.lock().unwrap().state, "navigating",
            "the reconciler must only ever retire its OWN `engaging` word");
    }

    #[test]
    fn entering_melee_supersedes_an_active_goto_exactly_once() {
        let (mut al, nav, command, _collision, _za) = shared_nav_action_loop();
        let n = command.request_goto((10.0, 0.0, 0.0));
        assert_eq!(nav.nav_state.lock().unwrap().goal_id, n);

        let mut gs = GameState::new();
        gs.player_id = 7;
        gs.player_x = 0.0;
        gs.player_y = 0.0;
        gs.upsert_entity(eqoxide_core::game_state::make_entity(42, "a skeleton", 150.0, 0.0, 0.0, true));
        gs.target_id = Some(42);
        al.auto_attack = true;

        al.reconcile_engage_nav_state(&gs);
        let after_entry = nav.nav_state.lock().unwrap().clone();
        assert_eq!(after_entry.goal_id, n + 1, "the in-flight goto is superseded exactly once");
        assert_eq!(after_entry.state, eqoxide_ipc::NAV_STATE_ENGAGING);

        for _ in 0..5 {
            al.reconcile_engage_nav_state(&gs);
        }
        let after_5 = nav.nav_state.lock().unwrap().clone();
        assert_eq!(after_5.goal_id, n + 1, "no further goal_id churn while the pursuit continues (#349)");
        assert_eq!(after_5.state, eqoxide_ipc::NAV_STATE_ENGAGING);
    }

    #[test]
    fn entering_melee_with_no_active_goto_does_not_touch_goal_id() {
        let (mut al, nav, _command, _collision, _za) = shared_nav_action_loop();
        let before = nav.nav_state.lock().unwrap().goal_id;
        let mut gs = GameState::new();
        gs.player_x = 0.0;
        gs.player_y = 0.0;
        gs.upsert_entity(eqoxide_core::game_state::make_entity(42, "a skeleton", 150.0, 0.0, 0.0, true));
        gs.target_id = Some(42);
        al.auto_attack = true;

        al.reconcile_engage_nav_state(&gs);

        assert_eq!(nav.nav_state.lock().unwrap().goal_id, before,
            "with no goto in flight, entering melee is not a goal accept");
        assert_eq!(nav.nav_state.lock().unwrap().state, eqoxide_ipc::NAV_STATE_ENGAGING);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `~/bin/rbuild . test -p eqoxide-net --lib -- engaging_ the_reconciler_runs_above the_retire_guard entering_melee` (`timeout: 420000`)
Expected: FAIL to compile — `no method named reconcile_engage_nav_state`, `no field engage_active`.

- [ ] **Step 3: Add the `engage_active` field**

On `struct ActionLoop`, immediately after `auto_attack: bool,`:

```rust
    /// #1007: latched true for the duration of one melee-engage episode — set when
    /// `reconcile_engage_nav_state` first publishes `engaging`, cleared when it retires.
    /// Makes the one-shot goto-supersede edge-triggered (one `goal_id` bump per episode,
    /// not one per gated tick — #349).
    engage_active: bool,
```

- [ ] **Step 4: Initialise it in `ActionLoop::new`**

In the struct literal returned by `ActionLoop::new`, immediately after `auto_attack: false,`:

```rust
            engage_active: false,
```

- [ ] **Step 5: Add the reconciler method**

At the blank line after `drive_auto_pet_combat`'s closing `}`, before `/// Returns true if this handled the tick and the caller must stop (melee engage/hold fired).`:

```rust
    /// Reconcile the `engaging` `nav_state` word against the live auto-attack pursuit
    /// predicate (#1007). Pure: reads `self.auto_attack` + `gs`, writes only the nav_state
    /// word (and, once per episode, supersedes an in-flight goto). `drive_auto_engage_melee`
    /// owns steering/facing and no longer touches nav command state.
    ///
    /// Runs above the 150 ms gate and every early return in `tick`, so the word tracks the
    /// pursuit within one tick in both directions.
    fn reconcile_engage_nav_state(&mut self, gs: &GameState) {
        // Same predicate as `drive_auto_engage_melee`'s own gates: auto_attack on, a live
        // (non-dead) target, 2D distance < 200u. Kept in lock-step with that fn on purpose
        // — the word must mean exactly "that driver is about to steer."
        let want_engage = self.auto_attack
            && gs.target_id
                .and_then(|tid| gs.world.entities.get(&tid))
                .filter(|e| !e.dead)
                .map(|e| {
                    let dx = e.x - gs.player_x;
                    let dy = e.y - gs.player_y;
                    (dx * dx + dy * dy).sqrt() < 200.0
                })
                .unwrap_or(false);

        if want_engage {
            if !self.engage_active {
                // First tick of the episode. Take the wheel from any in-flight goto ONCE,
                // so `goal_id` bumps once (#349) and `goal` is cleared before `engaging` is
                // published (so `nav_goal` is null under `engaging`, #732 discipline).
                if self.command.has_active_goto() {
                    self.command.request_cancel_goto();
                }
                self.engage_active = true;
            }
            // Idempotent on every later tick: `enter_engaging`'s own early-return guard
            // makes a re-publish of the same `engaging`/`melee_engaged` a no-op — no lock
            // write, no `goal_id` touch. It also nulls `goal`/`local` on the transition in,
            // so `nav_goal` is null under `engaging` even when the episode began from a
            // stale `arrived`.
            self.walker.enter_engaging();
        } else {
            // Retire ONLY our own word — guarded so we never stomp navigating / arrived /
            // blocked / dead written by anyone else.
            if self.walker.nav_state_is(eqoxide_nav::walker::NAV_STATE_ENGAGING) {
                self.walker.set_nav_state_because(
                    "idle", Some(eqoxide_nav::walker::NAV_REASON_MELEE_DISENGAGED));
            }
            self.engage_active = false;
        }
    }
```

- [ ] **Step 6: Wire the call site in `tick`**

Find the `nav_halt_if_dead` guard near the top of `tick` (it currently reads exactly):

```rust
        if self.walker.nav_halt_if_dead(gs) {
            return;
        }

        self.walker.apply_fast_steering(gs);
```

Insert the reconciler call between them:

```rust
        if self.walker.nav_halt_if_dead(gs) {
            return;
        }

        self.reconcile_engage_nav_state(gs);   // #1007 — peer with nav_halt_if_dead: above the gate

        self.walker.apply_fast_steering(gs);
```

- [ ] **Step 7: Delete the per-tick cancel in `drive_auto_engage_melee`**

In `drive_auto_engage_melee`, the two lines directly before the method's `false` fall-through currently read:

```rust
                        self.command.request_cancel_goto(); // cancel any stale walk
                        return true;
```

Delete the first. Leave:

```rust
                        return true;
```

- [ ] **Step 8: Rewrite the now-false `#1109` comment in `drive_auto_engage_melee`**

Replace the whole comment block (from `// #1109: a DEAD target is not engageable. This filter used to be redundant —` through `// Do not read the short case as the bound.`) with:

```rust
                // #1109 / #1007: a DEAD target is not engageable. The dead entity stays in
                // `world.entities` until the SERVER removes the spawn — measured live in
                // `fieldofbone`: ~2 s / 13 ticks for a no-corpse mob, and far longer for one
                // that leaves a corpse (EQEmu hands the corpse the dead NPC's own entity id,
                // so `target_id` stays resolvable-and-dead for the corpse's whole decay; do
                // not read the short case as the bound). Two consumers must filter it, for the
                // same reason:
                //   - this driver, so auto-attack does not pin the player walking at a corpse;
                //   - `reconcile_engage_nav_state`, whose `want_engage` predicate is the same
                //     `auto_attack && live target && < 200u` shape — an unfiltered dead target
                //     would pin `nav_state` at `engaging` indefinitely (the #1007 lie in a new
                //     place).
                // `drive_auto_pet_combat` above has always filtered `!e.dead` for exactly this.
```

- [ ] **Step 9: Correct the three stale `nav.rs` rustdoc mentions**

In `crates/eqoxide-command/src/nav.rs`:

**(a) Module doc.** Change the sentence:

> `request_cancel_goto` is a DIFFERENT, narrower write used by keyboard/manual-move cancellation (`app.rs`) and the melee-engage auto-cancel (`eq_net/action_loop.rs`): it clears only `goto_target`, leaving `goto_entity` alone — preserving the pre-migration behavior at each of those call sites (they never touched `goto_entity`).

to:

> `request_cancel_goto` is a DIFFERENT, narrower write used by keyboard/manual-move cancellation and camera reset (`app.rs`), and — exactly once, on episode entry — the #1007 melee-engage reconciler's goto supersede (`eq_net/action_loop.rs`): it clears only `goto_target`, leaving `goto_entity` alone — preserving the pre-migration behavior at each of those call sites (they never touched `goto_entity`).

**(b) `NAV_REASON_GOTO_CANCELLED` const doc.** Change:

> the narrower cancel taken when manual movement, the HTTP manual-move escape hatch, or the auto-melee-engage override takes over steering.

to:

> the narrower cancel taken when manual movement (keyboard WASD, camera reset) or the HTTP manual-move escape hatch takes over steering. A melee-engage episode also supersedes an in-flight goto once, but the reconciler immediately relabels the word to `engaging`/`melee_engaged` — an agent never observes `goto_superseded` from auto-attack.

**(c) `request_cancel_goto` fn doc + body comment.** In the fn doc, change:

> used where manual movement (keyboard WASD, the HTTP manual-move escape hatch, or an auto-melee-engage override) needs to take over steering this frame/tick but isn't itself a `/stop`.

to:

> used where manual movement (keyboard WASD, camera reset, the HTTP manual-move escape hatch) — or, exactly once on entry, the #1007 melee-engage reconciler — needs to take over steering this frame/tick but isn't itself a `/stop`.

In the body comment, change:

> `goto_superseded` = you did not, and steering was taken over by manual movement or the melee-engage override.

to:

> `goto_superseded` = you did not, and steering was taken over by manual movement or the manual-move escape hatch. (A melee engage supersedes a goto once on entry too, but its observable `nav_reason` is `melee_engaged`, not this.)

- [ ] **Step 10: Run the reconciler tests**

Run: `~/bin/rbuild . test -p eqoxide-net --lib -- engaging_ the_reconciler_runs_above the_retire_guard entering_melee` (`timeout: 420000`)
Expected: PASS (7 tests).

- [ ] **Step 11: Run the full workspace suite (Task 5 changes cross three crates)**

Run: `~/bin/rbuild . test --workspace --locked` (`timeout: 420000`)
Expected: **0 failed.** Passed count ≈ baseline 1145 + (Task 1: 3) + (Task 2: 3) + (Task 3: 1) + (Task 4: 1) + (Task 5: 7) = **~1160 passed / 0 failed / 29 ignored**. Watch specifically:
- `a_life_halt_retires_above_the_auto_engage_early_return_1007` (`action_loop.rs`) — still green: the reconciler sits *below* `nav_halt_if_dead` and its retire branch is guarded by `nav_state_is(ENGAGING)`, so it never touches a life-halt word.
- `eqoxide-nav`'s driving-word lint — untouched.
- `eqoxide-command`'s life-halt preservation tests — untouched.

- [ ] **Step 12: Commit**

Write `/home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt`:

```
feat(#1007): reconcile the `engaging` nav_state word in ActionLoop::tick

A new per-tick reconciler, inserted above the 150 ms gate right after
nav_halt_if_dead, publishes engaging/melee_engaged while auto_attack has a
live target within ~200u and retires it to idle/melee_disengaged the first
tick that predicate is false. The goto-supersede is edge-triggered by an
engage_active latch — one goal_id bump per episode (#349), not one per
gated tick.

drive_auto_engage_melee keeps steering and facing but no longer calls
request_cancel_goto(); its now-false #1109 comment is rewritten, and three
stale nav.rs rustdoc mentions of a per-tick melee cancel are corrected.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01G8ggWADJdrxci4Q5MEhqJ6
```

```bash
git add crates/eqoxide-net/src/action_loop.rs crates/eqoxide-command/src/nav.rs
git commit -F /home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt
```

> **Self-Review note (carried into the final review):** the "move `enter_engaging` before `request_cancel_goto`" and "delete the `engage_active` latch" mutations are **not** distinguishable in these synchronous, single-threaded tests — both orderings reach the same final `state`/`reason`/`goal`/`goal_id`, and `has_active_goto()` also stops the churn once the slot clears. `entering_melee_supersedes_an_active_goto_exactly_once` and `entering_melee_with_no_active_goto_does_not_touch_goal_id` pin the two invariants that *are* observable: exactly-one bump with a goto present, zero bumps without. The latch is defense-in-depth / intent-explicit.

---

### Task 6: `move_api.rs` — auto-disengage on a fresh `/move/{goto,follow,zone_cross}`

**Files:**
- Modify: `crates/eqoxide-http/src/move_api.rs`
  - Add `use crate::refusal::Refusal;` to the import block (after `use crate::name_match::{…};`).
  - Add free fn `should_disengage_for_new_move(s: &HttpState) -> bool` as a sibling of `current_target_match` (just before or after it).
  - `post_goto` — inline the canonical disengage block immediately before `let goal_id = s.command.request_goto(target);`; add `"disengaged": disengaged` to the `json!` response object (the one with `"status": "navigating"`).
  - `post_follow` — inline the same block immediately before `let goal_id = s.command.request_follow(matched.key.clone(), pos);`; add `"disengaged": disengaged` to its `json!` response (the `"status": "following"` one).
  - `post_zone_cross` — inline the same block immediately before `let goal_id = s.command.request_zone_cross(zone_id);`; capture the final `format!(...)` into `let mut body = format!(...)`, then `if disengaged { body.push_str(" Auto-attack was disengaged (a fresh zone_cross supersedes a melee pursuit)."); }`, then `text(StatusCode::OK, body)`.
- Modify: `crates/eqoxide-http/src/guild.rs` — `const CANONICAL_SITES: usize = 38;` → `41`, and update the reconciliation comment above it.
- Test: `crates/eqoxide-http/src/move_api.rs`, `mod tests` (router-level). **The test must not contain the literal string `s.command.request_`** — it would be picked up by `guild.rs`'s `refusal_sites` scan of this file.

**Interfaces:**
- Consumes: `HttpState` (`s.game_state.load()` derefs to `GameState`; `s.command: CommandState`); `CommandState::request_attack(&self, on: bool) -> bool` (async enqueue) and `take_attack(&self) -> Option<bool>`; `crate::refusal::Refusal` (`bool::refused_json(self, serde_json::Value) -> Option<Response>`); `gs.auto_attack: bool`, `gs.target_id: Option<u32>`, `gs.world.entities: HashMap<u32, Entity>`, `Entity::dead`; `crate::testkit::{empty_state, set_gs}`; `eqoxide_core::game_state::make_entity`; the module's `body_text` helper; `router()`.
- Produces: `fn should_disengage_for_new_move(s: &HttpState) -> bool` (crate-private, no `request_*` call inside). New `disengaged: bool` key on the `/goto` and `/follow` JSON responses; an appended sentence on the `/zone_cross` text response.

- [ ] **Step 1: Write the failing test**

In `crates/eqoxide-http/src/move_api.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn fresh_goto_while_engaging_disengages_and_echoes_it() {
        // auto_attack on + a live target → the fresh goto disengages it.
        let state = empty_state();
        set_gs(&state, |gs| {
            gs.auto_attack = true;
            gs.target_id = Some(42);
            gs.upsert_entity(eqoxide_core::game_state::make_entity(42, "a skeleton", 10.0, 0.0, 0.0, true));
        });
        let app = router().with_state(state.clone());
        let req = Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"x":1.0,"y":2.0,"z":3.0}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let j: serde_json::Value = serde_json::from_str(&body_text(resp).await).unwrap();
        assert_eq!(j["disengaged"], serde_json::json!(true), "a fresh goto disengages an active auto-attack");
        assert_eq!(j["status"], serde_json::json!("navigating"));
        assert_eq!(state.command.take_attack(), Some(false), "the disengage toggle was queued");

        // auto_attack off → nothing to disengage, disengaged:false, no toggle queued.
        let state2 = empty_state();
        let app2 = router().with_state(state2.clone());
        let req2 = Request::post("/goto")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"x":1.0,"y":2.0,"z":3.0}"#)).unwrap();
        let resp2 = app2.oneshot(req2).await.unwrap();
        let j2: serde_json::Value = serde_json::from_str(&body_text(resp2).await).unwrap();
        assert_eq!(j2["disengaged"], serde_json::json!(false));
        assert_eq!(state2.command.take_attack(), None, "no disengage queued when auto_attack was already off");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `~/bin/rbuild . test -p eqoxide-http --lib -- fresh_goto_while_engaging_disengages_and_echoes_it` (`timeout: 420000`)
Expected: FAIL — `j["disengaged"]` is `Null` (key absent), and `take_attack()` is `None`.

- [ ] **Step 3: Add the import**

In the `move_api.rs` import block, after `use crate::name_match::{distance_between, resolve_in_world, MatchQuality, NameMatch};`:

```rust
use crate::refusal::Refusal;
```

- [ ] **Step 4: Add the predicate**

As a sibling free fn of `current_target_match` (place it directly above `current_target_match`'s rustdoc, or directly below its closing `}`):

```rust
/// #1007: should a fresh `/move/{goto,follow,zone_cross}` disengage an active auto-attack melee
/// pursuit? True iff auto-attack is on AND the current target still resolves to a live entity.
///
/// Deliberately a PURE predicate with no `request_*` call inside — the caller does the refusable
/// toggle in the canonical `if let Some(busy) = … { return busy; }` shape, so the `guild.rs`
/// refusal-lint sees a checked site. Deliberately NO `< 200u` distance gate: "I issued a new
/// movement command" is reason enough to end the pursuit; the geometry is the melee driver's
/// concern, not this one's.
fn should_disengage_for_new_move(s: &HttpState) -> bool {
    let gs = s.game_state.load();
    gs.auto_attack
        && gs.target_id
            .and_then(|tid| gs.world.entities.get(&tid))
            .is_some_and(|e| !e.dead)
}
```

- [ ] **Step 5: Wire `post_goto`**

Immediately before `let goal_id = s.command.request_goto(target);`:

```rust
    // #1007: newest explicit command wins. A fresh /goto supersedes an in-flight melee pursuit —
    // disengage auto-attack so the body walks to the point instead of being dragged back to the
    // target every tick by drive_auto_engage_melee. A raced toggle returns 409 here rather than a
    // false `"disengaged": true`.
    let disengaged = should_disengage_for_new_move(&s);
    if disengaged {
        if let Some(busy) = s.command.request_attack(false).refused_json(serde_json::json!({
            "status": "busy_attack",
            "message": "an auto-attack toggle is already queued — nothing changed, retry (it was NOT queued)",
        })) { return busy; }
    }
```

**The `message` string must contain no `;`** (see Global Constraints). Then, in the `json!({ … })` response object with `"status": "navigating"`, add a line (anywhere in the object, e.g. right after `"status": "navigating",`):

```rust
        "disengaged": disengaged,
```

- [ ] **Step 6: Wire `post_follow`**

Immediately before `let goal_id = s.command.request_follow(matched.key.clone(), pos);`, insert:

```rust
    // #1007: newest explicit command wins. A fresh /follow supersedes an in-flight melee pursuit —
    // disengage auto-attack so the body follows the named entity instead of being dragged back to
    // the target every tick by drive_auto_engage_melee. A raced toggle returns 409 here rather than
    // a false `"disengaged": true`.
    let disengaged = should_disengage_for_new_move(&s);
    if disengaged {
        if let Some(busy) = s.command.request_attack(false).refused_json(serde_json::json!({
            "status": "busy_attack",
            "message": "an auto-attack toggle is already queued — nothing changed, retry (it was NOT queued)",
        })) { return busy; }
    }
```

(The `message` string contains no `;` — see Global Constraints.) Then in the `json!({ … })` with `"status": "following"`, add:

```rust
        "disengaged": disengaged,
```

- [ ] **Step 7: Wire `post_zone_cross`**

Immediately before `let goal_id = s.command.request_zone_cross(zone_id);`, insert:

```rust
    // #1007: newest explicit command wins. A fresh /zone_cross supersedes an in-flight melee
    // pursuit — disengage auto-attack so the body walks to the zone line instead of being dragged
    // back to the target every tick by drive_auto_engage_melee. A raced toggle returns 409 here
    // rather than a false disengage disclosure.
    let disengaged = should_disengage_for_new_move(&s);
    if disengaged {
        if let Some(busy) = s.command.request_attack(false).refused_json(serde_json::json!({
            "status": "busy_attack",
            "message": "an auto-attack toggle is already queued — nothing changed, retry (it was NOT queued)",
        })) { return busy; }
    }
```

(The `message` string contains no `;` — see Global Constraints.) Then change the final response from:

```rust
    text(StatusCode::OK, format!(
        "zone_cross to zone_id={zone_id} accepted [goal_id={goal_id}] — walking to the zone line (async, not a teleport). \
         Poll GET /v1/observe/debug: the `zone` field changes on success. Every failure is now reported \
         honestly in `nav_state` (+`nav_reason`): `no_path` = no route to the line EXISTS (definitive), \
         `search_exhausted` = the planner gave up ('I don't know', not 'no'), `blocked` = a route exists \
         but the walker physically wedged. See docs/http-api.md 'Navigation state'."))
```

to:

```rust
    let mut body = format!(
        "zone_cross to zone_id={zone_id} accepted [goal_id={goal_id}] — walking to the zone line (async, not a teleport). \
         Poll GET /v1/observe/debug: the `zone` field changes on success. Every failure is now reported \
         honestly in `nav_state` (+`nav_reason`): `no_path` = no route to the line EXISTS (definitive), \
         `search_exhausted` = the planner gave up ('I don't know', not 'no'), `blocked` = a route exists \
         but the walker physically wedged. See docs/http-api.md 'Navigation state'.");
    if disengaged {
        body.push_str(" Auto-attack was disengaged (a fresh zone_cross supersedes a melee pursuit).");
    }
    text(StatusCode::OK, body)
```

(If `post_zone_cross` already binds a local named `body`, name the new one `resp_body` consistently.)

- [ ] **Step 8: Bump `CANONICAL_SITES`**

In `crates/eqoxide-http/src/guild.rs`, change `const CANONICAL_SITES: usize = 38;` to `41`, and extend the reconciliation comment immediately above it with:

```rust
    /// #1007 added three: `move_api.rs`'s `post_goto` / `post_follow` / `post_zone_cross` each
    /// disengage an active auto-attack (`request_attack(false)`) before accepting the new move,
    /// each in the canonical `if let Some(busy) = … .refused_json(…) { return busy; }` shape. 38 → 41.
```

- [ ] **Step 9: Run the test + the refusal lint**

Run: `~/bin/rbuild . test -p eqoxide-http --lib -- fresh_goto_while_engaging_disengages_and_echoes_it every_refusable_command_request_is_checked_by_its_http_caller` (`timeout: 420000`)
Expected: PASS. If `every_refusable_command_request_is_checked_by_its_http_caller` reports an **offender** at one of the three new sites, the `refused_json` message string picked up a `;` (or the block isn't the canonical shape) — fix the string, not `CANONICAL_SITES`. If it reports `checked.len()` below `CANONICAL_SITES`, the three sites aren't all being recognised — re-check the exact `if let Some(busy) = s.command.request_attack(false)` prefix.

- [ ] **Step 10: Run the `eqoxide-http` suite**

Run: `~/bin/rbuild . test -p eqoxide-http --lib` (`timeout: 420000`)
Expected: PASS. Watch `every_mutating_route_in_the_documented_modules_can_answer_409` (its `MODULES` list excludes `move`, so it should be unaffected) and any `post_goto`/`post_follow`/`post_zone_cross` response-shape tests (a new always-present key is additive).

- [ ] **Step 11: Commit**

Write `/home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt`:

```
feat(#1007): auto-disengage auto-attack on a fresh /move/{goto,follow,zone_cross}

Newest explicit command wins: a fresh move ends an in-flight melee
pursuit. should_disengage_for_new_move is a pure predicate (auto_attack &&
live target, no distance gate); each handler does the refusable
request_attack(false) inline in the canonical shape, so a raced toggle
returns 409 busy_attack instead of a false `"disengaged": true`.
goto/follow gain a `disengaged` bool; zone_cross appends a sentence.
guild.rs CANONICAL_SITES 38 -> 41.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01G8ggWADJdrxci4Q5MEhqJ6
```

```bash
git add crates/eqoxide-http/src/move_api.rs crates/eqoxide-http/src/guild.rs
git commit -F /home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt
```

---

### Task 7: `auto_attack` on `GET /v1/observe/debug`

**Files:**
- Modify: `crates/eqoxide-http/src/lib.rs`
  - `struct PlayerState` — add `pub auto_attack: bool,` immediately after `pub run_mode:  bool,`.
  - `PlayerState::from_game_state` — add `auto_attack:   gs.auto_attack,` immediately after `run_mode:      gs.run_mode,` (this `PlayerState { … }` literal is the only construction site).
- Modify: `crates/eqoxide-http/src/observe.rs`
  - Bind `let player_auto_attack = player.auto_attack;` immediately after `let player_run_mode = player.run_mode;`.
  - Insert `player.insert("auto_attack".into(), serde_json::json!(player_auto_attack));` immediately after the `player.insert("run_mode".into(), …)` line.
- Test: `crates/eqoxide-http/src/observe.rs`, `mod tests` (using the `body_json(&HttpState, &str)` helper).

**Interfaces:**
- Consumes: `gs.auto_attack: bool`; the module's `body_json` helper; `crate::testkit::{empty_state, set_gs}`.
- Produces: `PlayerState.auto_attack: bool` (always serialised — no `skip_serializing_if`); `body["player"]["auto_attack"]` on `/v1/observe/debug`, always present.

- [ ] **Step 1: Write the failing test**

In `crates/eqoxide-http/src/observe.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn observe_debug_player_always_carries_auto_attack() {
        // Always present, false at boot.
        let state = empty_state();
        let j = body_json(&state, "/debug").await;
        assert_eq!(j["player"]["auto_attack"], serde_json::json!(false),
            "auto_attack is always present on /observe/debug, false by default");

        // Tracks gs.auto_attack.
        let state2 = empty_state();
        set_gs(&state2, |gs| gs.auto_attack = true);
        let j2 = body_json(&state2, "/debug").await;
        assert_eq!(j2["player"]["auto_attack"], serde_json::json!(true));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `~/bin/rbuild . test -p eqoxide-http --lib -- observe_debug_player_always_carries_auto_attack` (`timeout: 420000`)
Expected: FAIL to compile (`no field auto_attack on PlayerState`) or, if only the test compiles first, FAIL on `j["player"]["auto_attack"]` being `Null`.

- [ ] **Step 3: Add the `PlayerState` field**

In `crates/eqoxide-http/src/lib.rs`, immediately after `pub run_mode:  bool,`:

```rust
    /// #1007: our own last-SENT auto-attack toggle intent (`true` = pursuing/swinging at
    /// `target_id`). Like `run_mode` above and `sitting`, this is what we told the server via
    /// `/v1/combat/attack/{on,off}` (`OP_Attack` has no ack) — NOT a server confirmation. An agent
    /// that just POSTed a disengaging `/v1/move/{goto,follow,zone_cross}` reads this to confirm the
    /// pursuit was actually called off.
    pub auto_attack: bool,
```

- [ ] **Step 4: Wire `from_game_state`**

In the `PlayerState { … }` literal inside `from_game_state`, immediately after `run_mode:      gs.run_mode,`:

```rust
            auto_attack:   gs.auto_attack,
```

- [ ] **Step 5: Bind + insert in `observe.rs`**

Immediately after `let player_run_mode = player.run_mode;`:

```rust
    let player_auto_attack = player.auto_attack;
```

Immediately after the `player.insert("run_mode".into(),               serde_json::json!(player_run_mode));` line:

```rust
        // #1007 — our own last-SENT auto-attack toggle intent. `OP_Attack` has no server ack, so
        // this is NOT a confirmation — same epistemic level as `run_mode`/`sitting` above. Always
        // present so an agent can poll "did my /move/* disengage the pursuit?" without inferring it
        // from target/hp deltas.
        player.insert("auto_attack".into(),            serde_json::json!(player_auto_attack));
```

- [ ] **Step 6: Run test to verify it passes**

Run: `~/bin/rbuild . test -p eqoxide-http --lib -- observe_debug_player_always_carries_auto_attack` (`timeout: 420000`)
Expected: PASS.

- [ ] **Step 7: Run the `eqoxide-http` suite**

Run: `~/bin/rbuild . test -p eqoxide-http --lib` (`timeout: 420000`)
Expected: PASS. If a `PlayerState`-serialisation round-trip test enumerates exact keys, add `auto_attack` to its expected set.

- [ ] **Step 8: Commit**

Write `/home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt`:

```
feat(#1007): surface auto_attack on GET /v1/observe/debug

Always-present player field, projected from gs.auto_attack. Send-time
intent, not a server confirmation (same epistemic level as run_mode /
sitting). Lets an agent confirm a disengaging /move/* actually called off
a melee pursuit.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01G8ggWADJdrxci4Q5MEhqJ6
```

```bash
git add crates/eqoxide-http/src/lib.rs crates/eqoxide-http/src/observe.rs
git commit -F /home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt
```

---

### Task 8: `docs/http-api.md` — document the new vocabulary and fields

Docs-only; no RED/GREEN cycle. Each anchor's line number drifts — read the surrounding region first and match the existing table/column format exactly. `docs/http-api.md`'s `nav_state` table **must** agree with `walker.rs::TERMINAL_NAV_STATES` (its doc says so): list `engaging` as **non-terminal**.

**Files:**
- Modify: `docs/http-api.md` only.

- [ ] **Step 1: `/v1/observe/debug` player-field list**

In the player-field bullet list (near the top of the `/v1/observe/debug` section), add:

```
- `auto_attack` (bool, always present) — our own last-sent auto-attack toggle intent (`true` = pursuing/swinging at the current target). Send-time intent only: `OP_Attack` has no server ack, so this is what we told the server, not a confirmation — the same epistemic level as `run_mode` and `sitting`.
```

- [ ] **Step 2: `nav_state` table — add `engaging`**

In the `nav_state` values table (the one listing `pending` / `idle` / `planning` / `navigating` / … / `zone_loading`), add a row:

```
| `engaging` | Auto-attack is pursuing a live target into melee range (#1007) — `drive_auto_engage_melee` is steering the body at `target_id`. **TRANSIENT**, retires to `idle` / `melee_disengaged` the first tick the pursuit ends (target died/despawned, moved beyond ~200u, `auto_attack` turned off, or a fresh `/move/{goto,follow,zone_cross}` disengaged it). `nav_goal` is `null` (a live entity, not a fixed point). **Not a terminal state** — never read it as a finished outcome. |
```

If there is a prose sentence nearby stating "the terminal words are …", leave it — `engaging` is deliberately absent from it.

- [ ] **Step 3: `nav_reason` meanings table — add two rows**

In the `nav_reason` meanings table:

```
| `melee_engaged` | Companion to `nav_state: engaging`. Auto-attack has a live target within the ~200u engage radius and is steering toward it. If a `/move/goto` was in flight when the pursuit began it was superseded once (a single `nav_goal_id` bump). |
| `melee_disengaged` | On the `idle` that `engaging` retires to: the melee pursuit ended and nothing replaced it (target died/despawned, moved out of range, `auto_attack` off, or a fresh `/move/*` disengaged it). Distinct from `stopped` (you asked via `/move/stop`) and `goto_superseded` (manual movement took over). |
```

- [ ] **Step 4: `nav_goal` null-reason list**

Wherever the doc enumerates the `nav_reason` values under which `nav_goal` is `null` (the "`nav_goal` is null when …" list), add `melee_disengaged`, and note that `nav_goal` is also `null` under `nav_state: engaging` (a melee pursuit has no fixed-point goal).

- [ ] **Step 5: `nav_local` null-reason list**

Same treatment for the `nav_local` null-reason list: add `melee_disengaged` / note `engaging` (a melee pursuit does not use the fine local planner).

- [ ] **Step 6: `disengaged` response key on the move endpoints**

In the `POST /v1/move/goto` response documentation, add:

```
- `disengaged` (bool) — `true` if this goto disengaged an active auto-attack melee pursuit ("newest explicit command wins"). `false` when auto-attack was already off or had no live target. If an auto-attack toggle was already queued, the request instead returns `409` with `{"status":"busy_attack"}` and nothing is changed.
```

Add the equivalent line to `POST /v1/move/follow`. For `POST /v1/move/zone_cross` (a text response), add a sentence: when a melee pursuit was disengaged, the 200 body ends with `Auto-attack was disengaged (a fresh zone_cross supersedes a melee pursuit).`

- [ ] **Step 7: Sanity-check + full suite**

Re-read each edited region for column-count / format consistency. Then:

Run: `~/bin/rbuild . test --workspace --locked` (`timeout: 420000`)
Expected: **≥ 1158 passed / 0 failed / 29 ignored.** (No new tests in this task; this run is the whole-plan regression gate.) If any test asserts on `docs/http-api.md` content (a doc-consistency lint), fold its required wording in here.

- [ ] **Step 8: Commit**

Write `/home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt`:

```
docs(#1007): document engaging / melee_engaged / melee_disengaged + auto_attack

nav_state gains `engaging` (transient, non-terminal); nav_reason gains
melee_engaged / melee_disengaged; /move/{goto,follow} responses gain the
`disengaged` bool and /move/zone_cross an appended sentence; the debug
player object gains the always-present auto_attack field.

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01G8ggWADJdrxci4Q5MEhqJ6
```

```bash
git add docs/http-api.md
git commit -F /home/dhenry/.claude/jobs/5d723a63/tmp/cm.txt
```

---

## After all tasks

1. **Push the branch** (it has a remote): `git push -u origin <branch>` — never `main`, never `--force`.
2. **Open a draft PR** referencing `#1007` **without** an auto-closing keyword (say "Re #1007" / "Part of #1007", not "Closes #1007"). PR body ends with:
   ```
   🤖 Generated with [Claude Code](https://claude.com/claude-code)

   https://claude.ai/code/session_01G8ggWADJdrxci4Q5MEhqJ6
   ```
3. **Report** path / branch / PR URL and the final test counts.

## Spec-coverage map (Self-Review)

| Spec section | Task |
|---|---|
| §3.1 vocabulary (`NAV_STATE_ENGAGING`, `NAV_REASON_MELEE_ENGAGED`, `NAV_REASON_MELEE_DISENGAGED`, `nav_state_is_suspended`) | Task 1 |
| §3.1 `walker.rs` re-export growth | Task 2 |
| §3.2 reconciler (`reconcile_engage_nav_state`, `engage_active` latch, `tick` call site) | Task 5 |
| §3.3 delete `drive_auto_engage_melee`'s per-tick `request_cancel_goto`; rewrite `#1109` comment; 3 stale `nav.rs` doc fixes | Task 5 |
| §3.4 `has_active_goto` | Task 3 |
| §3.4 `enter_engaging` | Task 2 |
| §3.5 widen `stamp_new_goal` idle branch to `nav_state_is_suspended` + comment | Task 4 |
| §3.6 auto-disengage on `/move/{goto,follow,zone_cross}` (via the §8 deviation: pure predicate + canonical `refused_json`) | Task 6 |
| §3.7 `engaging` stays OFF `TERMINAL_NAV_STATES` | Task 2 (`engaging_is_not_terminal` guard) |
| §3.8 routing table | Exercised across Tasks 4 & 5 tests |
| §3.9 `auto_attack` on `/v1/observe/debug` | Task 7 |
| §4 files-touched (incl. `guild.rs` `CANONICAL_SITES` 38→41) | Task 6 |
| §6 forcing tests 1–7 | Tasks 2, 4, 5, 6, 7 |
| §8 deviations (enter_engaging kept; §3.9 in scope; pure-predicate disengage; semicolon-free `refused_json` message) | Tasks 2, 6, 7 |
