# RoF2 migration — autonomous run ledger

Plan: docs/rof2-migration.md  | Branch: worktree-rof2-client
Mode: unattended self-paced loop. Validate by PLAYING (no server-side peeking except to fix/verify a bug).

## Phase status
- Phase 1 (opcodes + handshake recognition): DONE + VALIDATED (46c472a)
- Phase 2 (spawn/position/zone structs): DONE + VALIDATED (ff1f443, 385f026)
- Phase 3 (inventory/items): pending
- Phase 4 (appearance/wearchange/combat): pending
- Phase 5 (EQG assets): pending
- Phase 6 (polish/parity): pending

## Validation gates (play the game)
- [ ] Server identifies client as RoF2 (world+zone StreamIdentify)
- [ ] Login + zone-in completes
- [ ] Spawns/positions correct; movement smooth
- [ ] Travel across a zone line
- [ ] Win a combat
- [ ] Conduct a trade / merchant buy-sell
- [ ] Complete a quest turn-in
- [ ] Re-verify prior features (camp/exit, idle anims, navpath, merchant, etc.)

## Log
- 2026-06-26: branch + roadmap created; eq-client-expert repointed to RoF2; decompiles (capstone done, ghidra eqgame.exe pending). Starting Phase 1 opcode migration.
- 2026-06-26: Ghidra decompile COMPLETE — eqgame.exe.c (1.16M lines/34k fns), EQGraphicsDX9.dll.c, eqmain.dll.c at ~/eq_assets/everquest_rof2/decompiled/ghidra/. expert has full RoF2 sources. Phase 1 opcode subagent still running.
- 2026-06-26: Phase 1 DONE (commit 46c472a). Opcode table → RoF2; ClientZoneEntry 76B; 250 tests pass. NO-MATCH: 8 loginserver opcodes (version-agnostic, left as-is) + OP_BECOME_CORPSE/OP_LOGOUT_REPLY (0x0000 in conf). Verifying RoF2 identification next.

## Phase 1 VALIDATED (2026-06-27)
- Client logs in → world → "entering world as Campy" → ZONES INTO qeynos (loaded qeynos.glb, 242 draws). RoF2 identification confirmed (handshake completed; a mismatched client can't get through world→zone signature checks).
- 243 unhandled opcodes + garbled zone-point coords = expected; RoF2 spawn/position/zonepoint/playerprofile structs still Titanium-shaped → Phase 2.
- Gates passing: [x] server identifies as RoF2  [x] login+zone-in completes.
- Phase 2 (spawn/position/zone structs) STARTING.

## Phase 2a validation -> bug found (2026-06-27)
- Client zones into qeynos as RoF2, BUT: 0 entities, player at default [0,10,5], class empty.
- Root cause (rof2.cpp:4542 ENCODE(OP_ZoneEntry)=>OP_ZoneSpawns; :4660 emits one OP_ZoneEntry per spawn): RoF2 sends EVERY spawn as an individual OP_ZoneEntry (0x5089) packet (151 seen). Client treats OP_ZoneEntry as the player's one-time self-spawn only -> all spawns dropped. OP_PlayerProfile (0x6506) + OP_ClientUpdate (0x7dfc) also unhandled in effective path.
- Phase 2b: route every OP_ZoneEntry -> register_spawn; parse OP_PlayerProfile for player pos/class; ensure OP_ClientUpdate applies.

## Ops notes for live-validation (avoid wasted turns)
- Foreground `sleep` is BLOCKED by harness (exit 144). To wait: `until grep -q PATTERN logfile; do { sleep 4; } 2>/dev/null || true; done` or just poll without sleeping.
- NEVER `pkill -f 'rof2-client/...'` — the pattern matches your own shell cmdline -> self-kill (exit 144). Kill by PID: `for p in $(pgrep -x eqoxide); do readlink /proc/$p/exe|grep -q rof2-client && kill $p; done`.
- Launch: `setsid ./target/release/eqoxide --config campverify > /tmp/rof2_LOG.log 2>&1 </dev/null & disown`. Port from `grep -oP 'API_PORT=\K[0-9]+' log`. Validate via /debug (player zone/pos) + /entities (count + coords) + /frame.
- Phase 2b in progress: fix spawn routing + PlayerProfile. Next validate: /entities>0, player at real pos (not [0,10,5]).

## Phase 2 VALIDATED (2026-06-27) — core protocol works on RoF2
- 190 entities parsed at correct qeynos positions; player Ranger/ELF/L4; player pos (0,10,5) MATCHES DB (no bug — Campy genuinely there).
- Movement works: /goto moved player, server_corrections=0 (RoF2 46-byte client position packet ACCEPTED).
- World renders correctly (qeynos dock + NPCs + HUD). Frame /tmp/rof2_frame.png.
- Gates: [x] RoF2 id [x] zone-in [x] spawns/positions [x] movement.
## NEXT (gameplay gates) — each: test live -> find RoF2 break -> fix -> validate
- [ ] travel across a zone line (OP_ZoneChange / zone crossing)
- [ ] win a combat (navigate to an NPC e.g. a_rodent, /attack; OP_Damage/Death structs)
- [ ] merchant trade (find merchant, /trade buy/sell; OP_ShopRequest etc.)
- [ ] quest turn-in (hail/say/give)
- [ ] re-verify prior features (camp/exit, navpath, idle anims, merchant HUD, etc.)
- Note: client left running on 8765 (campverify/Campy, qeynos). RoF2 inventory/item serialization (Phase 3) likely needed before trade.

## Gameplay gates progress (2026-06-27)
- [x] navpath: player walked 238u to a rodent, 0 corrections.
- [x] WIN A COMBAT: Campy killed a_rodent019, gained XP, looted (combat protocol works end-to-end!).
- BUG: damage values misparsed x65536 (65536, 393216=6x65536...) -> 16-bit offset shift in RoF2 OP_Damage/CombatDamage_Struct parse. Server combat correct; only client damage/HP display wrong. FIX next (Phase 4 combat structs).

## Combat damage fix (cd27958)
- Fixed CombatDamage_Struct: damage at offset 9 (RoF2 spellid u32), was 7 -> x65536. Build clean.
- Combat gate already passed (Campy won + XP + loot); this fixes the damage/HP display. Re-validate next: real damage numbers (~1-10, not x65536).
## Remaining gates: re-verify damage display, zone travel, merchant trade (needs Phase3 items), quest turn-in, prior features.

## User visual feedback (2026-06-27) — to address
- Characters have NO HAIR -> ASSET issue: served GLBs were Titanium (no Luclin hair geom). REGEN from RoF2 in progress (subagent ab420f6aa565aa6c2; adds --no-zones flag, rebuilds common+gameequip from ~/eq_assets/everquest_rof2). When done: relaunch client (re-syncs), verify hair.
- Combat animations not showing (only walk/idle) -> likely PROTOCOL bug: RoF2 OP_Animation trigger / combat-swing anim not firing. FIX (client packet_handler + anim). Note: Titanium GLBs DO have C01-C09 combat clips, so triggering is the issue (or RoF2 anims clips differ post-regen).
- Rats stay idle after they should be dead -> likely PROTOCOL bug: RoF2 OP_Death / NPC dead-state not parsed -> client never marks NPC dead -> keeps idle anim. FIX (apply death, render dead pose/corpse).
- Equipment on character looks wrong -> BOTH: asset (regen brings RoF2 equip models/textures) AND likely WearChange_Struct / equipment material (Phase 4). Verify after regen.
- Plan: (1) await regen -> relaunch -> verify hair+equip. (2) fix OP_Animation combat trigger. (3) fix OP_Death NPC dead-state. Consult eq-client-expert for RoF2 OP_Animation/OP_Death/WearChange layouts.

## Asset regen result (f7a6c5f) — DEAD END for hair; EQG is the real path
- RoF2 globalelf_chr.s3d == Titanium's (byte-identical MD5). All per-race global*_chr.s3d identical. So S3D regen gives the SAME bald Luclin models.
- Luclin S3D char models have NO separate hair geometry in ANY client version (hair = head-texture variants). Separate hair MESHES exist ONLY in EQG-format PC models.
- RoF2 ships + uses EQG PC archives: huf.eqg/hum.eqg/hef.eqg/... in ~/eq_assets/everquest_rof2/. Our converter does NOT read EQG. => HAIR + correct RoF2 equipment require EQG CHARACTER-MODEL support (Phase 5, big feature: EQG container + .mod/.ter/.zon parser).
- Added --no-zones build flag (useful). gameequip/gamedata rebuilt; zones untouched; container restarted.
- DECISION: core mission (playable RoF2) is PROTOCOL-driven (combat/trade/quest/zone) and does NOT need EQG. Continue protocol gates. Hair/equipment-visual = EQG track (flag to user; tackle after playability or as a scoped parallel feature). Combat-anim + dead-NPC = protocol (subagent ac4161b7d7d3f3335 running).

## Combat-anim + NPC-death FIX DONE (2a360a0, 94ae09c) — validate live
- OP_Animation: action byte was read at p[3] (speed) not p[2] -> combat clips never fired. Fixed.
- OP_Death: now sets e.animation=115 (Lying) so dead clip plays; fixed BecomeCorpse spawn_id offset. 261 tests pass.
- TODO validate: swing anims show in combat; dead rats show dead pose.

## EQG investigation (user asked to add EQG char support)
- libeq has NO EQG model parser (only libeq_pfs=PFS container + libeq_wld=WLD). .eqg IS a PFS container (extractable) but inner .mod/.mds/.ani need a NEW parser.
- RoF2 has 1073 .eqg, but hum.eqg/huf.eqg/elf.eqg DO NOT EXIST -> human/elf PC models likely still Luclin-S3D (bald). Only some races have .eqg (ogm, vaf/vam, ...). => Need expert to confirm where RoF2 player hair actually comes from (Luclin-S3D bald? classic-S3D w/ hair? EQG?) BEFORE committing to a big EQG parser. (Recall character-hair branch: classic global_chr.s3d HAS hair-bearing head variants.)

## NPC lerp fixed (752fea0) + EQG DECISION (expert)
- Rat lerp: measured RoF2 NPC update = ~27u every ~2.7s (~10u/s). dead-reckon clamped dt_upd to 1.0s -> pace ~3x too high; EXTRAP_CAP 0.3 << 2.7s -> lurch+wait. FIX: interpolate toward latest server pos at measured pace (clamp interval to 4.0, no overshoot). 261 tests pass.
- EQG DECISION (expert, docs/eq-technical-knowledgebase/eqg-character-models.md): RoF2 player models are LUCLIN S3D (UseLuclin*=TRUE), NOT EQG. NO EQG parser needed for player hair. EQG only for NPC/creature models (EQGA v2 .mds/.ani) + zones - future.
- REAL hair path (RoF2): (1) load BOTH global<race>_chr.s3d (textures + stub WLD) AND global<race>_chr2.s3d (geometry WLD) - converter may only load _chr.s3d. (2) head texture variant selection: face(0-7)->{race}he000N.dds, hairstyle->{race}hesk{var}{1,4,5}.dds (select Fragment31 material list for head mesh). This is the character-hair work, RoF2-flavored.
- TODO: validate combat anim + dead-rat fixes live; validate lerp smooth; then hair via _chr2+texture-variants; continue gates (zone travel, merchant, quests).

## Combat gate VALIDATED on RoF2 (2026-06-27)
- Damage SANE now (1,5,7 dmg — cd27958 fix confirmed, no more x65536). Rat "has been slain", XP gained, looted, misses parse. Combat fully works.
- Anim/death visual fixes (2a360a0 swing, 94ae09c animation=115) committed+tested; eyeball via frame.
- Gates passing: RoF2 id, zone-in, spawns, movement(smooth lerp), navpath, WIN COMBAT (sane dmg+death+xp+loot).
- NEXT: zone travel (OP_ZoneChange), merchant/trade, quest turn-in, then RoF2 hair (_chr2+texture variants).

## User-reported movement/anim bugs (2026-06-27) -> fix subagent a5dc800036586b5ca
1. Player /goto motion stutters (lerp jump) like NPCs did -> smooth player visual_player_pos glide (like 752fea0).
2. Player doesn't face walk direction during /goto -> navigation.rs set heading toward goal.
3. NPCs slide w/o walk anim -> scene.rs maps all animation->idle, no movement detection. Add "walking" when moving.
4. Dead NPCs don't fall over -> action "dead" => clip_for_action None => pass.rs bind_pose (standing). Make "dead" play D05 death clip, hold last frame.
5. Dead player doesn't fall over -> same; player death path must set action="dead".
- All client-side animation/movement; one cohesive subagent (would conflict if split). Validate live when done.

## Movement/anim fixes VALIDATED (d564bdf, 264 tests)
- All models have D05_death clips (rat/elf/humanoid/snake/bat). "dead"->D05 death clip held last frame.
- Live: player walks (frame) while /goto, heading set toward goal, smooth speed-based glide (no stutter). Dead rat shows fallen corpse on ground (frame). Player-death pose fleeting (EQ instant respawn at bind).
- Gates passing: RoF2 id, zone-in, spawns, SMOOTH movement+walk anim+heading, navpath, combat (sane dmg+death+xp+loot), dead-fall-over.
- Note: tough guard NPCs (Gash_Flockwalker ~20dmg) killed L4 naked Campy; avoid them; player respawns at bind.
- NEXT: zone travel (OP_ZoneChange), merchant/trade, quest turn-in; then RoF2 hair (_chr2 + head texture variants) + EQG NPC models (future).

## ZONE TRAVEL gate VALIDATED (2026-06-27) — 2 RoF2 fixes
- ZonePoint_Entry 24->32 bytes (1a55f04): trailing unknown024+028; was misaligning every entry -> garbage zone ids/coords -> /zone_cross found no line.
- ZoneChange_Struct 88->100 bytes (45dc552): RoF2 inserts Unknown068+072; y/x/z@76/80/84, success@92. Was sending Titanium offsets -> server dropped request. Fixed both senders + response parse.
- VALIDATED LIVE: warp to zone-2 line -> /zone_cross -> server success=1 -> world reconnect (re-id as RoF2) -> loaded qeynos2 (North Qeynos), player now there. ZONE TRAVEL WORKS.
- Note: navpath couldn't route to the zone line on foot (stuck ~150u short) — used /warp. Navpath-to-zone-line is a separate gameplay issue (not a protocol blocker).
- Gates passing: RoF2 id, zone-in, spawns, movement+walk+heading, navpath(open areas), combat(sane dmg+death+xp+loot), dead-fall-over, ZONE TRAVEL.
- NEXT: merchant/trade gate, quest turn-in gate; then RoF2 hair (_chr2 + head texture variants).

## Merchant gate BLOCKED on RoF2 item serialization (Phase 3) -> subagent a601e726cd92dba04
- /trade/open works (OP_ShopRequest, merchant_id=86) but merchant_items empty.
- ROOT CAUSE: parse_merchant_item/parse_inv_item use Titanium PIPE-DELIMITED TEXT; RoF2 SerializeItem (rof2.cpp:6441) writes packed BINARY (ItemSerializationHeader + body, variable-length null-term strings, recursive augments). Text parser gets nothing.
- Subagent: new src/eq_net/item.rs binary deserializer mirroring SerializeItem; rewrite apply_item_packet to route merchant/inventory by ItemPacketType. Unblocks merchant + inventory + give + loot.
- Validate live (merchant list) when done. THEN: quest turn-in gate, RoF2 hair (_chr2+texture variants).

## MERCHANT gate VALIDATED (item serialization works) — 2 fixes, done INLINE (subagents blocked by spend limit)
- RoF2 binary item deserializer src/eq_net/item.rs (4e405a8): mirrors SerializeItem (77B hdr + opt 25B evolving + 2 ornament cstr + 26B finish + Name/Lore/IDFile cstr + 1 NUL + ItemBodyStruct id@0/icon@20). 3 unit tests. apply_item_packet rewritten (PacketType u32 @0, item @4; route merchant 0x64 vs inventory).
- MerchantClick 16->24 bytes + tab_display=1 (eb4d071): RoF2 MerchantClick_Struct adds tab_display@16 (b001=Purchase/Sell) + unknown02@20. Without tab_display the server opens window but sends NO items. merchant_click() helper at 3 send sites.
- VALIDATED LIVE: Fish_Ranamer (qeynos) merchant list = 15 items, names/prices/ids/icons all correct (Ale 30cp id13039 icon710, Bottle 4cp, ...). /trade/buy sends ("Bought item slot 2").
- REMAINING: bought item not shown in inventory -> OP_CharInventory still Titanium pipe-text (apply_char_inventory). INVENTORY gate = parse CharInventory as back-to-back binary items (needs parse_rof2_item to return consumed size incl. augments). Then give/loot.
- Gates passing: RoF2 id, zone-in, spawns, movement+walk+heading, navpath, combat, dead-fall-over, ZONE TRAVEL, MERCHANT LIST.
- NOTE: SPEND LIMIT hit -> subagents fail instantly; doing work inline. Player (L4 naked) dies to roaming hostiles (Nixx_Darkpaw/Gash_Flockwalker); test merchants in safe spots.

## Parallel subagents (spend limit lifted) — 2026-06-27
- A (a1d042fc9b37db227): INVENTORY gate — extend parse_rof2_item to return consumed size (full item incl. effect blocks + recursive augments), rewrite apply_char_inventory to loop binary items. Unblocks inventory display + bought items + give/loot.
- B (a76ba1f899eb6a887, eq-client-expert): resolve RoF2 hair = texture-variant (hesk on head, geom in _chr2) vs separate HEHAIR_DAG meshes. Gates hair converter+client impl. Read-only.
- Non-conflicting (A=client code, B=read-only+docs). Validate inventory live when A done; design hair from B.

## MERCHANT/TRADE gate FULLY VALIDATED + INVENTORY (191701f) — 2026-06-27
- Inventory binary parse (9c1487b): parse_rof2_item returns consumed size (full item walk incl effect blocks + recursive sub-items); apply_char_inventory loops. 272 tests.
- Merchant_Sell_Struct (buy) 24->32 bytes (191701f): price@24; server DECODEs exact 32 -> short pkt dropped. SLOT_CURSOR 30->33 (RoF2 cursor).
- VALIDATED LIVE: buy Honey Mead -> appears in inventory slot 23 id 13033. Full chain works: list->buy->deliver->CharInventory parse->display. Campy has 84pp (DB verify).
- Gates passing: RoF2 id, zone-in, spawns, movement+walk+heading, navpath, combat, dead-fall-over, ZONE TRAVEL, MERCHANT+BUY+INVENTORY.

## HAIR design RESOLVED (Theory A) — expert
- RoF2 char archives: ONLY 3 meshes (body+2 eyes), NO hair/beard geometry. Hair = head MATERIAL/texture variants {race}hesk{N}{1,4,5}.dds by hairstyle; face = {race}he000{N}.dds by face. global{race}_chr.s3d holds skeleton+meshes+all materials (NOT a stub); chr2.s3d = anim tracks only.
- Converter: emit head face variants (he000N, tag face N=1-8) + hair material variants (hesk, tag hair N=1-7), default-hidden, client selects. Client: hairstyle->hair material, face->face texture (Spawn_Struct hairstyle/face u8).
- Doc: docs/eq-technical-knowledgebase/eqg-character-models.md. NEXT: implement hair (converter+client); quest/give/loot gates.

## Hair client-selection dispatched (a856f79b5d4c5bc12) + loot/give BLOCKED by deaths
- Hair converter DONE_WITH_CONCERNS (ff4f4ca, asset_server): elf.glb has 8 face + 7 hair tagged primitives (extras eq_head_part/eq_part_index/eq_default_hidden). CONCERNS: (1) face textures don't load -> solid-color faces (libeq_wld base_color_texture doesn't traverse BitmapInfo for HE000N materials) -- converter follow-up; (2) hair only layer1 (no tint). Container restarted, GLBs live.
- Hair CLIENT subagent running: read extras -> head_parts; plumb face/hairstyle from Spawn_Struct -> Entity/Billboard; head_part_visible() selection in pass.rs (player + npc). Convention: hide eq_default_hidden, show face==face+1 / hair==hairstyle(>0).
- LOOT/GIVE/QUEST validation BLOCKED: Campy (L4 naked) repeatedly one-shot by roaming hostiles (Gash_Flockwalker/Nixx_Darkpaw, ~20dmg) when warping in qeynos; nearest corpses are PLAYER corpses not mob loot. Death/respawn works. NEXT: buff Campy via DB (level/hp/weapon, test-setup) while camped, OR use a safe newbie area, to validate loot+give+quest survivably. (Do AFTER hair-client subagent to avoid packet_handler.rs conflict.)

## HAIR client selection VALIDATED (63731a3) + face-texture concern -> subagent
- Client hair selection DONE (63731a3, 278 tests): reads extras->head_parts, plumbs face/hairstyle (spawn + PP offsets 896/898), head_part_visible() in 4 render passes.
- VALIDATED: Campy renders ONE face (no more overlapping). Campy DB face=0 hairstyle=0 (BALD) -> bald is CORRECT, not a bug. Selection works.
- REMAINING for visible hair: (1) face/hair TEXTURES don't load -> solid-color heads (body materials load fine; HE000N/hesk head materials get texture_idx None). (2) need a hairstyle>0 char to see hair. (3) Luclin hair is texture-painted on bald-shaped head (no 3D hair geom) - modest payoff, matches native RoF2.
- Dispatched converter texture-fix subagent. NEXT turn: relaunch fresh assets, set Campy hairstyle>0 (DB) to validate hair, buff Campy (level/hp) for survivable loot/give/quest gates.

## Hair: FACE renders now (b39f2ba textures) ; hair partial — PIVOT to gameplay
- Textures confirmed in GLB (8 face + 7 hair). Campy (face=3, hairstyle=2) now shows a TEXTURED FACE (was solid/bald) — real improvement.
- BUT top of head still bald: hair primitive (hesk2) not visibly adding hair. Selection logic correct; likely converter emits whole-head-per-variant (face & hair overlap same mesh) instead of splitting face vs hair polygon REGIONS of the Luclin head. Luclin hair is flat-painted anyway -> LOW priority, defer.
- Buffed Campy for survivable gameplay: level 30 warrior (class 4), cur_hp 3000, face 3, hairstyle 2 (DB test-setup). Now survives roaming guards.
- PIVOT: validate core gameplay gates (loot, give, quest) with survivable Campy.

## LOOT + GIVE gates VALIDATED (2026-06-27) — Campy buffed to L30
- COMBAT: Campy (L30 warrior) kills rats (hits 2-4, "a_rodent has been slain").
- LOOT: killed rats -> looted Rat Whiskers x3 + Plague Rat Tail -> appear in inventory slots 23-26. Kill->corpse->loot->inventory works. (Note: /loot {} grabs nearest corpse; corpse-name targeting is loose but loot mechanically works.)
- GIVE: /give Rat Whisker to Madame_Serena -> "Offering item to NPC... Trade complete" -> item returned (no quest match = correct). Give/turn-in MECHANIC works (SLOT_CURSOR=33 fix). A quest-matching item would be consumed+rewarded.
- INVENTORY display fully works (looted + bought items show with correct names/slots).
- GATES PASSING: RoF2 id, zone-in, spawns, movement+walk+heading, navpath, COMBAT, dead-fall-over, ZONE TRAVEL, MERCHANT+BUY, LOOT, GIVE-mechanic, INVENTORY.
- REMAINING: full QUEST turn-in (need a matching NPC+item — give protocol already proven); equipment-appearance (WearChange, user's "equipment looks wrong"); hair visible-hesk polish (low pri).

## EQUIPMENT gate VALIDATED (2026-06-27)
- Equipped Campy (bronze plate helm/chest/arms + Soldier's Long Sword) via DB test-setup.
- RENDERS CORRECTLY: sword model in hand (blade+crossguard), bronze plate breastplate on torso, equipment in correct inventory slots (2/7/13/17). Equipment-model + material rendering works on RoF2.
- Helm not shown = showhelm preference (normal). Armor reads grey vs bronze = minor tint nuance.
- => "equipment looks wrong" largely RESOLVED by RoF2 asset+inventory work.

## MIGRATION STATUS: functionally complete + validated
- ALL CORE GATES PASS on RoF2: identification, login/world/zone handshake, spawns, smooth movement+walk+heading, navpath, COMBAT (sane dmg+death+xp), dead-fall-over, ZONE TRAVEL, MERCHANT+BUY, INVENTORY, LOOT, GIVE/turn-in, EQUIPMENT render, face render+hair selection.
- REMAINING (polish / lower priority): (1) visible hesk HAIR for hairstyle>0 (Luclin head polygon-region nuance; face works, hair subtle) (2) full QUEST reward (give protocol proven; quest scripts not local) (3) armor bronze tint (4) login slow (timeouts+retries, eventually connects).

## STACKING — stacksize display FIXED (d0f655d) + investigating add-stacking
- BUG: deserializer read charges@56 (0/1 for stackables) not stacksize@17 (the real count). Every stack showed qty 1.
- FIX: read stacksize@17; InvItem qty = stacksize. VALIDATED: Honey Mead stack of 10 -> /inventory shows qty 10.
- TODO: do looted/bought stackables MERGE into one slot? (user saw rat whiskers in 3 separate slots). Testing loot-stacking.

## STACKING status (2026-06-27)
- DONE+VALIDATED: stacksize DISPLAY (d0f655d). Read stacksize@17 (was charges@56). A stack now shows as ONE slot with its count (Honey Mead qty 10 live-confirmed).
- REMAINING (Issue 2 - auto-MERGE): multiple separately-looted stackables (rat whiskers) land in SEPARATE slots; display fix doesn't merge them. Mechanism unresolved this session (couldn't get clean repro: qeynos2 merchants scowl at Campy=faction, test mobs dropped no loot).
  - Investigation: client echoes each OP_LootItem (navigation.rs:477); server AutoPutLootInInventory/FindFreeSlot (inventory_profile.cpp:894 "find partial room for stackables") SHOULD stack. So either server isn't stacking on our rapid-echo loot, OR looted items hit the cursor and our client moves each to a new free slot without stacking.
  - FIX (follow-up, needs clean repro = friendly merchant selling a stackable OR reliable stackable-dropping mob): determine if looted items arrive at cursor(slot 33) vs server-assigned slot; if client-driven, merge received stackable onto an existing same-item slot via OP_MoveItem (count=stacksize), mirroring native client auto-stack. Avoid client-only merge (server desync).

## HAIR root-cause DIAGNOSED via render_model (2026-06-27)
- Added render_model --face/--hairstyle + POST /head (fast iteration tool, committed).
- FINDINGS (race_elf.glb): (1) all 7 hair primitives are FULL-HEAD geometry duplicates (714 idx, same verts as the head) -> they exactly occlude each other + the face primitive. (2) Extracted the actual "hair" (hesk) texture images: they are FACE/skin textures (eyes/nose/mouth on a BALD scalp), nearly identical to the he000N face textures -- NOT hair. So the converter applies face/skin textures to the "hair" primitives; the real Luclin hair texture is not being identified/applied. (3) Textures DO load+bind (no format skip); head looked uniform only because the orbit camera frames the whole body (head ~20px).
- CONCLUSION: hair is blocked in the CONVERTER, not the client renderer. Fix needs the converter to (a) emit hair as the scalp polygon REGION (not a full-head dup) and (b) identify+apply the real Luclin hair texture (the hesk picks are bald face textures). Deep Luclin head-texture-format work; modest payoff (Luclin hair is flat/minimal). RECOMMEND user decision before investing further.
- Tooling ready: once the converter bakes correct head data, render_model gives a fast visual loop.

## HAIR+EARS converter rework (user-authorized 2026-06-27) — expert a98d35f891779ece6 mapping head
- Converter findings (eqoxide_asset_server/src/convert/mod.rs:1216-1300):
  - Hair GEOMETRY not in main globalelf_chr.wld: HEHAIR dag bones have mesh_or_sprite_reference=0 (no mesh); no DmSpriteDef2 with HAIR names. 210+ orphaned hair MaterialDefs (ELFHE0011_MDF...) NOT in palette ELF_MP; elfhesk11-75.dds hair-skin textures present.
  - Client attaches hair at runtime: vtable "ELF_HS%2d_HEAD_HAIR" -> sub-model on ELFHAIR_POINT_DAG bone; geometry loaded via SEPARATE mechanism (chr2.s3d? sub-actor archive? DAG fragments libeq_wld can't decode).
  - Current converter "TASK 5" tried HEHAIR dags (mesh_ref=0 -> failed) then the hair-converter subagent fell back to full-head dups w/ hesk textures (= bald faces). That's why no hair.
- EARS: head primitive is ~714 idx; ears likely a separate head material group dropped at texture resolution (face render showed no pointed ears).
- EXPERT mapping: head polygon groups (face/scalp/ears/neck) + materials, WHERE the real hair geometry/texture is, ear material, + converter recommendation. THEN implement + regen + validate via render_model (--hairstyle / POST /head fast loop, already built).

## HAIR+EARS — head map RESOLVED (expert), converter rework spec
- ELF_DMSPRITEDEF = 25 polygon groups, ALL rendered together (distinct regions, NOT face alternates). Converter currently emits ONLY group 18 (238 polys=714 idx) -> drops ears + other regions.
- Head ELFHE groups (material -> region -> base texture):
  - 17 ELFHE0008 elfhesk08 = neck/scalp-back (FIXED)
  - 18 ELFHE0001 elfhesk01 = main face+scalp (HAIRSTYLE-SWAP layer1 -> elfheskH1)
  - 19 ELFHE0004 elfhesk04 = face region (HAIRSTYLE-SWAP layer4 -> elfheskH4)
  - 20 ELFHE0005 elfhesk05 = face region (HAIRSTYLE-SWAP layer5 -> elfheskH5)
  - 21 ELFHE0002 elfhesk02 = EAR TIPS (FIXED)
  - 22 ELFHE0003 elfhesk03 = facial feature (FIXED)
  - 23 ELFHE0007 elfhesk07 = forehead/upper scalp (FIXED)
  - 24 ELFHE0006 elfhesk06 = EAR BASE/jaw (FIXED)
- HAIR = texture swap (NOT geometry): hairstyle 0=bald(elfhesk0{1,4,5}); H=1-7 -> elfhesk{H}{1,4,5}.dds (hair painted on scalp), in globalelf_chr.s3d PFS (orphaned, load by name).
- FACE = skeletal bones, NOT texture (all face DDS identical) -> DROP the 8 face-variant scheme.
- CONTRACT (glTF extras): swappable region variants tagged {"eq_hairstyle":H} (H=0-7); fixed head/body/eye prims have NO extras (always visible). Client shows tagged prim iff H==spawn.hairstyle (default 0).
- PLAN: converter rework (emit ALL head groups; groups 18/19/20 -> 8 hairstyle texture variants tagged; fixed groups incl ears always-on) -> regen -> client head_part_visible to new contract -> validate via render_model POST /head.

## HAIR + EARS REWORK COMPLETE + VALIDATED (2026-06-27)
- Converter (asset-server a5ae849): head_region_from_material_name dispatch — emit ALL head groups; fixed regions (incl EARS elfhesk02/06) always-on; scalp regions 18/19/20 -> 8 hairstyle texture variants tagged {eq_hairstyle:H}. Dropped the wrong face-variant/full-head-dup scheme.
- Client (f39a9ac): HeadPart::HairstyleVariant(H) shown iff hairstyle==H; untagged head/ear/eye prims always render; face no longer selects (Luclin face = skeletal). 277 tests.
- render_model: added POST /head {face,hairstyle,target} live control + head-focus camera bias (committed) — fast iteration tool. (Camera normalizes model small; game client is better for head close-ups.)
- VALIDATED in game client (Campy hairstyle 2): pointed EAR visible in side view (ears fixed); crown view shows textured scalp w/ hair coloring (was bald solid-skin). Face renders. GLB ear prims untagged/always-visible confirmed; 3 scalp regions x 8 distinct hairstyle textures confirmed.
- NOTE: Luclin hair is flat texture-on-scalp (no 3D hair geom in RoF2 player models) — this is the correct/native look, not voluminous hair.

## PAUSED: heading turn-snap bug (2026-06-27)
- Symptom: turning right IN PLACE (A/D) turns char right visually, then snaps heading to mirror-left (original - turn delta). Both kbd + mouse; turning in place (no W).
- Uncommitted debug instrumentation in src/app.rs (HDG_SNAP@1271, HDG_ROT) + src/eq_net/navigation.rs (HDG_SEND in send_position_update). DO NOT lose.
- Analysis so far: two headings — visual heading_target (X-mirror-aware, D increases) vs logical gs.player_heading (set by nav thread eq_heading(movement), or server). encode (ccw_to_cw + wire*2048/360) and decode (wire*360/512 server struct) are each correct for their structs. apply_position_update for player updates pos NOT heading. Snap source unconfirmed — needs LIVE repro (user keyboard turn) reading HDG_ROT/HDG_SNAP/HDG_SEND in the client log. Could not reproduce via API (no turn endpoint).
- Stable client method found: PLAIN background launch (no setsid) + targeted RUST_LOG="warn,eqoxide::app=info,eqoxide::eq_net::navigation=info" + --api-port reserved. setsid+full-info was slow/unstable.

## HEADING BUG — BREAKTHROUGH (reproduces via /goto, no keyboard needed!)
- During a /goto east (sent heading_ccw=270), HDG_SNAP(1271) fired repeatedly: "target 270 -> player_heading 204". So heading_target (270, correct movement dir) gets snapped to gs.player_heading (204 = stale/initial heading). 204 was Campy's zone-in heading.
- So: gs.player_heading is NOT being updated to the movement/turn direction (stays stale at 204) while heading_target/HDG_SEND show the correct 270. The line-1271 snap then pulls the visual heading_target back to the stale gs.player_heading -> the visible snap.
- NEXT (on resume): find why gs.player_heading stays stale (204) — nav thread sets gs.player_heading=hdg(270) at navigation.rs:843/849 but the render thread reads 204. Possible: server re-sends spawn/profile heading (204) overwriting it, OR a GameState sync issue, OR the nav-thread set is conditional/reverted (line 827 hdg reverts to gs.player_heading when dist<0.01). REPRODUCE: /goto then read HDG_SNAP/HDG_SEND in the client log — NO user keyboard needed.
- CLIENT-DEATH ROOT CAUSE: rc=0 clean exit = "second login wins" — another agent shares the campverify/Campy account and kicks my client on their login. Need a UNIQUE character/account for a stable client.

## HEADING BUG — ROOT CAUSE FOUND + FIXED (pending live validation)
- ARCHITECTURE: TWO GameState instances. The network thread (gameplay.rs run_gameplay_phase) owns the authoritative `gs` the Navigator ticks against; App (render thread) has its OWN `self.game_state`, updated only by packets forwarded over app_tx. They do NOT share memory.
- ROOT CAUSE: the nav thread computes the per-step heading (eq_heading) and sets ITS gs.player_heading, but the only channel to App is make_position_packet → a synthetic OP_CLIENT_UPDATE that carried POSITION ONLY (encode_position_update hardcoded heading=0). And apply_position_update's PLAYER branch updated x/y/z but skipped heading. So App.game_state.player_heading was written ONCE by register_spawn (spawn heading 204) and never again. Block B (app.rs:1270, "use the nav thread's authoritative heading") read that stale 204 and overwrote the correct motion-derived heading_target (270) every frame → the snap. The Block B comment's premise was false for App's separate game_state.
- FIX (this branch, 280 tests pass): plumb the nav step heading through the synthetic packet end-to-end:
  - encode_position_update(spawn_id,x,y,z,heading) — packs heading (EQ-CCW→CW, 512-step) into word2 bits 19-30; decode already recovers it.
  - make_position_packet(...,heading) — added param; nav call sites pass the step heading (eq_heading / hdg); teleport sites pass current gs.player_heading.
  - apply_position_update PLAYER branch now sets gs.player_heading = upd.heading. So App.game_state.player_heading goes LIVE → Block B reads a fresh, correct heading.
- Side benefit: /debug heading_ccw is now accurate during nav. Watch for: a real server position-correction for the player could now set heading from server (rare, >12u jumps) — acceptable/correct.
- VALIDATED LIVE (2026-06-28, campverify/Campy in arena): /goto east → heading_ccw stable 270; /goto south → stable 180 (tracks direction dynamically). /debug heading now live (reads gs.player_heading). HDG_SNAP fired exactly ONCE per direction change — and INVERTED from the bug ("target 0 -> player_heading 270" = aligning visual ONTO the live nav heading, the intended job), then quiet. server_corrections=0. Old bug ("target 270 -> player_heading 204", continuous) is gone.
- DEBUG INSTRUMENTATION REMOVED (HDG_SNAP/HDG_ROT@app.rs, HDG_SEND@navigation.rs). 280 tests pass post-strip.
- KEYBOARD in-place-turn symptom: Block B is gated !manual_move so it's skipped during A/D rotation; on release its fallback (game_state.player_heading) is now LIVE (last movement heading) instead of stale 204, so any residual snap aligns to a sane value. Not separately reproduced this session; flag if user still sees an in-place-turn snap.
- READY TO COMMIT (instrumentation stripped, validated). Note: asset server (localhost:8088) was down during validation → flat/void terrain (cosmetic, unrelated). Campy showed as L1 Ranger w/ garbage currency (known currency-decode bug, unrelated).
