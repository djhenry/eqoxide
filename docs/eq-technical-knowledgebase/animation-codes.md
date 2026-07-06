# WLD Animation Track Codes (RoF2) — the client's own decoder table

## Source: a debug string table baked into eqgame.exe itself

`eqgame.exe` contains a switch/jump-table function (capstone disassembly,
`everquest_rof2/decompiled/capstone/eqgame.exe.asm:368255-368360`, code at VA
`0x00515d10`-`0x00515ea4`, `cmp eax,0x2f; ja default; jmp [eax*4+0x515eac]`) that
maps a raw numeric animation id (0-47) to a human-readable description string —
almost certainly used by an internal dev/debug tooltip (called from
`FUN_004b7c70`/`FUN_004b7d60` in the Ghidra decompile,
`everquest_rof2/decompiled/ghidra/eqgame.exe.c:128610,128662`, which build a
"say"/tooltip-style string containing this description).

**This is the client's own authoritative decoder ring for the 3-char WLD
animation-track-name codes** (e.g. `L06`, `P07`) used to prefix `<code><bone>_TRACK`
track names in every character WLD — confirmed the same code space is what
`eqoxide_asset_server`'s `gather_anims`/`anim_label`
(`eqoxide_asset_server/src/convert/mod.rs:919-925`) parses off the front of each
Track fragment name.

Jump-table order was verified directly against the PE image (not just assumed
linear-address order) by resolving VA `0x515eac` to a file offset and reading 48
consecutive `u32` entries — they exactly match ascending address order, so switch
value `N` ⇒ the Nth string block below, in order.

## The full table (id → code → client's own description)

| id | code | Description (verbatim from client) | asm line |
|----|------|--------------------------------------|----------|
| 0  | —    | "STILL" (default/no-op) | 368263 |
| 1  | C01  | KICK | 368264 |
| 2  | C02  | STAB | 368266 |
| 3  | C03  | 2H ATK | 368268 |
| 4  | C04  | IMPALE ATK | 368270 |
| 5  | C05  | OVRHAND ATK | 368272 |
| 6  | C06  | LEFT HND ATK | 368274 |
| 7  | C07  | BASH | 368276 |
| 8  | C08  | PUNCH | 368278 |
| 9  | C09  | BOW | 368280 |
| 10 | C10  | **SWIM ATK** (combat swing while swimming, not a locomotion clip) | 368282 |
| 11 | C11  | MONK RND KICK | 368284 |
| 12 | D01  | LT DMG | 368286 |
| 13 | D02  | NORMAL DMG | 368288 |
| 14 | D03  | FALL DMG | 368290 |
| 15 | D04  | DEATH SHUDDER | 368292 |
| 16 | D05  | FALL DOWN | 368294 |
| 17 | L01  | WALK | 368296 |
| 18 | L02  | RUN | 368298 |
| 19 | L03  | JUMP ACROSS | 368300 |
| 20 | L04  | JUMP | 368302 |
| 21 | L05  | **FREE FALL** | 368304 |
| 22 | L06  | **CROUCH WALK** (duckwalk) | 368306 |
| 23 | L07  | **CLIMB** | 368308 |
| 24 | L08  | **CROUCH** (a land squat/crouch pose — NOT swim-related) | 368310 |
| 25 | L09  | **TREAD WATER** — the stationary-in-water loop | 368312 |
| 26 | O01  | IDLE | 368314 |
| 27 | S01  | OH YAH! | (367.. see asm) |
| 28 | S02  | AGONY | 368318 |
| 29 | S03  | WAVE | 368320 |
| 30 | S04  | UP YOURS | 368322 |
| 31 | S05  | "?" (unlabeled) | 368324 |
| 32 | P01  | STAND STILL | 368326 |
| 33 | P02  | SIT | 368328 |
| 34 | P03  | **TURN RIGHT** (NOT crouch) | 368330 |
| 35 | P04  | "?" (unlabeled — presumably TURN LEFT by symmetry with P03, not confirmed) | 368332 |
| 36 | P05  | **KNEEL** | 368334 |
| 37 | P06  | **SWIM FORWD** — the forward-swim-stroke loop | 368336 |
| 38 | P07  | "?" (unlabeled — **NOT confirmed to be any kind of swim/idle animation**; the client itself has no description for it) | 368338 |
| 39 | T01  | PLAY DRUM | 368340 |
| 40 | T02  | PLAY LUTE | 368342 |
| 41 | T03  | PLAY HORN | 368344 |
| 42 | T04  | DEFENSE SPELL | 368346 |
| 43 | T05  | GENERAL SPELL | 368348 |
| 44 | T06  | MISSILE SPELL | 368350 |
| 45 | T07  | FLYING KICK | 368352 |
| 46 | T08  | MONK HND ATK 1 | 368354 |
| 47 | T08  | MONK HND ATK 2 | 368356 |

(Raw per-instruction lines for the bold/swim-relevant rows, all in
`capstone/eqgame.exe.asm`: `L05`@368304 "0x00515dcc", `L06`@368306 "0x00515dd4",
`L07`@368308 "0x00515ddc", `L08`@368310 "0x00515de4", `L09`@368312 "0x00515dec",
`P03`@368330 "0x00515e34", `P04`@368332 "0x00515e3c", `P06`@368336 "0x00515e4c",
`P07`@368338 "0x00515e54".)

## Swimming, specifically — the two real codes

- **Stationary in water / treading water: `L09` ("TREAD WATER")**, confirmed.
- **Actively swimming forward: `P06` ("SWIM FORWD")**, confirmed.
- `C10` ("SWIM ATK") is a *combat* swing animation played while swimming, not a
  locomotion/idle clip — irrelevant to idle-vs-moving selection.
- `L08` ("CROUCH") is a **land crouch/squat pose, unrelated to water**. A model
  whose clip discovery mislabels `L08` as a swim-idle animation will visually
  reproduce exactly the "trying to sit/squat down" symptom, because that is
  literally what the clip is.
- `P07` has **no description in the client's own table** ("?"). Do not assume
  it is a second swim-idle/treading variant — that is unconfirmed. If a given
  race's WLD happens to ship a `P07` track, treat its content as unknown until
  visually verified per-race; don't rely on it as *the* tread-water clip.

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
"P07" => "swim_idle",    // UNCONFIRMED: client's own table has no description
                         //   for P07 ("P07: ?") — do not trust this as the
                         //   canonical tread-water clip
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
"P07" => omit / mark unconfirmed,  // client's own table has no description
```

## Selection logic (idle-in-water vs forward-swim vs on-land) — NOT confirmed from decompile

The debug string table above only proves *what the codes mean*; it does not, by
itself, prove the runtime state machine that decides which numeric id to play
each frame (that logic lives elsewhere in the stripped `eqgame.exe` and was not
cheaply locatable — the callers found, `FUN_005159f0`/`FUN_00515660`, are an
unrelated keybinding/skill list, not the movement-animation selector). Treat the
selection rule as **inferred, not directly verified**:

- The `L`-prefixed codes are the continuously-blended locomotion set (walk, run,
  jump, fall, crouch-walk, climb, tread-water) — logically selected purely from
  the character's local velocity/verticality/submerged state each frame, matching
  how `L01`/`L02` already work for land walk/run.
  Cheapest way to actually confirm the state-machine thresholds: capture a
  packet trace or watch `eqgame.exe` live with a debugger while entering/leaving
  water and swimming in place vs. forward — out of scope for a static-decompile
  answer.

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
   case, per the client's own table.
3. `src/app.rs:869-880` currently always emits `player_action = "swimming"` for
   any `in_water` state (comment: "never a stand/sit"), never invoking the
   `"treading"` action anim.rs already defines (`anim.rs:266-271`). Once the
   label fix lands, consider driving the stationary-in-water case to
   `"treading"` (⇒ `L09` TREAD WATER) and only the actively-moving case to
   `"swimming"` (⇒ `P06` SWIM FORWD), rather than collapsing both into one
   action — this matches the two genuinely distinct native animations rather
   than reusing one clip for both states.
4. Do not build any renderer logic around `P07` as if it were a confirmed
   tread-water/idle-in-water animation — the client's own table has no name for
   it ("P07: ?"). If it needs to be used at all, verify its content visually
   per race first.

Related: `swimming-and-fall-damage.md` (water regions, no fall damage entering
water, `want_swim`/`in_water` gating in the controller).
