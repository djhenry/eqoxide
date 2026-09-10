# Melee-engage `nav_state` — the idle-during-melee honesty gap (#1007, re-scoped)

**Status:** DRAFT FOR REVIEW. No crate code written — the brainstorming HARD-GATE is in
effect until the user approves this document. Design approved verbally in chat
("I'm happy with the rest of the design"); one change was requested and is folded in
(§3.6, auto-disengage on a fresh `/goto`). The user has said to expect further
iterations after user feedback, so this document is structured section-by-section for
cheap revision.

**Scope:** `crates/eqoxide-ipc/src/lib.rs` (new `nav_state`/`nav_reason` vocabulary +
one predicate), `crates/eqoxide-nav/src/walker.rs` (re-export + one small publish
helper), `crates/eqoxide-command/src/nav.rs` (`has_active_goto`, widen one
preservation predicate, stale-doc fixes), `crates/eqoxide-net/src/action_loop.rs`
(a new reconciler in `tick`; delete one line from `drive_auto_engage_melee`),
`crates/eqoxide-http/src/move_api.rs` (auto-disengage in three handlers),
`docs/http-api.md` (state/reason tables). Optionally
`crates/eqoxide-http/src/lib.rs` + `crates/eqoxide-http/src/observe.rs` (§3.9).
It does **NOT** touch the pathfinding tiers, the controller, or the combat wire code.

**Closes / unblocks:** #1007, re-scoped. #1007 stays **OPEN** until this lands. The
original #1007 (life-halt goal-word freeze) already shipped — `nav.rs:176-209` +
`docs/http-api.md:462` — and is revisited here only to widen one predicate
(`nav_state_is_life_halt` → `nav_state_is_suspended`, §3.5). Related honesty issues in
the same family: #343 (`connected: true` forever), #349 (`goal_id` churn), #725
(in-progress `nav_state` that never retires), #1000/#1109.

---

## 0. Provenance rules for this document

Every number below is one of three kinds, and it is labelled:

* **[cited]** — traceable to an issue, a PR body, or a `file:line` on `main` @ `09ec76dc`.
* **[derived]** — arithmetic done here from cited constants. Reproducible, not measured.
* **[guess]** — not traced and not measured. **Re-derive before relying on it.**

There are no unlabelled numbers. If you find one, it is a bug in this document.

---

## 1. What is actually true today (read from `main` @ `09ec76dc`)

### 1.1 The `tick` loop — `crates/eqoxide-net/src/action_loop.rs`, `fn tick` @1422

`NAV_TICK_MS: u128 = 150` [cited `action_loop.rs:9`]. The order that matters:

| # | step | line | runs when < 150 ms since last tick? | on fire, does `tick` return early? |
|---|------|------|-----------------------------------|-----------------------------------|
| 1 | `drain_combat` (all `drain_*`) | `1449` | **yes** — top-of-tick, ungated | no |
| 2 | `stream_position` | `1463` | **yes** | no |
| 3 | `walker.nav_halt_if_dead` | `1470` | **yes** | **yes** if dead (`return` @1471) |
| — | **← the reconciler is inserted here (§3.2)** | `~1473` | **yes** | no |
| 4 | `walker.apply_fast_steering` | `1474` | **yes** | no |
| 5 | **the 150 ms gate** — `if last_tick.elapsed() < NAV_TICK_MS { return }` | `1476` | — | **yes**, most ticks |
| 6 | `tick_give` | `1485` | no (gated) | no |
| 7 | `drive_auto_pet_combat` | `1490` | no (gated) | no |
| 8 | `drive_auto_engage_melee` | `1492` | no (gated) | **yes** if it returns `true` (`{ return; }`) |
| 9 | `walker.drive_chase` | `1497` | no (gated) | no |
| 10 | `walker.drive_teleport_detect` | `1499` | no (gated) | no |
| 11 | `walker.resolve_goal` | `1501` | no (gated) | **yes** if `None` (no goto goal) |
| 12 | `walker.drive_walk` | `1511` | no (gated) | — |

Two consequences the design turns on:

* `drain_combat` @1449 sets **both** `self.auto_attack` (`action_loop.rs:2295`) and
  `gs.auto_attack` (`action_loop.rs:2298`) from the A3 command queue *before* anything
  below runs. A reconciler at ~1473 reads an already-current `self.auto_attack` for
  this tick. **No drain needs hoisting.**
* `drive_auto_engage_melee` @1492 is **below** the 150 ms gate and returns `true` @2822,
  so on a tick where melee-engage fires, `tick` never reaches `resolve_goal`
  (@1501) or `drive_walk` (@1511). This is why the state word must be reconciled by
  a step **above** the gate, not by the driver.

### 1.2 `drive_auto_engage_melee` — `action_loop.rs` @2756

`fn drive_auto_engage_melee(&mut self, stream, gs) -> bool`. Gates (all [cited]):

| gate | line | value |
|------|------|-------|
| `if self.auto_attack` | `2760` | — |
| `if let Some(tid) = gs.target_id` | `2761` | — |
| `gs.world.entities.get(&tid).filter(\|e\| !e.dead).map(\|e\| (e.x, e.y))` | `2780` | #1109 dead-target filter |
| `let dist = (dx*dx + dy*dy).sqrt()` (2D, `dx = ex - gs.player_x`, `dy = ey - gs.player_y`) | `2784` | — |
| `if dist < 200.0` | `2785` | `200.0` [cited] — "engage targets within ~200u" |
| `const MELEE: f32 = 5.0` | `2786` | [cited] |
| `const PET_STANDOFF: f32 = 25.0` | `2787` | [cited] — pet classes hang back |

Inside the `dist < 200.0` branch it does exactly two kinds of work:

1. **Steer** — if `dist > engage` (`engage` = `PET_STANDOFF` when `gs.pet_id.is_some()`
   else `MELEE`): writes `*self.controller.nav_intent.lock().unwrap() = Some(MoveIntent{…})`
   toward the target. Else (in range): `*nav_intent = None` and a stationary
   `send_position_update` to keep server facing current.
2. **`self.command.request_cancel_goto(); // cancel any stale walk`** — `action_loop.rs:2821`.
   **This is the only line in the whole function that touches nav command state.** It is
   immediately followed by `return true;` @2822.

The function **never** calls `set_nav_state_because` and never writes a `nav_state`
word. The `nav_state` an agent reads while this driver is active is therefore whatever
`request_cancel_goto` last published — see §2.

The #1109 comment @2762-2779 explains the `!e.dead` filter and, at @2766-2767, says
the driver "`request_cancel_goto()`s every 150 ms tick" — that phrasing describes the
line this design deletes and needs a light rewrite (§3.3).

### 1.3 The `nav_state` writer trio — `crates/eqoxide-ipc/src/lib.rs`

`pub struct NavStatus` @2012, `#[derive(Clone, Debug, PartialEq)]`. All fields `pub`
(cross-crate writes happen — see §1.4). Fields: `state: String`, `reason: Option<String>`,
`goal_id: u64`, `goal: Option<[f32;3]>`, `blocked_goal`, `blocked_frontier`, `tier`,
`local: Option<NavLocal>`, `stall`, `local_planner_dead: bool`.

Three methods, **each destructures `NavStatus` with no `..`** so a new field is
`error[E0027]` until every one of them decides its fate — this is the load-bearing
"exhaustive writer" net (#732 / #851):

| method | line | `state` | `reason` | `goal_id` | `goal` | `local` | `blocked_*`/`tier`/`stall` |
|--------|------|---------|----------|-----------|--------|---------|---------------------------|
| `retire_to_idle(why)` | `2250` | `"idle"` | `why` | **kept** | cleared | cleared | cleared |
| `stamp_fresh_goal(state, why, goal)` | `2321` | `state` (≠ `idle`) | `why` | kept | set from arg | cleared | cleared |
| `transition_within_goal(state, why)` | `2360` | `state` (≠ `idle`) | `why` | **kept** | **kept** | **kept** | cleared |

`impl Default for NavStatus` @2387: `state: "idle"`, `reason: None`, `goal_id: 0`,
everything else `None`/`false`. This is the only writer that produces
`idle` + `reason: null` — the reserved "no request since boot" reading (#725).

Life-halt vocabulary, same file @1929-1993:

| item | line | value |
|------|------|-------|
| `NAV_STATE_DEAD` | `1957` | `"dead"` |
| `NAV_STATE_HALTED_HP_ZERO` | `1975` | `"halted_hp_zero"` |
| `nav_state_is_life_halt(s) -> bool` | `1991` | `s == DEAD \|\| s == HALTED_HP_ZERO` |

`crates/eqoxide-nav/src/walker.rs:76` re-exports them:
`pub use eqoxide_ipc::{NAV_STATE_DEAD, NAV_STATE_HALTED_HP_ZERO, nav_state_is_life_halt};`

Publish entry points on `Walker` (`walker.rs`):

| fn | line | behaviour |
|----|------|-----------|
| `set_nav_state_because(state, reason)` | `675` | `debug_assert!(!(state=="idle" && reason.is_none()))` @684; then `write_nav_state_locked` |
| `write_nav_state_locked(s, state, reason)` | `701` | `if state == "idle" { s.retire_to_idle(reason); return; }` @708; else `if s.state != state \|\| s.reason.as_deref() != reason { s.transition_within_goal(state, reason); }` @709 — **the change-check makes re-publishing the same word a no-op** |
| `nav_state_is(state) -> bool` | `981` | `self.nav.nav_state.lock().unwrap().state == state` |
| `retire_life_halt` | `1418` | **the model for our retirement.** `let current = …state.clone()` @1419; `if nav_state_is_terminal(&current) { return; }` @1424; `why` = `RESPAWNED`/`HP_RESTORED`/`return` by `current`; `set_nav_state_because("idle", Some(why))` @1434. Its mutation-check tests are `action_loop.rs:8217` and `:8288` [cited via prior session] |

### 1.4 `stamp_new_goal` idle-branch preservation — `crates/eqoxide-command/src/nav.rs`

`fn stamp_new_goal(&self, new_state, reason, goal) -> u64` @141. `s.goal_id += 1`
**always** @155. Then, for the `idle` branch (@176-210) — heavy #1007/#1000 comment
@176-201, then this preservation block @202-209:

```rust
let halted = eqoxide_ipc::nav_state_is_life_halt(&s.state)
    .then(|| (s.state.clone(), s.reason.clone()));
s.retire_to_idle(reason);
if let Some((halt_state, halt_reason)) = halted {
    s.state  = halt_state;
    s.reason = halt_reason;
}
return s.goal_id;
```

This exists because the field has **two unordered writers on two threads** (#1007 §1,
measured: the word alternated 46 times in 0.386 s while `hp` held 0 and `dead` held
`false`): the net thread's `nav_halt_if_dead` republishes the halt every tick, and the
**render thread's per-frame `request_cancel_goto`** writes `idle`/`goto_superseded`.
The block lets `retire_to_idle` stay the single exhaustive clearer, then restores the
two published *words* by explicit assignment (deliberately not by destructuring, so a
new `NavStatus` field is still force-decided in `retire_to_idle`).

Callers of the idle branch: `request_stop` @255 (`"idle"`, `NAV_REASON_STOPPED` @30),
`request_cancel_goto` @278 (`"idle"`, `NAV_REASON_GOTO_CANCELLED` = `"goto_superseded"`
@37). Non-idle callers: `request_goto` @225, `request_follow` @235,
`request_zone_cross` @291 — all `stamp_new_goal("pending", None, …)`, which routes to
`stamp_fresh_goal` and **clobbers any prior word synchronously at HTTP time**.

`request_cancel_goto` @273: clears `goto_target` only (leaves `goto_entity`), then
`stamp_new_goal("idle", Some(NAV_REASON_GOTO_CANCELLED), None)`.

`goto_target()` accessor @350 is `#[cfg(any(test, feature = "test-fixtures"))]` — **not
available in a release build.**

Stale doc mentions of "the auto-melee-engage override" as a `request_cancel_goto`
caller, all of which go stale when §3.3 deletes `action_loop.rs:2821`:
module doc @16-17, `NAV_REASON_GOTO_CANCELLED` doc @32-37, `request_cancel_goto` doc
@258-272.

### 1.5 The render-thread per-frame `request_cancel_goto` — `src/app.rs`

| site | line | condition | cadence |
|------|------|-----------|---------|
| WASD | `1899` | `wasd_active` (any of `de/dn/dz != 0`) | **every frame**, unconditional while a movement key is held |
| HTTP `/move/manual` | `1926` | `manual` slot live (`Instant::now() < m.until`) | **every frame** until the deadline |
| WASD keydown edge | `2835` | `KeyboardInput` Pressed, W/A/S/D/Q/E | once per keypress |
| camera reset (R / F9) | `2840` | `KeyboardInput` Pressed, R/F9 | once per keypress |

`request_cancel_goto` here goes through `stamp_new_goal("idle", "goto_superseded", …)`
(§1.4) and, crucially, **bumps `goal_id` every frame** while WASD is held. That
`goal_id` churn is pre-existing and is a **Non-Goal** for #1007 (§7). What §3.5 fixes
is the *word* flap: with the preservation predicate widened to include `engaging`,
these per-frame writes can no longer flip `engaging` → `idle` between two polls.

### 1.6 `nav_state` vocabulary & `TERMINAL_NAV_STATES` — `walker.rs:99`

`pub const TERMINAL_NAV_STATES: [&str; 5] = ["idle", "arrived", "no_path", "search_exhausted", "blocked"];`
`nav_state_is_terminal(s)` @113 = `TERMINAL_NAV_STATES.contains(&s)`.

The documented **"#1007 trap"** (`walker.rs:101-113`): the surrounding prose tempts a
reader to "fix" the absence of `dead`/`halted_hp_zero` from this array. Adding a word
here makes every `!nav_state_is_terminal`-guarded retirement (`retire_life_halt` @1424,
`resolve_goal` None-branch @1557) **dead code**, and the state becomes permanent — "a
bounded lie converted into a never-clearing one." `engaging` is transient (§3.8) and
**must not** be added to this array.

Documented state list (`docs/http-api.md:363-378`): `pending | idle | planning |
navigating | navigating_partial | navigating_stalled | following | arrived | no_path |
search_exhausted | blocked | zone_loading`, plus `dead` / `halted_hp_zero`.
Documented idle-reason list (`http-api.md:366`, repeated @1449 and @1722-1723):
`zoned, stopped, goto_superseded, goal_dropped, respawned, hp_restored,
zone_cross_dropped_unhandled`.

### 1.7 The move handlers — `crates/eqoxide-http/src/move_api.rs`

| handler | line | accept call | response |
|---------|------|-------------|----------|
| `post_goto` | `330` | `let goal_id = s.command.request_goto(target);` @400 | `json(200, {status:"navigating", goal:[…], goal_id, matched, ignored_fields, zone_assets_pending, hold, note})` @417-433 |
| `post_follow` | `445` | `let goal_id = s.command.request_follow(matched.key.clone(), pos);` @509 | `json(200, {status:"following", goal_id, matched, hold})` @512-517 |
| `post_stop` | `530` | `let goal_id = s.command.request_stop();` @534 | `text(200, "navigation stopped [goal_id={goal_id}]")` @536 |
| `post_zone_cross` | `579` | `let goal_id = s.command.request_zone_cross(zone_id);` @613 | `text(200, "zone_cross … accepted [goal_id={goal_id}] — walking …")` @618-623 |
| `post_manual` | `69` | (writes the render-thread `manual_move` slot, not `command`) | `text(…)` |

All guard `require_live_session(&s)` first (loads `s.game_state.load()` once — a
`409` life-halt gate and a `503` session-liveness gate). `text` @24, `json` @31.
`s.game_state.load()` gives a snapshot with `auto_attack: bool` and `target_id:
Option<u32>` and `world.entities` — everything the disengage predicate needs.

### 1.8 `auto_attack` state

* `GameState::auto_attack: bool` — `crates/eqoxide-core/src/game_state.rs:1608`
  ("what we told the server, not a server confirmation"). Set by `drain_combat`
  @`action_loop.rs:2298`.
* `ActionLoop::auto_attack: bool` — `action_loop.rs:546`, init `false` @717, set by
  `drain_combat` @2295.
* `request_attack(on: bool) -> bool` — `crates/eqoxide-command/src/combat.rs:22` =
  `self.enqueue(&self.combat.attack, on, false, Action::CombatAttack)`. **Asynchronous
  enqueue** onto the A3 queue; drained by `drain_combat` @1449 which sends
  `OP_AUTO_ATTACK` and updates both `auto_attack` mirrors. `take_attack()` @`combat.rs:68`.
* **`PlayerState` (`crates/eqoxide-http/src/lib.rs:129`) has NO `auto_attack` field.**
  The doc @289 mentions `auto_attack` only as a trust analogy for `run_mode`.
  `from_game_state` @349 sets `run_mode: gs.run_mode` @462 but nothing for auto-attack.
  `observe::get_debug` hand-builds its `player` object with `player.insert(…)` calls
  (`observe.rs:1523-1727`) — nothing serialises `PlayerState` whole.

---

## 2. The defect, precisely

**`auto_attack` is not "swing when a mob wanders into range."** It means *pursue and
stay in melee range of `target_id`* — `drive_auto_engage_melee` actively steers the
controller toward the target (`nav_intent`) every gated tick, "regardless of any
pending goto" (`action_loop.rs:2758`).

The honesty gap: while that driver is pursuing a mob, it calls `request_cancel_goto()`
every gated tick (`action_loop.rs:2821`), which publishes **`nav_state: "idle"`,
`nav_reason: "goto_superseded"`**. An agent polling `/v1/observe/debug` during a melee
chase reads `idle` — "nothing to do, ready for work" — while the body is walking
across the zone toward a skeleton. This is the #343 family: *a confident plausible
wrong answer is worse than a crash or a null.* The correct reading is "navigating into
melee," and there is no word for it.

Two secondary problems:

1. **`goto_superseded` is overloaded.** WASD, `/move/manual`, and the melee driver all
   publish the same `idle`/`goto_superseded`. An agent that issued `/goto` and sees
   `goto_superseded` cannot tell "the human grabbed the keyboard" from "auto-attack ate
   my route." A distinct `engaging` word (and a distinct disengage reason) separates
   them.
2. **`goal_id` churn (#349).** `drive_auto_engage_melee` calls `request_cancel_goto`
   on *every gated tick* it is active, and each call does `s.goal_id += 1`
   (`nav.rs:155`). An agent using the #349 rule ("ignore any `nav_state` whose
   `nav_goal_id` is lower than the one your POST returned") sees the id advance ~7×/s
   for the whole pursuit. The reconciler makes the supersede **one-shot** per engage
   episode.

---

## 3. Design

A **reconciler** — `reconcile_engage_nav_state` — runs once per `tick`, above the
150 ms gate, peering with `nav_halt_if_dead`. It is a pure predicate over
already-current state (`self.auto_attack`, `gs.target_id`, `gs.world.entities`,
`gs.player_x/y`). It owns exactly one thing: the `engaging` `nav_state` word and its
retirement. `drive_auto_engage_melee` keeps owning steering and facing, and stops
touching nav command state entirely. A fresh explicit `/move` command
(`goto`/`follow`/`zone_cross`) turns `auto_attack` **off** in its handler — "newest
explicit command wins" — which makes the predicate go false and retires `engaging`
cleanly.

### 3.1 New vocabulary — `crates/eqoxide-ipc/src/lib.rs`, beside the life-halt block (@1929-1993)

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

And `walker.rs:76` grows to re-export **all four** new items, alongside the three
life-halt ones already there — so every downstream crate (`eqoxide-nav`,
`eqoxide-net`) names them by one path (`eqoxide_nav::walker::…`) and no new
crate→`eqoxide-ipc` dependency edge is needed:

```rust
pub use eqoxide_ipc::{
    NAV_STATE_DEAD, NAV_STATE_HALTED_HP_ZERO, NAV_STATE_ENGAGING,
    NAV_REASON_MELEE_ENGAGED, NAV_REASON_MELEE_DISENGAGED,
    nav_state_is_life_halt, nav_state_is_suspended,
};
```

(`eqoxide-command`'s `nav.rs` already names `eqoxide_ipc::` directly for
`nav_state_is_life_halt` — §3.5 keeps that path for `nav_state_is_suspended`.)

### 3.2 The reconciler — `crates/eqoxide-net/src/action_loop.rs`, new `fn`, called @~1473

**Placement:** in `tick`, immediately after the `nav_halt_if_dead` return block
(@1470-1472), before `apply_fast_steering` (@1474). Rationale, all from §1.1:

* **Above the 150 ms gate (@1476)** so the word is reconciled every tick, not once per
  gate period — matching `nav_halt_if_dead`.
* **Above every early return below it** — the gate's `return` @1477, the driver's
  `return` @1492, the `resolve_goal`-`None` early return just below @1501 — so a melee
  episode can never leave a stale word standing because `tick` bailed before reaching a
  reconciliation point.
* **After `nav_halt_if_dead`'s `return`** so it never runs while the character is
  dead/hp-zero — the life-halt words own the field then, and the reconciler must not
  compete. (The predicate would be false anyway, but ordering makes it structural.)
* **After `drain_combat` (@1449)** so `self.auto_attack` is this tick's value.

**New field on `struct ActionLoop`** (beside `auto_attack` @546):

```rust
/// #1007: latched true for the duration of one melee-engage episode — set when
/// `reconcile_engage_nav_state` first publishes `engaging`, cleared when it retires.
/// Makes the one-shot goto-supersede edge-triggered (one `goal_id` bump per episode,
/// not one per gated tick — #349).
engage_active: bool,
```

init `false` @717 (beside `auto_attack: false`).

**The reconciler:**

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
        // (§3.4) makes a re-publish of the same `engaging`/`melee_engaged` a no-op —
        // no lock write, no `goal_id` touch. It also nulls `goal`/`local` on the
        // transition in (§3.4), so `nav_goal` is null under `engaging` even when the
        // episode began from a stale `arrived`.
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

Call site (@~1473):

```rust
    if self.walker.nav_halt_if_dead(gs) {
        return;
    }

    self.reconcile_engage_nav_state(gs);   // ← NEW (#1007)

    self.walker.apply_fast_steering(gs);
```

### 3.3 `drive_auto_engage_melee` loses its one nav line

**Delete `action_loop.rs:2821`** — `self.command.request_cancel_goto(); // cancel any
stale walk` — the only nav-command line in the function. `return true;` @2822 stays;
the driver still handles the tick (steer + face) and still short-circuits `tick`.

Rewrite the #1109 comment @2762-2779: the `!e.dead` filter and its rationale stay, but
the clause "and `request_cancel_goto()`s every 150 ms tick — silently cancelling the
agent's own `/v1/move/goto`" (@2766-2767) is now false. Replace with: a dead target
that stays in `world.entities` until the server deletes the spawn would keep the
*reconciler's* `want_engage` true (pinning `nav_state` at `engaging`) if the `!e.dead`
filter were absent — so both the driver and the reconciler filter it, for the same
reason.

Fix the three stale doc mentions in `nav.rs` that name "the auto-melee-engage override"
as a `request_cancel_goto` caller (§1.4: module doc @16-17, `NAV_REASON_GOTO_CANCELLED`
doc @32-37, `request_cancel_goto` doc @258-272). After this change the callers of
`request_cancel_goto` are: WASD (`app.rs:1899`, `:2835`), `/move/manual`
(`app.rs:1926`), camera-reset (`app.rs:2840`), and **the #1007 reconciler's one-shot
episode-entry supersede** (`action_loop.rs`, §3.2). Update the prose to that list.

### 3.4 `has_active_goto` on `CommandState`; `enter_engaging` on `Walker`

**`crates/eqoxide-command/src/nav.rs`** — a release-safe sibling of the test-only
`goto_target()` @350:

```rust
/// Is a `/move/goto` (or the goto half of a `/follow`) target currently set? Un-gated
/// (unlike `goto_target()` @350, which is `#[cfg(test)]`-only) — the #1007 reconciler
/// needs it in a release build to decide whether entering melee must supersede a walk.
pub fn has_active_goto(&self) -> bool {
    self.nav.goto_target.lock().unwrap().is_some()
}
```

**`crates/eqoxide-nav/src/walker.rs`** — a small publish helper, because `engaging`
carries **no coordinate goal** (the target is a live entity, read from
`target_id`/`target_name`/`target_hp_pct`), so `nav_goal` must be `null` under it — the
same discipline #732 enforces for every `idle`. `set_nav_state_because("engaging", …)`
alone would route through `transition_within_goal`, which *keeps* `goal` and `local` —
and entering `engaging` from a stale terminal `arrived` (whose `goto_target` was already
cleared on arrival, `walker.rs:2107`, but whose `NavStatus.goal` still holds the arrived
coords) would then publish `engaging` beside a stale `nav_goal`. That is exactly the
#732 defect class running again.

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

The two explicit `= None` lines follow the same "override after the exhaustive writer,
deliberately not by destructuring" pattern already used in `stamp_new_goal`'s idle
branch (`nav.rs:205-208`): a new `NavStatus` field is still force-decided (E0027) inside
`transition_within_goal`, and these two lines only narrow two of its keeps.

Retirement needs no helper — the reconciler's else-branch calls
`set_nav_state_because("idle", Some(NAV_REASON_MELEE_DISENGAGED))`, which
`write_nav_state_locked` routes straight to `retire_to_idle` (clears `goal`/`local`/
`blocked_*`/`tier`/`stall`, keeps `goal_id`). Guarded by `nav_state_is(NAV_STATE_ENGAGING)`
at the call site so it only ever retires the reconciler's own word.

### 3.5 Widen the `stamp_new_goal` idle-branch preservation to `nav_state_is_suspended`

**`crates/eqoxide-command/src/nav.rs:202`** — one identifier:

```rust
let halted = eqoxide_ipc::nav_state_is_suspended(&s.state)   // was: nav_state_is_life_halt
    .then(|| (s.state.clone(), s.reason.clone()));
```

Effect: when a goal-level event reaches the idle branch — `request_stop` (`/move/stop`),
or a per-frame `request_cancel_goto` from WASD (`app.rs:1899`) or `/move/manual`
(`app.rs:1926`) — and the current word is `engaging`, the published `state`/`reason`
are preserved (goal is still retired, `goal_id` still bumps). Without this, the
render thread's unconditional per-frame `request_cancel_goto` under held WASD would
flip `engaging` → `idle`/`goto_superseded` between two agent polls — reintroducing the
#1007 flap on a different axis.

Light comment rewrite in the same block (@176-201): the comment currently reasons
purely about the life halt. Add a sentence that the same argument covers `engaging` — a
`/stop` or a manual-movement supersede says nothing about whether auto-attack is still
pursuing, and only the reconciler (which re-affirms `engaging` on the very next tick
if the pursuit predicate still holds, or retires it to `melee_disengaged` if not) can
know. Keep the measured-flap paragraph as-is (it is the orchestrator's #1007 §1 figure,
not re-derived here).

**Behavioural note — `/move/stop` while engaging.** `/stop` clears `goto_target`/
`goto_entity`/`zone_cross` and bumps `goal_id`, but the published word stays
`engaging`/`melee_engaged`, and the reconciler re-affirms it next tick. This is
deliberate and matches the approved design: `auto_attack` means "pursue `target_id`,"
`/stop` cancels the *goal* and says nothing about the *posture*, so a bare `/stop`
with a live nearby target and auto-attack still on **legitimately keeps pursuing**. To
actually stop, the agent turns auto-attack off (`DELETE /v1/combat/attack`), which the
reconciler sees on the next tick and retires `engaging` → `idle`/`melee_disengaged`.
This mirrors the life-halt `/stop` exception already documented at `http-api.md:462`.

### 3.6 Auto-disengage on a fresh `/move/{goto,follow,zone_cross}`  — the one requested change

"Newest explicit command wins." When one of these handlers accepts a request **and**
auto-attack is on with a live target, the handler also disengages auto-attack, so the
agent's new destination is not immediately fought by `drive_auto_engage_melee` steering
back at the mob.

**`crates/eqoxide-http/src/move_api.rs`.** A shared helper, called in each of the three
handlers immediately before the `request_{goto,follow,zone_cross}` accept:

```rust
/// #1007: a fresh explicit destination auto-disengages auto-attack — "newest explicit
/// command wins." Returns whether the handler decided to disengage (auto_attack was on
/// AND a live target was set), so the response can echo it. `request_attack(false)` is
/// an async enqueue drained by `drain_combat`; the return value is the HANDLER's
/// decision, independent of a momentary A3 `409` on an in-flight toggle.
fn disengage_for_new_move(s: &HttpState) -> bool {
    let gs = s.game_state.load();
    let live_target = gs.target_id
        .and_then(|tid| gs.world.entities.get(&tid))
        .is_some_and(|e| !e.dead);
    if gs.auto_attack && live_target {
        s.command.request_attack(false);
        true
    } else {
        false
    }
}
```

Wire-up:

| handler | change |
|---------|--------|
| `post_goto` @400 | `let disengaged = disengage_for_new_move(&s);` before the accept; add `"disengaged": disengaged` to the JSON @417-433 |
| `post_follow` @509 | same; add `"disengaged": disengaged` to the JSON @512-517 |
| `post_zone_cross` @613 | `let disengaged = disengage_for_new_move(&s);` before the accept; when `true`, append one sentence to the `text(…)` body @618-623 — e.g. `" Auto-attack was disengaged (a fresh zone_cross supersedes a melee pursuit)."`. No JSON conversion. |

`post_stop` and `post_manual` are **untouched** — `/stop` cancels a goal without
touching posture (§3.5), and `/manual` is the documented "reposition mid-fight" escape
hatch. Neither disengages.

**Deliberate asymmetry with the reconciler predicate.** `disengage_for_new_move`
tests only `auto_attack && live target` — it does **not** apply the reconciler's
`< 200.0` distance gate (§3.2). Rationale: `auto_attack` with a live target is a
standing order to pursue *whatever the distance*, so a `/goto` issued while the target
is still 300u away (not yet `engaging`, but about to be) must still disengage — the
agent's intent ("go there instead") is identical. Gating the handler on 200u would
leave `auto_attack` on for a `/goto` fired a fraction of a second too early, and the
character would abandon the new route the instant the mob closed to 200u. So the
handler is the broader predicate on purpose.

Sequence after a disengaging `/goto` (scenario 2b, §5): handler enqueues
`request_attack(false)` and returns `200 {… "disengaged": true}`. Next `tick`:
`drain_combat` sets `auto_attack = false` → reconciler's `want_engage` is false →
`engaging` (if it was showing) retires to `idle`/`melee_disengaged`, then
`request_goto` had already stamped `pending` synchronously at HTTP time, so the
observable sequence is `engaging → pending → planning → navigating` with **no
observable `idle`** (the retire and the `pending` stamp both happen before the next
agent poll can land; the `pending` was stamped at HTTP accept time, ahead of the
reconciler tick).

### 3.7 `engaging` stays OFF `TERMINAL_NAV_STATES`

No change to `walker.rs:99`. This section exists so the implementer does not "tidy" it.
`engaging` is transient — retired by the reconciler's else-branch the first tick the
pursuit predicate is false. Adding it to `TERMINAL_NAV_STATES` would make
`resolve_goal`'s None-branch retirement (@1557, `!nav_state_is_terminal` guard) skip it,
and `engaging` would then stick after the mob dies until the next `/move/*` — the exact
"#1007 trap" the doc at `walker.rs:101-113` warns against. A forcing test pins this
(§6, test 5).

### 3.8 State routing summary

| trigger | writer | path | `state` after | `reason` | `goal` | `goal_id` |
|---------|--------|------|---------------|----------|--------|-----------|
| pursuit predicate true, 1st tick, goto active | reconciler | `request_cancel_goto` then `enter_engaging` | `engaging` | `melee_engaged` | `null` | +1 (once) |
| pursuit predicate true, 1st tick, no goto | reconciler | `enter_engaging` | `engaging` | `melee_engaged` | `null` | unchanged |
| pursuit predicate true, later ticks | reconciler | `enter_engaging` (idempotent) | `engaging` | `melee_engaged` | `null` | unchanged |
| pursuit predicate false, was `engaging` | reconciler | `set_nav_state_because("idle", …)` → `retire_to_idle` | `idle` | `melee_disengaged` | `null` | unchanged |
| `/move/stop` while `engaging` | `request_stop` | idle branch, `nav_state_is_suspended` preserves word | `engaging` | `melee_engaged` | `null` | +1 |
| WASD / `/manual` per-frame cancel while `engaging` | `request_cancel_goto` | idle branch, `nav_state_is_suspended` preserves word | `engaging` | `melee_engaged` | `null` | +1 per frame (pre-existing, Non-Goal) |
| fresh `/goto` while `engaging` + live target | handler + reconciler | `request_attack(false)` → next tick predicate false → retire; `request_goto` stamped `pending` at accept | `pending`→… | — | `[x,y,z]` | +1 (the goto) |

### 3.9 Optional honesty add-on — `auto_attack` on `/v1/observe/debug`

Not required by the fix. An agent debugging "why did my `/goto` come back
`disengaged: true`?" or "why is `nav_state` `engaging`?" currently cannot read
auto-attack posture from any endpoint. Small, isolated, and in the honesty spirit:

* `PlayerState` (`crates/eqoxide-http/src/lib.rs:129`): add `pub auto_attack: bool;`.
* `from_game_state` @349: `auto_attack: gs.auto_attack,`.
* `observe::get_debug` (`observe.rs`, beside the `run_mode` insert @1569):
  `player.insert("auto_attack".into(), serde_json::json!(player_auto_attack));`
  (nothing serialises `PlayerState` whole — §1.8).
* `docs/http-api.md`: add `auto_attack` to the `/v1/observe/debug` player-field list
  (@36) and a one-line note that, like `run_mode`/`sitting`, it is last-sent intent,
  not a server confirmation.

**Recommendation:** include it — it is ~4 lines + a doc line, it closes the obvious
follow-up question the new `disengaged`/`engaging` observables raise, and it matches
the existing `run_mode` precedent exactly. If the user would rather keep this PR
minimal, it splits cleanly into its own change.

---

## 4. Files touched

| file | change | § |
|------|--------|---|
| `crates/eqoxide-ipc/src/lib.rs` | `NAV_STATE_ENGAGING`, `NAV_REASON_MELEE_ENGAGED`, `NAV_REASON_MELEE_DISENGAGED`, `nav_state_is_suspended` | 3.1 |
| `crates/eqoxide-nav/src/walker.rs` | re-export the new ipc items @76; `enter_engaging` helper | 3.1, 3.4 |
| `crates/eqoxide-command/src/nav.rs` | `has_active_goto`; `nav_state_is_life_halt`→`nav_state_is_suspended` @202; comment rewrite @176-201; 3 stale doc fixes | 3.4, 3.5, 3.3 |
| `crates/eqoxide-net/src/action_loop.rs` | `engage_active` field; `reconcile_engage_nav_state` + call @~1473; **delete** line 2821; #1109 comment rewrite @2762-2779 | 3.2, 3.3 |
| `crates/eqoxide-http/src/move_api.rs` | `disengage_for_new_move` helper; wire into `post_goto`/`post_follow`/`post_zone_cross`; `disengaged` in responses | 3.6 |
| `docs/http-api.md` | `nav_state` table +`engaging` row @363-378; `nav_reason` +`melee_engaged`/`melee_disengaged` @459+; idle-reason lists @366/@1449/@1722; `#725` "can never stick" note @380-396 (name `engaging` as transient-not-terminal); `disengaged` key on `/goto`/`/follow` bodies | 3.1, 3.6, 3.7 |
| `crates/eqoxide-http/src/lib.rs` + `observe.rs` + `docs/http-api.md` | **optional** — `auto_attack` on `PlayerState`/`from_game_state`/`get_debug` | 3.9 |

---

## 5. State machine & scenario traces

```mermaid
stateDiagram-v2
    [*] --> idle
    idle --> engaging: auto_attack ON + live target < 200u\n(reconciler, tick above the gate)
    navigating --> engaging: same predicate;\none-shot request_cancel_goto (goal_id +1)
    arrived --> engaging: same predicate (goal nulled by enter_engaging)
    engaging --> engaging: predicate still true\n(enter_engaging idempotent)
    engaging --> idle: predicate false —\ntarget dead/despawned/>200u,\nauto_attack OFF,\nor /goto,/follow,/zone_cross disengaged\n(reason: melee_disengaged)
    engaging --> pending: fresh /goto,/follow,/zone_cross\n(request_* stamps pending at HTTP time;\nreconciler retires engaging same tick)
    note right of engaging
        NOT in TERMINAL_NAV_STATES.
        /move/stop keeps the word (goal_id +1).
        WASD/-/manual per-frame cancel keeps the word.
        nav_goal is always null here.
    end note
```

**Scenario 1 — auto-attack a nearby live mob, no goto in flight.**
`POST /v1/combat/attack` → `drain_combat` sets `auto_attack=true` →
reconciler tick: `want_engage=true`, `has_active_goto()=false`, `enter_engaging()` →
`nav_state: engaging`, `nav_reason: melee_engaged`, `nav_goal: null`, `nav_goal_id`
unchanged. `drive_auto_engage_melee` @1492 steers toward the mob. Agent polling
`/observe/debug` reads `engaging` for the whole pursuit. Mob dies → next tick
`want_engage=false` (`!e.dead` filter) → `engaging` retires to `idle` /
`melee_disengaged`.

**Scenario 2a — `/goto` elsewhere, THEN auto-attack a mob that is within 200u of the route.**
`/goto` → `pending` → `planning` → `navigating` (goal_id = N). Agent turns on
auto-attack with a live target 150u away → reconciler tick: `want_engage=true`,
`engage_active=false`, `has_active_goto()=true` → **one** `request_cancel_goto()`
(goal_id → N+1, `goto_target` cleared, `goal` nulled) → `enter_engaging()` →
`nav_state: engaging`, `nav_goal_id: N+1`. No further `goal_id` churn for the rest of
the pursuit. Observable sequence for an agent watching goal N:
`navigating → engaging` (at N+1). **No `idle` is ever observable** — the
`request_cancel_goto`'s `idle` write is overwritten by `enter_engaging()` on the same
tick, same thread, before any poll can land.

**Scenario 2b — `/goto` elsewhere WHILE already engaging (the requested change).**
Currently `engaging` (goal_id = M). `POST /v1/move/goto {x,y,z}` → handler:
`disengage_for_new_move` sees `auto_attack && live target` → `request_attack(false)`
enqueued, returns `true` → `request_goto` stamps `pending` synchronously (goal_id → M+1)
→ `200 {"status":"navigating","goal_id":M+1,"disengaged":true, …}`. Next `tick`:
`drain_combat` sets `auto_attack=false` → reconciler `want_engage=false`, but
`nav_state` is already `pending` (not `engaging`), so the guarded retire is a no-op;
`engage_active` reset to false. Walker proceeds: `pending → planning → navigating` at
goal_id M+1. Observable: `engaging → pending → planning → navigating`, **one** `goal_id`
bump (the goto's), `disengaged: true` in the POST body.

**Scenario 3 — target dies mid-pursuit, no goto.**
`engaging` (goal_id = K). Server marks the target `dead` (or deletes the spawn). Next
`tick`: reconciler `want_engage=false` (`!e.dead` filter, or `entities.get` returns
`None`) → `nav_state_is(NAV_STATE_ENGAGING)` true → `set_nav_state_because("idle",
Some(melee_disengaged))` → `retire_to_idle` → `nav_state: idle`, `nav_reason:
melee_disengaged`, `nav_goal: null`, `nav_goal_id: K` (unchanged). `engage_active`
false. `drive_auto_engage_melee` @1492 also bails (its own `!e.dead` filter), returns
`false`; `resolve_goal` @1501 finds no goto, returns `None`, and its own retirement is
a no-op (`idle` is terminal). One clean write.

**Scenario 4 — bare `/combat/attack off` mid-pursuit, no follow-up move.**
`engaging` → `DELETE /v1/combat/attack` → `drain_combat` sets `auto_attack=false` →
reconciler `want_engage=false` → retire to `idle` / `melee_disengaged`. Identical to
scenario 3's write; different trigger.

**Scenario 5 — `/move/stop` while auto-attacking a live nearby mob.**
`engaging` (goal_id = P). `POST /v1/move/stop` → `request_stop` clears the goto/follow/
zone_cross slots, `stamp_new_goal("idle", stopped, None)`: idle branch,
`nav_state_is_suspended("engaging")` true → published word preserved as
`engaging`/`melee_engaged`, `goal_id → P+1`, returns `"navigation stopped
[goal_id=P+1]"`. Next `tick`: reconciler `want_engage` still true (auto-attack still on,
target still live and near) → `enter_engaging()` re-affirms (idempotent, no write).
Net: `nav_state` stays `engaging` across the `/stop`; `goal_id` bumped once;
pursuit continues. To actually stop: `DELETE /v1/combat/attack` (→ scenario 4).

---

## 6. Testing

~5 forcing tests. Each names the mutation(s) that must turn it **red** — both a
*delete* (remove the line/guard) and a *wrap/flip* (invert the predicate) where the
code is a condition — modelled on `retire_life_halt`'s mutation-check tests
(`action_loop.rs:8217`, `:8288`). Placement: `action_loop.rs` `#[cfg(test)]` for the
reconciler; `nav.rs` tests for the preservation predicate; `move_api.rs` tests for the
handlers.

| # | test | asserts | must go RED when… |
|---|------|---------|-------------------|
| 1 | `engaging_is_published_while_auto_attack_pursues_a_live_nearby_target` | after `auto_attack=true` + a live target at dist 150u, one reconciler pass → `nav_state == "engaging"`, `nav_reason == "melee_engaged"`, `nav_goal == None` | delete the `enter_engaging()` call; **flip** `want_engage`'s `< 200.0` to `> 200.0`; delete the `.filter(|e| !e.dead)` (then a dead target at 150u wrongly engages) |
| 2 | `engaging_retires_to_idle_melee_disengaged_when_the_target_dies` | target flips `dead=true` → next reconciler pass → `nav_state == "idle"`, `nav_reason == "melee_disengaged"`, `nav_goal_id` unchanged from the engage | delete the else-branch retire; delete the `nav_state_is(ENGAGING)` guard (then it stomps an unrelated `navigating` — a second assertion covers that); **wrap** the retire so it also fires when `nav_state` was `navigating` |
| 3 | `entering_melee_supersedes_an_active_goto_exactly_once` | `/goto` (goal_id N) then engage → `goal_id == N+1` and stays N+1 across 5 further reconciler passes; `nav_state == "engaging"` throughout; no observable `"idle"` | delete the `engage_active` latch (then `goal_id` climbs every pass); delete the `has_active_goto()` guard (then it bumps `goal_id` even with no goto); move `enter_engaging()` before the `request_cancel_goto` (then an `idle` is briefly observable — assert via an intermediate snapshot) |
| 4 | `stop_and_wasd_cancel_do_not_flip_engaging_to_idle` (`nav.rs`) | with `nav_state.state = "engaging"`, calling `request_stop()` and `request_cancel_goto()` each leaves `state == "engaging"`, `reason == "melee_engaged"`, bumps `goal_id`, clears `goto_target` | change `nav_state_is_suspended` back to `nav_state_is_life_halt` at `nav.rs:202`; make `nav_state_is_suspended` return `nav_state_is_life_halt(s) && false` |
| 5 | `engaging_is_not_terminal` | `TERMINAL_NAV_STATES` does not contain `"engaging"`; and a functional check: `engaging` + no goto + `resolve_goal` None-branch still lets the reconciler retire it next pass (it is not frozen) | add `"engaging"` to `TERMINAL_NAV_STATES` (both the `[&str; N]` len and the functional assertion fail) |
| 6 | `fresh_goto_while_engaging_disengages_and_echoes_it` (`move_api.rs`) | router-level: `auto_attack=true` + live target, `POST /v1/move/goto` → body has `"disengaged": true` and `"status":"navigating"`; a queued `take_attack()` == `Some(false)`; with `auto_attack=false` the same POST → `"disengaged": false` | delete the `disengage_for_new_move` call in `post_goto`; **flip** its `gs.auto_attack && live_target` to `||`; delete the `"disengaged"` json line |

Baseline to preserve: `rbuild . test --workspace --locked` → **1145 passed / 0 failed /
29 ignored** on `main` [cited, build-run skill]. Run with `timeout: 420000`.

---

## 7. Non-goals

* **The per-frame `goal_id` churn under held WASD** (`app.rs:1899`, unconditional
  `request_cancel_goto` every frame). Pre-existing (#349-adjacent), unrelated to melee,
  and untouched here. §3.5 only stops the *word* from flapping, not the id.
* **The client choosing targets.** `#1109` — targeting is the agent's decision via
  `/v1/combat/target{,/name}`. The reconciler reads `gs.target_id`, never sets it.
* **Passive "swing when in range" auto-attack.** Not how `auto_attack` works here
  (§2); no change proposed.
* **Reconciling `drive_chase` / `drive_teleport_detect` honesty.** Assessed as
  possible "and friends" siblings; not in this scope. Flag to the user if it grows.
* **A `following`-style latched sub-state for "in melee range, holding".**
  `drive_auto_engage_melee` already distinguishes steer-vs-face internally; a second
  word ("engaged" vs "engaging") was considered and dropped as YAGNI — one transient
  word is enough to kill the `idle` lie. Revisit after user feedback.

---

## 8. Open questions / deviations from the approved chat sketch

1. **`enter_engaging` nulls `goal`/`local` (§3.4).** The chat sketch said only
   "`set_nav_state_because(NAV_STATE_ENGAGING, …)`." Source review found that path keeps
   a stale `goal` when entering `engaging` from a terminal `arrived` (goto slot already
   cleared, `NavStatus.goal` not) — the #732 defect class. The helper is the minimal
   fix that keeps `nav_goal` honest (`null`) under `engaging`. **Alternative** if the
   reviewer prefers no new `walker.rs` method: make the reconciler's episode-entry
   `request_cancel_goto()` **unconditional** (not gated on `has_active_goto()`), which
   also clears `goal` via `retire_to_idle` — at the cost of one `goal_id` bump per
   engage episode even when no goto was in flight. That bump is arguably correct #349
   semantics ("a nav-relevant transition an agent watching an old goal_id should see"),
   but it is a new `goal_id` writer not tied to a `/move/*` POST. Recommendation: keep
   the helper.
2. **§3.9 (`auto_attack` on `/observe/debug`)** — include now, or split? Recommendation:
   include (4 lines + a doc line, closes the obvious follow-up question).
3. **`post_zone_cross` disengage disclosure** is an appended sentence on the existing
   `text(…)` body, not a JSON key (that handler returns plain text). Confirm that is
   acceptable vs. converting it to JSON (a larger, riskier change to a well-worn
   handler).
4. **`melee_disengaged` vs. reusing `goal_dropped`.** A distinct reason costs one
   constant and one doc row but lets an agent tell "my melee pursuit ended" from "my
   goto's chase target despawned." Kept distinct. Push back if the vocabulary is
   getting too wide.
