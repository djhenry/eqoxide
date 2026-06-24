# Merchant Trading — Faction Gates, Sell Packet Layout, and "Misplaced Item" Error

Sources confirmed 2026-06-24.

---

## Faction threshold for merchant access (OP_ShopRequest)

The faction gate is enforced **only in OP_ShopRequest**, not in OP_ShopPlayerSell or OP_ShopPlayerBuy.

`Handle_OP_ShopRequest` (`EQEmu/zone/client_packet.cpp:14648–14656`):

```cpp
int primaryfaction = tmp->CastToNPC()->GetPrimaryFaction();
int factionlvl     = GetFactionLevel(
    CharacterID(), tmp->CastToNPC()->GetNPCTypeID(), GetRace(), GetClass(), GetDeity(),
    primaryfaction, tmp
);
if (factionlvl >= 7) {
    MerchantRejectMessage(tmp, primaryfaction);
    action = MerchantActions::Close;
}
```

`GetFactionLevel` returns a `FACTION_VALUE` enum (`EQEmu/common/faction.h:26–35`):

| Value | Enum                   | Numeric score range (defaults) |
|-------|------------------------|-------------------------------|
| 1     | FACTION_ALLY           | >= 1100                       |
| 2     | FACTION_WARMLY         | >= 750                        |
| 3     | FACTION_KINDLY         | >= 500                        |
| 4     | FACTION_AMIABLY        | >= 100                        |
| 5     | FACTION_INDIFFERENTLY  | >= 0                          |
| 6     | FACTION_APPREHENSIVELY | >= -100                       |
| 7     | FACTION_DUBIOUSLY      | >= -500                       |
| 8     | FACTION_THREATENINGLY  | >= -750                       |
| 9     | FACTION_SCOWLS         | < -750                        |

Rule defaults from `EQEmu/common/ruletypes.h:1064–1071`.

**Gate: `factionlvl >= 7` = DUBIOUSLY or worse blocks merchant window opening.**

Threshold: you need **APPREHENSIVELY (6) or better** to open the merchant window.
Both buying FROM and selling TO the merchant are blocked at the same threshold — there is no separate, lower sell threshold. The merchant window simply doesn't open.

### Merchant fix in GetFactionLevel

`EQEmu/zone/client.cpp:8233–8234`:

```cpp
if (tnpc && tnpc->IsNPC() && tnpc->CastToNPC()->MerchantType && (fac == FACTION_THREATENINGLY || fac == FACTION_SCOWLS))
    fac = FACTION_DUBIOUSLY;
```

For merchant NPCs, THREATENING and SCOWLS are both clamped to DUBIOUSLY before the value is returned. **This does not help** — DUBIOUSLY is still >= 7 and still blocked. The effect of this clamp is only to prevent merchants from being reported as THREATENING/SCOWLS to the caller (affects /con display), not to allow trade.

---

## MerchantRejectMessage — what the player sees

`EQEmu/zone/client.cpp:8465–8531`. When blocked (faction >= 7), the merchant NPC says one of these messages (chosen by what caused the lowest faction modifier — base deeds, race, class, or deity):

- **Deed-based** (faction hit or deity): random from WONT_SELL_DEEDS1–6 (string IDs 1166–1171), e.g. "Creatures like you make me sick..the things you do..get out of here Pagan!", "Get out of here now!"
- **Race-based** (player race): WONT_SELL_RACE1–4 (1154, 1161–1163), e.g. "Don't you [Race] have your own merchants?"
- **Class-based**: WONT_SELL_CLASS1–5 (1155–1159), e.g. "I don't have anything to do with [Class]..move along."
- **Non-standard race** (illusioned): WONT_SELL_NONSTDRACE1–3 (1160, 1164–1165)

No "I don't want to do business with you" literal string — the actual text is one of the above. The merchant window sends back `command = MerchantActions::Close` so the client window does not open.

---

## OP_ShopPlayerSell — "You seem to have misplaced that item.."

`EQEmu/zone/client_packet.cpp:14392–14401`:

```cpp
uint32 itemid = GetItemIDAt(mp->itemslot);
if (itemid == 0)
    return;  // silent return if slot is totally empty

const EQ::ItemData* item = database.GetItem(itemid);
EQ::ItemInstance* inst = GetInv().GetItem(mp->itemslot);
if (!item || !inst) {
    Message(Chat::Red, "You seem to have misplaced that item..");
    return;
}
```

`GetItemIDAt` (`EQEmu/zone/inventory.cpp:857`) does a bitmask check for valid slot ranges and then looks up `m_inv[slot_id]`. It returns `INVALID_ID` (0) if the slot is out of range or empty.

**"You seem to have misplaced that item.." means the `mp->itemslot` value sent in the Titanium `Merchant_Purchase_Struct` does not map to a valid item in the server's inventory model for that character.** Specifically:
- `GetItemIDAt(mp->itemslot)` returned a non-zero ID (so the item exists by ID), but either `database.GetItem(itemid)` returned null OR `GetInv().GetItem(mp->itemslot)` returned null.
- The most common cause in a reimplementation: the slot number in `mp->itemslot` is wrong — using a client-side slot number that does not match the EQEmu server-side inventory slot numbering.

Note: there is a **known off-by-one** between client wire slot and DB slot for some ranges (see `eq-newchar-and-quest-api-gaps.md` in memory). The Titanium client sends `itemslot` as an inventory slot number (e.g. primary=13, secondary=14, general1=22, etc.); the server maps this directly through `GetInv()[slot_id]`. If the client sends slot+1 or slot-1 relative to what EQEmu expects, the item won't be found.

---

## Does KOS faction reach OP_ShopPlayerSell?

**No.** The flow is:

1. Player right-clicks merchant → client sends **OP_ShopRequest**.
2. Server checks faction in `Handle_OP_ShopRequest`. If factionlvl >= 7 (DUBIOUSLY+), `MerchantRejectMessage` fires, response `command = MerchantActions::Close` is sent, and `BulkSendMerchantInventory` is **not** called. Merchant window does not open.
3. If the window never opened, the client should not be sending OP_ShopPlayerSell.
4. If the client sends OP_ShopPlayerSell anyway (e.g. a reimplementation bug), `Handle_OP_ShopPlayerSell` (`client_packet.cpp:14374`) has **no faction check at all**. It only checks: NPC is a merchant, player is in range, item slot resolves. A KOS-blocked merchant would still "accept" a raw sell packet if the slot is correct — the faction check was only in ShopRequest.

**Conclusion**: If a KOS merchant generates "You seem to have misplaced that item..", it is NOT the faction causing that specific message. The faction already blocked the window open. The misplaced-item message is a slot resolution failure — the `mp->itemslot` in the sell packet does not map to a real item instance on the server.

---

## Level gating

`Handle_OP_ShopPlayerBuy` (`client_packet.cpp:14162`): when iterating the merchant's item list, items with `level_required > player level` are skipped (not shown/bought). This is a per-item filter, not a blanket level gate on the merchant itself.

There is no player-level gate on OP_ShopPlayerSell.
There is no race or deity gate on OP_ShopPlayerSell or OP_ShopPlayerBuy themselves — those filters only affect OP_ShopRequest via the faction calculation (which incorporates race/deity modifiers into the faction score).

---

## Titanium wire struct: Merchant_Purchase_Struct

`EQEmu/common/patches/titanium_structs.h:1710–1715`:

```c
struct Merchant_Purchase_Struct {
/*000*/ uint32 npcid;      // Merchant NPC entity id
/*004*/ uint32 itemslot;   // Player's inventory slot holding the item being sold
/*008*/ uint32 quantity;
/*012*/ uint32 price;
};
```

Total size: 16 bytes. Used for both the client→server sell request and the server→client sell confirmation.

---

## Qeynos merchant faction — Wood Elf Ranger baseline

Qeynos merchants (e.g. Tanlyn Galliway, class 41) use the **Merchants of Qeynos** faction (or equivalent Qeynos-aligned faction). A freshly DB-created Wood Elf Ranger has default faction values. Wood Elves are naturally allied with good-aligned human factions (Qeynos), so a default character should be AMIABLY or KINDLY, not KOS — unless the DB-created character had faction values explicitly zeroed or set negative, or the NPC's primary faction is different from the standard Qeynos Merchant faction.

If the NPC cons "scowls at you, ready to attack", one of these is true:
1. The character's faction value for that NPC's primary faction was set very negative in the DB (< -750 before modifiers, or after race/class mods reaches SCOWLS).
2. The NPC's primary faction is not Merchants of Qeynos but something the Wood Elf has negative standing with.
3. The NPC has `CheckAggro` returning true, which also forces THREATENINGLY (but is then clamped to DUBIOUSLY for merchants).

To trade with Tanlyn Galliway, the character needs the effective faction score after base+class_mod+race_mod+deity_mod to be >= -100 (APPREHENSIVELY threshold, numeric value 6 or better).

---

## Related files

- `EQEmu/zone/client_packet.cpp` — ShopRequest:14589, ShopPlayerSell:14374, ShopPlayerBuy:14126
- `EQEmu/zone/client.cpp` — GetFactionLevel:8187, MerchantRejectMessage:8465
- `EQEmu/common/faction.h` — FACTION_VALUE enum:26
- `EQEmu/common/faction.cpp` — CalculateFaction:57
- `EQEmu/common/ruletypes.h` — faction threshold defaults:1064–1071
- `EQEmu/zone/string_ids.h` — WONT_SELL_* strings:265–282
- `EQEmu/common/patches/titanium_structs.h` — Merchant_Purchase_Struct:1710
- `EQEmu/zone/inventory.cpp` — GetItemIDAt:857
