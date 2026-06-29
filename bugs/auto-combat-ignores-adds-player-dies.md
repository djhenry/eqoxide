# Auto-combat ignores adds — a second mob beats the player to death unanswered

**Summary:** When a second mob aggros the player while auto-combat is focused on its current
target, the player never retaliates against (or flees from) the add. The add lands hits for tens
of seconds with zero response until the player dies.

**Severity:** High (results in avoidable player death; blocks unattended grinding)

**Zone / area:** Combat / nav auto-engage (`src/eq_net/navigation.rs` auto-combat tick).

## Steps to reproduce
1. Brogan (L1 Human Warrior, ~50 HP) grinding rats in South Qeynos with auto-attack ON
   (`/v1/combat/target/name a_rodent` → `/v1/combat/attack`).
2. While auto-combat is killing one rat, a second rodent (`a_rodent009`) aggros and starts
   attacking Brogan.
3. Observe the combat log: the add hits Brogan repeatedly with **no "Brogan hits <add>"** lines.

## Expected
Auto-combat should react to the mob actually attacking the player — switch/add it as a target and
fight back (or disengage/flee at low HP) — so the player isn't beaten to death by an ignored add.

## Actual
For ~40s `a_rodent009` hit Brogan continuously (2–7 dmg each) while Brogan landed 0 hits on it.
Brogan was busy with a different target (`a_sewer_rat000`, slain at 19:21:32) and even after that
kill did not retarget the mob hitting him. Result: `*** You have been slain! ***` (19:21:42).

Combat log excerpt (`/tmp/eqoxide-garrik.log`):
```
19:21:02 a_rodent009 hits Brogan for 2 damage
19:21:18 a_rodent009 hits Brogan for 7 damage
19:21:30 a_rodent009 hits Brogan for 7 damage
19:21:32 a_sewer_rat000 has been slain          <- Brogan's actual target
19:21:36 a_rodent009 hits Brogan for 7 damage
19:21:39 a_rodent009 hits Brogan for 7 damage
19:21:42 *** You have been slain! ***
```
(Note: no `Brogan hits a_rodent009` line appears anywhere in that 40s window.)

## Diagnosis notes
- Auto-retarget (`navigation.rs`) picks the *nearest reachable trash mob*, not the mob that is
  currently attacking the player, so an add that is in melee range but not "nearest/clear-path"
  can be ignored indefinitely.
- There is no threat/aggro feedback: the client doesn't track "who is hitting me" (no HP readout
  either — see combat verification notes in `docs/autonomous-play.md`), so it can't prioritize the
  attacker or bail at low health.
- Possibly related: after the death, the client issued a clean OP_Logout ~90s later
  (`clean shutdown requested`) rather than handling the respawn/bind screen. Unconfirmed whether
  that logout is client-driven (unhandled death screen) or external; recorded here for follow-up.

## Suspected root cause
(unconfirmed) Auto-combat has no "mob attacking me" awareness and retargets purely on
nearest-reachable trash. An add that aggros mid-fight is never engaged, and with no HP/low-health
bail-out the player tanks it to death. Fix: prefer the mob attacking the player when retargeting,
and/or disengage at low HP. (HP awareness is itself missing.)

## Fix (branch worktree-auto-combat-adds)
Confirmed root cause: `apply_combat_damage` parsed the attacker (`source_id`) but discarded it, and
the nav auto-target step (navigation.rs) only retargeted when the current target died, then to the
nearest trash mob — so an add hitting the player was never engaged.
- `game_state.rs`: added `recent_attackers: HashMap<spawn_id, Instant>`.
- `packet_handler.rs`: `apply_combat_damage` records each NPC that swings at the player (hit OR miss).
- `navigation.rs`: new pure `pick_combat_target()` — priority: a mob currently attacking the player
  > a still-valid current target > nearest reachable trash; keeps the current target when it is
  itself an attacker (so two adds don't thrash). Attackers age out after 6s. 4 unit tests; 288 pass.

Live (Campy, qeynos): confirmed the player auto-engages and fights back against the mob attacking
her. A clean multi-add SWITCH could not be staged live (L1 char does ~0 dmg, can't survive, only one
mob aggroed, navpath stalls) — the switch is covered by unit tests. Merged as-is per maintainer.

Out of scope (follow-ups): low-HP disengage/flee; player melee showing "-5 damage"; `/v1/observe/debug`
target_id not synced from the nav thread to the render thread (shows None during combat).

## Status
Fixed (retarget-to-attacker; low-HP flee deferred)
