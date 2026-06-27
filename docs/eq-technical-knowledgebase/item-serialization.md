# RoF2 Item Serialization — SerializeItem + OP_CharInventory

Confirmed against:
- `EQEmu/common/patches/rof2.cpp` — ENCODE(OP_CharInventory) and SerializeItem
- `EQEmu/common/patches/rof2_structs.h` — struct layouts (all under `#pragma pack(1)`)
- `EQEmu/common/patches/rof2_limits.h` — invtype/invslot/invbag/invaug constants
- `EQEmu/common/emu_constants.h` — EQ:: namespace re-exports (all point to RoF2 values)

---

## OP_CharInventory ENCODE

Source: `rof2.cpp:1043-1091`

**Wire format:**

1. `uint32` item_count (number of items to follow) — `rof2.cpp:1075`
2. N back-to-back serialized items, each produced by `SerializeItem(ob, inst, slot_id, 0, ItemPacketCharInventory)` — `rof2.cpp:1078`

No padding between items. If there are zero items the packet is 4 bytes of zeros (`rof2.cpp:1050-1055`).

The `item_count` is computed as `in->size / sizeof(EQ::InternalSerializedItem_Struct)` (`rof2.cpp:1061`).

---

## SerializeItem — Complete Byte Layout

Source: `rof2.cpp:6441-6927`

All structs are `#pragma pack(1)` (`rof2_structs.h:35-36`), so sizeof = sum of fields with no padding.

### 1. ItemSerializationHeader — 77 bytes

`rof2_structs.h:4733-4754`, written at `rof2.cpp:6498`

| Offset | Size | Field |
|--------|------|-------|
| 0      | 17   | `char unknown000[17]` — serial number string (16-char zero-padded + null) |
| 17     | 4    | `uint32 stacksize` |
| 21     | 4    | `uint32 unknown004` |
| 25     | 1    | `uint8 slot_type` — typePossessions=0, typeBank=1, typeSharedBank=2, typeMerchant=9, invalid=-1 |
| 26     | 2    | `uint16 main_slot` |
| 28     | 2    | `uint16 sub_slot` (0xffff for top-level items) |
| 30     | 2    | `uint16 aug_slot` (0xffff normally) |
| 32     | 4    | `uint32 price` |
| 36     | 4    | `uint32 merchant_slot` (1 for non-merchant items) |
| 40     | 4    | `uint32 scaled_value` |
| 44     | 4    | `uint32 instance_id` |
| 48     | 4    | `uint32 parcel_item_id` |
| 52     | 4    | `uint32 last_cast_time` |
| 56     | 4    | `uint32 charges` |
| 60     | 4    | `uint32 inst_nodrop` |
| 64     | 4    | `uint32 unknown044` |
| 68     | 4    | `uint32 unknown048` |
| 72     | 4    | `uint32 unknown052` |
| 76     | 1    | `uint8 isEvolving` |

**Total: 77 bytes**

### 2. [CONDITIONAL] EvolvingItem_Struct — 25 bytes

Written only if `item->EvolvingItem > 0` (`rof2.cpp:6500-6511`).
`rof2_structs.h:4756-4763`

Fields: `uint32 final_item_id`, `int32 evolve_level`, `double progress`, `uint8 activated`, `int32 evolve_max_level`, `uint8 unknown005[4]`

**Total: 25 bytes (conditional)**

### 3. Ornamentation — variable

Written at `rof2.cpp:6516-6537`:
- If `GetOrnamentationIDFile()` is set: `"IT%d\0"` (main hand) + `"IT%d\0"` (off hand)
- Otherwise: `"\0"` + `"\0"` (two single null bytes)

### 4. ItemSerializationHeaderFinish — 26 bytes

`rof2_structs.h:4765-4775`, written at `rof2.cpp:6550`

Fields: `uint32 ornamentIcon`, `int32 unknowna1` (0xffffffff), `uint32 ornamentHeroModel`, `int32 unknown063` (0), `uint8 Copied`, `int32 unknowna4` (0xffffffff), `int32 unknowna5` (0), `uint8 ItemClass`

**Total: 26 bytes**

### 5. Variable-length strings

Written at `rof2.cpp:6552-6565`:
- `item->Name` as C-string + null (always null even if empty, length > 0 check gates only the text)
- `item->Lore` as C-string + null
- `item->IDFile` as C-string + null
- One extra null byte (`"\0"`)

### 6. ItemBodyStruct — 255 bytes

`rof2_structs.h:4777-4863`, written at `rof2.cpp:6656`

Key fields (packed, full list in struct): id(u32), weight(i32), norent/nodrop/attune/size(u8x4), slots/price/icon(u32x3), unknown1/unknown2(u8x2), BenefitFlag(u32), tradeskills(u8), CR/DR/PR/MR/FR/SVCorruption/AStr/ASta/AAgi/ADex/ACha/AInt/AWis(i8x13), HP/Mana(i32x2), Endur(u32), AC/regen/mana_regen/end_regen(i32x4), Classes/Races/Deity(u32x3), SkillMod*(i32x3+u32), BaneDmg*(u32x3+i32), Magic(u8), CastTime_(i32), ReqLevel/RecLevel/RecSkill/BardType(u32x4), BardValue(i32), Light/Delay/ElemDmgType/ElemDmgAmt/Range(u8x5), Damage/Color/Prestige(u32x3), ItemType(u8), Material/MaterialUnknown1/EliteMaterial/HerosForgeModel/MaterialUnknown2(u32x5), SellRate(f32), CombatEffects through Accuracy(i32x9), CharmFileID(u32), FactionMod1/Amt1/../Mod4/Amt4(u32+i32 x4)

**Total: 255 bytes**

### 7. CharmFile string

`rof2.cpp:6659-6661`: `item->CharmFile` as C-string + null (null always written)

### 8. ItemSecondaryBodyStruct — 74 bytes

`rof2_structs.h:4872-4896`, written at `rof2.cpp:6690`

Contains: `uint32 augtype`, `int32 augrestrict2`, `uint32 augrestrict`, **6 x AugSlotStruct** (each 6 bytes = uint32 type + uint8 visible + uint8 unknown), ldon fields (5 x u32), bag fields (bagtype/bagslots/bagsize/wreduction as u8x4), book/booktype (u8x2)

**AugSlotStruct is 6 bytes; augslots[6] = 36 bytes. SOCKET_BEGIN=0, SOCKET_END=5 (6 sockets).**
**Augments are NOT recursive — their type info is embedded in the parent's `isbs.augslots[]`.**

**Total: 74 bytes**

### 9. Filename string

`rof2.cpp:6692-6694`: `item->Filename` as C-string + null

### 10. ItemTertiaryBodyStruct — 76 bytes

`rof2_structs.h:4898-4930`, written at `rof2.cpp:6736`

Fields include loregroup(i32), artifact/summonedflag(u8x2), favor(u32), fvnodrop(u8), dotshield/atk/haste/damage_shield(i32x4), guildfavor/augdistil(u32x2), unknown3(i32=0xffffffff), unknown4(u32), no_pet/unknown5(u8x2), potion_belt_enabled(u8), potion_belt_slots(u32), stacksize(u32), no_transfer(u8), expendablearrow(u16), unknown8-11(u32x4), unknown12-14(u8x3)

**Total: 76 bytes**

### 11. Effect blocks — 5 blocks total

Each block is: **fixed struct** + **effect name C-string** + **`int32` unknown (0)**

All written sequentially at `rof2.cpp:6738-6848`.

| Order | Variable | Struct | Struct Size | Name field | +int32 |
|-------|----------|--------|-------------|------------|--------|
| 1 | ices | `ClickEffectStruct` | 30 bytes | `item->ClickName` | clickunk7 (`rof2.cpp:6759`) |
| 2 | ipes | `ProcEffectStruct`  | 30 bytes | `item->ProcName`  | unknown5 (`rof2.cpp:6776`) |
| 3 | iwes | `WornEffectStruct`  | 30 bytes | `item->WornName`  | unknown6 (`rof2.cpp:6792`) |
| 4 | ifes | `WornEffectStruct`  | 30 bytes | `item->FocusName` | unknown6 (`rof2.cpp:6808`) |
| 5 | ises | `WornEffectStruct`  | 30 bytes | `item->ScrollName`| unknown6 (`rof2.cpp:6824`) |

Bard effect (ibes) also uses `WornEffectStruct` (30 bytes) but the name is always `"\0"` (the ClickName branch is commented out — `rof2.cpp:6838-6845`), followed by `int32` unknown6 (`rof2.cpp:6847`).

**6 effect structs total (Click, Proc, Worn, Focus, Scroll, Bard), each 30 bytes + C-string name + 4-byte int32 = (30 + name_len + 1 + 4) per block.**

- `ClickEffectStruct` (`rof2_structs.h:4932-4945`): effect(i32), level2(u8), type(u32), level(u8), max_charges(i32), cast_time(i32), recast(u32), recast_type(i32), clickunk5(u32) — **30 bytes**
- `ProcEffectStruct` (`rof2_structs.h:4947-4960`): effect(i32), level2(u8), type(u32), level(u8), unknown1-4(u32x4), procrate(u32) — **30 bytes**
- `WornEffectStruct` (`rof2_structs.h:4962-4975`): effect(i32), level2(u8), type(u32), level(u8), unknown1-5(u32x5) — **30 bytes**

### 12. ItemQuaternaryBodyStruct — 171 bytes

`rof2_structs.h:4977-5039`, written at `rof2.cpp:6892`

Notable fields: scriptfileid(u32), quest_item(u8), Power(u32), Purity(u32), unknown16(u8), BackstabDmg/DSMitigation(u32x2), heroic stats (13 x i32), HealAmt/SpellDmg/Clairvoyance/SubType(i32x4), various unknown bytes/uint16/uint32/float fields, NoZone/NoGround(u8), **unknown37a/unknown38/unknown39** (u8x3 — last 3 bytes, new to RoF2).

Key sentinel: `unknown29 = (packet_type == ItemPacketInvalid ? 0xFF : 0)`, `unknown39 = (packet_type == ItemPacketInvalid ? 0 : 1)` (`rof2.cpp:6884, 6890`).

**Total: 171 bytes**

### 13. Sub-item count + sub-items (bag contents)

`rof2.cpp:6894-6926`:

- `uint32 subitem_count` placeholder written first (at saved position `count_pos`)
- For each sub-item present (`inst->GetItem(index)` for index 0..199):
  - `uint32 index` (bag slot index, 0-based) — `rof2.cpp:6919`
  - Recursive `SerializeItem(ob, sub, SubSlotNumber, depth+1, packet_type)` — `rof2.cpp:6921`
- `subitem_count` is back-patched via `ob.overwrite(count_pos, ...)` — `rof2.cpp:6925-6926`

**Depth parameter**: passed and incremented but there is NO depth guard in the RoF2 encoder — the parameter is tracked but never checked (`rof2.cpp:6441, 6921`). Recursion terminates naturally because sub-items in practice only go one level deep (bags don't contain bags).

---

## Slot Mapping — ServerToRoF2Slot

Source: `rof2.cpp:6930-7017`, `rof2_limits.h:27-252`, `emu_constants.h:100-251`

### Key constants (all from RoF2 namespace, used as EQ:: via `using`)

- `IINVALID = -1`, `INULL = 0` (`rof2_limits.h:27-28`)
- `typePossessions = 0`, `typeBank = 1`, `typeSharedBank = 2`, `typeTrade = 3`, `typeWorld = 4`, `typeTribute = 6`, `typeGuildTribute = 8`, `typeMerchant = 9`, `TYPE_INVALID = -1` (`rof2_limits.h:46-73, 107`)
- `POSSESSIONS_SIZE = 34` — so server slots 0..33 map directly to RoF2 possessions type (`rof2_limits.h:77`)

### InventorySlot_Struct wire layout (12 bytes, packed)

`rof2_structs.h:57-66`:
- int16 Type (0=possessions, 1=bank, 2=shared bank, 9=merchant, -1=invalid)
- int16 Unknown02
- int16 Slot
- int16 SubIndex
- int16 AugIndex (0xffff = no augment)
- int16 Unknown01

### RoF2 equipment slot numbers (possessions type, direct pass-through)

For server slots 0..33, `ServerToRoF2Slot` sets `Type=typePossessions(0)`, `Slot=server_slot` (`rof2.cpp:6942-6944`).

The EQ:: invslot enum IS the RoF2 enum (`emu_constants.h:156` — `using namespace RoF2::invslot::enum_`):

| Slot Name | RoF2 Wire Slot# | Notes |
|-----------|-----------------|-------|
| slotCharm | 0 | |
| slotEar1  | 1 | |
| slotHead  | 2 | "helm" = 2, NOT 1 |
| slotFace  | 3 | |
| slotEar2  | 4 | |
| slotNeck  | 5 | |
| slotShoulders | 6 | |
| slotArms  | 7 | |
| slotBack  | 8 | |
| slotWrist1 | 9 | |
| slotWrist2 | 10 | |
| slotRange | 11 | |
| slotHands | 12 | |
| slotPrimary | 13 | |
| slotSecondary | 14 | |
| slotFinger1 | 15 | |
| slotFinger2 | 16 | |
| slotChest | 17 | "chest" = 17, NOT 2 |
| slotLegs  | 18 | |
| slotFeet  | 19 | |
| slotWaist | 20 | |
| slotPowerSource | 21 | |
| slotAmmo  | 22 | |
| slotGeneral1 | 23 | |
| slotGeneral2 | 24 | |
| ... | ... | |
| slotGeneral10 | 32 | |
| slotCursor | 33 | |

**The caller's claim that "helm=slot 1, chest=2, arms=3, wrists=4, hands=5, legs=6, feet=7" is Titanium-era numbering — WRONG for RoF2.**

### Server slot ranges for non-possessions types

All from `emu_constants.h:215-251` (derived from RoF2 limits, SLOT_COUNT=200):

| Range | EQ server slots | RoF2 Type | RoF2 Slot |
|-------|-----------------|-----------|-----------|
| Equipment/General/Cursor | 0–33 | typePossessions (0) | = server slot |
| Bank slots | 2000–2023 | typeBank (1) | server_slot - 2000 |
| Shared bank | 2500–2501 | typeSharedBank (2) | server_slot - 2500 |
| Tribute | 400–404 | typeTribute (6) | server_slot - 400 |
| Guild tribute | 450–451 | typeGuildTribute (8) | server_slot - 450 |
| World | 4000–4009 | typeWorld (4) | server_slot - 4000 |
| General bag contents | 4010–6009 | typePossessions | Slot=23+(idx/200), Sub=idx%200 |
| Cursor bag contents | 6010–6209 | typePossessions | Slot=33, Sub=idx |
| Bank bag contents | 6210–11009 | typeBank | Slot=idx/200, Sub=idx%200 |
| Shared bank bag contents | 11010–11409 | typeSharedBank | similar |

### Augment slots in InventorySlot_Struct

`AugIndex` field: set to the augment socket index (0-5) or `invaug::SOCKET_INVALID (-1)` for non-augment context. (`rof2_limits.h:245-247`: SOCKET_INVALID=-1, SOCKET_BEGIN=0, SOCKET_END=5, SOCKET_COUNT=6)
