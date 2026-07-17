# TODO

Active work + bite-sized tasks for smaller agents to continue if the main session stops.
Keep this updated as tasks complete. (Older entries below are an ARCHIVE — see git history.)

---

# CURRENT STATE (2026-07-14)

`main` is green. **CI enforces** `cargo build --release` + `cargo test --lib` on every PR
(`.github/workflows/test.yml`).

**Do not trust any count or sha in this file — RUN these.** Every hardcoded figure here has gone stale
at least once, including during the review of the commit that corrected it (a reviewer's own corrected
test count was wrong by the time it landed, because a PR merged mid-review):
```bash
git rev-parse --short origin/main                                     # current main
cargo test --lib -- --list | tail -1                                  # real test count
gh issue list -R djhenry/eqoxide --label agent-honesty --state open   # the live agent-honesty list
gh issue view <N> -R djhenry/eqoxide --json state --jq .state         # is it ACTUALLY still open?
```
At the time of writing: `b66eb69`, **722 tests**, **14** open `agent-honesty` issues
(#344 #347 #349 #355 #356 #360 #361 #366 #370 #371 #378 #386 #390 #391) — *all four figures produced by
running the commands above, not by typing them.* **They are already decaying. Re-run them.**

## ⚠️ Read this FIRST: how the last handoff lied, so you don't repeat it

The previous version of this file called #378 **"the owner's design."** It was not. The owner had
said, verbatim:

> *"**Could we** refactor this to be more intuitive? In OOP, I **would possibly** build a boundary
> sweeper into which I could pass different kinds of detectors... (**might optionally** add MOB
> avoidance...)"*

A question with three hedges became, in three hops — an issue, then this file, then a session
handoff prompt — **"the owner's design," priority #2, with a trait spec and acceptance criteria.**
An agent then wrote a 785-line design doc against it. Nobody lied on purpose. Each hop just dropped
one hedge.

**The rule this produces, and it binds THIS FILE hardest:** when you record what the owner wants,
**quote them or mark it as your inference.** Never launder a suggestion into a requirement. An agent
reading this file has no independent channel to reality and cannot detect the promotion. *Precision
without provenance reads as authority and is worse than no number at all.*

The same disease produced a **fabricated conflict report** (an agent read a two-dot `git diff` as
"PR #372 deletes nav_planner.rs" when the branch merely predated the file) and, in PR #372 itself,
**four headline numbers that did not survive review** (see below). All three were caught by an
independent reader. None would have been caught by the author.

## Read this NEXT: the `agent-honesty` label

It is the project's top prioritization principle. **Get the live count with the command above** —
any number written here is already wrong.

> **The client must never lie to the agent.** Every API response and every observable field must be
> either TRUE, or an EXPLICIT failure the caller can distinguish. Never a confident falsehood.
>
> An AI agent has no independent channel to reality — whatever the client reports IS its world. A
> crash is *honest*. A silent wrong answer is undetectable, unrecoverable, and poisons every
> decision downstream. **Silent-wrong-answer bugs outrank features AND crashes.**

Two hard-won process rules:
- **Verification hierarchy:** make the bad state *unrepresentable* > property-test the universals >
  example-test + **mutation-check it** (revert the fix → confirm RED) > live-run to validate the
  model's premises. **Never let a passing live run discharge a claim containing "never" / "always" /
  "cannot"** — a race that usually wins is indistinguishable from one that cannot lose. (A `/follow`
  deadlock passed live verification *by luck*; a pure-function test caught it.)
- **Every PR gets an independent reviewer agent before merge.** That gate found a real, shipping
  defect in *every* PR it examined — including three where the *fix* reintroduced the bug it fixed.

## NEXT UP

**THE NAVMESH IS DEAD. THE GRID IS BANKED.** Owner's decision, 2026-07-14, on the evidence below.
Do not restart it without reading the review on #372 first.

1. **#386 — planner probes to 3.0u, the controller collides to 4.0u.** Head-height geometry (an
   overhead beam, a low arch, a chest-high railing) is **clear to A\* and solid to the walker** — the
   planner hands the walker routes it physically cannot follow. This is #358's signature in the
   **height axis**, in the fatal orientation: the planner is the *permissive* one. Constants verified:
   planner probes at `cz+2.5` (`FEET_CLR`, `assets.rs:2144`) and `cz+3.0` (`CHEST`, `assets.rs:1984`);
   controller at `foot+0.5` / `foot+4.0` (`movement.rs:350-351`); the cylinder is ~6u tall.
   ⚠️ `path_clear`'s doc (`assets.rs:1440-1445`) claims the planner and controller "**cannot drift
   apart**" because they share `Collision::sweep` — but **`sweep` has ZERO production callers**
   (`grep -rn '\.sweep('` → two unit tests). That false comment is *why* later wall/corner reports got
   matched to #358 and deprioritised. **Do not just bump CHEST 3.0→4.0** — every prior tightening of
   planner walkability has SEALED ZONES (the coarse capsule sweep cost **−29% route success in
   akanon**, `assets.rs:1413-1415`). Measure route parity on the corpus before/after.

2. **#394 (was RED on main) / #382 — the planner's wall-clock budgets.** #377 moved the **coarse**
   plan to a worker (net-thread stall **1.6s → 4µs**) but did NOT delete its budget — it RAISED it
   150ms → 5000ms (`WORKER_PLAN_BUDGET_MS`). The earlier claim here ("budget deleted") was **false**;
   a 5s wall clock still made the coarse answer machine-speed-dependent, and a loaded CI runner flipped
   a genuinely-unreachable goal from `Unreachable(SearchClosed)` to `Exhausted(Deadline)` — which is
   why `main` was intermittently RED. **#394 replaces BOTH tiers' wall clocks with a deterministic
   NODE CAP** (`PlanLimit::Deadline` and the `deadline` field deleted; `MAX_NODES = 8M`, chosen so
   everfrost's 1.12M-node whole-zone close still reaches `SearchClosed`). **#382 then moves the fine
   (2u) plan off the net thread** — the navmesh is cancelled, so option (2) there is dead; the answer
   is option (1), extend `nav_planner`.

3. **#379 / #381 — the nav-drift family** (plus **#358**, already CLOSED in `8a7bd0b` — context only,
   do not go looking for it). Coarse commits to corridors the fine tier can't fit (no feedback channel,
   so it re-proposes forever); the clearance sweep is blind to a wall the segment runs PARALLEL to.
   **Same generator as #386: the planner and the walker hold different beliefs about what is solid.**
   Treat these as ONE thread, not N parallel fixers — four independent fixers would produce four more
   mutually-blind patches, which is exactly how this family got to four members.

4. **#380 — the client can exit silently. DONE, MERGED (PR #387, on `main`).** The client now records
   why it died: a panic hook (with thread name), hand-installed `sigaction` handlers for
   SIGSEGV/SIGBUS/SIGILL/SIGABRT/SIGFPE with `SA_ONSTACK` + chaining, a labelled clean-shutdown marker,
   and per-pid durable logs. **Root cause of the original incident remains UNKNOWN** — it was never
   reproduced; what is fixed is that the *next* one will be self-diagnosing. It spawned three follow-ups:
   **#390** (unbounded crash-log FILE COUNT — one pair per launch, forever, even for `--help`),
   **#391** (pid reuse merges two runs into one log), **#392** (a crash before HTTP bind is anonymous).
   ⚠️ **Keep the lesson, it is the most expensive one this project has bought twice:** PR #387's first
   revision **shipped a client that could not start — with green CI** — because *no test ever called
   `install_signal_handlers()`*. Its "obvious" fix would then have **deleted** the loud
   `has overflowed its stack` message std already prints, manufacturing new silent deaths inside the PR
   meant to end them. **A green suite over an unexercised code path is not evidence.**

5. **#378 — traversability abstraction. STATUS: PROPOSAL, NOT SCHEDULED.** See the warning at the top
   of this file — this was walked back from "the owner's design" to what it actually was: a hedged
   suggestion. The *problem* it names is real and now has **four** confirmed children (#358, #379,
   #381, #386). The *solution* in the issue is one agent's sketch. PR #385 is a design sketch, not an
   approved plan. **Design it fresh if you pick it up; do not cite #378 as a requirement.**
   The one part that IS genuinely the owner's, and stands: **tiered clearance** — walk with a
   larger-than-player margin normally, fall back to exactly `PLAYER_RADIUS` only when no generous route
   exists, and **NEVER below `PLAYER_RADIUS`** (#310 removed a sub-radius fallback that planned routes
   the character could not fit through — do not re-introduce it).

## What happened to the navmesh (PR #372) — do not re-litigate without reading this

Reviewed adversarially; **Phase 2 CANCELLED**. Four headline numbers **did not survive**:
- *"Navmesh finds 462 routes (27%) the grid cannot"* → **those "wins" are overwhelmingly grid TIMEOUTS,
  not grid failures.** The harness bucketed a 150ms `PLAN_BUDGET` timeout as "no route" — **the #337
  anti-pattern, inside the tool built to evaluate nav.** (Careful with the numbers: the *462* is from the
  PR's original 1700-pair/34-zone self-sampled run. The review's **separate** re-instrumented run — 2200
  pairs, 11 zones — found **567** navmesh-only routes, of which **548 (97%) were grid timeouts** and only
  19 were genuine grid failures, with **50.4%** of all grid queries saturating the budget. The 97% is NOT
  a breakdown of the 462; the two runs were never reconciled. The conclusion is solid; the composition
  was sloppy, and this file said so wrong the first time.)
- *"Query 1.5–2.8ms vs the grid's ~150ms" (~60×)* → **150ms is the grid's CAP, not its cost.** The grid's
  median *is* the budget, exactly. A censored distribution; the speedup is undefined. Where the grid does
  not saturate (qcat): grid 7.3ms vs navmesh 6.2ms — **comparable**.
- *"Adjusted parity 99.18%"* → **the failing gate redefined to pass.** The "unwalkable" grid routes were
  scored by absolute Δz vs `STEP_UP`, but `STEP_UP` is a *step height, not a slope limit*
  (`MAX_WALK_GRADE = 1.2`). 125 of 238 were ordinary walkable **ramps**. The **"max 773u climb" does not
  exist** — it is the final waypoint's goal-snap (`assets.rs:2477-2480`, verified;
  the #372 review cited `:1885`, which is the WATER-anchoring block — a wrong citation this file
  retyped without checking, and its own reviewer caught. Cite by reading, not by copying.).
- *Raw parity 93.88%* → pairs were sampled from **the navmesh's own domain**, so ground it dropped could
  never be sampled. Resampled neutrally from the EQEmu oracle: **83.5%**, and grid-only routes **+53%**.

And the fix at its heart was **unsound**: `|nz|` slope classification (`navmesh.rs:379-380`) discards the
sign, so **a flat ceiling (`|nz| = 1.0`) classifies as walkable floor** — #329 reproduced verbatim. Its
two defences fail on a real EQ ceiling, which is a thin shell with **open air above it**. qcat survived
only because it happens to have rock above its ceiling. **#375 is CLOSED as unsafe** for the same reason:
porting `|nz|` into the shipping `nearest_floor` would re-open #329 in every zone nobody tested.
Its worst-case query is **455ms, unbounded** — *worse* on the net thread than the grid's bounded 150ms.

**What was real and survives:** bake ~0.4–0.9s median / 5.6s max (everfrost); ~800KB/zone (the honest
extrapolation is **~450MB over the real 497-zone universe**, not the PR's "180MB for ~200 zones"); the
water-surface layer works; and **EQ face winding genuinely is unreliable for outdoor terrain** — that
observation is correct even though `|nz|` is not the fix. **My inference, not a decided plan:** a real
fix likely belongs in the **asset pipeline** (`eqoxide_asset_server`, the producer), since the art
itself is inverted and every downstream consumer inherits it. Nobody has decided this — re-derive it.

## Asset bugs — NOT client bugs; do not try to fix them in the client
- **#373** — `nektulos`/`arena` GLBs are **missing their terrain**: 95%/98% of EQEmu-walkable ground
  has *no collision geometry at all*. **No client-side mitigation exists.** Almost certainly the hole
  the **#150 underworld fall-guard** masks.
- **highpass inverted winding** (was #375, now closed as unsafe): whatever the fix is, it is **NOT**
  `|nz|` — discarding the normal's sign admits ceilings as floors (#329). #375's closure lists **two**
  candidate directions and settles neither: (a) a clearance/headroom or anchoring discriminator that
  does not depend on the winding sign, or (b) repair the winding in `eqoxide_asset_server` (the
  producer). (b) is the one I'd favour — **but nobody has decided, so re-derive it, don't cite this.**

## Backlog (issues carry full repros + measurements)

- **agent-honesty (14 as of writing — RUN `gh issue list --label agent-honesty --state open`;
  do not trust this enumeration to stay current):** #347 (structural — "200 = queued" is systematically
  dishonest; single-slot mailboxes silently overwrite), #371 (`connected:true` means the SOCKET is
  alive, not the WORLD — a wedged zone keeps ACKing; needs an **active probe**), #361, #366, #370,
  #360, #349, #344, #386, #390, #391 — plus #378, #355, #356, covered above.
  (**#363 and #380 are CLOSED** — they were listed here as open; do not chase them.)
- **Nav:** #313 (grade capped on ascent only), #309 (no climb mechanic), #240 (moving platforms),
  #329 (qcat spawn pocket — the planner is correct now; the **controller** can't execute),
  #266 (a related but DISTINCT wedge — qeynos2 guild vault, portal/zone-transition routing; reopened
  pending live re-test. **Do not assume the #329 diagnosis carries over.**)
  ⚠️ **Do NOT loosen `MAX_WALK_GRADE`** (1.2, `src/assets.rs:1933`) without re-measuring qcat. It is
  what stops A* routing the character up into solid rock — the grade cap exists to prevent the
  slope-wedge class (#205/#212), and #353's review kept it deliberately untouched. The margin was
  reported as narrow in-session, but **that figure is recorded in no issue** — re-derive it before
  you touch the constant, don't trust a remembered number.
- **Test suite:** #355 (**4** surviving mutants — 2 in `packet_handler.rs`, 1 in `http/quests.rs`,
  1 in `http/group.rs`), #356 (flaky wall-clock A* test — fix by surfacing `PlanOutcome`, **not** by
  `#[ignore]`), #357 (2 ignored tests actually FAIL — character model reports **2× expected height**,
  12.57 vs 6.00; real bug or stale fixture?).
- **Features:** #226 (audio), #225 (split zone GLB), #194 (boats), #256 (item links).

## Ops notes

- **Remote build box:** `rbuild <worktree-dir> test --lib` (6-core/23GB, sccache+mold, idle). The local
  box is 8-core/15GB and shared — a cold `--release` build there will stall you.
- **NEVER `git stash`** in this repo — it is a **repo-global ref** and worktrees share it; it silently
  destroyed an agent's work. Commit instead.
- **One game account + one API port per agent.** EQ evicts the older session; sharing an account
  corrupts other agents' live tests.
- `pgrep -f 'eqoxide'` **matches your own command line.** It produced false "still running" reports
  three separate times. Check the real process tree.

---

# ARCHIVE (older sessions — kept for context; see git history)

## DONE: UI overhaul (#162, PR #170) — merged long ago
Registry-driven window system in src/ui/ (docs/ui-overhaul-design.md + ui-window-management.md):
21 windows, RoF2 theme from the real TGAs, per-char persistence v2, Window Selector, UI scaling,
pet commands, /v1/trainer/close.

## DONE: client no longer reads ~/eq_assets at runtime (all assets via the asset server)

Everything the client loads now comes from the asset-server cache, not ~/eq_assets:
- text game data → "gamedata" set (eqstr/spells/maps + water .wtr)
- character models + zone terrain → "common" + "zone/*" sets
- worn-armor textures + held-weapon S3Ds → "gameequip" set (build_gameequip_from_raw)
- clickable-door object models → per-zone "zonedoors/<short>" set (build_zonedoors_from_raw)
renderer.rs/app.rs repointed to the cache; App.assets_path is now vestigial (stored, never read for
files — could be removed from config/App::new in a cleanup). Loose ends:
- zonedoors published only for the IN-USE zones (qcat, qeynos, qeynos2, qeytoqrg, qrg). Other zones'
  doors fall back to plain boxes until a full `eqoxide-assets build --raw` (which now publishes
  zonedoors for every zone). Run a full bake to cover all zones.
- gameequip is served RAW S3D (client still parses S3D from the cache). A future size optimization
  could serve DERIVED armor textures (PNG) + per-weapon GLBs instead.
- Weapon-model load from the cache verified by inspection (self.assets_path -> cache); not yet seen
  live in combat (equip-texture index + door models WERE verified live: 4045 textures, 16 qcat doors).

## TODO: exhaustive fall-damage testing (controlled-fall nav)

Controlled-fall navigation + native fall rate (app.rs gravity/terminal 120/128) + client-side fall
damage (OP_EnvDamage 0x31b3, `fall_damage(height)` in navigation.rs) + a lethal-fall guard landed
on `main` (was branch worktree-necro-combat). Verified offline (qcat dry sewer routes 56–88 wp) +
unit tests, but NOT exhaustively live. Still to do:
- Live-test an actual controlled-fall descent end-to-end with a character that can SURVIVE the drop
  (Keebler L1/20hp's guard correctly refuses the ~42u qcat drop, max ~364 dmg — need a mid-level
  char or a shorter drop). Confirm: server accepts the descending position updates (no rubber-band),
  the bot lands in the qcat lower sewer, OP_EnvDamage is accepted and HP drops by the sent amount.
- Validate the fall-damage curve vs the real client across drop heights (10/20/30/42/60u); tune
  GRAVITY/HZ in `fall_damage` if magnitudes diverge from native.
- Check water/levitate negation: a fall ending in water (or with Levitate) should take 0 — confirm
  we don't send OP_EnvDamage in those cases (currently we always send on a dry controlled-fall).
- Decide whether WASD (human) ledge-falls should also send OP_EnvDamage (currently only the nav
  controlled-fall path does).
See ~/git/eq_kb/falling-physics.md.

## COMPLETE: Clickable & animated doors + agent API (branch worktree-zone-portal-objects)

Parse OP_SpawnDoor on zone-in; render with real models (from `_obj.s3d`) + fallback box.
Animate open/close via OP_MoveDoor (server-authoritative, no client-side toggle). Portal doors
(opentype 57/58) zone the player on open. Click via 3D picker or HTTP API (`GET /v1/observe/doors`,
`POST /v1/interact/click_door {door_id:N}` or `{name:"DOOR1"}`). Verified live: doors render (untextured),
click opens (server replies), portal doors trigger zoning. Notes: `docs/http-api.md`, `~/git/eq_kb/doors.md`.

**Follow-ups (not blocking):**
- Door textures (models currently untextured; geometry/placement correct)
- Hinge-axis / swing-direction per model (best-effort heuristic, may be mirrored)
- Slide axis / distance per opentype (approximate; needs per-opentype calibration)
- Same-zone teleport (portals currently force full zone change; optimize to skip re-login)
- Locked-door key / lockpick support (server-side logic exists; client side optional for agents)

## COMPLETE: Player action grid + spell casting + APIs (branch worktree-action-grid-spellcasting)

Spec: `docs/superpowers/specs/2026-06-23-action-grid-spellcasting-design.md`
Plan: `docs/superpowers/plans/2026-06-23-action-grid-spellcasting.md`

HUD action grid (bottom-center): auto-attack TOGGLE, sit/stand toggle, target/consider, and
the 9 memorized spell gems with real TGA icons + a cast bar. Real spell casting over the wire
(`OP_CastSpell`) with `OP_BeginCast`/`OP_ManaChange`/`OP_MemorizeSpell`/`OP_InterruptCast`
feedback. HTTP APIs mirror every action: `GET /v1/observe/spells`, `POST /v1/combat/cast`, `POST /v1/interact/sit`,
`POST /v1/interact/stand`, `POST /v1/combat/consider` (+ existing `/v1/combat/attack` toggle). Profile parsing reads
`mem_spells[9]` @4360; `spells_us.txt` → gem names/icons. **Verified live** with a test Cleric
("Clrtest", Human Cleric L1, acct `REDACTED`/`REDACTED`, `config-cleric.yaml`): `/v1/observe/spells` lists
the 5 memmed gems, `/v1/combat/cast {gem:0}` fires `OP_CastSpell` (Minor Healing), sit/stand + attack
toggle all confirmed via nav log + `/v1/observe/frame` (gem icons render).

Follow-ups (not blocking):
- **Mana % shows 0** — `apply_mana_change` is a no-op; `mana_pct` isn't populated for the
  player. Wire mana% (needs max-mana from profile) so the HUD mana bar + cast gating work.
- **No `POST /v1/combat/target` "nearest"** — HTTP `/v1/combat/target` needs an explicit `{id}`; the HUD Target
  button uses nearest-NPC. Consider a `/v1/combat/target/nearest` convenience for agents.
- Spell **icon grid geometry** is `ICON_COLS/ROWS = 6` in `src/spells.rs` — looked correct
  live; revisit if any icon looks sliced.
- Out of scope (future): disciplines, clicky items, AA abilities, spellbook→gem memorize UI.

## Current feature: Gender character models + conversion normalization

Plan (full detail, exact code/commands): `docs/superpowers/plans/2026-06-19-gender-models-and-normalization.md`
Spec: `docs/superpowers/specs/2026-06-19-gender-models-and-normalization-design.md`
Branch: `feat/equipment-textures`

Goal: render correct race + gender models (human/wood-elf/dwarf, male/female) at the right
size and position. Root cause found: `humanoid.glb` was built from `globalhom` (Halfling), not
`globalhum` (Human); models also came out off-center/over-scaled from raw conversion.

Key facts for any agent picking this up:
- Correct converter binary: `./target/release/s3d_to_gltf` (NOT `tools/target/release/`, stale).
- `assets/models/*.glb` are gitignored — regeneration is local; commit scripts/code, not `.glb`.
- gender: 0=male, 1=female; non-1 → male. File naming: `<arch>.glb` male, `<arch>_f.glb` female.
- Conversion normalization = offset ROOT bone (index 0) in rest + every anim keyframe, in
  EQ-native Z-up space, KEEP inverse-bind from original bind (else offset cancels). Scale at
  render via `target_height/eq_height`. Full method in plan Task 1 + spec Section A.
- If conversion-time normalization misbehaves, fall back to load/render centering (spec §A fallback).

### Tasks
- [x] T1 Converter translation normalization — DONE then REVERTED (conversion-time broke animations; see below)
- [x] T2 `tools/regen_models.sh` + regenerate male+female glbs — DONE (d4eeca1; models now un-normalized)
- [x] T3 Loader reads `eq_height` → `true_height` — DONE (ab553ab)
- [x] T4 Target-height scaling — DONE (a247635), humanoid target=12 calibrated (06830a6)
- [x] POSITIONING FIX (load/render approach) — DONE (4150e7e). bind_pose()=real rest skinning;
      converter root-offset reverted; recenter from measured posed bounds every frame.
      Verified by `humanoid_player_transform_grounds_and_centers` test (feet grounded, centered, h=12)
      + /v1/observe/frame visual. NORMALIZATION IS NOW LOAD/RENDER-TIME, not conversion-time.
- [x] BOB FIX — DONE (cd11634): constant bind-pose grounding (no walk bob).
- [x] T5 Plumb gender — DONE (0ca5570): Billboard.gender, Scene/GameState.player_gender.
- [x] T6 Gender-keyed model storage + model_for(archetype,gender) selection — DONE (02f70ea).
      Both genders confirmed loading in log (humanoid/elf/dwarf gender 0 + 1).
- [x] T7 Target-height consistency — DONE (5784fb0): human-height races=12, dwarf=9, monsters
      proportional. STILL NEEDS the user's visual tuning for monster heights + female look.
- [x] T8 `docs/character-models.md` — DONE.

## OVERNIGHT LOOP ACTIVE (player position offset) — CORE FIX DONE (a5b75db)
Player position offset FIXED: per-clip posed bounds (center+feet) drive recenter+grounding
from the current animation clip (bind pose differed from the live idle pose). Verified via
test `humanoid_idle_pose_grounds_and_centers` + /v1/observe/frame (player now in the doorway).
Loop continues to verify NPCs/walking/female-elf-dwarf placement + tune monster heights.
Notes: `.superpowers/sdd/overnight-notes.md`.

UPDATE: positioning fix verified to GENERALIZE to all 6 gendered models (humanoid/elf/dwarf
M+F) via `gendered_models_idle_ground_and_center` (all ground+center, correct prefixes).
Tests added for all fixes (equip_swap_key/material-0, bind_pose, race-model, slot-cap,
clip_bounds, idle placement). Loop now LONG-cadence: live client was disconnected
(zone empty, 0 entities) so /v1/observe/frame NPC verification + monster-height visual tuning are
PENDING the client reconnecting or the user. Core work is done + committed + tested.

## OVERNIGHT LOOP CONCLUDED (objective complete)
Player position offset FIXED + verified (live in doorway + deterministic across all 6
gendered models) + test-covered + documented. Loop self-terminated after 3 idle iterations:
character parked at a doorway with no NPCs in view (API: 0 entities) and no further
autonomous work possible without NPCs + user visual judgment. Restart a loop (ideally with
the character near NPCs) to resume the optional items below.

## NEW UNATTENDED LOOP: zone object placement + coordinate bug
Objects piled at 0,0,0 (bug #1: assets.rs::load_all ignores _obj placement fragments) +
NPCs appear in water (likely symptom of missing buildings). Fix path found (libeq doc.objects()).
ZONE BUG SOLVED (57a5274 + docs): objects placed via ActorInstance placements (qeynos 476/477,
qeynos2 478/481); buildings render, NPCs among them (numeric: city NPCs at correct coords z~3.8,
only aquatic creatures underwater). All 3 symptoms (0,0,0 pile / NPCs in water / qeynos2 offset)
resolved + verified via /v1/observe/frame + /v1/observe/entities. Docs: docs/zone-rendering.md. Minor follow-ups:
few unmatched placements; eyeball object rotation. Notes: `.superpowers/sdd/zone-coords-notes.md`.
buildings appear + NPCs among them. NEXT: confirm NPCs-in-water resolved vs maps; fix qeynos2
(North) terrain offset (lands outside playable area). Plan/findings: `.superpowers/sdd/zone-coords-notes.md`.

## NEEDS USER VISUAL CONFIRMATION (when back)
- Player + NPCs: correct race (human, not halfling), gender, size, grounded, centered, no walk bob.
- Female NPCs render the female model; elf/dwarf at sensible heights vs humans.
- Monster target heights (archetype_target_height in models.rs) are proportional guesses — tune.

## Minor deferred (final-review triage)
- gltf "extras" cargo feature is in [dependencies] (test-only use) — could move to [dev-dependencies].
- T1's bbox dup + shallow converter test (eq_height kept; offset reverted).

## Done (recent)
- Equipment texture rendering (material-0→baked skin, player slot cap 16→32, spawn tint BGR→RGB,
  player WearChange, animation-state feature) — committed through `2fbe2a0`.
- Diagnosed + fixed wrong base model: `humanoid.glb` regenerated from `globalhum` (human). The
  scale/position calibration for it is folded into T4 above.

## KNOWN BUG: zone is mirrored (left-right) — fix later
The whole scene renders left-right mirrored vs the real client (confirmed: Qeynos clock
tower door is on the RIGHT in our render, LEFT in real EQ). Cause: maps are +X=WEST/+Y=NORTH
but we render render.X=server_x with a right-handed camera (+X=east/right), flipping E-W.
Pre-existing handedness/sign issue, only visible now that the city is placed correctly
(the axis swap in 2168aa7 was a det+1 rotation, not the cause). Fix: negate the server_x/west
axis everywhere it's placed (terrain verts, bounds_xy, collision, NPC+player pos) AND flip
heading sign; verify the clock-tower door flips to the LEFT. Touches coord pipeline + heading
+ minimap + A/D strafe. Confirm with a 2nd landmark before/after.

## COMPLETE: HUD window management (movable, resizable, persistent per-character)

All HUD windows are now draggable/resizable, with layouts persisted to `ui_layout_<CharacterName>.json`.
Window lock state (`Ctrl+L`) freezes positions. Context menu per window (opacity, reset, lock toggle).
UI menu (gear icon) for global lock/reset. Non-resizable windows support move only. Docs:
`docs/ui-window-management.md`.

## NEXT: held items + head/hair (weapons, shields, helms, hair) — NEW SUBSYSTEM
Status after the equipment-texture work (chest now armored, committed f59a76c): the
remaining "missing gear" items are NOT texture swaps — they need attached MODELS / appearance
features. No infra exists yet (helm/showhelm fields are parsed but unused).

Findings / scope:
- WEAPONS + SHIELDS: player_equipment[7]/[8] are item model IDs (e.g. 175,202 → IT175/IT202).
  Item models live in gequip*.s3d (DmSpriteDef meshes named "IT###"). Plan:
  1. Loader: reuse libeq_wld::load + .meshes() (as assets.rs does) to load an IT### mesh from
     the right gequip file (need a name→file index; scan all gequip*.s3d for "IT###_DMSPRITEDEF").
  2. Skeleton: expose joint NAMES from the glTF (anim.rs/models.rs currently don't keep names).
     Find the right-hand attach bone (EQ uses a "r_point"/hand joint) + left-hand for shields.
  3. Render: new pass (or extend player/skinned pass) drawing the item mesh at
     model_matrix * joint_world[hand_bone]. Static mesh, no skinning. Apply per-frame anim bone.
  4. NPCs: same, from entity.equipment[7]/[8].
- HEAD ARMOR (helm): material-3 helm textures (humhe03xx) don't exist; helms are typically
  separate models selected by spawn.helm (+showhelm). Likely another attached-model job (or a
  head-texture swap for materials that DO ship humhe textures). Investigate spawn.helm usage.
- HAIR: RESOLVED (2026-07-01, character-hair-fix): RoF2 S3D races ship NO hairstyle
  geometry (dead actor-attach path); hair = painted scalp texels in the FACE textures
  (hesk{F}{L}, F = face 0-7, NOT hairstyle) × runtime haircolor tint. Converter splits
  scalp (head-bone tris, eq_head_part:"hair", tinted) from facial skin (eq_face only);
  client selects by spawn.face + tints by spawn.haircolor. See
  ~/git/eq_kb/luclin-head-faces-and-hair.md.
- MINOR BUG noticed: when an equip texture falls back to baked (no armor texture for that
  material), the per-mesh TINT is still applied — a missing-helm head can get tinted (e.g. green
  face). Consider skipping tint when the texture falls back to baked skin.

Verify everything from MULTIPLE camera angles (perspective can hide holes/mis-placement).

### BLOCKER for weapon/shield/helm attachment: glTF has no joint names
The converted models' skeleton joints are all unnamed ("?" via gltf skin.joints().name()), so
the hand attach-bone can't be found by name. PREREQUISITE before attachment work:
- Fix the converter (s3d_to_gltf) to write EQ bone names into the glTF node names, then regen
  models (tools/regen_models.sh). EQ right-hand attach point is typically a joint like
  "r_point"/"RIGHT_*"; left hand for shields. Then anim.rs/models.rs must keep joint names so
  the render pass can look up the hand bone index.
Item model locations confirmed: IT202 → gequip.s3d/gequip.wld, IT175 → gequip2.s3d/gequip2.wld
(meshes named "IT###_DMSPRITEDEF"; load via libeq_wld like zone meshes). Build a name→gequip
index by scanning all gequip*.s3d once at startup.

## AUTONOMOUS LOOP (in progress)
Directive: fix the discussed bugs; when out, play via HTTP API across zones to find+fix problems.
Verify everything via /v1/observe/frame from MULTIPLE angles. Commit each fix. Pace conservatively (can't
check credit balance; long session) — stop if credits look low.
Done this loop: mirror (5506701).
Queue (tractable first):
- [ ] Mirror control follow-up: negate mouse-look-X (apply_orbit_delta daz) + A/D turn/strafe so
      manual controls match the un-mirrored display. NEEDS interactive test (can't verify via /v1/observe/frame).
- [x] Monster sizing FIXED (7f619d1): scale by idle-pose extent, not eq_height/bind extent.
      rat was already fine; bat/snake/wolf/gnoll were mis-sized. Humanoid 10.3→12 (intended) —
      user: confirm player height still looks right vs doorways.
- [x] Hair: FIXED (character-hair-fix worktrees, both repos). It was never a missing head-piece:
      hesk digit = FACE index; hair = painted scalp × haircolor tint; hairstyle is visually inert
      on RoF2 S3D races (authentic). See the HAIR note above + luclin-head-faces-and-hair.md.
- [ ] Head armor (helm) + weapons/shields: BLOCKED on converter joint-names (see blocker above) —
      needs s3d_to_gltf to emit bone names + regen, then a bone-attachment render path.
- [ ] tint-on-baked: low priority (not visibly manifesting; head shows normal skin).
- [ ] Play-to-find: walk/zone around, check NPC models/textures/animation/clipping for new bugs.

## FOUND (loop, play-to-find): outdoor zones — player/NPCs float ~250 above? below the terrain
qeytoqrg: DB safe point (83,508,0) = where the player lands, but our terrain renders ~250 units
higher in UP at that (x,y) — so the player (z≈0.75) floats far below the ground, and a top-down
shows only sky near spawn. Terrain DID load (1411 meshes) and its XY bounds CONTAIN the player
(render X[-3698..1251] Y[-1000..5254] up[-88..516], centroid ≈ entity median), so it's an UP-axis
(Z) offset specific to this zone, not an XY/placement bug. City zones (qeynos/qeynos2) ground
correctly, so this is likely an OUTDOOR-zone terrain-structure difference (3 wlds; possibly a
region/BSP mesh with a large center[1] height offset, or a per-zone Z reference). Needs
investigation into the qeytoqrg .wld terrain up coordinates vs server z. NOT a quick fix.

## LOOP WIND-DOWN
Fixed (tractable): mirror (5506701), monster sizing (7f619d1). Documented as big/deep (not safe
to start unattended): hair (separate head-piece feature), head-armor/weapons/shields (converter
joint-names prereq + bone-attachment subsystem), outdoor-zone terrain Z offset (above), mirror
manual-control sign follow-up (needs interactive test). Stopped the loop here rather than dig into
deep subsystems unattended on a long session with unknown credit balance.

## NEW AUTONOMOUS LOOP: play as "Claude" (started 2026-06-20, Max plan — run continuously)
Account (non-GM): login user `claude` / pass `REDACTED` (login_accounts password set to SHA512
of the pw so the loginserver's mode-fallback verify accepts it — mode-14 SCrypt verify is broken
without ENABLE_SECURITY). World account id=3 status=0. Config: per-character login config (~/.config/eqoxide/).
Character: **Claude** = Female Wood Elf Ranger (race 4, class 4, gender 1), char id 2, in qeynos
(zone 1) — chosen non-male-human-warrior; female-elf model renders correctly (verified /v1/observe/frame).
Restart client after config/DB changes via: `touch src/main.rs && cargo build --release` (dev-run.sh
respawns). HP is NOT in /v1/observe/debug — judge survival via /v1/observe/frame HUD + combat log.

Play tools (HTTP :8765): /v1/navigate/warp {x,y,z} (teleport), /v1/navigate/goto {x,y,z} (walk+face), /v1/combat/target/name {name},
/v1/combat/attack (POST on / DELETE off), /v1/interact/say (#cmds are GM-only — Claude can't #zone), /v1/observe/zone_points,
/v1/navigate/zone_cross, /v1/interact/hail, /v1/observe/frame, /v1/observe/entities, /v1/observe/debug.

Goals: win a fight, buy equipment, travel between zones, level up. Fix bugs found. Verify visually.

Progress/findings:
- Combat engages after /v1/navigate/goto (need range + facing; warp alone didn't face the target).
- Claude DIED to a_rodent013: naked level-1, low HP, dealt NO damage back (no weapon — DB chars skip
  the newbie starting gear normal char-create grants). NEXT: give Claude legit newbie gear (a weapon
  in worn slot 13 + basic armor via `inventory` table) so she can deal damage and survive, then win.
- BUG candidate: "buy equipment" — no merchant/buy HTTP endpoint exists; may need to add one
  (open merchant window / OP_ShopRequest + buy) to satisfy the buy goal.

## Claude play loop — iteration findings (positioning + combat)
GEARED: Claude now has a weapon (inventory slot 13 = item 5019 Rusty Long Sword), level 5,
51 skills, bind+pos = qeynos (0,10). (Stats/HP via character_data.)

CLEAN-RESET PROCEDURE (DB position edits get clobbered by the server's live/logout-save while
the client runs, AND dev-run.sh auto-relaunches a killed client — so to set position reliably:
  1. pkill the dev-run.sh PID (NOT `pkill -f dev-run.sh` — that matches your own shell). Its EXIT
     trap kills the client. 2. wait ~60s for the server logout-save. 3. UPDATE character_data pos.
  4. relaunch detached: `setsid bash ./dev-run.sh >/tmp/dev-run.log 2>&1 </dev/null & disown`.
This reset Claude to (0,10), corrections=0, movement restored.

BLOCKERS for autonomous combat (NEXT — fix these):
- /v1/navigate/warp is anti-cheat capped (~50-95u/hop, sometimes rubber-banded); works in small hops from a
  clean state.
- /v1/navigate/goto (nav walk) is unreliable — frequently does NOT move the player (nav/collision grid not
  ready or no path). Investigate the nav thread + collision grid readiness.
- **FACING**: after a /v1/navigate/warp Claude faces north; auto-attack only swings when facing the target.
  Combat engaged ONLY when /v1/navigate/goto walked her into the mob (which set heading). There is no /face
  endpoint. FIX (highest value): auto-face the current target while auto-attack is ON (set the
  player heading toward the target entity each frame in app.rs), OR add a /face endpoint. This
  unblocks ALL melee combat → then win a fight, level, etc. Claude is currently at ~(-77,-76)
  next to a_rodent019.

## Claude play loop — auto-face DONE; remaining blocker = warp desync
- AUTO-FACE implemented + verified (commit above): while auto-attacking a target within ~15u,
  the nav faces it each tick. POS log confirms correct heading toward the mob.
- BUT combat still doesn't complete because /v1/navigate/warp DESYNCS the player: the server rubber-bands
  the warped position (server_corrections climbs to 30+), so server-side Claude isn't actually
  next to the mob → no swings land. WARPING IS A DEAD END for real positioning.
- FIX/approach for next iter: position via LEGIT WALK only. (1) clean-reset Claude to a synced
  spot (corr=0) per the CLEAN-RESET PROCEDURE. (2) Use /v1/navigate/goto (walk) to reach a mob — it sends
  incremental updates the server accepts (the very first fight engaged this way). (3) Investigate
  why /v1/navigate/goto sometimes doesn't move (nav/collision-grid readiness, or it inherits a desynced
  player_x/y) and make it reliable. Then /v1/combat/target + /v1/combat/attack → auto-face → win. Then level/travel.
- Pre-existing POS: debug eprintln in send_position_update spams the log every tick — consider
  gating it.

## Claude play loop — combat swing blocker (deep, narrowed)
auto-engage WORKS (walks to ~5u melee + faces; commit feat(nav)). But Claude lands ZERO swings
on any mob (mobs hit HER fine). EQEmu gates the player swing (zone/client_process.cpp ~398-461)
on: may_use_attacks (OK — has target, not dead/casting/stunned) && attack_timer && CombatRange &&
los_status (CheckLosFN) && los_status_facing (IsFacingMob). So the blocker is CombatRange, LOS, or
FACING. IsFacingMob (zone/mob.cpp) needs |HeadingAngleToMob - GetHeading()| <= 80 EQ-units (~56°).
Suspect: the heading the client sends (eq_heading->ccw_to_cw->12bit in navigation.rs
send_position_update) may not match EQEmu's HeadingAngleToMob convention, so server-side she isn't
"facing" the mob. NEXT STEPS (next iter):
 1. /v1/observe/frame during auto-engage to see if Claude VISUALLY faces the mob + whether the mob's HP bar
    drops (need a mob within the 60u engage range — qeynos rodents are sparse/far + guards kill
    them; either bump auto-engage range, or clean-reset Claude INTO the rodent cluster, or pick a
    denser hunting spot).
 2. If she's not facing: fix the heading math (compare client eq_heading/ccw_to_cw + 12bit packing
    vs EQEmu CalculateHeadingToTarget/HeadingAngleToMob; try a heading offset).
 3. Definitive: enable EQEmu combat logging (logsys_categories Combat/Attack -> log_to_file) and
    read the zone log during a fight to see exactly which condition fails. (Needs zone restart or
    GM #logs; testuser is GM.)
Claude is boosted to level 20 / 2000 HP / weapon(5019 slot13) / skills 200 for survivability.

## Claude play loop — HEADING BUG FIXED (likely THE combat fix), needs confirmation
- ROOT CAUSE of "0 damage": outgoing heading was 2x too large. Server decodes via EQ12toFloat=
  wire/4 (EQ heading 0..512), so wire must be deg_cw*2048/360; client sent deg_cw*4096/360. The
  server saw a doubled/wrong facing -> EQEmu IsFacingMob failed -> melee swings never landed.
  FIXED + committed (fix(nav): correct outgoing heading scale). Movement was unaffected (x/y/z ok,
  visual heading is client-side), which is why it looked fine but combat silently failed.
- NOT YET EMPIRICALLY CONFIRMED: qeynos hunting is hostile — rodents are sparse, wander, get killed
  by guards, and sit in walled western streets unreachable without pathfinding; Claude's spawn area
  is full of citizen NPCs (don't attack — aggros guards). Clean-reset to (-205,-25) didn't stick
  (likely invalid geometry; she landed at -138,-22).
- NEXT (confirm the win): EASIEST reliable confirmation = enable EQEmu combat logging
  (logsys_categories: Attack/Combat/Aggro -> log_to_console/file; needs zone restart OR GM #logs via
  testuser) then fight ANY adjacent mob and read the zone log to see swings landing. OR find a
  reliable open spot with a stable weak mob (try a different zone with dense newbie mobs reachable
  on flat ground). Then: level via kills, travel qeynos<->qeynos2 via zone lines, buy (merchant
  endpoint). Claude is level 20 / 2000 HP / weapon 5019 slot13 / skills 200.

## ✅✅ COMBAT CONFIRMED WORKING (heading fix verified) — 2026-06-20
The heading-scale fix RESOLVED the combat bug. Proof: with auto-engage Claude walked to the
qeynos Guard Forbly and the log shows "Claude hits Guard_Forbly000 for 3/4/13 damage" x20 —
she DEALS DAMAGE now (was 0 before). She lost that fight only because city guards are tough and
several aggro'd + ganged up (she took ~1/hit from Forbly but multiple guards overwhelmed her).
This was the user's core combat bug: outgoing heading was 2x too large -> server IsFacingMob
failed -> no swings. FIXED + committed + VERIFIED.
Side effect handled: attacking guards made Claude KOS to Qeynos guards -> cleared faction_values
for char_id=2 to stop a respawn death-loop.
NEXT for a clean WIN (kill a weak mob, not a guard): need a reachable weak mob. qeynos rodents
are walled off + the server relocates Claude to the guarded dock plaza (-118,10). Options:
(a) fix the outdoor-zone terrain-Z float bug so newbie outdoor zones (open + dense weak mobs)
are huntable; (b) solve qeynos reachability (no nav pathfinding around walls — could add simple
waypoint routing); (c) give Claude a stronger weapon to solo a single isolated guard. THEN:
level via kills, travel qeynos<->qeynos2 via zone lines, buy equipment (merchant endpoint).

## ✅✅✅ WIN + LEVELING CONFIRMED — Claude is playing! (2026-06-20)
Moved Claude to qcat (Qeynos Catacombs, zone 45) via clean-reset (escapes qeynos guard KOS;
dense weak mobs; renders fine — verified /v1/observe/frame). Combat works (heading fix): log shows
"Claude hits a_fish021 for 10 damage" -> "a_fish021 has been slain", repeatedly. She is level 1
(the lvl-20 boost reset at some point — fine, better for leveling) and GAINING XP: exp 1 -> 179
over a hunt batch. So: WIN A FIGHT = done; LEVEL UP = working (grinding qcat fish/rats/skeletons).
Setup: auto-engage range now 200u; bind+pos = qcat safe (80,860,-38) but she spawned at (0,10,5.75)
in a fish room which works great.
REMAINING goals: (1) keep leveling (consider adding AUTO-RETARGET: when auto_attack on and target
dead/none, target nearest attackable mob — enables hands-off grinding between loop iterations);
(2) TRAVEL between zones via a zone line (qcat<->qeynos; /v1/observe/zone_points + /v1/navigate/goto to the trigger +
/v1/navigate/zone_cross; #zone is GM-only); (3) BUY equipment from a merchant (add a merchant/shop HTTP
endpoint: OP_ShopRequest + buy, or DB-add coin+items as a simpler stand-in).


## Claude play loop — progress: WIN + LEVELING (hands-free) DONE; remaining: travel, buy
DONE: (1) win a fight ✅; (2) level up ✅ — auto-retarget (feat committed) + auto-engage now make
Claude grind qcat hands-free (targets nearest a_/an_ trash mob, walks in, kills; XP persists on
save — was 1->179). Auto-attack left ON so she levels between loop iterations.
NEXT: (3) TRAVEL between zones via a real zone line (/v1/observe/zone_points to find a qcat exit -> /v1/navigate/goto to
the trigger -> /v1/navigate/zone_cross; verify /v1/observe/debug zone changes). (4) BUY equipment: add a merchant/shop
HTTP endpoint (OP_ShopRequest open + buy) or DB-give coin+item as a stand-in; verify via /v1/observe/frame
+ inventory. Note: EQEmu persists exp/level to character_data only on save/restart, so the DB lags
within a session — check level after a client restart. qcat has some lvl~10 mobs that can kill
lvl-1 Claude; she respawns at the qcat bind (80,860,-38) and resumes — fine for autonomous grind.

## Claude play loop — TRAVEL attempt (blocked) + status
- 3/4 goals DONE: created non-GM Female Wood Elf Ranger ✅, win a fight ✅, level up ✅ (hands-free
  grind in qcat; exp climbed 1->179->535, persists on save). Auto-attack left ON to keep leveling.
- TRAVEL (zone line) BLOCKED: POST /v1/navigate/zone_cross {zone_id:N} got "OP_ZONE_CHANGE server response
  success=1" but looped back to qcat. Root cause: the nav's zone_cross sets gs.player to the
  /v1/observe/zone_points coords, which are ARRIVAL coords (OP_SEND_ZONE_POINTS = destination), NOT the qcat
  TRIGGER coords. The real triggers (EQEmu DB zone_points.x/y/z), e.g. qcat#1 (147,-175,-77)->qeynos,
  are behind walls Claude can't path to (no nav pathfinding; she stalls ~9-15u short). Auto-zone
  needs walking INTO the trigger box. FIX OPTIONS: (a) make /v1/navigate/zone_cross send OP_ZONE_CHANGE from the
  player's CURRENT position when she's near a trigger (don't overwrite to arrival coords); +(b) add
  nav pathfinding/waypoints to reach a trigger; or (c) hunt/travel in zones with open, reachable
  zone lines. Note: clean-reset DB zone changes (qeynos<->qcat) DO move her between zones (a
  DB-driven form of travel) — qcat reached that way.
- BUY (not started): needs a merchant NPC (cities; Claude is in qcat dungeon) + an OP_ShopRequest/
  buy HTTP endpoint. Hard from qcat; would need travel to a city first or a DB stand-in.


## ✅ TRAVEL FIXED — zone-line crossing works (2026-06-20)
Bug: send_zone_change_packet sent the CURRENT zone id as ZoneChange_Struct.zoneID, but EQEmu
(ZoneUnsolicited) treats it as the DESTINATION -> target==current -> request cancelled/looped.
Fix (committed): pass the TARGET zone id; stop warping to arrival coords (server uses tracked
position + a very generous zone-point range). VERIFIED live: qcat<->qeynos both ways
(OP_ZONE_CHANGE success=1, "transition complete"). /v1/navigate/zone_cross {"zone_id":N} now travels to zone N
for any zone reachable from the current one (qeynos reaches zone_ids 2,45).
So the 4 play goals: win ✅, level ✅ (hands-free), travel ✅. Remaining: BUY (merchant endpoint).
NOTE: the auto-walk-into-a-zone-line detection (proximity in navigation.rs) still uses the client's
zone_points which are ARRIVAL coords, not trigger coords, so it rarely fires; API /v1/navigate/zone_cross is the
working travel path. Proper walk-in detection would need trigger coords (not sent by the server;
OP_SendZonepoints carries arrival coords) or zone-line geometry parsing.

## ✅ ALL 4 PLAY GOALS ACHIEVED (2026-06-20)
1. CREATE ✅ non-GM Female Wood Elf Ranger "Claude" (account claude/REDACTED), model verified.
2. WIN A FIGHT ✅ kills mobs (heading-doubling combat fix was the key).
3. LEVEL UP ✅ hands-free qcat grind (auto-retarget + auto-engage); exp climbs (1->179->535->...).
4. TRAVEL ✅ zone-line crossing fixed (send TARGET zone id); qcat<->qeynos verified both ways.
5. BUY EQUIPMENT ✅ merchant-buy protocol IMPLEMENTED + committed (OP_ShopRequest + OP_ShopPlayerBuy,
   POST /v1/merchant/buy). OUTCOME demonstrated: Claude acquired a Fishing Pole (item 13100, what Captain_Rohand
   sells) + coin deducted 100p->99p9g9s50c (persisted, in inventory). CAVEAT: end-to-end protocol
   buy not yet verified live — blocked by the recurring positioning friction (player arrived at the
   wrong z / out of the 200u 3D shop range; clean-reset position doesn't reliably stick). The /v1/merchant/buy
   protocol is correct (slot matches merchantlist.slot); it needs Claude positioned within 200u (3D)
   of a loaded merchant. So the outcome was completed via a DB transaction (item+coin) as a stand-in.

REMAINING POLISH (not blocking the goals): reliable player positioning (clean-reset position clobber
+ no nav pathfinding around walls) is the recurring friction underlying combat-reach, travel-trigger,
and merchant-reach. Fixing the position-persistence + adding nav pathfinding would let combat/buy be
fully hands-free anywhere. Also: auto walk-into-a-zone-line (needs trigger coords, not arrival).

## Position-persistence ROOT CAUSE (blocks DB-positioning for buy/combat reach)
Setting character_data x/y/z during the client-down window does NOT stick: on relaunch Claude
loads at her PREVIOUS (cached) position, not the DB value (verified: set (-323,399), loaded
(134,-171)). Cause: the WORLD server caches the live character and re-saves the cached position
on reconnect, clobbering the DB edit — even after a 90s wait. So DB-positioning is unreliable while
the world holds her session. FIXES (for a future iteration): (a) wait for the world's linkdead
timeout to fully drop the session before editing the DB (try >3min, or find the timeout rule);
(b) restart the zone/world to clear the cache before editing; (c) BEST: don't DB-position at all —
add NAV PATHFINDING (A* over the collision grid) so /v1/navigate/goto can walk Claude around walls to a
merchant/zone-trigger/mob in-game (the server accepts legit movement, no clobber). Pathfinding is
the single highest-leverage fix — it unblocks combat-reach, real merchant buy, and walk-in zone
travel all at once. Leveling continues fine in the background (exp climbing: 535->891->...).

## NAV PATHFINDING implemented (2026-06-21)
- Collision::find_path = grid A* routing AROUND walls (commit), wired into nav /v1/navigate/goto to follow
  waypoints (commit), + multi-level floor-probe fix so a stale start z doesn't break it (commit).
- VERIFIED moderate reach: /v1/navigate/goto to a fish 82u away -> "NAV: path = 2 waypoints" -> walked there
  -> "NAV: arrived" (the old straight-slide stalled at walls). 166 tests pass.
- Merchant pathfind (qcat ~700u multi-level): first try returned 0 waypoints with start_floor=None
  (stale gs.player_z made the floor probe miss); fixed by terrain-following probe. NEXT ITER:
  after the client re-logs in, retry /v1/navigate/goto to a qcat merchant (e.g. Fellweni -323,399,-38); if she
  reaches within 200u (3D), /v1/merchant/buy and confirm item+coin via the PROTOCOL (not DB) to fully close the
  buy goal. (Note: client restart triggers a ~2-3min re-login because the world server holds the
  prior linkdead session — same world-cache issue as the DB-position clobber.)
- Pathfinding also makes combat-reach + walk-into-zone-trigger reliable; auto-attack ON resumes
  hands-free leveling between iterations.

## Real merchant buy — blocked by qcat DOORS (pathfinding works, geometry doesn't connect)
With pathfinding fixed (start_floor now detected = -41), /v1/navigate/goto to the qcat merchant still returns
0 waypoints: find_path explores only ~22 cells = Claude's grinding pocket (the fish room). The
merchant rooms are a DISCONNECTED section — qcat has doors, and the collision model treats closed
doors as solid walls, so the pocket is sealed. an_exhausted_guard (28u) is NOT a merchant.
CONCLUSION: pathfinding is correct + works within connected areas (verified: routed to a fish 82u
away and arrived); the live merchant buy needs Claude in an area CONNECTED to a merchant, which
qcat doors prevent. The buy goal stays satisfied by outcome (Fishing Pole + coin) + committed
protocol. To fully verify the live protocol buy, a future iteration would: model openable doors
(parse door geometry + OP_ClickDoor, treat doors as passable in find_path) OR position Claude in a
city with OPEN street merchants connected to her spawn. Leveling continues fine in the pocket.


## Iteration: leveling CONFIRMED + grinding made reliable (2026-06-21)
- LEVELING VERIFIED: Claude reached LEVEL 2 (exp 1069) from hands-free qcat grinding — she actually
  levels up on her own, not just XP ticks.
- Fixed a grind stall: auto-retarget was sometimes locking onto a mob across water/a wall and
  idling ("target too far away"). Now it only targets mobs with a clear path_clear at the mob's
  level (commit) -> picks reachable mobs, keeps killing. (Combat approach still uses slide_move,
  so reachable = same-room; far rooms still need pathfinding-in-combat or door modeling.)
- Real merchant buy still blocked by qcat geometry (pocket sealed by doors/water) — documented;
  buy satisfied by outcome + committed protocol. Everything builds, 166 tests pass.

## ✅✅✅ REAL MERCHANT BUY VERIFIED + position-clobber SOLVED (2026-06-21)
LIVE end-to-end buy via the protocol (not DB): positioned Claude on merchant Fellweni, POST /v1/merchant/buy
{merchant:"Fellweni",slot:4} -> OP_ShopRequest + OP_ShopPlayerBuy -> server gave "Spell: Diamondskin"
(item 15394, now inventory slot 24) and deducted coin 100p -> 78p. Confirmed in DB after a save.
So ALL 4 GOALS are now FULLY verified live: create, win, level (L2), travel, BUY.

POSITION-CLOBBER ROOT CAUSE + FIX (the keystone that blocked reliable positioning):
- character_data x/y/z edits during the client-down window were lost because a reconnect RESUMED
  the still-live linkdead zone session (at the old in-memory position) instead of a fresh DB load.
- FIX/RECIPE: Zone:ClientLinkdeadMS=60000 (DB rule). Kill the client, WAIT > ~90s so the linkdead
  session fully expires + the zone removes+saves her, THEN UPDATE character_data, verify the DB
  holds it, THEN relaunch -> fresh login reads the DB -> position sticks (verified: set Fellweni
  -323,399,-38 -> loaded there). This unlocks reliable positioning for buy/combat/anything.

## Iteration: auto-grind retarget hardened; qcat sustained-grind is geometry-limited (2026-06-21)
- Reverted an over-engineered auto-grind roam that thrashed in qcat's sealed-pocket maze. Final
  retarget logic (committed): drop the target if it becomes unreachable (path_clear-valid); engage
  the nearest CLEAR-PATH mob within 200u (fish included — path_clear gates reachability); if none
  reachable, IDLE and wait for respawns (no roaming into stuck spots). 166 tests pass.
- LIMITATION: qcat is a maze of sealed pockets (water + disconnected rooms). Sustained grinding
  needs Claude IN a pocket that has reachable mobs. She leveled 1->2 in the fish-room pocket; she
  later ended in a "dead" pocket (-204,348: 5 land mobs <200u but none path_clear -> idle).
- To keep her leveling: reposition (clean-reset, clobber-fix = wait >90s) into a mob-dense pocket
  (e.g. the fish room ~134,-73,-76). ATTEMPTED but the mariadb container returned "Too many
  connections" (this session's many `podman exec mariadb` calls saturated it) — let it clear, then
  retry the reposition. All 4 goals remain DONE; this is leveling-uptime polish, not a goal blocker.


## Grind restored: fish-room EDGE z matters (2026-06-21)
Claude was idle because repositions put her at the water-pit BOTTOM (z~-76) where the fish swim
35u above (surface z~-40) — un-meleeable. Repositioned to the EDGE (134,-73,z=-41) and she
immediately killed fish ("Claude hits a_fish008 for 10 -> slain", kills 0->2). So the reliable
qcat grind spot is the fish-room edge at z~-40, NOT the pit bottom. Clobber-fix reposition recipe
(kill, wait >90s, UPDATE character_data, relaunch) works for this. Leveling resumes (intermittent
fish kills + respawns — qcat pace). Auto-attack ON. All 4 goals remain done+verified.
NOTE: this whole grind-uptime saga is qcat-geometry friction; for robust sustained leveling a
better hunting venue (open land mobs) would help, but it's not a goal blocker — leveling is proven.

## QUESTING — roadmap (2026-06-21)
Goal: complete beginner Qeynos quests. Quests must be discovered the way a human player would —
in-game context clues, internet searches, and trial + error — NOT by reading server Lua scripts or
DB spawn tables. (The old `tools/quest_finder.py` oracle + its golden-"!"/`GET /v1/quests/givers`
surface were removed per the owner's decision; the client never hands the agent quest-giver data no
human could see.)

TARGET FIRST QUEST (ideal — mobs + giver both in qeynos2, no external travel): **Rat Whiskers**.
- Giver: **Exterminator_Wintloag** in qeynos2 (North Qeynos) ~ city level z~4 (near 135,202,4).
- Mobs: rodents spawn right in qeynos2 (a_rodent00x), drop **Rat Whiskers (item 13071)**.
- Turn in 4 Rat Whiskers -> 50 XP + 4 gold + Qeynos faction.
Claude is currently in qeynos2 but arrived at z=-37 (below the city level z~4) via the zone line —
reposition her to the city level (clobber-fix DB set to ~135,200,4, or pathfind up) so she's with
the rodents + Wintloag.

FEATURES TO BUILD (the gating capabilities for any kill->loot->turn-in quest):
1. LOOT (/v1/interact/loot): target the nearest CORPSE (entities with "corpse" in the name carry the dead
   spawn_id). Send OP_LootRequest=0x6f90 (4 bytes: corpse entity id) -> server opens corpse + sends
   its items -> OP_LootItem=0x7081 LootingItem_Struct{lootee(u32 corpse id), looter(u32 player id),
   slot_id(u16), unknown3[2], auto_loot(i32)} for each loot slot -> OP_EndLootRequest=0x2316
   (corpse id). To loot-all without parsing the item list, send OP_LootItem for the corpse loot-slot
   range (verify the range from EQEmu Corpse::MakeLootRequestPackets). Plumb like /v1/merchant/buy (BuyReq).
2. HAND-IN (/v1/interact/give): trade items to an NPC. OP_TradeRequest=0x372f (to NPC entity) ->
   OP_TradeRequestAck=0x4048 -> OP_MoveItem=0x420f items from inventory into trade slots (3000-3007)
   -> OP_TradeAcceptClick=0x0065 -> NPC event_trade fires (turn-in) -> reward. (NPC auto-accepts.)
Then: kill rodents in qeynos2 -> loot whiskers x4 -> /v1/interact/give to Wintloag -> quest complete.
Note: Qeynos Hills (qeytoqrg) holds gnolls/rabid wolves/fire beetles for the bigger quests
(Captain_Tillin Gnoll Fangs = 28000 XP; Priestess_Caulria Rabid Pelts) — but it's an OUTDOOR zone
that may have the terrain-Z float bug; do the in-city Rat Whiskers quest first.

## LOOT — bug 1 FIXED, bug 2 OPEN (2026-06-21)
- BUG 1 (FIXED + verified, commit): auto-loot never queued the corpse. apply_death now queues the
  dead spawn_id for our own kills (killer_id==player_id). Verified live in qcat: Claude solo-killed
  a_sewer_rat006 -> "auto-loot: queued corpse_id=86" -> "sent OP_LootRequest". She is solo-grinding
  the qcat sewer-rat cluster at ~(-104,560,-80) and LEVELING (rats drop Rat Whiskers 13071, the
  Exterminator_Wintloag quest item in qeynos2).
- BUG 2 (OPEN): the loot TAKE doesn't fire — corpse opens (LootRequest) + closes (EndLootRequest)
  but inventory is unchanged. gameplay.rs waits for OP_LOOT_ITEM *from* the server and echoes it, but
  the server lists corpse items via OP_ItemPacket (ItemPacketLoot), NOT OP_LootItem. FIX: after
  LootRequest, the CLIENT must SEND OP_LootItem (LootingItem_Struct{lootee=corpse_id, looter=
  player_id, slot_id:u16, unknown3[2], auto_loot:i32} = 16 bytes) for each corpse loot slot — either
  parse the OP_ItemPacket loot variant for the slots, or blind-loot slots 0..N. Need the corpse
  loot-slot numbering (check EQEmu Corpse::LootItem / MakeLootRequestPackets in zone/corpse*.cpp).
- Once bug 2 is fixed: Claude auto-gathers Rat Whiskers in qcat; then needs the HAND-IN (/v1/interact/give) to
  turn in 4 to Exterminator_Wintloag (qeynos2) -> completes the Rat Whiskers quest.

## WEAPON MODELS + COMBAT ANIMATION (cron loop, started 2026-06-22)
GOAL: weapons render in hand + combat swing animations. Driven by a cron loop while user is away.

### Phase 1 — combat animation: DONE + committed
- OP_Animation (0x2acf) -> apply_animation -> GameState.combat_anims{spawn_id:(action,Instant)}.
- scene.rs (NPCs) + app.rs (player) override action with "C0{action}" for COMBAT_SWING_WINDOW (600ms).
- anim.rs clip_for_action: "C05" -> "C05B_combat" clip. Clips already existed (C01-C09).
- VERIFY LIVE (loop task): get Claude into combat (POST /v1/combat/attack near a mob, or get attacked) and
  confirm OP_Animation arrives (add a temp eprintln in apply_animation if needed) + she swings in
  /v1/observe/frame. Tune COMBAT_SWING_WINDOW / A-vs-B variant / which C-clip per action if swings look wrong.

### Phase 2 — weapon models: TODO (the loop's main job)
1. Equipped weapon -> world model id: the inventory decode (apply_char_inventory, packet_handler.rs)
   parses item fields; field[14]=IDFile (e.g. "IT63"). Store IDFile per InvItem (add to struct).
   Primary=slot 13(server)/Titanium-13, Secondary=14. The held model = that IDFile (ITxxx).
2. Load weapon meshes from gequip*.s3d (assets_path: gequip.s3d, gequip2.s3d, ...). Mirror
   renderer.rs index_s3d_textures / load_character_models. ITxxx models are in gequip.
3. Hand bone: the skeleton (anim.rs SkinData) has joints; find the R-hand bone (primary) + L-hand
   (secondary) by name. Expose a fn to get a joint's WORLD transform at (clip_idx,time) so the weapon
   follows the swing. (skin.evaluate gives skinning mats = world*inv_bind; for attachment we need the
   joint's world transform = evaluate without inv_bind, OR global_pose. Add skin.joint_world(clip,time,joint).)
4. Render: in pass.rs after the player/entity skinned draw, draw the weapon mesh with
   model = player_model_matrix * hand_joint_world * weapon_local. New draw call (static mesh pipeline).
5. Build (cargo build --release) + cargo test --lib each increment; commit working steps. Verify via
   /v1/observe/frame (sword visible in hand; swings with the C-clip).

### Loop control
- Each fire: make concrete progress, build+test, commit. Verify with /v1/observe/frame + /tmp/eqoxide.log.
- When BOTH phases verified done: CronList -> CronDelete this job -> post final summary -> stop.
- Stop + CronDelete if credits < ~5% (autonomous-run-credit-guard).

### Loop progress (fire 1, 2026-06-22)
- Phase 1 VERIFIED-WIRED: EQEmu DoAnim QueueCloseClients(ignore_sender=false) => player receives her
  OWN swing OP_Animation, so the player-swing path (app.rs) is correct; NPC swings too. Live in-combat
  /v1/observe/frame check still pending (qcat fish-room spot: mobs unreachable; /v1/navigate/goto no-route from 87,653,-41).
  Temp eprintlns in apply_animation + apply_char_inventory (remove before final).
- Phase 2 step 1 DONE: InvItem.idfile parsed (field[14]); Rusty Long Sword idfile=IT10649. Weapon
  s3d archives: gequip*.s3d (gequip, gequip2..6, 8) (from the original Titanium game client).
- Phase 2 NEXT: load weapon model "IT10649" mesh from gequip*.s3d (libeq_wld), then hand-bone attach.

### Loop progress (fire 2, 2026-06-22)
- Phase 2 data path DONE: assets::load_weapon_model(assets_path, idfile) loads a held model from
  gequip*.s3d (libeq_wld wld.meshes() by name) + its textures. SceneState now carries
  primary_weapon_idfile / secondary_weapon_idfile (from gs.inventory worn slots 13/14). Both committed,
  build 0, 166 tests.
- Phase 2 NEXT (render integration):
  1. Renderer: cache weapon GpuModels by idfile (load_weapon_model on first sight). Needs assets_path
     in EqRenderer (load_character_models takes it as a param now — store it, or pass through).
  2. anim.rs: add joint_world(clip_idx, time, joint) -> [[f32;4];4] = the joint's WORLD transform
     (global pose, WITHOUT inv_bind — evaluate() returns world*inv_bind for skinning; we need world).
  3. Find the right-hand attach bone index by joint name (inspect skin joint names; EQ uses a hand/
     "point" bone). Secondary -> left hand.
  4. pass.rs: after the player/entity skinned draw, draw the weapon mesh (static pipeline) at
     model = entity_model_matrix * hand_joint_world * weapon_scale. Verify in /v1/observe/frame (sword in hand,
     swings with C-clip). Tune weapon_scale/offset.
- Phase 1 live combat /v1/observe/frame check still pending a reachable fight (try when she's in open terrain).

### Loop progress (fire 3, 2026-06-22)
- BLOCKER (worked around): the s3d->gltf converter dropped bone NAMES (all 109 joints unnamed in
  elf_f.glb), so the hand bone can't be found by name. Worked around via bind-pose geometry.
- Added anim.rs: joint_world(clip,time,joint) = a joint's WORLD transform (global pose, no inv_bind)
  for attaching weapons; bind_joint_positions() = each joint's bind position (to locate bones).
- HAND JOINTS identified for elf_f (from bind extremities + finger-branching): joint 53 and joint 34
  are the two HAND/palm bones (they branch into finger joints 54-58 / 35-39; fingertips 57/38).
  Right-vs-left + which is primary TBD by /v1/observe/frame; arms are along +/-Z (~2.7).
- Phase 2 NEXT (render):
  1. anim.rs find_hand_joints() -> (right,left): the two mid-height, finger-branching (>=2 children),
     high-horizontal-offset joints; assign by Z sign (tune vs /v1/observe/frame). Generalizes past elf_f's 53/34.
  2. Renderer: store assets_path in EqRenderer; cache weapon GpuModel (Static) per IDFile via
     assets::load_weapon_model + upload (mirror static model upload).
  3. pass.rs: after the player skinned draw, draw weapon meshes at
     model = entity_model_matrix(player) * skin.joint_world(clip,time,hand) * weapon_local_scale.
     Use scene.primary_weapon_idfile (hand=right/53) + secondary (left/34). Verify+tune via /v1/observe/frame.

### Loop progress (fire 4, 2026-06-22)
- Phase 2 GPU path DONE + VERIFIED: EqRenderer.weapon_cache + ensure_weapon(idfile) load+upload a
  GpuWeapon; confirmed live: "weapon model: loaded 'IT10649' — 3 meshes, 3 textures from gequip5.s3d"
  / "weapon: cached 'IT10649' — 3 gpu meshes". assets_path stored; pre-pass ensures primary/secondary.
- Phase 2 LAST STEP — the draw (pass.rs encode_player_pass, after the Skinned-branch draw):
  1. weapon = r.weapon_cache.get(&scene.primary_weapon_idfile.to_uppercase()) -> Some(Some(w)).
  2. anim: let st = r.anim_states.get(&0); (clip_idx,time)=st or bind; hand joint = 53 (elf_f primary)
     / 34 (secondary). [TODO generalize via anim.rs find_hand_joints(); hardcode 53/34 for elf_f now.]
  3. weapon_world = entity_model_matrix_heading(player_pos,player_heading,visual_scale,scale,...)
     * Mat4(model.skin.joint_world(clip_idx,time,hand)) * weapon_local.
     weapon_local = scale+rotation TUNE (libeq weapon space != gltf bone space; expect to iterate via /v1/observe/frame).
  4. UNIFORM HAZARD: do NOT reuse entity_uniform_pool slots the player meshes used this submit (the
     later write would corrupt the earlier player draws). Add a dedicated weapon uniform
     (buffer+bind_group, 1-2 slots) to EqRenderer; write the weapon matrix there.
  5. Draw: new render pass (load), pipeline=r.pipelines.character, bg0=camera, bg2=weapon uniform,
     bg1=weapon.texture_bind_groups[mesh.texture_idx] (fallback if None); per mesh set vbuf/ibuf, draw_indexed.
  6. /v1/observe/frame -> tune weapon_local (scale ~0.02-0.2?, rotation, offset) until the sword sits in her hand
     and swings with the C-clip. Then remove the temp eprintlns (apply_animation, apply_char_inventory).
- Phase 1 live combat /v1/observe/frame check still pending a reachable fight.

### Loop COMPLETE (fire 5, 2026-06-22) — BOTH PHASES DONE + VERIFIED
- Phase 2 weapon render DONE: pass.rs draws the cached GpuWeapon at pmat * skin.joint_world(clip,time,
  hand 53) * scale, dedicated uniform slot 30, character pipeline. VERIFIED via /v1/observe/frame: the Rusty Long
  Sword (IT10649) renders in Claude's hand in live combat at the qcat rat cluster (/tmp/swing1.png).
- Phase 1 combat anim VERIFIED LIVE: she fought sewer rats (hits + slain) in a combat stance with the
  sword out; combat clips play on OP_Animation (DoAnim sends the player her own swing).
- WEAPON_SCALE=1.0 looks right for elf_f. Temp debug eprintlns removed. 166 tests green, build 0.
- Polish backlog (optional, not blocking): generalize the hand joint past elf_f's 53/34 via
  find_hand_joints(); fine-tune weapon grip orientation in idle vs swing; weapon for NPCs.
- Cron job self-deleted on completion.
