//! Single source of truth for applying EQ server packets to GameState.
//!
//! Called from both the login phase (to keep entity positions current) and the
//! render loop (to update the scene).  No I/O or logging here — just pure state
//! mutation.

use crate::eq_net::protocol::*;
use crate::eq_net::transport::AppPacket;
use crate::game_state::{GameState, Entity, ZonePoint, CastState};

/// Apply one EQ server packet to `gs`.
pub fn apply_packet(gs: &mut GameState, packet: &AppPacket) {
    let p = &packet.payload;
    match packet.opcode {
        OP_NEW_SPAWN            => apply_new_spawn(gs, p),
        OP_DELETE_SPAWN         => apply_delete_spawn(gs, p),
        OP_CLIENT_UPDATE        => apply_position_update(gs, p),
        OP_HP_UPDATE            => apply_hp_update(gs, p),
        OP_MOB_HEALTH           => apply_mob_health(gs, p),
        OP_TARGET_MOUSE         => apply_set_target(gs, p), // synthetic (nav → render gs); see fn doc
        OP_MOVE_ITEM            => apply_move_item(gs, p),  // synthetic (nav → render gs); see fn doc
        OP_NEW_ZONE             => apply_new_zone(gs, p),
        OP_ZONE_SPAWNS          => apply_zone_spawns(gs, p),
        OP_ZONE_ENTRY           => apply_zone_entry(gs, p),
        OP_WEATHER              => { gs.zone_changed = false; }
        OP_PLAYER_PROFILE       => apply_player_profile(gs, p),
        OP_DEATH                => apply_death(gs, p),
        OP_EXP_UPDATE           => apply_exp_update(gs, p),
        OP_LEVEL_UPDATE         => apply_level_update(gs, p),
        OP_CHANNEL_MESSAGE      => apply_channel_message(gs, p),
        OP_SET_CHAT_SERVER      => apply_set_chat_server(gs, p),
        OP_SPECIAL_MESG         => apply_special_message(gs, p),
        OP_FORMATTED_MESSAGE    => apply_formatted_message(gs, p),
        OP_SIMPLE_MESSAGE       => apply_simple_message(gs, p),
        OP_EMOTE                => apply_emote(gs, p),
        OP_CONSIDER             => apply_consider(gs, p),
        OP_SPAWN_APPEARANCE     => apply_spawn_appearance(gs, p),
        OP_SEND_ZONE_POINTS           => apply_zone_points(gs, p),
        OP_SPAWN_DOOR           => apply_spawn_doors(gs, p),
        OP_MOVE_DOOR            => apply_move_door(gs, p),
        OP_REQUEST_CLIENT_ZONE_CHANGE => {
            // The actual response (OP_ZONE_CHANGE with the server's real zone_id, or a same-zone
            // teleport) is sent by the gameplay.rs handler for this opcode. Here we only surface it
            // in the message log. (Previously set gs.pending_server_zone, which the nav thread
            // re-answered with a zoneID=0 packet → misroute; removed for #235.)
            if p.len() >= 4 {
                let zone_id = u16::from_le_bytes([p[0], p[1]]);
                let instance_id = u16::from_le_bytes([p[2], p[3]]);
                tracing::info!("EQ: OP_REQUEST_CLIENT_ZONE_CHANGE → zone_id={zone_id} instance={instance_id} ({} bytes)", p.len());
            } else {
                tracing::info!("EQ: OP_REQUEST_CLIENT_ZONE_CHANGE ({} bytes)", p.len());
            }
            gs.log_msg("zone", "Zone change requested by server");
        }
        OP_ZONE_PLAYER_TO_BIND  => apply_bind_respawn(gs, p),
        OP_DAMAGE               => apply_combat_damage(gs, p),
        OP_MONEY_ON_CORPSE      => apply_money_on_corpse(gs, p),
        OP_MONEY_UPDATE         => apply_money_update(gs, p),
        OP_WEAR_CHANGE          => apply_wear_change(gs, p),
        OP_TASK_DESCRIPTION     => apply_task_description(gs, p),
        OP_TASK_ACTIVITY        => apply_task_activity(gs, p),
        OP_COMPLETED_TASKS      => apply_completed_tasks(gs, p),
        OP_TASK_SELECT_WINDOW   => apply_task_select_window(gs, p),
        OP_GM_TRAINING          => apply_gm_training(gs, p),
        OP_GM_END_TRAINING      => apply_gm_end_training(gs, p),  // synthetic (nav → render gs); see fn doc
        OP_AUTO_ATTACK          => apply_auto_attack(gs, p),      // synthetic (nav → render gs); see fn doc
        OP_UI_LOCAL_ECHO        => apply_ui_local_echo(gs, p),    // internal-only; see protocol.rs
        OP_UI_LOOT_STATE        => apply_ui_loot_state(gs, p),    // internal-only; see protocol.rs
        OP_UI_CLEAR_INVITE      => { gs.pending_invite = None; }  // internal-only; see protocol.rs
        OP_SKILL_UPDATE         => apply_skill_update(gs, p),
        OP_GROUP_UPDATE_B       => apply_group_update_b(gs, p),
        OP_GROUP_UPDATE         => apply_group_join(gs, p),
        OP_GROUP_DISBAND_YOU    => apply_group_disband_you(gs, p),
        OP_GROUP_DISBAND_OTHER  => apply_group_disband_other(gs, p),
        OP_GROUP_LEADER_CHANGE  => apply_group_leader_change(gs, p),
        OP_GROUP_INVITE         => apply_group_invite(gs, p),
        OP_GROUP_ACKNOWLEDGE    => apply_group_acknowledge(gs, p),
        OP_CHAR_INVENTORY       => apply_char_inventory(gs, p),
        OP_ITEM_PACKET          => apply_item_packet(gs, p),
        OP_DELETE_ITEM | OP_DELETE_CHARGE => apply_delete_item(gs, p),
        OP_SHOP_REQUEST         => apply_shop_request(gs, p),
        OP_SHOP_PLAYER_SELL     => apply_shop_player_sell(gs, p),
        OP_SHOP_END             => {
            // Server confirmed the merchant window closed.
            gs.merchant_open = None;
            gs.merchant_items.clear();
            tracing::info!("EQ: merchant window closed (OP_ShopEnd)");
        }
        OP_TRADE_REQUEST_ACK    => {
            // Server acknowledged our OP_TradeRequest — the trade session now exists. The give
            // state machine (navigation.rs) waits on this before moving the item into the NPC slot.
            gs.trade_ack_ready = true;
            tracing::info!("EQ: OP_TradeRequestAck — trade session open");
        }
        OP_FINISH_TRADE         => {
            // Server finalized the trade (0-byte packet). For a quest turn-in this means the NPC
            // accepted the item; if the item didn't match, the server returns it on the cursor
            // via OP_ItemPacket (handled above), which we treat as a soft failure.
            // The server consumed the handed-in items via m_inv.PopItem (zone/trading.cpp) with no
            // per-item packet, so clear our mirrored trade slots now that the turn-in is finalized.
            gs.clear_trade_slots();
            gs.log_msg("trade", "Trade complete");
            tracing::info!("EQ: give: turn-in complete (OP_FinishTrade)");
        }
        OP_ANIMATION            => apply_animation(gs, p),
        OP_BEGIN_CAST           => apply_begin_cast(gs, p),
        OP_MANA_CHANGE          => apply_mana_change(gs, p),
        OP_MEMORIZE_SPELL       => apply_memorize_spell(gs, p),
        OP_INTERRUPT_CAST       => apply_interrupt_cast(gs, p),
        OP_READ_BOOK            => apply_read_book(gs, p),
        OP_GUILD_LIST           => apply_guild_list(gs, p),
        OP_GUILD_MEMBER_LIST    => apply_guild_member_list(gs, p),
        OP_GUILD_MEMBER_UPDATE  => apply_guild_member_update(gs, p),
        OP_GUILD_INVITE         => apply_guild_invite(gs, p),
        OP_WHO_ALL_RESPONSE     => apply_who_all(gs, p),
        _                       => {}
    }
}

/// OP_ReadBook (reply) — the server returns a book/note's text in the same 8216-byte struct we sent
/// (#288). Store the readable text so the observer API can surface it. The trailing empty
/// OP_FinishWindow the server also sends is a no-op for us (not dispatched).
fn apply_read_book(gs: &mut GameState, p: &[u8]) {
    if let Some(text) = crate::eq_net::navigation::parse_read_book_reply(p) {
        gs.log_msg("book", &text);
        gs.last_book_text = Some(text);
    }
}

/// OP_Animation — a spawn performs a one-shot animation.
/// RoF2 Animation_Struct (rof2_structs.h):
///   /*00*/ uint16 spawnid
///   /*02*/ uint8  action   ← byte 2
///   /*03*/ uint8  speed    ← byte 3
/// We record COMBAT swings (action 1..=9: kick/pierce/slash/weapon/hand-to-hand) keyed
/// by spawn_id (the player's own swings arrive under gs.player_id); the renderer plays clip
/// C0{action} for a short window then reverts. Non-combat anim codes are ignored.
fn apply_animation(gs: &mut GameState, p: &[u8]) {
    if p.len() < 4 { return; }
    let spawnid = u16::from_le_bytes([p[0], p[1]]) as u32;
    let action  = p[2];   // RoF2: action at byte 2 (was p[3]=speed — off-by-one)
    if (1..=9).contains(&action) {
        gs.combat_anims.insert(spawnid, (action, std::time::Instant::now()));
    }
}

// ── Native Task-system quest log (OP_TaskDescription / OP_TaskActivity / OP_CompletedTasks) ──────
// These are variable-length, packed (no struct padding) wire records with embedded null-terminated
// strings. Layouts cross-checked against EQEmu titanium.cpp ENCODE(OP_TaskDescription) + the
// TaskActivity_Struct in eq_packet_structs.h. See docs/protocol-notes.md.

/// Read a u32 LE at `*off`, advancing `off`. Returns 0 if out of bounds.
fn rd_u32(p: &[u8], off: &mut usize) -> u32 {
    if *off + 4 > p.len() { return 0; }
    let v = u32::from_le_bytes([p[*off], p[*off + 1], p[*off + 2], p[*off + 3]]);
    *off += 4;
    v
}

/// Read a null-terminated string at `*off`, advancing `off` past the terminator.
fn rd_cstr(p: &[u8], off: &mut usize) -> String {
    let start = *off;
    while *off < p.len() && p[*off] != 0 { *off += 1; }
    let s = String::from_utf8_lossy(&p[start..*off]).into_owned();
    if *off < p.len() { *off += 1; } // skip the null
    s
}

/// Read one byte at `*off`, advancing `off`. Returns 0 if out of bounds.
fn rd_u8(p: &[u8], off: &mut usize) -> u8 {
    if *off >= p.len() { return 0; }
    let v = p[*off];
    *off += 1;
    v
}

/// Read a u16 LE at `*off`, advancing `off`. Returns 0 if out of bounds.
fn rd_u16(p: &[u8], off: &mut usize) -> u16 {
    if *off + 2 > p.len() { return 0; }
    let v = u16::from_le_bytes([p[*off], p[*off + 1]]);
    *off += 2;
    v
}

/// Read a fixed-width `len`-byte field at `*off` as a string, stopping at the first embedded NUL
/// (or the field's end if there isn't one), advancing `off` by exactly `len` regardless of the
/// packet's actual length (clamped to `p.len()` so this never panics on a truncated packet). Used
/// for the Group* structs' `name[64]`-style fixed fields, unlike `rd_cstr`'s variable-length
/// NUL-terminated fields used by the Task-system packets.
fn rd_fixed_cstr(p: &[u8], off: &mut usize, len: usize) -> String {
    let start = (*off).min(p.len());
    let end = (*off + len).min(p.len());
    let slice = &p[start..end];
    let nul = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    let s = String::from_utf8_lossy(&slice[..nul]).into_owned();
    *off += len;
    s
}

/// Extract the display name from an EQ saylink. `EQ::SayLinkEngine::GenerateLink()` (EQEmu
/// common/say_link.cpp) emits exactly two `\x12` delimiters: `\x12<56-char body><Name>\x12` — the
/// body and name are one concatenated segment, not separately delimited. Returns the raw string
/// unchanged if it isn't link-formatted (e.g. empty string for "no reward item") or the body is
/// shorter than SAY_LINK_BODY_SIZE.
fn extract_saylink_text(s: &str) -> String {
    let parts: Vec<&str> = s.split('\x12').collect();
    if parts.len() >= 3 {
        parts[1].get(SAY_LINK_BODY_SIZE..).unwrap_or("").to_string()
    } else {
        s.to_string()
    }
}

/// OP_TaskDescription — a task's header + title + reward. Upserts into gs.tasks (preserving any
/// activities already received for it). Layout: Header{seq,task_id,open_window:u8,task_type,
/// reward_type}(17) + title cstr + Data1{duration,dur_code,start_time}(12) + desc cstr +
/// Data2{has_rewards:u8,coin,xp,faction}(13) + reward_text cstr + item_link cstr + Trailer(4).
/// `sequence_number` (the header's SequenceNumber) is kept — OP_CancelTask addresses a task by it.
/// `reward_item_text` is the item name extracted from item_link's EQ saylink markup.
fn apply_task_description(gs: &mut GameState, p: &[u8]) {
    let mut o = 0usize;
    let sequence_number = rd_u32(p, &mut o);
    let task_id = rd_u32(p, &mut o);
    o += 1; // open_window u8
    let _task_type = rd_u32(p, &mut o);
    let _reward_type = rd_u32(p, &mut o);
    let title = rd_cstr(p, &mut o);
    let _duration = rd_u32(p, &mut o);
    let _dur_code = rd_u32(p, &mut o);
    let _start_time = rd_u32(p, &mut o);
    let description = rd_cstr(p, &mut o);
    o += 1; // has_rewards u8
    let coin_reward = rd_u32(p, &mut o);
    let xp_reward = rd_u32(p, &mut o);
    let _faction = rd_u32(p, &mut o);
    let _reward_text = rd_cstr(p, &mut o);
    let item_link = rd_cstr(p, &mut o);
    let reward_item_text = extract_saylink_text(&item_link);
    if task_id == 0 { return; }
    let title_for_log = title.clone();
    {
        let task = gs.tasks.entry(task_id).or_default();
        task.task_id = task_id;
        task.sequence_number = sequence_number;
        task.title = title;
        task.description = description;
        task.xp_reward = xp_reward;
        task.coin_reward = coin_reward;
        task.reward_item_text = reward_item_text;
        task.status = crate::game_state::TaskStatus::Active;
    }
    gs.log_msg("quest", &format!("Quest accepted: {}", title_for_log));
}

/// OP_TaskActivity — one objective + live progress for a task. Layout: 8×u32 fixed
/// (activity_count,id3,taskid,activity_id,unk,activity_type,unk,unk) + mob_name cstr + item_name
/// cstr + goal_count u32 + 4×u32 unknown + activity_name cstr + done_count u32 (+ unknown).
fn apply_task_activity(gs: &mut GameState, p: &[u8]) {
    let mut o = 0usize;
    let _activity_count = rd_u32(p, &mut o);
    let _id3 = rd_u32(p, &mut o);
    let task_id = rd_u32(p, &mut o);
    let activity_id = rd_u32(p, &mut o);
    let _unk16 = rd_u32(p, &mut o);
    let activity_type = rd_u32(p, &mut o);
    let _unk24 = rd_u32(p, &mut o);
    let _unk28 = rd_u32(p, &mut o);
    let mob_name = rd_cstr(p, &mut o);
    let item_name = rd_cstr(p, &mut o);
    let goal_count = rd_u32(p, &mut o);
    o += 16; // 4 unknown u32s
    let activity_name = rd_cstr(p, &mut o);
    let done_count = rd_u32(p, &mut o);
    if task_id == 0 { return; }
    // Objective text: prefer the explicit name, else the mob/item the step targets.
    let target = if !activity_name.is_empty() { activity_name }
        else if !mob_name.is_empty() { mob_name }
        else { item_name };
    let task = gs.tasks.entry(task_id).or_default();
    task.task_id = task_id;
    let act = crate::game_state::TaskActivity { activity_id, activity_type, target, done_count, goal_count };
    if let Some(existing) = task.activities.iter_mut().find(|a| a.activity_id == activity_id) {
        *existing = act; // progress update
    } else {
        task.activities.push(act);
        task.activities.sort_by_key(|a| a.activity_id);
    }
}

// ── Group management (OP_GroupInvite / OP_GroupFollow / OP_GroupUpdate[B] / OP_GroupDisband* /
// OP_GroupLeaderChange / OP_GroupAcknowledge) ───────────────────────────────────────────────────
// Server-authoritative: every inbound packet here is the server confirming or announcing a group
// change; eqoxide never applies a roster change locally before one of these arrives. See
// docs/eq-technical-knowledgebase/group-protocol.md for full struct layouts and source citations.

/// OP_GroupUpdateB — full personalized roster snapshot, sent at group founding and on refresh.
/// Streamed/variable layout (mirrors OP_PlayerProfile's streaming quirk): header
/// (group_id_or_unused: u32, member_count: u32, leader_name: cstr) then member_count records of
/// (member_index: u32, member_name: cstr, is_merc_flag: u16, merc_owner_name: cstr, level: u32,
/// tank_flag: u8, assist_flag: u8, puller_flag: u8, offline_flag: u32, timestamp: u32).
/// Full-replaces gs.group_members/group_leader.
fn apply_group_update_b(gs: &mut GameState, p: &[u8]) {
    let mut o = 0usize;
    let _group_id = rd_u32(p, &mut o);
    let member_count = rd_u32(p, &mut o);
    let leader_name = rd_cstr(p, &mut o);
    let mut members = Vec::new();
    for _ in 0..member_count {
        if o >= p.len() { break; } // truncated packet — stop instead of reading zeroed garbage
        let _member_index = rd_u32(p, &mut o);
        let member_name = rd_cstr(p, &mut o);
        let is_merc = rd_u16(p, &mut o) != 0;
        let _merc_owner_name = rd_cstr(p, &mut o);
        let level = rd_u32(p, &mut o);
        let tank = rd_u8(p, &mut o) != 0;
        let assist = rd_u8(p, &mut o) != 0;
        let puller = rd_u8(p, &mut o) != 0;
        let offline = rd_u32(p, &mut o) != 0;
        let _timestamp = rd_u32(p, &mut o);
        if member_name.is_empty() { continue; }
        let is_leader = !leader_name.is_empty() && member_name == leader_name;
        members.push(crate::game_state::GroupMember {
            name: member_name, level, is_leader, is_merc, tank, assist, puller, offline,
        });
    }
    gs.group_leader = leader_name;
    gs.group_members = members;
    gs.log_msg("group", "Group roster updated");
}

/// OP_GroupUpdate — an incremental "member joined" notice sent to EVERY existing member (EQEmu
/// `Group::AddMember` queues it to all slots). RoF2 `GroupJoin_Struct` (148 bytes) uses FIXED-width
/// char arrays, NOT NUL-terminated variable fields: owner_name[64]@0, membername[64]@64, merc u8@128,
/// padding[3]@129, level u32@132, unknown[12]@136. Reading the names as sequential `rd_cstr` (as
/// before) advanced only past owner_name's NUL — landing inside the zero padding — so membername came
/// back EMPTY and the append was skipped, leaving existing members blind to later joiners (#101).
fn apply_group_join(gs: &mut GameState, p: &[u8]) {
    let mut o = 0usize;
    let _owner_name = rd_fixed_cstr(p, &mut o, 64);
    let member_name = rd_fixed_cstr(p, &mut o, 64);
    let is_merc = rd_u8(p, &mut o) != 0;
    o += 3; // padding
    let level = rd_u32(p, &mut o);
    if member_name.is_empty() { return; }
    if gs.group_members.iter().any(|m| m.name == member_name) { return; } // already known
    gs.group_members.push(crate::game_state::GroupMember {
        name: member_name.clone(), level, is_merc, ..Default::default()
    });
    gs.push_event("group", "member_joined", &member_name, false, &format!("{member_name} joined the group"));
    gs.log_msg("group", &format!("{member_name} joined the group"));
}

/// OP_GroupDisbandYou — the server telling US we left/were kicked/the group disbanded. 148-byte
/// common GroupGeneric_Struct, but we don't need its fields — the opcode alone means "clear
/// everything".
fn apply_group_disband_you(gs: &mut GameState, _p: &[u8]) {
    gs.group_members.clear();
    gs.group_leader.clear();
    gs.pending_invite = None;
    gs.push_event("group", "disbanded", "", true, "You are no longer in a group");
    gs.log_msg("group", "Group disbanded");
}

/// OP_GroupDisbandOther — someone else left/was removed. 148-byte common GroupGeneric_Struct:
/// name1[64], name2[64]. Which field carries the departing member isn't documented in the
/// decompile; we defensively remove whichever of the two names is a CURRENT roster member (and
/// no-op with a warning if neither matches) rather than guessing wrong and corrupting state.
fn apply_group_disband_other(gs: &mut GameState, p: &[u8]) {
    let mut o = 0usize;
    let name1 = rd_fixed_cstr(p, &mut o, 64);
    let name2 = rd_fixed_cstr(p, &mut o, 64);
    let removed = if gs.group_members.iter().any(|m| m.name == name1) {
        Some(name1.clone())
    } else if gs.group_members.iter().any(|m| m.name == name2) {
        Some(name2.clone())
    } else {
        None
    };
    match removed {
        Some(name) => {
            gs.group_members.retain(|m| m.name != name);
            gs.push_event("group", "member_left", &name, false, &format!("{name} left the group"));
            gs.log_msg("group", &format!("{name} left the group"));
        }
        None => tracing::warn!("EQ: OP_GroupDisbandOther: neither '{name1}' nor '{name2}' matched a current group member"),
    }
}

/// OP_GroupLeaderChange — leader name push. 148-byte common struct: Unknown000[64],
/// LeaderName[64], Unknown128[20].
fn apply_group_leader_change(gs: &mut GameState, p: &[u8]) {
    let mut o = 64usize; // skip Unknown000[64]
    let leader_name = rd_fixed_cstr(p, &mut o, 64);
    if leader_name.is_empty() { return; }
    gs.group_leader = leader_name.clone();
    for m in gs.group_members.iter_mut() {
        m.is_leader = m.name == leader_name;
    }
    gs.push_event("group", "leader_changed", &leader_name, false, &format!("{leader_name} is now the group leader"));
    gs.log_msg("group", &format!("{leader_name} is now the group leader"));
}

/// OP_GMTraining reply — the guildmaster's offered skill CAPS. GMTrainee_Struct: npcid u32@0,
/// playerid u32@4, then `skills[100]` u32 @8 = the value the trainer will raise each skill to
/// (0 = the class can't train it here). Opens the training window; trainable = cap > current
/// (eqoxide#99). The client sent all-zero skills; the server overwrote them with the caps.
fn apply_gm_training(gs: &mut GameState, p: &[u8]) {
    if p.len() < 8 { return; }
    let npcid = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    let mut caps = vec![0u32; crate::skills::NUM_SKILLS];
    for (i, c) in caps.iter_mut().enumerate() {
        let o = 8 + i * 4;
        if o + 4 <= p.len() { *c = u32::from_le_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]); }
    }
    gs.trainer_open = Some(npcid);
    gs.trainer_skills = caps;
    gs.log_msg("trainer", "Training window opened");
}

/// OP_GMEndTraining — SYNTHETIC mirror only. The wire packet is client→server (the server never
/// echoes it), so this arm fires only for the copy navigation.rs sends over app_tx after ending a
/// training session: it closes the RENDER GameState's trainer window (the transient Trainer window
/// gates on `scene.trainer_open`, which otherwise stayed Some forever).
fn apply_gm_end_training(gs: &mut GameState, _p: &[u8]) {
    gs.trainer_open = None;
    gs.trainer_skills.clear();
    gs.log_msg("trainer", "Training window closed");
}

/// OP_AutoAttack — SYNTHETIC mirror only (client→server on the wire; never received). The nav
/// thread mirrors its own OP_AutoAttack sends over app_tx so the RENDER GameState's `auto_attack`
/// tracks the toggle (the Actions/Target windows' Attack button reads `scene.auto_attack`).
/// Payload: 4 bytes, byte[0] = 1 enables / 0 disables — the same buffer sent to the server.
fn apply_auto_attack(gs: &mut GameState, p: &[u8]) {
    gs.auto_attack = p.first().copied().unwrap_or(0) != 0;
}

/// OP_UI_LOCAL_ECHO (internal-only, never on the wire) — local echo of the player's own outgoing
/// chat. Payload: `kind` NUL `text`; logs as gs.log_msg(kind, text) so the chat window shows it.
fn apply_ui_local_echo(gs: &mut GameState, p: &[u8]) {
    let Some(nul) = p.iter().position(|&b| b == 0) else { return; };
    let kind = String::from_utf8_lossy(&p[..nul]).into_owned();
    let text = String::from_utf8_lossy(&p[nul + 1..]).into_owned();
    if kind.is_empty() || text.is_empty() { return; }
    gs.log_msg(&kind, &text);
}

/// OP_UI_LOOT_STATE (internal-only, never on the wire) — mirror of the gameplay loop's auto-loot
/// session, which runs entirely on the NAV GameState. Byte 0: 1 = session active, 0 = idle. On the
/// RENDER GameState `pending_loot` is filled by inbound corpse packets but never drained (only the
/// gameplay loop drains its copy), so going idle also clears it — otherwise `scene.loot_active`
/// would gate the Loot window open forever after the first corpse.
fn apply_ui_loot_state(gs: &mut GameState, p: &[u8]) {
    let active = p.first().copied().unwrap_or(0) != 0;
    gs.loot_session_active = active;
    if !active {
        gs.pending_loot.clear();
        gs.loot_queued_at = None;
    }
}

/// OP_SkillUpdate — one skill's new value (after training or skill-ups). SkillUpdate_Struct:
/// skillId u32@0, value u32@4. Reflects the change into gs.player_skills so /v1/observe/skills and
/// /v1/trainer/list stay current (eqoxide#99).
fn apply_skill_update(gs: &mut GameState, p: &[u8]) {
    if p.len() < 8 { return; }
    let id = u32::from_le_bytes([p[0], p[1], p[2], p[3]]) as usize;
    let val = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
    if gs.player_skills.len() < crate::skills::NUM_SKILLS {
        gs.player_skills = vec![0u32; crate::skills::NUM_SKILLS];
    }
    if id < gs.player_skills.len() {
        gs.player_skills[id] = val;
        gs.log_msg("trainer", &format!("Skill {} raised to {}", crate::skills::skill_name(id as u32).unwrap_or("?"), val));
    }
}

/// OP_GroupInvite (received) — 148-byte GroupInvite_Struct: invitee_name[64], inviter_name[64],
/// then 5 unknown/zero-filled u32s. Only acts when we are the invitee (should always be true for
/// an inbound invite, but guards against a stray/misrouted packet).
fn apply_group_invite(gs: &mut GameState, p: &[u8]) {
    let mut o = 0usize;
    let invitee_name = rd_fixed_cstr(p, &mut o, 64);
    let inviter_name = rd_fixed_cstr(p, &mut o, 64);
    if invitee_name != gs.player_name { return; }
    gs.pending_invite = Some(inviter_name.clone());
    gs.push_event("group", "invite_received", &inviter_name, true, &format!("{inviter_name} invited you to a group"));
    gs.log_msg("group", &format!("{inviter_name} invited you to a group"));
}

/// OP_GroupAcknowledge — 4-byte, no fields. Server→client only; purely a "you joined" UI trigger.
fn apply_group_acknowledge(gs: &mut GameState, _p: &[u8]) {
    gs.push_event("group", "joined", "", true, "You have joined the group");
    gs.log_msg("group", "Joined group");
}

// ── Inventory (OP_CharInventory / OP_ItemPacket) ────────────────────────────────────────────────
// RoF2 serializes items as packed binary blobs — NOT the old Titanium pipe-delimited text.
// OP_CharInventory wire format (rof2.cpp:1043 ENCODE(OP_CharInventory)):
//   uint32 item_count  — 0 → 4-byte zero packet
//   [item_count × SerializeItem output back-to-back, no padding]
// Each item is parsed by crate::eq_net::item::parse_rof2_item which returns (RoF2Item, consumed).
// Slot numbers are RoF2 wire slots: equipment 0-22, general-inv 23-32, cursor 33 (rof2_limits.h).
// We store them directly in InvItem.slot, consistent with how apply_item_packet already works.

/// Push a parsed RoF2 item into `out` as an InvItem, then flatten any bag contents into their own
/// InvItems at the flat wire slot `bag_wire_slot(parent, sub_index)`. Bagged items thus appear in
/// gs.inventory (and `/v1/observe/inventory`) and are movable via the same MoveItem path as any
/// other slot. RoF2 bags don't nest, so one level is enough. (eqoxide#201)
fn push_item_and_contents(out: &mut Vec<crate::game_state::InvItem>, item: crate::eq_net::item::RoF2Item) {
    let parent_slot = item.main_slot as i32;
    let bag_contents = item.bag_contents; // move the Vec out before consuming the rest of `item`
    out.push(crate::game_state::InvItem {
        slot:    parent_slot,
        item_id: item.id,
        name:    item.name,
        icon:    item.icon,
        charges: (item.stacksize as i32).max(1),
        idfile:  item.idfile,
        click_spell_id: item.click_spell_id,
        filename: item.filename,
    });
    for (sub_index, sub) in bag_contents {
        let Some(flat) = crate::game_state::bag_wire_slot(parent_slot, sub_index) else { continue };
        out.push(crate::game_state::InvItem {
            slot:    flat,
            item_id: sub.id,
            name:    sub.name,
            icon:    sub.icon,
            charges: (sub.stacksize as i32).max(1),
            idfile:  sub.idfile,
            click_spell_id: sub.click_spell_id,
            filename: sub.filename,
        });
    }
}

/// OP_CharInventory — the full player inventory + equipment, binary-serialized in RoF2 format.
/// Reads `uint32 item_count` then N back-to-back SerializeItem blobs, replacing gs.inventory.
fn apply_char_inventory(gs: &mut GameState, p: &[u8]) {
    if p.len() < 4 { return; }
    let item_count = u32::from_le_bytes([p[0], p[1], p[2], p[3]]) as usize;
    if item_count == 0 { return; }
    let mut off = 4usize;
    let mut items = Vec::with_capacity(item_count);
    for _ in 0..item_count {
        if off >= p.len() { break; }
        let Some((item, consumed)) = crate::eq_net::item::parse_rof2_item(&p[off..]) else {
            tracing::warn!("EQ: OP_CharInventory: failed to parse item at offset {off}; stopping");
            break;
        };
        push_item_and_contents(&mut items, item);
        off += consumed;
    }
    if !items.is_empty() {
        tracing::info!("EQ: OP_CharInventory: {} items loaded", items.len());
        for it in &items {
            gs.inventory.retain(|x| x.slot != it.slot);
        }
        gs.inventory.extend(items);
    }
}

/// OP_ItemPacket — a single item. The 4-byte header is the `ItemPacketType` (Titanium): 0x64 =
/// Merchant (an item the open merchant offers), 0x66 = Loot, 0x69 = CharInventory, etc. Merchant
/// items are routed to `gs.merchant_items` (for `GET /trade/list` + the HUD merchant window);
/// everything else upserts into the player inventory by slot.
fn apply_item_packet(gs: &mut GameState, p: &[u8]) {
    // RoF2 OP_ItemPacket: ItemPacket_Struct = PacketType (u32) + one binary-serialized item.
    // (Titanium sent pipe-delimited text; RoF2 uses the packed SerializeItem format — see
    // crate::eq_net::item.) 0x64 = Merchant, others (0x66 Loot, 0x69 CharInventory…) are items.
    const ITEM_PACKET_MERCHANT: u32 = 0x64;
    if p.len() < 4 { return; }
    let packet_type = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    let Some((item, _)) = crate::eq_net::item::parse_rof2_item(&p[4..]) else { return; };
    if packet_type == ITEM_PACKET_MERCHANT {
        let mi = crate::game_state::MerchantItem {
            merchant_slot: item.main_slot as u32,
            item_id:       item.id,
            name:          item.name,
            icon:          item.icon,
            price:         item.price,
            quantity:      item.merchant_count as i32,
        };
        gs.merchant_items.retain(|x| x.merchant_slot != mi.merchant_slot);
        gs.merchant_items.push(mi);
        gs.merchant_items.sort_by_key(|m| m.merchant_slot);
    } else {
        const ITEM_PACKET_LOOT: u32 = 0x66;
        if packet_type == ITEM_PACKET_LOOT {
            // A Loot item's `main_slot` is NOT a safe inventory destination — it collides with
            // occupied general-inventory wire slots and would evict a real item (eqoxide#56).
            let it = crate::game_state::InvItem {
                slot:    item.main_slot as i32,
                item_id: item.id,
                name:    item.name,
                icon:    item.icon,
                charges: (item.stacksize as i32).max(1),
                idfile:  item.idfile,
                click_spell_id: item.click_spell_id,
                filename: item.filename,
            };
            apply_looted_item(gs, it);
        } else {
            // OP_CharInventory / equip / cursor etc.: `main_slot` IS the authoritative slot.
            // Expand bag contents too, so a container delivered here shows its items. (eqoxide#201)
            let mut upserts = Vec::new();
            push_item_and_contents(&mut upserts, item);
            for it in upserts {
                // Guard against non-inventory slots. A resync/diagnostic ItemPacketTrade can carry a
                // trade-window slot (>= SLOT_TRADE_BEGIN 3000), a bank slot, or the 0xFFFF sentinel;
                // upserting those leaves phantom items the player can never reach (#275). Real
                // possessions (equip 0-22, general 23-32, cursor 33) and bag contents (251+, < 3000)
                // pass through.
                if it.slot < 0 || it.slot as u32 >= SLOT_TRADE_BEGIN {
                    tracing::debug!("item_packet: skip non-inventory slot {} (item {})", it.slot, it.item_id);
                    continue;
                }
                gs.inventory.retain(|x| x.slot != it.slot);
                gs.inventory.push(it);
            }
        }
    }
}

/// OP_DeleteItem / OP_DeleteCharge (S->C) — the server destroys an item, or removes charges from a
/// stack, at a slot. RoF2 `DeleteItem_Struct` (28 bytes) mirrors `MoveItem`: `from_slot`
/// (InventorySlot_Struct — Type i16@0, Slot i16@4) @0, `to_slot`@12, `number_in_stack`@24. The
/// server sends this to clear a slot during `SwapItemResync` (the "Inventory Desyncronization"
/// recovery after a rejected move); leaving it unhandled left the resync's scratch token stuck in
/// inventory forever, so a quest turn-in appeared to strand junk items in invalid slots (#275).
fn apply_delete_item(gs: &mut GameState, p: &[u8]) {
    if p.len() < 6 { return; }
    let slot_type = i16::from_le_bytes([p[0], p[1]]);
    // Only possessions-type slots (Type 0) address our inventory wire slots.
    if slot_type != 0 { return; }
    let slot = i16::from_le_bytes([p[4], p[5]]) as i32;
    // The resync clear sends number_in_stack = 0xFFFFFFFF; OP_DeleteCharge may send a real count.
    let count = if p.len() >= 28 { u32::from_le_bytes([p[24], p[25], p[26], p[27]]) } else { 0 };
    if let Some(idx) = gs.inventory.iter().position(|i| i.slot == slot) {
        let charges = gs.inventory[idx].charges.max(0) as u32;
        if count > 0 && count < charges {
            gs.inventory[idx].charges -= count as i32; // OP_DeleteCharge: partial stack removal
        } else {
            gs.inventory.remove(idx); // OP_DeleteItem / whole-stack clear
        }
    }
}

/// General-inventory wire slots (rof2_limits.h): 23..=32. Loot lands here (or a bag, not modelled).
const GENERAL_BEGIN: i32 = 23;
const GENERAL_END:   i32 = 32;

/// Place a freshly-looted item into the client inventory model WITHOUT trusting the loot packet's
/// `main_slot` (eqoxide#56). Merge into an existing stack of the same item in general inventory, else
/// drop it into the first free general slot. The server holds the authoritative inventory and a
/// resync (OP_CharInventory on relog / next sync) reconciles anything approximate here.
fn apply_looted_item(gs: &mut GameState, mut it: crate::game_state::InvItem) {
    // Stack-merge: same item already sitting in a general-inventory slot → add to its quantity.
    // Restricted to general slots so we never merge into an EQUIPPED item that shares the id.
    if let Some(stack) = gs.inventory.iter_mut()
        .find(|x| x.item_id == it.item_id && (GENERAL_BEGIN..=GENERAL_END).contains(&x.slot))
    {
        stack.charges = stack.charges.saturating_add(it.charges.max(1));
        return;
    }
    // Otherwise the first free general slot (never an occupied one → no eviction).
    let occupied: std::collections::HashSet<i32> = gs.inventory.iter().map(|x| x.slot).collect();
    if let Some(free) = (GENERAL_BEGIN..=GENERAL_END).find(|s| !occupied.contains(s)) {
        it.slot = free;
    }
    // else: general inventory full (item really went to a bag) — leave main_slot as a best effort;
    // the next server inventory sync corrects it. Don't retain-evict here.
    gs.inventory.retain(|x| x.slot != it.slot);
    gs.inventory.push(it);
}


/// OP_ShopPlayerSell (server→client echo) — confirms a sale completed. The server ENCODEs it as the
/// RoF2 Merchant_Purchase_Struct (20 bytes): npcid @0, inventory_slot(TypelessInventorySlot_Struct —
/// Slot i16 @4, SubIndex @6, AugIndex @8, Unknown @10) @4, quantity @12, price @16. (The old 16-byte
/// read grabbed quantity/price from the wrong offsets, so the item never left `gs.inventory` and the
/// sale looked failed — #269.) `Slot` is the RoF2 wire slot, matching `gs.inventory[].slot`. The
/// server has already removed the item; mirror that so the display + `GET /inventory` refresh.
fn apply_shop_player_sell(gs: &mut GameState, p: &[u8]) {
    if p.len() < 20 { return; }
    let itemslot = i16::from_le_bytes([p[4], p[5]]) as i32;
    let quantity = u32::from_le_bytes([p[12], p[13], p[14], p[15]]) as i32;
    let price    = u32::from_le_bytes([p[16], p[17], p[18], p[19]]);
    if let Some(idx) = gs.inventory.iter().position(|i| i.slot == itemslot) {
        let it = &mut gs.inventory[idx];
        let sold_name = it.name.clone();
        if quantity <= 0 || it.charges <= quantity {
            gs.inventory.remove(idx);
        } else {
            it.charges -= quantity;
        }
        gs.log_msg("merchant", &format!("Sold {} x{} for {}c", sold_name, quantity.max(1), price));
        tracing::info!("EQ: sale confirmed — slot={itemslot} qty={quantity} price={price}");
    }
}

/// OP_ShopRequest (server→client echo) — the merchant accepted (command=Open=1) or rejected
/// (command=Close=0, e.g. KOS faction / busy) the window. MerchantClick_Struct: npc_id(u32) @0,
/// player_id(u32) @4, command(u32) @8. Opens/closes the HUD merchant window + `/trade` session.
fn apply_shop_request(gs: &mut GameState, p: &[u8]) {
    if p.len() < 12 { return; }
    let npc_id = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    let command = u32::from_le_bytes([p[8], p[9], p[10], p[11]]);
    if command == 1 {
        gs.merchant_open = Some(npc_id);
        gs.merchant_items.clear(); // fresh list arrives via OP_ItemPacket(Merchant)
        gs.log_msg("merchant", "Merchant window opened");
        tracing::info!("EQ: merchant window opened (npc_id={npc_id})");
    } else {
        gs.merchant_open = None;
        gs.merchant_items.clear();
        gs.log_msg("merchant", "Merchant won't trade (window closed)");
        tracing::info!("EQ: merchant window closed/refused (npc_id={npc_id}, command={command})");
    }
}

/// OP_CompletedTasks — count then per-entry {task_id, title cstr, completed_time}. The server
/// sends the full record here (not bare ids — the previous flat-u32-array parse was a bug that
/// desynced after the first entry). Flips the matching gs.tasks entry to Completed (inserting a
/// stub if we never saw its OP_TaskDescription, so the id isn't silently lost) and upserts
/// gs.completed_task_history with the title/time the packet already carries.
fn apply_completed_tasks(gs: &mut GameState, p: &[u8]) {
    let mut o = 0usize;
    // Each entry is at least 9 bytes (task_id u32 + empty-title null byte + completed_time u32);
    // clamp so a malformed/truncated count can't spin the loop needlessly.
    let count = rd_u32(p, &mut o).min((p.len() as u32 / 9).max(1));
    for _ in 0..count {
        let task_id = rd_u32(p, &mut o);
        let title = rd_cstr(p, &mut o);
        let completed_time = rd_u32(p, &mut o);
        if task_id == 0 { continue; }
        let task = gs.tasks.entry(task_id).or_insert_with(|| crate::game_state::ActiveTask {
            task_id, ..Default::default()
        });
        task.status = crate::game_state::TaskStatus::Completed;
        if task.title.is_empty() { task.title = title.clone(); }
        if let Some(existing) = gs.completed_task_history.iter_mut().find(|e| e.task_id == task_id) {
            existing.title = title;
            existing.completed_time = completed_time;
        } else {
            gs.completed_task_history.push(crate::game_state::CompletedTaskEntry { task_id, title, completed_time });
        }
    }
}

/// OP_TaskSelectWindow — a set of task offers from an open selector window (an NPC script called
/// `tasksetselector` instead of auto-granting via `assigntask`; no content on this server's live
/// scripts uses this path, but it's a genuine protocol case). Layout: header{task_count,type,
/// task_giver}(12) then per task: task_id, reward_multiplier(f32, unused), duration, duration_code,
/// title cstr, description cstr, has_rewards u8, element_count u32 ("initial active elements").
/// `element_count` is 0 for every offer this server's content can produce; if a future task sends a
/// nonzero count, its nested ActivityInformation::SerializeSelector payload is variable-length and
/// not modeled here — stop parsing this packet (leaving gs.task_offers untouched) and log a warning
/// rather than guess at the layout and desync/garble subsequent offers in the same packet.
fn apply_task_select_window(gs: &mut GameState, p: &[u8]) {
    let mut o = 0usize;
    // Each entry is at least 23 bytes (task_id u32 + reward_multiplier f32 + duration u32 +
    // duration_code u32 + title cstr≥1 + desc cstr≥1 + has_rewards u8 + element_count u32).
    // Header is 12 bytes (task_count u32 + type u32 + task_giver u32). Clamp the count so a
    // malformed/truncated packet can't request unbounded allocation.
    let task_count = rd_u32(p, &mut o);
    let max_entries = (p.len().saturating_sub(12) as u32) / 23;
    let task_count = task_count.min(max_entries);
    let _sel_type = rd_u32(p, &mut o);
    let task_giver = rd_u32(p, &mut o);
    let mut offers = Vec::with_capacity(task_count as usize);
    for _ in 0..task_count {
        let task_id = rd_u32(p, &mut o);
        o += 4; // reward_multiplier f32 (unused)
        let _duration = rd_u32(p, &mut o);
        let _duration_code = rd_u32(p, &mut o);
        let title = rd_cstr(p, &mut o);
        let description = rd_cstr(p, &mut o);
        let has_rewards = rd_u8(p, &mut o) != 0;
        let element_count = rd_u32(p, &mut o);
        if element_count != 0 {
            tracing::warn!(
                "EQ: OP_TaskSelectWindow: task_id={task_id} has element_count={element_count} \
                 (nested ActivityInformation not modeled) — bailing out of this packet"
            );
            return;
        }
        offers.push(crate::game_state::TaskOffer { task_id, npc_id: task_giver, title, description, has_rewards });
    }
    gs.task_offers = offers;
}

// ── Per-opcode helpers ────────────────────────────────────────────────────────

fn apply_new_spawn(gs: &mut GameState, payload: &[u8]) {
    if let Some((info, _)) = parse_rof2_spawn(payload) {
        let name = info.name.clone();
        // If this new spawn is an NPC corpse, queue it for auto-looting. (The dead/Lying flagging that
        // lays the corpse down is now done for ALL spawn paths inside `register_spawn` — #253.)
        let sid = info.spawn_id;
        if info.npc != 0 && name.to_lowercase().contains("corpse") {
            tracing::info!("EQ: NPC corpse spawned: id={} name={:?} → queuing for loot", sid, name);
            gs.pending_loot.push_back(sid);
            if gs.loot_queued_at.is_none() {
                gs.loot_queued_at = Some(std::time::Instant::now());
            }
            gs.log_msg("combat", &format!("Corpse found: {} — auto-looting…",
                name.replace("_corpse", "").replace('_', " ")));
        }
        register_spawn(gs, info);
    }
}

fn apply_delete_spawn(gs: &mut GameState, payload: &[u8]) {
    if payload.len() >= 4 {
        let id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
        gs.remove_entity(id);
    }
}

fn apply_position_update(gs: &mut GameState, payload: &[u8]) {
    let Some(upd) = decode_position_update(payload) else { return; };
    let sid = upd.spawn_id as u32;
    if sid == gs.player_id {
        let dx = upd.x - gs.player_x;
        let dy = upd.y - gs.player_y;
        let dz = upd.z - gs.player_z;
        // Small deltas during movement are NORMAL client/server sync lag (≈ run speed × update
        // interval, so up to ~6u) — they only adjust the logical position (the visual is driven by
        // the WASD override / lerp), so they don't jerk the character. Only surface + count GENUINE
        // corrections (anti-cheat snaps, wall clips, teleports), which are much larger.
        const CORRECTION_SQ: f32 = 144.0; // 12u — above normal movement jitter, below real rubber-bands
        if dx * dx + dy * dy > CORRECTION_SQ {
            tracing::info!("SERVER_CORRECT: player pos ({:.1},{:.1},{:.1}) → ({:.1},{:.1},{:.1}) delta ({:.1},{:.1},{:.1})",
                      gs.player_x, gs.player_y, gs.player_z, upd.x, upd.y, upd.z, dx, dy, dz);
            gs.log_msg("zone", &format!("Server corrected position by ({:.0},{:.0},{:.0})", dx, dy, dz));
            gs.server_corrections = gs.server_corrections.wrapping_add(1);
        }
        gs.player_x = upd.x;
        gs.player_y = upd.y;
        gs.player_z = upd.z;
        // Keep the player's heading live. The nav thread's synthetic position packets carry the
        // step direction here (make_position_packet); without this the render loop's Block B
        // (app.rs) snaps facing back to the stale spawn heading during /goto.
        gs.player_heading = upd.heading;
    } else if let Some(e) = gs.entities.get_mut(&sid) {
        e.x = upd.x;
        e.y = upd.y;
        e.z = upd.z;
        e.heading = upd.heading;
        e.animation = upd.animation;
        tracing::debug!("EQ: npc_pos id={} name={} pos=({:.1},{:.1},{:.1})", sid, e.name, e.x, e.y, e.z);
    } else {
        tracing::debug!("EQ: npc_pos id={} NOT IN ENTITY MAP (known: {})", sid, gs.entities.len());
    }
}

fn apply_hp_update(gs: &mut GameState, payload: &[u8]) {
    if payload.len() >= SIZE_HP_UPDATE {
        let hp = unsafe { safe_read::<HPUpdate_S>(payload) };
        gs.update_hp(hp.spawn_id as u32, hp.cur_hp as i32, hp.max_hp);
    }
}

/// OP_MobHealth: percent-only HP for a mob you have targeted/x-targeted but aren't
/// grouped with (the server only sends the full OP_HPUpdate to self/group/pet).
/// Without this, a fought mob's `hp_pct` — and thus `target_hp_pct` — stays frozen
/// at its seeded value the whole fight. (eqoxide#51)
fn apply_mob_health(gs: &mut GameState, payload: &[u8]) {
    if payload.len() >= SIZE_MOB_HEALTH {
        let mh = unsafe { safe_read::<MobHealth_S>(payload) };
        gs.update_hp_pct(mh.spawn_id as u32, mh.hp as f32);
    }
}

/// Synthetic packet (OP_TARGET_MOUSE on the app_tx channel, NOT from the server): the nav thread
/// emits this when a /v1/combat/target request sets the target, so the render GameState — which
/// backs the HUD and HTTP API — learns the target_id (a client-initiated change that otherwise
/// only reaches the network GameState). Payload is the 4-byte LE spawn_id (build_target_packet).
/// target_name/_hp_pct are seeded from the entity here and kept live in app.rs from the entity list.
/// See the two-GameState split note. (eqoxide#9)
/// Synthetic OP_MoveItem (nav → render gs). `/v1/inventory/move` sends the real 28-byte move to the
/// server (which applies it silently, no echo for a client-initiated move) and updates the network
/// gs; the render gs would otherwise only learn of it on the next OP_CharInventory (relog/zone),
/// leaving held-item models stale. The nav thread mirrors the move here via app_tx so
/// `scene.*_weapon_idfile` refresh within a frame. Payload is a synthetic 8 bytes: from_slot(i32
/// LE) + to_slot(i32 LE).
///
/// IMPORTANT: the server DOES send `OP_MoveItem` to the client in other flows (trade, autostack,
/// resync — EQEmu `zone/trading.cpp`, `zone/inventory.cpp`, `zone/client.cpp`), as a 28-byte
/// `MoveItem_Struct`. Those inbound packets are dispatched through this same `apply_packet` on both
/// gamestates, so they reach this handler too — and decoding the wire struct's first 8 bytes as
/// (from,to) would relocate slot 0 into a garbage slot and corrupt the inventory. Guard on the
/// EXACT synthetic length (8) so only our own synthetic packet is applied; the 28-byte wire form is
/// ignored here (real inventory changes arrive via OP_CharInventory / OP_ItemPacket).
/// (eqoxide#141, same render/network GameState split as #9.)
fn apply_move_item(gs: &mut GameState, payload: &[u8]) {
    if payload.len() != 8 { return; } // synthetic is exactly 8; ignore the 28-byte inbound wire MoveItem_Struct
    let from = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let to   = i32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    gs.move_item(from, to);
}

fn apply_set_target(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 4 { return; }
    let id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    gs.target_id = Some(id);
    gs.target_con = None;
    match gs.entities.get(&id) {
        Some(e) => { gs.target_name = Some(e.name.clone()); gs.target_hp_pct = Some(e.hp_pct); }
        None    => { gs.target_name = None; gs.target_hp_pct = None; }
    }
}

fn apply_new_zone(gs: &mut GameState, payload: &[u8]) {
    gs.doors.clear();
    // Purge the previous zone's spawns (#270). OP_NewZone fires on EVERY server-driven zone entry
    // — normal travel, a same-zone #zone, AND a death-respawn — whereas the login/gameplay reconnect
    // clears (login.rs / gameplay.rs) only run on the first-entry path. Without this, respawns and
    // re-zones accumulate stale + duplicate cross-zone entities, so name→position resolution
    // (goto/follow/merchant/target-by-name) picks ghosts. The OP_ZoneEntry spawn stream that follows
    // repopulates the map for the new zone; sync_entities full-replaces the HTTP maps from it.
    gs.entities.clear();
    if payload.len() < SIZE_NEW_ZONE { return; }
    // RoF2 NewZone_Struct (rof2_structs.h, 948 bytes). Use direct byte offsets
    // to avoid struct-padding issues with the packed 948-byte layout.
    // zone_short_name[128] @ offset 64
    let zs_end = 64 + payload[64..192].iter().position(|&b| b == 0).unwrap_or(128);
    gs.zone_name = String::from_utf8_lossy(&payload[64..zs_end]).into_owned();
    // safe_y @ 588, safe_x @ 592, safe_z @ 596
    gs.safe_y = f32::from_le_bytes([payload[588], payload[589], payload[590], payload[591]]);
    gs.safe_x = f32::from_le_bytes([payload[592], payload[593], payload[594], payload[595]]);
    gs.safe_z = f32::from_le_bytes([payload[596], payload[597], payload[598], payload[599]]);
    // underworld (min-z floor) @ 608 — below this the server does a ZoneToBindPoint recovery (#150).
    gs.zone_underworld = Some(f32::from_le_bytes([payload[608], payload[609], payload[610], payload[611]]));
    // zone_id @ 852
    gs.zone_id = u16::from_le_bytes([payload[852], payload[853]]);
    gs.zone_changed = true;
    let entered = format!("Entered {}", gs.zone_name);
    gs.log_msg("zone", &entered);
    // Surface zone changes on the async event feed (GET /v1/events/navigate or /all) so an agent
    // driving the client hears "I just zoned" as soon as it happens — including server-initiated
    // zone changes and cross-zone respawns. `directed` since it concerns us.
    gs.push_event("navigate", "zone", "system", true, &entered);
}

fn apply_zone_spawns(gs: &mut GameState, payload: &[u8]) {
    // RoF2 OP_ZoneSpawns: stream of variable-length spawn records.
    let mut offset = 0usize;
    while offset < payload.len() {
        match parse_rof2_spawn(&payload[offset..]) {
            Some((info, consumed)) => {
                register_spawn(gs, info);
                offset += consumed;
            }
            None => break,
        }
    }
}

fn apply_zone_entry(gs: &mut GameState, payload: &[u8]) {
    // RoF2: OP_ZoneEntry is sent ONCE PER SPAWN (not just for the player). EQEmu's
    // ENCODE(OP_ZoneEntry) forwards directly to ENCODE(OP_ZoneSpawns), which emits
    // one new EQApplicationPacket(OP_ZoneEntry, ...) containing a single Spawn_Struct
    // for each entity in the zone (rof2.cpp:4542/4575/4660). Register every one of
    // them; `register_spawn` handles player-self detection internally.
    if let Some((info, _)) = parse_rof2_spawn(payload) {
        tracing::debug!("EQ: ZONE_ENTRY spawn id={} name='{}' npc={} pos=({:.1},{:.1},{:.1})",
            info.spawn_id, info.name, info.npc, info.x, info.y, info.z);
        register_spawn(gs, info);
    }
}

/// EQ class id (1..=16) → name. From EQEmu common/classes.h.
pub fn class_name(id: u32) -> &'static str {
    match id {
        1 => "Warrior", 2 => "Cleric", 3 => "Paladin", 4 => "Ranger",
        5 => "Shadow Knight", 6 => "Druid", 7 => "Monk", 8 => "Bard",
        9 => "Rogue", 10 => "Shaman", 11 => "Necromancer", 12 => "Wizard",
        13 => "Magician", 14 => "Enchanter", 15 => "Beastlord", 16 => "Berserker",
        _ => "",
    }
}

/// Useful fields parsed from the RoF2 PlayerProfile_Struct wire packet.
pub struct ProfileInfo {
    pub level: u32,
    pub class_id: u32,
    pub coin: [u32; 4],  // platinum, gold, silver, copper
    pub stats: [u32; 7], // STR, STA, CHA, DEX, INT, AGI, WIS
    pub mem_spells: [u32; 9], // 9 memorized spell-gem ids; 0xFFFFFFFF = empty
    /// Current HP from the profile (rof2_structs.h /*00948*/ cur_hp; before `disciplines` so the
    /// struct offset is correct). The profile carries no max_hp (it's derived), so the caller seeds
    /// max_hp = cur_hp (full at zone-in) until the first real OP_HPUpdate. (eqoxide#19)
    pub cur_hp: u32,
    /// Current mana from the profile (rof2_structs.h /*00944*/ mana; just before cur_hp, also before
    /// `disciplines` so the offset is reliable). 0 for non-casters. No max in the profile — seeded
    /// = cur (full at zone-in), then OP_ManaChange tracks current. (eqoxide#27)
    pub cur_mana: u32,
}

/// Parse the RoF2 PlayerProfile_Struct wire packet for the fields needed by the HUD/API.
///
/// All byte offsets are from EQEmu common/patches/rof2.cpp ENCODE(OP_PlayerProfile) —
/// the encode uses sequential WriteUInt32/WriteFloat/WriteUInt8 calls without padding,
/// so these are WIRE offsets (not struct compiler offsets):
///
///   @16:   gender (u8)
///   @17:   race (u32)
///   @21:   class_ (u8)
///   @22:   level (u8)
///   @184:  equipment[9] visual slots — Texture_Struct (20B each), first u32 = Material
///          Slots: helm(0) chest(1) arms(2) wrists(3) hands(4) legs(5) feet(6) primary(7) secondary(8)
///   @808:  tint_count (u32) = 9
///   @812:  item_tint[9] — Tint_Struct (4B each): Blue(u8), Green(u8), Red(u8), UseTint(u8)
///   @952:  STR(u32), STA@956, CHA@960, DEX@964, INT@968, AGI@972, WIS@976
///   @1008: aa_count(u32)=300, aa_array[300]×12B  — (passes through to other fixed fields)
///   @9380: mem_spell_count(u32)=16
///   @9384: mem_spells[16] (int32 each; 0xFFFF_FFFF = empty gem)
///   @12869: platinum(u32), gold@12873, silver@12877, copper@12881
///
/// Returns None if the payload is too short to contain even the basic stats block (@976).
/// Coin and mem_spells default to zeros/empty when the payload is shorter than their offsets.
pub fn parse_player_profile(payload: &[u8]) -> Option<ProfileInfo> {
    // Minimum: WIS at @976 needs @976+4 = @980 bytes.
    if payload.len() < 980 { return None; }
    let u32_at = |o: usize| u32::from_le_bytes([payload[o], payload[o + 1], payload[o + 2], payload[o + 3]]);
    let class_id = payload[21] as u32;
    let level    = payload[22] as u32;
    let stats = [
        u32_at(952), u32_at(956), u32_at(960), u32_at(964),
        u32_at(968), u32_at(972), u32_at(976),
    ];
    // NOTE on offsets past @952: RoF2 *streams* OP_PlayerProfile (rof2.cpp
    // ENCODE(OP_PlayerProfile)), so the rof2_structs.h struct offsets are only
    // valid up to `disciplines`. ENCODE writes structs::MAX_PP_DISCIPLINES = 300
    // disciplines, but the struct reserves only 200 (/*05124*/ disciplines, 800B),
    // a 100-entry / +400-byte undercount. Every field after disciplines therefore
    // sits 400 bytes later on the wire than its struct comment claims.
    // (Stats @952 and earlier are *before* disciplines, so they stay correct.)

    // mem_spells[0..9]: first 9 of the 16 spell gem slots.
    // rof2_structs.h /*09384*/ + 400 = @9784.
    let mem_spells = if payload.len() >= 9784 + 9 * 4 {
        let mut m = [0xFFFF_FFFFu32; 9];
        for (i, slot) in m.iter_mut().enumerate() { *slot = u32_at(9784 + i * 4); }
        m
    } else { [0xFFFF_FFFFu32; 9] };
    // coin: rof2_structs.h /*12869*/ platinum + 400 = @13269 (gold 13273, silver
    // 13277, copper 13281). Reading @12869 landed in the buff array → garbage coin.
    let coin = if payload.len() >= 13285 {
        [u32_at(13269), u32_at(13273), u32_at(13277), u32_at(13281)]
    } else { [0u32; 4] };
    // cur_hp: rof2_structs.h /*00948*/ — before `disciplines`, so the struct offset is correct
    // (same reliable region as stats@952). The len>=980 check above already guarantees @948 is read.
    let cur_hp = u32_at(948);
    // mana: rof2_structs.h /*00944*/ — 4 bytes before cur_hp@948, same reliable pre-disciplines region.
    let cur_mana = u32_at(944);
    Some(ProfileInfo { level, class_id, stats, coin, mem_spells, cur_hp, cur_mana })
}

/// RoF2 `BeginCast_Struct` (10 bytes, EQEmu common/patches/rof2_structs.h:719):
///   /*00*/ uint32 spell_id;  /*04*/ uint16 caster_id;  /*06*/ uint32 cast_time; (ms)
/// The old Titanium-style read (caster u16@0, spell u16@2, cast_ms u32@4) misaligned every field —
/// cast_ms straddled caster_id + the low half of cast_time, yielding the ~108-hour phantom cast bar
/// (eqoxide#222). Returns (caster_id, spell_id, cast_ms).
pub fn parse_begin_cast(p: &[u8]) -> Option<(u16, u32, u32)> {
    if p.len() < 10 { return None; }
    let spell_id  = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    let caster_id = u16::from_le_bytes([p[4], p[5]]);
    let cast_ms   = u32::from_le_bytes([p[6], p[7], p[8], p[9]]);
    Some((caster_id, spell_id, cast_ms))
}

pub fn parse_mana_change(p: &[u8]) -> Option<u32> {
    if p.len() < 4 { return None; }
    Some(u32::from_le_bytes([p[0], p[1], p[2], p[3]]))
}

pub fn parse_memorize_spell(p: &[u8]) -> Option<(u32, u32, u32)> {
    if p.len() < 12 { return None; }
    let r = |o: usize| u32::from_le_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
    Some((r(0), r(4), r(8)))
}

pub fn parse_interrupt_cast(p: &[u8]) -> Option<u32> {
    if p.len() < 4 { return None; }
    Some(u32::from_le_bytes([p[0], p[1], p[2], p[3]]))
}

fn apply_player_profile(gs: &mut GameState, payload: &[u8]) {
    // ── RoF2 PlayerProfile early fixed fields ──────────────────────────────────
    // rof2.cpp ENCODE(OP_PlayerProfile) / rof2_structs.h:
    //   @16: gender(u8), @17: race(u32), @21: class_(u8), @22: level(u8)
    if payload.len() >= 23 {
        let gender   = payload[16];
        let race     = u32::from_le_bytes([payload[17], payload[18], payload[19], payload[20]]);
        let class_id = payload[21] as u32;
        let level    = payload[22] as u32;
        gs.player_gender = gender;
        let race_code = eq_race_to_code(race).to_string();
        if !race_code.is_empty() { gs.player_race = race_code; }
        let cls = class_name(class_id);
        if !cls.is_empty() { gs.player_class = cls.to_string(); }
        if (1..=65).contains(&level) { gs.player_level = level; }
        tracing::info!("EQ: PlayerProfile: level={} class={} race={} gender={}",
            level, cls, race, gender);
    }

    // ── Stats, coin, mem_spells (fixed offsets, no variable-length content before them) ──
    if let Some(p) = parse_player_profile(payload) {
        gs.coin = p.coin;
        gs.stats = p.stats;
        gs.mem_spells = p.mem_spells;
        // Seed the player's own HP. The server only sends a self OP_HPUpdate when HP *changes*
        // (EQEmu Mob::SendHPUpdate), so without this the player's hp stays 0/0 at full health
        // forever — the HUD/API show no health (eqoxide#19). The profile has no max_hp (derived),
        // so use cur_hp as the initial max (full at zone-in); the first real OP_HPUpdate then
        // supplies the true max. Don't clobber a max already learned from an OP_HPUpdate.
        if p.cur_hp > 0 {
            gs.cur_hp = p.cur_hp as i32;
            if gs.max_hp <= 0 { gs.max_hp = p.cur_hp as i32; }
            gs.hp_pct = (gs.cur_hp as f32 / gs.max_hp.max(1) as f32) * 100.0;
            // A profile with HP means we're alive (respawn/zone-in) → clear death bookkeeping.
            gs.player_dead = false;         // nav walker / dead pose (eqoxide#61)
            gs.player_dead_since = None;    // respawn safety-net timer (eqoxide#50)
            tracing::info!("EQ: PlayerProfile: seeded hp={}/{} ({:.0}%)", gs.cur_hp, gs.max_hp, gs.hp_pct);
        }
        // Seed mana the same way (eqoxide#27): no max in the profile, so set_mana seeds max = cur
        // (a rested caster is at full mana at zone-in) and OP_ManaChange keeps `cur_mana` live.
        // 0 for non-casters → 0% (correct). Only seed once, before the first OP_ManaChange.
        if gs.max_mana <= 0 {
            gs.set_mana(p.cur_mana as i32);
            if p.cur_mana > 0 {
                tracing::info!("EQ: PlayerProfile: seeded mana={}/{} ({:.0}%)", gs.cur_mana, gs.max_mana, gs.mana_pct);
            }
        }
    }

    // ── Face + hair style (rof2_structs.h PlayerProfile_Struct) ────────────────
    // /*00896*/ hairstyle  /*00897*/ beard  /*00898*/ face
    if payload.len() >= 899 {
        gs.player_hairstyle = payload[896];
        gs.player_face      = payload[898];
        gs.player_haircolor = payload[888]; // rof2 PlayerProfile_Struct /*00888*/ haircolor (u8)
        tracing::debug!("EQ: PlayerProfile: face={} hairstyle={} haircolor={}",
            gs.player_face, gs.player_hairstyle, gs.player_haircolor);
    }

    // ── Equipment materials @184 + i*20, tints @812 + i*4 ──────────────────────
    // rof2.cpp writes 9 Texture_Struct entries (20B each) starting at @184 for visual
    // slots (helm..secondary). Tint_Struct[9] written at @812 as Color(u32) BGRA each.
    // rof2_structs.h: /*00184*/ Texture_Struct equip_helmet .. equip_secondary,
    //                 /*00812*/ TintProfile item_tint (9 × Tint_Struct)
    // Only overwrite with a real material — spawn's materials take precedence over profile
    // zeros (EQEmu often leaves equipment2 zeroed on zone-in, relying on WearChange later).
    let u32_at = |o: usize| u32::from_le_bytes([payload[o], payload[o+1], payload[o+2], payload[o+3]]);
    for i in 0..9usize {
        let mo = 184 + i * 20;
        if mo + 4 <= payload.len() {
            let mat = u32_at(mo);
            if mat != 0 { gs.player_equipment[i] = mat; }
        }
        let to = 812 + i * 4;
        if to + 4 <= payload.len() {
            // Tint_Struct: Blue=byte0, Green=byte1, Red=byte2, UseTint=byte3 → store as RGB
            let (b_b, g_b, r_b) = (payload[to], payload[to+1], payload[to+2]);
            if r_b != 0 || g_b != 0 || b_b != 0 {
                gs.player_equipment_tint[i] = [r_b, g_b, b_b];
            }
        }
    }

    // ── Skills @04616 (rof2 PlayerProfile_Struct skills[100]) ──────────────────────
    // Only the first NUM_SKILLS ids are real; the rest is wire padding. Exposed via
    // GET /v1/observe/skills and raised by the trainer API (eqoxide#99).
    let mut sk = vec![0u32; crate::skills::NUM_SKILLS];
    for (i, slot) in sk.iter_mut().enumerate() {
        let so = 4616 + i * 4;
        if so + 4 <= payload.len() {
            *slot = u32_at(so);
        }
    }
    gs.player_skills = sk;
}

pub fn apply_begin_cast(gs: &mut GameState, p: &[u8]) {
    if let Some((caster_id, spell_id, cast_ms)) = parse_begin_cast(p) {
        // Only the PLAYER's own cast drives the local cast bar. Any OP_BeginCast (a nearby NPC or
        // other player casting) would otherwise set gs.casting — which the UI reads to DISABLE the
        // player's spellcasting (spellbook.rs/spellgems.rs gate on casting.is_none()) and never
        // clears for a non-self cast (only OP_InterruptCast / self memorize resets it). (eqoxide#222)
        if caster_id as u32 != gs.player_id { return; }
        gs.casting = Some(CastState {
            spell_id,
            started: std::time::Instant::now(),
            cast_ms,
        });
    }
}

pub fn apply_mana_change(gs: &mut GameState, p: &[u8]) {
    // OP_ManaChange carries the player's new *current* mana (ManaChange_Struct.new_mana @0); no max.
    // Apply it so the HUD/API mana bar tracks spending/regen. set_mana keeps max as a high-water-mark
    // (the profile seed sets the true max for a rested caster at zone-in). (eqoxide#27)
    if let Some(new_mana) = parse_mana_change(p) {
        gs.set_mana(new_mana as i32);
    }
}

pub fn apply_memorize_spell(gs: &mut GameState, p: &[u8]) {
    if let Some((slot, spell_id, scribing)) = parse_memorize_spell(p) {
        match scribing {
            1 => { if (slot as usize) < 9 { gs.mem_spells[slot as usize] = spell_id; } }
            2 => { if (slot as usize) < 9 { gs.mem_spells[slot as usize] = 0xFFFF_FFFF; } }
            3 => { gs.casting = None; } // spellbar re-enable: cast finished
            _ => {}
        }
    }
}

pub fn apply_interrupt_cast(gs: &mut GameState, p: &[u8]) {
    if gs.casting.is_some() && parse_interrupt_cast(p).is_some() {
        gs.casting = None;
        gs.log_msg("combat", "Your spell is interrupted.");
    }
}

/// RoF2 Death_Struct wire layout (eq_packet_structs.h — no ENCODE in rof2.cpp so server sends raw):
///   /*000*/ uint32 spawn_id     — dying entity's spawn id
///   /*004*/ uint32 killer_id
///   /*008*/ uint32 corpseid
///   /*012*/ uint32 bindzoneid
///   /*016*/ uint32 spell_id
///   /*020*/ uint32 attack_skill
///   /*024*/ uint32 damage
///   /*028*/ uint32 unknown028
/// (Note: rof2_structs.h swaps attack_skill/bindzoneid at offsets 12/20 vs eq_packet_structs.h,
/// but since OP_Death has no encode, the wire always uses the eq_packet_structs.h ordering.)
fn apply_death(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < SIZE_DEATH { return; }
    let d = unsafe { safe_read::<Death_S>(payload) };
    let d_id = d.spawn_id;
    let killer_id = d.killer_id; // copy out of the packed struct
    if d_id == gs.player_id {
        // The server sometimes delivers OP_Death for our own spawn twice in quick
        // succession; ignore the duplicate so we don't double-log the slain message or
        // restart the respawn safety-net timer. player_dead_since is cleared once we're
        // alive again (HP restored), so a genuine later death is still processed. (eqoxide#50)
        if gs.player_dead_since.is_some() {
            return;
        }
        gs.player_dead_since = Some(std::time::Instant::now());
        // Capture who killed us + when, and keep it past the respawn so /observe/debug can report a
        // recent death (dead / killed_by / died_ago_secs) — an agent polling state can otherwise not
        // tell it died (#284).
        gs.killed_by = gs.entities.get(&killer_id).map(|e| e.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "something".to_string());
        gs.died_at   = Some(std::time::Instant::now());
        gs.hp_pct    = 0.0;
        gs.player_dead = true; // nav walker checks this and clears any stale /goto (eqoxide#61)
        // Zero cur_hp too: the self-render path derives the dead pose from
        // `cur_hp <= 0 && max_hp > 0` (app.rs), and the death packet — not an
        // OP_HPUpdate — is the authoritative "you died" signal. Without this the
        // player's own model keeps standing. Respawn reseeds cur_hp from the fresh
        // PlayerProfile, so the avatar stands back up automatically. (eqoxide#44)
        gs.cur_hp    = 0;
        gs.strategy  = "Dead — POST /v1/lifecycle/respawn to revive".into();
        let killer = gs.killed_by.clone();
        tracing::info!("EQ: combat: *** You have been slain by {killer}! ***");
        gs.log_msg("combat", &format!("*** You have been slain by {killer}! *** (POST /v1/lifecycle/respawn to revive at bind)"));
        // Async combat event so an agent learns of its own death immediately (GET /v1/events/combat).
        gs.push_event("combat", "slain", &killer, true,
            &format!("You have been slain by {killer} — POST /v1/lifecycle/respawn to revive"));
    } else {
        let name = gs.entities.get(&d_id).map(|e| e.name.clone());
        if let Some(name) = name {
            if let Some(e) = gs.entities.get_mut(&d_id) {
                e.dead      = true;
                e.hp_pct    = 0.0;
                e.animation = 115; // Animation::Lying — triggers "dead" clip in scene renderer
            }
            tracing::info!("EQ: combat: {} has been slain", name);
            gs.log_msg("combat", &format!("{} has been slain", name));
            // Auto-loot our OWN kills: the NPC corpse reuses the mob's spawn_id, and the server
            // doesn't send OP_BecomeCorpse for these deaths — so queue the corpse here. The
            // gameplay loop sends OP_LootRequest after a short delay (loot-empty corpses are a no-op).
            if killer_id == gs.player_id {
                gs.pending_loot.push_back(d_id);
                if gs.loot_queued_at.is_none() {
                    gs.loot_queued_at = Some(std::time::Instant::now());
                }
                tracing::info!("EQ: auto-loot: queued corpse_id={} ({})", d_id, name);
            }
        }
    }
}

fn apply_exp_update(gs: &mut GameState, payload: &[u8]) {
    if payload.len() >= SIZE_EXP_UPDATE {
        let eu = unsafe { safe_read::<ExpUpdate_S>(payload) };
        gs.set_xp(eu.exp);
        gs.log_msg("exp", "Experience gained");
    }
}

// RoF2 CombatDamage_Struct (rof2_structs.h): target(u16)@0 source(u16)@2 type(u8)@4
// spellid(u32)@5 damage(int32)@9 force(f32)@13 ... (RoF2 widened spellid to u32, so damage is
// at offset 9, not Titanium's 7 — reading it at 7 gave damage<<16, i.e. every value ×65536).
fn apply_combat_damage(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 13 { return; }
    let target_id = u16::from_le_bytes([payload[0], payload[1]]) as u32;
    let source_id = u16::from_le_bytes([payload[2], payload[3]]) as u32;
    // spellid (u32)@5 — non-zero for a SPELL action (heal/buff/nuke/DoT); 0 for a melee swing.
    let spellid   = u32::from_le_bytes([payload[5], payload[6], payload[7], payload[8]]);
    let damage    = i32::from_le_bytes([payload[9], payload[10], payload[11], payload[12]]);
    let target_name = gs.entities.get(&target_id).map(|e| e.name.clone())
        .unwrap_or_else(|| if target_id == gs.player_id { gs.player_name.clone() } else { format!("#{target_id}") });
    let source_name = gs.entities.get(&source_id).map(|e| e.name.clone())
        .unwrap_or_else(|| if source_id == gs.player_id { gs.player_name.clone() } else { format!("#{source_id}") });
    // A `CombatDamage_Struct.damage` of 0 is a plain miss; a POSITIVE value is real damage; a
    // NEGATIVE value is an EQEmu special-outcome sentinel (zone/common.h DMG_*), NOT "negative
    // damage" (#262). Map each to native combat wording instead of leaking "-N damage" / "(type=N)".
    let msg = if spellid != 0 {
        // A SPELL landed via OP_Damage — NOT a melee swing. A heal on a full-HP target arrives with
        // damage==0, which previously fell into the "tries to hit … misses" branch (#272). Resolve the
        // spell name and word it as a spell: beneficial (heal/buff) → "lands on", damaging → "hits for".
        let db = crate::spells::global();
        let sname = db.and_then(|d| d.get(spellid)).map(|s| s.name.clone());
        let beneficial = db.is_some_and(|d| d.is_beneficial(spellid));
        match sname {
            Some(n) if beneficial            => format!("{source_name}'s {n} lands on {target_name}"),
            Some(n) if damage > 0            => format!("{source_name}'s {n} hits {target_name} for {damage} damage"),
            Some(n)                          => format!("{source_name}'s {n} lands on {target_name}"),
            None    if damage > 0            => format!("{source_name}'s spell hits {target_name} for {damage} damage"),
            None                             => format!("{source_name} casts a spell on {target_name}"),
        }
    } else if damage > 0 {
        format!("{source_name} hits {target_name} for {damage} damage")
    } else if damage == 0 {
        format!("{source_name} tries to hit {target_name}, but misses!")
    } else {
        match damage {
            -1 => format!("{target_name} blocks {source_name}'s attack!"),   // DMG_BLOCKED
            -2 => format!("{target_name} parries {source_name}'s attack!"),  // DMG_PARRIED
            -3 => format!("{target_name} ripostes {source_name}'s attack!"), // DMG_RIPOSTED
            -4 => format!("{target_name} dodges {source_name}'s attack!"),   // DMG_DODGED
            -5 => format!("{source_name} tries to hit {target_name}, but {target_name} is invulnerable!"), // DMG_INVULNERABLE
            -6 => format!("{source_name}'s attack is absorbed by a rune!"),  // DMG_RUNE
            _  => format!("{source_name} tries to hit {target_name}, but the attack fails!"),
        }
    };
    tracing::info!("EQ: combat: {msg}");
    gs.log_msg("combat", &msg);

    // Optimistic local HP (eqoxide#55): apply damage the player TOOK immediately so the HUD/API
    // react per-hit instead of pinning at the last server value until the next OP_HPUpdate (which
    // then reconciles the authoritative HP). `damage`@9 is the same reliable field shown above; only
    // real hits (>0) reduce HP, clamped at 0. Guarded on a known max so the percent stays sane.
    // A BENEFICIAL spell (heal/buff) whose `damage` field carries the heal amount must NOT be
    // subtracted from HP — that would drain the player on every heal (#272); the OP_HPUpdate carries
    // the true post-heal HP.
    let beneficial_spell = spellid != 0 && crate::spells::global().is_some_and(|d| d.is_beneficial(spellid));
    if target_id == gs.player_id && damage > 0 && gs.max_hp > 0 && !beneficial_spell {
        gs.cur_hp = (gs.cur_hp - damage).max(0);
        gs.hp_pct = (gs.cur_hp as f32 / gs.max_hp.max(1) as f32) * 100.0;
    }

    // Remember who is swinging at us (hit OR miss) so auto-combat can engage an add that aggros
    // mid-fight instead of tanking it unanswered. Only NPC attackers on the player count.
    if target_id == gs.player_id && source_id != gs.player_id && gs.entities.contains_key(&source_id) {
        // Emit an async "attacked" event only when a NEW mob starts hitting us (not already a recent
        // attacker), so an agent is notified once when something aggros — not on every swing.
        if !gs.recent_attackers.contains_key(&source_id) {
            gs.push_event("combat", "attacked", &source_name, true,
                &format!("{source_name} is attacking you"));
        }
        gs.recent_attackers.insert(source_id, std::time::Instant::now());
    }
}

fn apply_level_update(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < SIZE_LEVEL_UPDATE { return; }
    let lu    = unsafe { safe_read::<LevelUpdate_S>(payload) };
    let level = lu.level;
    gs.player_level = level;
    gs.log_msg("exp", &format!("*** Level {}! ***", level));
}

/// OP_SetChatServer — the UCS (chat server) address + mail key, sent at zone-in. Capture it so the
/// UCS link can connect for cross-zone tells/OOC. (Connection/login is built on top of this.)
fn apply_set_chat_server(gs: &mut GameState, payload: &[u8]) {
    match crate::eq_net::ucs::parse_set_chat_server(payload) {
        Some(info) => {
            tracing::info!("UCS: chat server {}:{} mailbox='{}' type='{}'",
                info.host, info.port, info.mailbox, info.conn_type);
            gs.ucs = Some(info);
        }
        None => tracing::warn!("UCS: could not parse OP_SetChatServer payload ({} bytes)", payload.len()),
    }
}

/// Read a NUL-terminated string from the front of `buf`, returning the string (without the
/// terminator) and the slice following it. Returns `None` if there is no NUL byte.
fn read_cstr(buf: &[u8]) -> Option<(String, &[u8])> {
    let nul = buf.iter().position(|&b| b == 0)?;
    let s = String::from_utf8_lossy(&buf[..nul]).to_string();
    Some((s, &buf[nul + 1..]))
}

/// Read a little-endian u32 from the front of `buf`, returning it and the slice following it.
/// `None` if fewer than 4 bytes remain. Companion to [`read_cstr`] for cursor-style wire parsing.
fn take_u32(buf: &[u8]) -> Option<(u32, &[u8])> {
    if buf.len() < 4 { return None; }
    Some((u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]), &buf[4..]))
}

/// Parse an `OP_WhoAllResponse` (RoF2 wire) roster into `gs.who_roster` (#300). Layout
/// (`common/patches/rof2.cpp` ENCODE(OP_WhoAllResponse)):
///   64-byte `WhoAllReturnStruct` header — the online COUNT is a u32 at offset 44 (RoF2 moves it
///   into `unknown44[0]`) — then `count` player records, each WIDENED by one always-zero u32 after
///   `FormatMSGID`:
///     FormatMSGID u32 | pad0 u32 | PIDMSGID u32 | Name cstr | RankMSGID u32 | Guild cstr
///     | Unknown80[2] u32×2 | ZoneMSGID u32 | Zone u32 | Class u32 | Level u32 | Race u32
///     | Account cstr | Unknown100 u32
/// Anonymous players carry `ZoneMSGID == 0xFFFFFFFF` and zeroed class/level/race/zone.
fn apply_who_all(gs: &mut GameState, p: &[u8]) {
    if p.len() < 64 {
        tracing::warn!("who: OP_WhoAllResponse too short ({} bytes)", p.len());
        return;
    }
    let count = u32::from_le_bytes([p[44], p[45], p[46], p[47]]) as usize;
    let mut rest = &p[64..];
    let mut roster: Vec<crate::game_state::WhoEntry> = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        // Parse one record; on truncation, keep what we already have rather than dropping all.
        let parsed = (|| {
            let (_fmt, r) = take_u32(rest)?;
            let (_pad, r) = take_u32(r)?;
            let (_pid, r) = take_u32(r)?;
            let (name, r) = read_cstr(r)?;
            let (_rank, r) = take_u32(r)?;
            let (guild, r) = read_cstr(r)?;
            let (_u80a, r) = take_u32(r)?;
            let (_u80b, r) = take_u32(r)?;
            let (zonestr, r) = take_u32(r)?;
            let (zone, r) = take_u32(r)?;
            let (class, r) = take_u32(r)?;
            let (level, r) = take_u32(r)?;
            let (race, r) = take_u32(r)?;
            let (_acct, r) = read_cstr(r)?;
            let (_u100, r) = take_u32(r)?;
            let anon = zonestr == 0xFFFF_FFFF || (class == 0 && level == 0 && race == 0);
            Some((crate::game_state::WhoEntry { name, level, class, race, zone_id: zone, guild, anon }, r))
        })();
        match parsed {
            Some((entry, r)) => { roster.push(entry); rest = r; }
            None => break,
        }
    }
    tracing::info!("who: parsed {}/{} online player(s) from OP_WhoAllResponse", roster.len(), count);
    gs.who_roster = roster;
}

fn apply_channel_message(gs: &mut GameState, payload: &[u8]) {
    // RoF2 OP_ChannelMessage is a variable-length, NUL-terminated wire format — NOT the
    // fixed Titanium struct. See EQEmu common/patches/rof2.cpp ENCODE(OP_ChannelMessage):
    //   sender\0 | target\0 | u32 unknown | u32 language | u32 chan_num
    //   | u32 unknown | u8 unknown | u32 skill_in_language | message\0 | (trailing unknowns)
    let (sender, rest) = match read_cstr(payload) { Some(v) => v, None => return };
    let (targetname, rest) = match read_cstr(rest) { Some(v) => v, None => return };
    // After the two strings: 4 (unk) + 4 (lang) + 4 (chan) + 4 (unk) + 1 (unk) + 4 (skill) = 21
    // bytes, then the NUL-terminated message.
    if rest.len() < 21 { return; }
    let chan_num = u32::from_le_bytes([rest[8], rest[9], rest[10], rest[11]]);
    let msg = String::from_utf8_lossy(&rest[21..])
        .split('\0').next().unwrap_or("")
        .to_string();
    if msg.is_empty() { return; }
    // NPC dialogue may embed saylink hyperlinks; show the readable label and capture any clickable
    // choices. Only the Say channel (8) is NPC conversation — a saylink arriving on a player chat
    // channel (tell/OOC/etc.) is not a dialogue prompt, so choices are only adopted for `say`.
    let (msg, choices) = parse_say_links(&msg);
    if chan_num == 8 && !choices.is_empty() { gs.dialogue_choices = choices; }

    // EQEmu ChatChannel: 0 guild, 2 group, 3 shout, 4 auction, 5 OOC, 6 broadcast, 7 tell,
    // 8 say, 11 gmsay. The inter-agent channels are ALSO recorded as structured chat events for
    // the GET /events feed; `say` (8) is NPC dialogue / local say and stays in the message log only.
    let event_channel = match chan_num {
        7  => Some("tell"),
        5  => Some("ooc"),
        3  => Some("shout"),
        2  => Some("group"),
        0  => Some("guild"),
        11 => Some("gmsay"),
        _  => None,
    };
    if let Some(ch) = event_channel {
        if !sender.is_empty() {
            // `directed` = addressed specifically to us: a /tell to our name, or any GM message.
            let directed = (ch == "tell" && targetname.eq_ignore_ascii_case(&gs.player_name))
                        || ch == "gmsay";
            gs.push_event("chat", ch, &sender, directed, &msg);
        }
    }

    if !sender.is_empty() {
        // Log under the channel kind (tell/ooc/shout/group) so the chat window
        // can color and tab-filter by channel; plain say stays "chat" (#162).
        gs.log_msg(event_channel.unwrap_or("chat"), &format!("<{}> {}", sender, msg));
    } else {
        // Zone-wide broadcasts without a sender (server messages like "An earthquake strikes!").
        gs.log_msg("zone", &msg);
    }
}

/// OP_GuildsList — the server-wide guild directory (guild_id → name). Little-endian, variable
/// length: u8[64] header, u32 count, then `count` × (u32 guild_id, NUL-terminated name). Used to
/// resolve the player's own guild_id (and any member's) to a display name. Rebuilt on every receipt
/// (the server re-sends it whenever any guild is created/renamed/deleted). (#295)
fn apply_guild_list(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 68 { return; }
    let count = u32::from_le_bytes([payload[64], payload[65], payload[66], payload[67]]) as usize;
    let mut rest = &payload[68..];
    let mut names = std::collections::HashMap::with_capacity(count);
    for _ in 0..count {
        if rest.len() < 4 { break; }
        let id = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
        let (name, r) = match read_cstr(&rest[4..]) { Some(v) => v, None => break };
        rest = r;
        if !name.is_empty() { names.insert(id, name); }
    }
    tracing::info!("EQ: guild directory: {} guilds", names.len());
    gs.guild_names = names;
}

/// OP_GuildMemberList — the full guild roster snapshot. NOTE: this is the one guild packet sent in
/// NETWORK byte order (BIG-ENDIAN). Layout: cstr prefix_name, u32 guild_id (uninitialized — ignore),
/// u32 member_count, then per member: cstr name, u32 level, u32 banker, u32 class, u32 rank(0-8),
/// u32 time_last_on, u32 tribute_enable, u32 unk, u32 total_tribute, u32 last_tribute, u32 unk,
/// cstr public_note, u16 zoneinstance, u16 zone_id, u32 unk, u32 unk. Online = zone_id != 0. Full
/// replace (the server re-sends the whole list on membership changes). (#295)
fn apply_guild_member_list(gs: &mut GameState, payload: &[u8]) {
    // All integers here are big-endian.
    let rd_u32 = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let rd_u16 = |b: &[u8]| u16::from_be_bytes([b[0], b[1]]);
    let (_prefix, rest) = match read_cstr(payload) { Some(v) => v, None => return };
    if rest.len() < 8 { return; }
    let count = rd_u32(&rest[4..]) as usize; // skip the uninitialized guild_id u32 at rest[0..4]
    let mut cur = &rest[8..];
    let mut members = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let (name, r) = match read_cstr(cur) { Some(v) => v, None => break };
        cur = r;
        if cur.len() < 40 { break; } // 10 × u32 before the public_note cstr
        let level = rd_u32(&cur[0..]);
        let class = rd_u32(&cur[8..]);   // cur[4..] = banker flags (skipped)
        let rank  = rd_u32(&cur[12..]);
        // consume the 10 fixed u32s: level, banker, class, rank, time_last_on, tribute_enable,
        // unknown, total_tribute, last_tribute, unknown_one.
        cur = &cur[40..];
        let (public_note, r) = match read_cstr(cur) { Some(v) => v, None => break };
        cur = r;
        if cur.len() < 12 { break; } // u16 zoneinstance + u16 zone_id + u32 + u32
        let zone_id = rd_u16(&cur[2..]) as u32; // cur[0..2] = zoneinstance (skipped)
        cur = &cur[12..];
        members.push(crate::game_state::GuildMember {
            name, rank, level, class, zone_id, online: zone_id != 0, public_note,
        });
    }
    tracing::info!("EQ: guild roster: {} members", members.len());
    gs.guild_members = members;
}

/// OP_GuildMemberUpdate — a live presence ping for one member (little-endian, 80-byte fixed struct):
/// GuildID(u32) MemberName[64] ZoneID(u16) InstanceID(u16) LastSeen(u32) Unknown(u32). ZoneID==0
/// means the member just went offline; nonzero means online in that zone. Patches the matching
/// roster entry in place. (#295)
fn apply_guild_member_update(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 72 { return; }
    let name = {
        let nb = &payload[4..68];
        let end = nb.iter().position(|&b| b == 0).unwrap_or(nb.len());
        String::from_utf8_lossy(&nb[..end]).to_string()
    };
    let zone_id = u16::from_le_bytes([payload[68], payload[69]]) as u32;
    if let Some(m) = gs.guild_members.iter_mut().find(|m| m.name == name) {
        m.zone_id = zone_id;
        m.online = zone_id != 0;
        tracing::info!("EQ: guild presence: {} {}", name, if zone_id != 0 { "online" } else { "offline" });
    }
}

/// OP_GuildInvite (inbound) — the server forwards a guild invite to us verbatim as a
/// GuildCommand_Struct (140B): othername[64]@0 (= us), myname[64]@64 (= the inviter), u16
/// guildeqid@128 (the guild), u8 unknown[2]@130, u32 officer@132 (offered rank). We capture it as a
/// pending invite so POST /v1/guild/accept can reply with OP_GuildInviteAccept. (#295)
fn apply_guild_invite(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 136 { return; }
    let inviter = {
        let nb = &payload[64..128];
        let end = nb.iter().position(|&b| b == 0).unwrap_or(nb.len());
        String::from_utf8_lossy(&nb[..end]).to_string()
    };
    let guild_id = u16::from_le_bytes([payload[128], payload[129]]) as u32;
    let rank = u32::from_le_bytes([payload[132], payload[133], payload[134], payload[135]]);
    if inviter.is_empty() { return; }
    gs.log_msg("guild", &format!("{} invites you to their guild", inviter));
    tracing::info!("EQ: guild invite from {} (guild_id={}, rank={})", inviter, guild_id, rank);
    gs.pending_guild_invite = Some((inviter, guild_id, rank));
}

/// RoF2 saylink body length (`SAY_LINK_BODY_SIZE`, EQEmu `common/patches/rof2_limits.h`).
const SAY_LINK_BODY_SIZE: usize = 56;

/// Strip EQ "saylink" framing from chat text, leaving only the human-readable label.
///
/// Thin wrapper over [`parse_say_links`] for callers that only want the readable text.
/// (eqoxide#46)
fn strip_say_links(s: &str) -> String {
    parse_say_links(s).0
}

/// Parse EQ "saylink" framing out of an NPC message, returning both the human-readable text
/// (identical to what [`strip_say_links`] produced) and the structured, clickable choices.
///
/// On the wire a saylink is `\x12` + a fixed 56-char hex body + the display text + `\x12`
/// (RoF2). Splitting on the `\x12` control byte (as EQEmu's `Strings::Split(msg, '\x12')` does)
/// yields plain text at even indices and link contents (body+text) at odd indices. For each
/// well-formed link (odd segment at least `SAY_LINK_BODY_SIZE` long) we drop the body from the
/// display text and decode the body's hex fields into a [`DialogueChoice`] so the link can later
/// be re-clicked via `OP_ItemLinkClick`. Only real saylinks (body `item_id == SAYLINK_ITEM_ID`)
/// become choices; other item links keep their display text but are not offered as choices.
/// A malformed or short link segment is kept verbatim (minus the control byte) so we never eat
/// real text.
///
/// Body field offsets (hex chars), from EQEmu `common/say_link.cpp`
/// `DegenerateLinkBody` / RoF2 `SAY_LINK_BODY_SIZE == 56`:
///   action_id[0..1] item_id[1..6] augment_1[6..11] augment_2[11..16] augment_3[16..21]
///   augment_4[21..26] augment_5[26..31] augment_6[31..36] is_evolving[36..37]
///   evolve_group[37..41] evolve_level[41..43] ornament_icon[43..48] hash[48..56]
fn parse_say_links(s: &str) -> (String, Vec<crate::game_state::DialogueChoice>) {
    if !s.contains('\x12') {
        return (s.to_string(), Vec::new());
    }
    let mut out = String::with_capacity(s.len());
    let mut choices = Vec::new();
    for (i, seg) in s.split('\x12').enumerate() {
        if i & 1 == 1 && seg.len() >= SAY_LINK_BODY_SIZE {
            // Link content: drop the fixed-length body, keep the trailing display text.
            // Body is ASCII (hex digits), so byte offset 56 is a valid UTF-8 boundary.
            let (body, display) = seg.split_at(SAY_LINK_BODY_SIZE);
            out.push_str(display);
            let hx = |a: usize, b: usize| u32::from_str_radix(&body[a..b], 16).unwrap_or(0);
            if hx(1, 6) == SAYLINK_ITEM_ID {
                choices.push(crate::game_state::DialogueChoice {
                    text:      display.to_string(),
                    item_id:   SAYLINK_ITEM_ID,
                    augments:  [hx(6, 11), hx(11, 16), hx(16, 21), hx(21, 26), hx(26, 31), hx(31, 36)],
                    link_hash: hx(48, 56),
                    icon:      hx(43, 48),
                });
            }
        } else {
            // Plain text (even index) or a too-short/malformed link body — keep verbatim.
            out.push_str(seg);
        }
    }
    (out, choices)
}

/// Item id that marks an EQ item-link body as a "saylink" rather than a real item
/// (EQEmu `common/features.h` `SAYLINK_ITEM_ID`).
const SAYLINK_ITEM_ID: u32 = 0xF_FFFF;

/// EQEmu sends GM-flagged accounts verbose server-side debug messages that are not
/// player-facing. These flood the NPC Dialogue panel and should be silently dropped.
/// Examples: "[Loot] [AddLootDrop] ...", "[CombatRecord] [Stop] [Summary] ...",
/// "[EVENT_KILLED_MERIT] ..." verbose combat/quest analytics.
fn is_debug_spam(msg: &str) -> bool {
    // Loot table debug
    msg.contains("AddLootDrop") || msg.contains("min/max") || msg.contains("[Loot]")
    // Combat record analytics sent to GM accounts after each fight
    || msg.contains("[CombatRecord]")
    // Kill/event merit debug records
    || msg.contains("[EVENT_KILLED_MERIT]") || msg.contains("[EVENT_ITEM_GIVEN]")
}

/// OP_FormattedMessage — eqstr-table text with arguments. Layout: unknown0(u32) +
/// string_id(u32) + type(u32) + args (null-separated strings). Resolved via the eqstr
/// table loaded at startup; if the table or id is missing, the raw args are shown.
fn apply_formatted_message(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 12 { return; }
    let string_id = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let args: Vec<String> = payload[12..]
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).to_string())
        .collect();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let text = crate::eqstr::format_id(string_id, &arg_refs)
        .unwrap_or_else(|| arg_refs.join(" "));
    // Formatted quest/server text can embed item saylinks in its arguments (e.g. "You need
    // [<56-hex body>rat whiskers]."). Strip the fixed hex link body so only the readable name
    // shows, matching the says/emote paths (#256). apply_channel_message/apply_special_message
    // already do this; this was the remaining un-stripped message render path.
    let text = strip_say_links(&text);
    if !text.trim().is_empty() && !is_debug_spam(&text) {
        gs.log_msg("system", &text);
    }
}

/// OP_Emote — world/NPC emote text (some quest flavor, often with [keywords]).
/// Emote_Struct: type(u32) + message[1024]. type 0xffffffff = animation command (no
/// useful text); only non-empty custom text is shown, in the NPC dialogue panel.
fn apply_emote(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 5 { return; }
    let etype = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    if etype == 0xffff_ffff { return; } // /dance, /flip, etc. — animation only
    let msg = strip_say_links(
        String::from_utf8_lossy(&payload[4..])
            .trim_end_matches('\0')
            .trim(),
    );
    if !msg.is_empty() && !is_debug_spam(&msg) {
        gs.log_msg("npc", &msg);
    }
}

/// OP_SimpleMessage — eqstr-table text, no arguments. Layout: string_id(u32) + color(u32)
/// + unknown(u32).
fn apply_simple_message(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 8 { return; }
    let string_id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    if let Some(text) = crate::eqstr::format_id(string_id, &[]) {
        if !text.trim().is_empty() && !is_debug_spam(&text) {
            gs.log_msg("system", &text);
        }
    }
}

/// Map a faction-con value (EQEmu FACTION_*, 1..=9) to the line EQ shows on /consider.
pub fn consider_message(faction: u32) -> &'static str {
    match faction {
        1 => "regards you as an ally",
        2 => "looks upon you warmly",
        3 => "kindly regards you",
        4 => "regards you amiably",
        5 => "regards you indifferently",
        6 => "looks your way apprehensively",
        7 => "looks at you dubiously",
        8 => "glares at you threateningly",
        9 => "scowls at you, ready to attack",
        _ => "regards you",
    }
}

/// Map an EQEmu ConsiderColor (the OP_Consider reply's `level` field) to a nameplate RGB.
/// Titanium remaps Gray→Green and White→WhiteTitanium server-side, but we cover all.
pub fn con_color(level: u32) -> [u8; 3] {
    match level {
        2  => [ 90, 220,  90], // Green   — trivial
        4  => [ 80, 120, 240], // DarkBlue
        6  => [150, 150, 150], // Gray    — no exp
        18 => [120, 200, 240], // LightBlue
        10 | 20 => [235, 235, 235], // White / WhiteTitanium — even con
        15 => [240, 230,  80], // Yellow  — slightly higher
        13 => [240,  80,  80], // Red     — much higher / dangerous
        _  => [235, 235, 235],
    }
}

/// Map the OP_Consider `level` field (EQEmu ConsiderColor) to a readable difficulty TIER for the API
/// (#292), parallel to the RGB in [`con_color`]: gray = trivial (no exp), green, light_blue, blue,
/// white = even con, yellow = slightly higher, red = much higher / dangerous.
pub fn con_level_name(level: u32) -> &'static str {
    match level {
        6       => "gray",        // no exp — trivial
        2       => "green",
        18      => "light_blue",
        4       => "blue",        // DarkBlue
        15      => "yellow",
        13      => "red",         // dangerous
        10 | 20 => "white",       // even con (White / WhiteTitanium)
        _       => "white",
    }
}

/// Map the OP_Consider faction value (1..=9) to a compact attitude enum for the API (#292),
/// so agents don't string-match the localized [`consider_message`] prose. ally … scowls (KOS).
pub fn attitude_name(faction: u32) -> &'static str {
    match faction {
        1 => "ally",
        2 => "warmly",
        3 => "kindly",
        4 => "amiable",
        5 => "indifferent",
        6 => "apprehensive",
        7 => "dubious",
        8 => "threatening",
        9 => "scowls",           // ready to attack (KOS)
        _ => "indifferent",
    }
}

/// OP_Consider reply — the server's con of our target. Consider_Struct: playerid(u32) +
/// targetid(u32) + faction(u32) + level(u32 = con color) + cur_hp + ... Sets the target
/// (so its nameplate highlights) plus the con-color tint and a /consider log line.
fn apply_consider(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 16 { return; }
    let target_id = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let faction   = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
    let level     = u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
    let name = gs.entities.get(&target_id).map(|e| e.name.clone())
        .unwrap_or_else(|| "Your target".to_string());
    gs.target_id  = Some(target_id);
    gs.target_con = Some(con_color(level));
    // #292: also record the structured difficulty tier + attitude enum so agents can read "how
    // tough" from /observe/debug instead of scraping the localized chat line or the RGB tint.
    gs.target_con_name = Some(con_level_name(level).to_string());
    gs.target_attitude = Some(attitude_name(faction).to_string());
    let msg = format!("{} {}.", name, consider_message(faction));
    tracing::info!("EQ: consider: {msg}");
    gs.log_msg("combat", &msg);
}

/// OP_SpawnAppearance render-side handler: `{ id: u16, kind: u16, param: u32 }`.
///
/// We only consume the ANIMATION appearance (kind 14) for OUR OWN player, mapping param 110→sitting,
/// 100→standing. A client-initiated sit/stand is issued on the nav thread, which sets the *nav*
/// GameState's `sitting` and mirrors the same appearance packet here through `app_tx` (like the
/// target/money bridges) — without this handler the render GameState's `sitting` never flips, so the
/// player's own sit animation never plays (#53, the two-GameState split). Server broadcasts of the
/// same opcode also land here. Other kinds / other spawns are ignored (their pose comes from spawn
/// and scene state).
fn apply_spawn_appearance(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 8 { return; }
    let id    = u16::from_le_bytes([payload[0], payload[1]]) as u32;
    let kind  = u16::from_le_bytes([payload[2], payload[3]]);
    let param = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    const ANIMATION: u16 = 14;
    const SITTING:   u32 = 110;
    if kind == ANIMATION && id == gs.player_id {
        gs.sitting = param == SITTING;
    }
    // Guild membership reflect (#295): the server pushes our own guild id/rank as SpawnAppearance
    // kinds when membership changes (e.g. a GM `#guild add/remove`), with no client action. Applying
    // them here keeps /observe/debug's guild identity live without any guild-specific opcode. Only
    // our own spawn carries our membership. (GuildID=22, GuildRank=23, GuildShow=52.)
    if id == gs.player_id {
        const GUILD_ID: u16 = 22;
        const GUILD_RANK: u16 = 23;
        match kind {
            GUILD_ID   => { gs.player_guild_id = param;
                            tracing::info!("EQ: guild membership changed → guild_id={}", param); }
            GUILD_RANK => { gs.player_guild_rank = param; }
            _ => {}
        }
    }
}

/// OP_SpecialMesg — NPC dialogue / emotes, where quest text arrives.
/// SpecialMesg_Struct: header[3] + msg_type(u32) + target_spawn_id(u32) +
/// sayer(null-terminated, variable) + unknown[12] + message(null-terminated).
/// Logged with kind "npc" so the quest dialogue panel can pick it out.
fn apply_special_message(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 12 { return; }
    // sayer is a null-terminated string starting at offset 11.
    let sayer_start = 11;
    let rel_end = payload[sayer_start..].iter().position(|&b| b == 0);
    let Some(rel_end) = rel_end else { return; };
    let sayer = String::from_utf8_lossy(&payload[sayer_start..sayer_start + rel_end]).to_string();
    // message follows sayer's null + 12 unknown bytes.
    let msg_start = sayer_start + rel_end + 1 + 12;
    if msg_start >= payload.len() { return; }
    let msg = String::from_utf8_lossy(&payload[msg_start..])
        .trim_end_matches('\0')
        .to_string();
    let (msg, choices) = parse_say_links(&msg);
    if msg.trim().is_empty() || is_debug_spam(&msg) { return; }
    // A new NPC line carrying clickable saylinks (e.g. a Soulbinder's "[bind your soul]") replaces
    // the current dialogue choices (#120).
    if !choices.is_empty() { gs.dialogue_choices = choices; }
    if sayer.is_empty() {
        gs.log_msg("npc", &msg);
    } else {
        gs.log_msg("npc", &format!("{} says, '{}'", sayer, msg));
    }
}

/// EQEmu marks a zone line with no fixed destination/trigger coordinate using the sentinel
/// `999999`. Such an entry is not a real navigable point — treat any coord at/near it as garbage.
fn is_sentinel_zone_point(x: f32, y: f32, z: f32) -> bool {
    const SENTINEL: f32 = 900_000.0;
    x.abs() >= SENTINEL || y.abs() >= SENTINEL || z.abs() >= SENTINEL
}

fn apply_zone_points(gs: &mut GameState, payload: &[u8]) {
    // Wire format: optional 4-byte header + N × ZonePointEntry_S (24 bytes each).
    // Detect header: if (len-4) % 24 == 0 and len >= 4, skip header.
    let offset = if payload.len() >= 4 && (payload.len() - 4) % SIZE_ZONE_POINT_ENTRY == 0 {
        4
    } else {
        0
    };
    gs.zone_points.clear();
    let mut i = offset;
    while i + SIZE_ZONE_POINT_ENTRY <= payload.len() {
        let e = unsafe { safe_read::<ZonePointEntry_S>(&payload[i..]) };
        i += SIZE_ZONE_POINT_ENTRY;
        // Copy out of the packed struct before use (unaligned field refs are UB).
        let (ex, ey, ez, heading, iterator, zoneid) = (e.x, e.y, e.z, e.heading, e.iterator, e.zoneid);
        // Drop sentinel/garbage zone points: EQEmu uses 999999 as a "no coordinate" marker for some
        // zone lines. Feeding it into the auto-zone-cross proximity math and destination selection
        // corrupts them (a point ~1e6 units away), so ignore any entry with a sentinel coordinate
        // (#136).
        if is_sentinel_zone_point(ex, ey, ez) {
            tracing::debug!("EQ: ignoring sentinel zone point zone_id={} x={:.0} y={:.0}", zoneid, ex, ey);
            continue;
        }
        gs.zone_points.push(ZonePoint {
            iterator,
            server_x: ex,
            server_y: ey,
            server_z: ez,
            heading,
            zone_id: zoneid,
        });
    }
    tracing::info!("EQ: {} zone exit points received:", gs.zone_points.len());
    for zp in &gs.zone_points {
        tracing::info!("  zone_id={} server_x={:.1} server_y={:.1} z={:.1} heading={:.1}",
                  zp.zone_id, zp.server_x, zp.server_y, zp.server_z, zp.heading);
    }
}

/// OP_SpawnDoor — a header-less flat array of Door_Struct records (max 500).
/// RoF2 records are 100 bytes: the server's internal 80-byte Door_Struct is ENCODE-expanded
/// to the 100-byte RoF2 client struct (EQEmu `ENCODE(OP_SpawnDoor)`), which appends 20 bytes
/// of RoF2-only trailing fields after `door_param`. The fields we read all sit in the first
/// 80 bytes (identical in both structs); only the per-record STRIDE differs, so an 80-byte
/// stride drifts every record after the first and decodes garbage/empty names.
/// Wire order is y(north) then x(east); we store client convention (x=east, y=north).
fn apply_spawn_doors(gs: &mut GameState, p: &[u8]) {
    const REC: usize = 100;
    let rd_f32 = |b: &[u8], o: usize| f32::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3]]);
    let rd_u32 = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3]]);
    let mut off = 0;
    while off + REC <= p.len() {
        let r = &p[off..off + REC];
        let name_end = r[..32].iter().position(|&c| c == 0).unwrap_or(32);
        let name = String::from_utf8_lossy(&r[..name_end]).into_owned();
        let door = crate::game_state::Door {
            door_id: r[60],
            name,
            y: rd_f32(r, 32),   // north (yPos)
            x: rd_f32(r, 36),   // east  (xPos)
            z: rd_f32(r, 40),
            heading: rd_f32(r, 44),
            incline: rd_u32(r, 48) as i32,
            size: u16::from_le_bytes([r[52], r[53]]),
            opentype: r[61],
            invert_state: r[63] != 0,
            is_open: r[62] != 0,   // state_at_spawn is already invert-adjusted
            door_param: rd_u32(r, 64),
        };
        gs.upsert_door(door);
        off += REC;
    }
}

/// OP_MoveDoor — MoveDoor_Struct {door_id u8, action u8}. For a normal door 0x02=open,
/// 0x03=close; for an inverted door the meaning flips. We store the visual open state as
/// (action == 0x02) XOR invert_state.
fn apply_move_door(gs: &mut GameState, p: &[u8]) {
    if p.len() < 2 { return; }
    let door_id = p[0];
    let action_open = p[1] == 0x02;
    let invert = gs.doors.get(&door_id).map(|d| d.invert_state).unwrap_or(false);
    gs.set_door_open(door_id, action_open ^ invert);
}

fn apply_bind_respawn(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 20 { return; }
    gs.player_x = f32::from_le_bytes([payload[4],  payload[5],  payload[6],  payload[7]]);
    gs.player_y = f32::from_le_bytes([payload[8],  payload[9],  payload[10], payload[11]]);
    gs.player_z = f32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
    // Real EQ revives a bind-respawned character at FULL HP. `apply_death` zeroed hp_pct and left
    // cur_hp/max_hp stale, so without this the HUD/API show a dead-but-full contradiction
    // (hp/hp_max full, hp_pct 0) until some later OP_HPUpdate happens to reconcile it (eqoxide#68).
    let full = gs.max_hp.max(1);
    gs.update_hp(gs.player_id, full, full); // cur=max → hp_pct=100, consistent with hp/hp_max
    gs.strategy = "Respawning...".into();
    gs.log_msg("zone", "Respawning at bind point");
}

/// Apply a WearChange: update one equipment slot's material + tint on an entity.
pub fn apply_wear_change(gs: &mut GameState, p: &[u8]) {
    use crate::eq_net::protocol::{WearChange_S, SIZE_WEAR_CHANGE, safe_read};
    if p.len() < SIZE_WEAR_CHANGE { return; }
    let wc: WearChange_S = unsafe { safe_read(p) };
    let slot = wc.wear_slot_id as usize;
    if slot >= 9 { return; }
    let spawn_id = wc.spawn_id as u32;
    let material = wc.material as u32;
    let color = wc.color; // [B, G, R, UseTint]
    let tint = [color[2], color[1], color[0]]; // store RGB
    // The local player is registered separately (not in `entities`), so a WearChange
    // for the player's own spawn_id must update the player fields, or live equip/unequip
    // (e.g. GM #gearup) never shows on the player until a re-zone re-parses the profile.
    if spawn_id == gs.player_id {
        gs.player_equipment[slot] = material;
        gs.player_equipment_tint[slot] = tint;
    } else if let Some(e) = gs.entities.get_mut(&spawn_id) {
        e.equipment[slot] = material;
        e.equipment_tint[slot] = tint;
    }
}

fn apply_money_on_corpse(gs: &mut GameState, payload: &[u8]) {
    // MoneyOnCorpse_Struct: response(u8) + 3×pad + platinum(u32) + gold(u32) + silver(u32) + copper(u32)
    if payload.len() < 20 { return; }
    let response  = payload[0];
    if response != 0 {
        tracing::warn!("EQ: OP_MoneyOnCorpse denied (response={})", response);
        return;
    }
    let platinum = u32::from_le_bytes([payload[4],  payload[5],  payload[6],  payload[7]]);
    let gold     = u32::from_le_bytes([payload[8],  payload[9],  payload[10], payload[11]]);
    let silver   = u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
    let copper   = u32::from_le_bytes([payload[16], payload[17], payload[18], payload[19]]);
    gs.loot_last_activity = Some(std::time::Instant::now());
    if platinum > 0 || gold > 0 || silver > 0 || copper > 0 {
        // Add the looted coins to the on-hand total for the HUD. Corpse loot calls the server's
        // AddMoneyToPP with update_client=false (verified in EQEmu), so it does NOT also send an
        // OP_MoneyUpdate — this is the only coin notification for loot, so we must add here.
        gs.coin[0] = gs.coin[0].saturating_add(platinum);
        gs.coin[1] = gs.coin[1].saturating_add(gold);
        gs.coin[2] = gs.coin[2].saturating_add(silver);
        gs.coin[3] = gs.coin[3].saturating_add(copper);
        let mut parts = Vec::new();
        if platinum > 0 { parts.push(format!("{}pp", platinum)); }
        if gold     > 0 { parts.push(format!("{}gp", gold)); }
        if silver   > 0 { parts.push(format!("{}sp", silver)); }
        if copper   > 0 { parts.push(format!("{}cp", copper)); }
        gs.log_msg("loot", &format!("Looted coins: {}", parts.join(", ")));
        tracing::info!("EQ: looted coins: {}", parts.join(", "));
    } else {
        tracing::info!("EQ: no coins on corpse");
    }
}

/// OP_MoneyUpdate — the server's authoritative NEW coin total after a change that it tracks
/// server-side (trade completion, quest reward, trader sell, etc.). Without handling this the HUD
/// coin stayed stuck at the login-profile value. (Loot uses OP_MoneyOnCorpse; merchant *buys* are
/// deducted client-side — see the /buy path — because the server takes the money with
/// update_client=false and sends nothing.)
fn apply_money_update(gs: &mut GameState, payload: &[u8]) {
    // MoneyUpdate_Struct (16 bytes): platinum/gold/silver/copper as int32.
    if payload.len() < 16 { return; }
    let rd = |o: usize| i32::from_le_bytes([payload[o], payload[o + 1], payload[o + 2], payload[o + 3]]).max(0) as u32;
    gs.coin = [rd(0), rd(4), rd(8), rd(12)];
    tracing::info!("EQ: money update -> {}p {}g {}s {}c", gs.coin[0], gs.coin[1], gs.coin[2], gs.coin[3]);
}

// ── Shared spawn registration ─────────────────────────────────────────────────

/// Insert or update one spawn in `gs`. If it matches the player name the
/// player fields are updated instead and the spawn is NOT added to entities.
pub fn register_spawn(gs: &mut GameState, info: SpawnInfo) {
    let is_npc = info.npc != 0;
    // A spawn can arrive ALREADY a corpse (npc: 2=pc_corpse, 3=npc_corpse) — most commonly the
    // corpses that already exist when you zone in (delivered via OP_ZoneSpawns / OP_ZoneEntry, which
    // flow straight through here). Flag it dead + Lying so the scene renderer plays the D05 death clip
    // and the corpse lies down, instead of standing in the idle pose. Doing this in `register_spawn`
    // (the ONE path every spawn takes) covers zone-in corpses too, not just fresh OP_NewSpawn ones —
    // the earlier fix only patched the single-spawn path, so zone-in corpses still stood (#253/#118).
    let is_corpse = info.npc == 2 || info.npc == 3;

    if !is_npc && !gs.player_name.is_empty() && info.name == gs.player_name {
        gs.player_id      = info.spawn_id;
        gs.player_x       = info.x;
        gs.player_y       = info.y;
        gs.player_z       = info.z;
        gs.player_heading = info.heading;
        gs.player_level   = info.level as u32;
        gs.player_race    = eq_race_to_code(info.race).to_string();
        gs.player_gender  = info.gender;
        gs.player_equipment      = info.equipment;
        gs.player_equipment_tint = info.equipment_tint;
        gs.player_face           = info.face;
        gs.player_hairstyle      = info.hairstyle;
        // Our own guild identity, from the self-spawn stream (#295). 0xFFFFFFFF/0 = no guild.
        gs.player_guild_id       = info.guild_id;
        gs.player_guild_rank     = info.guild_rank;
        tracing::info!("EQ: player spawn id={} pos=({:.1},{:.1},{:.1}) equip={:?} guild_id={}",
            info.spawn_id, info.x, info.y, info.z, gs.player_equipment, info.guild_id);
        return;
    }

    // Track the player's own pet (necro/mage/etc.) via petOwnerId.
    if gs.player_id != 0 && info.pet_owner_id == gs.player_id {
        gs.pet_id = Some(info.spawn_id);
        tracing::info!("EQ: player pet spawned id={} name='{}'", info.spawn_id, info.name);
    }

    gs.upsert_entity(Entity {
        spawn_id:       info.spawn_id,
        name:           info.name,
        level:          info.level as u32,
        is_npc,
        x: info.x, y: info.y, z: info.z,
        // curHp in RoF2 Spawn_Struct is an HP percent (0..100), same as Titanium. A corpse is dead —
        // force 0 so the HUD/con logic agrees with the dead pose.
        hp_pct:         if is_corpse { 0.0 } else { (info.cur_hp as f32).min(100.0) },
        cur_hp:         if is_corpse { 0 } else { info.cur_hp as i32 },
        max_hp:         100, // RoF2 spawn has no separate max_hp; treat as percent
        race:           eq_race_to_code(info.race).to_string(),
        floating:       crate::eq_net::protocol::is_boat_race(info.race),
        heading:        info.heading,
        dead:           is_corpse,
        equipment:      info.equipment,
        equipment_tint: info.equipment_tint,
        gender:         info.gender,
        helm:           info.helm,
        showhelm:       info.show_helm as u8,
        face:           info.face,
        hairstyle:      info.hairstyle,
        haircolor:      info.haircolor,
        // Corpses use the Lying animation (115) so the scene renderer picks the "dead"/D05 clip.
        animation:      if is_corpse { 115 } else { info.stand_state as u32 },
    });
}

#[cfg(test)]
mod tests {
    use super::{apply_emote, apply_death, apply_who_all, class_name, con_color, consider_message, parse_player_profile,
                parse_begin_cast, apply_begin_cast, parse_memorize_spell, apply_char_inventory,
                apply_money_update, apply_money_on_corpse, apply_set_target, apply_move_item, apply_spawn_appearance,
                extract_saylink_text, apply_task_description, apply_completed_tasks, apply_task_select_window,
                strip_say_links, SAY_LINK_BODY_SIZE, SIZE_DEATH,
                apply_group_update_b, apply_group_join, apply_group_disband_you,
                apply_group_disband_other, apply_group_leader_change, apply_group_invite, apply_group_acknowledge};
    use crate::game_state::{GameState, Entity, TaskStatus};

    /// Build a RoF2 saylink: 0x12 + 56-char body + display text + 0x12.
    fn saylink(body_seed: char, text: &str) -> String {
        let body: String = std::iter::repeat(body_seed).take(SAY_LINK_BODY_SIZE).collect();
        format!("\u{12}{}{}\u{12}", body, text)
    }

    #[test]
    fn strip_say_links_extracts_label() {
        let msg = format!("Are you {} to begin?", saylink('0', "[ready]"));
        assert_eq!(strip_say_links(&msg), "Are you [ready] to begin?");
    }

    #[test]
    fn strip_say_links_handles_multiple_and_plain() {
        assert_eq!(strip_say_links("no links here"), "no links here");
        let msg = format!("{} and {}", saylink('a', "first"), saylink('b', "second"));
        assert_eq!(strip_say_links(&msg), "first and second");
    }

    #[test]
    fn strip_say_links_short_body_kept_verbatim() {
        // A control byte with too-short a body (malformed) must not eat text.
        assert_eq!(strip_say_links("a\u{12}short\u{12}b"), "ashortb");
    }

    fn make_formatted(string_id: u32, args: &[&str]) -> Vec<u8> {
        // OP_FormattedMessage: unknown0(u32) + string_id(u32) + type(u32) + NUL-separated args.
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&string_id.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        for a in args { buf.extend_from_slice(a.as_bytes()); buf.push(0); }
        buf
    }

    #[test]
    fn apply_formatted_message_strips_saylink_hex_body() {
        // #256: a formatted server/quest message whose argument embeds an item saylink must show
        // only the readable name, not the 56-char hex body — the says/emote paths already do this;
        // this was the remaining un-stripped render path. string_id 0 isn't in the eqstr table, so
        // the handler falls back to joining the raw args (which carry the link).
        let mut gs = GameState::new();
        let arg = format!("You need {} for this.", saylink('0', "rat whiskers"));
        super::apply_formatted_message(&mut gs, &make_formatted(0, &[&arg]));
        let logged = gs.messages.back().expect("a message was logged");
        assert!(logged.text.contains("rat whiskers"), "name kept: {:?}", logged.text);
        assert!(!logged.text.contains('\u{12}'), "control byte stripped: {:?}", logged.text);
        assert!(!logged.text.contains("00000"), "hex body stripped: {:?}", logged.text);
    }

    #[test]
    fn apply_death_of_self_zeroes_cur_hp_for_dead_pose() {
        // The self-render path derives the dead pose from cur_hp<=0 && max_hp>0,
        // so a self death packet must zero cur_hp (not just hp_pct). (eqoxide#44)
        let mut gs = GameState::new();
        gs.player_id = 42;
        gs.cur_hp = 30;
        gs.max_hp = 30;
        let mut payload = vec![0u8; SIZE_DEATH];
        payload[0..4].copy_from_slice(&42u32.to_le_bytes()); // spawn_id = player
        apply_death(&mut gs, &payload);
        assert_eq!(gs.cur_hp, 0, "self death must zero cur_hp so the dead pose triggers");
        assert!((gs.hp_pct - 0.0).abs() < 1e-4);
        assert!(gs.cur_hp <= 0 && gs.max_hp > 0, "player_dead condition must hold");
    }

    fn self_death_payload(player_id: u32) -> Vec<u8> {
        let mut p = vec![0u8; SIZE_DEATH];
        p[0..4].copy_from_slice(&player_id.to_le_bytes()); // spawn_id = player
        p
    }

    #[test]
    fn apply_death_captures_killer_and_death_time() {
        // #284: a self death records who killed us + when, persisted for /observe/debug so a headless
        // agent can tell it died (and by what) even though it later respawns.
        let mut gs = GameState::new();
        gs.player_id = 7; gs.max_hp = 100; gs.cur_hp = 100;
        gs.entities.insert(42, test_entity(42, "a_giant_rat", 100.0));
        let mut p = vec![0u8; SIZE_DEATH];
        p[0..4].copy_from_slice(&7u32.to_le_bytes());  // spawn_id = player
        p[4..8].copy_from_slice(&42u32.to_le_bytes()); // killer_id
        super::apply_death(&mut gs, &p);
        assert!(gs.player_dead, "self death marks player_dead");
        assert_eq!(gs.killed_by, "a_giant_rat", "killer name captured");
        assert!(gs.died_at.is_some(), "death time recorded");
        // An unknown killer falls back to a readable placeholder, never empty.
        let mut gs2 = GameState::new();
        gs2.player_id = 7; gs2.max_hp = 100; gs2.cur_hp = 100;
        super::apply_death(&mut gs2, &self_death_payload(7));
        assert_eq!(gs2.killed_by, "something");
    }

    #[test]
    fn apply_death_dedupes_duplicate_self_death() {
        // The server sometimes double-delivers OP_Death; the second must be ignored so we
        // don't double-log or restart the respawn timer, but player_dead_since is set once.
        let mut gs = GameState::new();
        gs.player_id = 42;
        apply_death(&mut gs, &self_death_payload(42));
        let first = gs.player_dead_since;
        assert!(first.is_some(), "first self death sets player_dead_since");
        let slain_count = gs.messages.iter().filter(|m| m.text.contains("slain")).count();
        assert_eq!(slain_count, 1);

        apply_death(&mut gs, &self_death_payload(42)); // duplicate
        assert_eq!(gs.player_dead_since, first, "duplicate must not restart the timer");
        let slain_count = gs.messages.iter().filter(|m| m.text.contains("slain")).count();
        assert_eq!(slain_count, 1, "duplicate death must not log a second slain message");
    }

    #[test]
    fn update_hp_alive_clears_death_bookkeeping() {
        let mut gs = GameState::new();
        gs.player_id = 42;
        apply_death(&mut gs, &self_death_payload(42));
        assert!(gs.player_dead_since.is_some());
        // Respawn HP restore → clears the safety-net flag.
        gs.update_hp(42, 36, 36);
        assert!(gs.player_dead_since.is_none(), "restoring HP clears player_dead_since");
    }

    fn test_entity(id: u32, name: &str, hp_pct: f32) -> Entity {
        Entity {
            spawn_id: id, name: name.to_string(), level: 1, is_npc: true,
            x: 0.0, y: 0.0, z: 0.0, hp_pct, cur_hp: 100, max_hp: 100,
            race: String::new(), heading: 0.0, dead: false,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0, helm: 0, showhelm: 0,
            face: 0, hairstyle: 0, haircolor: 0, animation: 0, floating: false,
        }
    }

    #[test]
    fn new_zone_clears_stale_entities_from_prior_zone() {
        // #270: OP_NewZone fires on every server-driven zone entry (travel, same-zone #zone,
        // death-respawn) and must purge the previous zone's spawns, or respawns/re-zones leak
        // stale + duplicate cross-zone entities into name→position resolution. The clears run
        // before the length guard, so a short payload still exercises them; the following
        // OP_ZoneEntry stream repopulates the map for the new zone.
        let mut gs = GameState::new();
        gs.entities.insert(1, test_entity(1, "Fippy_Darkpaw", 100.0));
        gs.entities.insert(2, test_entity(2, "a_gnoll_pup", 100.0));
        assert_eq!(gs.entities.len(), 2);
        super::apply_new_zone(&mut gs, &[]);
        assert!(gs.entities.is_empty(), "prior-zone entities must be cleared on zone entry (#270)");
    }

    #[test]
    fn apply_shop_player_sell_parses_rof2_echo_and_removes_item() {
        // #269: the server echoes the 20-byte RoF2 Merchant_Purchase_Struct (Slot i16@4,
        // quantity@12, price@16). The old 16-byte read took quantity/price from the wrong offsets
        // and never removed the item, so a sale looked failed. A correct echo for slot 28 must
        // drop the item and log the payout.
        let mut gs = GameState::new();
        gs.inventory.push(crate::game_state::InvItem {
            slot: 28, item_id: 13007, name: "Bone Chips".into(), icon: 0, charges: 1,
            idfile: String::new(), click_spell_id: 0, filename: String::new(),
        });
        let mut echo = [0u8; 20];
        echo[0..4].copy_from_slice(&123u32.to_le_bytes());   // npcid
        echo[4..6].copy_from_slice(&28i16.to_le_bytes());     // inventory_slot.Slot
        echo[6..8].copy_from_slice(&(-1i16).to_le_bytes());   // SubIndex (not in a bag)
        echo[12..16].copy_from_slice(&1u32.to_le_bytes());    // quantity
        echo[16..20].copy_from_slice(&3u32.to_le_bytes());    // price
        super::apply_shop_player_sell(&mut gs, &echo);
        assert!(gs.inventory.iter().all(|i| i.slot != 28), "sold item must leave inventory");
        assert!(gs.messages.back().unwrap().text.contains("Sold Bone Chips"),
            "sale confirmation logged: {}", gs.messages.back().unwrap().text);
    }

    #[test]
    fn apply_consider_parses_20byte_reply_and_logs_attitude() {
        // #273: the server ENCODEs OP_Consider as the 20-byte RoF2 Consider_Struct (targetid@4,
        // faction@8, level@12). apply_consider must produce the attitude line + set the con color.
        let mut gs = GameState::new();
        gs.entities.insert(450, test_entity(450, "Guard_Phaeton", 100.0));
        let mut reply = [0u8; 20];
        reply[4..8].copy_from_slice(&450u32.to_le_bytes());   // targetid
        reply[8..12].copy_from_slice(&9u32.to_le_bytes());     // faction 9 = scowls, ready to attack
        reply[12..16].copy_from_slice(&2u32.to_le_bytes());    // level (con color)
        super::apply_consider(&mut gs, &reply);
        let m = gs.messages.back().unwrap().text.clone();
        assert!(m.contains("Guard_Phaeton") && m.contains("scowls"), "attitude line: {m}");
        assert!(gs.target_con.is_some(), "con color must be set for the HUD tint");
    }

    #[test]
    fn con_level_and_attitude_names_map_wire_values() {
        // #292: the ConsiderColor `level` → difficulty tier, and faction → attitude enum.
        use super::{con_level_name, attitude_name};
        assert_eq!(con_level_name(6), "gray");       // no exp / trivial
        assert_eq!(con_level_name(2), "green");
        assert_eq!(con_level_name(18), "light_blue");
        assert_eq!(con_level_name(4), "blue");
        assert_eq!(con_level_name(10), "white");     // even
        assert_eq!(con_level_name(20), "white");     // WhiteTitanium
        assert_eq!(con_level_name(15), "yellow");
        assert_eq!(con_level_name(13), "red");       // dangerous
        assert_eq!(attitude_name(1), "ally");
        assert_eq!(attitude_name(5), "indifferent");
        assert_eq!(attitude_name(9), "scowls");      // KOS
    }

    #[test]
    fn apply_consider_sets_structured_con_fields() {
        // #292: apply_consider records the difficulty tier + attitude enum for /observe/debug.
        let mut gs = GameState::new();
        gs.entities.insert(450, test_entity(450, "a_guard", 100.0));
        let mut reply = [0u8; 20];
        reply[4..8].copy_from_slice(&450u32.to_le_bytes());   // targetid
        reply[8..12].copy_from_slice(&9u32.to_le_bytes());     // faction 9 = scowls
        reply[12..16].copy_from_slice(&13u32.to_le_bytes());   // level 13 = red
        super::apply_consider(&mut gs, &reply);
        assert_eq!(gs.target_con_name.as_deref(), Some("red"), "difficulty tier stored");
        assert_eq!(gs.target_attitude.as_deref(), Some("scowls"), "attitude enum stored");
    }

    fn inv_item(slot: i32, item_id: u32, charges: i32) -> crate::game_state::InvItem {
        crate::game_state::InvItem {
            slot, item_id, name: format!("item{item_id}"), icon: 0, charges,
            idfile: String::new(), click_spell_id: 0, filename: String::new(),
        }
    }

    /// Build a RoF2 DeleteItem_Struct (28B): from_slot InventorySlot_Struct (Type@0, Slot@4),
    /// to_slot@12, number_in_stack@24.
    fn delete_item_wire(slot_type: i16, slot: i16, count: u32) -> [u8; 28] {
        let mut b = [0u8; 28];
        b[0..2].copy_from_slice(&slot_type.to_le_bytes());
        b[4..6].copy_from_slice(&slot.to_le_bytes());
        b[24..28].copy_from_slice(&count.to_le_bytes());
        b
    }

    #[test]
    fn apply_delete_item_clears_slot_and_leaves_others() {
        // #275: the SwapItemResync clear sends OP_DeleteItem with number_in_stack=0xFFFFFFFF to
        // erase the scratch token it dropped on an empty slot. Handle it (was unhandled → token
        // stuck forever); only the named slot is removed.
        let mut gs = GameState::new();
        gs.inventory.push(inv_item(33, 22292, 1)); // Copper Coin scratch token on cursor
        gs.inventory.push(inv_item(28, 13007, 1)); // real Bone Chips in general inv
        super::apply_delete_item(&mut gs, &delete_item_wire(0, 33, 0xFFFF_FFFF));
        assert!(gs.inventory.iter().all(|i| i.slot != 33), "cleared token removed");
        assert!(gs.inventory.iter().any(|i| i.slot == 28), "other slots untouched");
    }

    #[test]
    fn apply_delete_charge_reduces_stack_partial() {
        let mut gs = GameState::new();
        gs.inventory.push(inv_item(28, 13007, 5));
        super::apply_delete_item(&mut gs, &delete_item_wire(0, 28, 2)); // remove 2 of 5
        assert_eq!(gs.inventory.iter().find(|i| i.slot == 28).unwrap().charges, 3);
    }

    #[test]
    fn apply_delete_item_ignores_non_possessions_slot() {
        // A typeTrade (3) delete must not touch our possessions inventory.
        let mut gs = GameState::new();
        gs.inventory.push(inv_item(5, 999, 1));
        super::apply_delete_item(&mut gs, &delete_item_wire(3, 5, 0xFFFF_FFFF));
        assert!(gs.inventory.iter().any(|i| i.slot == 5), "non-possessions delete ignored");
    }

    #[test]
    fn combat_damage_to_player_decrements_local_hp() {
        // eqoxide#55: a hit on the player should optimistically reduce local HP between OP_HPUpdates.
        use super::apply_combat_damage;
        // CombatDamage_Struct: target@0(u16) source@2(u16) type@4(u8) spellid@5(u32) damage@9(i32).
        let dmg = |target: u16, source: u16, damage: i32| -> [u8; 13] {
            let mut b = [0u8; 13];
            b[0..2].copy_from_slice(&target.to_le_bytes());
            b[2..4].copy_from_slice(&source.to_le_bytes());
            b[9..13].copy_from_slice(&damage.to_le_bytes());
            b
        };
        let mut gs = GameState::new();
        gs.player_id = 7; gs.cur_hp = 100; gs.max_hp = 100; gs.hp_pct = 100.0;

        apply_combat_damage(&mut gs, &dmg(7, 99, 14)); // mob hits player for 14
        assert_eq!(gs.cur_hp, 86, "player HP should drop by the hit");
        assert!((gs.hp_pct - 86.0).abs() < 1e-4, "hp_pct recomputed: {}", gs.hp_pct);

        apply_combat_damage(&mut gs, &dmg(7, 99, 0)); // a miss
        assert_eq!(gs.cur_hp, 86, "a miss must not change HP");

        apply_combat_damage(&mut gs, &dmg(99, 7, 50)); // player hits an NPC
        assert_eq!(gs.cur_hp, 86, "damage to an NPC must not change the player's HP");

        apply_combat_damage(&mut gs, &dmg(7, 99, 9999)); // lethal hit
        assert_eq!(gs.cur_hp, 0, "HP clamps at 0");
        assert!((gs.hp_pct - 0.0).abs() < 1e-4);
    }

    #[test]
    fn apply_combat_damage_formats_hits_misses_and_special_sentinels() {
        // #262: damage>0 = a hit, ==0 = a clean miss (no "(type=N)" leak), <0 = an EQEmu
        // special-outcome sentinel (zone/common.h DMG_*) rendered with native wording — never
        // as "-N damage".
        use super::apply_combat_damage;
        let dmg = |target: u16, source: u16, damage: i32| -> [u8; 13] {
            let mut b = [0u8; 13];
            b[0..2].copy_from_slice(&target.to_le_bytes());
            b[2..4].copy_from_slice(&source.to_le_bytes());
            b[4] = 1; // skill type — must NOT leak into player-facing text
            b[9..13].copy_from_slice(&damage.to_le_bytes());
            b
        };
        let mut gs = GameState::new();
        gs.player_id = 7; gs.player_name = "Hero".into(); gs.max_hp = 100; gs.cur_hp = 100;
        let last = |gs: &GameState| gs.messages.back().unwrap().text.clone();

        apply_combat_damage(&mut gs, &dmg(7, 99, 24));
        assert!(last(&gs).contains("for 24 damage"), "hit: {}", last(&gs));

        apply_combat_damage(&mut gs, &dmg(7, 99, 0));
        assert!(last(&gs).contains("misses"), "miss: {}", last(&gs));
        assert!(!last(&gs).contains("type="), "miss must not leak skill type: {}", last(&gs));

        for (d, verb) in [(-1, "blocks"), (-2, "parries"), (-3, "ripostes"),
                          (-4, "dodges"), (-5, "invulnerable"), (-6, "rune")] {
            apply_combat_damage(&mut gs, &dmg(7, 99, d));
            let m = last(&gs);
            assert!(m.contains(verb), "sentinel {d} should say '{verb}': {m}");
            assert!(!m.contains("damage"), "sentinel {d} must not render as damage: {m}");
            assert!(!m.contains(&format!("{d}")), "sentinel {d} must not leak the raw number: {m}");
        }
    }

    #[test]
    fn apply_combat_damage_spell_is_not_a_melee_miss() {
        // #272: a spell landing via OP_Damage carries a non-zero spellid@5. A heal on a full target
        // arrives with damage==0 and previously fell into the melee "tries to hit … misses!" branch.
        // (No spell db is loaded in the unit test, so name/beneficial resolve to None/false and the
        // wording falls to the generic spell phrasing — which is still NOT melee-miss text.)
        use super::apply_combat_damage;
        let spell = |target: u16, source: u16, spellid: u32, damage: i32| -> [u8; 13] {
            let mut b = [0u8; 13];
            b[0..2].copy_from_slice(&target.to_le_bytes());
            b[2..4].copy_from_slice(&source.to_le_bytes());
            b[5..9].copy_from_slice(&spellid.to_le_bytes()); // spellid@5 (u32)
            b[9..13].copy_from_slice(&damage.to_le_bytes());
            b
        };
        let mut gs = GameState::new();
        gs.player_id = 7; gs.player_name = "Piety".into(); gs.max_hp = 100; gs.cur_hp = 100;
        let last = |gs: &GameState| gs.messages.back().unwrap().text.clone();

        // Self-cast heal restoring 0 HP (full target): spellid=200, damage=0.
        apply_combat_damage(&mut gs, &spell(7, 7, 200, 0));
        let m = last(&gs);
        assert!(!m.contains("misses") && !m.contains("tries to hit"),
            "a landed heal must NOT render as a melee miss: {m}");
        assert!(m.contains("spell") || m.contains("Piety"), "should read as a spell: {m}");
        assert_eq!(gs.cur_hp, 100, "a 0-damage spell must not change HP");

        // Spell DAMAGE (nuke) still shows the amount, worded as a spell not plain melee.
        apply_combat_damage(&mut gs, &spell(7, 99, 300, 40));
        assert!(last(&gs).contains("for 40 damage"), "spell damage shows the amount: {}", last(&gs));

        // Sanity: a real MELEE miss (spellid==0, damage==0) is unaffected.
        apply_combat_damage(&mut gs, &spell(7, 99, 0, 0));
        assert!(last(&gs).contains("misses"), "melee 0-damage still a miss: {}", last(&gs));
    }

    #[test]
    fn looted_item_places_in_free_slot_and_stacks_never_overwrites() {
        // eqoxide#56: loot must not evict an occupied slot, and stackable loot should merge.
        use super::apply_looted_item;
        use crate::game_state::InvItem;
        let inv = |slot: i32, id: u32, ch: i32| InvItem {
            slot, item_id: id, name: String::new(), icon: 0, charges: ch, idfile: String::new(),
            click_spell_id: 0, filename: String::new(),
        };
        let mut gs = GameState::new();
        // Skin of Milk x20 @23, Bread Cakes x20 @24, Rat Whiskers x1 @25.
        gs.inventory = vec![inv(23, 9990, 20), inv(24, 9991, 20), inv(25, 13071, 1)];

        // Loot another Rat Whiskers; the packet's main_slot bogusly names an occupied slot (23).
        apply_looted_item(&mut gs, inv(23, 13071, 1));
        assert_eq!(gs.inventory.iter().find(|x| x.slot == 23).map(|x| x.item_id), Some(9990),
            "Skin of Milk must not be overwritten");
        assert_eq!(gs.inventory.iter().find(|x| x.slot == 24).map(|x| x.item_id), Some(9991),
            "Bread Cakes must not be overwritten");
        let rw: Vec<_> = gs.inventory.iter().filter(|x| x.item_id == 13071).collect();
        assert_eq!(rw.len(), 1, "Rat Whiskers should stack into one slot, not split");
        assert_eq!(rw[0].charges, 2, "stack quantity should merge to 2");

        // Loot a brand-new item with a bogus main_slot (24) → first FREE general slot (26).
        apply_looted_item(&mut gs, inv(24, 9131, 1));
        assert_eq!(gs.inventory.iter().find(|x| x.slot == 24).map(|x| x.item_id), Some(9991),
            "Bread Cakes still untouched");
        assert_eq!(gs.inventory.iter().find(|x| x.item_id == 9131).unwrap().slot, 26,
            "new loot goes to the first free general slot");
        assert_eq!(gs.inventory.len(), 4);
    }

    #[test]
    fn apply_spawn_appearance_toggles_player_sitting() {
        // The synthetic bridge (nav -> render gs) for a client-initiated sit must flip the RENDER
        // gs.sitting so the player's own sit animation plays. (eqoxide#53, two-GameState split)
        let mut gs = GameState::new();
        gs.player_id = 77;
        // kind 14 (Animation), param 110 (sit) for our own id -> sitting.
        apply_spawn_appearance(&mut gs, &crate::eq_net::navigation::build_spawn_appearance_packet(77, 14, 110));
        assert!(gs.sitting, "sit appearance for our player must set render sitting");
        // param 100 (stand) -> not sitting.
        apply_spawn_appearance(&mut gs, &crate::eq_net::navigation::build_spawn_appearance_packet(77, 14, 100));
        assert!(!gs.sitting, "stand appearance clears render sitting");
        // Another spawn's sit must NOT change our flag.
        apply_spawn_appearance(&mut gs, &crate::eq_net::navigation::build_spawn_appearance_packet(77, 14, 110));
        apply_spawn_appearance(&mut gs, &crate::eq_net::navigation::build_spawn_appearance_packet(999, 14, 100));
        assert!(gs.sitting, "another spawn's stand must not clear our sitting");
    }

    #[test]
    fn apply_set_target_sets_render_target_from_entity() {
        // Synthetic OP_TARGET_MOUSE (nav -> render gs) carries the 4-byte LE spawn_id; the render
        // gs should adopt the target and seed name/hp from the entity list. (eqoxide#9)
        let mut gs = GameState::new();
        gs.upsert_entity(test_entity(332, "Merchant Kwein", 80.0));
        apply_set_target(&mut gs, &332u32.to_le_bytes());
        assert_eq!(gs.target_id, Some(332));
        assert_eq!(gs.target_name.as_deref(), Some("Merchant Kwein"));
        assert_eq!(gs.target_hp_pct, Some(80.0));
    }

    #[test]
    fn apply_move_item_equips_held_weapon_in_render_gs() {
        // Synthetic OP_MoveItem (nav -> render gs): 8-byte from(i32 LE) + to(i32 LE). Equipping a
        // weapon from the cursor (33) to the off hand (14) must move it in the render gs so the
        // scene derives the held model from slot 14 without a relog. (eqoxide#141)
        let mut gs = GameState::new();
        gs.inventory.push(crate::game_state::InvItem {
            slot: 33, item_id: 9023, name: "Qeynos Kite Shield".into(), idfile: "IT63".into(),
            ..Default::default()
        });
        let mut mv = [0u8; 8];
        mv[0..4].copy_from_slice(&33i32.to_le_bytes());
        mv[4..8].copy_from_slice(&14i32.to_le_bytes());
        apply_move_item(&mut gs, &mv);
        assert_eq!(gs.inventory.iter().find(|i| i.slot == 14).map(|i| i.idfile.as_str()), Some("IT63"));
        assert!(gs.inventory.iter().all(|i| i.slot != 33), "cursor slot vacated");
        // A too-short payload is ignored (no panic, no change).
        apply_move_item(&mut gs, &[0u8; 4]);
        assert_eq!(gs.inventory.iter().find(|i| i.slot == 14).map(|i| i.idfile.as_str()), Some("IT63"));

        // A REAL 28-byte inbound wire MoveItem_Struct (server sends these on trade/autostack/resync)
        // is dispatched through the same apply_packet and reaches this handler — it must be IGNORED,
        // not decoded as (from=0, to=garbage), which would relocate slot 0 and corrupt inventory.
        gs.inventory.push(crate::game_state::InvItem {
            slot: 0, item_id: 1234, name: "Charm".into(), ..Default::default()
        });
        // Mirror a real Worn/Normal wire move: from_slot = Type(0)|Unknown02(0) → first 4 bytes 0;
        // to_slot = Slot|SubIndex(-1) → a large garbage i32 (bytes 6..8 = 0xFFFF). Decoding this as
        // (from=0, to=garbage) is exactly what would relocate slot 0 without the exact-length guard.
        let mut wire = [0u8; 28]; // MoveItem_Struct: from_slot(12) + to_slot(12) + number_in_stack(4)
        wire[6] = 0xFF; wire[7] = 0xFF; // to_slot SubIndex = -1 → `to` is nonzero garbage
        apply_move_item(&mut gs, &wire);
        assert!(gs.inventory.iter().any(|i| i.slot == 0 && i.name == "Charm"),
            "28-byte inbound wire MoveItem must be ignored — slot 0 must not be relocated");
        assert_eq!(gs.inventory.iter().find(|i| i.slot == 14).map(|i| i.idfile.as_str()), Some("IT63"),
            "the equipped weapon is untouched by the ignored wire packet");
    }

    #[test]
    fn apply_set_target_unknown_entity_sets_id_only() {
        let mut gs = GameState::new();
        apply_set_target(&mut gs, &7u32.to_le_bytes());
        assert_eq!(gs.target_id, Some(7));
        assert_eq!(gs.target_name, None);
        assert_eq!(gs.target_hp_pct, None);
    }

    #[test]
    fn money_update_sets_coin_total() {
        let mut gs = GameState::new();
        gs.coin = [1, 2, 3, 4];
        // MoneyUpdate_Struct: platinum=84 gold=9 silver=13 copper=8 (i32 LE)
        let mut p = Vec::new();
        for v in [84i32, 9, 13, 8] { p.extend_from_slice(&v.to_le_bytes()); }
        apply_money_update(&mut gs, &p);
        assert_eq!(gs.coin, [84, 9, 13, 8]);
    }

    #[test]
    fn money_on_corpse_adds_looted_coin() {
        let mut gs = GameState::new();
        gs.coin = [10, 0, 5, 0];
        // MoneyOnCorpse_Struct: response(0)+3pad + platinum=2 gold=1 silver=0 copper=3 (u32 LE)
        let mut p = vec![0u8; 4];
        for v in [2u32, 1, 0, 3] { p.extend_from_slice(&v.to_le_bytes()); }
        apply_money_on_corpse(&mut gs, &p);
        assert_eq!(gs.coin, [12, 1, 5, 3]); // added on top of existing
    }
    use crate::eq_net::item::tests::{fixture, fixture2};

    #[test]
    fn class_name_maps_ids() {
        assert_eq!(class_name(1), "Warrior");
        assert_eq!(class_name(11), "Necromancer");
        assert_eq!(class_name(16), "Berserker");
        assert_eq!(class_name(0), "");
        assert_eq!(class_name(99), "");
    }

    #[test]
    fn who_all_request_is_156_bytes_with_filters() {
        let p = crate::eq_net::protocol::build_who_all_request(3);
        assert_eq!(p.len(), 156, "RoF2 Who_All_Struct must be exactly 156 bytes (DECODE_LENGTH_EXACT)");
        // whom[0..64] and unknown088[64..128] are zero.
        assert!(p[0..128].iter().all(|&b| b == 0), "whom + unknown088 pad are empty/zero");
        // wrace..guildid (offsets 128..152) are all 0xFFFFFFFF (no filter).
        for off in (128..152).step_by(4) {
            assert_eq!(u32::from_le_bytes(p[off..off + 4].try_into().unwrap()), 0xFFFF_FFFF);
        }
        assert_eq!(u32::from_le_bytes(p[152..156].try_into().unwrap()), 3, "type=3 = /who all");
    }

    #[test]
    fn apply_who_all_parses_rof2_widened_records() {
        // Build a synthetic RoF2 OP_WhoAllResponse: 64-byte header (count at offset 44) + 2 records
        // in the RoF2 widened layout (the extra pad0 u32 after FormatMSGID).
        fn push_u32(v: &mut Vec<u8>, x: u32) { v.extend_from_slice(&x.to_le_bytes()); }
        fn push_cstr(v: &mut Vec<u8>, s: &str) { v.extend_from_slice(s.as_bytes()); v.push(0); }
        fn record(v: &mut Vec<u8>, name: &str, guild: &str, zonestr: u32, zone: u32, class: u32, level: u32, race: u32) {
            push_u32(v, 5025);        // FormatMSGID
            push_u32(v, 0);           // pad0 (RoF2-only)
            push_u32(v, 0xFFFF_FFFF); // PIDMSGID
            push_cstr(v, name);
            push_u32(v, 0);           // RankMSGID
            push_cstr(v, guild);
            push_u32(v, 0xFFFF_FFFF); // Unknown80[0]
            push_u32(v, 0xFFFF_FFFF); // Unknown80[1]
            push_u32(v, zonestr);     // ZoneMSGID
            push_u32(v, zone);
            push_u32(v, class);
            push_u32(v, level);
            push_u32(v, race);
            push_cstr(v, "");         // Account
            push_u32(v, 207);         // Unknown100
        }
        let mut p = vec![0u8; 64];
        p[44..48].copy_from_slice(&2u32.to_le_bytes()); // count = 2 at offset 44
        record(&mut p, "Alice", "Knights of Truth", 5, 2, 3 /*Paladin*/, 10, 1 /*HUM*/);
        record(&mut p, "Bob", "", 0xFFFF_FFFF, 0, 0, 0, 0); // anonymous

        let mut gs = GameState::new();
        apply_who_all(&mut gs, &p);
        assert_eq!(gs.who_roster.len(), 2);

        let a = &gs.who_roster[0];
        assert_eq!(a.name, "Alice");
        assert_eq!(a.guild, "Knights of Truth");
        assert_eq!(a.zone_id, 2);
        assert_eq!(a.class, 3);
        assert_eq!(a.level, 10);
        assert_eq!(a.race, 1);
        assert!(!a.anon);

        let b = &gs.who_roster[1];
        assert_eq!(b.name, "Bob");
        assert!(b.anon, "ZoneMSGID=0xFFFFFFFF and zeroed stats => anonymous");
        assert_eq!(b.level, 0);
    }

    #[test]
    fn parse_player_profile_reads_offsets() {
        // Too short → None (need at least @980 for WIS).
        assert!(parse_player_profile(&[0u8; 100]).is_none());
        assert!(parse_player_profile(&[0u8; 979]).is_none());

        // RoF2 PlayerProfile wire offsets. The stream (rof2.cpp ENCODE) writes 300
        // disciplines vs the 200 the struct reserves, so fields after disciplines are
        // +400 bytes vs their rof2_structs.h comment:
        //   @21: class_, @22: level
        //   @952: STR, @976: WIS  (before disciplines → struct offset is correct)
        //   @9784: mem_spells[0..9]      (struct /*09384*/ + 400)
        //   @13269: platinum .. @13281: copper  (struct /*12869*/ + 400)
        let mut buf = vec![0u8; 14000];
        buf[21] = 9;   // class_ = Rogue
        buf[22] = 12;  // level
        buf[944..948].copy_from_slice(&333u32.to_le_bytes());   // mana @944 (before disciplines)
        buf[948..952].copy_from_slice(&1234u32.to_le_bytes());  // cur_hp @948 (before disciplines)
        buf[952..956].copy_from_slice(&75u32.to_le_bytes());    // STR
        buf[976..980].copy_from_slice(&110u32.to_le_bytes());   // WIS
        // mem_spells[0] @9784 = 200 (Minor Healing), mem_spells[1] @9788 = 0xFFFFFFFF (empty)
        buf[9784..9788].copy_from_slice(&200u32.to_le_bytes());
        buf[9788..9792].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        // platinum/gold/silver/copper @13269..13285
        buf[13269..13273].copy_from_slice(&5u32.to_le_bytes());  // platinum
        buf[13273..13277].copy_from_slice(&3u32.to_le_bytes());  // gold
        buf[13277..13281].copy_from_slice(&7u32.to_le_bytes());  // silver
        buf[13281..13285].copy_from_slice(&9u32.to_le_bytes());  // copper
        let p = parse_player_profile(&buf).unwrap();
        assert_eq!(p.level, 12);
        assert_eq!(p.class_id, 9);
        assert_eq!(p.coin, [5, 3, 7, 9]);
        assert_eq!(p.stats[0], 75);  // STR
        assert_eq!(p.stats[6], 110); // WIS
        assert_eq!(p.cur_hp, 1234);  // cur_hp @948
        assert_eq!(p.cur_mana, 333); // mana @944
        assert_eq!(class_name(p.class_id), "Rogue");
        assert_eq!(p.mem_spells[0], 200);
        assert_eq!(p.mem_spells[1], 0xFFFF_FFFF);
    }

    #[test]
    fn is_debug_spam_filters_gm_loot_dumps() {
        assert!(super::is_debug_spam(
            "[Loot] [AddLootDrop] NPC [Guard_Tyrak000] Item (5019) ... trivial min/max [0/0]"));
        assert!(!super::is_debug_spam("Greetings, traveler. Are you my [contact]?"));
    }

    #[test]
    fn con_color_maps_levels() {
        assert_eq!(con_color(2), [90, 220, 90]);    // green (trivial)
        assert_eq!(con_color(13), [240, 80, 80]);   // red (dangerous)
        assert_eq!(con_color(15), [240, 230, 80]);  // yellow
        assert_eq!(con_color(20), con_color(10));   // WhiteTitanium == White
        assert_eq!(con_color(999), [235, 235, 235]); // unknown → white
    }

    #[test]
    fn consider_message_covers_faction_cons() {
        assert_eq!(consider_message(9), "scowls at you, ready to attack");
        assert_eq!(consider_message(5), "regards you indifferently");
        assert_eq!(consider_message(1), "regards you as an ally");
        // Out-of-range falls back to a neutral phrasing.
        assert_eq!(consider_message(0), "regards you");
        assert_eq!(consider_message(99), "regards you");
    }

    fn emote_payload(etype: u32, msg: &str) -> Vec<u8> {
        let mut p = etype.to_le_bytes().to_vec();
        p.extend_from_slice(msg.as_bytes());
        p.push(0);
        p
    }

    #[test]
    fn apply_emote_logs_custom_text_skips_animations() {
        let mut gs = GameState::new();
        apply_emote(&mut gs, &emote_payload(0, "Guard Phaeton beckons you closer."));
        assert!(gs.messages.iter().any(|m| m.kind == "npc"
            && m.text == "Guard Phaeton beckons you closer."));

        // Animation-command emotes (0xffffffff) carry no useful text and are skipped.
        let before = gs.messages.len();
        apply_emote(&mut gs, &emote_payload(0xffff_ffff, ""));
        assert_eq!(gs.messages.len(), before, "animation emote should not be logged");
    }

    // --- is_debug_spam: new filter coverage ---

    #[test]
    fn is_debug_spam_filters_combat_record() {
        assert!(super::is_debug_spam("[CombatRecord] [Stop] [Summary] Warrior hit 3 times"));
    }

    #[test]
    fn is_debug_spam_filters_event_killed_merit() {
        assert!(super::is_debug_spam("[EVENT_KILLED_MERIT] npc=Guard_Tyrak000 merit=5"));
    }

    #[test]
    fn is_debug_spam_filters_event_item_given() {
        assert!(super::is_debug_spam("[EVENT_ITEM_GIVEN] item=5019 to player=Testhero"));
    }

    #[test]
    fn is_debug_spam_allows_contact_npc_speech() {
        assert!(!super::is_debug_spam("Greetings, traveler. Are you my [contact]?"));
    }

    // --- apply_channel_message helpers and tests ---

    // Build a RoF2 OP_ChannelMessage wire packet (server->client), matching
    // rof2.cpp ENCODE(OP_ChannelMessage): sender\0 target\0 u32 unk | u32 lang |
    // u32 chan | u32 unk | u8 unk | u32 skill | message\0.
    fn make_chan_payload_to(sender: &str, target: &str, chan_num: u32, msg: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(sender.as_bytes()); buf.push(0);
        buf.extend_from_slice(target.as_bytes()); buf.push(0);
        buf.extend_from_slice(&0u32.to_le_bytes());      // unknown
        buf.extend_from_slice(&0u32.to_le_bytes());      // language
        buf.extend_from_slice(&chan_num.to_le_bytes());  // chan_num
        buf.extend_from_slice(&0u32.to_le_bytes());      // unknown
        buf.push(0);                                     // unknown u8
        buf.extend_from_slice(&0u32.to_le_bytes());      // skill_in_language
        buf.extend_from_slice(msg.as_bytes()); buf.push(0);
        buf
    }

    fn make_chan_payload(sender: &str, chan_num: u32, msg: &str) -> Vec<u8> {
        make_chan_payload_to(sender, "", chan_num, msg)
    }

    #[test]
    fn apply_channel_message_logs_under_channel_kind() {
        // Channel messages log under their channel kind (shout/tell/ooc/group)
        // so the chat window can tab-filter and color them (#162).
        let mut gs = GameState::new();
        let payload = make_chan_payload("Soandso", 3, "Hello zone!");
        super::apply_channel_message(&mut gs, &payload);
        assert!(gs.messages.iter().any(|m| m.kind == "shout"
            && m.text == "<Soandso> Hello zone!"));

        // Plain say (8) still logs as "chat".
        let payload = make_chan_payload("Soandso", 8, "hi there");
        super::apply_channel_message(&mut gs, &payload);
        assert!(gs.messages.iter().any(|m| m.kind == "chat"
            && m.text == "<Soandso> hi there"));
    }

    #[test]
    fn apply_channel_message_zone_without_sender_logs_zone() {
        let mut gs = GameState::new();
        let payload = make_chan_payload("", 3, "An earthquake strikes!");
        super::apply_channel_message(&mut gs, &payload);
        assert!(gs.messages.iter().any(|m| m.kind == "zone"
            && m.text == "An earthquake strikes!"));
    }

    #[test]
    fn apply_channel_message_say_with_sender_logs_chat() {
        let mut gs = GameState::new();
        let payload = make_chan_payload("Guard_Janior", 8, "Halt, adventurer!");
        super::apply_channel_message(&mut gs, &payload);
        assert!(gs.messages.iter().any(|m| m.kind == "chat"
            && m.text == "<Guard_Janior> Halt, adventurer!"));
        // Say (NPC dialogue) is NOT an inter-agent chat event.
        assert!(gs.chat_events.is_empty(), "say must not produce a chat event");
    }

    fn make_tell(sender: &str, target: &str, msg: &str) -> Vec<u8> {
        make_chan_payload_to(sender, target, 7, msg) // chan 7 = tell
    }

    #[test]
    fn apply_channel_message_tell_to_me_is_directed_event() {
        let mut gs = GameState::new();
        gs.player_name = "Mordeth".to_string();
        super::apply_channel_message(&mut gs, &make_tell("Garrik", "Mordeth", "you stuck?"));
        let e = gs.chat_events.back().expect("a chat event");
        assert_eq!(e.category, "chat");
        assert_eq!(e.kind, "tell");
        assert_eq!(e.from, "Garrik");
        assert!(e.directed, "a tell addressed to us is directed");
        assert_eq!(e.text, "you stuck?");
    }

    #[test]
    fn apply_channel_message_tell_to_someone_else_not_directed() {
        let mut gs = GameState::new();
        gs.player_name = "Mordeth".to_string();
        super::apply_channel_message(&mut gs, &make_tell("Garrik", "Katie", "hi"));
        let e = gs.chat_events.back().expect("a chat event");
        assert_eq!(e.category, "chat");
        assert_eq!(e.kind, "tell");
        assert!(!e.directed, "a tell to someone else is not directed at us");
    }

    #[test]
    fn apply_channel_message_guild_logs_and_events_as_guild() {
        // #294: guild chat is EQEmu ChatChannel 0. It logs under kind "guild" and surfaces as an
        // undirected chat event so agents can filter guild traffic via GET /v1/events/chat.
        let mut gs = GameState::new();
        gs.player_name = "Mordeth".to_string();
        super::apply_channel_message(&mut gs, &make_chan_payload("Garrik", 0, "forming up at the gate"));
        assert!(gs.messages.iter().any(|m| m.kind == "guild"
            && m.text == "<Garrik> forming up at the gate"));
        let e = gs.chat_events.back().expect("a chat event");
        assert_eq!(e.kind, "guild");
        assert_eq!(e.from, "Garrik");
        assert!(!e.directed, "guild chat is a broadcast, not directed at us");
        assert_eq!(e.text, "forming up at the gate");
    }

    #[test]
    fn apply_channel_message_ooc_is_undirected_event() {
        let mut gs = GameState::new();
        gs.player_name = "Mordeth".to_string();
        super::apply_channel_message(&mut gs, &make_chan_payload("Garrik", 5, "any GM around?"));
        let e = gs.chat_events.back().expect("a chat event");
        assert_eq!(e.category, "chat");
        assert_eq!(e.kind, "ooc");
        assert!(!e.directed);
    }

    #[test]
    fn apply_channel_message_too_short_logs_nothing() {
        let mut gs = GameState::new();
        super::apply_channel_message(&mut gs, &[0u8; 100]);
        assert!(gs.messages.is_empty(), "short payload should produce no messages");
    }

    #[test]
    fn apply_channel_message_empty_msg_logs_nothing() {
        let mut gs = GameState::new();
        // Build payload where message field is all zeros (empty after null-trim).
        let payload = make_chan_payload("Soandso", 3, "");
        super::apply_channel_message(&mut gs, &payload);
        assert!(gs.messages.is_empty(), "empty message should produce no log entry");
    }

    // --- guild membership (#295) ---

    #[test]
    fn apply_guild_list_builds_directory() {
        let mut gs = GameState::new();
        let mut p = vec![0u8; 64];               // 64-byte zero header
        p.extend_from_slice(&2u32.to_le_bytes()); // count = 2
        p.extend_from_slice(&7u32.to_le_bytes()); p.extend_from_slice(b"Knights\0");
        p.extend_from_slice(&9u32.to_le_bytes()); p.extend_from_slice(b"Mages\0");
        super::apply_guild_list(&mut gs, &p);
        assert_eq!(gs.guild_names.get(&7).map(|s| s.as_str()), Some("Knights"));
        assert_eq!(gs.guild_names.get(&9).map(|s| s.as_str()), Some("Mages"));
    }

    #[test]
    fn apply_guild_member_list_parses_big_endian_roster() {
        // OP_GuildMemberList is the one BIG-ENDIAN packet. Build one member and verify the fields
        // land, and that online is derived from zone_id != 0.
        let mut gs = GameState::new();
        let mut p = Vec::new();
        p.extend_from_slice(b"MyGuild\0");            // prefix cstr
        p.extend_from_slice(&0u32.to_be_bytes());     // guild_id (uninitialized — ignored)
        p.extend_from_slice(&1u32.to_be_bytes());     // member_count
        p.extend_from_slice(b"Alice\0");              // name
        for v in [10u32/*level*/, 0/*banker*/, 1/*class*/, 5/*rank*/, 0, 0, 0, 0, 0, 1] {
            p.extend_from_slice(&v.to_be_bytes());    // 10 u32s
        }
        p.extend_from_slice(b"tank\0");               // public_note
        p.extend_from_slice(&0u16.to_be_bytes());     // zoneinstance
        p.extend_from_slice(&22u16.to_be_bytes());    // zone_id (online)
        p.extend_from_slice(&1u32.to_be_bytes());     // unk
        p.extend_from_slice(&0u32.to_be_bytes());     // unk
        super::apply_guild_member_list(&mut gs, &p);
        assert_eq!(gs.guild_members.len(), 1);
        let m = &gs.guild_members[0];
        assert_eq!(m.name, "Alice");
        assert_eq!((m.level, m.class, m.rank, m.zone_id), (10, 1, 5, 22));
        assert!(m.online, "zone_id 22 → online");
        assert_eq!(m.public_note, "tank");
    }

    #[test]
    fn apply_guild_member_update_patches_presence() {
        let mut gs = GameState::new();
        gs.guild_members = vec![crate::game_state::GuildMember {
            name: "Bob".into(), rank: 5, level: 3, class: 1, zone_id: 22, online: true,
            public_note: String::new(),
        }];
        // Little-endian 80-byte update: GuildID(4) MemberName[64] ZoneID(2) InstanceID(2) ...
        let mut p = vec![0u8; 80];
        p[4..7].copy_from_slice(b"Bob");
        // zone_id = 0 at offset 68 → offline
        super::apply_guild_member_update(&mut gs, &p);
        assert!(!gs.guild_members[0].online, "zone_id 0 → offline");
        assert_eq!(gs.guild_members[0].zone_id, 0);
    }

    #[test]
    fn apply_wear_change_updates_one_slot() {
        use super::{register_spawn, apply_wear_change};
        use crate::eq_net::protocol::SpawnInfo;
        let mut gs = GameState::new();
        gs.player_name = "Nobody".into();
        let info = SpawnInfo {
            spawn_id: 42, name: "a".into(), last_name: String::new(),
            level: 5, npc: 1, gender: 0, race: 54, class_: 1, guild_id: 0xFFFF_FFFF, guild_rank: 0, body_type: 1,
            cur_hp: 100, helm: 0, show_helm: false, face: 0, hairstyle: 0, haircolor: 0, stand_state: 100,
            pet_owner_id: 0, player_state: 64,
            x: 0.0, y: 0.0, z: 0.0, heading: 0.0, animation: 100,
            equipment: [0u32; 9], equipment_tint: [[0u8; 3]; 9],
        };
        register_spawn(&mut gs, info);

        // spawn_id=42, material=17, color B,G,R=(1,2,3),UseTint=0xFF, wear_slot_id=1 (chest)
        let pkt = [42u8, 0, 17, 0, 1, 2, 3, 0xFF, 1];
        apply_wear_change(&mut gs, &pkt);

        let e = gs.entities.get(&42).unwrap();
        assert_eq!(e.equipment[1], 17);
        assert_eq!(e.equipment_tint[1], [3, 2, 1]); // stored RGB
        assert_eq!(e.equipment[0], 0, "other slots untouched");
    }

    #[test]
    fn apply_wear_change_ignores_short_packet() {
        use super::apply_wear_change;
        let mut gs = GameState::new();
        apply_wear_change(&mut gs, &[1, 2, 3]); // shorter than SIZE_WEAR_CHANGE; must not panic
    }

    #[test]
    fn register_spawn_lays_down_zone_in_corpses() {
        use super::register_spawn;
        use crate::eq_net::protocol::SpawnInfo;
        let mk = |npc: u8, name: &str| SpawnInfo {
            spawn_id: 7, name: name.into(), last_name: String::new(),
            level: 2, npc, gender: 0, race: 1, class_: 1, guild_id: 0xFFFF_FFFF, guild_rank: 0, body_type: 1,
            cur_hp: 100, helm: 0, show_helm: false, face: 0, hairstyle: 0, haircolor: 0,
            stand_state: 100, pet_owner_id: 0, player_state: 64,
            x: 0.0, y: 0.0, z: 0.0, heading: 0.0, animation: 100,
            equipment: [0u32; 9], equipment_tint: [[0u8; 3]; 9],
        };
        // A PC corpse (npc=2) arriving via the bulk zone-in path (which calls register_spawn directly,
        // NOT apply_new_spawn) must be flagged dead + Lying so the renderer lays it down — the earlier
        // fix only patched the single-spawn path, so zone-in corpses still stood in idle (#253).
        let mut gs = GameState::new();
        gs.player_name = "Someone".into();
        register_spawn(&mut gs, mk(2, "Aldric's corpse378"));
        let e = gs.entities.get(&7).unwrap();
        assert!(e.dead, "pc corpse (npc=2) must be flagged dead");
        assert_eq!(e.animation, 115, "corpse uses the Lying animation → scene picks the D05 dead clip");
        assert_eq!(e.hp_pct, 0.0, "a corpse is at 0 hp");

        // An NPC corpse (npc=3) likewise.
        let mut gs = GameState::new();
        register_spawn(&mut gs, mk(3, "a_rat_corpse"));
        assert!(gs.entities.get(&7).unwrap().dead, "npc corpse (npc=3) must be flagged dead");

        // A LIVING npc (npc=1) is untouched: not dead, keeps its stand_state animation and hp.
        let mut gs = GameState::new();
        register_spawn(&mut gs, mk(1, "a rat"));
        let e = gs.entities.get(&7).unwrap();
        assert!(!e.dead, "a living spawn must not be flagged dead");
        assert_eq!(e.animation, 100);
        assert_eq!(e.hp_pct, 100.0);
    }

    #[test]
    fn apply_wear_change_updates_player_when_spawn_is_player() {
        use super::apply_wear_change;
        let mut gs = GameState::new();
        gs.player_id = 7; // local player's spawn id
        // spawn_id=7 (player), material=17, color B,G,R=(1,2,3), wear_slot_id=1 (chest)
        let pkt = [7u8, 0, 17, 0, 1, 2, 3, 0xFF, 1];
        apply_wear_change(&mut gs, &pkt);
        assert_eq!(gs.player_equipment[1], 17);
        assert_eq!(gs.player_equipment_tint[1], [3, 2, 1]); // stored RGB
        assert!(gs.entities.is_empty(), "player must not be added to entities");
    }

    // --- decode/encode position round-trip: NPC-relevant edge cases ---

    #[test]
    fn position_roundtrip_negative_z() {
        use crate::eq_net::protocol::{decode_position_update, encode_position_update};
        let pkt = encode_position_update(42, 100.0, 200.0, -15.5, 0.0);
        let d = decode_position_update(&pkt).expect("decode negative z");
        assert_eq!(d.spawn_id, 42);
        assert!((d.x - 100.0).abs() < 0.2);
        assert!((d.y - 200.0).abs() < 0.2);
        assert!((d.z - (-15.5)).abs() < 0.2);
    }

    #[test]
    fn position_roundtrip_heading_near_360() {
        use crate::eq_net::protocol::{decode_position_update, encode_position_update};
        // A heading near a full circle should survive encode/decode (CCW convention).
        let pkt = encode_position_update(7, -250.0, 80.0, 3.0, 359.0);
        let d = decode_position_update(&pkt).expect("decode heading near 360");
        assert_eq!(d.spawn_id, 7);
        assert!((d.x - (-250.0)).abs() < 0.2);
        assert!((d.y - 80.0).abs() < 0.2);
        // 359° wraps to ~0 within wire quantization; accept either end of the circle.
        let dh = (d.heading - 359.0).rem_euclid(360.0);
        assert!(dh < 1.0 || dh > 359.0, "heading={}", d.heading);
    }

    #[test]
    fn player_profile_parses_equipment() {
        use super::apply_player_profile;
        let mut gs = GameState::new();
        // RoF2 PlayerProfile equipment offsets (rof2.cpp ENCODE(OP_PlayerProfile)):
        //   equipment[1] (chest) Material @204 = @184 + 1*20
        //   item_tint[1] (chest) Color @816 = @812 + 1*4, wire: B,G,R,UseTint
        let mut buf = vec![0u8; 5000];
        buf[204..208].copy_from_slice(&17u32.to_le_bytes()); // chest material
        buf[816] = 3; buf[817] = 2; buf[818] = 1; buf[819] = 0xFF; // B,G,R,UseTint
        apply_player_profile(&mut gs, &buf);
        assert_eq!(gs.player_equipment[1], 17);
        assert_eq!(gs.player_equipment_tint[1], [1, 2, 3]); // stored RGB
    }

    #[test]
    fn register_spawn_parses_equipment_le() {
        use crate::eq_net::protocol::SpawnInfo;
        use super::register_spawn;
        let mut gs = GameState::new();
        gs.player_name = "Someone Else".into();
        let mut equipment = [0u32; 9];
        equipment[1] = 17; // chest material
        let mut equipment_tint = [[0u8; 3]; 9];
        equipment_tint[1] = [30, 20, 10]; // RGB (already in RGB order for SpawnInfo)
        let info = SpawnInfo {
            spawn_id: 7, name: "Orc".into(), last_name: String::new(),
            level: 10, npc: 1, gender: 1, race: 54, class_: 1, guild_id: 0xFFFF_FFFF, guild_rank: 0, body_type: 1,
            cur_hp: 100, helm: 0, show_helm: false, face: 0, hairstyle: 0, haircolor: 0, stand_state: 100,
            pet_owner_id: 0, player_state: 64,
            x: 0.0, y: 0.0, z: 0.0, heading: 0.0, animation: 100,
            equipment, equipment_tint,
        };
        register_spawn(&mut gs, info);
        let e = gs.entities.get(&7).expect("entity registered");
        assert_eq!(e.equipment[1], 17);
        assert_eq!(e.equipment_tint[1], [30, 20, 10]); // stored RGB
        assert_eq!(e.gender, 1);
    }

    #[test]
    fn begin_cast_sets_casting_state() {
        // The player's OWN cast (caster_id == player_id) sets the cast bar. RoF2 10-byte layout.
        let mut gs = crate::game_state::GameState::new(); // player_id defaults to 0
        let mut b = [0u8; 10];
        b[0..4].copy_from_slice(&200u32.to_le_bytes());               // spell_id
        b[4..6].copy_from_slice(&(gs.player_id as u16).to_le_bytes()); // caster = self
        b[6..10].copy_from_slice(&3000u32.to_le_bytes());            // cast_time ms
        super::apply_begin_cast(&mut gs, &b.to_vec());
        let c = gs.casting.as_ref().expect("casting set");
        assert_eq!(c.spell_id, 200);
        assert_eq!(c.cast_ms, 3000);
    }

    #[test]
    fn parse_begin_cast_reads_fields() {
        // RoF2 BeginCast_Struct: spell_id u32@0, caster_id u16@4, cast_time u32@6 (10 bytes).
        let mut b = [0u8; 10];
        b[0..4].copy_from_slice(&200u32.to_le_bytes());  // spell_id
        b[4..6].copy_from_slice(&55u16.to_le_bytes());   // caster_id
        b[6..10].copy_from_slice(&3500u32.to_le_bytes()); // cast_time ms
        assert_eq!(parse_begin_cast(&b), Some((55, 200, 3500))); // (caster_id, spell_id, cast_ms)
        assert_eq!(parse_begin_cast(&[0u8; 8]), None); // 8 bytes is too short for the 10-byte struct
    }

    #[test]
    fn apply_begin_cast_only_paints_player_own_cast() {
        // eqoxide#222: an NPC's OP_BeginCast must NOT set the player's cast bar (which would disable
        // the player's spellcasting and never clear). Only the player's own cast (caster_id ==
        // player_id) drives gs.casting.
        let mut gs = GameState::new();
        gs.player_id = 42;
        let begin = |caster: u16, spell: u32, ms: u32| {
            let mut b = [0u8; 10];
            b[0..4].copy_from_slice(&spell.to_le_bytes());
            b[4..6].copy_from_slice(&caster.to_le_bytes());
            b[6..10].copy_from_slice(&ms.to_le_bytes());
            b
        };
        // NPC (caster 999) casting → ignored, no phantom cast bar.
        apply_begin_cast(&mut gs, &begin(999, 500, 3000));
        assert!(gs.casting.is_none(), "an NPC's cast must not set the player's cast bar");
        // The player's own cast (caster 42) → cast bar set with the real cast time (not garbage).
        apply_begin_cast(&mut gs, &begin(42, 700, 2500));
        let cs = gs.casting.as_ref().expect("player's own cast sets the bar");
        assert_eq!(cs.spell_id, 700);
        assert_eq!(cs.cast_ms, 2500);
    }

    #[test]
    fn parse_memorize_spell_reads_slot_and_scribing() {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&2u32.to_le_bytes());   // slot
        b[4..8].copy_from_slice(&200u32.to_le_bytes()); // spell_id
        b[8..12].copy_from_slice(&3u32.to_le_bytes());  // scribing = spellbar re-enable
        assert_eq!(parse_memorize_spell(&b), Some((2, 200, 3)));
    }

    #[test]
    fn zone_points_drop_sentinel_entries() {
        use super::apply_zone_points;
        use crate::eq_net::protocol::SIZE_ZONE_POINT_ENTRY;
        // Build one 32-byte RoF2 ZonePoint_Entry: iterator@0, y@4, x@8, z@12, heading@16,
        // zoneid(u16)@20, zoneinstance@22, then two trailing u32s.
        let entry = |iter: u32, y: f32, x: f32, z: f32, zoneid: u16| -> Vec<u8> {
            let mut b = vec![0u8; SIZE_ZONE_POINT_ENTRY];
            b[0..4].copy_from_slice(&iter.to_le_bytes());
            b[4..8].copy_from_slice(&y.to_le_bytes());
            b[8..12].copy_from_slice(&x.to_le_bytes());
            b[12..16].copy_from_slice(&z.to_le_bytes());
            b[16..20].copy_from_slice(&0f32.to_le_bytes());
            b[20..22].copy_from_slice(&zoneid.to_le_bytes());
            b
        };
        let mut payload = Vec::new();
        payload.extend(entry(0, 200.0, 100.0, -7.0, 2));       // real qeynos2 line
        payload.extend(entry(1, -350.0, 999999.0, 0.0, 2));    // sentinel garbage (x=999999)
        payload.extend(entry(2, 1395.0, 734.5, 4.0, 2));       // another real line

        let mut gs = GameState::new();
        apply_zone_points(&mut gs, &payload);
        assert_eq!(gs.zone_points.len(), 2, "the sentinel (x=999999) entry must be dropped");
        assert!(gs.zone_points.iter().all(|zp| zp.server_x.abs() < 900_000.0),
            "no sentinel coordinate survives");
        assert!(gs.zone_points.iter().any(|zp| (zp.server_x - 100.0).abs() < 0.5), "kept the real lines");
    }

    #[test]
    fn spawn_door_parses_one_record() {
        use super::apply_spawn_doors;
        let mut rec = [0u8; 100];                  // RoF2 Door_Struct is 100 bytes
        rec[..5].copy_from_slice(b"DOOR1");        // name @0
        rec[32..36].copy_from_slice(&20.0f32.to_le_bytes()); // yPos(north) @32
        rec[36..40].copy_from_slice(&10.0f32.to_le_bytes()); // xPos(east)  @36
        rec[40..44].copy_from_slice(&5.0f32.to_le_bytes());  // zPos @40
        rec[44..48].copy_from_slice(&128.0f32.to_le_bytes());// heading @44
        rec[48..52].copy_from_slice(&0u32.to_le_bytes());    // incline @48
        rec[52..54].copy_from_slice(&100u16.to_le_bytes());  // size @52
        rec[60] = 7;   // door_id @60
        rec[61] = 5;   // opentype @61
        rec[62] = 0;   // state_at_spawn @62 (closed)
        rec[63] = 0;   // invert_state @63
        rec[64..68].copy_from_slice(&0u32.to_le_bytes());    // door_param @64

        let mut gs = GameState::new();
        apply_spawn_doors(&mut gs, &rec);

        let d = gs.doors.get(&7).expect("door 7 present");
        assert_eq!(d.name, "DOOR1");
        assert_eq!(d.x, 10.0);   // east  <- xPos
        assert_eq!(d.y, 20.0);   // north <- yPos
        assert_eq!(d.z, 5.0);
        assert_eq!(d.heading, 128.0);
        assert_eq!(d.opentype, 5);
        assert!(!d.is_open);
        assert!(!d.invert_state);
    }

    #[test]
    fn spawn_door_parses_multiple_records_without_drift() {
        // RoF2 sends 100-byte Door_Struct records (the server's 80-byte struct is ENCODE-
        // expanded to 100 for the client). Parsing with the wrong stride drifts each record
        // after the first, so the 2nd+ doors decode garbage/empty names. Two records guard it.
        use super::apply_spawn_doors;
        const REC: usize = 100;
        let build = |name: &[u8], door_id: u8| -> [u8; REC] {
            let mut r = [0u8; REC];
            r[..name.len()].copy_from_slice(name); // name @0
            r[60] = door_id;                        // door_id @60
            r[61] = 5;                              // opentype @61
            r
        };
        let mut p = Vec::new();
        p.extend_from_slice(&build(b"DOORONE", 7));
        p.extend_from_slice(&build(b"DOORTWO", 9));

        let mut gs = GameState::new();
        apply_spawn_doors(&mut gs, &p);

        assert_eq!(gs.doors.get(&7).expect("door 7 present").name, "DOORONE");
        assert_eq!(gs.doors.get(&9).expect("door 9 present").name, "DOORTWO");
        assert_eq!(gs.doors.len(), 2, "exactly two doors, no phantom drifted records");
    }

    #[test]
    fn move_door_open_close_with_invert() {
        use super::apply_move_door;
        let mut gs = GameState::new();
        // normal door (invert_state = false)
        gs.upsert_door(crate::game_state::Door {
            door_id: 1, name: "D".into(), x:0.0,y:0.0,z:0.0,heading:0.0,incline:0,size:100,
            opentype:5, door_param:0, invert_state:false, is_open:false,
        });
        apply_move_door(&mut gs, &[1, 0x02]); // action 0x02 = open
        assert!(gs.doors.get(&1).unwrap().is_open);
        apply_move_door(&mut gs, &[1, 0x03]); // action 0x03 = close
        assert!(!gs.doors.get(&1).unwrap().is_open);

        // inverted door: action 0x02 means "close", 0x03 means "open"
        gs.upsert_door(crate::game_state::Door {
            door_id: 2, name: "D".into(), x:0.0,y:0.0,z:0.0,heading:0.0,incline:0,size:100,
            opentype:5, door_param:0, invert_state:true, is_open:true,
        });
        apply_move_door(&mut gs, &[2, 0x02]);
        assert!(!gs.doors.get(&2).unwrap().is_open);
        apply_move_door(&mut gs, &[2, 0x03]);
        assert!(gs.doors.get(&2).unwrap().is_open);
    }

    // ── Phase 2b: OP_ZoneEntry registers spawns + PlayerProfile identity ─────

    /// Reuse the NPC spawn buffer builder from protocol.rs tests via the public
    /// parse_rof2_spawn + register_spawn pipeline. This is the canonical RoF2 test:
    /// each OP_ZoneEntry carries one Spawn_Struct, and every such packet must land
    /// in gs.entities (or update the player, if name matches).
    #[test]
    fn zone_entry_registers_npc_spawn() {
        use crate::eq_net::protocol::parse_rof2_spawn;
        use super::apply_zone_entry;

        // Build a minimal NPC spawn buffer (same as protocol.rs helper).
        fn build_npc_buf(name: &str, id: u32, x: f32, y: f32, z: f32) -> Vec<u8> {
            let mut b = Vec::new();
            b.extend_from_slice(name.as_bytes()); b.push(0);
            b.extend_from_slice(&id.to_le_bytes());
            b.push(10); // level
            b.extend_from_slice(&5.0f32.to_le_bytes()); // bounding
            b.push(1); // NPC=1
            b.extend_from_slice(&0u32.to_le_bytes()); // bitfields
            b.push(0); // OtherData
            b.extend_from_slice(&0.0f32.to_le_bytes()); // unk3
            b.extend_from_slice(&0.0f32.to_le_bytes()); // unk4
            b.push(1); b.extend_from_slice(&1u32.to_le_bytes()); // props_count=1, bodytype=1
            b.push(100); // curHp
            b.extend_from_slice(&[0u8; 6]); // hair..beard
            b.extend_from_slice(&[0u8; 12]); // drakkin
            b.extend_from_slice(&[0, 0, 0, 0]); // equip_chest2..helm
            b.extend_from_slice(&6.0f32.to_le_bytes()); // size
            b.push(0); // face
            b.extend_from_slice(&0.35f32.to_le_bytes()); // walkspeed
            b.extend_from_slice(&0.7f32.to_le_bytes());  // runspeed
            b.extend_from_slice(&54u32.to_le_bytes()); // race=54 (gnoll — non-playable, >12)
            b.push(0); b.extend_from_slice(&[0u8; 12]); // holding, deity, guild, rank
            b.extend_from_slice(&[1, 0, 100, 0, 0]); // class_, pvp, StandState, light, fly
            b.push(0); // lastName\0
            b.extend_from_slice(&[0u8; 6]); // aatitle, guild_show, TempPet
            b.extend_from_slice(&0u32.to_le_bytes()); // petOwnerId
            b.push(0); // FindBits
            b.extend_from_slice(&64u32.to_le_bytes()); // PlayerState
            b.extend_from_slice(&[0u8; 20]); // NpcTintIndex..unk (5×u32)
            // Non-playable equipment block (60 bytes): 5×u32 zeros, Primary.Material, 4×u32, Secondary.Material, 4×u32
            b.extend_from_slice(&[0u8; 60]);
            // Position (20 bytes): encode x/y/z at correct bit positions
            let yp = ((y * 8.0) as i32 as u32) & 0x7FFFF;
            let xp = ((x * 8.0) as i32 as u32) & 0x7FFFF;
            let zp = ((z * 8.0) as i32 as u32) & 0x7FFFF;
            b.extend_from_slice(&(yp << 12).to_le_bytes()); // word0: y
            b.extend_from_slice(&0u32.to_le_bytes());        // word1: deltas
            b.extend_from_slice(&xp.to_le_bytes());          // word2: x
            b.extend_from_slice(&(zp << 10).to_le_bytes());  // word3: z
            b.extend_from_slice(&100u32.to_le_bytes());       // word4: animation=100
            b.extend_from_slice(&[0u8; 8]); // unknown20
            b.push(0); // IsMercenary
            b.extend_from_slice(b"0000000000000000\0"); // RealEstateItemGuid (17B)
            b.extend_from_slice(&0xffffffffu32.to_le_bytes()); // RealEstateID
            b.extend_from_slice(&0xffffffffu32.to_le_bytes()); // RealEstateItemID
            b.extend_from_slice(&[0u8; 29]); // padding
            b
        }

        let buf = build_npc_buf("a_gnoll", 77, 150.0, -100.0, 5.0);

        // 1. parse_rof2_spawn extracts sane values
        let (info, consumed) = parse_rof2_spawn(&buf).expect("parse must succeed");
        assert_eq!(consumed, buf.len());
        assert_eq!(info.name, "a_gnoll");
        assert_eq!(info.spawn_id, 77);
        assert!((info.x - 150.0).abs() < 0.2, "x={}", info.x);
        assert!((info.y - (-100.0)).abs() < 0.2, "y={}", info.y);
        assert!((info.z - 5.0).abs() < 0.2, "z={}", info.z);

        // 2. apply_zone_entry routes it to register_spawn → appears in entities
        let mut gs = GameState::new();
        gs.player_name = "Someone_Else".into(); // make sure it's not mistaken for player
        apply_zone_entry(&mut gs, &buf);
        let e = gs.entities.get(&77).expect("NPC must be in entities after OP_ZoneEntry");
        assert_eq!(e.name, "a_gnoll");
        assert!((e.x - 150.0).abs() < 0.2);
        assert!((e.y - (-100.0)).abs() < 0.2);
    }

    /// A corpse that arrives as a fresh spawn (npc: 2=pc_corpse, 3=npc_corpse) never goes through
    /// `apply_death`, so `apply_new_spawn` must mark it dead + Lying (animation 115) or the scene
    /// renderer shows it standing in an idle pose (#118). A live NPC (npc=1) must stay upright.
    #[test]
    fn corpse_spawn_marked_dead_and_lying() {
        use super::apply_new_spawn;

        // Minimal RoF2 spawn buffer with a settable NPC/corpse type byte (mirrors build_npc_buf).
        fn build_spawn(name: &str, id: u32, npc: u8) -> Vec<u8> {
            let mut b = Vec::new();
            b.extend_from_slice(name.as_bytes()); b.push(0);
            b.extend_from_slice(&id.to_le_bytes());
            b.push(10); // level
            b.extend_from_slice(&5.0f32.to_le_bytes()); // bounding
            b.push(npc); // NPC type: 0=pc, 1=npc, 2=pc_corpse, 3=npc_corpse
            b.extend_from_slice(&0u32.to_le_bytes()); // bitfields
            b.push(0); // OtherData
            b.extend_from_slice(&0.0f32.to_le_bytes()); // unk3
            b.extend_from_slice(&0.0f32.to_le_bytes()); // unk4
            b.push(1); b.extend_from_slice(&1u32.to_le_bytes()); // props_count=1, bodytype=1
            b.push(0); // curHp — a corpse is at 0%
            b.extend_from_slice(&[0u8; 6]);  // hair..beard
            b.extend_from_slice(&[0u8; 12]); // drakkin
            b.extend_from_slice(&[0, 0, 0, 0]); // equip_chest2..helm
            b.extend_from_slice(&6.0f32.to_le_bytes()); // size
            b.push(0); // face
            b.extend_from_slice(&0.35f32.to_le_bytes()); // walkspeed
            b.extend_from_slice(&0.7f32.to_le_bytes());  // runspeed
            b.extend_from_slice(&54u32.to_le_bytes()); // race
            b.push(0); b.extend_from_slice(&[0u8; 12]); // holding, deity, guild, rank
            b.extend_from_slice(&[1, 0, 100, 0, 0]); // class_, pvp, StandState, light, fly
            b.push(0); // lastName\0
            b.extend_from_slice(&[0u8; 6]); // aatitle, guild_show, TempPet
            b.extend_from_slice(&0u32.to_le_bytes()); // petOwnerId
            b.push(0); // FindBits
            b.extend_from_slice(&64u32.to_le_bytes()); // PlayerState
            b.extend_from_slice(&[0u8; 20]); // NpcTintIndex..unk
            b.extend_from_slice(&[0u8; 60]); // non-playable equipment block
            b.extend_from_slice(&0u32.to_le_bytes()); // pos word0
            b.extend_from_slice(&0u32.to_le_bytes()); // pos word1
            b.extend_from_slice(&0u32.to_le_bytes()); // pos word2
            b.extend_from_slice(&0u32.to_le_bytes()); // pos word3
            b.extend_from_slice(&100u32.to_le_bytes()); // word4: animation=100 (standing)
            b.extend_from_slice(&[0u8; 8]); // unknown20
            b.push(0); // IsMercenary
            b.extend_from_slice(b"0000000000000000\0"); // RealEstateItemGuid (17B)
            b.extend_from_slice(&0xffffffffu32.to_le_bytes()); // RealEstateID
            b.extend_from_slice(&0xffffffffu32.to_le_bytes()); // RealEstateItemID
            b.extend_from_slice(&[0u8; 29]); // padding
            b
        }

        // NPC corpse (npc=3) → dead + Lying.
        let mut gs = GameState::new();
        gs.player_name = "Someone_Else".into();
        apply_new_spawn(&mut gs, &build_spawn("a_gnoll_corpse", 77, 3));
        let e = gs.entities.get(&77).expect("npc corpse in entities");
        assert!(e.dead, "npc corpse must be marked dead");
        assert_eq!(e.animation, 115, "npc corpse uses the Lying clip");
        assert_eq!(e.hp_pct, 0.0);

        // PC corpse (npc=2) → another player's corpse lies down too (not auto-looted, but dead).
        let mut gs2 = GameState::new();
        gs2.player_name = "Someone_Else".into();
        apply_new_spawn(&mut gs2, &build_spawn("Aldric`s corpse", 88, 2));
        let e2 = gs2.entities.get(&88).expect("pc corpse in entities");
        assert!(e2.dead, "pc corpse must be marked dead");
        assert_eq!(e2.animation, 115);

        // A live NPC (npc=1) must NOT be marked dead.
        let mut gs3 = GameState::new();
        gs3.player_name = "Someone_Else".into();
        apply_new_spawn(&mut gs3, &build_spawn("a_gnoll", 99, 1));
        let e3 = gs3.entities.get(&99).expect("live npc in entities");
        assert!(!e3.dead, "a live npc must not be marked dead");
        assert_eq!(e3.animation, 100, "a live npc keeps its standing animation");
    }

    #[test]
    fn zone_entry_updates_player_when_name_matches() {
        use super::apply_zone_entry;

        fn build_npc_buf(name: &str, id: u32, x: f32, y: f32, z: f32) -> Vec<u8> {
            let mut b = Vec::new();
            b.extend_from_slice(name.as_bytes()); b.push(0);
            b.extend_from_slice(&id.to_le_bytes());
            b.push(10); b.extend_from_slice(&5.0f32.to_le_bytes()); b.push(0); // level, bounding, NPC=0 (PC)
            b.extend_from_slice(&0u32.to_le_bytes()); // bitfields (gender=0)
            b.push(0); // OtherData
            b.extend_from_slice(&0.0f32.to_le_bytes()); b.extend_from_slice(&0.0f32.to_le_bytes()); // unk3,unk4
            b.push(1); b.extend_from_slice(&1u32.to_le_bytes()); // props_count=1, bodytype
            b.push(100); b.extend_from_slice(&[0u8; 6]); // curHp, hair..beard
            b.extend_from_slice(&[0u8; 12]); // drakkin
            b.extend_from_slice(&[0u8; 4]); // equip_chest2..helm
            b.extend_from_slice(&6.0f32.to_le_bytes()); b.push(0); // size, face
            b.extend_from_slice(&0.35f32.to_le_bytes()); b.extend_from_slice(&0.7f32.to_le_bytes()); // speeds
            b.extend_from_slice(&1u32.to_le_bytes()); // race=1 HUM (playable)
            b.push(0); b.extend_from_slice(&[0u8; 12]); // holding, deity, guild, rank
            b.extend_from_slice(&[1, 0, 100, 0, 0]); // class_, pvp, StandState, light, fly
            b.push(0); // lastName\0
            b.extend_from_slice(&[0u8; 6]); // aatitle, guild_show, TempPet
            b.extend_from_slice(&0u32.to_le_bytes()); // petOwnerId
            b.push(0); b.extend_from_slice(&64u32.to_le_bytes()); // FindBits, PlayerState
            b.extend_from_slice(&[0u8; 20]); // 5×u32 tint indices
            // Playable race → TintProfile(36) + Equipment(180) = 216 bytes
            b.extend_from_slice(&[0u8; 36]); // TintProfile
            b.extend_from_slice(&[0u8; 180]); // Equipment
            // Position (20 bytes)
            let yp = ((y * 8.0) as i32 as u32) & 0x7FFFF;
            let xp = ((x * 8.0) as i32 as u32) & 0x7FFFF;
            let zp = ((z * 8.0) as i32 as u32) & 0x7FFFF;
            b.extend_from_slice(&(yp << 12).to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&xp.to_le_bytes());
            b.extend_from_slice(&(zp << 10).to_le_bytes());
            b.extend_from_slice(&100u32.to_le_bytes());
            // Tail
            b.extend_from_slice(&[0u8; 8]); b.push(0);
            b.extend_from_slice(b"0000000000000000\0");
            b.extend_from_slice(&0xffffffffu32.to_le_bytes());
            b.extend_from_slice(&0xffffffffu32.to_le_bytes());
            b.extend_from_slice(&[0u8; 29]);
            b
        }

        let mut gs = GameState::new();
        gs.player_name = "Frodo".into();
        let buf = build_npc_buf("Frodo", 12, 200.0, -50.0, 10.0);
        apply_zone_entry(&mut gs, &buf);

        // Player self-spawn must NOT land in entities
        assert!(gs.entities.is_empty(), "player self-spawn must not be in entities");
        assert_eq!(gs.player_id, 12);
        assert!((gs.player_x - 200.0).abs() < 0.2);
        assert!((gs.player_y - (-50.0)).abs() < 0.2);
        assert!((gs.player_z - 10.0).abs() < 0.2);
    }

    #[test]
    fn player_profile_sets_class_and_race() {
        use super::apply_player_profile;
        let mut gs = GameState::new();
        // RoF2: class_ @21, level @22, gender @16, race @17
        let mut buf = vec![0u8; 1000];
        buf[16] = 1;   // gender = female
        buf[17..21].copy_from_slice(&4u32.to_le_bytes()); // race = 4 (wood elf)
        buf[21] = 4;   // class_ = 4 (Ranger)
        buf[22] = 35;  // level
        apply_player_profile(&mut gs, &buf);
        assert_eq!(gs.player_class, "Ranger");
        assert_eq!(gs.player_level, 35);
        assert_eq!(gs.player_gender, 1);
        assert_eq!(gs.player_race, "ELF");
    }

    #[test]
    fn player_profile_seeds_player_hp() {
        use super::apply_player_profile;
        // The server only sends a self OP_HPUpdate on HP *change*, so the profile must seed the
        // player's hp or it stays 0/0 at full health (eqoxide#19). cur_hp @948; no max in the
        // profile → seed max = cur (full at zone-in) → 100%.
        let mut gs = GameState::new();
        let mut buf = vec![0u8; 1000];
        buf[21] = 1;  // class (warrior) — len/offset sanity
        buf[22] = 10; // level
        buf[948..952].copy_from_slice(&850u32.to_le_bytes()); // cur_hp
        apply_player_profile(&mut gs, &buf);
        assert_eq!(gs.cur_hp, 850);
        assert_eq!(gs.max_hp, 850, "max seeded from cur (full at zone-in)");
        assert!((gs.hp_pct - 100.0).abs() < 1e-3, "full health = 100%, got {}", gs.hp_pct);

        // A real OP_HPUpdate supplies the true max; a later profile updates cur but must NOT
        // clobber the learned max (so the percent stays accurate).
        gs.update_hp(gs.player_id, 600, 1000); // player_id == 0 by default → player branch
        apply_player_profile(&mut gs, &buf);   // profile again: cur=850, max stays 1000
        assert_eq!(gs.max_hp, 1000, "must not overwrite a max learned from OP_HPUpdate");
        assert_eq!(gs.cur_hp, 850);
        assert!((gs.hp_pct - 85.0).abs() < 1e-3, "850/1000 = 85%, got {}", gs.hp_pct);
    }

    #[test]
    fn player_profile_seeds_mana_then_mana_change_tracks_it() {
        use super::{apply_player_profile, apply_mana_change};
        // Profile seeds full mana (cur@944, max = cur). OP_ManaChange then tracks current. (eqoxide#27)
        let mut gs = GameState::new();
        let mut buf = vec![0u8; 1000];
        buf[21] = 11; // necro
        buf[22] = 10;
        buf[944..948].copy_from_slice(&400u32.to_le_bytes()); // mana
        apply_player_profile(&mut gs, &buf);
        assert_eq!(gs.cur_mana, 400);
        assert_eq!(gs.max_mana, 400);
        assert!((gs.mana_pct - 100.0).abs() < 1e-3);

        // OP_ManaChange (ManaChange_Struct.new_mana @0) = spent to 150 → 150/400 = 37.5%.
        let mut mc = vec![0u8; 20];
        mc[0..4].copy_from_slice(&150u32.to_le_bytes());
        apply_mana_change(&mut gs, &mc);
        assert_eq!(gs.cur_mana, 150);
        assert_eq!(gs.max_mana, 400, "spend must not lower max");
        assert!((gs.mana_pct - 37.5).abs() < 1e-3, "150/400 = 37.5%, got {}", gs.mana_pct);
    }

    // ── RoF2 Animation_Struct byte-layout tests ──────────────────────────────────────────────────

    /// RoF2 Animation_Struct (rof2_structs.h):
    ///   /*00*/ uint16 spawnid
    ///   /*02*/ uint8  action   ← combat swing code (1–9)
    ///   /*03*/ uint8  speed
    /// Regression: old code read p[3] (speed) instead of p[2] (action), so combat anims never fired.
    #[test]
    fn apply_animation_reads_action_from_byte2_not_byte3() {
        use super::apply_animation;
        let mut gs = GameState::new();
        // Build a 4-byte Animation_Struct: spawnid=55 (LE u16), action=5, speed=50
        let pkt: [u8; 4] = [
            55, 0,   // spawnid = 55 (LE)
            5,       // action  = 5  (at byte 2)
            50,      // speed   = 50 (at byte 3) — must NOT be used as action
        ];
        apply_animation(&mut gs, &pkt);
        let entry = gs.combat_anims.get(&55).expect("combat anim must be recorded for spawnid=55");
        assert_eq!(entry.0, 5, "action code must be 5 (byte 2), not 50 (byte 3 speed)");
    }

    // ── OP_CharInventory (RoF2 binary) ──────────────────────────────────────────────────────────

    /// Build a valid OP_CharInventory payload: uint32 count + N item blobs concatenated.
    fn build_char_inventory(items: &[Vec<u8>]) -> Vec<u8> {
        let mut buf = (items.len() as u32).to_le_bytes().to_vec();
        for item in items { buf.extend_from_slice(item); }
        buf
    }

    #[test]
    fn apply_char_inventory_loads_two_items_at_correct_slots() {
        let mut gs = GameState::new();
        let payload = build_char_inventory(&[fixture(), fixture2()]);
        apply_char_inventory(&mut gs, &payload);
        // fixture()  → id=1001, main_slot=23 (RoF2 general slot 1)
        // fixture2() → id=2002, main_slot=24 (RoF2 general slot 2), same name
        assert_eq!(gs.inventory.len(), 2, "exactly two items must land in inventory");
        let item1 = gs.inventory.iter().find(|i| i.slot == 23).expect("item at slot 23");
        assert_eq!(item1.item_id, 1001);
        assert_eq!(item1.icon, 678);
        assert_eq!(item1.name, "Cloth Cap");
        let item2 = gs.inventory.iter().find(|i| i.slot == 24).expect("item at slot 24");
        assert_eq!(item2.item_id, 2002);
        assert_eq!(item2.icon, 999);
    }

    #[test]
    fn apply_char_inventory_ignores_zero_count() {
        let mut gs = GameState::new();
        // Push a dummy item so we can verify inventory is untouched
        gs.inventory.push(crate::game_state::InvItem {
            slot: 99, item_id: 1, name: "existing".into(), icon: 1, charges: 1, idfile: "IT1".into(),
            click_spell_id: 0, filename: String::new(),
        });
        let payload = 0u32.to_le_bytes().to_vec(); // count = 0
        apply_char_inventory(&mut gs, &payload);
        assert_eq!(gs.inventory.len(), 1, "zero-count packet must not clear inventory");
    }

    #[test]
    fn apply_char_inventory_upserts_by_slot() {
        // Second call with same slot should replace, not duplicate.
        let mut gs = GameState::new();
        let payload1 = build_char_inventory(&[fixture()]);
        apply_char_inventory(&mut gs, &payload1);
        assert_eq!(gs.inventory.len(), 1);
        // Send same slot again (fixture uses slot 23)
        let payload2 = build_char_inventory(&[fixture()]);
        apply_char_inventory(&mut gs, &payload2);
        assert_eq!(gs.inventory.len(), 1, "duplicate slot must upsert, not append");
    }

    /// Speed=50 is NOT in the valid action range 1..=9, so no anim should be recorded.
    /// If we had read p[3]=50 instead of p[2]=5, the combat_anim would never fire.
    #[test]
    fn apply_animation_speed_byte_does_not_trigger_anim() {
        use super::apply_animation;
        let mut gs = GameState::new();
        // action=0 (non-combat), speed=5 (in range 1..=9 — must NOT be used as action)
        let pkt: [u8; 4] = [10, 0, 0, 5];
        apply_animation(&mut gs, &pkt);
        assert!(gs.combat_anims.is_empty(), "non-combat action=0 must not create an anim entry");
    }

    // ── OP_TaskDescription / OP_CompletedTasks tests ──────────────────────────────────────────────

    #[test]
    fn extract_saylink_text_strips_link_markup() {
        // Real EQEmu format (say_link.cpp GenerateLink): one \x12-delimited segment holding a
        // fixed 56-char hex body immediately followed by the item name, closed by a second \x12.
        let body = "0".repeat(SAY_LINK_BODY_SIZE);
        let link = format!("\x12{body}Rusty Dagger\x12");
        assert_eq!(extract_saylink_text(&link), "Rusty Dagger");
    }

    #[test]
    fn extract_saylink_text_handles_short_body_without_panicking() {
        // Malformed/truncated link shorter than the fixed body size must not panic.
        assert_eq!(extract_saylink_text("\x12short\x12"), "");
    }

    #[test]
    fn extract_saylink_text_passes_through_plain_string() {
        assert_eq!(extract_saylink_text(""), "");
        assert_eq!(extract_saylink_text("not a link"), "not a link");
    }

    /// Build a 56-char saylink body exactly as EQEmu `say_link.cpp` `GenerateLinkBody` does, so the
    /// parser's field offsets are checked against the real wire format.
    fn mk_saylink_body(item_id: u32, aug1: u32, ornament: u32, hash: u32) -> String {
        let b = format!(
            "{:01x}{:05x}{:05x}{:05x}{:05x}{:05x}{:05x}{:05x}{:01x}{:04x}{:02x}{:05x}{:08x}",
            0, item_id, aug1, 0, 0, 0, 0, 0, 0, 0, 0, ornament, hash,
        );
        assert_eq!(b.len(), SAY_LINK_BODY_SIZE);
        b
    }

    #[test]
    fn parse_say_links_extracts_clickable_choice() {
        use super::{parse_say_links, SAYLINK_ITEM_ID};
        let body = mk_saylink_body(SAYLINK_ITEM_ID, 42, 7, 0xABCD);
        let msg = format!("Do you wish to \x12{body}bind your soul\x12 here?");
        let (text, choices) = parse_say_links(&msg);
        assert_eq!(text, "Do you wish to bind your soul here?");
        assert_eq!(choices.len(), 1);
        let c = &choices[0];
        assert_eq!(c.text, "bind your soul");
        assert_eq!(c.item_id, SAYLINK_ITEM_ID);
        assert_eq!(c.augments[0], 42, "sayid decoded from augment_1");
        assert_eq!(c.icon, 7);
        assert_eq!(c.link_hash, 0xABCD);
    }

    #[test]
    fn parse_say_links_plain_text_has_no_choices() {
        use super::parse_say_links;
        let (text, choices) = parse_say_links("just a greeting");
        assert_eq!(text, "just a greeting");
        assert!(choices.is_empty());
    }

    #[test]
    fn parse_say_links_ignores_non_saylink_item_links() {
        use super::parse_say_links;
        // A real item link (item_id != SAYLINK_ITEM_ID) keeps its display text but is not a choice.
        let body = mk_saylink_body(1001, 0, 0, 0);
        let (text, choices) = parse_say_links(&format!("\x12{body}Rusty Dagger\x12"));
        assert_eq!(text, "Rusty Dagger");
        assert!(choices.is_empty(), "item links are not dialogue choices");
    }

    #[test]
    fn special_message_populates_dialogue_choices() {
        use super::{apply_special_message, SAYLINK_ITEM_ID};
        // SpecialMesg: header[11] | sayer\0 | unknown[12] | message\0
        let mut p = vec![0u8; 11];
        p.extend_from_slice(b"Soulbinder\0");
        p.extend_from_slice(&[0u8; 12]);
        let body = mk_saylink_body(SAYLINK_ITEM_ID, 5, 0, 0);
        p.extend_from_slice(format!("Do you wish to \x12{body}bind your soul\x12?").as_bytes());

        let mut gs = GameState::new();
        apply_special_message(&mut gs, &p);
        assert_eq!(gs.dialogue_choices.len(), 1);
        assert_eq!(gs.dialogue_choices[0].text, "bind your soul");
        assert_eq!(gs.dialogue_choices[0].augments[0], 5);
        // The logged line shows clean text (no link markup).
        assert!(gs.messages.back().unwrap().text.contains("bind your soul"));
        assert!(!gs.messages.back().unwrap().text.contains('\u{12}'));
    }

    #[test]
    fn build_item_link_click_lays_out_struct() {
        use crate::eq_net::protocol::{build_item_link_click, OP_ITEM_LINK_CLICK};
        assert_eq!(OP_ITEM_LINK_CLICK, 0x4cef);
        let p = build_item_link_click(0xF_FFFF, &[42, 0, 0, 0, 0, 0], 0xABCD, 7);
        assert_eq!(p.len(), 52);
        assert_eq!(u32::from_le_bytes([p[0], p[1], p[2], p[3]]), 0xF_FFFF); // item_id
        assert_eq!(u32::from_le_bytes([p[4], p[5], p[6], p[7]]), 42);       // augments[0] = sayid
        assert_eq!(u32::from_le_bytes([p[28], p[29], p[30], p[31]]), 0xABCD); // link_hash
        assert_eq!(u32::from_le_bytes([p[32], p[33], p[34], p[35]]), 4);      // unknown028
        assert_eq!(u16::from_le_bytes([p[48], p[49]]), 7);                    // icon
    }

    fn build_task_description(seq: u32, task_id: u32, title: &str, desc: &str, coin: u32, xp: u32, reward_text: &str, item_link: &str) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&seq.to_le_bytes());
        p.extend_from_slice(&task_id.to_le_bytes());
        p.push(0); // open_window
        p.extend_from_slice(&0u32.to_le_bytes()); // task_type
        p.extend_from_slice(&0u32.to_le_bytes()); // reward_type
        p.extend_from_slice(title.as_bytes()); p.push(0);
        p.extend_from_slice(&0u32.to_le_bytes()); // duration
        p.extend_from_slice(&0u32.to_le_bytes()); // dur_code
        p.extend_from_slice(&0u32.to_le_bytes()); // start_time
        p.extend_from_slice(desc.as_bytes()); p.push(0);
        p.push(1); // has_rewards
        p.extend_from_slice(&coin.to_le_bytes());
        p.extend_from_slice(&xp.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes()); // faction
        p.extend_from_slice(reward_text.as_bytes()); p.push(0);
        p.extend_from_slice(item_link.as_bytes()); p.push(0);
        p
    }

    #[test]
    fn apply_task_description_parses_reward_item_and_sequence_number() {
        let mut gs = GameState::new();
        let body = "0".repeat(SAY_LINK_BODY_SIZE);
        let item_link = format!("\x12{body}Rusty Dagger\x12");
        let p = build_task_description(3, 500, "Kill Rats", "Kill 5 rats", 10, 200, "reward!", &item_link);
        apply_task_description(&mut gs, &p);
        let task = gs.tasks.get(&500).expect("task inserted");
        assert_eq!(task.sequence_number, 3);
        assert_eq!(task.title, "Kill Rats");
        assert_eq!(task.coin_reward, 10);
        assert_eq!(task.xp_reward, 200);
        assert_eq!(task.reward_item_text, "Rusty Dagger");
        assert_eq!(task.status, TaskStatus::Active);
    }

    fn build_completed_tasks(entries: &[(u32, &str, u32)]) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for (id, title, time) in entries {
            p.extend_from_slice(&id.to_le_bytes());
            p.extend_from_slice(title.as_bytes()); p.push(0);
            p.extend_from_slice(&time.to_le_bytes());
        }
        p
    }

    #[test]
    fn apply_completed_tasks_parses_title_and_flips_status() {
        let mut gs = GameState::new();
        // Task 500 was already in the journal (arrived via OP_TaskDescription earlier).
        gs.tasks.insert(500, crate::game_state::ActiveTask {
            task_id: 500, title: "Kill Rats".into(), status: TaskStatus::Active, ..Default::default()
        });
        let p = build_completed_tasks(&[(500, "Kill Rats", 1_700_000_000), (501, "Deliver Note", 1_700_000_100)]);
        apply_completed_tasks(&mut gs, &p);

        assert_eq!(gs.tasks.get(&500).unwrap().status, TaskStatus::Completed);
        // Task 501 was never seen via OP_TaskDescription — inserted as a stub, still flipped.
        let stub = gs.tasks.get(&501).expect("stub inserted for unseen completed task");
        assert_eq!(stub.status, TaskStatus::Completed);
        assert_eq!(stub.title, "Deliver Note");

        assert_eq!(gs.completed_task_history.len(), 2);
        assert_eq!(gs.completed_task_history[0].title, "Kill Rats");
        assert_eq!(gs.completed_task_history[0].completed_time, 1_700_000_000);
        assert_eq!(gs.completed_task_history[1].task_id, 501);
    }

    #[test]
    fn apply_completed_tasks_handles_truncated_packet_without_hanging() {
        let mut gs = GameState::new();
        // count says 5 entries but the buffer only has the count field — must not loop forever
        // or panic; rd_u32/rd_cstr degrade to 0/empty on out-of-bounds reads.
        let p = 5u32.to_le_bytes().to_vec();
        apply_completed_tasks(&mut gs, &p);
        assert!(gs.completed_task_history.is_empty());
    }

    fn build_task_select_window(task_giver: u32, tasks: &[(u32, &str, &str, bool, u32)]) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&(tasks.len() as u32).to_le_bytes()); // task_count
        p.extend_from_slice(&2u32.to_le_bytes());                 // type = Quest
        p.extend_from_slice(&task_giver.to_le_bytes());
        for (task_id, title, desc, has_rewards, element_count) in tasks {
            p.extend_from_slice(&task_id.to_le_bytes());
            p.extend_from_slice(&0f32.to_le_bytes());   // reward_multiplier
            p.extend_from_slice(&0u32.to_le_bytes());   // duration
            p.extend_from_slice(&0u32.to_le_bytes());   // duration_code
            p.extend_from_slice(title.as_bytes()); p.push(0);
            p.extend_from_slice(desc.as_bytes()); p.push(0);
            p.push(if *has_rewards { 1 } else { 0 });
            p.extend_from_slice(&element_count.to_le_bytes());
        }
        p
    }

    #[test]
    fn apply_task_select_window_parses_offers() {
        let mut gs = GameState::new();
        let p = build_task_select_window(9001, &[
            (10, "Offer One", "Do a thing", true, 0),
            (11, "Offer Two", "Do another thing", false, 0),
        ]);
        apply_task_select_window(&mut gs, &p);
        assert_eq!(gs.task_offers.len(), 2);
        assert_eq!(gs.task_offers[0].task_id, 10);
        assert_eq!(gs.task_offers[0].npc_id, 9001);
        assert_eq!(gs.task_offers[0].title, "Offer One");
        assert!(gs.task_offers[0].has_rewards);
        assert!(!gs.task_offers[1].has_rewards);
    }

    #[test]
    fn apply_task_select_window_bails_out_on_nonzero_element_count() {
        let mut gs = GameState::new();
        let p = build_task_select_window(9001, &[
            (10, "Offer One", "Do a thing", true, 2), // unmodeled nested elements
        ]);
        apply_task_select_window(&mut gs, &p);
        assert!(gs.task_offers.is_empty(), "must not guess at the nested ActivityInformation layout");
    }

    #[test]
    fn apply_task_select_window_handles_truncated_packet_without_hanging() {
        let mut gs = GameState::new();
        // task_count says 100000 entries but the buffer only has the count field — must not hang
        // or panic; rd_u32/rd_cstr degrade to 0/empty on out-of-bounds reads.
        let p = 100000u32.to_le_bytes().to_vec();
        apply_task_select_window(&mut gs, &p);
        assert!(gs.task_offers.is_empty());
    }

    // ── RoF2 Death_Struct byte-layout tests ─────────────────────────────────────────────────────

    /// RoF2 / eq_packet_structs.h Death_Struct (no ENCODE in rof2.cpp — wire is server's layout):
    ///   /*000*/ uint32 spawn_id    ← the dying entity's id
    ///   /*004*/ uint32 killer_id
    ///   ... (32 bytes total)
    /// The handler must: (a) mark the correct entity dead, (b) set animation=115 (Lying).
    #[test]
    fn apply_death_marks_npc_dead_and_sets_lying_animation() {
        use super::apply_death;
        use crate::game_state::Entity;
        let mut gs = GameState::new();
        gs.player_id = 1;
        // Register an NPC entity with id=42
        gs.entities.insert(42, Entity {
            spawn_id: 42, name: "Orc Pawn".into(),
            x: 0.0, y: 0.0, z: 0.0, heading: 0.0, animation: 100, floating: false,
            level: 5, is_npc: true, gender: 0, race: "ORC".into(),
            cur_hp: 50, max_hp: 100, hp_pct: 50.0,
            dead: false,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9],
            helm: 0, showhelm: 0,
            face: 0, hairstyle: 0, haircolor: 0,
        });
        // Build a 32-byte Death_Struct payload: spawn_id=42, killer_id=1 (player)
        let mut pkt = [0u8; 32];
        pkt[0..4].copy_from_slice(&42u32.to_le_bytes());  // spawn_id  (bytes 0-3)
        pkt[4..8].copy_from_slice(&1u32.to_le_bytes());   // killer_id (bytes 4-7)
        apply_death(&mut gs, &pkt);
        let e = gs.entities.get(&42).expect("entity must remain in map after death");
        assert!(e.dead, "entity must be marked dead");
        assert_eq!(e.hp_pct, 0.0, "hp_pct must be zeroed");
        assert_eq!(e.animation, 115, "animation must be set to 115 (Lying) for dead clip");
        // Auto-loot queued for player's own kill
        assert!(gs.pending_loot.contains(&42), "corpse id must be queued for auto-loot");
    }

    #[test]
    fn player_death_sets_dead_flag_and_revive_clears_it() {
        // eqoxide#61: the nav walker keys on gs.player_dead to abandon a stale /goto.
        use super::apply_death;
        let mut gs = GameState::new();
        gs.player_id = 7;
        gs.player_dead = false;
        // Death_Struct: spawn_id@0 = the player, killer_id@4.
        let mut pkt = [0u8; 32];
        pkt[0..4].copy_from_slice(&7u32.to_le_bytes());
        pkt[4..8].copy_from_slice(&99u32.to_le_bytes());
        apply_death(&mut gs, &pkt);
        assert!(gs.player_dead, "player death must set player_dead");
        assert_eq!(gs.hp_pct, 0.0);
        // Revive / heal above 0 clears it (respawn also clears via the profile HP seed).
        gs.update_hp(7, 40, 100);
        assert!(!gs.player_dead, "restoring HP must clear player_dead");
    }

    #[test]
    fn bind_respawn_restores_full_hp_and_position() {
        // eqoxide#68: after death (hp_pct=0, cur/max stale), a bind-respawn must revive at full HP.
        use super::apply_bind_respawn;
        let mut gs = GameState::new();
        gs.player_id = 7;
        gs.hp_pct = 0.0; gs.cur_hp = 34; gs.max_hp = 34; // post-death contradiction (full hp, 0 pct)
        // OP_ZONE_PLAYER_TO_BIND payload: x@4, y@8, z@12 (needs len >= 20).
        let mut pkt = [0u8; 20];
        pkt[4..8].copy_from_slice(&100.0f32.to_le_bytes());
        pkt[8..12].copy_from_slice(&200.0f32.to_le_bytes());
        pkt[12..16].copy_from_slice(&(-5.0f32).to_le_bytes());
        apply_bind_respawn(&mut gs, &pkt);
        assert_eq!(gs.cur_hp, 34, "cur_hp stays at max");
        assert!((gs.hp_pct - 100.0).abs() < 1e-4, "hp_pct restored to full, got {}", gs.hp_pct);
        assert!((gs.player_x - 100.0).abs() < 1e-4 && (gs.player_y - 200.0).abs() < 1e-4,
            "position moved to the bind point");
    }

    /// Sanity: OP_Death for the player's own id must NOT touch entities or animation=115.
    #[test]
    fn apply_death_player_self_death_sets_hp_zero_not_entity() {
        use super::apply_death;
        let mut gs = GameState::new();
        gs.player_id = 7;
        let mut pkt = [0u8; 32];
        pkt[0..4].copy_from_slice(&7u32.to_le_bytes());  // spawn_id = player
        apply_death(&mut gs, &pkt);
        assert_eq!(gs.hp_pct, 0.0, "player hp_pct must be zeroed on self-death");
        assert!(gs.entities.is_empty(), "entities map must be untouched on player self-death");
    }

    fn build_group_update_b(leader: &str, members: &[(&str, u32, bool, bool, bool, bool, bool)]) -> Vec<u8> {
        // members: (name, level, is_merc, tank, assist, puller, offline)
        let mut p = Vec::new();
        p.extend_from_slice(&0u32.to_le_bytes()); // group_id_or_unused
        p.extend_from_slice(&(members.len() as u32).to_le_bytes());
        p.extend_from_slice(leader.as_bytes()); p.push(0);
        for (name, level, is_merc, tank, assist, puller, offline) in members {
            p.extend_from_slice(&0u32.to_le_bytes()); // member_index
            p.extend_from_slice(name.as_bytes()); p.push(0);
            p.extend_from_slice(&(*is_merc as u16).to_le_bytes());
            p.push(0); // merc_owner_name (empty cstr)
            p.extend_from_slice(&level.to_le_bytes());
            p.push(*tank as u8);
            p.push(*assist as u8);
            p.push(*puller as u8);
            p.extend_from_slice(&(*offline as u32).to_le_bytes());
            p.extend_from_slice(&0u32.to_le_bytes()); // timestamp
        }
        p
    }

    #[test]
    fn apply_group_update_b_replaces_roster_and_marks_leader() {
        let mut gs = GameState::new();
        let p = build_group_update_b("Aldric", &[
            ("Aldric", 10, false, true, false, false, false),
            ("Sariel", 8, false, false, true, false, false),
        ]);
        apply_group_update_b(&mut gs, &p);
        assert_eq!(gs.group_leader, "Aldric");
        assert_eq!(gs.group_members.len(), 2);
        let aldric = gs.group_members.iter().find(|m| m.name == "Aldric").unwrap();
        assert!(aldric.is_leader);
        assert!(aldric.tank);
        assert_eq!(aldric.level, 10);
        let sariel = gs.group_members.iter().find(|m| m.name == "Sariel").unwrap();
        assert!(!sariel.is_leader);
        assert!(sariel.assist);
    }

    #[test]
    fn apply_group_update_b_handles_truncated_packet_without_panicking() {
        let mut gs = GameState::new();
        let p = 5u32.to_le_bytes().to_vec(); // claims 5 members but buffer ends immediately
        apply_group_update_b(&mut gs, &p);
        assert!(gs.group_members.iter().all(|m| m.name.is_empty()) || gs.group_members.len() <= 5);
    }

    // Build the REAL RoF2 GroupJoin_Struct wire (148 bytes): fixed char[64] name fields, not cstrs.
    fn build_group_join(owner: &str, member: &str, level: u32) -> Vec<u8> {
        let mut p = vec![0u8; 148];
        let o = owner.as_bytes();
        p[0..o.len().min(63)].copy_from_slice(&o[..o.len().min(63)]);   // owner_name[64] @ 0
        let m = member.as_bytes();
        p[64..64 + m.len().min(63)].copy_from_slice(&m[..m.len().min(63)]); // membername[64] @ 64
        // merc @128 = 0; padding @129..132; level @132
        p[132..136].copy_from_slice(&level.to_le_bytes());
        p
    }

    #[test]
    fn apply_group_join_appends_new_member_once() {
        let mut gs = GameState::new();
        let p = build_group_join("Aldric", "Sariel", 8);
        apply_group_join(&mut gs, &p);
        apply_group_join(&mut gs, &p); // duplicate arrival must not double-add
        assert_eq!(gs.group_members.len(), 1);
        assert_eq!(gs.group_members[0].name, "Sariel");
        assert_eq!(gs.group_members[0].level, 8);
    }

    #[test]
    fn existing_member_learns_about_a_later_joiner() {
        // The #101 desync: an already-present member (Elaria, who joined with a {Sylvaris, Elaria}
        // roster) receives OP_GroupUpdate when Fenwick joins later. With the fixed-offset parse she
        // must append Fenwick; the old sequential-cstr parse read an empty name and dropped it,
        // leaving her blind to everyone who joined after her.
        let mut gs = GameState::new();
        gs.group_members = vec![
            crate::game_state::GroupMember { name: "Sylvaris".into(), is_leader: true, ..Default::default() },
            crate::game_state::GroupMember { name: "Elaria".into(), ..Default::default() },
        ];
        apply_group_join(&mut gs, &build_group_join("Sylvaris", "Fenwick", 12));
        let names: Vec<_> = gs.group_members.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"Fenwick"), "existing member must learn the later joiner: {names:?}");
        assert_eq!(gs.group_members.len(), 3);
        assert_eq!(gs.group_members.iter().find(|m| m.name == "Fenwick").unwrap().level, 12);
    }

    fn fixed_name_pair(name1: &str, name2: &str) -> Vec<u8> {
        let mut p = vec![0u8; 128];
        let n1 = name1.as_bytes();
        p[0..n1.len()].copy_from_slice(n1);
        let n2 = name2.as_bytes();
        p[64..64 + n2.len()].copy_from_slice(n2);
        p
    }

    #[test]
    fn apply_group_disband_other_removes_whichever_name_is_a_current_member() {
        let mut gs = GameState::new();
        gs.group_members.push(crate::game_state::GroupMember { name: "Sariel".into(), ..Default::default() });
        gs.group_members.push(crate::game_state::GroupMember { name: "Aldric".into(), ..Default::default() });
        // name1 = the departing member, name2 = something unrelated.
        let p = fixed_name_pair("Sariel", "Unrelated");
        apply_group_disband_other(&mut gs, &p);
        assert_eq!(gs.group_members.len(), 1);
        assert_eq!(gs.group_members[0].name, "Aldric");
    }

    #[test]
    fn apply_group_disband_other_no_op_when_neither_name_matches() {
        let mut gs = GameState::new();
        gs.group_members.push(crate::game_state::GroupMember { name: "Aldric".into(), ..Default::default() });
        let p = fixed_name_pair("Nobody", "AlsoNobody");
        apply_group_disband_other(&mut gs, &p);
        assert_eq!(gs.group_members.len(), 1);
    }

    #[test]
    fn apply_group_disband_you_clears_group_state() {
        let mut gs = GameState::new();
        gs.group_members.push(crate::game_state::GroupMember { name: "Aldric".into(), ..Default::default() });
        gs.group_leader = "Aldric".into();
        gs.pending_invite = Some("Someone".into());
        apply_group_disband_you(&mut gs, &[]);
        assert!(gs.group_members.is_empty());
        assert!(gs.group_leader.is_empty());
        assert!(gs.pending_invite.is_none());
    }

    #[test]
    fn apply_group_leader_change_updates_leader_and_flags() {
        let mut gs = GameState::new();
        gs.group_members.push(crate::game_state::GroupMember { name: "Aldric".into(), is_leader: true, ..Default::default() });
        gs.group_members.push(crate::game_state::GroupMember { name: "Sariel".into(), ..Default::default() });
        let mut p = vec![0u8; 148];
        let name = b"Sariel";
        p[64..64 + name.len()].copy_from_slice(name); // LeaderName at offset 64
        apply_group_leader_change(&mut gs, &p);
        assert_eq!(gs.group_leader, "Sariel");
        assert!(gs.group_members.iter().find(|m| m.name == "Sariel").unwrap().is_leader);
        assert!(!gs.group_members.iter().find(|m| m.name == "Aldric").unwrap().is_leader);
    }

    #[test]
    fn apply_group_invite_sets_pending_invite_when_addressed_to_us() {
        let mut gs = GameState::new();
        gs.player_name = "Aldric".into();
        let mut p = vec![0u8; 148];
        p[0..6].copy_from_slice(b"Aldric");
        p[64..70].copy_from_slice(b"Sariel");
        apply_group_invite(&mut gs, &p);
        assert_eq!(gs.pending_invite, Some("Sariel".to_string()));
        assert_eq!(gs.chat_events.back().unwrap().kind, "invite_received");
    }

    #[test]
    fn apply_group_invite_ignores_invite_addressed_to_someone_else() {
        let mut gs = GameState::new();
        gs.player_name = "Aldric".into();
        let mut p = vec![0u8; 148];
        p[0..6].copy_from_slice(b"Sariel"); // invitee is someone else
        apply_group_invite(&mut gs, &p);
        assert!(gs.pending_invite.is_none());
    }

    #[test]
    fn apply_group_acknowledge_pushes_joined_event() {
        let mut gs = GameState::new();
        apply_group_acknowledge(&mut gs, &[]);
        assert_eq!(gs.chat_events.back().unwrap().kind, "joined");
    }

    // ── Synthetic nav→render mirror packets (two-GameState split) ────────────

    #[test]
    fn apply_ui_local_echo_logs_kind_and_text() {
        let mut gs = GameState::new();
        let p = crate::eq_net::protocol::build_ui_local_echo("tell", "You told Sariel, 'on my way'");
        super::apply_ui_local_echo(&mut gs, &p);
        assert!(gs.messages.iter().any(|m| m.kind == "tell"
            && m.text == "You told Sariel, 'on my way'"));
    }

    #[test]
    fn apply_ui_local_echo_ignores_malformed_payloads() {
        let mut gs = GameState::new();
        super::apply_ui_local_echo(&mut gs, b"no-nul-separator");
        super::apply_ui_local_echo(&mut gs, b"\0text-without-kind");
        super::apply_ui_local_echo(&mut gs, b"chat\0");
        assert!(gs.messages.is_empty(), "malformed echoes must log nothing");
    }

    #[test]
    fn apply_gm_end_training_clears_trainer_state() {
        // The end-training packet is client→server; the synthetic mirror must close the RENDER
        // GameState's trainer window or the transient Trainer window never closes (bug #1).
        let mut gs = GameState::new();
        gs.trainer_open = Some(77);
        gs.trainer_skills = vec![100; crate::skills::NUM_SKILLS];
        super::apply_gm_end_training(&mut gs, &[0u8; 8]);
        assert!(gs.trainer_open.is_none());
        assert!(gs.trainer_skills.is_empty());
    }

    #[test]
    fn apply_auto_attack_mirrors_toggle_from_payload() {
        let mut gs = GameState::new();
        super::apply_auto_attack(&mut gs, &[1, 0, 0, 0]);
        assert!(gs.auto_attack, "byte[0]=1 must switch auto-attack ON");
        super::apply_auto_attack(&mut gs, &[0, 0, 0, 0]);
        assert!(!gs.auto_attack, "byte[0]=0 must switch auto-attack OFF");
        super::apply_auto_attack(&mut gs, &[]);
        assert!(!gs.auto_attack, "empty payload is treated as OFF, not a panic");
    }

    #[test]
    fn apply_ui_loot_state_sets_and_clears_session() {
        let mut gs = GameState::new();
        super::apply_ui_loot_state(&mut gs, &[1]);
        assert!(gs.loot_session_active);
        // Render-side pending_loot is filled by corpse packets but never drained there — the
        // idle mirror must clear it or scene.loot_active would stay true forever (bug #4).
        gs.pending_loot.push_back(42);
        gs.loot_queued_at = Some(std::time::Instant::now());
        super::apply_ui_loot_state(&mut gs, &[0]);
        assert!(!gs.loot_session_active);
        assert!(gs.pending_loot.is_empty());
        assert!(gs.loot_queued_at.is_none());
    }

    #[test]
    fn apply_packet_ui_clear_invite_clears_pending_invite() {
        use crate::eq_net::transport::AppPacket;
        let mut gs = GameState::new();
        gs.pending_invite = Some("Sariel".into());
        super::apply_packet(&mut gs, &AppPacket {
            opcode: crate::eq_net::protocol::OP_UI_CLEAR_INVITE, payload: Vec::new(),
        });
        assert!(gs.pending_invite.is_none());
    }

    #[test]
    fn apply_task_select_window_empty_payload_clears_offers() {
        // navigation.rs mirrors an accept/decline by sending an EMPTY OP_TaskSelectWindow, which
        // must clear the render-side offers so the Task selector window closes (bug #5).
        let mut gs = GameState::new();
        gs.task_offers.push(crate::game_state::TaskOffer {
            task_id: 9, npc_id: 3, title: "T".into(), description: "D".into(), has_rewards: false,
        });
        apply_task_select_window(&mut gs, &[]);
        assert!(gs.task_offers.is_empty());
    }
}
