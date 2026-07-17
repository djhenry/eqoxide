# Loot protocol (RoF2): OP_LootRequest / OP_MoneyOnCorpse / OP_EndLootRequest

## OP_LootRequest (client -> server)

Payload is a bare `uint32` corpse spawn_id, no wrapper struct.
`EQEmu/zone/client_packet.cpp:10302-10328` (`Handle_OP_LootRequest`):
validates `app->size == sizeof(uint32)`, resolves via `entity_list.GetID(*(uint32*)pBuffer)`,
calls `SetLooting(ent->GetID())` (a single-slot field on the *Client* — see below), then
`Corpse::MakeLootRequestPackets(this, app)`.

## moneyOnCorpseStruct — confirmed 20 bytes, NO corpse/entity id anywhere

`EQEmu/common/patches/rof2_structs.h:2041-2056` (RoF2 patch copy, byte-identical to the
generic `EQEmu/common/eq_packet_structs.h:1823-1841` copy; no ENCODE/DECODE override for
`OP_MoneyOnCorpse` exists in `EQEmu/common/patches/rof2.cpp` — grepped, zero hits — so the
struct is sent verbatim, no translation layer):

```
struct moneyOnCorpseStruct {
/*0000*/ uint8  response;   // LootResponse enum (see below)
/*0001*/ uint8  unknown1;   // magic const, NOT per-corpse (0x5a on error path, 0x42 on success path)
/*0002*/ uint8  unknown2;   // magic const, NOT per-corpse (0x40 on error path, 0xef on success path)
/*0003*/ uint8  unknown3;   // always 0 — true pad
/*0004*/ uint32 platinum;
/*0008*/ uint32 gold;
/*0012*/ uint32 silver;
/*0016*/ uint32 copper;
};                          // sizeof == 20 (the "Length: 22 Bytes" comment in the header is stale/wrong)
```

Opcode: `OP_MoneyOnCorpse = 0x5f44` on RoF2 (`EQEmu/utils/patches/patch_RoF2.conf:188`).

`unknown1`/`unknown2` are **not** correlation data — they're fixed per response-path
constants written inline at each call site, not per-corpse values:
- Error/refuse path: `Corpse::SendLootReqErrorPacket` writes `0x5a`/`0x40` —
  `EQEmu/zone/corpse.cpp:59-64` and reused at `corpse.cpp:1147,1159,1170,1175` (called with
  `LootResponse::SomeoneElse`/`TooFar`/`NotAtThisTime`).
- Success/GM-peek path: writes `0x42`/`0xef` — `EQEmu/zone/corpse.cpp:1258-1263` (GM peek) and
  `corpse.cpp:1276-1281` (normal accept).

**Confirmed: the 20-byte `OP_MoneyOnCorpse` reply carries zero corpse/entity/spawn id in
any field.** There is no way to look at a lone `OP_MoneyOnCorpse` packet and determine which
corpse it answers.

`LootResponse` enum values (`EQEmu/zone/common.h:172-180`): `SomeoneElse=0, Normal=1,
NotAtThisTime=2, Normal2=3, Hostiles=4, TooFar=5, LootAll=6` (SoD+).

## Server does NOT serialize loot sessions per-client — only per-corpse, and only against OTHER clients

This is the load-bearing finding for issue #414 (misattributed late `OP_MoneyOnCorpse`).

- The lock lives on the **Corpse**, not the Client: `Corpse::m_being_looted_by_entity_id`
  (`EQEmu/zone/corpse.h`, set at `EQEmu/zone/corpse.cpp:1229`, initialized/reset to
  `0xFFFFFFFF` at `corpse.cpp:143,279,580,1795`).
- `Corpse::MakeLootRequestPackets` (`EQEmu/zone/corpse.cpp:1139-1231`) only refuses
  (`LootResponse::SomeoneElse`) when **a different** client currently holds *that specific
  corpse's* lock (`corpse.cpp:1174-1177`: `m_being_looted_by_entity_id != 0xFFFFFFFF &&
  m_being_looted_by_entity_id != c->GetID()`). It never checks whether `c` already has an
  open session on some *other* corpse.
- The Client-side field that superficially looks like session tracking,
  `Mob::entity_id_being_looted` / `SetLooting()`/`IsLooting()`
  (`EQEmu/zone/mob.h:1292-1293,1547`), is a **single slot that is unconditionally overwritten**
  on every `OP_LootRequest` (`EQEmu/zone/client_packet.cpp:10317`), with **no check against a
  prior value** and **no side effect on the previous corpse's lock**. It is used for exactly
  two things, neither of which is request serialization: auto `EndLoot()` cleanup on
  zone-transfer (`EQEmu/zone/zoning.cpp:923-936`) and an admin/API debug field
  (`EQEmu/zone/api_service.cpp:495`).
- Net effect: a client can send `OP_LootRequest` for corpse B while corpse A's lock is still
  held by that same client's entity id — the server **accepts** it (assuming B is otherwise
  lootable) and does nothing to release A. A's lock then sits open indefinitely.

**No time-based expiry of a corpse's loot lock exists.** It is cleared only by:
1. Explicit `OP_EndLootRequest` for *that* corpse → `Corpse::EndLoot`
   (`EQEmu/zone/corpse.cpp:1787-1802`) unconditionally sets
   `m_being_looted_by_entity_id = 0xFFFFFFFF` at line 1795 and replies `OP_LootComplete`
   (0-byte payload). Note `EndLoot` does **not** check `IsBeingLootedBy(c)` first — any client
   naming the corpse can release its lock. Safe/idempotent to call speculatively.
2. `ResetLooter()` calls inside `OP_LootItem` handling error branches, guarded by
   `IsBeingLootedBy(c)` (`corpse.cpp:1439-1440,1449-1450,1461-1462,1493,1536,1549`).
3. A lazy self-heal check at the *top* of the next `MakeLootRequestPackets` call — if the
   entity that holds the lock no longer exists in `entity_list` (logged off/zoned/despawned),
   the lock is cleared to `0xFFFFFFFF` (`corpse.cpp:1164-1167`). This is not a timer; it only
   fires when someone next tries to loot that corpse.
4. Corpse destruction/decay (constructor/reset paths reinit the field).

`Corpse::m_loot_cooldown_timer` (`corpse.cpp:136` `SetTimer(10)`, checked at
`corpse.cpp:1445` inside `LootCorpseItem`) is an unrelated **per-item pull rate limiter** on
`OP_LootItem`, not a session/request-serialization mechanism — don't conflate the two.

## No other correlator exists in the loot flow

- `LootingItem_Struct` (used only by `OP_LootItem`, not the `OP_LootRequest`/
  `OP_MoneyOnCorpse` accept/refuse handshake) does carry a `lootee` corpse id at offset 0
  (`EQEmu/common/patches/rof2_structs.h:2058-2059`), but that opcode only fires after the
  loot window is already open and an item is clicked — it can't disambiguate the ack itself.
- `opcode_dispatch.h:386` comment `// follows OP_LootRequest` on `OP_MoneyOnCorpse` documents
  an *assumed* UI-flow ordering from the original client (which only ever has one loot window
  open at a time, so it never races itself), not a wire-level or server-enforced guarantee.
  Given #414's premise is exactly "reordering via retransmit delay," relying on send-order
  == receive-order is unsound regardless.

## Conclusion for #414

`OP_MoneyOnCorpse` is genuinely uncorrelatable to a corpse at the protocol level — no field,
no session id, no nonce. Server-side "serialization" does not exist for the same client
(only blocks a *different* client from stealing an already-locked corpse). eqoxide cannot
fix this by parsing the packet harder; the fix has to be structural on the client side:
never allow a second `OP_LootRequest` to go out while a prior one's fate (accept/refuse OR
a defensive close) hasn't been resolved, and proactively send `OP_EndLootRequest` for an
abandoned corpse (safe per point 1 above — no ownership check) before moving on, rather than
just clearing local state and hoping the late ack never arrives.

### Implemented (eqoxide fix)

Both `apply_money_on_corpse` and `apply_loot_complete` (`src/eq_net/packet_handler.rs`) now
gate on `GameState`'s own request-state flags (`loot_session_active` / `loot_confirmed` /
`loot_defensive_close_at`) rather than applying an inbound ack unconditionally. On
`OpenTimedOut`/`TimedOut` the gameplay loop (`src/eq_net/gameplay.rs`) sends a defensive/
idempotent `OP_EndLootRequest` for the abandoned corpse and withholds the next corpse's
`OP_LootRequest` until that resolves (`loot_defensive_close_at`), narrowing — not eliminating,
per the "no field, no session id, no nonce" fact above — the window in which a genuinely-late
ack for an abandoned corpse could land on a different, later session.

Related: `docs/eq-technical-knowledgebase/eqstream-reliable-retransmit.md` (why a late-but-
not-lost ack is possible in the first place).
