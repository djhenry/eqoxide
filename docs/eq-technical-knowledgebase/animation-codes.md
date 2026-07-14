# WLD Animation Track Codes (RoF2)

## What these are

Every character WLD prefixes its animation tracks with a 3-character code:
`<code><bone>_TRACK` (e.g. `L06`, `P07`). The code identifies which animation the track belongs to.
These codes are part of the **WLD data format** and are read directly off the Track fragment names —
`eqoxide_asset_server`'s `gather_anims`/`anim_label` (`eqoxide_asset_server/src/convert/mod.rs:919-925`)
parses them off the front of each Track fragment name.

The code→meaning mapping below is the established WLD animation-code convention (long documented in
the EQ data-format / model-tooling community, e.g. Lantern/OpenEQ), cross-checked here against the
codes eqoxide's own converter encounters in real character WLDs. The bold/annotated rows are the
ones that matter for the swim/idle/crouch selection bug discussed further down.

## The table (id → code → meaning)

| id | code | Meaning |
|----|------|---------|
| 0  | —    | STILL (default/no-op) |
| 1  | C01  | KICK |
| 2  | C02  | STAB |
| 3  | C03  | 2H ATK |
| 4  | C04  | IMPALE ATK |
| 5  | C05  | OVRHAND ATK |
| 6  | C06  | LEFT HND ATK |
| 7  | C07  | BASH |
| 8  | C08  | PUNCH |
| 9  | C09  | BOW |
| 10 | C10  | **SWIM ATK** (combat swing while swimming, not a locomotion clip) |
| 11 | C11  | MONK RND KICK |
| 12 | D01  | LT DMG |
| 13 | D02  | NORMAL DMG |
| 14 | D03  | FALL DMG |
| 15 | D04  | DEATH SHUDDER |
| 16 | D05  | FALL DOWN |
| 17 | L01  | WALK |
| 18 | L02  | RUN |
| 19 | L03  | JUMP ACROSS |
| 20 | L04  | JUMP |
| 21 | L05  | **FREE FALL** |
| 22 | L06  | **CROUCH WALK** (duckwalk) |
| 23 | L07  | **CLIMB** |
| 24 | L08  | **CROUCH** (a land squat/crouch pose — NOT swim-related) |
| 25 | L09  | **TREAD WATER** — the stationary-in-water loop |
| 26 | O01  | IDLE |
| 27 | S01  | OH YAH! |
| 28 | S02  | AGONY |
| 29 | S03  | WAVE |
| 30 | S04  | UP YOURS |
| 31 | S05  | "?" (unlabeled) |
| 32 | P01  | STAND STILL |
| 33 | P02  | SIT |
| 34 | P03  | **TURN RIGHT** (NOT crouch) |
| 35 | P04  | "?" (unlabeled — presumably TURN LEFT by symmetry with P03, not confirmed) |
| 36 | P05  | **KNEEL** |
| 37 | P06  | **SWIM FORWD** — the forward-swim-stroke loop |
| 38 | P07  | "?" (unlabeled — **NOT confirmed to be any kind of swim/idle animation**) |
| 39 | T01  | PLAY DRUM |
| 40 | T02  | PLAY LUTE |
| 41 | T03  | PLAY HORN |
| 42 | T04  | DEFENSE SPELL |
| 43 | T05  | GENERAL SPELL |
| 44 | T06  | MISSILE SPELL |
| 45 | T07  | FLYING KICK |
| 46 | T08  | MONK HND ATK 1 |
| 47 | T08  | MONK HND ATK 2 |

## Swimming, specifically — the two real codes

- **Stationary in water / treading water: `L09` ("TREAD WATER")**.
- **Actively swimming forward: `P06` ("SWIM FORWD")**.
- `C10` ("SWIM ATK") is a *combat* swing animation played while swimming, not a
  locomotion/idle clip — irrelevant to idle-vs-moving selection.
- `L08` ("CROUCH") is a **land crouch/squat pose, unrelated to water**. A model
  whose clip discovery mislabels `L08` as a swim-idle animation will visually
  reproduce exactly the "trying to sit/squat down" symptom, because that is
  literally what the clip is.
- `P07` has **no known description** ("?"). Do not assume it is a second
  swim-idle/treading variant — that is unconfirmed. If a given race's WLD happens
  to ship a `P07` track, treat its content as unknown until visually verified
  per-race; don't rely on it as *the* tread-water clip.

## eqoxide_asset_server's `anim_label` mapping is wrong for several codes

`eqoxide_asset_server/src/convert/mod.rs:1096-1125` currently has:

```rust
"L05" => "duckwalk",     // WRONG: L05 = FREE FALL (falling), not duckwalk
"L06" => "swim",         // WRONG: L06 = CROUCH WALK (duckwalk), not swim
"L07" => "walk_back",    // WRONG: L07 = CLIMB
"L08" => "swim_idle",    // WRONG: L08 = CROUCH (land squat) — this is the
                         //   mislabel that produces the "sitting down" bug
"L09" => "swim",         // WRONG bucket: L09 = TREAD WATER, the STATIONARY
                         //   swim loop — belongs in the "swim_idle"/treading
                         //   bucket, not the forward-stroke "swim" bucket
"P03" => "crouch",       // WRONG: P03 = TURN RIGHT
"P06" => "kneel",        // WRONG: P06 = SWIM FORWD — the correct forward-swim
                         //   loop; real KNEEL is P05 (currently unmapped)
"P07" => "swim_idle",    // UNCONFIRMED: there is no known meaning for P07 —
                         //   do not trust this as the canonical tread-water clip
```

Only `L01`-`L04` (walk/run/jump-across/jump), `O01`/`O02` (idle), `P01`
(stand-still ≈ idle_neutral), and `P02` (sit) are confirmed correct in the
existing table. Everything else in the L05-L09/P03/P06/P07 range is either
wrong or unconfirmed, per the table above.

**Corrected mapping recommendation:**
```rust
"L05" => "fall",          // FREE FALL
"L06" => "duckwalk",      // CROUCH WALK
"L07" => "climb",         // CLIMB
"L08" => "crouch",        // CROUCH (land squat — same bucket P03 should NOT be in)
"L09" => "swim_idle",     // TREAD WATER — the stationary swim loop
"P03" => "turn_right",    // TURN RIGHT (or leave unmapped if unused by renderer)
"P05" => "kneel",         // KNEEL (missing entirely today)
"P06" => "swim",          // SWIM FORWD — the forward-swim loop
"P07" => omit / mark unconfirmed,  // no known meaning
```

## Selection logic (idle-in-water vs forward-swim vs on-land) — inferred, not verified

The table above only proves *what the codes mean*; it does not, by itself, establish the runtime
state machine that decides which id to play each frame. Treat the selection rule as **inferred, not
directly verified**:

- The `L`-prefixed codes are the continuously-blended locomotion set (walk, run,
  jump, fall, crouch-walk, climb, tread-water) — logically selected purely from
  the character's local velocity/verticality/submerged state each frame, matching
  how `L01`/`L02` already work for land walk/run.
  Cheapest way to actually confirm the state-machine thresholds: capture a packet
  trace / observe the native client live while entering/leaving water and swimming
  in place vs. forward.

## Recommendation for eqoxide

1. Fix `anim_label` in `eqoxide_asset_server/src/convert/mod.rs:1096-1125` per
   the corrected table above — most importantly swap `L08`'s label from
   `"swim_idle"` to `"crouch"` (it is a literal squat pose) and move `L09` from
   the `"swim"` bucket into `"swim_idle"`, and move `P06` from `"kneel"` into
   `"swim"` (real kneel is `P05`).
2. In `eqoxide`'s renderer (`src/anim.rs:245-271`, action `"swimming"` vs
   `"treading"`), once the labels are fixed, the existing bucket logic ("swim"
   substring without "idle" = forward stroke; "swim"+"idle" = tread-water) will
   correctly resolve `P06` for the moving case and `L09` for the stationary
   case.
3. `src/app.rs:869-880` currently always emits `player_action = "swimming"` for
   any `in_water` state (comment: "never a stand/sit"), never invoking the
   `"treading"` action anim.rs already defines (`anim.rs:266-271`). Once the
   label fix lands, consider driving the stationary-in-water case to
   `"treading"` (⇒ `L09` TREAD WATER) and only the actively-moving case to
   `"swimming"` (⇒ `P06` SWIM FORWD), rather than collapsing both into one
   action — this matches the two genuinely distinct native animations rather
   than reusing one clip for both states.
4. Do not build any renderer logic around `P07` as if it were a confirmed
   tread-water/idle-in-water animation — there is no known meaning for it. If it
   needs to be used at all, verify its content visually per race first.

Related: `swimming-and-fall-damage.md` (water regions, no fall damage entering
water, `want_swim`/`in_water` gating in the controller).
