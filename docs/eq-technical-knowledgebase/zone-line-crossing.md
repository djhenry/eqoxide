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

## Client side (RoF2, confirmed)

- **Region-flag string parser**: the client decodes the ASCII "region type"
  name string that WLD BSP Region fragments (frag 0x29) carry into a 32-bit
  flag word, registered as the engine's region-type translator during
  world/display init.
- **Format** (established):
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
    `"ATP<N>"`, `"ASL"`): `"ATP<N>"` expands to the **canonical long-form
    zone-line region name `"DRNTP00255NNNNN000000000000000"`**, where `NNNNN`
    is the zone-point index, zero-padded to 5 digits, i.e. numerically
    identical to `zone_points.number` / `ZonePoint_Entry.iterator`. `"00255"`
    is a fixed literal (not yet decoded — possibly a legacy default
    radius/height code); the trailing zeros are placeholder bytes for the
    optional M/S/P/F suffix flags described above.
  - **The region string therefore only ever encodes an INDEX, never a
    destination.** There is no zone id, no target x/y/z in the WLD at all.
- **Per-region flag storage + zone-line test helper**: each BSP region's
  parsed flag word is cached on its per-region runtime state; a family of
  bit-test helpers read it — one tests bit `0x80000000` (zone-line), with
  sibling helpers testing the other bits (`0x1000000`, `0x2000000`,
  `0x10000000`, `0x400000`, etc. — water/lava/slime/other-terrain-type
  tests).
- **Not confirmed instruction-by-instruction — inferred**: the exact per-tick
  logic that (a) determines which BSP region the player's current position
  falls in, (b) calls the zone-line test above, (c) re-parses/extracts the
  embedded `NNNNN` digits from that region's raw name string, (d) matches
  `NNNNN` against the `iterator` field of the `ZonePoint_Entry[]` array
  received earlier via `OP_SendZonepoints`, and (e) builds/sends
  `OP_ZoneChange` with `zoneID = matched_entry.zoneid`. This final wiring
  step could not be pinned down precisely. The architecture (index in WLD
  region name → look up destination in the previously-received
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
     `OP_SendZonepoints`. This part of the client behavior is
     inferred/reconstructed, not independently confirmed — treat as a
     fallback design, not gospel.

See also: none yet on WLD BSP region fragment format in general (0x29 —
would be a good follow-up topic: `wld-bsp-regions.md`).

## MQZoneUnknownDest false positive on legitimate zone-line crossings (confirmed)

**Symptom:** server logs `/MQZone used at x [..] y [..] z [..] with Unknown
Destination` even though the crossing succeeds and the coords are right on the
real `zone_points` trigger (e.g. halas→everfrost trigger `x=-78,y=-693`,
observed crossing at `x=-68.98,y=-688.05,z=0.00`, only ~10 units of XY error).

**Two totally different validation paths inside `Handle_OP_ZoneChange`,
selected by whether the client sends `zc->zoneID == 0`:**

- `EQEmu/zone/zoning.cpp:38` (`Handle_OP_ZoneChange`). `zc->x/y/z/zone_reason/
  success` are logged (`zoning.cpp:56-69`) but **never used for matching** —
  matching always uses the server's own tracked `Mob::GetPosition()`
  (`zone/mob.h:678`, updated by the player's ordinary `OP_ClientUpdate`
  packets), not the payload coordinates.
- **`zc->zoneID == 0`** ("client doesn't know where it's going") →
  `Zone::GetClosestZonePointWithoutZone(x,y,z,client,ZONEPOINT_NOZONE_RANGE)`,
  `zoning.cpp:100`. Implementation `zone/zone.cpp:2093-2129`: pure **XY-only**
  squared-distance nearest-neighbor across **all** zone_points regardless of
  target zone (z term is explicitly commented out, `zone.cpp:2117`). **No
  water-map check at all.** Only failure mode: no zone_point within
  `sqrt(40000)=200`... actually `max_distance2 = max_distance*max_distance`
  with `max_distance=ZONEPOINT_NOZONE_RANGE=40000.0f`
  (`zone/common.h:62`) → effectively unbounded in practice. If it fails, the
  cheat raised is **`MQZone`/`MQGate`** (not `MQZoneUnknownDest`) plus
  `SendZoneCancel` — the zone is *rejected*, not silently flagged-but-allowed
  (`zoning.cpp:110-114`).
- **`zc->zoneID != 0`** (client names a destination zone) →
  `Zone::GetClosestZonePoint(glm::vec3(GetPosition()), target_zone_id, this,
  ZONEPOINT_ZONE_RANGE)`, `zoning.cpp:138`. Implementation
  `zone/zone.cpp:2031-2085`. This is the path that emits `MQZoneUnknownDest`
  ("... with Unknown Destination"), and it does so via **two independent
  false-positive heuristics**, gated on `Zone::HasWaterMap()`:
  - **No water map for the zone:** flags if the nearest same-target zone_point
    is `> 400.0f` units away in XY (`zone.cpp:2068`, straight `400.0f`
    constant, not tied to `ZONEPOINT_ZONE_RANGE`) — this is the "distance
    heuristic" documented earlier in this file. It's a *warning-only* flag; it
    does not null out `closest_zp` unless `closest_dist > max_distance2`
    (`zone.cpp:2078`), so the zone still succeeds.
  - **Zone HAS a water map (`<zone>.wtr` file present under `maps/water/`):**
    the 400-unit XY heuristic is **bypassed entirely** and replaced with
    `!zone->watermap->InZoneLine(glm::vec3(client->GetPosition()))`
    (`zone.cpp:2068`, first half of the `||`). `WaterMapV2::InZoneLine`
    (`zone/water_map_v2.cpp:58-60`) checks whether the position falls inside
    any authored `OrientedBoundingBox` region tagged `RegionTypeZoneLine`
    (`zone/water_map_v2.h:43`, `zone/oriented_bounding_box.h:23-37`). **This
    box test is fully 3-dimensional** (`min_z/max_z` are real fields checked
    inside `ContainsPoint`, `zone/oriented_bounding_box.cpp`) and is a
    **third, independent geometry source** — separate from both the client's
    WLD `DRNTP` BSP region and the DB `zone_points` trigger `x/y/z`. **Confirmed:
    `halas.wtr` exists** (`everquest_rof2/maps/water/halas.wtr`), so Halas→
    Everfrost crossings go through this stricter box test, not the lenient
    400-unit heuristic.
  - Either branch true → `client->cheat_manager.CheatDetected(MQZoneUnknownDest,
    location)` (`zone.cpp:2070-2071`) where `location` is the same
    `GetPosition()` passed in — i.e. **the coordinates in the log line are the
    server's last-tracked position for the player at the moment `OP_ZoneChange`
    was processed, not anything read out of the `ZoneChange_Struct` payload.**
    This flag is *advisory only* here too — `closest_zp` is still returned/used
    (`zone.cpp:2078-2084`) unless it's also beyond `max_distance2`, so the
    crossing succeeds anyway. Matches the observed symptom exactly: successful
    crossing + a cheat-log line.
  - The `.wtr` z-bound check is the prime suspect for the false positive: the
    observed logged `z [0.00]` is very unlikely to be Halas's real terrain
    elevation at that spot; if the server's last `OP_ClientUpdate`-derived
    z for the player was stale/wrong (e.g. sent one tick before reaching the
    doorway, or a floor-raycast miss at the zone seam) when the server happened
    to process `OP_ZoneChange`, the OBB's `min_z..max_z` for the authored
    zone-line volume can reject an XY-perfect position. **Not fully confirmed
    against eqoxide's own tracked z at the crossing instant** — would need a
    debug log of the last `send_position_update` z value immediately before
    `send_zone_change_packet` fires to nail down definitively — but the
    mechanism (3D OBB test, independent of DB trigger data, gated only on
    `zone->HasWaterMap()`) is fully confirmed in EQEmu source.
  - Corroborating changelog: `EQEmu/changelog.txt:3736` — *"JJ: Initial fix
    for /MQZone detection to reduce false positives"* (11/25/2012) — EQEmu's
    own history acknowledges this detector is false-positive-prone.
  - `RuleB(Cheat, EnableMQZoneDetector)` defaults `true`,
    `RuleI(Cheat, MQZoneExemptStatus)` defaults `-1` = **no status is exempt**
    (`common/ruletypes.h:1125,1130`) — GMs/admins are not automatically
    spared either.

**Why the detector's two zc->zoneID branches differ this way (inferred, not
in source comments):** `GetClosestZonePoint` (the `zoneID != 0`, stricter
branch) is exactly the code path a `/MQZone <zone>` MacroQuest-style cheat
would hit, since that cheat names its destination explicitly — hence the rule
is literally named `MQZone`. `GetClosestZonePointWithoutZone` (the `zoneID ==
0` branch) has no such check because a client that says "I don't know where
I'm going" can't be pre-naming a destination to exploit. This asymmetry is
almost certainly *why* the stricter/buggier check only exists on the
non-zero-zoneID path.

**Open question, not resolved here — whether the vanilla RoF2 client sends
`zoneID = 0` or `zoneID = <destination>` on an organic zone-line walk.** Not
directly determined from the client alone — see "Client side" section
above. Both are structurally valid
server-side (`zoning.cpp:78` and `:120` are siblings in the same `if`), so
either could be "native."

## Recommendation for eqoxide (MQZone false positive)

1. **Send `OP_ZoneChange.zoneID = 0`** instead of the resolved destination
   zone id. This routes the request through `GetClosestZonePointWithoutZone`
   (`zone.cpp:2093`), which has **no water-map/OBB check and no 400-unit
   heuristic** — it's a plain nearest-zone_point-by-XY lookup across all
   zones, tolerant of z entirely (z term explicitly disabled,
   `zone.cpp:2117`). Given eqoxide is already walking to within ~10 units of
   the real DB trigger (per the "Client side" fix above), this will reliably
   resolve to the correct zone_point without risk of the `MQZoneUnknownDest`
   flag, and without needing to track/send a precise z to satisfy a
   `.wtr`-authored 3D box that eqoxide has no data for and no way to author
   itself (eqoxide doesn't parse `.wtr` water-map files at all today).
   `instanceID`/other fields are unaffected — leave `instanceID=0` (or current
   instance) as before; the server resolves `target_instance_id` from the
   matched `zone_point` in the zero-zoneID branch too (`zoning.cpp:104-105`).
2. This is a **strict improvement, not just cheat-flag suppression**: it also
   removes the last remaining edge case where a stale/incorrect destination
   zone_id (e.g. multiple zone_points at slightly different trigger spots
   heading to different target zones, or an index/iterator mismatch bug) could
   cause `target_zone_id != zone_point->target_zone_id` and an outright
   `SendZoneCancel` (`zoning.cpp:141-146`) — that mismatch check is skipped
   entirely on the zero-zoneID path.
3. Keep sending real current `x/y/z` in the packet for protocol fidelity/
   server logging (still unused for matching either way).
4. If eqoxide ever wants to be defensive against the (much rarer) case where
   NO zone_point is within `ZONEPOINT_NOZONE_RANGE` at all, that still results
   in `SendZoneCancel` + `MQZone`/`MQGate` (not `MQZoneUnknownDest`) — but this
   should not happen given the existing trigger-walk-to logic already gets
   eqoxide within DB-tolerance of the real trigger.
5. Do **not** attempt to parse/author `.wtr` water-map zone-line boxes just
   to satisfy this check — that's solving a false positive the server itself
   documents as historically flaky (changelog above), and the zero-zoneID path
   sidesteps it entirely with less client-side work.
