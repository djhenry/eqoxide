# Zone-line crossing (walking between zones): trigger, index, and OP_ZoneChange

## Two completely separate coordinate sets — do not conflate them

`zone_points` DB row / `ZonePoint` struct carries BOTH:
- **Trigger coords** `x,y,z,heading` — where the player must physically stand *in
  the current zone* to cross. Encoded client-side only in the zone's WLD BSP
  region geometry (never sent over the wire).
- **Destination coords** `target_x,target_y,target_z,target_heading,target_zone_id` —
  where they land *in the new zone*. This (and only this) is what
  `OP_SendZonepoints` transmits to the client.

`OP_SendZonepoints` NEVER contains trigger coordinates. eqoxide's prior bug was
walking toward the `OP_SendZonepoints` x/y/z as if it were the trigger — that's
actually the arrival point in the *other* zone.

## Server side (confirmed, EQEmu RoF2)

- `ZonePoint` struct: `EQEmu/zone/zone.h:61-77` — `x/y/z/heading` (trigger),
  `number` (index), `target_x/y/z/target_heading/target_zone_id/target_zone_instance`
  (destination).
- DB load: `EQEmu/zone/zone.cpp:2137-2192` (`ZoneDatabase::LoadStaticZonePoints`,
  query `SELECT ... FROM zone_points WHERE zone=... ORDER BY number`).
- `OP_SendZonepoints` build: `EQEmu/zone/client.cpp:6937-6994`
  (`Client::SendZonePoints`). Key line: `zp->zpe[i].iterator = data->number;`
  then `zp->zpe[i].x/y/z/heading/zoneid/zoneinstance = data->target_*` —
  **the wire packet carries `number` as `iterator`, and only the destination
  fields**, never the trigger x/y/z.
- Wire structs (RoF2): `ZonePoint_Entry` (32B) and `ZonePoints` —
  `EQEmu/common/patches/rof2_structs.h:2530-2547`.
- `OP_ZoneChange` (client→server) wire struct, RoF2, 100 bytes:
  `EQEmu/common/patches/rof2_structs.h:1425-1438`
  (`char_name[64], zoneID, instanceID, Unknown068, Unknown072, y, x, z,
  zone_reason, success, Unknown096`). Note float order is **y, x, z** (not x,y,z).
- Handler: `Client::Handle_OP_ZoneChange`, `EQEmu/zone/zoning.cpp:38` onward.
  Registered at `EQEmu/zone/client_packet.cpp:416`.
- `ZoneMode` enum: `EQEmu/zone/client.h:109-119`. `ZoneUnsolicited` = "client
  came up with this on its own" — this is the mode used for an organic
  zone-line walk (as opposed to GM summon / gate / evac / portal-spell, which
  are all server-solicited). It's the mode the server is in by default/after
  reset: `EQEmu/zone/zoning.cpp:440,475,566,1066` all reset to
  `ZoneUnsolicited` after handling other zone events.
- **Trigger resolution is NOT based on the incoming packet's x/y/z.** For
  `ZoneUnsolicited`, the server calls
  `zone->GetClosestZonePoint(glm::vec3(GetPosition()), target_zone_id, this, ZONEPOINT_ZONE_RANGE)`
  — `GetPosition()` is the mob's own server-tracked position (kept current via
  ordinary movement/`OP_ClientUpdate` packets), **not** `zc->x/y/z` from the
  `ZoneChange_Struct` itself. `EQEmu/zone/zoning.cpp:138`. If no `zone_point`
  with matching `target_zone_id` is found nearby (or the found one targets a
  different zone than `zc->zoneID` claims), it's flagged a cheat
  (`MQZone`/`MQGate`) and `SendZoneCancel` is sent: `EQEmu/zone/zoning.cpp:127-146`.
  → **Confirms**: the server relies on the client having actually walked to
  the real WLD-encoded trigger region; `zc->zoneID` only needs to name the
  *target zone*, the *position* used for matching is the server's own
  tracked position, and `ZONEPOINT_ZONE_RANGE`/`ZONEPOINT_NOZONE_RANGE` are
  both `40000.0f` (`EQEmu/zone/common.h:62,64`) — effectively very permissive;
  in practice the closest zone_point targeting the right zone always wins,
  there's just a >400-unit distance warning/cheat-flag heuristic
  (`EQEmu/zone/zone.cpp:2067-2076`), not a hard reject.
- Arrival position actually used comes from the matched `zone_point`'s
  **destination** fields, with a documented sentinel:
  `target_x/y/z == 999999` or `target_heading == 999` means "keep the
  player's current position/heading" (used for e.g. same-instance zone
  lines). `EQEmu/zone/zoning.cpp:305-317` — comment: *"999999 is a placeholder
  for 'same as where they were from'"*.
- Safe-coords fallback (`zone_data->safe_x/y/z/heading`) is used only for
  `ZoneToSafeCoords`/`EvacToSafeCoords` modes (evac/succor), not the normal
  walk-across-a-zone-line path (`ZoneUnsolicited`) — that path *requires* a
  real `zone_point` match or it's rejected outright
  (`EQEmu/zone/zoning.cpp:317-325`, cheat detected + `SendZoneCancel`).
  So: if eqoxide's walker is not standing near the DB trigger coords, the
  server does NOT silently fall back to safe coords for an unsolicited
  zone-line request — it cancels the zone (this matches the reported bug
  symptom of "safe-coords-fallback the arrival", which likely happens via a
  *different* code path/zone_mode than `ZoneUnsolicited`, or via the eventual
  `GetClosestZonePointWithoutZone` call when `zc->zoneID==0`,
  `zoning.cpp:100`).

## Client side (RoF2 eqgame.exe, decompiled, confirmed)

- **Region-flag string parser**: `FUN_00487530` @
  `everquest_rof2/decompiled/ghidra/eqgame.exe.c:96178-96301`. This decodes the
  ASCII "region type" name string that WLD BSP Region fragments (frag 0x29)
  carry, into a 32-bit flag word. Registered as the engine's region-type
  translator callback during world/display init:
  `(**(code **)(*DAT_015d46a8 + 0x2c))(FUN_00487530);` —
  `eqgame.exe.c:106052`.
- **Format, confirmed by decompile** (`eqgame.exe.c:96222-96296`):
  - bytes[0..2) = 2-letter terrain-type prefix: `"DR"` (normal/drop), `"WT"`
    (water), `"LA"` (lava), `"SL"` (slime), `"VW"` (underwater-vision water),
    `"W2"`/`"W3"` (extra water tiers).
  - byte[2] == `'P'` → sets bit `0x40000000` (a PvP-ish region flag).
  - bytes[3..5) == `"TP"` → sets bit `0x80000000`. **This is the zone-line
    flag.** (i.e. full prefix looks like `"DRNTP"`, `"WTNTP"`, etc. — the type
    code, then `N`, then `TP` for teleport.)
  - byte[0x1f]==`'M'`→`0x20000000`, byte[0x20]==`'S'`→`0x10000000`,
    byte[0x21]==`'P'`→`0x8000000`(returns immediately), byte[0x21]==`'F'`→
    `0x4000000` — extra suffix flags, only read if the string is long enough
    (`length > 0x21`).
  - There's a short "Area" form too (`"AWT"`, `"ALV"`, `"AVW"`, `"APK"`,
    `"ATP<N>"`, `"ASL"`): `"ATP<N>"` is expanded via
    `sprintf(buf, "DRNTP00255%05d000000000000000", N)` — `eqgame.exe.c:96211-96215`.
    This is direct evidence for the **canonical long-form zone-line region
    name literally being `"DRNTP00255NNNNN000000000000000"`**, where `NNNNN`
    is the zone-point index, zero-padded to 5 digits, i.e. numerically
    identical to `zone_points.number` / `ZonePoint_Entry.iterator`. `"00255"`
    is a fixed literal (not yet decoded — possibly a legacy default
    radius/height code); the trailing zeros are placeholder bytes for the
    optional M/S/P/F suffix flags described above.
  - **The region string therefore only ever encodes an INDEX, never a
    destination.** There is no zone id, no target x/y/z in the WLD at all.
- **Per-region flag storage + zone-line test helper**: each BSP region's
  parsed flag word is cached at offset `+0x198` of its per-region struct; a
  family of `__thiscall` bit-test helpers read it —
  `FUN_007dc710(region_container, region_index)` tests bit `0x80000000`
  (zone-line): `eqgame.exe.c:633252-633269`. Sibling helpers test the other
  bits (`0x1000000`, `0x2000000`, `0x10000000`, `0x400000`, etc. — water/lava/
  slime/other-terrain-type tests) at `eqgame.exe.c:633178-633334`.
- **Not fully traced (time-boxed out) — inferred, not confirmed instruction-
  by-instruction**: the exact per-tick code that (a) determines which BSP
  region the player's current position falls in, (b) calls the zone-line
  test above, (c) re-parses/extracts the embedded `NNNNN` digits from that
  region's raw name string, (d) matches `NNNNN` against the `iterator` field
  of the `ZonePoint_Entry[]` array received earlier via `OP_SendZonepoints`,
  and (e) builds/sends `OP_ZoneChange` with `zoneID = matched_entry.zoneid`.
  The client is fully stripped (no `OP_ZoneChange`/`ZonePoint` string
  literals to anchor on — opcodes appear to be loaded from an external patch
  table rather than hardcoded), so this final wiring step could not be
  pinned to a specific `FUN_xxxx`/address within budget. The architecture
  (index in WLD region name → look up destination in the previously-received
  `OP_SendZonepoints` array by matching `iterator`) is strongly implied by
  the struct/opcode evidence above and is consistent with known EQ WLD
  modding-community documentation of the `DRNTP` region-name convention, but
  flag it as inferred if precision matters.
- `zc->x/y/z` in the outgoing `OP_ZoneChange` is presumably the player's
  *actual current position* at the moment of crossing (their normal tracked
  position, same as every other movement packet) — **not** the destination.
  The server's `ZoneUnsolicited` path never reads `zc->x/y/z` for zone-point
  matching (see above), which is consistent with the client just reporting
  "here's where I am" and letting the server look up both the matching
  zone_point and the real destination server-side.

## Recommendation for eqoxide

1. **Stop using `OP_SendZonepoints` x/y/z as trigger locations.** Those are
   destination coordinates in the target zone. Use them only to know where
   you'll land / which zone you're heading to for a given `iterator` index —
   not where to walk to in the current zone.
2. **eqoxide doesn't need to parse WLD BSP regions to be protocol-correct.**
   The cheapest, fully-faithful fix: source the **trigger** `x,y,z` (and
   ideally `height`/`width` for the trigger volume) from the same data the
   server uses — the EQEmu `zone_points` table itself (columns `x,y,z,
   heading,zone,number` = trigger; `target_x,target_y,target_z,
   target_heading,target_zone_id` = destination). If eqoxide has DB access
   (it's talking to the same EQEmu deployment), query
   `SELECT number, x,y,z,heading, target_zone_id FROM zone_points WHERE
   zone='<current_zone_short_name>' AND (version=<v> OR version=-1)` and use
   `x,y,z` as the walk-to target for crossing into `target_zone_id`. This
   reproduces the client's WLD-region trigger geometry without implementing
   a BSP region parser.
3. **Send `OP_ZoneChange` once near the trigger**, populating:
   `zoneID = target_zone_id` (from the same `zone_points` row you walked to),
   `instanceID = 0` (or current instance if targeting same zone),
   `x/y/z = <player's current actual position>` (matches native behavior —
   server ignores it for the match anyway, but keep for protocol fidelity/
   logging), `zone_reason = 0`, `success = 0` (client→server value). Leave
   `zone_mode` semantics to the server (it defaults to `ZoneUnsolicited`).
4. **Edge cases**:
   - `target_x/y/z == 999999` or `target_heading == 999` sentinel from
     `OP_SendZonepoints` (or the DB) → keep the player's pre-zone position/
     heading unchanged (same-instance zone lines). `EQEmu/zone/zoning.cpp:311`.
   - Multiple zone_points can target the same zone from different physical
     spots in the current zone — always pick the trigger closest to the
     player's actual position that targets the intended destination zone,
     mirroring `Zone::GetClosestZonePoint` (`EQEmu/zone/zone.cpp:2031-2084`).
   - Don't rely on the >400-unit "closest zone point" warning threshold as a
     hard requirement — it's a cheat-detection heuristic, not enforced
     (`EQEmu/zone/zone.cpp:2067-2076`); `ZONEPOINT_ZONE_RANGE` is `40000.0f`
     (`EQEmu/zone/common.h:64`), effectively unbounded in practice.
   - If eqoxide ever needs the true WLD-encoded trigger (no DB access
     scenario), the region name to look for in BSP Region fragments is
     `"DRNTP00255NNNNN..."` (or `"<XX>NTP...` for water/lava/slime variants)
     — parse the 5 digits at string offset 10 (after `"DRNTP00255"`) as the
     zone-point index and match against `ZonePoint_Entry.iterator` from
     `OP_SendZonepoints`. This part of the client is inferred/reconstructed,
     not directly traced in the decompile — treat as a fallback design, not
     gospel.

See also: none yet on WLD BSP region fragment format in general (0x29 —
would be a good follow-up topic: `wld-bsp-regions.md`).
