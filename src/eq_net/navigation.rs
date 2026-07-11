//! Player navigation: walk toward a target position in capped steps at 150 ms intervals,
//! sending EQ movement packets and notifying the render loop.

use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;

/// Nav tick interval (ms). Steps are gated to fire no more often than this.
const NAV_TICK_MS: u128 = 150;
/// Native Titanium base run speed in EQ units/second (runspeed 0.7 → 44 u/s; 10 Hz updates of
/// 4.4 u each). Per eq-client-expert, see docs/eq-technical-knowledgebase/player-movement-speed.md.
/// We must NOT move faster than this: even where THIS server tolerates it, others rubber-band or
/// reject motion the real client can't produce.
const RUN_SPEED: f32 = 44.0;
use crate::eq_net::protocol::*;
use crate::eq_net::transport::{AppPacket, EqStream};
use crate::game_state::{GameState, ZonePoint};
use crate::http::{AttackReq, BuyReq, SellReq, TradeReq, TradeCmd, MerchantShared, DoorClickReq, DoorsShared, MoveReq, GiveReq, InventoryShared, LootReq, MessagesShared, ChatEventsShared, ChatSendShared, CastReq, MemSpellReq, SitReq, ConsiderReq, CampReq, CampCmd, EntityIds, EntityPositions, GotoTarget, HailReq, SayReq, TargetReq, WhoReq, TaskLog, ZoneCrossReq, ZonePoints, ControllerShared, NavIntent, PosCorrection, DialogueShared, DialogueClickReq, NavStateShared};
use crate::movement::MoveIntent;

/// Min interval (ms) between OP_ClientUpdate sends while moving (native `0x118` = 280 ms).
const POS_SEND_MOVING_MS: u128 = 280;
/// Forced keepalive interval (ms) when idle (native `0x514` = 1300 ms).
const POS_SEND_KEEPALIVE_MS: u128 = 1300;
/// Interval (ms) between OP_FloatListThing (movement-history) sends. The server's MQGhost detector
/// (`cheat_manager.cpp`) trips ~70s after movement if this packet never arrives, then re-flags on
/// every movement check. Sending one benign entry every 30s keeps the 70s timer alive (eqoxide#105).
const MOVEMENT_HISTORY_MS: u128 = 30_000;

/// Build a RoF2 OP_FloatListThing payload: one `UpdateMovementEntry` (packed, 17 bytes) at the given
/// server position. `type = Collision` (1) is a normal move — it resets the server's movement-history
/// timer without tripping the TeleportA/ZoneLine special-cases in `ProcessMovementHistory`. Field
/// order matches EQEmu `UpdateMovementEntry`: Y(f32)@0, X(f32)@4, Z(f32)@8, type(u8)@12, ts(u32)@13.
pub fn build_movement_history(x: f32, y: f32, z: f32) -> Vec<u8> {
    const TYPE_COLLISION: u8 = 1; // UpdateMovementType::Collision — benign, skips teleport/zoneline checks
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0);
    let mut b = Vec::with_capacity(17);
    b.extend_from_slice(&y.to_le_bytes()); // Y @0 (server north)
    b.extend_from_slice(&x.to_le_bytes()); // X @4 (server east)
    b.extend_from_slice(&z.to_le_bytes()); // Z @8
    b.push(TYPE_COLLISION);                // type @12
    b.extend_from_slice(&ts.to_le_bytes()); // timestamp @13
    b
}
/// A >12u jump in the network gs player position between ticks that we did NOT stream is a genuine
/// server correction (anti-cheat snap / teleport), handed to the render controller to apply.
const CORRECTION_SQ: f32 = 144.0;

/// Pending state of a quest turn-in (POST /give). The trade window spans multiple nav ticks:
/// we send OP_TradeRequest, then must wait for the server's OP_TradeRequestAck before moving the
/// item into the NPC trade slot. `ticks_waiting` counts nav ticks (~150ms each) for the timeout.
struct GiveState {
    npc_id:        u32,
    ticks_waiting: u32,
}

/// ~3 second ack timeout, in nav ticks (tick gating is ~150ms → 20 ticks ≈ 3s).
const GIVE_ACK_TIMEOUT_TICKS: u32 = 20;

/// OP_TargetCommand payload: ClientTarget_Struct = just the target spawn id (u32).
pub fn build_target_packet(spawn_id: u32) -> Vec<u8> {
    spawn_id.to_le_bytes().to_vec()
}

/// Auto-combat target priority. Prefers the mob currently attacking the player (an add that aggros
/// mid-fight) so the player fights back instead of being beaten unanswered — but keeps the current
/// target when it is itself one of the attackers, so two adds don't cause target thrash. Falls back
/// to a still-valid current target, then the nearest reachable trash mob.
///
/// - `current_valid`: the current target is alive and reachable.
/// - `current_is_attacker`: the current target has swung at the player recently.
/// - `attacker`: a recent attacker that is alive + reachable (the add to engage), if any.
pub fn pick_combat_target(
    current: Option<u32>,
    current_valid: bool,
    current_is_attacker: bool,
    attacker: Option<u32>,
    nearest_trash: Option<u32>,
) -> Option<u32> {
    // Already fighting one of our attackers — stay on it (don't thrash to a second add).
    if current_valid && current_is_attacker {
        return current;
    }
    // An add is hitting us and isn't our current target — engage it.
    if let Some(a) = attacker {
        return Some(a);
    }
    // Nobody attacking us; finish the current target if it's still good, else pick fresh trash.
    if current_valid {
        return current;
    }
    nearest_trash
}

/// OP_Consider payload: Consider_Struct (28 bytes). The client fills playerid+targetid;
/// the server replies with the same opcode carrying faction (con standing) + level
/// (con color). Size must be exactly 28 or EQEmu rejects it.
pub fn build_consider_packet(player_id: u32, target_id: u32) -> Vec<u8> {
    // RoF2 Consider_Struct is 20 bytes (rof2_structs.h): playerid(u32)@0, targetid(u32)@4,
    // faction(u32)@8, level(u32)@12, pvpcon(u8)@16, pad[3]. (RoF2 dropped Titanium's cur_hp/max_hp,
    // so it's 20 not 28.) The old 28-byte send failed the server's DECODE_LENGTH_EXACT, so the
    // consider was silently dropped and no OP_Consider reply ever came back — con returned nothing
    // (#273). Only playerid/targetid are read by the server; the rest are zero.
    let mut buf = vec![0u8; 20];
    buf[0..4].copy_from_slice(&player_id.to_le_bytes());
    buf[4..8].copy_from_slice(&target_id.to_le_bytes());
    buf
}

/// RoF2 `BookRequest_Struct` (fixed 8216 bytes, rof2_structs.h:2899) for OP_ReadBook — reads a
/// book/note item's text (#288). Fixed-size: the server's DECODE_LENGTH_EXACT rejects any other
/// length. Layout: window(u32)@0, invslot(TypelessInventorySlot 8B)@4, type(u32)@12,
/// target_id(u32)@16, can_cast(u8)@20, can_scribe(u8)@21, txtfile(char[8194])@22. The server keys the
/// `books` table by the FILENAME in `txtfile`, so that string is what matters; `invslot` only drives a
/// secondary type/can_scribe override. The server copies txtfile into a 20-char buffer, so a filename
/// ≥20 chars won't resolve — keep it short.
pub fn build_read_book_packet(slot: i16, target_id: u32, filename: &str) -> Vec<u8> {
    let mut buf = vec![0u8; 8216];
    buf[0..4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());   // window = new
    // invslot: TypelessInventorySlot_Struct { Slot, SubIndex, AugIndex, Unknown01 } — i16 each.
    buf[4..6].copy_from_slice(&slot.to_le_bytes());
    buf[6..8].copy_from_slice(&(-1i16).to_le_bytes());          // SubIndex = -1 (not inside a bag)
    buf[8..10].copy_from_slice(&(-1i16).to_le_bytes());         // AugIndex = -1
    buf[12..16].copy_from_slice(&1u32.to_le_bytes());           // type = 1 (Book) — echoed in the reply
    buf[16..20].copy_from_slice(&target_id.to_le_bytes());
    // txtfile @22: the item's Filename, NUL-terminated. Cap at 19 so the server's 20-char copy stays
    // NUL-terminated and resolves against the books table.
    let fb = filename.as_bytes();
    let n = fb.len().min(19);
    buf[22..22 + n].copy_from_slice(&fb[..n]);
    buf
}

/// Parse an OP_ReadBook REPLY (same 8216-byte struct). The book text is at offset 22, NUL-terminated;
/// RoF2 uses a backtick as the newline marker. Returns the readable text. (#288)
pub fn parse_read_book_reply(payload: &[u8]) -> Option<String> {
    if payload.len() < 23 { return None; }
    let body = &payload[22..];
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    Some(String::from_utf8_lossy(&body[..end]).replace('`', "\n"))
}

/// RoF2 `CastSpell_Struct` (44 bytes, rof2_structs.h): slot(u32), spell_id(u32),
/// inventory_slot(InventorySlot_Struct, 12B), target_id(u32), cs_unknown[2](u32), y/x/z_pos(f32).
/// The client targets RoF2; the old Titanium 20-byte layout failed the server's
/// DECODE_LENGTH_EXACT and every cast was silently dropped — no spell ever landed (eqoxide#42).
///
/// `slot` is the gem index 0-8 (RoF2 CastingSlot::Gem1..Gem9 == server enum, passes through). For a
/// normal memorized-gem cast the server reads only slot/spell_id/target_id and IGNORES
/// inventory_slot (that's for Item/Potion clicky casts), so inventory_slot is sent as an INVALID
/// structured slot (all -1 → RoF2ToServerSlot = SLOT_INVALID). y/x/z are the cast position, only
/// used by ground-targeted AE spells; 0 is fine for single-target casts.
pub fn build_cast_packet(slot: u32, spell_id: u32, target_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 44];
    buf[0..4].copy_from_slice(&slot.to_le_bytes());
    buf[4..8].copy_from_slice(&spell_id.to_le_bytes());
    // inventory_slot @8..20: InventorySlot_Struct all -1 (no clicky item → SLOT_INVALID server-side).
    for b in &mut buf[8..20] { *b = 0xFF; }
    buf[20..24].copy_from_slice(&target_id.to_le_bytes());
    // cs_unknown[2] @24..32 = 0; y_pos@32 / x_pos@36 / z_pos@40 = 0.0 (already zeroed).
    buf
}

/// RoF2 item "clicky" cast — activates an item's click effect (teleport ring / port potion, etc.).
/// Same 44-byte `CastSpell_Struct` as [`build_cast_packet`], but `slot` = `CastingSlot::Item` (22)
/// and `inventory_slot` carries the real possessions slot of the item (as an `InventorySlot_Struct`)
/// instead of SLOT_INVALID. `spell_id` is the item's click effect (`ClickEffectStruct.effect`).
/// Server (common/patches/rof2.cpp): `RoF2ToServerCastingSlot` maps 22→Item 1:1, `RoF2ToServerSlot`
/// decodes the slot, and `Handle_OP_CastSpell` validates the item at that slot has that click
/// effect — so both the slot value and the real inventory_slot must be correct. (eqoxide#193)
pub fn build_item_cast_packet(inventory_slot: u32, spell_id: u32, target_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 44];
    buf[0..4].copy_from_slice(&22u32.to_le_bytes());   // slot = CastingSlot::Item
    buf[4..8].copy_from_slice(&spell_id.to_le_bytes()); // item's click effect spell id
    buf[8..20].copy_from_slice(&rof2_possessions_slot(inventory_slot)); // the item's real slot
    buf[20..24].copy_from_slice(&target_id.to_le_bytes());
    // cs_unknown[2] @24..32 = 0; y/x/z_pos @32..44 = 0.0 (already zeroed).
    buf
}

/// `MemorizeSpell_Struct` (16 bytes): slot, spell_id, scribing, reduction. Identical layout under
/// Titanium and RoF2 (verified against EQEmu rof2_structs.h — no ENCODE), opcode 0x217c.
/// scribing: 0 = scribe a scroll into the spellbook at `slot`; 1 = memorize a known spell into
/// gem `slot` (0-8); 2 = un-memorize. NOTE: scribing (0) only works if the scroll is on the CURSOR
/// (the server reads `m_inv[slotCursor]`); the caller must move it there first. See eqoxide#11.
pub fn build_memorize_packet(slot: u32, spell_id: u32, scribing: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0..4].copy_from_slice(&slot.to_le_bytes());
    buf[4..8].copy_from_slice(&spell_id.to_le_bytes());
    buf[8..12].copy_from_slice(&scribing.to_le_bytes());
    buf
}

/// Encode one RoF2 `InventorySlot_Struct` (12 bytes) for a flat *possessions* slot — equipment
/// 0-22, general inventory 23-32, cursor 33. RoF2 does NOT send a bare slot int; it sends a
/// structured record {Type(i16), Unknown02, Slot(i16), SubIndex(i16), AugIndex(i16), Unknown01}
/// which the server decodes via RoF2ToServerSlot (common/patches/rof2.cpp). For a top-level
/// possessions slot: Type = typePossessions (0), Slot = the flat slot, SubIndex = SLOT_INVALID (-1),
/// AugIndex = SOCKET_INVALID (-1). AugIndex MUST be in [-1, 6) or the server rejects the whole slot
/// as SLOT_INVALID. (Bank/trade/world slots use other Type values + offsets; not handled here.)
fn rof2_possessions_slot(slot: u32) -> [u8; 12] {
    let mut s = [0u8; 12];
    s[0..2].copy_from_slice(&0i16.to_le_bytes());          // Type = typePossessions
    s[2..4].copy_from_slice(&0i16.to_le_bytes());          // Unknown02
    s[4..6].copy_from_slice(&(slot as i16).to_le_bytes()); // Slot
    s[6..8].copy_from_slice(&(-1i16).to_le_bytes());       // SubIndex = SLOT_INVALID (top-level)
    s[8..10].copy_from_slice(&(-1i16).to_le_bytes());      // AugIndex = SOCKET_INVALID
    s[10..12].copy_from_slice(&0i16.to_le_bytes());        // Unknown01
    s
}

/// Encode a RoF2 `InventorySlot_Struct` for any possessions OR bag-content flat slot. Top-level
/// slots (equipment/general/cursor, < 251) → [`rof2_possessions_slot`] (SubIndex = -1). A general-
/// bag content flat slot (251-350) → the parent general slot with `SubIndex` = the 0-9 bag index,
/// which the server decodes to the bagged item (`RoF2ToServerSlot`, common/patches/rof2.cpp:7080:
/// `GENERAL_BAGS_BEGIN + (Slot-GENERAL_BEGIN)*SLOT_COUNT + SubIndex`). This is what makes bagged
/// items movable. (eqoxide#201)
fn rof2_inventory_slot(flat: u32) -> [u8; 12] {
    let Some((parent, sub_index)) = crate::game_state::bag_wire_parent(flat as i32) else {
        return rof2_possessions_slot(flat);
    };
    let mut s = [0u8; 12];
    s[0..2].copy_from_slice(&0i16.to_le_bytes());               // Type = typePossessions
    s[2..4].copy_from_slice(&0i16.to_le_bytes());               // Unknown02
    s[4..6].copy_from_slice(&(parent as i16).to_le_bytes());    // Slot = parent general slot (23-32)
    s[6..8].copy_from_slice(&(sub_index as i16).to_le_bytes()); // SubIndex = bag index 0-9
    s[8..10].copy_from_slice(&(-1i16).to_le_bytes());           // AugIndex = SOCKET_INVALID
    s[10..12].copy_from_slice(&0i16.to_le_bytes());             // Unknown01
    s
}

/// RoF2 `MoveItem_Struct` (28 bytes): from_slot(InventorySlot_Struct,12) + to_slot(…,12) +
/// number_in_stack(u32). NOTE: unlike Titanium's 3×u32 flat struct, RoF2 slots are *structured*
/// (see [`rof2_possessions_slot`]); a flat 12-byte packet fails the server's DECODE_LENGTH_EXACT and
/// the move is silently dropped — that was the real eqoxide#11 scribe failure (the scroll never
/// reached the cursor, so OP_MemorizeSpell scribing=0 saw an empty cursor). number_in_stack = 0 for
/// a whole-item move (equip/cursor/rearrange); a count would split a stack. Handles top-level and
/// general-bag content slots (see [`rof2_inventory_slot`]).
pub fn build_move_item(from_slot: u32, to_slot: u32) -> [u8; 28] {
    let mut buf = [0u8; 28];
    buf[0..12].copy_from_slice(&rof2_inventory_slot(from_slot));
    buf[12..24].copy_from_slice(&rof2_inventory_slot(to_slot));
    buf[24..28].copy_from_slice(&0u32.to_le_bytes()); // number_in_stack = 0 (whole item)
    buf
}

/// Encode one RoF2 `InventorySlot_Struct` (12 bytes) for a *trade-window* slot (handing an item to
/// an NPC / another player). Trade slots are NOT possessions slots: the server decodes typeTrade via
/// RoF2ToServerSlot as `server_slot = TRADE_BEGIN(3000) + Slot`, so the wire `Slot` is the 0-based
/// trade-window index (0 = the NPC's first trade slot). `server_slot` here is the absolute eqoxide
/// slot (SLOT_TRADE_BEGIN..); we subtract TRADE_BEGIN back to the index. Type = typeTrade (3) per
/// rof2_limits.h InventoryTypes; SubIndex/AugIndex = -1 (top-level, not a bag/aug).
fn rof2_trade_slot(server_slot: u32) -> [u8; 12] {
    let index = server_slot.saturating_sub(SLOT_TRADE_BEGIN);
    let mut s = [0u8; 12];
    s[0..2].copy_from_slice(&3i16.to_le_bytes());           // Type = typeTrade
    s[2..4].copy_from_slice(&0i16.to_le_bytes());           // Unknown02
    s[4..6].copy_from_slice(&(index as i16).to_le_bytes()); // Slot = trade-window index
    s[6..8].copy_from_slice(&(-1i16).to_le_bytes());        // SubIndex = SLOT_INVALID
    s[8..10].copy_from_slice(&(-1i16).to_le_bytes());       // AugIndex = SOCKET_INVALID
    s[10..12].copy_from_slice(&0i16.to_le_bytes());         // Unknown01
    s
}

/// RoF2 `MoveItem_Struct` (28 bytes) for moving a *possessions* item (e.g. the cursor) INTO an NPC
/// trade-window slot — the cursor→trade step of a quest hand-in. `from_slot` is a possessions slot
/// (cursor/general); `to_trade_slot` is the absolute trade slot (SLOT_TRADE_BEGIN = first NPC slot).
/// Like [`build_move_item`], a flat 12-byte packet would fail DECODE_LENGTH_EXACT and be dropped —
/// that was the eqoxide#26 turn-in failure (the cursor→trade move never reached the server). (#26)
pub fn build_move_item_to_trade(from_slot: u32, to_trade_slot: u32) -> [u8; 28] {
    let mut buf = [0u8; 28];
    buf[0..12].copy_from_slice(&rof2_possessions_slot(from_slot)); // cursor = possessions
    buf[12..24].copy_from_slice(&rof2_trade_slot(to_trade_slot));
    buf[24..28].copy_from_slice(&0u32.to_le_bytes()); // number_in_stack = 0 (whole item)
    buf
}

/// Native Titanium fall damage for a fall of `height` EQ units. Fall damage is CLIENT-computed in
/// EQ (the server only validates OP_EnvDamage). Model: impact velocity = min(terminal,
/// sqrt(2·g·h)) converted to the client's internal per-update z-velocity units (~5-13); then
/// `fall_score = |z_vel| − 4` (char_counter≈0, no safe-fall skill): ≤0 → no damage, ≥9 → lethal
/// (20000), else a roll in `[0, score²·10]`. Returns (rolled_damage, max_damage). See
/// docs/eq-technical-knowledgebase/falling-physics.md.
pub fn fall_damage(height: f32) -> (u32, u32) {
    const GRAVITY: f32 = 120.0;   // matches the renderer's fall physics
    const TERMINAL: f32 = 128.0;  // native internal z-velocity clamp
    const HZ: f32 = 10.0;         // native position-update rate the formula is calibrated to
    let v = (2.0 * GRAVITY * height.max(0.0)).sqrt().min(TERMINAL);
    let score = v / HZ - 4.0;
    if score <= 0.0 { return (0, 0); }
    if score >= 9.0 { return (20_000, 20_000); }
    let max = (score * score * 10.0) as u32;
    let roll = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos()).unwrap_or(0);
    (if max == 0 { 0 } else { roll % (max + 1) }, max)
}

/// RoF2 `EnvDamage2_Struct` (39 bytes): id@0, damage(u32)@6, dmgtype(u8)@26, constant(u16)@33.
/// The RoF2 server's DECODE reads only id/damage/dmgtype (it forces `constant = 0xFFFF` itself); the
/// rest of the struct is unknown padding. The old Titanium 31-byte layout (dmgtype@22) failed the
/// server's `DECODE_LENGTH_EXACT` and was silently dropped, so a fall's local HP decrement never
/// reached the server and HP desynced (#195). dmgtype: 0xFA=Lava, 0xFB=Drowning, 0xFC=Falling,
/// 0xFD=Trap.
pub fn build_env_damage_packet(player_id: u32, damage: u32, dmgtype: u8) -> Vec<u8> {
    let mut buf = vec![0u8; 39];
    buf[0..4].copy_from_slice(&player_id.to_le_bytes());
    buf[6..10].copy_from_slice(&damage.to_le_bytes());
    buf[26] = dmgtype;
    buf[33..35].copy_from_slice(&0xFFFFu16.to_le_bytes());
    buf
}

/// Accept a translocate offer (#192). The server sends `OP_Translocate` with a `Translocate_Struct`
/// (92 bytes: ZoneID@0, SpellID@4, Caster[64]@12, y@76, x@80, z@84, Complete@88) as a "do you accept?"
/// prompt; the client accepts by echoing the SAME struct back with `Complete@88 = 1`. The RoF2 wire
/// struct isn't transformed, so we just copy the prompt and flip that field. Returns the 92-byte ack.
pub fn build_translocate_ack(prompt: &[u8]) -> Vec<u8> {
    let mut ack = vec![0u8; 92];
    let n = prompt.len().min(92);
    ack[..n].copy_from_slice(&prompt[..n]);
    ack[88..92].copy_from_slice(&1u32.to_le_bytes()); // Complete = 1 → accept
    ack
}

/// Titanium `PetCommand_Struct` (8 bytes): command(u32), target(u32). e.g. PET_ATTACK + a mob
/// spawn id sends the player's pet to attack it.
pub fn build_pet_command(command: u32, target: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 8];
    buf[0..4].copy_from_slice(&command.to_le_bytes());
    buf[4..8].copy_from_slice(&target.to_le_bytes());
    buf
}

/// RoF2 `MerchantClick_Struct` (24 bytes): npc_id@0, player_id@4, command@8 (1=open, 0=close),
/// rate@12, **tab_display@16** (bitmask — b001 = Purchase/Sell tab), unknown02@20 (-1 from client).
/// Titanium was 16 bytes with no tab_display; without tab_display set the RoF2 server opens the
/// window but sends NO merchant inventory, so it must be 1.
fn merchant_click(npc_id: u32, player_id: u32, command: u32) -> [u8; 24] {
    let mut b = [0u8; 24];
    b[0..4].copy_from_slice(&npc_id.to_le_bytes());
    b[4..8].copy_from_slice(&player_id.to_le_bytes());
    b[8..12].copy_from_slice(&command.to_le_bytes());
    b[16..20].copy_from_slice(&1i32.to_le_bytes());    // tab_display = Purchase/Sell
    b[20..24].copy_from_slice(&(-1i32).to_le_bytes());  // unknown02 = -1 (client value)
    b
}

/// Titanium `SpawnAppearance_Struct` (8 bytes): spawn_id(u16), type(u16), parameter(u32).
/// For sit/stand: kind=14 (Animation), parameter=110 (sit) / 100 (stand).
pub fn build_spawn_appearance_packet(spawn_id: u16, kind: u16, parameter: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 8];
    buf[0..2].copy_from_slice(&spawn_id.to_le_bytes());
    buf[2..4].copy_from_slice(&kind.to_le_bytes());
    buf[4..8].copy_from_slice(&parameter.to_le_bytes());
    buf
}

/// OP_ClickDoor payload: ClickDoor_Struct (16 bytes). The lite client is an observer —
/// picklockskill and item_id are 0; the server only uses doorid for lookup and reads
/// skills/inventory from the Client object. player_id is our own spawn id (u16).
pub fn build_click_door(door_id: u8, player_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 16];
    buf[0] = door_id;                                       // doorid @0x00
    // [1..4] action/unknown = 0
    buf[4] = 0;                                             // picklockskill @0x04
    // [8..12] item_id = 0
    buf[12..14].copy_from_slice(&(player_id as u16).to_le_bytes()); // player_id @0x0c
    buf
}

/// OP_AcceptNewTask payload: AcceptNewTask_Struct (12 bytes, all u32): unknown00, task_id
/// (0 = decline all pending offers), task_master_id (the offering NPC's entity id; irrelevant for
/// a decline — only task_id==0 matters per the struct's own EQEmu comment).
pub fn build_accept_new_task(task_id: u32, task_master_id: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 12];
    // buf[0..4] unknown00 = 0
    buf[4..8].copy_from_slice(&task_id.to_le_bytes());
    buf[8..12].copy_from_slice(&task_master_id.to_le_bytes());
    buf
}

/// OP_CancelTask payload: CancelTask_Struct (8 bytes, both u32): SequenceNumber (the task's
/// journal display-order slot, NOT its task_id — see ClientTaskState::CancelTask), type
/// (TaskType — 2 = Quest, the only type this server's content grants).
pub fn build_cancel_task(sequence_number: u32) -> Vec<u8> {
    const TASK_TYPE_QUEST: u32 = 2;
    let mut buf = vec![0u8; 8];
    buf[0..4].copy_from_slice(&sequence_number.to_le_bytes());
    buf[4..8].copy_from_slice(&TASK_TYPE_QUEST.to_le_bytes());
    buf
}

/// OP_GMTraining open request (GMTrainee_Struct, 448 bytes): npcid@0, playerid@4, skills[100]@8
/// (sent as zeros — the server fills them with the offered CAPS in its reply), unknown[40]@408.
pub fn build_gm_training(npcid: u32, playerid: u32) -> Vec<u8> {
    let mut b = vec![0u8; 448];
    b[0..4].copy_from_slice(&npcid.to_le_bytes());
    b[4..8].copy_from_slice(&playerid.to_le_bytes());
    b
}

/// OP_GMTrainSkill (GMSkillChange_Struct, 12 bytes): npcid u16@0, skillbank u16@4 (0 = normal
/// skills, not languages), skill_id u16@8. Trains one point of `skill_id` at the given trainer.
pub fn build_gm_train_skill(npcid: u32, skill_id: u32) -> Vec<u8> {
    let mut b = vec![0u8; 12];
    b[0..2].copy_from_slice(&(npcid as u16).to_le_bytes());
    b[8..10].copy_from_slice(&(skill_id as u16).to_le_bytes());
    b
}

/// OP_GMEndTraining (GMTrainEnd_Struct, 8 bytes): npcid@0, playerid@4. Closes the training window.
pub fn build_gm_end_training(npcid: u32, playerid: u32) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    b[0..4].copy_from_slice(&npcid.to_le_bytes());
    b[4..8].copy_from_slice(&playerid.to_le_bytes());
    b
}

/// OP_GroupInvite payload: GroupInvite_Struct (148 bytes): invitee_name[64], inviter_name[64],
/// then 5 unknown/zero-filled u32s.
pub fn build_group_invite(invitee_name: &str, inviter_name: &str) -> [u8; 148] {
    let mut buf = [0u8; 148];
    let n = invitee_name.as_bytes().len().min(63);
    buf[0..n].copy_from_slice(&invitee_name.as_bytes()[..n]);
    let n2 = inviter_name.as_bytes().len().min(63);
    buf[64..64 + n2].copy_from_slice(&inviter_name.as_bytes()[..n2]);
    buf
}

/// OP_GroupFollow payload (accepting an invite): GroupFollow_Struct (152 bytes): name1=inviter[64],
/// name2=invitee(us)[64], then 6 unknown/zero-filled u32s.
pub fn build_group_follow(inviter_name: &str, invitee_name: &str) -> [u8; 152] {
    let mut buf = [0u8; 152];
    let n = inviter_name.as_bytes().len().min(63);
    buf[0..n].copy_from_slice(&inviter_name.as_bytes()[..n]);
    let n2 = invitee_name.as_bytes().len().min(63);
    buf[64..64 + n2].copy_from_slice(&invitee_name.as_bytes()[..n2]);
    buf
}

/// OP_GroupDisband payload (leave/kick/decline-cleanup). CONFIRMED LIVE (2026-07-01, task-6
/// validation pass) against a running EQEmu RoF2 zone server: the doc's inferred 128-byte
/// "common" GroupGeneric_Struct is WRONG for this opcode — the server logged
/// `Wrong size on incoming [OP_GroupDisband] (structs::GroupGeneric_Struct): Got [128], expected
/// [148]` and silently dropped the packet (no roster change, no disband on either side). The
/// server actually wants the 148-byte RoF2-namespaced struct (same shape as GroupInvite_Struct):
/// name1[64], name2[64], then 5 trailing zero uint32s. `own_name` is the acting player's own
/// name; `target_name` is who's being removed (self for leave/decline, the kicked member's name
/// for a kick).
pub fn build_group_disband(own_name: &str, target_name: &str) -> [u8; 148] {
    let mut buf = [0u8; 148];
    let n = own_name.as_bytes().len().min(63);
    buf[0..n].copy_from_slice(&own_name.as_bytes()[..n]);
    let n2 = target_name.as_bytes().len().min(63);
    buf[64..64 + n2].copy_from_slice(&target_name.as_bytes()[..n2]);
    buf
}

/// GuildCommand_Struct (RoF2, 140 bytes) — the payload for BOTH OP_GuildInvite and OP_GuildRemove
/// (#295): othername[64]@0 (target / member acted on), myname[64]@64 (sender), u16 guildeqid@128
/// (sender's guild id; server overwrites with our real GuildID if 0), u8 unknown[2]@130,
/// u32 officer@132 (the target RANK on the 0-8 scale — for a plain invite this is GUILD_RECRUIT=8;
/// for a self-leave/remove it's ignored), u32 unknown136. A self-leave is othername == myname.
pub fn build_guild_command(othername: &str, myname: &str, guild_id: u32, rank: u32) -> [u8; 140] {
    let mut buf = [0u8; 140];
    let n = othername.as_bytes().len().min(63);
    buf[0..n].copy_from_slice(&othername.as_bytes()[..n]);
    let n2 = myname.as_bytes().len().min(63);
    buf[64..64 + n2].copy_from_slice(&myname.as_bytes()[..n2]);
    buf[128..130].copy_from_slice(&(guild_id as u16).to_le_bytes());
    buf[132..136].copy_from_slice(&rank.to_le_bytes());
    buf
}

/// GuildInviteAccept_Struct (RoF2, 136 bytes) — reply to an incoming OP_GuildInvite (#295):
/// inviter[64]@0, newmember[64]@64 (us), u32 response@128 (the rank to accept at, 0-8; >=9 declines),
/// u32 guildeqid@132 (the guild being joined).
pub fn build_guild_invite_accept(inviter: &str, newmember: &str, response: u32, guild_id: u32) -> [u8; 136] {
    let mut buf = [0u8; 136];
    let n = inviter.as_bytes().len().min(63);
    buf[0..n].copy_from_slice(&inviter.as_bytes()[..n]);
    let n2 = newmember.as_bytes().len().min(63);
    buf[64..64 + n2].copy_from_slice(&newmember.as_bytes()[..n2]);
    buf[128..132].copy_from_slice(&response.to_le_bytes());
    buf[132..136].copy_from_slice(&guild_id.to_le_bytes());
    buf
}

/// OP_GroupMakeLeader payload: GroupMakeLeader_Struct (456 bytes): Unknown000(u32)=0,
/// CurrentLeader[64], NewLeader[64], Unknown072[324]=0. Only NewLeader is read server-side.
pub fn build_group_make_leader(current_leader: &str, new_leader: &str) -> [u8; 456] {
    let mut buf = [0u8; 456];
    let n = current_leader.as_bytes().len().min(63);
    buf[4..4 + n].copy_from_slice(&current_leader.as_bytes()[..n]);
    let n2 = new_leader.as_bytes().len().min(63);
    buf[68..68 + n2].copy_from_slice(&new_leader.as_bytes()[..n2]);
    buf
}

/// Build a RoF2 `OP_ChannelMessage` for the Say channel (used for NPC hails).
/// chan_num 8 = ChatChannel_Say; the server delivers say text to NPCs within 200
/// units, triggering EVENT_SAY (a "Hail, <name>" message fires the NPC's hail script).
pub fn build_say_packet(sender: &str, target: &str, message: &str) -> Vec<u8> {
    build_channel_message(sender, target, 8, message) // chan_num 8 = ChatChannel_Say
}

/// Build an `OP_ChannelMessage` for an arbitrary chat channel. `target` is the recipient
/// for directed channels (tell), empty for broadcasts (ooc/shout/group). EQEmu ChatChannel:
/// 2 group, 3 shout, 5 OOC, 7 tell, 8 say.
///
/// RoF2 uses a **variable-length, NUL-terminated** wire format — NOT the fixed Titanium
/// `ChannelMessage_Struct`. See EQEmu `common/patches/rof2.cpp` `DECODE(OP_ChannelMessage)`:
///   sender\0 | target\0 | u32 unknown | u32 language | u32 chan_num
///   | u32 unknown | u8 unknown | u32 skill_in_language | message\0
/// Sending the fixed 64-byte-field struct makes the server read an empty target + garbage
/// chan_num, so tells/OOC are silently dropped (no cross-zone routing).
pub fn build_channel_message(sender: &str, target: &str, chan_num: u32, message: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(sender.len() + target.len() + message.len() + 24);
    buf.extend_from_slice(sender.as_bytes()); buf.push(0);
    buf.extend_from_slice(target.as_bytes()); buf.push(0);
    buf.extend_from_slice(&0u32.to_le_bytes());      // unknown
    buf.extend_from_slice(&0u32.to_le_bytes());      // language = CommonTongue
    buf.extend_from_slice(&chan_num.to_le_bytes());  // chan_num
    buf.extend_from_slice(&0u32.to_le_bytes());      // unknown
    buf.push(0);                                     // unknown (u8)
    buf.extend_from_slice(&100u32.to_le_bytes());    // skill_in_language
    buf.extend_from_slice(message.as_bytes()); buf.push(0);
    buf
}

/// Choose a movement delta `(dx, dy)` from the desired `(full_dx, full_dy)` step,
/// sliding along a single axis when the diagonal is blocked by a wall. `dx`/`dy` are
/// in EQ server axes: dx = east (server_x), dy = north (server_y). Returns `None`
/// only when fully boxed in. Cast at chest height (z+3) so low lips/stairs don't block.
/// Collision world points are `[east, north, height]` = `[server_x, server_y, server_z]`.
pub fn slide_move(
    col: &crate::assets::Collision,
    px: f32, py: f32, z: f32,
    full_dx: f32, full_dy: f32, radius: f32,
) -> Option<(f32, f32)> {
    let chest = z + 3.0;
    let clear = |sx: f32, sy: f32| col.path_clear([px, py, chest], [px + sx, py + sy, chest], radius);
    if clear(full_dx, full_dy) {
        Some((full_dx, full_dy))
    } else if clear(full_dx, 0.0) {
        Some((full_dx, 0.0))
    } else if clear(0.0, full_dy) {
        Some((0.0, full_dy))
    } else {
        None
    }
}

/// Consecutive no-progress nav ticks (~150 ms each) before the pure-pursuit walker is declared
/// stuck and re-paths. ~3 s — long enough to ride out a brief wall-slide, short enough to recover.
const NAV_STUCK_TICKS: u32 = 20;
/// After this many consecutive no-progress ticks (well before the `NAV_STUCK_TICKS` give-up), the
/// walker commands the controller to hop — net progress has stalled, which is the real "wedged
/// against a fence/cart" signal (sliding along it still looks like motion frame-to-frame). (#41)
const NAV_HOP_TICKS: u32 = 6;
/// On a hard stall (NAV_STUCK_TICKS), drive the reverse (downhill) direction for this many ticks
/// before re-pathing — long enough to clear a wedged slope-face start (~150 ms/tick). (eqoxide#212)
const NAV_BACKOFF_TICKS: u32 = 3;
/// Proactive re-plan (#246): after this many consecutive ticks where the fine 2u plan can't REACH its
/// carrot on the committed coarse route, the route is treated as blocked ahead and re-planned from the
/// current position — long before the ~3 s NAV_STUCK_TICKS give-up, so the walker detours instead of
/// pressing into the obstacle. Small so the reaction is quick (~0.5 s) but > 1 to ride out a carrot
/// that momentarily lands on a fine-impassable lip.
const NAV_LOCAL_STUCK_TICKS: u32 = 3;
/// Minimum ticks between two proactive coarse re-plans, so a persistently-awkward carrot can't thrash
/// the coarse planner every tick (~1 s). The existing stall/back-off recovery still handles a genuine
/// wedge the fresh coarse plan can't route around.
const REPLAN_COOLDOWN_TICKS: u32 = 6;
/// After auto-escaping a sealed interior through an in-zone teleport (#266), block another escape for
/// this long (~10 s at 150 ms/tick) so a goal that's STILL unreachable after the teleport can't
/// ping-pong the char back and forth through the portal. One escape attempt, then it walks/stalls.
const PORTAL_COOLDOWN_TICKS: u32 = 66;
/// A path segment longer than this (horizontal) is a find_path JUMP-EDGE, not a walk — normal
/// adjacent nav cells are ≤ 8·√2 ≈ 11.3u apart, jump-edges span ≥ 16u across a real gap. The walker
/// asks the controller to jump when traversing such a segment. (eqoxide#190)
const JUMP_SEG_MIN: f32 = 12.0;
/// Only fire the jump while within this of the takeoff waypoint — so the leap starts grounded at
/// the near edge and does NOT re-trigger after landing (just under the 8u nav cell). (eqoxide#190)
const JUMP_TAKEOFF_DIST: f32 = 7.0;

/// EQ heading in degrees (0..360) for a movement delta in server axes.
/// EQ convention: heading 0 faces +Y (north) and increases counter-clockwise
/// (90 = -X = west, 180 = -Y = south, 270 = +X = east). A point at heading θ lies
/// at (east, north) = (-sinθ, cosθ), so θ = atan2(-east, north).
pub fn eq_heading(d_east: f32, d_north: f32) -> f32 {
    (-d_east).atan2(d_north).to_degrees().rem_euclid(360.0)
}

/// Plan a route to `goal`, trying progressively harder before giving up (#188 — "A* gives up too
/// easily"):
///   1. a full route at the native radius, then at shrinking radii — a smaller clearance threads
///      narrower gaps between geometry that the full width is boxed out of;
///   2. if no full route exists at any width, a PARTIAL route toward the goal, so a stranded
///      character walks as close as it can (and the walker re-paths from there) instead of not
///      moving at all. Truly boxed in (no progress possible) still returns `None`.
fn plan_path(
    col: &crate::assets::Collision,
    start: [f32; 3],
    goal: [f32; 3],
    avoid: &[[f32; 2]],
    aggro_buffer: f32,
) -> Option<Vec<[f32; 3]>> {
    use crate::movement::PLAYER_RADIUS;
    // This runs on the network thread, so a slow plan delays keepalive/ACK handling. Long-range
    // pathing isn't time-critical, but if it ever gets THIS slow we want it visible — warn so a slow
    // plan can't silently starve the connection the way the zone-line scan did (#204).
    const SLOW_MS: u128 = 100;
    let t0 = std::time::Instant::now();
    let plan = (|| {
        for r in [PLAYER_RADIUS, PLAYER_RADIUS * 0.5, PLAYER_RADIUS * 0.25] {
            if let Some(p) = col.find_path_res(start, goal, r, avoid, false, 8.0, None, aggro_buffer) {
                return Some(p);
            }
        }
        // Fallback: no FULL route at any width — walk a PARTIAL route as far toward the goal as A*
        // can reach (the walker re-paths from the far end). Warn so this degraded routing is visible.
        let partial = col.find_path_res(start, goal, PLAYER_RADIUS, avoid, true, 8.0, None, aggro_buffer);
        if let Some(ref p) = partial {
            tracing::warn!("nav: no full route from ({:.0},{:.0}) to ({:.0},{:.0}) — walking a PARTIAL route ({} wp) toward it",
                start[0], start[1], goal[0], goal[1], p.len());
        }
        partial
    })();
    let ms = t0.elapsed().as_millis();
    if ms >= SLOW_MS {
        tracing::warn!("nav: path plan from ({:.0},{:.0}) to ({:.0},{:.0}) took {ms}ms on the net thread — slower than expected",
            start[0], start[1], goal[0], goal[1]);
    }
    if let Some(p) = plan {
        return Some(p);
    }
    // Isolated start: A* couldn't leave the start cell (it's on a steep slope face / wall lip where
    // the controller slid, and no neighbor cell resolves a walkable floor from there) — so every
    // goto returns no_path and the character is stranded forever (#205). Re-anchor: a clean walkable
    // floor is almost always a few units away laterally (usually just downhill). Retry from the
    // nearest such floor and route from there; the walker then heads off the face to that floor
    // first. Prepend the anchor so the character is steered to the clean ground before the route.
    const RING: [(f32, f32); 12] = [
        (-16.0, 0.0), (16.0, 0.0), (0.0, -16.0), (0.0, 16.0),
        (-16.0, -16.0), (16.0, -16.0), (-16.0, 16.0), (16.0, 16.0),
        (-32.0, 0.0), (32.0, 0.0), (0.0, -32.0), (0.0, 32.0),
    ];
    for (dx, dy) in RING {
        let (ax, ay) = (start[0] + dx, start[1] + dy);
        // A floor reachable from the char's height (generous down-search finds ground below a face).
        let Some(af) = col.nearest_floor(ax, ay, start[2], 20.0, 100.0) else { continue };
        let anchor = [ax, ay, af];
        if let Some(mut p) = col.find_path_res(anchor, goal, PLAYER_RADIUS, avoid, true, 8.0, None, aggro_buffer) {
            // Only worthwhile if the re-anchored start could actually move (more than the lone
            // start cell A* was stuck on) — otherwise it's the same dead spot.
            if p.len() > 1 {
                p.insert(0, anchor);
                tracing::warn!("nav: start isolated at ({:.0},{:.0},{:.0}) — re-anchored to clean floor ({:.0},{:.0},{:.0})",
                    start[0], start[1], start[2], ax, ay, af);
                return Some(p);
            }
        }
    }
    None
}

/// What the walker should do on reaching (near) its goal, kept pure so the follow-vs-goto distinction
/// is unit-tested off the tick. `Arrived` = a one-shot /goto is done → stop for good. `FollowHold` = a
/// /follow chase has caught up → stand near the leader but STAY latched so it re-engages when the
/// leader moves (#268). `Drive` = not there yet → keep walking.
#[derive(Debug, PartialEq, Eq)]
enum ArrivalAction { Drive, Arrived, FollowHold }

/// Arrival radius for a one-shot /goto (melee range is ~14u, so 2u keeps us well inside it).
const STOP_DIST: f32 = 2.0;
/// A /follow settles up to this far behind the leader (a bit behind, still in group range).
const FOLLOW_DIST: f32 = 10.0;

/// Stop within 2u for a one-shot /goto; a /follow settles up to FOLLOW_DIST behind the leader.
fn arrival_action(gdist: f32, following: bool) -> ArrivalAction {
    if following {
        if gdist <= FOLLOW_DIST { ArrivalAction::FollowHold } else { ArrivalAction::Drive }
    } else if gdist <= STOP_DIST {
        ArrivalAction::Arrived
    } else {
        ArrivalAction::Drive
    }
}

/// A pure-pursuit carrot: the point `reach` units along `path` (starting from segment `start_i`),
/// measured from the projection of `from` onto that segment. Returns `[east, north, z]`, carrying the
/// z of the segment the carrot lands on. Used at two scales: a far carrot (~LOCAL_REACH) as the fine
/// plan's goal, and a near carrot (LOOK_AHEAD) along the fine plan as the steering aim.
pub(crate) fn carrot_along(path: &[[f32; 3]], start_i: usize, from: [f32; 2], reach: f32) -> Option<[f32; 3]> {
    let a = *path.get(start_i)?;
    let b = path.get(start_i + 1).copied().unwrap_or(a);
    let ab = [b[0] - a[0], b[1] - a[1]];
    let l2 = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if l2 < 1e-6 { 0.0 } else { (((from[0] - a[0]) * ab[0] + (from[1] - a[1]) * ab[1]) / l2).clamp(0.0, 1.0) };
    let mut cur = [a[0] + ab[0] * t, a[1] + ab[1] * t];
    let (mut rem, mut i, mut cz) = (reach, start_i, b[2]);
    loop {
        match path.get(i + 1).copied() {
            Some(bp) => {
                cz = bp[2];
                let d = [bp[0] - cur[0], bp[1] - cur[1]];
                let dl = (d[0] * d[0] + d[1] * d[1]).sqrt();
                if dl >= rem || i + 2 >= path.len() {
                    let c = if dl < 1e-6 { cur } else { [cur[0] + d[0] * (rem / dl).min(1.0), cur[1] + d[1] * (rem / dl).min(1.0)] };
                    break Some([c[0], c[1], cz]);
                }
                rem -= dl; cur = [bp[0], bp[1]]; i += 1;
            }
            None => break Some([cur[0], cur[1], cz]),
        }
    }
}

pub struct Navigator {
    goto_target:      GotoTarget,
    /// Live nav state for GET /v1/observe/debug (#166): idle|navigating|arrived|no_path|blocked.
    nav_state:        NavStateShared,
    goto_entity:      crate::http::GotoEntity,
    entity_positions: EntityPositions,
    entity_ids:       EntityIds,
    zone_points:      ZonePoints,
    task_log:         TaskLog,
    task_offers_shared:    crate::http::TaskOffersShared,
    completed_tasks_shared: crate::http::CompletedTasksShared,
    accept_task:           crate::http::AcceptTaskReq,
    cancel_task:           crate::http::CancelTaskReq,
    group:             crate::http::GroupShared,
    group_invite:      crate::http::GroupInviteReq,
    trainer_open_req:  crate::http::TrainerOpenReq,
    trainer_train_req: crate::http::TrainerTrainReq,
    group_accept:      crate::http::GroupAcceptReq,
    group_decline:     crate::http::GroupDeclineReq,
    group_leave:       crate::http::GroupLeaveReq,
    group_kick:        crate::http::GroupKickReq,
    group_make_leader: crate::http::GroupMakeLeaderReq,
    zone_cross:       ZoneCrossReq,
    hail:             HailReq,
    say:              SayReq,
    target:           TargetReq,
    /// GET /v1/observe/who registers a oneshot here; drained in `tick` to send OP_WhoAllRequest. (#300)
    who_req:          WhoReq,
    /// Held between sending the request and receiving OP_WhoAllResponse; fired by `fulfill_who`. (#300)
    pending_who:      Option<tokio::sync::oneshot::Sender<Vec<crate::game_state::WhoEntry>>>,
    attack:           AttackReq,
    buy:              BuyReq,
    sell:             SellReq,
    trade:            TradeReq,
    merchant:         MerchantShared,
    move_req:         MoveReq,
    give:             GiveReq,
    cast:             CastReq,
    mem_spell:        MemSpellReq,
    sit:              SitReq,
    consider:         ConsiderReq,
    /// Manual pet command (POST /v1/pet/command or a Pet-window button): one OP_PetCommands
    /// command byte (PET_ATTACK/PET_BACKOFF/…), drained once per tick. Attack uses the current
    /// target; see the drain in `tick`.
    pet_cmd:          crate::http::PetCmdReq,
    /// Camp request slot, shared with the gameplay loop. The nav thread only WRITES it — when the
    /// `/camp` chat keyword is typed it pushes a `Toggle` here instead of sending the text as Say.
    camp:             CampReq,
    /// In-progress quest turn-in (POST /give), or None when idle. Drives the trade-window
    /// state machine across nav ticks (request → wait for ack → move item + accept).
    give_state:       Option<GiveState>,
    /// Shared inventory snapshot (published each tick for GET /inventory) and the pending
    /// POST /loot corpse request (drained into gs.pending_loot to reuse the auto-loot loop).
    inventory:        InventoryShared,
    loot:             LootReq,
    door_click:       DoorClickReq,
    /// Snapshot of the current zone's doors, published each tick for GET /doors.
    doors_shared:     DoorsShared,
    messages:         MessagesShared,
    /// Snapshot of the current NPC-dialogue choices (published each tick for GET
    /// /v1/observe/dialogue) and the pending POST /v1/interact/dialogue click request (drained
    /// into an OP_ItemLinkClick). (#120)
    dialogue:         DialogueShared,
    dialogue_click:   DialogueClickReq,
    chat_events:      ChatEventsShared,
    chat_send:        ChatSendShared,
    collision:        crate::assets::SharedCollision,
    maps_dir:         std::path::PathBuf,
    current_zone:     String,
    last_zone_cross:  Instant,
    position_seq:     u16,
    last_tick:        Instant,
    /// Whether auto-attack is currently engaged (set by the /attack toggle). While true and a
    /// target is set, the nav thread keeps the player facing the target so melee swings land.
    auto_attack:      bool,
    /// Cached A* waypoints for the current goto goal (routes around walls). `path_i` is the
    /// current waypoint; `path_goal` is the goal these waypoints were computed for (recompute
    /// when the goal changes). Empty path = straight-line fallback.
    path:             Vec<[f32; 3]>,  // [east, north, floor_z] per waypoint
    path_i:           usize,
    path_goal:        Option<(f32, f32, f32)>,
    /// Fine LOCAL A* plan (2u grid, bounded) the walker actually steers along, re-run each tick from
    /// the player toward a carrot ~LOCAL_REACH ahead on the coarse `path`. It threads sub-8u detail
    /// (thin ramps, narrow openings) the coarse grid can't see. Empty = coarse aim (fine plan failed
    /// or no coarse path). Two-tier planner (#nav-multires).
    local_path:       Vec<[f32; 3]>,
    /// The spawn id the pet was last ordered to attack (avoids re-spamming OP_PetCommands every
    /// tick). Reset when the target changes; see the auto-pet-combat block.
    last_pet_target:  Option<u32>,
    /// `Some(landing_z)` while a controlled fall is in progress (the walker descends at the native
    /// rate until reaching it, then applies fall damage); `fall_start_z` is where the fall began.
    falling:          Option<f32>,
    fall_start_z:     f32,
    /// No-progress detector for the path walker (see `nav_progress`). `stuck_best` is the
    /// closest distance reached toward the current aim, `stuck_ticks` the consecutive
    /// no-progress ticks, and `stuck_i` the `path_i` the detector is tracking (so it resets
    /// when the aim waypoint changes). Without this the walker can wedge into geometry and
    /// slide in place forever with no stop log (gfaydark/neriakc stalls, #4/#2).
    stuck_best:       f32,
    stuck_ticks:      u32,
    stuck_i:          usize,
    /// Stall-recovery re-paths WITHOUT forward progress; capped so a truly unreachable snag stops
    /// instead of re-pathing forever, but reset whenever the walker gets meaningfully closer to the
    /// goal — so a long cross-zone journey that clears several distinct wedges isn't killed by the
    /// cap while it's still making progress (#229).
    nav_repaths:      u32,
    /// Closest straight-line distance to the current goal reached so far; when it drops by
    /// `REPATH_RESET_DIST` the re-path budget resets (real progress → the last wedge is behind us).
    nav_best_gdist:   f32,
    /// Downhill back-off (eqoxide#212): when the walker wedges on a slope face, drive the reverse
    /// direction for this many ticks before re-pathing, so the re-plan starts from cleaner ground.
    /// 0 = not backing off.
    backoff_ticks:    u32,
    backoff_dir:      [f32; 2],
    /// Proactive coarse re-plan (#246). The coarse 8u route is committed at goal-change and, without
    /// this, only re-planned on a ~3s no-progress stall — so an obstacle the coarse grid skims but the
    /// fine 2u planner can't thread makes the walker press into it for seconds while the overlay (which
    /// re-plans continuously) already shows a clean detour. `local_stuck_ticks` counts consecutive
    /// ticks the fine plan fails to REACH its coarse carrot (blocked ahead); after a few, `replan_coarse`
    /// is armed so the next tick re-plans the coarse route from the CURRENT position (routing around
    /// BEFORE the stall). `replan_cooldown` throttles those re-plans so they don't thrash.
    local_stuck_ticks: u32,
    replan_coarse:     bool,
    replan_cooldown:   u32,
    /// Auto-escape a SEALED interior via an in-zone teleport (#266). When a /goto goal is walk-
    /// unreachable and the nearest zone-line region is a translocator that loops back to THIS zone (the
    /// Qeynos guild-vault waterfall), the goto is temporarily redirected to that region — the char
    /// walks in via the normal machinery, the auto-cross teleports it out, and the post-teleport jump
    /// restores the real goal. `escape_return` holds the real goal while escaping; `last_walk_pos`
    /// detects the teleport jump; `portal_cooldown` blocks an immediate re-escape so a still-unreachable
    /// goal can't ping-pong through the portal forever.
    escape_return:     Option<(f32, f32, f32)>,
    last_walk_pos:     [f32; 3],
    portal_cooldown:   u32,
    /// Single-authority controller integration (design §2). `controller_view` is the render
    /// thread's authoritative position snapshot we stream to the server; `nav_intent` is the
    /// `/goto` planner's per-frame wish written for the render controller; `pos_correction` hands a
    /// genuine server correction back to the controller.
    controller_view:  ControllerShared,
    nav_intent:       NavIntent,
    pos_correction:   PosCorrection,
    /// Draw-only mirror of the walker's committed `path`/`local_path`, published each tick for the
    /// nav-debug overlay so it shows what the walker actually follows, not a separate recompute (#246).
    nav_path_view:    crate::http::NavPathView,
    /// Aggro-avoidance knobs from /v1/move/* (#242): whether to route around NPC camps and how wide a
    /// buffer to give them. Read each time a route is (re)planned.
    nav_avoid:        crate::http::NavAvoidShared,
    /// POST /v1/interact/read request: the inventory wire slot of a book/note to read (#288). Drained
    /// each tick; the item's Filename is sent as OP_ReadBook and the server replies with the text.
    read_book:        crate::http::ReadBookReq,
    /// Guild roster + identity published each tick for GET /v1/guild/roster + /observe/debug (#295).
    guild:            crate::http::GuildShared,
    /// POST /v1/guild/{invite,accept,leave,remove} — one queued guild action, drained each tick (#295).
    guild_action:     crate::http::GuildActionReq,
    /// Last time we sent OP_FloatListThing (movement history) — the anti-MQGhost keepalive (#105).
    last_movement_history_send: Instant,
    /// Last position we streamed, and the last-send timestamp (for the 280 ms / 1300 ms cadence).
    last_streamed:    [f32; 3],
    last_pos_send:    Instant,
    streamed_init:    bool,
}

impl Navigator {
    pub fn new(
        goto_target:      GotoTarget,
        nav_state:        NavStateShared,
        goto_entity:      crate::http::GotoEntity,
        entity_positions: EntityPositions,
        entity_ids:       EntityIds,
        zone_points:      ZonePoints,
        task_log:         TaskLog,
        task_offers_shared:    crate::http::TaskOffersShared,
        completed_tasks_shared: crate::http::CompletedTasksShared,
        accept_task:           crate::http::AcceptTaskReq,
        cancel_task:           crate::http::CancelTaskReq,
        group:             crate::http::GroupShared,
        group_invite:      crate::http::GroupInviteReq,
    trainer_open_req:  crate::http::TrainerOpenReq,
    trainer_train_req: crate::http::TrainerTrainReq,
        group_accept:      crate::http::GroupAcceptReq,
        group_decline:     crate::http::GroupDeclineReq,
        group_leave:       crate::http::GroupLeaveReq,
        group_kick:        crate::http::GroupKickReq,
        group_make_leader: crate::http::GroupMakeLeaderReq,
        zone_cross:       ZoneCrossReq,
        hail:             HailReq,
        say:              SayReq,
        target:           TargetReq,
        who_req:          WhoReq,
        attack:           AttackReq,
        buy:              BuyReq,
        sell:             SellReq,
        trade:            TradeReq,
        merchant:         MerchantShared,
        move_req:         MoveReq,
        give:             GiveReq,
        inventory:        InventoryShared,
        loot:             LootReq,
        door_click:       DoorClickReq,
        doors_shared:     DoorsShared,
        messages:         MessagesShared,
        dialogue:         DialogueShared,
        dialogue_click:   DialogueClickReq,
        chat_events:      ChatEventsShared,
        chat_send:        ChatSendShared,
        cast:             CastReq,
        mem_spell:        MemSpellReq,
        sit:              SitReq,
        consider:         ConsiderReq,
        pet_cmd:          crate::http::PetCmdReq,
        collision:        crate::assets::SharedCollision,
        maps_dir:         std::path::PathBuf,
        camp:             CampReq,
        controller_view:  ControllerShared,
        nav_intent:       NavIntent,
        pos_correction:   PosCorrection,
        nav_path_view:    crate::http::NavPathView,
        nav_avoid:        crate::http::NavAvoidShared,
        read_book:        crate::http::ReadBookReq,
        guild:            crate::http::GuildShared,
        guild_action:     crate::http::GuildActionReq,
    ) -> Self {
        Navigator {
            goto_target,
            nav_state,
            goto_entity,
            entity_positions,
            entity_ids,
            zone_points,
            task_log,
            task_offers_shared,
            completed_tasks_shared,
            accept_task,
            cancel_task,
            group,
            group_invite,
            trainer_open_req,
            trainer_train_req,
            group_accept,
            group_decline,
            group_leave,
            group_kick,
            group_make_leader,
            zone_cross,
            hail,
            say,
            target,
            who_req,
            pending_who: None,
            attack,
            buy,
            sell,
            trade,
            merchant,
            move_req,
            give,
            cast,
            mem_spell,
            sit,
            consider,
            pet_cmd,
            camp,
            give_state: None,
            inventory,
            loot,
            door_click,
            doors_shared,
            messages,
            dialogue,
            dialogue_click,
            chat_events,
            chat_send,
            collision,
            maps_dir,
            current_zone: String::new(),
            last_zone_cross: Instant::now(),
            position_seq: 0,
            last_tick: Instant::now(),
            auto_attack: false,
            path: Vec::new(),
            path_i: 0,
            path_goal: None,
            local_path: Vec::new(),
            last_pet_target: None,
            falling: None,
            fall_start_z: 0.0,
            stuck_best: f32::MAX,
            stuck_ticks: 0,
            stuck_i: 0,
            nav_repaths: 0,
            nav_best_gdist: f32::MAX,
            backoff_ticks: 0,
            local_stuck_ticks: 0,
            replan_coarse: false,
            replan_cooldown: 0,
            escape_return: None,
            last_walk_pos: [0.0, 0.0, 0.0],
            portal_cooldown: 0,
            backoff_dir: [0.0, 0.0],
            controller_view,
            nav_intent,
            pos_correction,
            nav_path_view,
            nav_avoid,
            read_book,
            guild,
            guild_action,
            last_streamed: [0.0, 0.0, 0.0],
            last_pos_send: Instant::now(),
            last_movement_history_send: Instant::now(),
            streamed_init: false,
        }
    }

    /// Copy all entity positions from `gs` into the shared entity map
    /// (used by the HTTP /entities endpoint and /goto by-name lookup).
    pub fn sync_entities(&self, gs: &GameState) {
        let mut map = self.entity_positions.lock().unwrap();
        let mut ids = self.entity_ids.lock().unwrap();
        // Full replace: clear stale entries so positions reflect the current zone only.
        map.clear();
        ids.clear();
        for (&id, e) in &gs.entities {
            map.insert(e.name.clone(), (e.x, e.y, e.z));
            ids.insert(e.name.clone(), id);
        }
    }

    /// Publish the native Task-system quest log from `gs` into the shared slot (GET /quests/log).
    pub fn sync_tasks(&self, gs: &GameState) {
        let mut log = self.task_log.lock().unwrap();
        log.clear();
        let mut tasks: Vec<_> = gs.tasks.values().cloned().collect();
        tasks.sort_by_key(|t| t.task_id);
        log.extend(tasks);
        drop(log);

        let mut offers = self.task_offers_shared.lock().unwrap();
        offers.clear();
        offers.extend(gs.task_offers.iter().cloned());
        drop(offers);

        let mut completed = self.completed_tasks_shared.lock().unwrap();
        completed.clear();
        completed.extend(gs.completed_task_history.iter().cloned());
    }

    /// Publish the group roster from `gs` into the shared slot (GET /v1/group/roster + the UI
    /// roster panel). Looks up each other member's HP% from `gs.entities` by name (group
    /// membership is what unlocks receiving another mob's OP_MobHealth percent, so this reuses
    /// existing Entity.hp_pct rather than needing a new opcode); the player's own HP% comes
    /// directly from `gs.hp_pct` since the player is never in `gs.entities`.
    pub fn sync_group(&self, gs: &GameState) {
        let mut g = self.group.lock().unwrap();
        g.leader = gs.group_leader.clone();
        g.pending_invite = gs.pending_invite.clone();
        g.you_are_leader = !gs.player_name.is_empty() && gs.group_leader == gs.player_name;
        g.members = gs.group_members.iter().map(|m| {
            let hp_pct = if m.name == gs.player_name {
                gs.hp_pct
            } else {
                gs.entities.values().find(|e| e.name == m.name).map(|e| e.hp_pct).unwrap_or(0.0)
            };
            crate::http::GroupMemberView {
                // m.level from OP_GroupUpdateB is a server placeholder (70/65); resolve the real
                // level from our profile / the member's spawn instead. (eqoxide#104)
                name: m.name.clone(), level: gs.group_member_level(&m.name),
                is_leader: m.is_leader, is_merc: m.is_merc,
                tank: m.tank, assist: m.assist, puller: m.puller, offline: m.offline, hp_pct,
            }
        }).collect();
    }

    /// Publish the player's guild identity + roster from `gs` into the shared slot (GET
    /// /v1/guild/roster and the guild fields of /observe/debug). Resolves guild_id → name via the
    /// OP_GuildsList table. (#295)
    pub fn sync_guild(&self, gs: &GameState) {
        let mut g = self.guild.lock().unwrap();
        // GUILD_NONE is 0xFFFFFFFF (and 0 also means none). Normalize both to 0 so the API cleanly
        // reports "no guild" as guild_id 0 / empty name / empty roster.
        let in_guild = gs.player_guild_id != 0 && gs.player_guild_id != 0xFFFF_FFFF;
        if in_guild {
            g.guild_id = gs.player_guild_id;
            g.guild_rank = gs.player_guild_rank;
            g.guild_name = gs.guild_names.get(&gs.player_guild_id).cloned().unwrap_or_default();
            g.members = gs.guild_members.clone();
        } else {
            g.guild_id = 0;
            g.guild_rank = 0;
            g.guild_name.clear();
            g.members.clear();
        }
        g.pending_invite = gs.pending_guild_invite.as_ref().map(|(inviter, _, _)| inviter.clone());
    }

    /// Publish the player's inventory + equipment from `gs` into the shared slot (GET /inventory).
    pub fn sync_inventory(&self, gs: &GameState) {
        let mut inv = self.inventory.lock().unwrap();
        inv.clear();
        inv.extend(gs.inventory.iter().cloned());
    }

    /// Deliver the freshly-parsed `/who all` roster to the pending GET /v1/observe/who (#300). Called
    /// from the gameplay drain loop right after an OP_WhoAllResponse updates `gs.who_roster`. No-op if
    /// no request is in flight (e.g. an unsolicited/duplicate response).
    pub fn fulfill_who(&mut self, gs: &GameState) {
        if let Some(tx) = self.pending_who.take() {
            let _ = tx.send(gs.who_roster.clone());
        }
    }

    /// Publish the open-merchant session from `gs` into the shared slot (GET /trade/list + the HUD
    /// merchant window).
    pub fn sync_merchant(&self, gs: &GameState) {
        let mut m = self.merchant.lock().unwrap();
        m.open = gs.merchant_open.is_some();
        m.merchant_id = gs.merchant_open;
        m.items.clear();
        m.items.extend(gs.merchant_items.iter().cloned());
    }

    /// Publish the in-game message log from `gs` into the shared slot (GET /messages), converting
    /// each LogEntry into a serializable MessageEntry and extracting `[bracketed]` quest keywords
    /// (the same splitter the HUD dialogue panel uses).
    pub fn sync_messages(&self, gs: &GameState) {
        let mut out = self.messages.lock().unwrap();
        out.clear();
        out.extend(gs.messages.iter().map(|m| {
            let keywords = crate::game_state::split_keywords(&m.text).into_iter()
                .filter(|(_, is_kw)| *is_kw)
                .map(|(seg, _)| seg.trim_matches(|c| c == '[' || c == ']').trim().to_string())
                .filter(|k| !k.is_empty())
                .collect();
            crate::http::MessageEntry { kind: m.kind.clone(), text: m.text.clone(), keywords }
        }));
        drop(out);
        // Publish the current clickable NPC-dialogue choices (GET /v1/observe/dialogue, #120).
        *self.dialogue.lock().unwrap() = gs.dialogue_choices.clone();
        // Publish async events (GET /v1/events/*), preserving their stable monotonic ids.
        let mut ev = self.chat_events.lock().unwrap();
        ev.clear();
        ev.extend(gs.chat_events.iter().map(|e| crate::http::Event {
            id: e.id, category: e.category.clone(), kind: e.kind.clone(),
            from: e.from.clone(), directed: e.directed, text: e.text.clone(),
        }));
    }

    /// Publish the current zone's doors from `gs` into the shared slot (GET /doors).
    pub fn sync_doors(&self, gs: &GameState) {
        let mut out = self.doors_shared.lock().unwrap();
        out.clear();
        out.extend(gs.doors.values().map(|d| crate::http::DoorView {
            door_id: d.door_id, name: d.name.clone(),
            x: d.x, y: d.y, z: d.z, heading: d.heading,
            opentype: d.opentype, is_open: d.is_open,
        }));
    }

    /// Sync zone exit points from `gs` into the shared zone_points map.
    /// On zone change, also loads map-label exits from disk as fallback zone points.
    pub fn sync_zone_points(&mut self, gs: &GameState) {
        // On zone change, load map labels from disk as fallback zone points.
        if gs.zone_name != self.current_zone {
            self.current_zone = gs.zone_name.clone();

            // Reset the nav destination + route on a zone change (#248). The old goal/path are in the
            // PREVIOUS zone's coordinate space; kept across a crossing they aim the walker at an
            // arbitrary spot (usually a corner near the arrival point) and wedge it there. A completed
            // crossing IS the "walk to the zone line" goal reached, so the character should come to
            // rest in the new zone; a driver that wants to keep going re-issues /v1/move/* afterward.
            // (This is the zone-boundary sibling of the mid-zone stale-plan bug #246.)
            *self.goto_target.lock().unwrap() = None;
            *self.goto_entity.lock().unwrap() = None;
            *self.nav_intent.lock().unwrap() = None; // stop driving the controller toward the stale aim
            *self.nav_path_view.lock().unwrap() = (Vec::new(), Vec::new()); // clear the overlay line
            self.path.clear();
            self.local_path.clear();
            self.path_goal = None;
            self.path_i = 0;
            self.stuck_i = 0;
            self.stuck_best = f32::MAX;
            self.stuck_ticks = 0;
            self.nav_repaths = 0;
            self.nav_best_gdist = f32::MAX;
            self.backoff_ticks = 0;
            self.local_stuck_ticks = 0;
            self.replan_coarse = false;
            self.replan_cooldown = 0;
            self.falling = None;
            self.set_nav_state("idle");

            let mut shared = self.zone_points.lock().unwrap();
            // Start fresh with server entries.
            shared.clear();
            shared.extend(gs.zone_points.iter().cloned());
            // Load map labels from disk.
            if let Some(zm) = crate::zone_map::ZoneMap::load(&self.maps_dir, &gs.zone_name) {
                let before = shared.len();
                for label in &zm.labels {
                    let lower = label.text.to_lowercase();
                    if !lower.starts_with("to ") { continue; }
                    let dest_zone_id: u16 = if lower.contains("north qeynos") || lower.contains("qeynos2") {
                        2
                    } else if lower.contains("south qeynos") {
                        1 // qeynos south
                    } else {
                        0
                    };
                    if dest_zone_id == 0 { continue; }
                    let dup = shared.iter().any(|zp| {
                        zp.zone_id == dest_zone_id
                            && ((zp.server_x - label.east).powi(2) + (zp.server_y - label.north).powi(2)) < 2500.0
                    });
                    if dup { continue; }
                    shared.push(ZonePoint {
                        iterator: u32::MAX,
                        server_x: label.east,
                        server_y: label.north,
                        server_z: 0.0,
                        heading: 0.0,
                        zone_id: dest_zone_id,
                    });
                    tracing::info!("zone_map: added exit '{}' at ({:.1}, {:.1}) → zone_id={}",
                              label.text, label.east, label.north, dest_zone_id);
                }
                if shared.len() > before {
                    tracing::info!("zone_map: {} fallback exits added (total {})", shared.len() - before, shared.len());
                }
            }
        } else {
            // Same zone: update server entries but keep map labels.
            let mut shared = self.zone_points.lock().unwrap();
            let map_labels: Vec<_> = shared.drain(..)
                .filter(|zp| zp.iterator == u32::MAX)
                .collect();
            shared.extend(gs.zone_points.iter().cloned());
            shared.extend(map_labels);
        }
    }

    /// Publish the current `/move/goto` navigation state for GET /v1/observe/debug (#166):
    /// idle | navigating | arrived | no_path | blocked.
    fn set_nav_state(&self, state: &str) {
        let mut s = self.nav_state.lock().unwrap();
        if *s != state { *s = state.to_string(); }
    }

    /// Is the player slain? Detected the SAME way the render/anim path picks the dead pose
    /// (`cur_hp <= 0` with a known `max_hp`) OR via the OP_Death `player_dead` flag. Using cur_hp —
    /// not just `player_dead` — catches an HP-to-0 update that lands before OP_Death arrives, which is
    /// the window in which a corpse was seen still walking (#238).
    fn is_player_dead(gs: &GameState) -> bool {
        gs.player_dead || (gs.cur_hp <= 0 && gs.max_hp > 0)
    }

    /// Stop all navigation the instant the player is slain (#238): abandon the destination + route +
    /// controller intent so a corpse doesn't keep walking toward the goal, and clear the overlay line.
    /// The route is wiped so a later respawn/relog starts fresh instead of resuming the dead man's
    /// path. Returns true when the player is dead (the caller returns early from the walk tick).
    fn nav_halt_if_dead(&mut self, gs: &GameState) -> bool {
        if !Self::is_player_dead(gs) {
            return false;
        }
        if self.goto_target.lock().unwrap().take().is_some() {
            tracing::info!("NAV: player is dead — abandoning /goto");
        }
        *self.goto_entity.lock().unwrap() = None;      // drop any entity chase
        *self.zone_cross.lock().unwrap() = None;        // drop a queued zone-cross
        *self.nav_intent.lock().unwrap() = None;        // stop driving the controller
        *self.nav_path_view.lock().unwrap() = (Vec::new(), Vec::new()); // clear the overlay line
        self.path.clear();
        self.local_path.clear();
        self.path_goal = None;
        self.path_i = 0;
        self.set_nav_state("idle");
        true
    }

    /// Live NPC-camp positions to route AROUND (aggro-avoidance, #67), excluding NPCs near the
    /// goal (you're walking TO the destination, often a target mob, so its own camp isn't avoided).
    /// The nearest FLOOR-REACHABLE in-zone translocator region (a zone-line region whose destination
    /// is THIS zone — the Qeynos guild-vault waterfall), as a goto target the char can walk INTO to
    /// teleport out (#266). None if no reachable in-zone portal exists.
    ///
    /// Two things this handles (both from find-issues-1's verified goto-Nerissa wedge at (-607,-71,z-14)):
    ///  1. Restrict to IN-ZONE indices, not the nearest zone-line overall — once a stranded char drifts
    ///     toward its goal a normal neighbour-zone exit can become the closest line, and "nearest line
    ///     then check in-zone" returned None from there, so the escape never fired.
    ///  2. Require the region's DRNTP footprint to reach the char's floor height, so walking to its XY
    ///     actually fires the z-EXACT auto-cross — skipping top-of-waterfall-column leaves whose point
    ///     sits high up (the char reaches the XY on the floor, stands below the leaf, never crosses).
    fn find_in_zone_portal(&self, gs: &GameState) -> Option<(f32, f32, f32)> {
        let guard = self.collision.read().unwrap();
        let c = guard.as_ref()?;
        let pos = [gs.player_x, gs.player_y, gs.player_z];
        let in_zone_idxs: Vec<i32> = self.zone_points.lock().unwrap().iter()
            .filter(|zp| zp.zone_id == gs.zone_id)
            .map(|zp| zp.iterator as i32)
            .collect();
        let portal = c.find_reachable_in_zone_line(&in_zone_idxs, pos).map(|(_, l)| (l[0], l[1], l[2]));
        if tracing::enabled!(tracing::Level::DEBUG) {
            let cands: Vec<_> = in_zone_idxs.iter()
                .filter_map(|&idx| c.find_zone_line_near(Some(idx), pos)
                    .map(|(_, l)| (idx, [l[0].round(), l[1].round(), l[2].round()])))
                .collect();
            tracing::debug!("find_in_zone_portal: pos_z={:.0} in_zone_idxs={in_zone_idxs:?} nearest_per_idx={cands:?} chose_reachable={portal:?}", pos[2]);
        }
        portal
    }

    fn aggro_avoid(gs: &GameState, goal: (f32, f32, f32), enabled: bool) -> Vec<[f32; 2]> {
        // `enabled == false` (from `avoid_aggro:false` on /v1/move/*) routes straight through — no
        // avoid points — for when the caller WANTS to path into a mob (#242). Default stays on (#67).
        if !enabled { return Vec::new(); }
        const NEAR_GOAL_SQ: f32 = 55.0 * 55.0;
        gs.entities.values()
            .filter(|e| e.is_npc && !e.dead)
            .filter(|e| { let (dx, dy) = (e.x - goal.0, e.y - goal.1); dx * dx + dy * dy > NEAR_GOAL_SQ })
            .map(|e| [e.x, e.y])
            .collect()
    }

    /// Advance one navigation tick (no-op if fewer than 150 ms have elapsed).
    pub fn tick(
        &mut self,
        stream:  &mut EqStream,
        gs:      &mut GameState,
        app_tx:  &UnboundedSender<AppPacket>,
    ) {
        // POST /loot: queue the requested corpse onto the existing auto-loot pipeline. The gameplay
        // loop drains pending_loot — sends OP_LootRequest, echoes each OP_LootItem to take it, then
        // OP_EndLootRequest. The 500ms delay (loot_queued_at) lets the server register the corpse.
        if let Some(corpse_id) = self.loot.lock().unwrap().take() {
            gs.pending_loot.push_back(corpse_id);
            if gs.loot_queued_at.is_none() {
                gs.loot_queued_at = Some(Instant::now());
            }
            tracing::info!("loot: queued corpse_id={} for looting (via POST /loot)", corpse_id);
        }

        // POST /doors/click or a human door click: send OP_ClickDoor. The door opens
        // visually only when the server replies with OP_MoveDoor.
        if let Some(door_id) = self.door_click.lock().unwrap().take() {
            stream.send_app_packet(OP_CLICK_DOOR, &build_click_door(door_id, gs.player_id));
            tracing::info!("EQ: click door_id={}", door_id);
            gs.log_msg("door", &format!("Clicked door {}", door_id));
        }

        // POST /v1/quests/accept ({"task_id":N}) or /decline (task_id=0): send OP_AcceptNewTask.
        // For a real accept, look up the offering NPC's id from gs.task_offers (task_master_id is
        // required by the struct); a decline sends task_master_id=0 (irrelevant when task_id==0).
        // Either way, the selector window is done with — clear all pending offers.
        if let Some(task_id) = self.accept_task.lock().unwrap().take() {
            let task_master_id = if task_id == 0 {
                0
            } else {
                gs.task_offers.iter().find(|o| o.task_id == task_id).map(|o| o.npc_id).unwrap_or(0)
            };
            stream.send_app_packet(OP_ACCEPT_NEW_TASK, &build_accept_new_task(task_id, task_master_id));
            if task_id == 0 {
                tracing::info!("EQ: quests: declined all pending task offers");
                gs.log_msg("quest", "Declined task offer(s)");
            } else {
                tracing::info!("EQ: quests: accepted task_id={task_id} task_master_id={task_master_id}");
                gs.log_msg("quest", "Accepted task offer");
            }
            gs.task_offers.clear();
            // Mirror into the RENDER GameState: an EMPTY OP_TaskSelectWindow parses to zero offers
            // (apply_task_select_window), clearing render-side task_offers so the selector closes.
            let _ = app_tx.send(AppPacket { opcode: OP_TASK_SELECT_WINDOW, payload: Vec::new() });
        }

        // POST /v1/quests/cancel ({"task_id":N}): abandon an active task. OP_CancelTask addresses
        // the task by its journal sequence_number, not task_id — see build_cancel_task.
        if let Some(task_id) = self.cancel_task.lock().unwrap().take() {
            if let Some(task) = gs.tasks.get(&task_id) {
                let seq = task.sequence_number;
                stream.send_app_packet(OP_CANCEL_TASK, &build_cancel_task(seq));
                tracing::info!("EQ: quests: cancelled task_id={task_id} sequence_number={seq}");
                gs.log_msg("quest", "Cancelled task");
            } else {
                tracing::warn!("EQ: quests: cancel requested for unknown task_id={task_id} — ignoring");
            }
        }

        // POST /v1/group/invite {"name":"X"}: send OP_GroupInvite.
        if let Some(target) = self.group_invite.lock().unwrap().take() {
            stream.send_app_packet(OP_GROUP_INVITE, &build_group_invite(&target, &gs.player_name));
            tracing::info!("EQ: group: invited {target}");
            gs.log_msg("group", &format!("Invited {target} to group"));
        }

        // POST /v1/group/accept: send OP_GroupFollow. Optimistically clear pending_invite now —
        // the real roster confirmation arrives via OP_GroupUpdateB/OP_GroupAcknowledge.
        if self.group_accept.lock().unwrap().take().is_some() {
            if let Some(inviter) = gs.pending_invite.take() {
                stream.send_app_packet(OP_GROUP_FOLLOW, &build_group_follow(&inviter, &gs.player_name));
                // Mirror into the RENDER GameState (apply_group_invite set its pending_invite too,
                // and only a disband would ever clear it) so the forced-open Group window relaxes.
                let _ = app_tx.send(AppPacket { opcode: OP_UI_CLEAR_INVITE, payload: Vec::new() });
                tracing::info!("EQ: group: accepted invite from {inviter}");
                gs.log_msg("group", &format!("Accepted group invite from {inviter}"));
            }
        }

        // POST /v1/group/decline: RoF2 has no working OP_GroupCancelInvite, so send a defensive
        // OP_GroupDisband(self, self) cleanup instead.
        if self.group_decline.lock().unwrap().take().is_some() {
            if let Some(inviter) = gs.pending_invite.take() {
                stream.send_app_packet(OP_GROUP_DISBAND, &build_group_disband(&gs.player_name, &gs.player_name));
                // A decline produces NO server packet at all — mirror the cleared invite into the
                // RENDER GameState or its pending_invite (and the invite dialog) lingers forever.
                let _ = app_tx.send(AppPacket { opcode: OP_UI_CLEAR_INVITE, payload: Vec::new() });
                tracing::info!("EQ: group: declined invite from {inviter}");
                gs.log_msg("group", &format!("Declined group invite from {inviter}"));
            }
        }

        // POST /v1/group/leave: send OP_GroupDisband(self, self). If leader with < 3 members this
        // fully disbands the group server-side (no auto handoff — see Global Constraints).
        if self.group_leave.lock().unwrap().take().is_some() {
            stream.send_app_packet(OP_GROUP_DISBAND, &build_group_disband(&gs.player_name, &gs.player_name));
            tracing::info!("EQ: group: left group");
            gs.log_msg("group", "Left group");
        }

        // POST /v1/group/kick {"name":"X"}: send OP_GroupDisband(self, target). HTTP layer already
        // validated leadership + membership before queuing this.
        if let Some(target) = self.group_kick.lock().unwrap().take() {
            stream.send_app_packet(OP_GROUP_DISBAND, &build_group_disband(&gs.player_name, &target));
            tracing::info!("EQ: group: kicked {target}");
            gs.log_msg("group", &format!("Kicked {target} from group"));
        }

        // POST /v1/group/makeleader {"name":"X"}: send OP_GroupMakeLeader.
        if let Some(target) = self.group_make_leader.lock().unwrap().take() {
            stream.send_app_packet(OP_GROUP_MAKE_LEADER, &build_group_make_leader(&gs.group_leader, &target));
            tracing::info!("EQ: group: transferring leadership to {target}");
            gs.log_msg("group", &format!("Transferred group leadership to {target}"));
        }

        // POST /v1/trainer/open {"trainer":"X"}: send OP_GMTraining for the resolved NPC spawn id.
        // The server replies OP_GMTraining with the offered caps → apply_gm_training sets gs.trainer_*.
        // Sentinel: Some(0) ENDS the open session (OP_GMEndTraining) — 0 is never a real spawn id;
        // reusing the slot avoids threading one more field through the positional chains (#162).
        if let Some(npc_id) = self.trainer_open_req.lock().unwrap().take() {
            if npc_id == 0 {
                if let Some(open_npc) = gs.trainer_open.take() {
                    let payload = build_gm_end_training(open_npc, gs.player_id);
                    stream.send_app_packet(OP_GM_END_TRAINING, &payload);
                    gs.trainer_skills.clear();
                    // Mirror into the RENDER GameState: ending training is client-initiated (the
                    // server never echoes OP_GMEndTraining), so without this the render-side
                    // trainer_open stays Some and the transient Trainer window never closes.
                    let _ = app_tx.send(AppPacket { opcode: OP_GM_END_TRAINING, payload });
                    tracing::info!("EQ: trainer: ended training with npc {open_npc}");
                }
            } else {
                stream.send_app_packet(OP_GM_TRAINING, &build_gm_training(npc_id, gs.player_id));
                tracing::info!("EQ: trainer: opening training with npc {npc_id}");
            }
        }

        // POST /v1/trainer/train {"skill_id":N}: send OP_GMTrainSkill to the open trainer. The server
        // raises the skill and echoes OP_SkillUpdate → apply_skill_update reflects the new value.
        if let Some(skill_id) = self.trainer_train_req.lock().unwrap().take() {
            if let Some(npc_id) = gs.trainer_open {
                stream.send_app_packet(OP_GM_TRAIN_SKILL, &build_gm_train_skill(npc_id, skill_id));
                tracing::info!("EQ: trainer: training skill {skill_id} at npc {npc_id}");
                gs.log_msg("trainer", &format!("Training {}", crate::skills::skill_name(skill_id).unwrap_or("?")));
            } else {
                gs.log_msg("trainer", "Cannot train — no trainer window open");
            }
        }

        // Check zone-cross request — walk onto the target zone line so the auto-cross below fires.
        //
        // A zone line's real trigger is a `DRNTP` region baked into the zone geometry (native
        // mechanism), NOT the coords in OP_SendZonepoints — those are the DESTINATION of each line,
        // so walking to them lands the player nowhere near the trigger and the server safe-coords /
        // cheat-flags the crossing (the root cause of #174). Resolve the target zone to its
        // zone-point index (iterator), locate that DRNTP region in the zone BSP, and walk there.
        let cross_req = self.zone_cross.lock().unwrap().take();
        if let Some(want_zone) = cross_req {
            // want_zone != 0 → resolve it to a zone-point index; want_zone == 0 → any nearest line.
            let want_index = if want_zone != 0 {
                match self.zone_points.lock().unwrap().iter()
                    .find(|zp| zp.zone_id == want_zone).map(|zp| zp.iterator as i32)
                {
                    Some(idx) => Some(idx),
                    None => {
                        tracing::info!("zone_cross: no zone point advertised for zone_id={want_zone}");
                        gs.log_msg("zone", "No zone line found to cross");
                        // Make the failure observable instead of a silent no-op (#267): a caller that
                        // got 200 from POST /zone_cross can poll nav_state and see it didn't happen.
                        self.set_nav_state("no_path");
                        None
                    }
                }
            } else {
                None // any zone line
            };
            // Only proceed if we actually have a target (want_zone==0 always may; want_zone!=0 needs a match).
            if want_zone == 0 || want_index.is_some() {
                // Locate the NEAREST reachable zone-line region for the wanted zone (not the first
                // zone-point index that matches — a zone with several lines to the same target, or an
                // in-zone translocator with multiple advertised points, would otherwise pick one with
                // no nearby region and no-op, #266). want_index==None → any nearest line.
                let located = self.collision.read().unwrap().as_ref().and_then(|c| {
                    let pos = [gs.player_x, gs.player_y, gs.player_z];
                    match (want_zone, want_index) {
                        (0, _) => c.find_zone_line_near(None, pos),
                        (_, _) => {
                            // Every zone-point index advertised for `want_zone`, nearest region wins.
                            let idxs: Vec<i32> = self.zone_points.lock().unwrap().iter()
                                .filter(|zp| zp.zone_id == want_zone).map(|zp| zp.iterator as i32).collect();
                            idxs.iter()
                                .filter_map(|&idx| c.find_zone_line_near(Some(idx), pos))
                                .min_by(|a, b| {
                                    let da = (a.1[0]-pos[0]).hypot(a.1[1]-pos[1]);
                                    let db = (b.1[0]-pos[0]).hypot(b.1[1]-pos[1]);
                                    da.total_cmp(&db)
                                })
                        }
                    }
                });
                match located {
                    Some((index, [tx, ty, tz])) => {
                        // Destination zone for logging (resolve the located region's index).
                        let dest_zone = self.zone_points.lock().unwrap().iter()
                            .find(|zp| zp.iterator as i32 == index).map(|zp| zp.zone_id).unwrap_or(want_zone);
                        let d2 = (tx - gs.player_x).powi(2) + (ty - gs.player_y).powi(2);
                        const ZONE_LINE_DIST2: f32 = 15.0 * 15.0;
                        if d2 <= ZONE_LINE_DIST2 {
                            // Already standing on the line — the auto-cross below fires this tick.
                            tracing::info!("zone_cross: already on the zone_id={dest_zone} line (index={index})");
                        } else {
                            tracing::info!("zone_cross: walking {:.0}u to the zone_id={dest_zone} line at ({tx:.0},{ty:.0}) (index={index})", d2.sqrt());
                            gs.log_msg("zone", &format!("Walking to the zone {} line", dest_zone));
                            *self.goto_target.lock().unwrap() = Some((tx, ty, tz));
                            *self.goto_entity.lock().unwrap() = None;
                        }
                    }
                    None => {
                        tracing::info!("zone_cross: no zone-line region found for zone_id={want_zone}");
                        gs.log_msg("zone", "No zone line found to cross");
                        // Advertised in OP_SendZonepoints but no DRNTP region in the loaded map (a .wtr
                        // gap): report it so the caller isn't left thinking the 200 meant success (#267).
                        self.set_nav_state("no_path");
                    }
                }
            }
        }

        // Auto zone-cross (native mechanism): when the player stands in a DRNTP zone-line region
        // baked into the zone BSP, resolve its zone-point index to a destination via the
        // OP_SendZonepoints list and send OP_ZONE_CHANGE. The server then matches our real position
        // against the DB trigger and places us at the correct arrival point. Cooldown prevents
        // re-firing while still inside the region right after a crossing.
        {
            const ZONE_CROSS_COOLDOWN_MS: u128 = 10000; // 10 seconds
            // A dead corpse standing in a zone-line region must NOT auto-zone (#238) — this fires purely
            // from physical position, so a character killed right at a boundary would cross while dead.
            if !Self::is_player_dead(gs) && self.last_zone_cross.elapsed().as_millis() > ZONE_CROSS_COOLDOWN_MS {
                let index = self.collision.read().unwrap().as_ref()
                    .and_then(|c| c.zone_line_at([gs.player_x, gs.player_y, gs.player_z]));
                if let Some(index) = index {
                    // Resolve destination: the advertised zone point whose iterator matches this
                    // region's index. A region with no matching zone point (e.g. a WLD index the DB
                    // doesn't advertise) is left alone rather than crossing blindly.
                    let dest = self.zone_points.lock().unwrap().iter()
                        .find(|zp| zp.iterator as i32 == index && zp.zone_id != 0)
                        .map(|zp| zp.zone_id);
                    match dest {
                        Some(dest_zone) => {
                            tracing::info!("zone_cross: in zone-line region index={index} → zone_id={dest_zone}");
                            gs.log_msg("zone", &format!("Crossing to zone {}", dest_zone));
                            self.send_zone_change_packet(stream, gs, dest_zone);
                            self.last_zone_cross = Instant::now();
                        }
                        None => {
                            tracing::debug!("zone_cross: zone-line region index={index} has no matching zone point — ignoring");
                        }
                    }
                }
            }
        }

        // NOTE: server-initiated zone changes (GM #zone, portal doors, spell ports/gate/evac) are
        // answered by the gameplay.rs OP_REQUEST_CLIENT_ZONE_CHANGE handler, which echoes the
        // server's real zone_id via build_zone_change. This block USED to re-send via
        // send_zone_change_packet, but #199 changed that to always emit zoneID=0 (the resolve-from-
        // position sentinel, correct only for client-initiated WALK-IN crossings). That misrouted
        // every server-initiated teleport to a wrong zone (#235) — so it's removed; the wire
        // zoneID=0 path is now confined to /v1/move/zone_cross.

        // Check hail request — say "Hail, <name>" so the NPC fires its hail script. The server only
        // runs an NPC's EVENT_SAY on the player's CURRENT TARGET (client.cpp: `Mob* t = GetTarget()`),
        // so we must target the NPC FIRST, in the same tick and before the say packet, or the hail is
        // silently ignored (#130). The target packet precedes the say on the ordered stream, so the
        // server has GetTarget()==the NPC when it processes the say.
        // Local chat echo (#3 of the UI-overhaul bug cluster): the chat window renders only the
        // RENDER GameState's message log, and outgoing chat is client-initiated (the server doesn't
        // echo your own say/tell/... back), so every send below also mirrors a "You say/tell …"
        // line over app_tx via the internal-only OP_UI_LOCAL_ECHO packet.
        let echo = |kind: &str, text: &str| {
            let _ = app_tx.send(AppPacket {
                opcode: OP_UI_LOCAL_ECHO,
                payload: build_ui_local_echo(kind, text),
            });
        };

        let hail_req = self.hail.lock().unwrap().take();
        if let Some((name, spawn_id)) = hail_req {
            // A hail starts a FRESH interaction — drop any saylink choices left over from a prior
            // NPC (or a system/command message). Otherwise `/observe/dialogue` leaks the last
            // choices indefinitely, since they're only ever overwritten when a new say-line carries
            // saylinks and never cleared (#274). The hailed NPC's own reply repopulates them.
            gs.dialogue_choices.clear();
            if let Some(id) = spawn_id {
                gs.target_id = Some(id);
                if let Some(e) = gs.entities.get(&id) { gs.target_name = Some(e.name.clone()); }
                stream.send_app_packet(OP_TARGET_MOUSE, &build_target_packet(id));
                // Mirror the client-initiated target into the render GameState (HUD/HTTP). (#9)
                let _ = app_tx.send(AppPacket { opcode: OP_TARGET_MOUSE, payload: build_target_packet(id) });
            }
            let msg = format!("Hail, {}", name);
            let pkt = build_say_packet(&gs.player_name, &name, &msg);
            tracing::info!("EQ: hailing '{}' (target={:?}, say): {}", name, spawn_id, msg);
            stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
            let line = format!("You say, '{}'", msg);
            gs.log_msg("chat", &line);
            echo("chat", &line);
        }

        // Check say request — arbitrary Say text (HUD say box / quest keyword follow-up).
        let say_text = self.say.lock().unwrap().take();
        if let Some(text) = say_text {
            // The `/camp` chat keyword is a local command, not Say text: toggle a camp instead of
            // broadcasting it. The gameplay loop drains the camp slot and runs the camp/cancel.
            if text.trim().eq_ignore_ascii_case("/camp") {
                *self.camp.lock().unwrap() = Some(CampCmd::Toggle);
                tracing::info!("EQ: /camp chat command — toggling camp");
            } else {
                let pkt = build_say_packet(&gs.player_name, "", &text);
                tracing::info!("EQ: say: {}", text);
                stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
                let line = format!("You say, '{}'", text);
                gs.log_msg("chat", &line);
                echo("chat", &line);
            }
        }

        // Check dialogue-click request (POST /v1/interact/dialogue, or a GUI click): "click" a
        // parsed saylink by sending OP_ItemLinkClick with its ids. The server resolves the phrase
        // from its saylink table and processes it as if we said it to the NPC (#120).
        let click = self.dialogue_click.lock().unwrap().take();
        if let Some(c) = click {
            let pkt = build_item_link_click(c.item_id, &c.augments, c.link_hash, c.icon);
            tracing::info!("EQ: dialogue click: '{}' (sayid={})", c.text, c.augments[0]);
            stream.send_app_packet(OP_ITEM_LINK_CLICK, &pkt);
            let line = format!("You say, '{}'", c.text);
            gs.log_msg("chat", &line);
            echo("chat", &line);
        }

        // Drain queued outgoing chat (POST /tell|/ooc|/shout|/group): build + send OP_ChannelMessage.
        let outgoing: Vec<crate::http::ChatSend> = {
            let mut q = self.chat_send.lock().unwrap();
            std::mem::take(&mut *q)
        };
        for c in outgoing {
            let pkt = build_channel_message(&gs.player_name, &c.to, c.chan, &c.text);
            stream.send_app_packet(OP_CHANNEL_MESSAGE, &pkt);
            let label = match c.chan { 7 => format!("tell {}", c.to), 5 => "ooc".into(),
                                       3 => "shout".into(), 2 => "group".into(), 0 => "guild".into(),
                                       n => format!("chan{n}") };
            tracing::info!("EQ: {} -> {}", label, c.text);
            // Native-style local echo, logged under the channel's kind so the chat window
            // tab-filters and colors it like the matching incoming traffic.
            let (kind, line): (&str, String) = match c.chan {
                7 => ("tell",  format!("You told {}, '{}'", c.to, c.text)),
                5 => ("ooc",   format!("You say out of character, '{}'", c.text)),
                3 => ("shout", format!("You shout, '{}'", c.text)),
                2 => ("group", format!("You tell your party, '{}'", c.text)),
                0 => ("guild", format!("You say to your guild, '{}'", c.text)),
                _ => ("chat",  format!("You {}: {}", label, c.text)),
            };
            gs.log_msg(kind, &line);
            echo(kind, &line);
        }

        // Check target request — set target + auto-consider it (con color comes back as
        // an OP_CONSIDER reply, handled in packet_handler).
        let target_id = self.target.lock().unwrap().take();
        if let Some(id) = target_id {
            gs.target_id = Some(id);
            if let Some(e) = gs.entities.get(&id) {
                gs.target_name = Some(e.name.clone());
            }
            stream.send_app_packet(OP_TARGET_MOUSE, &build_target_packet(id));
            stream.send_app_packet(OP_CONSIDER, &build_consider_packet(gs.player_id, id));
            // Mirror the target into the RENDER GameState (HUD/HTTP) via a synthetic app packet —
            // this is a client-initiated change, so it won't otherwise reach the render side. (#9)
            let _ = app_tx.send(AppPacket { opcode: OP_TARGET_MOUSE, payload: build_target_packet(id) });
            tracing::info!("EQ: target spawn_id={} + consider", id);
        }

        // Check /who all request (#300) — send OP_WhoAllRequest (server-wide, type=3); the oneshot
        // sender is held in `pending_who` until OP_WhoAllResponse arrives (see `fulfill_who`). A newer
        // request supersedes an in-flight one (its sender drops → that GET times out).
        if let Some(tx) = self.who_req.lock().unwrap().take() {
            stream.send_app_packet(OP_WHO_ALL_REQUEST, &build_who_all_request(3));
            self.pending_who = Some(tx);
            tracing::info!("EQ: sent OP_WhoAllRequest (/who all)");
        }

        // Check attack request — send OP_AUTO_ATTACK(1) to start, OP_AUTO_ATTACK(0) to stop.
        // Server expects exactly 4 bytes; byte[0]=1 enables, byte[0]=0 disables.
        let attack_req = self.attack.lock().unwrap().take();
        if let Some(on) = attack_req {
            self.auto_attack = on;
            let payload = [if on { 1u8 } else { 0u8 }, 0, 0, 0];
            stream.send_app_packet(OP_AUTO_ATTACK, &payload);
            gs.auto_attack = on;
            // Mirror the toggle into the RENDER GameState (apply_auto_attack): the toggle is
            // client-initiated, so without this scene.auto_attack stays false forever and the
            // Actions/Target windows' Attack button can never show ON (or turn it off). (#2)
            let _ = app_tx.send(AppPacket { opcode: OP_AUTO_ATTACK, payload: payload.to_vec() });
            tracing::info!("EQ: auto-attack {}", if on { "ON" } else { "OFF" });
        }

        // POST /v1/pet/command or a Pet-window button: send one OP_PetCommands for the player's
        // pet. PET_ATTACK aims at the current target (like the auto-pet path); every other command
        // (back off / follow / guard / sit) targets 0 — the server acts on the pet itself.
        let pet_cmd = self.pet_cmd.lock().unwrap().take();
        if let Some(cmd) = pet_cmd {
            let cmd = cmd as u32;
            if gs.pet_id.is_none() {
                gs.log_msg("pet", "You have no pet");
            } else if cmd == PET_ATTACK {
                match gs.target_id.filter(|&t| t != 0) {
                    Some(tid) => {
                        stream.send_app_packet(OP_PET_COMMANDS, &build_pet_command(PET_ATTACK, tid));
                        // Keep the auto-pet-combat dedupe in sync so it doesn't immediately
                        // re-issue (or back-off-cancel) the manual order.
                        self.last_pet_target = Some(tid);
                        tracing::info!("EQ: pet command attack → target {tid}");
                        gs.log_msg("pet", "Pet attack ordered");
                    }
                    None => gs.log_msg("pet", "Pet attack: no target"),
                }
            } else {
                stream.send_app_packet(OP_PET_COMMANDS, &build_pet_command(cmd, 0));
                if cmd == PET_BACKOFF { self.last_pet_target = None; }
                tracing::info!("EQ: pet command {cmd}");
                gs.log_msg("pet", &format!("Pet command sent ({})", match cmd {
                    PET_BACKOFF => "back off", PET_FOLLOWME => "follow",
                    PET_GUARDHERE => "guard here", PET_SIT => "sit", _ => "other",
                }));
            }
        }

        // POST /v1/interact/read {"slot":N}: read a book/note. Look up the item at that wire slot;
        // if it carries a Filename it's readable, so send OP_ReadBook with that filename and the
        // server replies with the text (apply_read_book stores it → GET /v1/observe/item_text). (#288)
        let read_slot = self.read_book.lock().unwrap().take();
        if let Some(slot) = read_slot {
            match gs.inventory.iter().find(|i| i.slot == slot) {
                Some(item) if !item.filename.is_empty() => {
                    let pkt = build_read_book_packet(slot as i16, gs.player_id, &item.filename);
                    stream.send_app_packet(OP_READ_BOOK, &pkt);
                    tracing::info!("EQ: read book slot={} file='{}'", slot, item.filename);
                }
                Some(_) => gs.log_msg("book", &format!("Item in slot {slot} is not readable")),
                None    => gs.log_msg("book", &format!("No item in slot {slot} to read")),
            }
        }

        // POST /v1/guild/{invite,accept,leave,remove}: one queued guild action → the matching RoF2
        // guild opcode. Invite/remove/leave share GuildCommand_Struct; accept replies to a captured
        // pending invite with GuildInviteAccept_Struct. (#295)
        let guild_action = self.guild_action.lock().unwrap().take();
        if let Some(action) = guild_action {
            const GUILD_RECRUIT: u32 = 8; // default rank for a fresh invite (RoF2 0-8 scale)
            match action {
                crate::http::GuildAction::Invite(name) => {
                    let pkt = build_guild_command(&name, &gs.player_name, gs.player_guild_id, GUILD_RECRUIT);
                    stream.send_app_packet(OP_GUILD_INVITE, &pkt);
                    gs.log_msg("guild", &format!("Inviting {name} to the guild"));
                    tracing::info!("EQ: guild invite -> {name}");
                }
                crate::http::GuildAction::Remove(name) => {
                    let pkt = build_guild_command(&name, &gs.player_name, gs.player_guild_id, 0);
                    stream.send_app_packet(OP_GUILD_REMOVE, &pkt);
                    gs.log_msg("guild", &format!("Removing {name} from the guild"));
                    tracing::info!("EQ: guild remove -> {name}");
                }
                crate::http::GuildAction::Leave => {
                    // Self-leave: othername == myname.
                    let pkt = build_guild_command(&gs.player_name, &gs.player_name, gs.player_guild_id, 0);
                    stream.send_app_packet(OP_GUILD_REMOVE, &pkt);
                    gs.log_msg("guild", "Leaving guild");
                    tracing::info!("EQ: guild leave");
                }
                crate::http::GuildAction::Accept => match gs.pending_guild_invite.take() {
                    Some((inviter, guild_id, rank)) => {
                        let pkt = build_guild_invite_accept(&inviter, &gs.player_name, rank, guild_id);
                        stream.send_app_packet(OP_GUILD_INVITE_ACCEPT, &pkt);
                        gs.log_msg("guild", &format!("Accepting guild invite from {inviter}"));
                        tracing::info!("EQ: guild accept from {inviter} (guild_id={guild_id})");
                    }
                    None => gs.log_msg("guild", "No pending guild invite to accept"),
                },
            }
        }

        // Cast a memorized spell gem. Target priority: an explicit API target > the current target
        // > self. `Some(0)` is not a real spawn (the "clear target" sentinel), so collapse it to
        // "none" here or the self-fallback never fires. For BENEFICIAL spells (heals/buffs) that
        // aren't aimed at a friendly target, cast on the caster instead of a hostile/stale mob —
        // matching the real RoF2 client, which self-targets heals/buffs. (eqoxide#95)
        let cast_req = self.cast.lock().unwrap().take();
        if let Some(req) = cast_req {
          if let Some(item_slot) = req.item_slot {
            // Item "clicky" cast (teleport ring / port potion, etc.). Resolve the click spell from
            // the item currently at that wire slot and refuse if it isn't a clicky, so a stale slot
            // can't fire an unrelated cast. Target: explicit > current > self. (eqoxide#193)
            let click = gs.inventory.iter().find(|i| i.slot == item_slot as i32)
                .map(|i| i.click_spell_id).unwrap_or(0);
            if click == 0 {
                tracing::info!("EQ: item cast slot={} ignored — no clicky item at that slot", item_slot);
            } else {
                let target = req.target_id.filter(|&t| t != 0)
                    .or(gs.target_id.filter(|&t| t != 0))
                    .unwrap_or(gs.player_id);
                stream.send_app_packet(OP_CAST_SPELL, &build_item_cast_packet(item_slot, click, target));
                tracing::info!("EQ: item cast slot={} spell={} target={}", item_slot, click, target);
            }
          } else {
            let spell_id = gs.mem_spells.get(req.gem as usize).copied().unwrap_or(0xFFFF_FFFF);
            if spell_id != 0xFFFF_FFFF {
                let explicit = req.target_id.filter(|&t| t != 0);
                let current  = gs.target_id.filter(|&t| t != 0);
                let mut target = explicit.or(current).unwrap_or(gs.player_id);
                if let Some(db) = crate::spells::global() {
                    if db.is_self_only(spell_id) {
                        target = gs.player_id; // ST_SELF: always the caster
                    } else if explicit.is_none() && db.is_beneficial(spell_id) {
                        // Keep an explicitly-chosen friendly (PC) target for group heals; otherwise
                        // (no target, cleared, or a hostile NPC) land the buff/heal on ourselves.
                        let friendly = target == gs.player_id
                            || gs.entities.get(&target).map_or(false, |e| !e.is_npc);
                        if !friendly { target = gs.player_id; }
                    }
                }
                stream.send_app_packet(OP_CAST_SPELL, &build_cast_packet(req.gem as u32, spell_id, target));
                tracing::info!("EQ: cast gem={} spell={} target={}", req.gem, spell_id, target);
            } else {
                tracing::info!("EQ: cast gem={} ignored — empty gem", req.gem);
            }
          }
        }

        // Scribe a scroll into the spellbook (scribing=0) or memorize a known spell into a gem
        // (scribing=1) — OP_MemorizeSpell. The server validates (you hold the scroll / know the
        // spell) and pushes OP_MemorizeSpell back, which updates gs.mem_spells for the gem case.
        let mem_req = self.mem_spell.lock().unwrap().take();
        if let Some((slot, spell_id, scribing, from)) = mem_req {
            // Scribing (0) only takes effect on the scroll sitting on the CURSOR: the RoF2 server
            // reads m_inv[slotCursor] and ignores the packet otherwise (silent fail, eqoxide#11).
            // So move the scroll from its inventory slot → cursor first (same tick; the server
            // processes packets in order, so the cursor holds the scroll when the scribe arrives).
            if scribing == 0 {
                if let Some(from_slot) = from {
                    if from_slot != SLOT_CURSOR {
                        stream.send_app_packet(OP_MOVE_ITEM, &build_move_item(from_slot, SLOT_CURSOR));
                        gs.move_item(from_slot as i32, SLOT_CURSOR as i32); // mirror locally
                        tracing::info!("EQ: scribe — moved scroll slot {} → cursor", from_slot);
                    }
                }
            }
            stream.send_app_packet(OP_MEMORIZE_SPELL, &build_memorize_packet(slot, spell_id, scribing));
            if scribing == 0 {
                // The RoF2 server CONSUMES the scribed scroll: OPMemorizeSpell's memSpellScribing
                // case runs ScribeSpell(...) then DeleteItemInInventory(slotCursor) (zone/
                // client_process.cpp). We already moved the scroll to the cursor above, so mirror
                // that deletion locally — otherwise the (now server-deleted) scroll stays stuck on
                // cursor slot 33 in our view, blocking looting and any later cursor move (#271). No
                // OP_DeleteItem is sent: the server already removed it, so that would double-delete.
                gs.inventory.retain(|i| i.slot != SLOT_CURSOR as i32);
            }
            let what = match scribing { 0 => "scribe", 1 => "memorize", _ => "unmem" };
            tracing::info!("EQ: {what} spell={spell_id} slot={slot}");
            gs.log_msg("spell", &format!("{what} spell {spell_id} (slot {slot})"));
        }

        // Sit / stand (OP_SpawnAppearance type=14, param 110/100).
        let sit_req = self.sit.lock().unwrap().take();
        if let Some(sit) = sit_req {
            let param = if sit { 110u32 } else { 100u32 };
            let payload = build_spawn_appearance_packet(gs.player_id as u16, 14, param);
            stream.send_app_packet(OP_SPAWN_APPEARANCE, &payload);
            gs.sitting = sit;
            // Bridge to the RENDER GameState so the player's OWN sit animation plays. A client-
            // initiated sit sets only the nav-thread `gs.sitting`; the render loop reads its separate
            // GameState, updated solely from `app_tx`. Mirror the appearance through a synthetic
            // packet (apply_spawn_appearance), same pattern as the target/money bridges. (#53)
            let _ = app_tx.send(AppPacket { opcode: OP_SPAWN_APPEARANCE, payload });
            tracing::info!("EQ: {}", if sit { "sit" } else { "stand" });
        }

        // Standalone consider.
        let con_req = self.consider.lock().unwrap().take();
        if let Some(id) = con_req {
            stream.send_app_packet(OP_CONSIDER, &build_consider_packet(gs.player_id, id));
            tracing::info!("EQ: consider spawn_id={}", id);
        }

        // Merchant buy: open the merchant (OP_ShopRequest) then buy its inventory slot
        // (OP_ShopPlayerBuy). Sent in sequence — the server processes the open first so the
        // merchant is open by the time the buy arrives. Must be within ~200u of the merchant.
        let buy_req = self.buy.lock().unwrap().take();
        if let Some((merchant_id, slot)) = buy_req {
            let open = merchant_click(merchant_id, gs.player_id, 1);
            stream.send_app_packet(OP_SHOP_REQUEST, &open);
            // RoF2 Merchant_Sell_Struct (32b): npcid@0, playerid@4, itemslot@8, unknown12@12,
            // quantity@16, unknown20@20, price@24, unknown28@28. (Titanium was 24b with price@20;
            // the RoF2 server DECODEs an exact 32 bytes, so a short packet was silently dropped.)
            let mut buy = [0u8; 32];
            buy[0..4].copy_from_slice(&merchant_id.to_le_bytes());
            buy[4..8].copy_from_slice(&gs.player_id.to_le_bytes());
            buy[8..12].copy_from_slice(&slot.to_le_bytes());
            buy[16..20].copy_from_slice(&1u32.to_le_bytes()); // quantity = 1 (server sets the price)
            stream.send_app_packet(OP_SHOP_PLAYER_BUY, &buy);
            // Deduct the cost from on-hand coin for the HUD: the server takes the money with
            // update_client=false (Handle_OP_ShopPlayerBuy → TakeMoneyFromPP) and sends no
            // OP_MoneyUpdate, so the displayed coin would otherwise stay stale after a purchase.
            // spend_coin here only updates *this* (network-thread) GameState; the HUD / HTTP coin
            // is published from the render thread's separate GameState, which is fed solely by
            // packets through app_tx. So after deducting, synthesize an OP_MoneyUpdate carrying the
            // new total and route it through app_tx — apply_money_update applies it on the render
            // copy, keeping the HUD in sync (mirrors how real money packets reach both copies).
            let price = gs.merchant_items.iter().find(|m| m.merchant_slot == slot).map(|m| m.price);
            if let Some(p) = price {
                if gs.spend_coin(p as u64) {
                    let mut money = Vec::with_capacity(16);
                    for v in gs.coin { money.extend_from_slice(&(v as i32).to_le_bytes()); }
                    let _ = app_tx.send(AppPacket { opcode: OP_MONEY_UPDATE, payload: money });
                }
            }
            tracing::info!("EQ: shop buy — merchant_id={} slot={} qty=1 cost={}", merchant_id, slot, price.unwrap_or(0));
            gs.log_msg("merchant", &format!("Bought item (slot {})", slot));
        }

        // Merchant sell: open the merchant (OP_ShopRequest) then sell a player inventory slot
        // (OP_ShopPlayerSell). Same sequencing as buy so the shop is open server-side first.
        // Must be within ~200u of the merchant; the server computes the price (we send 0).
        let sell_req = self.sell.lock().unwrap().take();
        if let Some((merchant_id, slot, quantity)) = sell_req {
            let open = merchant_click(merchant_id, gs.player_id, 1);
            stream.send_app_packet(OP_SHOP_REQUEST, &open);
            // RoF2 Merchant_Purchase_Struct is 20 bytes (rof2_structs.h): npcid(u32)@0,
            // inventory_slot(TypelessInventorySlot_Struct: Slot i16@4, SubIndex i16@6, AugIndex i16@8,
            // Unknown i16@10)@4, quantity(u32)@12, price(u32)@16. The old 16-byte body (plain u32
            // slot@4) failed the server's DECODE_LENGTH_EXACT, so EVERY sell was silently dropped
            // (#269). `slot` is the RoF2 wire slot /observe/inventory reports (general inv 23-32);
            // RoF2ToServerTypelessSlot passes it straight through for a top-level possession, so
            // SubIndex/AugIndex are the "none" sentinels (SLOT_INVALID / SOCKET_INVALID = -1).
            let mut sell = [0u8; 20];
            sell[0..4].copy_from_slice(&merchant_id.to_le_bytes());
            sell[4..6].copy_from_slice(&(slot as i16).to_le_bytes());   // Slot (RoF2 wire slot)
            sell[6..8].copy_from_slice(&(-1i16).to_le_bytes());          // SubIndex: not inside a bag
            sell[8..10].copy_from_slice(&(-1i16).to_le_bytes());         // AugIndex: no augment socket
            // Unknown01 @10 stays 0.
            sell[12..16].copy_from_slice(&quantity.to_le_bytes());
            // price @16 = 0: the server charges its own buy-back price.
            stream.send_app_packet(OP_SHOP_PLAYER_SELL, &sell);
            tracing::info!("EQ: shop sell — merchant_id={} slot={} qty={}", merchant_id, slot, quantity);
            // No optimistic "Sold" log: the server's OP_ShopPlayerSell echo (apply_shop_player_sell)
            // confirms the real payout + removes the item, so a premature success can't be printed
            // when the sale fails (#269).
        }

        // Open/close a merchant window (POST /trade/open, /trade/close). OP_ShopRequest with
        // command=1 (open) or 0 (close). The server replies with OP_ShopRequest (Open/Close) +
        // OP_ItemPacket(Merchant) items, decoded in packet_handler into gs.merchant_*.
        let trade_req = self.trade.lock().unwrap().take();
        if let Some(cmd) = trade_req {
            let (merchant_id, command) = match cmd {
                TradeCmd::Open(id) => (id, 1u32),
                TradeCmd::Close    => (gs.merchant_open.unwrap_or(0), 0u32),
            };
            let open = merchant_click(merchant_id, gs.player_id, command);
            stream.send_app_packet(OP_SHOP_REQUEST, &open);
            tracing::info!("EQ: shop {} — merchant_id={}", if command == 1 { "open" } else { "close" }, merchant_id);
            if command == 0 { gs.merchant_open = None; gs.merchant_items.clear(); }
        }

        // Move/equip/unequip an item between inventory slots (OP_MoveItem).
        // MoveItem_Struct (12b): from_slot(u32), to_slot(u32), number_in_stack(u32).
        // number_in_stack MUST be 0 for a whole-item move (equip/unequip/rearrange): EQEmu's
        // SwapItem rejects number_in_stack > 0 for any non-stackable item (inventory.cpp ~2025,
        // "not a stackable item" -> SwapItemResync = the "Inventory Desyncronization" we hit). 0
        // takes the direct-swap/equip path. (A count would only be for splitting a stack.)
        let move_req = self.move_req.lock().unwrap().take();
        if let Some((from_slot, to_slot)) = move_req {
            // build_move_item emits the structured 28-byte RoF2 MoveItem_Struct; a flat 12-byte
            // packet is silently dropped by the server (see build_move_item / eqoxide#11).
            stream.send_app_packet(OP_MOVE_ITEM, &build_move_item(from_slot, to_slot));
            // EQEmu applies the move silently (no echo), so mirror it into our snapshot or
            // /inventory goes stale and the next move corrupts it (phantom items).
            gs.move_item(from_slot as i32, to_slot as i32);
            // Mirror the same move into the RENDER GameState via a synthetic app packet, or the
            // render side keeps stale held-item models (scene.*_weapon_idfile) until the next
            // OP_CharInventory (relog/zone). 8-byte payload: from(i32 LE) + to(i32 LE). (#141)
            let mut mv = [0u8; 8];
            mv[0..4].copy_from_slice(&(from_slot as i32).to_le_bytes());
            mv[4..8].copy_from_slice(&(to_slot as i32).to_le_bytes());
            let _ = app_tx.send(AppPacket { opcode: OP_MOVE_ITEM, payload: mv.to_vec() });
            tracing::info!("EQ: move item — from_slot={} to_slot={} qty=0(whole)", from_slot, to_slot);
            gs.log_msg("inventory", &format!("Moved item (slot {} -> {})", from_slot, to_slot));
        }

        // Stream the controller's authoritative position to the server every tick at native cadence
        // (independent of the 150 ms planner gate below). This is the single position authority.
        self.stream_position(stream, gs);

        // Dead men don't walk (#238, eqoxide#61): the instant the player is slain, abandon any /goto
        // or /zone_cross and stop driving the controller, so a corpse doesn't keep walking its route
        // toward the goal. Placed BEFORE the fast-steering refresh AND the 150 ms walk gate so
        // movement halts within a tick, not up to a gate-period later. Position streaming above still
        // runs, keeping the stationary corpse in sync with the server.
        if self.nav_halt_if_dead(gs) {
            return;
        }

        // FAST STEERING (#nav-multires). The plans (`path`, `local_path`) are refreshed on the 150ms
        // gate below, but the controller runs at ~100Hz — driving a 150ms-stale heading overshoots
        // every turn by up to RUN_SPEED·0.15 ≈ 6.6u and clips walls (the "not following the line"
        // bug). So each loop (~10ms), re-project the CURRENT position onto the stable fine path and
        // refresh ONLY nav_intent's `wish_dir` (+ facing) — the flags/speed the walker set stay. The
        // carrot slides along the line as we move, so the avatar hugs it through tight turns.
        if !self.local_path.is_empty() && self.goto_target.lock().unwrap().is_some() {
            if let Some(aim) = carrot_along(&self.local_path, 0, [gs.player_x, gs.player_y], 5.0) {
                let (dx, dy) = (aim[0] - gs.player_x, aim[1] - gs.player_y);
                let d = (dx * dx + dy * dy).sqrt();
                if d > 1e-3 {
                    if let Some(intent) = self.nav_intent.lock().unwrap().as_mut() {
                        intent.wish_dir = [dx / d, dy / d];
                    }
                    gs.player_heading = eq_heading(dx, dy);
                }
            }
        }

        if self.last_tick.elapsed().as_millis() < NAV_TICK_MS {
            return;
        }
        self.last_tick = Instant::now();

        // Quest turn-in (POST /give) trade-window state machine. Spans multiple ticks: we must
        // wait for the server's OP_TradeRequestAck (sets gs.trade_ack_ready) between sending the
        // trade request and moving the item into the NPC trade slot. Run on the throttled ~150ms
        // cadence so the per-tick ack timeout count matches the documented ~3s window.
        self.tick_give(stream, gs);

        // Auto-target: while auto-attacking, pick who to fight each tick. Priority (see
        // `pick_combat_target`): a mob that is actively attacking the player (engage adds instead of
        // tanking them unanswered) > a still-valid current target > the nearest reachable trash mob
        // (name starts "a_"/"an_", excluding named guards/merchants/citizens) within ~200u, so
        // grinding continues hands-free between kills.
        if self.auto_attack {
            // Drop attackers that haven't swung at us in a while so a long-dead aggressor or one
            // we've out-run doesn't keep pulling target priority.
            const ATTACKER_TTL: std::time::Duration = std::time::Duration::from_secs(6);
            gs.recent_attackers.retain(|_, t| t.elapsed() < ATTACKER_TTL);

            let col = self.collision.read().unwrap();
            let clear_to = |e: &crate::game_state::Entity| -> bool {
                col.as_ref().map_or(true, |c| {
                    c.path_clear([gs.player_x, gs.player_y, e.z + 3.0], [e.x, e.y, e.z + 3.0], 2.0)
                })
            };
            let alive_reachable = |id: u32| -> bool {
                gs.entities.get(&id).map(|e| !e.dead && e.is_npc && clear_to(e)).unwrap_or(false)
            };

            let current = gs.target_id;
            // The current target is valid only if alive AND still reachable in a straight line —
            // otherwise drop it so we retarget or roam (don't get stuck swinging "too far").
            let current_valid = current.map(|id| alive_reachable(id)).unwrap_or(false);
            let current_is_attacker = current.map(|id| gs.recent_attackers.contains_key(&id)).unwrap_or(false);

            // The add to engage: the most-recent attacker that is alive + reachable and isn't already
            // our current target. (If the current target is the attacker, `pick_combat_target` keeps it.)
            let attacker = gs.recent_attackers.iter()
                .filter(|(id, _)| Some(**id) != current && alive_reachable(**id))
                .max_by_key(|(_, t)| **t)
                .map(|(id, _)| *id);

            // Nearest reachable trash, only needed as the fallback (no attacker, no valid current).
            let nearest_trash = if attacker.is_none() && !current_valid {
                let mut best: Option<(f32, u32)> = None;
                for (id, e) in &gs.entities {
                    if e.dead || !e.is_npc { continue; }
                    let nl = e.name.to_ascii_lowercase();
                    if !(nl.starts_with("a_") || nl.starts_with("an_")) { continue; }
                    let dx = e.x - gs.player_x;
                    let dy = e.y - gs.player_y;
                    let d2 = dx * dx + dy * dy;
                    if d2 > 200.0 * 200.0 || !clear_to(e) { continue; }
                    if best.map(|(bd, _)| d2 < bd).unwrap_or(true) { best = Some((d2, *id)); }
                }
                best.map(|(_, id)| id)
            } else { None };
            drop(col);

            let desired = pick_combat_target(current, current_valid, current_is_attacker, attacker, nearest_trash);
            // Only send a target packet when the choice actually changes (avoid per-tick spam). If
            // `desired` is None we keep the current target and idle, matching the old behaviour of
            // waiting for a respawn rather than roaming out of a sealed pocket.
            if let Some(id) = desired {
                if Some(id) != current {
                    gs.target_id = Some(id);
                    if let Some(e) = gs.entities.get(&id) { gs.target_name = Some(e.name.clone()); }
                    stream.send_app_packet(OP_TARGET_MOUSE, &build_target_packet(id));
                }
            }
        }

        // Auto-pet-combat: if the player has a pet (e.g. a summoned necro pet), send it to attack
        // the current target. Only (re)issue PET_ATTACK when the target changes, so we don't spam
        // OP_PetCommands every tick. The player's own melee auto-engage (below) still runs, which
        // keeps her walking into loot range while the pet does the damage.
        if let Some(pet) = gs.pet_id {
            // Engage only a reasonably-close LIVE target (<=200u) so the pet doesn't run across the
            // zone after a distant mob and lose itself. When there's no such target (idle, or the
            // mob died), recall the pet with PET_BACKOFF so it returns to the owner instead of
            // wandering off — the previous version left it chasing and it dropped out of view.
            let engage = if self.auto_attack {
                gs.target_id
                    .and_then(|tid| gs.entities.get(&tid).map(|e| (tid, e)))
                    .filter(|(_, e)| {
                        let dx = e.x - gs.player_x; let dy = e.y - gs.player_y;
                        !e.dead && dx * dx + dy * dy <= 200.0 * 200.0
                    })
                    .map(|(tid, _)| tid)
            } else { None };
            match engage {
                Some(tid) if self.last_pet_target != Some(tid) => {
                    stream.send_app_packet(OP_PET_COMMANDS, &build_pet_command(PET_ATTACK, tid));
                    self.last_pet_target = Some(tid);
                    tracing::info!("EQ: pet {pet} → attack target {tid}");
                }
                Some(_) => {} // already attacking this target
                None => {
                    if self.last_pet_target.is_some() {
                        stream.send_app_packet(OP_PET_COMMANDS, &build_pet_command(PET_BACKOFF, 0));
                        self.last_pet_target = None;
                        tracing::info!("EQ: pet {pet} → back off (no target)");
                    }
                }
            }
        } else {
            self.last_pet_target = None;
        }

        // Auto-engage: while auto-attacking, walk into melee range of the target and face it so
        // the server registers swings. Closing the last few units via legit walking (not a held
        // far-away face) is what makes melee actually land. Runs regardless of any pending goto.
        if self.auto_attack {
            if let Some(tid) = gs.target_id {
                if let Some((ex, ey)) = gs.entities.get(&tid).map(|e| (e.x, e.y)) {
                    let dx = ex - gs.player_x;
                    let dy = ey - gs.player_y;
                    let dist = (dx * dx + dy * dy).sqrt();
                    if dist < 200.0 { // engage targets within ~200u (sparse spawns; walk to them)
                        const MELEE: f32 = 5.0;
                        const PET_STANDOFF: f32 = 25.0; // pet classes hang back and let the pet tank
                        // With a pet, DON'T walk into melee — the pet holds aggro (PET_ATTACK) and a
                        // squishy caster who closes to melee just gets killed (a level-1 necro died
                        // to a level-4 skeleton this way). Stand off ~25u: out of the mob's melee but
                        // close enough to loot the corpse after the pet kills it.
                        let engage = if gs.pet_id.is_some() { PET_STANDOFF } else { MELEE };
                        let hdg = if dist > 0.01 { eq_heading(dx, dy) } else { gs.player_heading };
                        gs.player_heading = hdg;
                        if dist > engage {
                            // Drive the controller toward the target (it owns collide-and-slide).
                            let swim = self.collision.read().unwrap().as_ref()
                                .is_some_and(|c| c.in_water([gs.player_x, gs.player_y, gs.player_z]));
                            *self.nav_intent.lock().unwrap() = Some(MoveIntent {
                                wish_dir:    [dx / dist, dy / dist],
                                wish_vspeed: 0.0,
                                jump:        false,
                                want_swim:   swim,
                                speed:       RUN_SPEED,
                                climb:       0.0, // nav uses the native step-up now (#239); fences handled by hop
                                hop:         false,                      // melee approach: no auto-hop
                            });
                        } else {
                            // In melee range: stop the controller and face the target so swings land
                            // (IsFacingMob). The explicit send keeps the server's facing current.
                            *self.nav_intent.lock().unwrap() = None;
                            self.send_position_update(stream, gs, gs.player_x, gs.player_y, gs.player_z, hdg);
                        }
                        *self.goto_target.lock().unwrap() = None; // cancel any stale walk
                        return;
                    }
                }
            }
        }

        // Controlled fall in progress: descend at the native rate until landed, then apply native
        // fall damage (client-computed in EQ; the server only validates OP_EnvDamage). Takes
        // priority over normal walking so the descent isn't interrupted.
        if let Some(land_z) = self.falling {
            const FALL_STEP: f32 = 12.0; // ~native per-update descent (under the 12.8 wire cap)
            let next_z = (gs.player_z - FALL_STEP).max(land_z);
            let hdg = gs.player_heading;
            self.send_position_update(stream, gs, gs.player_x, gs.player_y, next_z, hdg);
            let _ = app_tx.send(make_position_packet(gs.player_id, gs.player_x, gs.player_y, next_z, hdg));
            gs.player_z = next_z;
            if next_z <= land_z + 0.5 {
                let height = (self.fall_start_z - land_z).max(0.0);
                self.falling = None;
                let (dmg, _max) = fall_damage(height);
                if dmg > 0 {
                    stream.send_app_packet(OP_ENV_DAMAGE, &build_env_damage_packet(gs.player_id, dmg, DMGTYPE_FALLING));
                    gs.cur_hp = (gs.cur_hp - dmg as i32).max(0);
                    gs.log_msg("combat", &format!("Fell {:.0}u — {} fall damage", height, dmg));
                    tracing::info!("EQ: fall damage {dmg} (fell {height:.0}u)");
                }
                tracing::info!("NAV: landed at z={:.1} after {:.0}u fall", land_z, height);
            }
            return;
        }

        // (The dead-player guard now runs earlier — right after stream_position, before the fast-
        // steering refresh and the 150 ms gate — so a corpse stops within a tick. See #238.)

        // Chase (eqoxide#88): when /goto targets a named ENTITY, re-resolve its CURRENT position each
        // tick and follow it, instead of pathing to a one-time snapshot. Roaming mobs move, and their
        // client position is frozen (stale) until they come within the server's ~300u update range —
        // so as the player approaches the stale spot and the mob enters range, its real position is
        // revealed here and the walk homes in on it. If goto_target was cleared (WASD/arrival)
        // while a chase name lingers, the chase is over; if the entity left view, stop cleanly.
        {
            let chase = self.goto_entity.lock().unwrap().clone();
            if let Some(name) = chase {
                if self.goto_target.lock().unwrap().is_none() {
                    *self.goto_entity.lock().unwrap() = None; // cancelled elsewhere
                } else if let Some(&pos) = self.entity_positions.lock().unwrap().get(&name) {
                    *self.goto_target.lock().unwrap() = Some(pos); // follow the entity's latest position
                } else {
                    *self.goto_target.lock().unwrap() = None; // entity despawned / left view
                    *self.goto_entity.lock().unwrap() = None;
                }
            }
        }

        // Teleport detection (#266): a position jump far bigger than one tick of walking (RUN_SPEED
        // ·0.15 ≈ 6.6u) means we were repositioned — an in-zone waterfall teleport, a GM #goto, or a
        // server correction. If we were mid portal-escape, RESTORE the real goal (we're now on the far
        // side of the teleport) and re-plan; any other jump just forces a re-plan off the stale path.
        let jumped = (gs.player_x - self.last_walk_pos[0]).hypot(gs.player_y - self.last_walk_pos[1]) > 40.0;
        self.last_walk_pos = [gs.player_x, gs.player_y, gs.player_z];
        if jumped {
            if let Some(ret) = self.escape_return.take() {
                *self.goto_target.lock().unwrap() = Some(ret);
                tracing::info!("NAV: teleported via in-zone portal — resuming goto to ({:.0},{:.0}) (#266)", ret.0, ret.1);
            }
            self.path_goal = None; // force a re-plan from the new position
        }
        if self.portal_cooldown > 0 { self.portal_cooldown -= 1; }

        let goto = *self.goto_target.lock().unwrap(); // copy out so the lock is released
        let goal = match goto {
            Some(t) => t,
            // No active /goto ⇒ the controller must not be nav-driven. Clearing nav_intent here is the
            // catch-all for the invariant "no goto ⇒ no nav movement": any stop that cleared
            // goto_target without also clearing nav_intent would otherwise leave the controller
            // walking the last wish_dir forever (eqoxide#71). Harmless when already None.
            None    => {
                self.path.clear();
                self.path_goal = None;
                self.escape_return = None; // goto cancelled → abandon any in-progress portal escape (#266)
                *self.nav_intent.lock().unwrap() = None;
                // Only downgrade from "navigating" (an external cancel / WASD). Keep terminal states
                // (arrived / no_path / blocked) so a driver can still read the last outcome (#166).
                if *self.nav_state.lock().unwrap() == "navigating" { self.set_nav_state("idle"); }
                return;
            }
        };

        // (Re)compute a wall-avoiding A* path when the goal changes OR the proactive re-plan is armed
        // (#246). find_path returns waypoints (goal-inclusive); an empty path falls back to a straight
        // line to the goal. `replan_coarse` (armed below when the fine plan can't thread the committed
        // coarse route) re-plans from the CURRENT position without wiping the journey-level recovery
        // budget (nav_repaths / nav_best_gdist) — it's normal steering, not a stall recovery.
        if self.replan_cooldown > 0 { self.replan_cooldown -= 1; }
        let goal_changed = self.path_goal != Some(goal);
        if goal_changed || self.replan_coarse {
            self.path_goal = Some(goal);
            // Re-anchor the walker to the fresh route (which starts at the current position).
            self.path_i = 0;
            self.stuck_i = 0;
            self.backoff_ticks = 0;
            if goal_changed {
                self.stuck_best = f32::MAX;
                self.stuck_ticks = 0;
                self.nav_repaths = 0;
                self.nav_best_gdist = f32::MAX;
                self.local_stuck_ticks = 0;
                self.replan_cooldown = 0;
                self.replan_coarse = false;
            } else {
                // Proactive re-plan (#246): throttle the next one so a persistently-awkward carrot can't
                // thrash the coarse planner every tick, and clear the arm flag. Deliberately DO NOT
                // reset `stuck_ticks` — the stall clock must keep running across proactive re-plans so a
                // genuine wedge the fresh coarse route also can't escape still trips the ~3 s back-off
                // (and eventually the re-path cap), instead of re-planning forever pressed into a wall.
                self.replan_coarse = false;
                self.local_stuck_ticks = 0;
                self.replan_cooldown = REPLAN_COOLDOWN_TICKS;
            }
            // Route with the native collision radius (1.0, was 2.0): the 2× radius boxed the player
            // out of gaps the native client threads, causing "boxed in by walls" / platform stalls
            // (issues #22/#13/#2). Collide-and-slide in the controller keeps it off walls.
            // Aggro-avoidance (#67): route AROUND live NPC camps so a long goto doesn't plow through
            // a mob group and get the player killed. Exclude NPCs near the GOAL — you're walking TO
            // the destination (often a target mob), so its own camp must not be avoided.
            let av = *self.nav_avoid.lock().unwrap();
            let avoid = Self::aggro_avoid(gs, goal, av.enabled);
            let route = self.collision.read().unwrap().as_ref().map(|c|
                plan_path(c, [gs.player_x, gs.player_y, gs.player_z], [goal.0, goal.1, goal.2], &avoid, av.buffer));
            match route {
                // Collision loaded, but A* found NO navmesh route to the target. Report it and STOP,
                // rather than silently straight-lining into geometry and looking "wedged" — a driver
                // needs to tell "unreachable" from "stuck" (#166). Clear path_goal so re-issuing the
                // same target re-evaluates.
                Some(None) => {
                    tracing::info!("NAV: no path to ({:.0},{:.0}) — unreachable, stopping", goal.0, goal.1);
                    gs.log_msg("zone", "No path to destination (unreachable)");
                    self.set_nav_state("no_path");
                    self.path.clear();
                    self.path_goal = None;
                    *self.goto_target.lock().unwrap() = None;
                    *self.nav_intent.lock().unwrap() = None;
                    return;
                }
                Some(Some(path)) => {
                    // Does the coarse route actually REACH the goal, or is it a PARTIAL (goal walk-
                    // unreachable — the char is boxed in)? A full route is goal-inclusive so its last
                    // waypoint sits ~a cell from the goal; a partial ends far short.
                    let reaches = path.last().is_some_and(|w| (w[0]-goal.0).hypot(w[1]-goal.1) <= 10.0);
                    if !reaches && self.escape_return.is_none() && self.portal_cooldown == 0 {
                        // Boxed in. If the only way out is an in-zone translocator (a vault waterfall),
                        // REDIRECT the goto to it — walk in via the normal machinery, the auto-cross
                        // teleports us out, and the jump-restore above resumes the real goal (#266).
                        if let Some(portal) = self.find_in_zone_portal(gs) {
                            tracing::info!("NAV: goal ({:.0},{:.0}) unreachable by walking — escaping the sealed area via the in-zone teleport at ({:.0},{:.0}) (#266)", goal.0, goal.1, portal.0, portal.1);
                            self.escape_return = Some(goal);
                            *self.goto_target.lock().unwrap() = Some(portal);
                            self.portal_cooldown = PORTAL_COOLDOWN_TICKS;
                            self.path_goal = None; // re-plan to the portal next tick
                            *self.nav_intent.lock().unwrap() = None;
                            return;
                        }
                    }
                    self.path = path;
                    self.set_nav_state("navigating");
                }
                // Collision not loaded yet (zoning): keep the old straight-line-toward-goal fallback.
                None => { self.path = Vec::new(); self.set_nav_state("navigating"); }
            }
            tracing::info!("NAV: path to ({:.0},{:.0}) = {} waypoints", goal.0, goal.1, self.path.len());
        }

        // PURE-PURSUIT path following. Chasing each discrete waypoint made the walker OVERSHOOT it
        // (~6.6u/tick at RUN_SPEED vs a 3u arrival radius), oscillate at turns, and drift off the
        // path line into walls — the silent neriakc #2 / gfaydark #4 stall. Instead we steer toward
        // a look-ahead point ON the path line, so the avatar hugs the route through tight turns.
        const LOOK_AHEAD: f32 = 5.0;
        let px = gs.player_x;
        let py = gs.player_y;
        // Advance the active segment while our projection onto it has passed its end.
        while self.path_i + 2 < self.path.len() {
            let (a, b) = (self.path[self.path_i], self.path[self.path_i + 1]);
            let ab = [b[0] - a[0], b[1] - a[1]];
            let l2 = ab[0] * ab[0] + ab[1] * ab[1];
            let t = if l2 < 1e-6 { 1.0 } else { ((px - a[0]) * ab[0] + (py - a[1]) * ab[1]) / l2 };
            if t >= 1.0 { self.path_i += 1; } else { break; }
        }
        let have_path = !self.path.is_empty();
        let target: (f32, f32, f32) = if have_path {
            // Coarse aim: look-ahead carrot on the 8u global route (the fallback).
            let coarse = carrot_along(&self.path, self.path_i, [px, py], LOOK_AHEAD)
                .unwrap_or([goal.0, goal.1, gs.player_z]);
            // TWO-TIER (#nav-multires): thread sub-8u detail near the walker. Plan a FINE 2u route
            // (bounded → cheap, re-run every tick) from here to a carrot ~LOCAL_REACH ahead on the
            // coarse route, and steer along IT. This keeps the walker on thin ramps and threads narrow
            // openings the 8u grid can't resolve. Fall back to the coarse aim if the fine plan can't
            // reach (a real local dead-end), so a local snag never stalls the whole route.
            const LOCAL_REACH: f32 = 24.0;   // how far ahead on the coarse route the fine plan aims
            const LOCAL_CELL:  f32 = 2.0;    // fine grid resolution
            const LOCAL_BOUND: f32 = 40.0;   // cap the fine search radius (keeps it cheap)
            let local_goal = carrot_along(&self.path, self.path_i, [px, py], LOCAL_REACH).unwrap_or(coarse);
            self.local_path = self.collision.read().unwrap().as_ref()
                .and_then(|c| c.find_path_res([px, py, gs.player_z], local_goal,
                    crate::movement::PLAYER_RADIUS, &[], true, LOCAL_CELL, Some(LOCAL_BOUND), 0.0))
                .unwrap_or_default();
            // Proactive re-plan trigger (#246): did the fine plan actually REACH its coarse carrot? If
            // the committed coarse route skims an obstacle the 8u grid missed, the fine 2u plan stops
            // short (truncated partial toward the reachable frontier) and the walker presses into it.
            // Arm a fresh coarse re-plan (from the current position) after a few such ticks — long
            // before the ~3 s stall — so the route detours around the obstacle, matching the overlay.
            let reached_carrot = self.local_path.last().is_some_and(|p|
                (p[0] - local_goal[0]).hypot(p[1] - local_goal[1]) <= LOCAL_CELL * 2.0);
            // Only re-plan proactively while the walker is otherwise moving HEALTHILY: the point is to
            // detour BEFORE bonking. Once it's genuinely wedged (in a back-off, or the stall clock is
            // already climbing), the existing stall/back-off recovery owns it — arming another coarse
            // plan here just piles redundant (and, in some zones, ~2 s) A* runs onto the net thread.
            let healthy = self.backoff_ticks == 0 && self.stuck_ticks < NAV_HOP_TICKS;
            if !reached_carrot && healthy && self.replan_cooldown == 0 {
                self.local_stuck_ticks += 1;
                if self.local_stuck_ticks >= NAV_LOCAL_STUCK_TICKS {
                    self.replan_coarse = true;
                    tracing::debug!("NAV: fine plan blocked short of carrot near ({:.0},{:.0}) — re-planning coarse (#246)", px, py);
                }
            } else {
                self.local_stuck_ticks = 0;
            }
            // Publish the walker's ACTUAL committed plan for the nav-debug overlay (#246) so it draws
            // exactly what the walker follows — coarse route + fine local plan — rather than an
            // independent per-frame recompute that over-states how cleanly the walker is steering.
            *self.nav_path_view.lock().unwrap() = (self.path.clone(), self.local_path.clone());
            let aim = if self.local_path.len() >= 2 {
                carrot_along(&self.local_path, 0, [px, py], LOOK_AHEAD).unwrap_or(coarse)
            } else {
                coarse
            };
            (aim[0], aim[1], aim[2])
        } else {
            self.local_path.clear();
            *self.nav_path_view.lock().unwrap() = (Vec::new(), Vec::new());
            // No path computed: straight-line toward the goal at the player's CURRENT height.
            (goal.0, goal.1, gs.player_z)
        };

        let dx   = target.0 - gs.player_x; // east  delta (server_x)
        let dy   = target.1 - gs.player_y; // north delta (server_y)
        let dist = (dx * dx + dy * dy).sqrt();

        // Controlled-fall waypoint: a big single-step drop the walker can't walk down (find_path's
        // last-resort fall edge). Walk to the edge at the CURRENT height, then begin a controlled
        // fall. Refuse if the fall's native damage would likely be lethal — fall damage is
        // client-applied, so an unguarded drop can suicide a squishy character.
        const FALL_TRIGGER: f32 = 18.0; // bigger than a stair/ledge step (the walk STEP_H is 20)
        let drop_to_target = gs.player_z - target.2;
        // A submerged landing (e.g. a surface pool whose solid bottom find_path routed to) is NOT a
        // lethal fall: you splash into water, which negates fall damage in RoF2, then swim across.
        // Only guard DRY drops — otherwise a ground-level pool reads as a big fall and the character
        // gets stuck at the water's edge (#191).
        let water_landing = self.collision.read().unwrap().as_ref()
            .is_some_and(|c| c.in_water([target.0, target.1, target.2 + 3.0]));
        if drop_to_target > FALL_TRIGGER && dist <= STOP_DIST + 8.0 && !water_landing {
            let (_, max_dmg) = fall_damage(drop_to_target);
            if gs.cur_hp > 0 && max_dmg >= gs.cur_hp as u32 {
                tracing::info!("NAV: fall of {:.0}u (up to {} dmg) would exceed {} hp — stopping at ledge",
                    drop_to_target, max_dmg, gs.cur_hp);
                gs.log_msg("zone", "Fall too dangerous (HP too low) — stopped at the ledge");
                self.set_nav_state("blocked");
                *self.goto_target.lock().unwrap() = None;
                *self.nav_intent.lock().unwrap() = None; // else the controller keeps walking the last
                // wish_dir forever — drifting 1000s of units with no nav activity (eqoxide#71).
                return;
            }
            self.falling = Some(target.2);
            self.fall_start_z = gs.player_z;
            tracing::info!("NAV: stepping off a {:.0}u drop — controlled fall begins", drop_to_target);
            return;
        }

        // Arrival: measure distance to the FINAL goal, not the look-ahead carrot (which always leads
        // by ~LOOK_AHEAD). A one-shot /goto stops for good; a /follow (live `goto_entity`, re-resolved
        // by the block above) settles a bit behind the leader but STAYS latched so it re-engages the
        // moment the leader walks off — instead of clearing the chase and idling forever (#268).
        let gdx = goal.0 - gs.player_x;
        let gdy = goal.1 - gs.player_y;
        let gdist = (gdx * gdx + gdy * gdy).sqrt();
        let following = self.goto_entity.lock().unwrap().is_some();
        match arrival_action(gdist, following) {
            ArrivalAction::FollowHold => {
                // Caught up — hold, but keep goto_target/goto_entity so a later tick drives again once
                // the leader moves past FOLLOW_DIST.
                self.set_nav_state("following");
                self.path.clear();
                self.path_goal = None;
                *self.nav_intent.lock().unwrap() = None; // stand still until the leader moves
                gs.player_heading = eq_heading(gdx, gdy);
                return;
            }
            ArrivalAction::Arrived => {
                if let Some(ret) = self.escape_return.take() {
                    // Reached the in-zone portal but did NOT teleport (auto-cross cooldown / region miss)
                    // — give up the escape and resume the real goal; portal_cooldown blocks a re-escape
                    // (#266). A portal escape is a plain goto (following==false), so it lands here.
                    tracing::info!("NAV: reached the in-zone portal without teleporting — resuming goto to ({:.0},{:.0})", ret.0, ret.1);
                    *self.goto_target.lock().unwrap() = Some(ret);
                    self.path_goal = None;
                    *self.nav_intent.lock().unwrap() = None;
                    return;
                }
                tracing::info!("NAV: arrived at ({:.1},{:.1})", goal.0, goal.1);
                self.set_nav_state("arrived");
                *self.goto_target.lock().unwrap() = None;
                *self.nav_intent.lock().unwrap() = None; // stop driving the controller
                gs.player_heading = eq_heading(gdx, gdy);
                return;
            }
            ArrivalAction::Drive => {} // not there yet — keep walking / re-plan below
        }

        // Long-route progress (#229): a distant goto (e.g. zone_cross across a big overland zone)
        // crosses several tricky spots, each of which can cost a stall-recovery re-path. Reset the
        // re-path budget whenever we get meaningfully CLOSER to the goal, so the cap counts
        // CONSECUTIVE failed recoveries at one wedge — not the total over a long journey that is
        // otherwise progressing fine. Without this the 8-cap killed long crossings partway.
        const REPATH_RESET_DIST: f32 = 200.0;
        if gdist < self.nav_best_gdist - REPATH_RESET_DIST {
            self.nav_best_gdist = gdist;
            self.nav_repaths = 0;
        }

        // Active downhill back-off (eqoxide#212): after a hard stall we drive the REVERSE aim for a
        // few ticks to slide off a wedged slope face onto cleaner ground, THEN re-path from there.
        // This complements #205's start re-anchoring: back off first, then the (now grade-limited)
        // plan routes around the face instead of straight back up it.
        if self.backoff_ticks > 0 {
            self.backoff_ticks -= 1;
            *self.nav_intent.lock().unwrap() = Some(MoveIntent {
                wish_dir:    self.backoff_dir,
                wish_vspeed: 0.0,
                jump:        false,
                want_swim:   false,
                speed:       RUN_SPEED,
                // The back-off must move like a HUMAN (native step-up), NOT with the NAV_CLIMB super-
                // step. Its whole purpose is to slide DOWNHILL off a wedged face; with the 20u nav
                // climb it would instead scale the unwalkable slope/ridge it's wedged against and
                // strand itself higher up (#229). climb 0 → the controller uses STEP_UP (2u), so
                // gravity slides it down the face a player couldn't have climbed in the first place.
                climb:       0.0,
                hop:         false,
            });
            if self.backoff_ticks == 0 {
                // Backed off — re-plan from the cleaner spot. If it still can't route, the next
                // stall (nav_repaths already counted) stops us.
                let av = *self.nav_avoid.lock().unwrap();
                let avoid = Self::aggro_avoid(gs, goal, av.enabled);
                let fresh = self.collision.read().unwrap().as_ref().and_then(|c|
                    plan_path(c, [gs.player_x, gs.player_y, gs.player_z], [goal.0, goal.1, goal.2], &avoid, av.buffer));
                if let Some(np) = fresh {
                    if np.len() > 1 {
                        tracing::warn!("NAV: backed off downhill — re-pathing ({} wp, attempt {})",
                            np.len(), self.nav_repaths);
                        self.path = np;
                        self.path_i = 0;
                        self.stuck_i = 0;
                        self.stuck_ticks = 0;
                    }
                }
            }
            return;
        }

        // Progress-based stall detection. Pure-pursuit advances `path_i` steadily as the avatar moves
        // along the route; if it has NOT advanced for NAV_STUCK_TICKS we're genuinely wedged (or the
        // route crosses a spot the capsule controller can't track). Recover by re-pathing from the
        // ACTUAL position onto a route the controller can follow; cap re-paths so a truly unreachable
        // snag stops instead of looping. (A straight-line goto with no path skips this.)
        if have_path {
            if self.path_i > self.stuck_i {
                self.stuck_i = self.path_i;
                self.stuck_ticks = 0;
            } else {
                self.stuck_ticks += 1;
                if self.stuck_ticks >= NAV_STUCK_TICKS {
                    self.stuck_ticks = 0;
                    if self.nav_repaths < 8 {
                        // Count this recovery, then back off DOWNHILL (reverse the aim) before
                        // re-pathing (the re-plan happens when the back-off completes). This clears
                        // a wedged slope face instead of re-planning from the same stuck spot. (#212)
                        self.nav_repaths += 1;
                        self.backoff_ticks = NAV_BACKOFF_TICKS;
                        self.backoff_dir = if dist > 1e-3 { [-dx / dist, -dy / dist] } else { [0.0, 0.0] };
                        tracing::warn!("NAV: no progress near ({:.1},{:.1}) — backing off downhill (attempt {})",
                            gs.player_x, gs.player_y, self.nav_repaths);
                        return;
                    }
                    tracing::warn!("NAV: stalled (no progress) near ({:.1},{:.1}) — stopping",
                        gs.player_x, gs.player_y);
                    gs.log_msg("zone", "Path stalled — stopped");
                    self.set_nav_state("blocked");
                    *self.goto_target.lock().unwrap() = None;
                    *self.nav_intent.lock().unwrap() = None;
                    return;
                }
            }
        }

        // Planner (design §3.5): the walker no longer slides or writes positions. It emits a
        // MoveIntent toward the current waypoint; the render-thread CharacterController owns
        // collide-and-slide, step-up, gravity and the authoritative position. The streamer
        // (stream_position) sends that position to the server. Heading is set from the aim so the
        // render facing and the streamed heading agree.
        let heading = eq_heading(dx, dy);
        gs.player_heading = heading;
        // Swim when we're in water so the controller actively swims across/up to the surface instead
        // of trudging along the bottom (#191). The controller only swims when want_swim && in_water.
        let swim = self.collision.read().unwrap().as_ref()
            .is_some_and(|c| c.in_water([gs.player_x, gs.player_y, gs.player_z]));
        // Jump-edge execution (eqoxide#190): if the current path segment is a jump — a horizontal
        // hop bigger than any adjacent nav cell, which find_path only emits across a real gap — ask
        // the controller to jump. Gated on being near the takeoff waypoint so the leap starts
        // grounded at the near edge and doesn't re-trigger on landing; the forward wish_dir carries
        // it across (the ~22.7u reach the edge was sized for). The controller ignores jump unless
        // grounded, so it fires exactly once at takeoff.
        let jump = match (self.path.get(self.path_i), self.path.get(self.path_i + 1)) {
            (Some(a), Some(b)) => {
                let seg = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
                let to_takeoff = ((gs.player_x - a[0]).powi(2) + (gs.player_y - a[1]).powi(2)).sqrt();
                seg > JUMP_SEG_MIN && to_takeoff < JUMP_TAKEOFF_DIST
            }
            _ => false,
        };
        *self.nav_intent.lock().unwrap() = Some(MoveIntent {
            wish_dir:    [dx / dist, dy / dist],
            wish_vspeed: 0.0,
            jump,
            want_swim:   swim,
            speed:       RUN_SPEED,
            climb:       0.0, // nav uses the native step-up now (#239); fences handled by hop
            // Net progress has stalled toward this waypoint → ask the controller to hop the barrier
            // (it only does if grounded, off cooldown, and a near-level landing exists beyond). (#41)
            hop:         self.stuck_ticks >= NAV_HOP_TICKS,
        });
    }

    /// Advance the quest turn-in (POST /give) trade-window flow. The full sequence is:
    ///   1. New give request: put the item on the cursor (OP_MoveItem from_slot→30, skip if it's
    ///      already on the cursor), send OP_TradeRequest, and enter the "waiting for ack" state.
    ///   2. The server replies OP_TradeRequestAck (→ gs.trade_ack_ready); only then may we move the
    ///      cursor item into the NPC trade slot — the server rejects cursor→trade moves before the
    ///      trade session exists.
    ///   3. Ack seen: OP_MoveItem cursor(30)→trade slot(3000), then OP_TradeAcceptClick. Clear state.
    /// The server then sends OP_FinishTrade (handled in packet_handler). If no ack arrives within
    /// ~3s we abort and reset. Called every tick (not gated by the 150ms walk throttle).
    fn tick_give(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        // Begin a new give request if one is queued and we're not already mid-trade.
        if self.give_state.is_none() {
            if let Some((npc_id, from_slot)) = self.give.lock().unwrap().take() {
                // Step 1: put the item on the cursor (skip if it's already there). Use the 28-byte
                // structured MoveItem (possessions→cursor); the old flat 12-byte packet was silently
                // dropped by the server, so the item never reached the cursor (eqoxide#26).
                if from_slot != SLOT_CURSOR {
                    stream.send_app_packet(OP_MOVE_ITEM, &build_move_item(from_slot, SLOT_CURSOR));
                    gs.move_item(from_slot as i32, SLOT_CURSOR as i32); // mirror locally
                }
                // Send OP_TradeRequest { to_mob_id = npc, from_mob_id = player }.
                let mut req = [0u8; 8];
                req[0..4].copy_from_slice(&npc_id.to_le_bytes());
                req[4..8].copy_from_slice(&gs.player_id.to_le_bytes());
                stream.send_app_packet(OP_TRADE_REQUEST, &req);
                gs.trade_ack_ready = false;
                self.give_state = Some(GiveState { npc_id, ticks_waiting: 0 });
                tracing::info!("EQ: give: OP_TradeRequest to npc_id={} (item slot {})", npc_id, from_slot);
                gs.log_msg("trade", "Offering item to NPC...");
            }
            return;
        }

        // Mid-trade: either the ack has arrived (advance) or we keep waiting (with a timeout).
        if gs.trade_ack_ready {
            let npc_id = self.give_state.as_ref().map(|g| g.npc_id).unwrap_or(0);
            // Step 3: move the cursor item into the NPC's first trade slot, then accept. The trade
            // slot needs RoF2 typeTrade encoding (not possessions) — build_move_item_to_trade emits
            // the 28-byte structured MoveItem the server actually accepts (eqoxide#26).
            stream.send_app_packet(OP_MOVE_ITEM, &build_move_item_to_trade(SLOT_CURSOR, SLOT_TRADE_BEGIN));
            gs.move_item(SLOT_CURSOR as i32, SLOT_TRADE_BEGIN as i32); // mirror locally
            let mut accept = [0u8; 8];
            accept[0..4].copy_from_slice(&gs.player_id.to_le_bytes());
            // unknown4 = 0 (already zeroed).
            stream.send_app_packet(OP_TRADE_ACCEPT_CLICK, &accept);
            tracing::info!("EQ: give: cursor→trade slot + OP_TradeAcceptClick (npc_id={})", npc_id);
            self.give_state = None;
            gs.trade_ack_ready = false;
        } else if let Some(g) = self.give_state.as_mut() {
            g.ticks_waiting += 1;
            if g.ticks_waiting >= GIVE_ACK_TIMEOUT_TICKS {
                // Abort: cancel the (possibly half-open) trade session and reset.
                stream.send_app_packet(OP_CANCEL_TRADE, &[]);
                tracing::warn!("EQ: give: no trade ack (timed out)");
                gs.log_msg("trade", "Trade timed out (no NPC ack)");
                self.give_state = None;
                gs.trade_ack_ready = false;
            }
        }
    }

    /// Stream the render controller's authoritative position to the server at native cadence
    /// (design §2/§3.4). Runs every tick (not gated by the 150 ms planner). Mirrors the controller's
    /// position into the network `gs` so combat/targeting see the live position, detects genuine
    /// server corrections (>12u jumps the server pushed) and forwards them to the controller, and
    /// sends OP_ClientUpdate at ≤280 ms while moving with a forced 1300 ms keepalive when idle.
    fn stream_position(&mut self, stream: &mut EqStream, gs: &mut GameState) {
        let view = *self.controller_view.lock().unwrap();
        // Don't stream/mirror until the render controller has spawned (else we'd push origin).
        if !view.initialized { return; }
        // Anti-MQGhost keepalive (#105): send a movement-history entry every 30s (< the server's 70s
        // window) whether or not we're moving, so the server's CheatManager never false-flags us.
        if self.last_movement_history_send.elapsed().as_millis() >= MOVEMENT_HISTORY_MS {
            stream.send_app_packet(OP_FLOAT_LIST_THING,
                &build_movement_history(view.pos[0], view.pos[1], view.pos[2]));
            self.last_movement_history_send = Instant::now();
        }
        // A controlled fall owns the Z descent + fall-damage; let it stream, don't fight it here.
        if self.falling.is_some() { return; }
        let gp = [gs.player_x, gs.player_y, gs.player_z];
        if !self.streamed_init {
            self.last_streamed = gp;
            self.last_pos_send = Instant::now();
            self.streamed_init = true;
            return;
        }
        // Genuine server correction: the network gs player jumped (an incoming server packet moved
        // us) far from what we last mirrored. Hand it to the controller; adopt and re-stream it.
        let cd = [gp[0] - self.last_streamed[0], gp[1] - self.last_streamed[1]];
        if cd[0] * cd[0] + cd[1] * cd[1] > CORRECTION_SQ {
            tracing::info!("NAV: server correction → handing controller new pos ({:.1},{:.1},{:.1})", gp[0], gp[1], gp[2]);
            *self.pos_correction.lock().unwrap() = Some(gp);
            self.send_position_update(stream, gs, gp[0], gp[1], gp[2], gs.player_heading);
            self.last_streamed = gp;
            self.last_pos_send = Instant::now();
            return;
        }
        // Normal: stream the controller's position at cadence, then mirror into gs for game logic.
        let pos = view.pos;
        let since = self.last_pos_send.elapsed().as_millis();
        let d = [pos[0] - self.last_streamed[0], pos[1] - self.last_streamed[1], pos[2] - self.last_streamed[2]];
        let moved = d[0] * d[0] + d[1] * d[1] + d[2] * d[2] > 0.01;
        if (moved && since >= POS_SEND_MOVING_MS) || since >= POS_SEND_KEEPALIVE_MS {
            // send_position_update derives deltas from the still-old gs.player_*, so call it first.
            self.send_position_update(stream, gs, pos[0], pos[1], pos[2], view.heading);
            self.last_pos_send = Instant::now();
        }
        gs.player_x = pos[0];
        gs.player_y = pos[1];
        gs.player_z = pos[2];
        gs.player_heading = view.heading;
        self.last_streamed = pos;
    }

    fn send_position_update(
        &mut self,
        stream:  &mut EqStream,
        gs:      &GameState,
        x: f32, y: f32, z: f32,
        heading: f32,
    ) {
        let dx = x - gs.player_x; // east  delta (server_x)
        let dy = y - gs.player_y; // north delta (server_y)
        let dz = z - gs.player_z;
        let moving = dx != 0.0 || dy != 0.0 || dz != 0.0;
        let anim: i32 = if moving { 1 } else { 0 };
        // Internal heading is CCW (0=north, 90=west). The EQ wire (and server) expects
        // CW (0=north, 90=east). The server decodes the wire heading via EQ12toFloat = wire/4,
        // and EQ headings run 0..512 (= 0..360deg), so wire = EQ_units * 4 = deg_cw * 512/360 * 4
        // = deg_cw * 2048/360. (Previously this used 4096/360 = 2x too large, so the server saw
        // the wrong facing and melee never landed — IsFacingMob failed.)
        // Internal heading is CCW (0=north, 90=west). EQ wire expects CW (0=north, 90=east).
        // EQEmu decodes wire heading via EQ12toFloat = wire/4; full circle = 512 EQ units.
        // So wire = cw_degrees * 512/360 * 4 = cw_degrees * 2048/360.
        let h_cw = crate::eq_net::protocol::ccw_to_cw(heading);
        let eq_heading = ((h_cw * 2048.0 / 360.0) as u32) & 0xFFF;

        // RoF2 PlayerPositionUpdateClient_Struct (rof2_structs.h, 46 bytes):
        //   0: sequence(u16)  2: spawn_id(u16)  4: vehicle_id(u16)=0
        //   6: unknown[4]=0   10: delta_x(f32)  14: heading(u32 field, bits 0-11)
        //  18: x_pos(f32)     22: delta_z(f32)  26: z_pos(f32)  30: y_pos(f32)
        //  34: animation(u32 field, bits 0-9)   38: delta_y(f32)
        //  42: delta_heading(u32 field, bits 0-9 signed) = 0
        let mut buf = [0u8; 46];
        buf[0..2].copy_from_slice(&self.position_seq.to_le_bytes()); // sequence
        self.position_seq = self.position_seq.wrapping_add(1);
        buf[2..4].copy_from_slice(&(gs.player_id as u16).to_le_bytes()); // spawn_id
        // vehicle_id = 0 at [4..6], unknown[4] = 0 at [6..10] (already zeroed)
        buf[10..14].copy_from_slice(&dx.to_le_bytes());   // delta_x
        buf[14..18].copy_from_slice(&eq_heading.to_le_bytes()); // heading (12-bit in u32)
        buf[18..22].copy_from_slice(&x.to_le_bytes());    // x_pos (server east)
        buf[22..26].copy_from_slice(&dz.to_le_bytes());   // delta_z
        buf[26..30].copy_from_slice(&z.to_le_bytes());    // z_pos (height)
        buf[30..34].copy_from_slice(&y.to_le_bytes());    // y_pos (server north)
        buf[34..38].copy_from_slice(&anim.to_le_bytes()); // animation (10-bit in u32)
        buf[38..42].copy_from_slice(&dy.to_le_bytes());   // delta_y
        // delta_heading at [42..46] = 0 (already zeroed)
        // Position is a transient firehose — send it UNRELIABLY (ack_req=false), exactly like the
        // native client and the server's own position broadcasts. Sending it on the reliable stream
        // (which we never retransmit) makes a single dropped datagram an unfillable sequence gap, so
        // long continuous runs — which send the most position packets — reliably linkdead (eqoxide#127).
        stream.send_app_packet_unreliable(OP_CLIENT_UPDATE, &buf);
    }

    /// Send OP_ZONE_CHANGE to request crossing a zone line to `target_zone_id`.
    /// ZoneChange_Struct (88 bytes): char_name[64] + zoneID(u16) + instance_id(u16)
    ///   + y(f32) + x(f32) + z(f32) + zone_reason(u32) + success(i32=0)
    /// NOTE: zoneID is sent as **0** (the "resolve from my position" sentinel), NOT the resolved
    /// destination. On zoneID==0 the server (`Handle_OP_ZoneChange`, `zone/zoning.cpp:49`) routes to
    /// `GetClosestZonePointWithoutZone` (`zone.cpp:2093`) — an XY-only, z-agnostic match with no
    /// water-map/OBB check — and derives the real destination from the matched zone point. Sending a
    /// nonzero destination instead routes to `GetClosestZonePoint`, whose water-map `InZoneLine` OBB
    /// test (z-bounded) rejects a valid walk-in with a stale tracked z and logs
    /// `MQZone … with Unknown Destination` (a false positive that could flag/kick on a strict server),
    /// and also hard-cancels if the matched point's target != the named zone. zoneID=0 avoids both.
    /// (`target_zone_id` is kept for logging/clarity; the server resolves the true target itself.)
    /// This is NOT the same as the old bug of sending our *current* zone (target==current → cancel):
    /// 0 is the documented resolve-from-position sentinel, not a zone id. (eqoxide#199)
    fn send_zone_change_packet(&self, stream: &mut EqStream, gs: &GameState, target_zone_id: u16) {
        // RoF2 ZoneChange_Struct is 100 bytes (rof2_structs.h): char_name[64], zoneID@64,
        // instanceID@66, Unknown068@68, Unknown072@72, y@76, x@80, z@84, zone_reason@88,
        // success@92, Unknown096@96. (Titanium put y/x/z at @68/@72/@76 — 8 bytes earlier — which
        // made the RoF2 server read garbage coords and silently ignore the zone-change request.)
        let mut buf = [0u8; 100];
        let name_bytes = gs.player_name.as_bytes();
        let name_len = name_bytes.len().min(64);
        buf[..name_len].copy_from_slice(&name_bytes[..name_len]);
        buf[64..66].copy_from_slice(&0u16.to_le_bytes());             // zoneID = 0 → server resolves from pos (avoids MQZone false positive; eqoxide#199)
        buf[66..68].copy_from_slice(&0u16.to_le_bytes());             // instanceID = 0 (server resolves from matched zone point)
        // @68..76 Unknown068/Unknown072 left zero.
        buf[76..80].copy_from_slice(&gs.player_y.to_le_bytes());      // y (north)
        buf[80..84].copy_from_slice(&gs.player_x.to_le_bytes());      // x (east)
        buf[84..88].copy_from_slice(&gs.player_z.to_le_bytes());      // z
        buf[88..92].copy_from_slice(&0u32.to_le_bytes());             // zone_reason = 0
        buf[92..96].copy_from_slice(&0i32.to_le_bytes());             // success = 0 (request)
        tracing::info!("EQ: sending OP_ZONE_CHANGE target_zone={} from current_zone={} pos=({:.1},{:.1},{:.1})",
                  target_zone_id, gs.zone_id, gs.player_x, gs.player_y, gs.player_z);
        stream.send_app_packet(OP_ZONE_CHANGE, &buf);
    }
}

/// Build a synthetic OP_CLIENT_UPDATE packet so the render loop can update
/// `scene.player_pos` and keep the camera attached during navigation. Uses the real
/// Titanium bit-packed wire format so it decodes the same way as server updates.
/// `heading` (EQ-CCW degrees) carries the nav step direction so the render loop faces
/// the player along the path — server position updates for the player carry no usable
/// heading, so this synthetic packet is the only channel that delivers it.
pub fn make_position_packet(spawn_id: u32, x: f32, y: f32, z: f32, heading: f32) -> AppPacket {
    AppPacket {
        opcode: OP_CLIENT_UPDATE,
        payload: encode_position_update(spawn_id as u16, x, y, z, heading),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_damage_packet_is_rof2_39_byte_layout() {
        // RoF2 EnvDamage2_Struct: 39 bytes with dmgtype@26, constant@33 — the server's
        // DECODE_LENGTH_EXACT drops any other size (Titanium's 31 → silent HP desync, #195).
        let buf = build_env_damage_packet(0x1234_5678, 250, 0xFC /* falling */);
        assert_eq!(buf.len(), 39, "must be the RoF2 39-byte size");
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 0x1234_5678, "id@0");
        assert_eq!(u32::from_le_bytes(buf[6..10].try_into().unwrap()), 250, "damage@6");
        assert_eq!(buf[26], 0xFC, "dmgtype@26 (falling)");
        assert_eq!(u16::from_le_bytes(buf[33..35].try_into().unwrap()), 0xFFFF, "constant@33");
    }

    #[test]
    fn translocate_ack_echoes_prompt_with_complete_set() {
        // A 92-byte prompt: ZoneID=30@0, SpellID=1234@4, coords, Complete=0@88.
        let mut prompt = vec![0u8; 92];
        prompt[0..4].copy_from_slice(&30u32.to_le_bytes());
        prompt[4..8].copy_from_slice(&1234u32.to_le_bytes());
        prompt[80..84].copy_from_slice(&(-76.0f32).to_le_bytes()); // x
        let ack = build_translocate_ack(&prompt);
        assert_eq!(ack.len(), 92, "ack is the 92-byte Translocate_Struct");
        assert_eq!(&ack[0..4], &prompt[0..4], "ZoneID echoed");
        assert_eq!(&ack[4..8], &prompt[4..8], "SpellID echoed");
        assert_eq!(&ack[80..84], &prompt[80..84], "dest x echoed");
        assert_eq!(u32::from_le_bytes(ack[88..92].try_into().unwrap()), 1, "Complete=1 (accept)");
    }

    /// Build a minimal Navigator for unit tests that only exercise a single `sync_*`/tick method —
    /// every other shared slot gets an empty/default placeholder.
    fn test_navigator(group: crate::http::GroupShared) -> Navigator {
        Navigator::new(
            Default::default(), // goto_target
            std::sync::Arc::new(std::sync::Mutex::new("idle".to_string())), // nav_state
            Default::default(), // goto_entity
            Default::default(), // entity_positions
            Default::default(), // entity_ids
            Default::default(), // zone_points
            Default::default(), // task_log
            Default::default(), // task_offers_shared
            Default::default(), // completed_tasks_shared
            Default::default(), // accept_task
            Default::default(), // cancel_task
            group,               // group
            Default::default(), // group_invite
            Default::default(), // trainer_open_req
            Default::default(), // trainer_train_req
            Default::default(), // group_accept
            Default::default(), // group_decline
            Default::default(), // group_leave
            Default::default(), // group_kick
            Default::default(), // group_make_leader
            Default::default(), // zone_cross
            Default::default(), // hail
            Default::default(), // say
            Default::default(), // target
            Default::default(), // who_req
            Default::default(), // attack
            Default::default(), // buy
            Default::default(), // sell
            Default::default(), // trade
            Default::default(), // merchant
            Default::default(), // move_req
            Default::default(), // give
            Default::default(), // inventory
            Default::default(), // loot
            Default::default(), // door_click
            Default::default(), // doors_shared
            Default::default(), // messages
            Default::default(), // dialogue
            Default::default(), // dialogue_click
            Default::default(), // chat_events
            Default::default(), // chat_send
            Default::default(), // cast
            Default::default(), // mem_spell
            Default::default(), // sit
            Default::default(), // consider
            Default::default(), // pet_cmd
            Default::default(), // collision
            std::path::PathBuf::new(), // maps_dir
            Default::default(), // camp
            Default::default(), // controller_view
            Default::default(), // nav_intent
            Default::default(), // pos_correction
            Default::default(), // nav_path_view
            Default::default(), // nav_avoid
            Default::default(), // read_book
            Default::default(), // guild
            Default::default(), // guild_action
        )
    }

    #[test]
    fn dead_player_halts_navigation() {
        // #238: a character that dies mid-goto must stop — the corpse must not keep walking the route.
        // Seed an in-progress nav, then assert nav_halt_if_dead() clears everything and reports dead.
        let seed_nav = |nav: &mut Navigator| {
            *nav.goto_target.lock().unwrap() = Some((100.0, 200.0, 0.0));
            *nav.goto_entity.lock().unwrap() = Some("a bat".into());
            *nav.nav_intent.lock().unwrap() = Some(crate::movement::MoveIntent::default());
            *nav.nav_path_view.lock().unwrap() = (vec![[0.0, 0.0, 0.0]], vec![[0.0, 0.0, 0.0]]);
            nav.path = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0]];
            nav.local_path = vec![[0.0, 0.0, 0.0]];
            nav.path_goal = Some((100.0, 200.0, 0.0));
            nav.path_i = 1;
            *nav.nav_state.lock().unwrap() = "navigating".into();
        };
        let assert_halted = |nav: &Navigator| {
            assert!(nav.goto_target.lock().unwrap().is_none(), "goto_target must clear on death");
            assert!(nav.goto_entity.lock().unwrap().is_none(), "goto_entity must clear on death");
            assert!(nav.nav_intent.lock().unwrap().is_none(), "nav_intent must clear so the controller stops");
            assert!(nav.path.is_empty() && nav.local_path.is_empty(), "route must clear on death");
            assert_eq!(nav.path_goal, None);
            assert_eq!(*nav.nav_state.lock().unwrap(), "idle");
        };
        let new_nav = || {
            let g: crate::http::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(crate::http::GroupSnapshot::default()));
            test_navigator(g)
        };

        // (a) An HP-to-0 update that arrives BEFORE OP_Death (player_dead still false) — the exact
        //     window in which the corpse was seen walking. cur_hp<=0 with a known max must halt nav.
        let mut nav = new_nav();
        seed_nav(&mut nav);
        let mut gs = GameState::new();
        gs.player_dead = false;
        gs.cur_hp = 0;
        gs.max_hp = 1284;
        assert!(nav.nav_halt_if_dead(&gs), "cur_hp<=0 (pre-OP_Death) must halt navigation");
        assert_halted(&nav);

        // (b) The OP_Death flag path (player_dead set, cur_hp already zeroed by apply_death).
        let mut nav = new_nav();
        seed_nav(&mut nav);
        let mut gs = GameState::new();
        gs.player_dead = true;
        gs.cur_hp = 0;
        gs.max_hp = 1284;
        assert!(nav.nav_halt_if_dead(&gs));
        assert_halted(&nav);

        // (c) A LIVE player must NOT be halted (and cur_hp<=0 with max_hp==0 = "unknown", not dead —
        //     e.g. a fresh spawn before the first HP update — must not spuriously stop nav).
        let mut nav = new_nav();
        seed_nav(&mut nav);
        let mut gs = GameState::new();
        gs.player_dead = false;
        gs.cur_hp = 900;
        gs.max_hp = 1284;
        assert!(!nav.nav_halt_if_dead(&gs), "a live player must keep navigating");
        assert!(nav.goto_target.lock().unwrap().is_some(), "live nav must be untouched");
        gs.cur_hp = 0;
        gs.max_hp = 0; // unknown HP, not a death
        assert!(!nav.nav_halt_if_dead(&gs), "cur_hp<=0 with max_hp==0 is unknown HP, not death");
        assert!(nav.goto_target.lock().unwrap().is_some());
    }

    #[test]
    fn arrival_action_follow_stays_latched_goto_stops() {
        use super::{arrival_action, ArrivalAction};
        // One-shot /goto (following=false): stops for good only within STOP_DIST(2u).
        assert_eq!(arrival_action(1.0, false), ArrivalAction::Arrived);
        assert_eq!(arrival_action(3.0, false), ArrivalAction::Drive);
        // /follow (following=true): HOLDS within FOLLOW_DIST(10u) — keeps the chase, never "arrives" —
        // and drives again once the leader moves past it (#268). A one-shot goto never HoldFollows.
        assert_eq!(arrival_action(1.0, true),  ArrivalAction::FollowHold);
        assert_eq!(arrival_action(9.0, true),  ArrivalAction::FollowHold);
        assert_eq!(arrival_action(12.0, true), ArrivalAction::Drive); // leader walked off → re-engage
        // Crucially, a follower within melee range does NOT get the terminal `Arrived` a goto would.
        assert_ne!(arrival_action(1.0, true), ArrivalAction::Arrived);
    }

    #[test]
    fn zone_change_resets_stale_destination_and_path() {
        // #248: a destination + route left over from the PREVIOUS zone must not survive a crossing —
        // in the new zone's coordinate space they aim the walker at a corner near the arrival point
        // and wedge it there. sync_zone_points must clear the goal, path, and recovery state.
        let group: crate::http::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(crate::http::GroupSnapshot::default()));
        let mut nav = test_navigator(group);

        // Simulate an in-progress nav in the OLD zone.
        nav.current_zone = "gfaydark".into();
        *nav.goto_target.lock().unwrap() = Some((100.0, 200.0, 0.0));
        *nav.goto_entity.lock().unwrap() = Some("a bat".into());
        *nav.nav_intent.lock().unwrap() = Some(crate::movement::MoveIntent::default());
        *nav.nav_path_view.lock().unwrap() = (vec![[0.0, 0.0, 0.0]], vec![[0.0, 0.0, 0.0]]);
        nav.path = vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0]];
        nav.local_path = vec![[0.0, 0.0, 0.0]];
        nav.path_goal = Some((100.0, 200.0, 0.0));
        nav.path_i = 1;
        nav.stuck_ticks = 5;
        nav.nav_repaths = 3;
        nav.backoff_ticks = 2;
        nav.replan_coarse = true;
        nav.falling = Some(0.0);
        *nav.nav_state.lock().unwrap() = "blocked".into();

        // Cross into a NEW zone.
        let mut gs = GameState::new();
        gs.zone_name = "crushbone".into();
        nav.sync_zone_points(&gs);

        // Destination + route + recovery state all cleared; walker comes to rest in the new zone.
        assert!(nav.goto_target.lock().unwrap().is_none(), "goto_target must clear on zone change");
        assert!(nav.goto_entity.lock().unwrap().is_none(), "goto_entity must clear on zone change");
        assert!(nav.nav_intent.lock().unwrap().is_none(), "nav_intent must clear so the controller stops");
        let (coarse, fine) = &*nav.nav_path_view.lock().unwrap();
        assert!(coarse.is_empty() && fine.is_empty(), "overlay path must clear on zone change");
        assert!(nav.path.is_empty() && nav.local_path.is_empty(), "route must clear on zone change");
        assert_eq!(nav.path_goal, None);
        assert_eq!(nav.path_i, 0);
        assert_eq!(nav.stuck_ticks, 0);
        assert_eq!(nav.nav_repaths, 0);
        assert_eq!(nav.backoff_ticks, 0);
        assert!(!nav.replan_coarse);
        assert!(nav.falling.is_none());
        assert_eq!(*nav.nav_state.lock().unwrap(), "idle");
        assert_eq!(nav.current_zone, "crushbone");
    }

    #[test]
    fn sync_group_publishes_own_and_other_member_hp_pct() {
        use crate::game_state::{Entity, GroupMember};
        let mut gs = GameState::new();
        gs.player_name = "Aldric".into();
        gs.hp_pct = 88.0;
        gs.group_leader = "Aldric".into();
        gs.group_members = vec![
            GroupMember { name: "Aldric".into(), is_leader: true, level: 10, ..Default::default() },
            GroupMember { name: "Sariel".into(), level: 8, ..Default::default() },
        ];
        gs.upsert_entity(Entity {
            spawn_id: 99, name: "Sariel".into(), level: 8, is_npc: false,
            x: 0.0, y: 0.0, z: 0.0, hp_pct: 42.0, cur_hp: 42, max_hp: 100, race: "HUM".into(),
            heading: 0.0, dead: false, equipment: [0; 9], equipment_tint: [[0; 3]; 9],
            gender: 0, helm: 0, showhelm: 0, face: 0, hairstyle: 0, haircolor: 0, animation: 100, floating: false,
        });

        let group: crate::http::GroupShared = std::sync::Arc::new(std::sync::Mutex::new(crate::http::GroupSnapshot::default()));
        let nav = test_navigator(group.clone());
        nav.sync_group(&gs);

        let snap = group.lock().unwrap();
        assert_eq!(snap.leader, "Aldric");
        assert!(snap.you_are_leader);
        let aldric = snap.members.iter().find(|m| m.name == "Aldric").unwrap();
        assert_eq!(aldric.hp_pct, 88.0); // own HP comes from gs.hp_pct, not gs.entities
        let sariel = snap.members.iter().find(|m| m.name == "Sariel").unwrap();
        assert_eq!(sariel.hp_pct, 42.0); // other member's HP comes from the matching Entity
    }

    #[test]
    fn build_accept_new_task_layout() {
        let b = build_accept_new_task(42, 9001);
        assert_eq!(b.len(), 12);
        assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]), 42);
        assert_eq!(u32::from_le_bytes([b[8], b[9], b[10], b[11]]), 9001);
    }

    #[test]
    fn build_cancel_task_layout() {
        let b = build_cancel_task(3);
        assert_eq!(b.len(), 8);
        assert_eq!(u32::from_le_bytes([b[0], b[1], b[2], b[3]]), 3);
        assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]), 2); // TaskType::Quest
    }

    #[test]
    fn build_group_invite_layout() {
        let b = build_group_invite("Sariel", "Aldric");
        assert_eq!(b.len(), 148);
        assert_eq!(&b[0..6], b"Sariel");
        assert_eq!(b[6], 0); // NUL after the name within the 64-byte field
        assert_eq!(&b[64..70], b"Aldric");
    }

    #[test]
    fn build_group_follow_layout() {
        let b = build_group_follow("Aldric", "Sariel");
        assert_eq!(b.len(), 152);
        assert_eq!(&b[0..6], b"Aldric");
        assert_eq!(&b[64..70], b"Sariel");
    }

    #[test]
    fn build_group_disband_layout_is_148_bytes_confirmed_live() {
        // CONFIRMED against a running EQEmu RoF2 zone server (task-6 live validation, 2026-07-01):
        // the doc's inferred 128-byte COMMON GroupGeneric_Struct was wrong for this build — the
        // server rejected it ("Wrong size on incoming [OP_GroupDisband] ... Got [128], expected
        // [148]") and silently dropped leave/kick/decline packets. It wants the 148-byte
        // RoF2-namespaced struct (name1[64], name2[64], 5 trailing zero uint32s), like GroupInvite.
        let b = build_group_disband("Aldric", "Sariel");
        assert_eq!(b.len(), 148);
        assert_eq!(&b[0..6], b"Aldric");
        assert_eq!(&b[64..70], b"Sariel");
        assert!(b[128..148].iter().all(|&x| x == 0), "trailing 20 bytes (5 u32s) must be zero-filled");
    }

    #[test]
    fn build_group_make_leader_layout() {
        let b = build_group_make_leader("Aldric", "Sariel");
        assert_eq!(b.len(), 456);
        assert_eq!(&b[0..4], &0u32.to_le_bytes()); // Unknown000
        assert_eq!(&b[4..10], b"Aldric");           // CurrentLeader @4
        assert_eq!(&b[68..74], b"Sariel");          // NewLeader @68
    }

    #[test]
    fn auto_combat_engages_add_attacking_player() {
        // Fighting rat #10 (valid, but NOT hitting us); rat #20 aggros and hits us → switch to #20.
        assert_eq!(
            pick_combat_target(Some(10), true, false, Some(20), Some(99)),
            Some(20),
        );
    }

    #[test]
    fn auto_combat_keeps_current_when_it_is_the_attacker() {
        // Current target is one of the mobs hitting us → stay on it; don't thrash to a second add.
        assert_eq!(
            pick_combat_target(Some(10), true, true, Some(20), Some(99)),
            Some(10),
        );
    }

    #[test]
    fn auto_combat_retargets_attacker_when_current_dead() {
        // Current target died; an add is on us → engage the add, not the nearest trash.
        assert_eq!(
            pick_combat_target(Some(10), false, false, Some(20), Some(99)),
            Some(20),
        );
    }

    #[test]
    fn auto_combat_falls_back_to_nearest_trash() {
        // No attacker, current invalid → nearest trash (existing grind behavior).
        assert_eq!(pick_combat_target(Some(10), false, false, None, Some(99)), Some(99));
        // No attacker, current still valid, nobody hitting us → finish current.
        assert_eq!(pick_combat_target(Some(10), true, false, None, Some(99)), Some(10));
        // Nothing to do.
        assert_eq!(pick_combat_target(None, false, false, None, None), None);
    }

    #[test]
    fn build_say_packet_matches_rof2_layout() {
        // RoF2 wire: sender\0 target\0 u32 unk | u32 lang | u32 chan | u32 unk | u8 unk |
        //            u32 skill | message\0   (see rof2.cpp DECODE(OP_ChannelMessage))
        let p = build_say_packet("Aiquestbot", "Guard Phaeton", "Hail, Guard Phaeton");
        let mut o = 0;
        assert_eq!(&p[o..o + 10], b"Aiquestbot"); o += 10;
        assert_eq!(p[o], 0, "sender NUL-terminated"); o += 1;
        assert_eq!(&p[o..o + 13], b"Guard Phaeton"); o += 13;
        assert_eq!(p[o], 0, "target NUL-terminated"); o += 1;
        assert_eq!(u32::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3]]), 0, "unknown"); o += 4;
        assert_eq!(u32::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3]]), 0, "language=CommonTongue"); o += 4;
        assert_eq!(u32::from_le_bytes([p[o], p[o+1], p[o+2], p[o+3]]), 8, "chan_num=Say"); o += 4;
        o += 4;            // unknown u32
        o += 1;            // unknown u8
        o += 4;            // skill_in_language
        let msg_end = o + "Hail, Guard Phaeton".len();
        assert_eq!(&p[o..msg_end], b"Hail, Guard Phaeton");
        assert_eq!(p[msg_end], 0, "message must be null-terminated");
        assert_eq!(p.len(), msg_end + 1);
    }

    fn wall_collision() -> crate::assets::Collision {
        // Vertical wall at world east=5: EQ p2=5 (render.X), north=p0 [0,10], height=p1 [0,10].
        let wall = crate::assets::MeshData {
            positions: vec![[0.0, 0.0, 5.0], [10.0, 0.0, 5.0], [10.0, 10.0, 5.0], [0.0, 10.0, 5.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0, 0.0, 0.0],
            render_mode: crate::assets::RenderMode::Opaque, anim: None,
        };
        crate::assets::Collision::build(
            &crate::assets::ZoneAssets { terrain: vec![wall], objects: vec![], textures: vec![] }, 4.0)
    }

    #[test]
    fn slide_move_slides_along_wall_when_diagonal_blocked() {
        let col = wall_collision();
        // Player at east=3, north=5, stepping toward the wall (east +2) and north (+2).
        // The diagonal hits the wall at east=5, so it should slide to north-only.
        // slide_move(col, px=east, py=north, z, full_dx=east, full_dy=north, radius)
        let r = slide_move(&col, 3.0, 5.0, 0.0, 2.0, 2.0, 2.0);
        assert_eq!(r, Some((0.0, 2.0)), "should slide along north, dropping the blocked east");

        // Moving away from the wall (east -2) is unobstructed → full move.
        assert_eq!(slide_move(&col, 3.0, 5.0, 0.0, -2.0, 2.0, 2.0), Some((-2.0, 2.0)));
    }

    #[test]
    fn build_target_packet_is_spawn_id_le() {
        assert_eq!(build_target_packet(0x12345678), vec![0x78, 0x56, 0x34, 0x12]);
    }

    #[test]
    fn build_gm_training_layout() {
        // GMTrainee_Struct: npcid@0, playerid@4, skills[100]@8 (zero on send), 448 bytes total.
        let b = build_gm_training(0x1122, 0x3344);
        assert_eq!(b.len(), 448);
        assert_eq!(&b[0..4], &0x1122u32.to_le_bytes());
        assert_eq!(&b[4..8], &0x3344u32.to_le_bytes());
        assert!(b[8..].iter().all(|&x| x == 0), "skills[] + trailing sent as zero");
    }

    #[test]
    fn build_gm_train_skill_layout() {
        // GMSkillChange_Struct (12 bytes): npcid u16@0, skillbank u16@4 (0), skill_id u16@8.
        let b = build_gm_train_skill(0x1122, 7 /* Archery */);
        assert_eq!(b.len(), 12);
        assert_eq!(&b[0..2], &0x1122u16.to_le_bytes(), "npcid @0");
        assert_eq!(&b[4..6], &0u16.to_le_bytes(), "skillbank @4 = normal skills");
        assert_eq!(&b[8..10], &7u16.to_le_bytes(), "skill_id @8");
    }

    #[test]
    fn build_gm_end_training_layout() {
        let b = build_gm_end_training(0x1122, 0x3344);
        assert_eq!(b.len(), 8);
        assert_eq!(&b[0..4], &0x1122u32.to_le_bytes());
        assert_eq!(&b[4..8], &0x3344u32.to_le_bytes());
    }

    #[test]
    fn build_movement_history_layout() {
        // EQEmu UpdateMovementEntry is a packed 17-byte struct: Y@0, X@4, Z@8, type@12, ts@13.
        // Must be >= sizeof(UpdateMovementEntry) or the server debug-logs + ignores it (#105).
        let p = build_movement_history(10.0, -20.0, 3.5);
        assert_eq!(p.len(), 17, "UpdateMovementEntry is 17 packed bytes");
        assert_eq!(&p[0..4], &(-20.0f32).to_le_bytes(), "Y field @0 = server north");
        assert_eq!(&p[4..8], &(10.0f32).to_le_bytes(), "X field @4 = server east");
        assert_eq!(&p[8..12], &(3.5f32).to_le_bytes(), "Z field @8");
        assert_eq!(p[12], 1, "type = Collision (benign; skips teleport/zoneline cheat checks)");
    }

    #[test]
    fn build_consider_packet_layout() {
        // #273: RoF2 Consider_Struct is 20 bytes (playerid, targetid, faction, level, pvpcon+pad).
        // The earlier 28-byte size (Titanium, with cur_hp/max_hp) failed the server's
        // DECODE_LENGTH_EXACT, so the consider was dropped and no OP_Consider reply came back.
        let p = build_consider_packet(7, 42);
        assert_eq!(p.len(), 20, "RoF2 Consider_Struct must be exactly 20 bytes");
        assert_eq!(u32::from_le_bytes([p[0], p[1], p[2], p[3]]), 7);
        assert_eq!(u32::from_le_bytes([p[4], p[5], p[6], p[7]]), 42);
    }

    #[test]
    fn guild_command_packet_layout() {
        // #295: GuildCommand_Struct is 140 bytes: othername@0, myname@64, guildeqid(u16)@128,
        // officer(u32 rank)@132. Used for invite (rank=8) and remove/leave.
        let p = build_guild_command("Target", "Me", 42, 8);
        assert_eq!(p.len(), 140);
        assert_eq!(&p[0..6], b"Target");
        assert_eq!(p[6], 0, "othername NUL-terminated");
        assert_eq!(&p[64..66], b"Me");
        assert_eq!(u16::from_le_bytes([p[128], p[129]]), 42);            // guildeqid
        assert_eq!(u32::from_le_bytes([p[132], p[133], p[134], p[135]]), 8); // officer/rank
    }

    #[test]
    fn guild_invite_accept_packet_layout() {
        // #295: GuildInviteAccept_Struct is 136 bytes: inviter@0, newmember@64, response(u32)@128,
        // guildeqid(u32)@132.
        let p = build_guild_invite_accept("Boss", "Me", 8, 42);
        assert_eq!(p.len(), 136);
        assert_eq!(&p[0..4], b"Boss");
        assert_eq!(&p[64..66], b"Me");
        assert_eq!(u32::from_le_bytes([p[128], p[129], p[130], p[131]]), 8);  // response (rank)
        assert_eq!(u32::from_le_bytes([p[132], p[133], p[134], p[135]]), 42); // guildeqid
    }

    #[test]
    fn read_book_packet_layout() {
        // #288: RoF2 BookRequest_Struct is a fixed 8216 bytes (DECODE_LENGTH_EXACT). Verify size,
        // the window/slot/type/target fields, and that the item's Filename lands at offset 22.
        let p = build_read_book_packet(23, 99, "book0001");
        assert_eq!(p.len(), 8216, "BookRequest_Struct must be exactly 8216 bytes");
        assert_eq!(u32::from_le_bytes([p[0], p[1], p[2], p[3]]), 0xFFFF_FFFF); // window = new
        assert_eq!(i16::from_le_bytes([p[4], p[5]]), 23);                       // invslot.Slot
        assert_eq!(u32::from_le_bytes([p[12], p[13], p[14], p[15]]), 1);        // type = Book
        assert_eq!(u32::from_le_bytes([p[16], p[17], p[18], p[19]]), 99);       // target_id
        assert_eq!(&p[22..30], b"book0001");                                    // txtfile = Filename
        assert_eq!(p[30], 0, "txtfile is NUL-terminated after the filename");
    }

    #[test]
    fn read_book_reply_decodes_backtick_newlines() {
        // The reply reuses the same 8216-byte struct; text starts at offset 22, NUL-terminated, and
        // RoF2 encodes newlines as a backtick. Build a synthetic reply and round-trip it.
        let mut reply = vec![0u8; 8216];
        let body = b"line one`line two";
        reply[22..22 + body.len()].copy_from_slice(body);
        let text = parse_read_book_reply(&reply).unwrap();
        assert_eq!(text, "line one\nline two");
    }

    #[test]
    fn build_say_packet_names_are_nul_terminated() {
        // RoF2 names are variable-length cstrings (no fixed 64-byte field). Verify both the
        // sender and target are emitted whole and each terminated by a single NUL.
        let p = build_say_packet("Aiquestbot", "Guard Phaeton", "hi");
        assert_eq!(p[10], 0, "sender NUL-terminated after 'Aiquestbot'");
        assert_eq!(p[11 + 13], 0, "target NUL-terminated after 'Guard Phaeton'");
    }

    #[test]
    fn cast_packet_layout() {
        // RoF2 CastSpell_Struct = 44 bytes (eqoxide#42). gem 1, spell 93, target 27.
        // slot@0, spell_id@4, inventory_slot@8..20 (all -1 = invalid/no-item), target_id@20,
        // cs_unknown@24..32, y/x/z@32..44 all 0. A 20-byte Titanium packet was dropped by the
        // server's DECODE_LENGTH_EXACT — that was the "no spell ever casts" bug.
        let p = build_cast_packet(1, 93, 27);
        assert_eq!(p.len(), 44, "RoF2 CastSpell_Struct is 44 bytes");
        assert_eq!(&p[0..4], &1u32.to_le_bytes(), "slot (gem)");
        assert_eq!(&p[4..8], &93u32.to_le_bytes(), "spell_id");
        assert_eq!(&p[8..20], &[0xFFu8; 12], "inventory_slot = all -1 (no clicky item)");
        assert_eq!(&p[20..24], &27u32.to_le_bytes(), "target_id");
        assert_eq!(&p[24..44], &[0u8; 20], "cs_unknown + y/x/z position = 0");
    }

    #[test]
    fn item_cast_packet_layout() {
        // eqoxide#193: item clicky cast — slot = CastingSlot::Item (22), spell = the item's click
        // effect, inventory_slot = the real possessions slot (Type=0, Slot=n, SubIndex/AugIndex=-1),
        // target@20. Activate the item at general slot 25 (spell 2512) on target 27.
        let p = build_item_cast_packet(25, 2512, 27);
        assert_eq!(p.len(), 44, "RoF2 CastSpell_Struct is 44 bytes");
        assert_eq!(&p[0..4], &22u32.to_le_bytes(), "slot = CastingSlot::Item");
        assert_eq!(&p[4..8], &2512u32.to_le_bytes(), "spell_id = item click effect");
        // inventory_slot @8..20 must equal the possessions-slot encoding for slot 25.
        assert_eq!(&p[8..20], &rof2_possessions_slot(25), "inventory_slot = real item slot");
        assert_eq!(&p[12..14], &25i16.to_le_bytes(), "…Slot field (struct @4) carries wire slot 25");
        assert_eq!(&p[20..24], &27u32.to_le_bytes(), "target_id");
        assert_eq!(&p[24..44], &[0u8; 20], "cs_unknown + y/x/z position = 0");
    }

    #[test]
    fn spawn_appearance_sit_layout() {
        // self 77, type 14 (Animation), 110 (sit) → 8 bytes: u16 id, u16 type, u32 param.
        let p = build_spawn_appearance_packet(77, 14, 110);
        assert_eq!(p.len(), 8);
        assert_eq!(&p[0..2], &77u16.to_le_bytes());
        assert_eq!(&p[2..4], &14u16.to_le_bytes());
        assert_eq!(&p[4..8], &110u32.to_le_bytes());
    }
}

#[cfg(test)]
mod door_tests {
    use super::*;

    #[test]
    fn click_door_layout() {
        let pkt = build_click_door(7, 0x1234);
        assert_eq!(pkt.len(), 16);
        assert_eq!(pkt[0], 7);            // doorid @0
        assert_eq!(pkt[4], 0);            // picklockskill @4 = 0 (observer)
        assert_eq!(&pkt[8..12], &[0, 0, 0, 0]); // item_id @8 = 0
        assert_eq!(&pkt[12..14], &0x1234u16.to_le_bytes()); // player_id @12
        assert_eq!(&pkt[14..16], &[0, 0]); // trailing unknowns zero
    }

    #[test]
    fn move_item_is_rof2_28byte_structured_slots() {
        // RoF2 MoveItem_Struct = from_slot(InventorySlot_Struct,12) + to_slot(…,12) +
        // number_in_stack(u32) = 28 bytes. Each slot is structured {Type, Unk02, Slot, SubIndex,
        // AugIndex, Unk01}, NOT a bare int — the server's RoF2ToServerSlot reads these fields and a
        // flat 12-byte packet fails DECODE_LENGTH_EXACT (silently dropped → the eqoxide#11 scribe
        // failure: the scroll never reached the cursor). Used by the scribe flow to move a scroll
        // from general slot 23 → cursor (33) before OP_MemorizeSpell.
        let pkt = build_move_item(23, SLOT_CURSOR);
        assert_eq!(pkt.len(), 28);
        // from_slot: Type=typePossessions(0), Slot=23, SubIndex/AugIndex=SLOT_INVALID(-1)
        assert_eq!(i16::from_le_bytes([pkt[0], pkt[1]]), 0, "from Type=typePossessions");
        assert_eq!(i16::from_le_bytes([pkt[4], pkt[5]]), 23, "from Slot");
        assert_eq!(i16::from_le_bytes([pkt[6], pkt[7]]), -1, "from SubIndex=SLOT_INVALID");
        assert_eq!(i16::from_le_bytes([pkt[8], pkt[9]]), -1, "from AugIndex=SOCKET_INVALID");
        // to_slot (offset +12): Type=typePossessions(0), Slot=cursor(33)
        assert_eq!(i16::from_le_bytes([pkt[12], pkt[13]]), 0, "to Type=typePossessions");
        assert_eq!(i16::from_le_bytes([pkt[16], pkt[17]]), SLOT_CURSOR as i16, "to Slot=cursor");
        assert_eq!(i16::from_le_bytes([pkt[18], pkt[19]]), -1, "to SubIndex=SLOT_INVALID");
        // number_in_stack = 0 (whole-item move; a count would split a stack)
        assert_eq!(u32::from_le_bytes(pkt[24..28].try_into().unwrap()), 0, "whole-item move");
    }

    #[test]
    fn build_move_item_encodes_bag_content_subindex() {
        // eqoxide#201: moving a bagged item OUT to the cursor. Flat slot 263 = general bag at slot
        // 24 (parent wire 24), sub-index 2 (263 = 251 + (24-23)*10 + 2). The server decodes a
        // possessions slot with SubIndex set to the bagged item (RoF2ToServerSlot, rof2.cpp:7080),
        // so the from_slot must carry Slot=24, SubIndex=2 (NOT SubIndex=-1 like a top-level slot).
        let pkt = build_move_item(263, SLOT_CURSOR);
        assert_eq!(pkt.len(), 28);
        assert_eq!(i16::from_le_bytes([pkt[0], pkt[1]]), 0, "from Type=typePossessions");
        assert_eq!(i16::from_le_bytes([pkt[4], pkt[5]]), 24, "from Slot=parent general slot 24");
        assert_eq!(i16::from_le_bytes([pkt[6], pkt[7]]), 2, "from SubIndex=bag index 2");
        assert_eq!(i16::from_le_bytes([pkt[8], pkt[9]]), -1, "from AugIndex=SOCKET_INVALID");
        // to = cursor: a top-level possessions slot (SubIndex=-1).
        assert_eq!(i16::from_le_bytes([pkt[16], pkt[17]]), SLOT_CURSOR as i16, "to Slot=cursor");
        assert_eq!(i16::from_le_bytes([pkt[18], pkt[19]]), -1, "to SubIndex=SLOT_INVALID");
    }

    #[test]
    fn build_move_item_to_trade_encodes_typetrade_slot() {
        // Quest hand-in cursor→trade step (eqoxide#26). The NPC's first trade slot is server slot
        // SLOT_TRADE_BEGIN(3000); RoF2 decodes typeTrade as server = TRADE_BEGIN + Slot, so the wire
        // Slot must be 0. from = cursor (a possessions slot). A flat 12-byte move was dropped before.
        let pkt = build_move_item_to_trade(SLOT_CURSOR, SLOT_TRADE_BEGIN);
        assert_eq!(pkt.len(), 28);
        // from_slot: Type=typePossessions(0), Slot=cursor(33), SubIndex/AugIndex=-1
        assert_eq!(i16::from_le_bytes([pkt[0], pkt[1]]), 0, "from Type=typePossessions");
        assert_eq!(i16::from_le_bytes([pkt[4], pkt[5]]), SLOT_CURSOR as i16, "from Slot=cursor");
        assert_eq!(i16::from_le_bytes([pkt[6], pkt[7]]), -1, "from SubIndex=SLOT_INVALID");
        // to_slot (offset +12): Type=typeTrade(3), Slot=0 (3000-TRADE_BEGIN), SubIndex/AugIndex=-1
        assert_eq!(i16::from_le_bytes([pkt[12], pkt[13]]), 3, "to Type=typeTrade");
        assert_eq!(i16::from_le_bytes([pkt[16], pkt[17]]), 0, "to Slot=trade index 0");
        assert_eq!(i16::from_le_bytes([pkt[18], pkt[19]]), -1, "to SubIndex=SLOT_INVALID");
        assert_eq!(i16::from_le_bytes([pkt[20], pkt[21]]), -1, "to AugIndex=SOCKET_INVALID");
        // number_in_stack = 0 (whole-item move)
        assert_eq!(u32::from_le_bytes(pkt[24..28].try_into().unwrap()), 0, "whole-item move");
    }
}
