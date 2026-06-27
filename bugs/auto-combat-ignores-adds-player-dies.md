# Auto-combat ignores adds — a second mob beats the player to death unanswered

**Summary:** When a second mob aggros the player while auto-combat is focused on its current
target, the player never retaliates against (or flees from) the add. The add lands hits for tens
of seconds with zero response until the player dies.

**Severity:** High (results in avoidable player death; blocks unattended grinding)

**Zone / area:** Combat / nav auto-engage (`src/eq_net/navigation.rs` auto-combat tick).

## Steps to reproduce
1. Brogan (L1 Human Warrior, ~50 HP) grinding rats in South Qeynos with auto-attack ON
   (`/target/name a_rodent` → `/attack`).
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

## Status
Open
