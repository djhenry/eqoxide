# Kelethin lift / elevator (issue #194) and RoF2 door wire format

## Verdict: lift is a DOOR, not the boat vehicle_id mechanic

The Kelethin "elevator" is **not** a moving NPC and **not** the boat
`vehicle_id` rider mechanic (see `boats-and-vehicles.md`). It is an ordinary
RoF2 **door** (`OP_SpawnDoor`/`OP_ClickDoor`/`OP_MoveDoor`) with
`opentype = 59`, one of a small set of "special" opentypes EQEmu recognizes.
There is no wire-protocol overlap with boats at all: no `vehicle_id` field is
ever assigned to a lift door, no `OP_BoardBoat`/`OP_ControlBoat`/`OP_LeaveBoat`
involvement, and the server performs **zero** server-side vertical motion for
these doors (confirmed: no `MovePC` call, no `FixZ`/`GetIsBoat()` exception —
`zone/waypoints.cpp:836-838` is boat-only). Any "ride the platform up" behavior
is necessarily 100% client-side rendering + client-side collision; the server
only ever tells the client "this door id is open" or "closed".

Boats and lift-doors are structurally unrelated mechanisms that both happen to
produce "player physically moves while standing on something else" — do not
conflate their implementation.

## Wire format (RoF2)

- Opcodes (`EQEmu/utils/patches/patch_RoF2.conf:89,269,270`):
  `OP_SpawnDoor=0x7291`, `OP_ClickDoor=0x3a8f`, `OP_MoveDoor=0x08e8`.
- `Door_Struct` (RoF2 wire, **100 bytes**) —
  `EQEmu/common/patches/rof2_structs.h:2999-3027`: `name[32]`, `yPos`, `xPos`,
  `zPos`, `heading`, `incline` (u32, "rotates the whole door"), `size` (u32,
  100 = normal scale), 4 unknown bytes, `doorId` (u8), `opentype` (u8),
  `state_at_spawn` (u8), `invert_state` (u8), `door_param` (u32), then ~36
  bytes of unknown/reserved trailer padding out to 100. eqoxide already has a
  KB entry on this 100-vs-80-byte mismatch (see prior note referenced in
  memory `eq-rof2-door-struct`).
- `ClickDoor_Struct` (16 bytes) — `rof2_structs.h:3037-3048`: `doorid`,
  3 unknown bytes, `picklockskill`, 3 unknown bytes, `item_id` (u32),
  `player_id` (u16), 2 unknown bytes.
- `MoveDoor_Struct` (2 bytes) — `rof2_structs.h:3050-3053`: `doorid` (u8),
  `action` (u8). `action` values: `OPEN_DOOR=0x02`, `CLOSE_DOOR=0x03` (see
  `zone/doors.cpp:37-40`), XORed against `invert_state` server-side to decide
  visual open/close. eqoxide already decodes this correctly:
  `src/eq_net/packet_handler.rs` `apply_move_door` — `action_open = p[1]==0x02`
  combined with the door's stored `invert_state`.
- `ENCODE(OP_SpawnDoor)` — `EQEmu/common/patches/rof2.cpp:3763-3794` — confirms
  only `name/xPos/yPos/zPos/heading/incline/size/doorId/opentype/
  state_at_spawn/invert_state/door_param` are populated from the DB row; the
  RoF2-only trailer bytes are fixed constants
  (`unknown0081=1, unknown0083=1` — "Both must be 1 to allow clicking doors",
  rof2.cpp:3785,3787) or zero. **There is no travel-distance/speed field on
  the wire at all** — the server never tells the client how far or how fast a
  door should visually translate.

## opentype semantics relevant to lifts (`EQEmu/zone/doors.cpp`)

- `opentype == 0`: ordinary swinging door (default case client-side).
- `opentype == 40`: auto-closes via `Process()` (doors.cpp:142); also part of
  the NPC "don't auto-open" bucket with 58 (doors.cpp:723-724).
- `EQ::ValueWithin(m_open_type, 57, 58)` (doors.cpp:542): **teleport doors** —
  triggers server-side `MovePC(...)` to `dest_zone/dest_x/y/z` on click. The
  Kelethin lift is **not** in this range (its `dest_zone` is literally the
  string `NONE` in the DB — confirmed below), so it is never a server-side
  teleport.
- `opentype == 58`: "reopen-able" special case checked throughout
  `HandleClick` (doors.cpp:273,310,375,401,473,489,564,590) — clicking it
  again while already open re-triggers the open logic instead of being a
  no-op.
- `opentype == 59`: grouped with 58 in `Doors::Open()`
  (doors.cpp:619-626, the NPC-auto-open path — NPCs are blocked from opening
  59/58 doors) and in `ForceClose`'s "reopen-able" bucket
  (doors.cpp:721-724, "borrowed some NPCOpen criteria"). **This is the
  Kelethin lift's actual opentype**, confirmed via live DB query below. It is
  a plain click-toggle door as far as the server is concerned — `HandleClick`
  (doors.cpp:158-617) just flips `m_is_open` and broadcasts `OP_MoveDoor`;
  there is no special-cased server behavior for 59 beyond the NPC-auto-open
  exclusion.
- `opentype == 54`: also present in gfaydark's door table (16 rows, see
  below) but **not** the vertical lift — not special-cased anywhere in
  `doors.cpp`, so it behaves as an ordinary toggle door client-side. These are
  a separate structure (rope-bridge-like plank chain in a different part of
  Greater Faydark, around `-400,-1900`, well outside the Kelethin tree-city
  coordinates). Do not confuse this with the elevator when reading the
  gfaydark doors table — filter specifically for the `FAYLEVATOR`/`FELE2`
  rows with `opentype=59` listed below.

## Kelethin lift topology (live DB, `podman exec eqemu_mariadb_1 mariadb -u agent -pagentpass peq`)

Query: `SELECT id,doorid,name,pos_x,pos_y,pos_z,heading,opentype,
invert_state,triggerdoor,triggertype,door_param,dest_zone FROM doors WHERE
zone='gfaydark' AND (name LIKE '%LEVATOR%' OR name LIKE '%FELE%');`

Three independent lift shafts, each = 1 `FAYLEVATOR` platform door
(`opentype=59, invert_state=1`) + 2 `FELE2` lever doors
(`opentype=59, invert_state=0, triggerdoor=<platform's doorid>,
triggertype=0, door_param=1`) — one lever at the bottom, one at the top of
the shaft. `triggerdoor`/`GetTriggerDoorID()` (doors.cpp:520-539) means
clicking a `FELE2` lever also calls `HandleClick` on the linked `FAYLEVATOR`
platform door (`entity_list.FindDoor(id)->HandleClick(...)`).

| shaft | doorid | name       | pos_z   | note                              |
|-------|--------|------------|---------|------------------------------------|
| 1     | 69     | FAYLEVATOR | 2.1582  | platform, invert_state=1           |
| 1     | 73     | FELE2      | 8.9092  | bottom lever, triggerdoor=69       |
| 1     | 74     | FELE2      | 77.4681 | top lever, triggerdoor=69          |
| 2     | 77     | FAYLEVATOR | -27.6523| platform, invert_state=1           |
| 2     | 78     | FELE2      | 77.9202 | top lever, triggerdoor=77          |
| 2     | 79     | FELE2      | -20.4945| bottom lever, triggerdoor=77       |
| 3     | 80     | FAYLEVATOR | 2.4082  | platform, invert_state=1           |
| 3     | 81     | FELE2      | 9.58386 | bottom lever, triggerdoor=80       |
| 3     | 82     | FELE2      | 78.1702 | top lever, triggerdoor=80          |

All 9 rows have `dest_zone = 'NONE'` — confirms these never hit the
`ValueWithin(m_open_type, 57, 58)` teleport branch; there genuinely is no
server-side position change for any of them.

Per-shaft top-minus-bottom z delta (best available proxy for total lift
travel distance, since it is NOT encoded anywhere on the wire — see below):
shaft 1 ≈ 75.3 units (2.16→77.47), shaft 2 ≈ 105.6 units (-27.65→77.92),
shaft 3 ≈ 75.8 units (2.41→78.17).

## Negative finding: no baked animation in the model asset

Checked both `FAYLEVATOR_ACTORDEF` and `FELE2_ACTORDEF` in
`gfaydark_obj.s3d`/`gfaydark_obj.wld` (via a scratch WLD fragment dumper,
`/tmp/pfslist`, built against this project's own `libeq_pfs`/`libeq_wld`
crates — reusable for future WLD animation investigations). Both models are
plain static `DmSpriteDef2` meshes: no `HierarchicalSpriteDef`/`Track`
skeletal animation, no `DmTrack`/`DmTrackDef2` per-vertex morph animation
(`animation_ref` is unset on both). **The exact travel distance/speed the
real client uses cannot be recovered from the model asset.**

Also checked whether `FAYLEVATOR`/`FELE2` appear as static placements in the
**main** zone WLD (`gfaydark.s3d` → `gfaydark.wld`, not `_obj.s3d`) — zero
matches for `LEVATOR` or `FELE2`. Door-table objects are spawned purely at
runtime from the `doors` DB row via `OP_SpawnDoor`; they are never also baked
into the zone's static Actor-placement list. This matters for the collision
gap below.

Also checked the RoF2 client decompile (`eqgame.exe.c`, `eqgame.exe.asm`,
`EQGraphicsDX9.dll.c`) for `opentype|movedoor|spawndoor|clickdoor|doorid|
elevator|LEVATOR|Door::` — **zero hits in all three files**. The stripped
RoF2 binary has no surviving door-animation symbols to verify client timing
directly (same limitation noted in `zone-line-crossing.md` for zone-point
symbols). This is an honest gap: the precise travel duration/speed used by
the real client is not recoverable from any source available here. Cheapest
way to get exact fidelity: capture packet timing + visually time the ride
against the native RoF2 Wine client (`~/Games/rof2`, see project memory
`eq-native-rof2-wine-client`), not further static analysis.

## Riding the lift: client-side collision only, no rider-attach packet

Since the server sends no continuous position stream and no `vehicle_id` for
doors, "riding" a translating door can only be implemented as ordinary
character-controller collision against the door's live, animated mesh
transform (the player literally stands on a rising platform's collision
surface and gets carried by floor contact, exactly like standing on a moving
platform in most 3D engines) — there is no "attach to door" wire packet in
RoF2 at all.

## eqoxide gaps found (both block #194 today)

1. **Wrong animation shape.** `src/pass.rs:311-319` `encode_door_pass`'s
   opentype dispatch only special-cases `100..=119` (Z-translate 10 units)
   and `11..=15` (X-translate 8 units); everything else, including `59`,
   falls into the `_` default hinge-swing rotation
   (`glam::Mat4::from_rotation_z(-f * FRAC_PI_2)`, pass.rs:317). The Kelethin
   lift currently visually swings open like a door instead of rising as a
   platform.
2. **No collision presence at all.** `src/nav/collision::Collision::build`
   (`collision.rs:565-640`) bakes a static triangle grid once at zone load
   from `assets.terrain` + `expand_objects(&assets.objects)`. It has no
   reference to doors anywhere in the file. The interactive door mesh set
   (`renderer.door_models`, loaded via the separate `<zone>_doors.glb` asset,
   `src/assets.rs:386-392`) is render-only and never enters this grid — this
   matches the ground-truth finding above that door objects are excluded from
   the zone's static Actor-placement bake too
   (`eqoxide_asset_server/src/zone.rs` `read_placements`, lines 137-159, reads
   only the main s3d's `wld.objects()`, which does not include
   `FAYLEVATOR`/`FELE2`). `eqoxide_asset_server/src/zone.rs:711-712`'s doc
   comment on `bake_object_models_glb` confirms this is intentional design
   ("Door placement/animation is applied client-side from live door state, so
   no instance transforms are emitted") — i.e. the asset server deliberately
   ships door meshes as named, un-transformed geometry for the client to
   place/animate itself, but eqoxide's collision system was never taught to
   consume that live-animated geometry. **Even a correctly Z-translating
   lift mesh today has zero standable collision at any height** — a player
   would fall through it.

## Option C: match native (owner-requested follow-up)

Owner asked specifically how the **native** RoF2 client rides an opentype-59
door, to sit alongside Option A (dynamic collider) and Option B (controller
special-case) as a third option. Investigated the stripped `eqgame.exe`
decompile directly for this; findings below are graded confirmed vs inferred.

**Confirmed (server/wire side, rules out several hypotheses):**
- No wire field and no DB field carries travel distance, speed, or duration
  for opentype 59. Re-checked `incline`/`size` on all 6 lift-shaft rows
  (`FAYLEVATOR`+`FELE2` x3): `incline=0` (not repurposed — contrast with the
  unrelated opentype-54 rows in the same zone, which use non-zero
  `incline≈127-147`), `size=100` (normal scale), `close_timer_ms=5000` (the
  same generic auto-reclose default used by ordinary swinging doors,
  `doors.cpp:142-149` `Process()` — not lift-specific). `door_param` on the
  platform doors (68/98/69) doesn't correspond to anything read by
  `doors.cpp`'s opentype-59 path either; only the levers' `door_param=1`
  paired with `triggerdoor` matters (chaining, doors.cpp:520-539).
- `doors.cpp` has no opentype branches beyond `0`(default)/`40`/`57`/`58`/`59`
  (full grep of `m_open_type ==`/`opentype ==` across `zone/*.cpp,*.h` —
  9 hits, all listed in this file's earlier section). None of them touch
  position for `59`. **`opentype` is treated by the server as a fully opaque
  byte it never interprets beyond these five special values** — it exists
  purely to tell the client which *visual animation style* to play.
  Conclusion: the travel distance/duration for a translating opentype **must**
  be a client-hardcoded constant (or a small hardcoded table keyed by
  opentype/opentype-bucket), not per-door authored data — there is nowhere
  else it could live. This is exactly the architecture eqoxide's own
  `src/pass.rs:311-319` `encode_door_pass` already uses (opentype-bucketed
  hardcoded local-space transform, e.g. `100..=119` → fixed 10-unit
  Z-translate) — eqoxide independently converged on the right general shape
  of the native solution; it's only missing the `59` bucket and the
  collision wiring.

**Confirmed (client binary):** `eqgame.exe` for RoF2 is fully stripped.
Exhaustive case-insensitive grep across `ghidra/eqgame.exe.c`,
`capstone/eqgame.exe.asm`, and `ghidra/EQGraphicsDX9.dll.c` for
`elevator|lift|platform|riding|ground_entity|standing_on|carrier` found no
door/lift-specific symbols (the only "platform" hits are unrelated Windows
`dwPlatformId`/`UdpPlatformDriver` OS-platform code,
`eqgame.exe.c:110653,328654,820056` etc.). Also tried locating the packet
dispatch for `OP_MoveDoor`/`OP_ClickDoor` by their raw wire opcode values
(`0x8e8`, `0x3a8f`) as disassembly immediates
(`capstone/eqgame.exe.asm:265132` `cmp dword ptr [ebp-0x5360],0x8e8` is a
plausible opcode-switch hit, and `:362903` `push 0x3a8f` for ClickDoor) but
without symbols there's no economical way to trace forward from there to the
specific animation/collision code within this investigation's budget — the
door-carry logic is folded into generic, unnamed movement/collision routines
indistinguishable from ordinary floor-collision code at the disassembly
level. **The exact in-memory algorithm (parent-to-door-transform vs.
per-frame position-delta-add vs. real per-frame OBB collision) cannot be
distinguished from the available binary with confidence.** This is an honest
gap, not a hedge — say so plainly to whoever decides between A/B/C.

**Inferred (strong, structurally necessary, not symbol-proven):** Closed EQ
doors are solid — walking through a closed door is not possible in the live
game, which is universal player experience. This means the client must
already maintain a **live, per-frame collidable representation of each
door's current world transform** (some form of dynamic OBB re-evaluated as
`open_frac` changes) purely to keep closed doors blocking movement. The most
economical, code-reuse-consistent explanation for "riding" a translating
door is that ordinary floor/ground-contact code tests against that same live
collidable surface every frame — i.e. **native's approach is structurally
Option A (a dynamic collider that includes the door's live animated
transform), not Option B (a one-off "if near a translating door, teleport/
carry the player" special case)**. There is no server hook, no packet, and
no DB field suggesting a distinct "board/attach" state exists at all — the
complete absence of any such mechanism (already established: no
`vehicle_id`, no boat-style opcodes, no server-side `MovePC` for opentype
59) is itself evidence against a bespoke attach/detach path. A further
consistency check: known live-game player experience is that if you dawdle
on the platform past `close_timer_ms` (5s) it auto-recloses and **carries
you back down** — this falls out for free if riding is symmetric live-OBB
contact (the same mechanism running in reverse as `open_frac` returns to 0),
but would require *extra* special-casing under an Option-B-style fixed-
duration "ride up, auto-dismount" hack. This favors Option A/C being the
same thing as the more likely native design.

**Gap (numeric travel distance/speed):** Not recoverable from any source
checked. No wire field, no DB field (confirmed above), no binary symbol.
The per-shaft platform→top-lever `pos_z` delta (~75.3, ~105.6, ~75.8 units,
already computed above) is the best available *proxy*, but native likely
does **not** compute distance this way at all — more likely a single
hardcoded distance/duration applied uniformly per opentype-bucket, with the
player simply arriving close enough that the static tree-platform geometry
(not the door mesh) catches their footing at the top, rather than a
mathematically exact per-shaft target. Community/anecdotal player experience
(not sourced from decompiled evidence — flagged explicitly as such, not a
citation) is that the ride takes on the order of several seconds; if exact
fidelity is wanted, the cheapest remaining path is a live timed capture
against the native Wine client (`~/Games/rof2`, project memory
`eq-native-rof2-wine-client`), not further static analysis.

**Q4 — mount/dismount:** No special mount/dismount packet or state exists
anywhere in the door protocol (re-confirmed: `ClickDoor_Struct`/
`MoveDoor_Struct` carry only doorid/action/lock-related fields, no player-
attach fields — `rof2_structs.h:3037-3053`). "Mounting" is inferred to be
nothing more than walking onto the platform's footprint while it's at rest,
identical to stepping onto any static floor; "dismounting" is walking off
that footprint onto adjacent static geometry once the platform is close
enough. If you don't step off before the door's auto-close timer fires, the
same live-collider mechanism (per the inference above) would carry you back
down symmetrically — consistent with well-known live-game behavior.

### Option C, concretely

Option C = Option A's mechanism (dynamic collider synced to the door's live
per-frame animated transform, feeding normal floor/ground-contact code) +
a client-hardcoded translate distance/duration selected by `opentype`
(extending eqoxide's existing `pass.rs` opentype-bucket convention to cover
`59`), with **no wire/protocol changes and no rider-attach state** — because
that is the best-supported reconstruction of what native actually does.
Practically, for eqoxide, choosing "C" is choosing "A, with the specific
59-bucket transform tuned to feel like native rather than left as the
generic swing/slide default" — it is not a third, structurally different
implementation path from A.

## Recommendation for eqoxide (#194)

- **No wire-protocol change needed.** No new opcode, no `vehicle_id` use for
  doors — confirm this explicitly to whoever implements #194 so they don't
  go looking for a rider-attach packet that doesn't exist.
- **Extend `pass.rs`'s opentype dispatch** to add `59` (and note `54` is a
  different, non-lift structure — do not lump them together) to the
  Z-translate bucket. Use a travel distance derived from data, not the
  current fixed `10.0` constant: either (a) pass the per-shaft empirical
  delta (~75-106 units, computed from paired platform/lever `pos_z` in the
  door DB export) through as door metadata, or (b) as a simpler first pass,
  pick one reasonable constant (~80-100 units) since exact per-shaft height
  isn't otherwise available client-side without extra data plumbing from the
  zone/door dataset.
- **Close the collision gap** — this is the harder, load-bearing half of
  "ride it." `nav::collision::Collision` needs to treat the live/open-frac
  transformed door mesh (at least for translating opentypes like 59) as a
  standable surface, re-evaluated per-frame as the platform rises, not baked
  once at zone load. This likely means either (a) a small supplementary
  dynamic-collider path alongside the static grid specifically for
  translating doors, sampled by `floor_z`/grounding each frame using the
  door's current `open_frac`, or (b) treating it as a moving-platform special
  case in the character controller (carry the player's z by the platform's
  per-frame delta while their footprint overlaps its XY extent) rather than
  trying to inject a moving object into the static bake. Either way, this
  needs new design, not just a data fix — flag it to the implementing agent
  as the real scope of #194, distinct from (and larger than) the animation
  fix above.
- **Edge cases:** `door_param` on lift/lever rows is not `-1`/`0xFFFFFFFF`
  (the documented "normal" sentinel per `rof2_structs.h:3013`) — levers carry
  `door_param=1`, worth preserving/ignoring correctly if eqoxide ever reads
  this field, since its documented meaning is door-type-specific and not used
  by any special-cased server logic for opentype 59. `triggerdoor` chaining
  (lever→platform) is server-authoritative — the server, not the client,
  decides that clicking a `FELE2` lever also opens the linked `FAYLEVATOR`
  platform door; eqoxide's client only needs to react to whichever
  `OP_MoveDoor` doorid(s) the server actually sends, no client-side chaining
  logic required.
