# Spell-cast outcome packets (RoF2, caster's own cast)

Server source: `/home/dhenry/git/EQEmu`. All findings below are **confirmed by
reading source** (not inferred) unless marked otherwise. Struct field layout
confirmed for the RoF2 wire specifically via `common/patches/rof2_structs.h`
+ `common/patches/rof2.cpp` ENCODE handlers, cross-checked against the
"pass-through by default" behavior of `StructStrategy` (see below).

## Key mechanism: base struct vs. wire struct

`zone/spells.cpp` builds packets using the **generic** structs in
`common/eq_packet_structs.h`. The per-client `Strategy` (RoF2 = `rof2.cpp`)
then either:
- **re-encodes** into a RoF2-specific wire struct (`common/patches/rof2_structs.h`,
  namespace `structs::`) if an `ENCODE(OP_x)` handler exists, or
- **passes the bytes straight through unmodified** if no handler exists
  (`common/struct_strategy.cpp:30-35,70-72`, `StructStrategy::PassEncoder`).

So for opcodes with no `ENCODE()` in `rof2.cpp`, the emu-side struct layout
*is* the wire layout for RoF2.

## 1. Cast begins — OP_BeginCast

- Server call site: `Mob::SendBeginCast`, `zone/spells.cpp:497-518`. Called from
  `Mob::DoCastSpell` at `zone/spells.cpp:450`, **only when `slot !=
  CastingSlot::Discipline`**. Sent even for 0-cast-time (instant) spells.
- Emu-side struct (`common/eq_packet_structs.h:480-486`):
  `caster_id(u16)@0, spell_id(u16)@2, cast_time(u32)@4` — 8 bytes.
- **RoF2 wire struct differs** (`common/patches/rof2_structs.h:720-726`):
  ```
  struct BeginCast_Struct { uint32 spell_id; /*0*/ uint16 caster_id; /*4*/ uint32 cast_time; /*6*/ }; // 10 bytes
  ```
  Confirmed by `ENCODE(OP_BeginCast)`, `common/patches/rof2.cpp:631-640`:
  `OUT(spell_id); OUT(caster_id); OUT(cast_time);` widens caster_id/spell_id
  order and the total size becomes 10 bytes (`SETUP_DIRECT_ENCODE` allocates
  `sizeof(structs::BeginCast_Struct)`, `common/patches/ss_define.h:41-58`).
- **Sent to the caster themselves too.** `SendBeginCast` calls
  `entity_list.QueueCloseClients(this, outapp, false /*ignore_sender*/, ...)`
  (`zone/spells.cpp:508-515`); `ignore_sender=false` means the sender (caster)
  is included (`zone/entity.cpp:1743`: `(!ignore_sender || client != sender)`).
- Opcode value: `OP_BeginCast=0x318f` (`utils/patches/patch_RoF2.conf:176`).
- **Fizzle and insufficient-mana never reach this call** — both bail out of
  `DoCastSpell` *before* line 450 (see below), so no `OP_BeginCast` is sent at
  all for those two outcomes.

## 2. Cast interrupted — OP_InterruptCast

- Struct (`common/eq_packet_structs.h:446-451` = `rof2_structs.h:688-693`,
  byte-identical, no ENCODE handler exists → pass-through):
  ```
  struct InterruptCast_Struct { uint32 spawnid; uint32 messageid; char message[0]; }; // 8 bytes + optional trailing C-string
  ```
- Two packets are sent per interrupt, from `Mob::InterruptSpell(uint16 message, uint16 color, uint16 spellid)`, `zone/spells.cpp:1242-1347`:
  1. **To the caster only** (if `IsClient()` and `message != SONG_ENDS`):
     8-byte packet, `messageid = message`, `spawnid = GetID()` (own id).
     `zone/spells.cpp:1303-1315`. Followed immediately by
     `SendSpellBarEnable(spellid)` → `OP_ManaChange` (see §4).
  2. **To nearby others** (`QueueCloseClients`, `ignore_sender=true` so NOT
     resent to the caster): variable-length packet with `messageid =
     message_other` and a trailing NUL-terminated caster name in `message[]`
     (`zone/spells.cpp:1339-1345`).
- Default `message` when the no-arg `InterruptSpell()` overload is used
  (`zone/spells.cpp:1232-1239`) is `INTERRUPT_SPELL` (eqstr 439) — i.e. the
  caster's own `OP_InterruptCast.messageid` is normally the **generic**
  "Your spell is interrupted." id, not a specific reason. Exceptions with an
  explicit non-generic `messageid`: Divine Aura cancel passes
  `InterruptSpell(173, 0x121, false)` — 173 happens to equal `SPELL_FIZZLE`'s
  string id (`zone/spells.cpp:568`), a special case, not the normal fizzle path.
- Triggers for `InterruptSpell()`/`OP_InterruptCast` (all confirmed in
  `zone/spells.cpp`): invalid spell id (300), no target found (400-403),
  spell-already-casting-another (1458-1464), movement/damage "lost
  concentration" channel-chance roll fail (1555-1558), various
  `DoCastingChecksOnCaster` cancellations (stunned/mezzed/feared — those
  `return false` *without* calling `InterruptSpell`, see below), Divine Aura
  (568), bard missing instrument in some cases (1651).
  **Note:** several early-exit checks in `DoCastingChecksOnCaster`
  (`zone/spells.cpp:520-620`, e.g. stunned/mezzed/feared) just `return false`
  and do **not** call `InterruptSpell` themselves — the caller
  (`Mob::CastSpell`) is responsible; verify call site if you need every last
  edge case (not fully traced here — inferred that most paths funnel through
  `InterruptSpell` eventually, not directly confirmed for every early return).
- Opcode value: `OP_InterruptCast=0x048c` (`utils/patches/patch_RoF2.conf:302`).

## 3. Fizzle — NOT OP_InterruptCast

- **Fizzle check happens in `Mob::DoCastSpell` BEFORE `OP_BeginCast` is even
  sent** (`zone/spells.cpp:321-354`, vs. `SendBeginCast` at line 450). So a
  fizzled cast never gets an `OP_BeginCast` *or* an `OP_InterruptCast` packet.
- On fizzle (`zone/spells.cpp:321-353`):
  1. Mana is partially consumed (1/4 cost): `SetMana(GetMana() - use_mana)`.
  2. `StopCasting()` is called (`zone/spells.cpp:1351-1379`) — for clients this
     sends **`OP_ManaChange`** only (`ManaChange_Struct{new_mana, stamina,
     spell_id=casting_spell_id, keepcasting=0, slot=casting_slot}`,
     `zone/spells.cpp:1369-1376`). This is what re-enables the spell gems.
  3. `MessageString(Chat::SpellFailure, fizzle_msg)` is sent to the caster —
     since it's called with **no format args**, this compiles to
     **`OP_SimpleMessage`** (12 bytes: `color(u32), string_id(u32),
     unknown8(u32)=0`), `zone/client.cpp:3803-3823`.
     `fizzle_msg = IsBardSong(spell_id) ? MISS_NOTE : SPELL_FIZZLE`
     (`zone/spells.cpp:322`).
  4. Nearby others get a separate close-range `OP_SimpleMessage`-shaped
     message via `entity_list.FilteredMessageCloseString(...,
     SPELL_FIZZLE_OTHER or MISSED_NOTE_OTHER, ..., GetName())` — this one DOES
     carry the caster's name as a format arg, so it is actually
     `OP_FormattedMessage`, not `OP_SimpleMessage` (`zone/spells.cpp:336-350`;
     `Client::MessageString` dispatches to `OP_FormattedMessage` when args are
     present, `zone/client.cpp:3831-3873`).
- eqstr ids (`zone/string_ids.h`): `SPELL_FIZZLE=173` ("Your spell
  fizzles!", line 69), `MISS_NOTE=180` (line 71), `SPELL_FIZZLE_OTHER=1218`
  (line 288), `MISSED_NOTE_OTHER=1219` (line 289).
- Opcodes: `OP_SimpleMessage=0x213f`, `OP_FormattedMessage=0x1024`,
  `OP_ManaChange=0x5467` (`utils/patches/patch_RoF2.conf:106,184,185`).
- **Distinguishing fizzle from interrupt on the wire (caster's own client):**
  fizzle = `OP_SimpleMessage{string_id=173 or 180}` + `OP_ManaChange`, with
  **no** `OP_InterruptCast` at all. A "true" interrupt = `OP_InterruptCast`
  (usually `messageid=439/INTERRUPT_SPELL`) + `OP_ManaChange`. These are
  reliably distinguishable by opcode alone (InterruptCast never fires on a
  chance-roll fizzle).

## 4. Cast completes successfully — no single "cast complete" opcode; inferred from a fixed tail sequence

There is **no explicit "spell cast succeeded" opcode.** Completion must be
inferred from the tail of `Mob::CastedSpellFinished` (`zone/spells.cpp:1405`,
success tail at `1760-1852`), reached only if the inner `SpellFinished()` call
(line 1744) returned true (i.e., the spell wasn't aborted for reasons like a
non-stacking buff — full **resists on detrimental spells still reach this
tail**, see §5).

Non-bard success tail (`zone/spells.cpp:1812-1839`):
1. `SendSpellBarEnable(spell_id)` (`zone/spells.cpp:1817`) → sends
   **`OP_ManaChange`** (`zone/spells.cpp:5752-5767`):
   `new_mana=GetMana(), spell_id=spell_id, stamina, keepcasting=0,
   slot=FindMemmedSpellBySpellID(spell_id)`. This is the primary "you may
   cast again" signal.
2. `c->MemorizeSpell(slot, spell_id, memSpellSpellbar, casting_spell_recast_adjust)`
   (`zone/spells.cpp:1824`) → **`OP_MemorizeSpell`**
   (`MemorizeSpell_Struct{slot, spell_id, scribing, reduction}`,
   `common/eq_packet_structs.h:415-420` = `rof2_structs.h:669-674`, byte
   identical, no ENCODE handler → pass-through). `scribing = memSpellSpellbar
   = 3` (enum in `zone/client.h:100-106`:
   `memSpellScribing=0, memSpellMemorize=1, memSpellForget=2,
   memSpellSpellbar=3`). **Yes — `OP_MemorizeSpell` with `scribing==3` (aka
   `memSpellSpellbar`) is the confirmed "spellbar/gem re-enabled" signal**,
   sent on both success (here) and — per bard-song branch — also for the
   bard-melody case (`zone/spells.cpp:1803`). Note it is *not itself* the
   success indicator on its own — it's also sent during interrupt/fizzle
   recovery in some paths — but combined with the *absence* of a preceding
   `OP_InterruptCast`/`OP_SimpleMessage(fizzle)` for that cast, it's a solid
   "casting ended, spellbar unlocked" marker.
3. `SetMana(GetMana())` (`zone/spells.cpp:1827`) — sends its own mana-update
   packet (not traced in depth here; effectively another `OP_ManaChange`-class
   update — **inferred**, not independently re-verified byte-for-byte in this
   pass).

**The actual "spell landed" visual/logical confirmation is `OP_Action`
sent twice + one `OP_Damage`:**
- First `OP_Action` (`Action_Struct.type=231` means "spell"; `effect_flag=0`)
  is sent immediately when targeting begins — to target, to **caster**
  (`if (IsClient()) CastToClient()->QueuePacket(action_packet);`,
  `zone/spells.cpp:4008-4010`), and to nearby others — this is just the cast
  animation, sent even if the spell later resists (`zone/spells.cpp:3957-4021`).
- A **second `OP_Action`** with `effect_flag = 0x04` ("success flag") is sent
  only if the spell actually landed / applied its effect
  (`zone/spells.cpp:4643-4679`, comment at 4643-4646: *"send the action packet
  again now that the spell is successful... the complete sequence is 2
  actions and 1 damage message"*). Also sent to the caster
  (`zone/spells.cpp:4676-4678`).
- One `OP_Damage` (`CombatDamage_Struct`, `damage=0` for non-damage spells) is
  sent right after, **except** for lifetap/AE-nuke/damage/BindAffinity spells
  which instead get their numeric damage via the normal combat-damage path
  (not traced here) (`zone/spells.cpp:4681-4707`).
- RoF2 wire structs for these: `ENCODE(OP_Action)` →
  `structs::ActionAlt_Struct` (`common/patches/rof2.cpp:220-249`);
  `ENCODE(OP_Damage)` → `structs::CombatDamage_Struct`
  (`common/patches/rof2.cpp:1123-1139`) — both explicitly re-encoded for RoF2,
  confirming the wire layout differs from the generic
  `common/eq_packet_structs.h` one (exact RoF2 byte offsets not extracted in
  this pass — only field presence/order via the `OUT()` calls).

**Recommendation for eqoxide:** treat "cast completed" as the observation of
`OP_ManaChange` (or `OP_MemorizeSpell{scribing==3}`) arriving for a
`spell_id` you are actively tracking as "casting", **without** an intervening
`OP_InterruptCast` or fizzle `OP_SimpleMessage` for that same cast attempt.
Treat the second `OP_Action{effect_flag & 0x04}` (or, more simply, an
`OP_Action` sequence of length 2 for the same spell/target) as "the spell
landed", separate from "casting mechanically finished".

## 5. Other cast-fail modes

| Failure | Where | Client-visible packets | Distinguishable? |
|---|---|---|---|
| Not enough mana | `Mob::DoSpellInterrupt`, `zone/spells.cpp:484-495`, called from `DoCastSpell:428` (before `OP_BeginCast` at 450 → **no BeginCast sent**) | `OP_SimpleMessage{INSUFFICIENT_MANA=199}` then `Mob::InterruptSpell()` → `OP_InterruptCast{messageid=439 generic}` + `OP_ManaChange` | Yes, via the preceding `OP_SimpleMessage{199}` — the InterruptCast messageid itself is generic |
| No target selected | `zone/spells.cpp:395-406` (before BeginCast) | `OP_SimpleMessage{SPELL_NEED_TAR=214}` + `OP_InterruptCast{439}` + `OP_ManaChange` | Yes, via `SimpleMessage{214}` |
| Silenced | `DoCastingChecksOnCaster`, `zone/spells.cpp:550-554` (before BeginCast, function just returns false — caller must interrupt; not independently confirmed which opcode fires here, **inferred** similar shape) | `OP_SimpleMessage{SILENCED_STRING=207}` confirmed sent; interrupt-packet emission at this exact call site not directly traced | eqstr 207 confirmed; opcode plumbing inferred |
| Recast timer not expired | `zone/spells.cpp:1418-1425` (after BeginCast already sent, inside `CastedSpellFinished`) | `OP_SimpleMessage{SPELL_RECAST=236}` + `StopCasting()` → `OP_ManaChange` (**no** `OP_InterruptCast` — same StopCasting-only pattern as fizzle) | Yes — `SimpleMessage{236}` + ManaChange, no InterruptCast |
| Already casting another spell | `zone/spells.cpp:1458-1464` | `OP_SimpleMessage{ALREADY_CASTING=12442}` + `InterruptSpell()` → `OP_InterruptCast{439}` + `OP_ManaChange` | Yes via SimpleMessage{12442} |
| Target resisted (detrimental spell doesn't land, casting itself still "succeeds") | `zone/spells.cpp:4448-4532` | Caster gets `OP_FormattedMessage{TARGET_RESISTED=425 or PHYSICAL_RESIST_FAIL=5817, arg=spell name}` (has a `%1` arg → FormattedMessage, not SimpleMessage); target gets `OP_FormattedMessage{YOU_RESIST=426}`; **no second `OP_Action{effect_flag=4}`**, only the first (animation-only) `OP_Action`; the normal success tail (`OP_ManaChange` + `OP_MemorizeSpell{scribing=3}`) **still runs** for non-buff detrimental spells (resist doesn't propagate a `false` up through `SpellFinished` unless it's a beneficial buff, `zone/spells.cpp:2590-2594`) | Yes — full resist = 1x `OP_Action` (no 2nd), formatted resist message, but casting still "completes" mechanically |
| Movement/damage interrupt (lost concentration) | `zone/spells.cpp:1500-1559` | `OP_InterruptCast{439}` + `OP_ManaChange` (standard interrupt path) | Same as generic interrupt |
| Regained concentration (didn't actually interrupt) | `zone/spells.cpp:1560-1570` | `OP_SimpleMessage{REGAIN_AND_CONTINUE=270}` to caster; no interrupt packets | Cast continues normally to its success tail |

eqstr ids used above, all from `zone/string_ids.h`: `INSUFFICIENT_MANA=199`
(78), `SPELL_NEED_TAR=214` (86), `SILENCED_STRING=207` (82),
`SPELL_RECAST=236` (93), `ALREADY_CASTING=12442` (566), `TARGET_RESISTED=425`
(166), `YOU_RESIST=426` (167), `PHYSICAL_RESIST_FAIL=5817` (427),
`INTERRUPT_SPELL=439` (177), `REGAIN_AND_CONTINUE=270` (113).

## 6. RoF2 opcode table values (`utils/patches/patch_RoF2.conf`)

```
OP_ManaChange=0x5467          (line 106)
OP_BeginCast=0x318f           (line 176)
OP_MemorizeSpell=0x217c       (line 179)
OP_CastSpell=0x1287           (line 182)
OP_InterruptCast=0x048c       (line 302)
OP_FormattedMessage=0x1024    (line 184)
OP_SimpleMessage=0x213f       (line 185)
OP_Action=0x744c              (line 212)
OP_Damage=0x6f15              (line 220)
```

Lightly corroborated in the RoF2 client binary itself
(`/home/dhenry/eq_assets/everquest_rof2/decompiled/capstone/eqgame.exe.asm`):
literal comparisons against `0x318f` (OP_BeginCast) and `0x5467`
(OP_ManaChange) appear in what looks like the opcode-dispatch switch around
`0x004c3fc8`/`0x004c4860` — consistent with the client treating these as
distinct handled opcodes (not deeply traced function-by-function; treat as
corroboration, not full confirmation).

## Wire-layout gotcha for eqoxide

`OP_BeginCast` is **10 bytes on the RoF2 wire**, not the 8-byte generic struct
seen in most emu-side code and in older (Titanium-era) client notes:
```
offset 0: u32 spell_id
offset 4: u16 caster_id
offset 6: u32 cast_time (ms)
```
(`common/patches/rof2_structs.h:720-726`, `common/patches/rof2.cpp:631-640`).
`OP_InterruptCast` (8 bytes: u32 spawnid, u32 messageid, + optional trailing
NUL-terminated name string for the "other players" broadcast variant) and
`OP_ManaChange` (20 bytes: u32 new_mana, u32 stamina, u32 spell_id, u8
keepcasting, u8 pad[3], i32 slot) and `OP_MemorizeSpell` (16 bytes: u32 slot,
u32 spell_id, u32 scribing, u32 reduction) are **byte-identical between the
generic and RoF2-wire struct definitions** and have no `ENCODE()` override in
`rof2.cpp`, so they pass through unmodified — safe to decode with the
`rof2_structs.h` layouts directly.
