//! Single source of truth for applying EQ server packets to GameState.
//!
//! Called from both the login phase (to keep entity positions current) and the
//! render loop (to update the scene).  No I/O or logging here — just pure state
//! mutation.

use crate::eq_net::protocol::*;
use crate::eq_net::transport::AppPacket;
use crate::eq_net::wire::WireReader;
use crate::game_state::{GameState, Entity, ZonePoint};

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
        OP_LOOT_COMPLETE        => apply_loot_complete(gs),
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
        OP_SHOP_PLAYER_BUY      => apply_shop_player_buy(gs, p),
        OP_SHOP_PLAYER_SELL     => apply_shop_player_sell(gs, p),
        OP_SHOP_END_CONFIRM     => apply_shop_end_confirm(gs, p),
        OP_SHOP_END             => {
            // NOT the NPC-merchant refusal/close signal — that's OP_ShopEndConfirm, above. Inbound
            // OP_ShopEnd only arrives from the PLAYER-TRADER (bazaar) path: TraderEndTrader() when a
            // trader we're shopping at shuts down (zone/trading.cpp:926) and CancelTraderTradeWindow()
            // (:3872). eqoxide doesn't drive trader shops today, so this is effectively unreachable —
            // but if it ever does arrive, dropping the stale session is the right move. No message: a
            // trader closing their shop is not a refused purchase, and claiming otherwise would lie.
            gs.merchant_open = None;
            gs.merchant_items.clear();
            tracing::info!("EQ: player-trader session ended (OP_ShopEnd)");
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
    if let Some(text) = crate::eq_net::protocol::parse_read_book_reply(p) {
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
    let mut r = WireReader::new(p, "OP_TaskDescription");
    let sequence_number = r.u32();
    let task_id = r.u32();
    r.skip(1); // open_window u8
    let _task_type = r.u32();
    let _reward_type = r.u32();
    let title = r.cstr();
    let _duration = r.u32();
    let _dur_code = r.u32();
    let _start_time = r.u32();
    let description = r.cstr();
    r.skip(1); // has_rewards u8
    let coin_reward = r.u32();
    let xp_reward = r.u32();
    let _faction = r.u32();
    let _reward_text = r.cstr();
    let item_link = r.cstr();
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
    let mut r = WireReader::new(p, "OP_TaskActivity");
    let _activity_count = r.u32();
    let _id3 = r.u32();
    let task_id = r.u32();
    let activity_id = r.u32();
    let _unk16 = r.u32();
    let activity_type = r.u32();
    let _unk24 = r.u32();
    let _unk28 = r.u32();
    let mob_name = r.cstr();
    let item_name = r.cstr();
    let goal_count = r.u32();
    r.skip(16); // 4 unknown u32s
    let activity_name = r.cstr();
    let done_count = r.u32();
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
    // VARIABLE-LENGTH: `member_count` records; a truncated packet must degrade gracefully (stop
    // early), never panic — so this uses the cursor's non-panicking `try_*` path throughout.
    let mut r = WireReader::new(p, "OP_GroupUpdateB");
    let _group_id = r.try_u32().unwrap_or(0);
    let member_count = r.try_u32().unwrap_or(0);
    let leader_name = r.try_cstr().unwrap_or_default();
    let mut members = Vec::new();
    for _ in 0..member_count {
        if r.at_end() { break; } // truncated packet — stop instead of reading zeroed garbage
        let _member_index = r.try_u32().unwrap_or(0);
        let member_name = r.try_cstr().unwrap_or_default();
        let is_merc = r.try_u16().unwrap_or(0) != 0;
        let _merc_owner_name = r.try_cstr().unwrap_or_default();
        let level = r.try_u32().unwrap_or(0);
        let tank = r.try_u8().unwrap_or(0) != 0;
        let assist = r.try_u8().unwrap_or(0) != 0;
        let puller = r.try_u8().unwrap_or(0) != 0;
        let offline = r.try_u32().unwrap_or(0) != 0;
        let _timestamp = r.try_u32().unwrap_or(0);
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
    let mut r = WireReader::new(p, "OP_GroupUpdate(GroupJoin)");
    let _owner_name = r.fixed_cstr(64);
    let member_name = r.fixed_cstr(64);
    let is_merc = r.u8() != 0;
    r.skip(3); // padding
    let level = r.u32();
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
/// name1[64], name2[64]. Which field carries the departing member isn't documented; we
/// defensively remove whichever of the two names is a CURRENT roster member (and
/// no-op with a warning if neither matches) rather than guessing wrong and corrupting state.
fn apply_group_disband_other(gs: &mut GameState, p: &[u8]) {
    let mut r = WireReader::new(p, "OP_GroupDisbandOther");
    let name1 = r.fixed_cstr(64);
    let name2 = r.fixed_cstr(64);
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
    let mut r = WireReader::new(p, "OP_GroupLeaderChange");
    r.skip(64); // skip Unknown000[64]
    let leader_name = r.fixed_cstr(64);
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
    let mut r = WireReader::new(p, "OP_GMTraining");
    let npcid = r.u32();
    r.skip(4); // playerid (unused)
    // skills[100] are optional trailing (a short packet just leaves later caps at 0) — non-panicking.
    let mut caps = vec![0u32; crate::skills::NUM_SKILLS];
    for c in caps.iter_mut() {
        match r.try_u32() { Some(v) => *c = v, None => break }
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
    let mut r = WireReader::new(p, "OP_SkillUpdate");
    let id = r.u32() as usize;
    let val = r.u32();
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
    let mut r = WireReader::new(p, "OP_GroupInvite");
    let invitee_name = r.fixed_cstr(64);
    let inviter_name = r.fixed_cstr(64);
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


/// OP_ShopPlayerBuy (server→client echo) — confirms a purchase completed. On success the RoF2
/// server echoes the SAME opcode with the SAME 32-byte Merchant_Sell_Struct sent in the request
/// (npcid@0, playerid@4, itemslot@8, unknown12@12, quantity@16, unknown20@20, price@24,
/// unknown28@28 — common/patches/rof2_structs.h), with `quantity`/`price` recomputed server-side
/// (zone/client_packet.cpp Handle_OP_ShopPlayerBuy). The item itself arrives separately as a plain
/// OP_ItemPacket(Trade), already handled generically by apply_item_packet via `main_slot`.
///
/// On failure the server sends NO echo of this opcode at all — for a bad merchant/out-of-range/qty
/// or a stale slot it sends OP_ShopEndConfirm instead (apply_shop_end_confirm below); for
/// insufficient funds it sends absolutely nothing. So receipt of THIS packet is itself the success
/// signal — there is no separate flag to check — and this is the only place allowed to deduct coin
/// or log "Bought item" (#345, generalizing the #269 sell fix). The server takes the money with
/// `TakeMoneyFromPP(price)` — the default `update_client=false` overload — so no OP_MoneyUpdate
/// ever follows a buy; this handler must apply the coin change itself once confirmed.
///
/// Note there is NO faction check anywhere in Handle_OP_ShopPlayerBuy (zone/client_packet.cpp
/// 14126-14372): faction only gates *opening* the window (Handle_OP_ShopRequest :14648-14654,
/// which rejects at DUBIOUS+ via MerchantRejectMessage). A buy from a KOS merchant whose window is
/// already open therefore SUCCEEDS — KOS is not a buy-refusal path.
fn apply_shop_player_buy(gs: &mut GameState, p: &[u8]) {
    if p.len() < 32 { return; }
    let itemslot = u32::from_le_bytes([p[8], p[9], p[10], p[11]]);
    let quantity = u32::from_le_bytes([p[16], p[17], p[18], p[19]]).max(1);
    let price    = u32::from_le_bytes([p[24], p[25], p[26], p[27]]);

    let msg = format!("Bought item (slot {itemslot}) x{quantity} for {price}c");
    gs.log_msg("merchant", &msg);
    gs.push_event("merchant", "bought", "", true, &msg);
    tracing::info!("EQ: buy confirmed — itemslot={itemslot} qty={quantity} price={price}");

    // The server HAS taken the coin (TakeMoneyFromPP). If our local snapshot can't cover the price
    // it is already stale/drifted — spend_coin() would then deduct NOTHING and return false, which
    // would leave `gs.coin` silently OVERSTATED. Never swallow that: the whole point of #345 is that
    // the client must not report a balance it knows is wrong. Say so out loud (#345).
    if !gs.spend_coin(price as u64) {
        let warn = format!(
            "Coin desync: server charged {price}c for the item above but our balance only showed \
             {}p {}g {}s {}c — the real balance is lower than displayed",
            gs.coin[0], gs.coin[1], gs.coin[2], gs.coin[3]
        );
        gs.log_msg("merchant", &warn);
        gs.push_event("merchant", "coin_desync", "", true, &warn);
        tracing::warn!("EQ: buy confirmed but local coin snapshot could not cover price={price} — coin is stale");
    }
    // Note: even a fully-covered, confirmed buy does NOT clear coin_verified here. `begin_shop_buy`
    // (send time) bumped `unverified_buys`, and this per-buy echo only confirms a relative delta —
    // it cannot rule out an EARLIER silent refusal (inventory-full/LORE) having already diverged the
    // absolute balance. Only `reconcile_coin` (a real OP_PlayerProfile) may restore trust (#361
    // review — FIX 1). `coin_verified()` is computed, so there is no field to wrongly set here.
}

/// OP_ShopEndConfirm (server→client, 0-byte body) — EQEmu's SendMerchantEnd() (zone/client.cpp
/// 13276-13286). For THIS client it is unambiguously a buy refusal. Every call site is either a
/// buy-path early return — bad merchant / not-a-merchant / qty<1 / out-of-range
/// (zone/client_packet.cpp:14151), a stale-or-removed item slot (:14194), or a negative price
/// (:14254) — or Handle_OP_ShopEnd (:14123), which only ever runs in response to a client-sent
/// OP_ShopEnd that eqoxide never sends (its merchant close is OP_ShopRequest with cmd=0).
///
/// This must NOT be gated on `merchant_open`. Handle_OP_ShopRequest returns SILENTLY, with no echo,
/// for a non-merchant target (:14605-14607) and for an out-of-range one (:14610-14612) — so in
/// exactly the case where this refusal is the only signal the server ever sends, `merchant_open` was
/// never set. Gating on it would drop the one honest packet on the floor and leave the agent unable
/// to tell "refused" from "no reply" — a quieter lie, but still a lie (#345 review).
///
/// Insufficient funds sends neither this nor the OP_ShopPlayerBuy echo, so the absence of BOTH still
/// genuinely means "no reply". Silence is acceptable; a fabricated success is not.
fn apply_shop_end_confirm(gs: &mut GameState, _p: &[u8]) {
    gs.merchant_open = None;
    gs.merchant_items.clear();
    // Deliberately does NOT clear coin_verified. No coin was taken on any path reaching this handler
    // (see the doc comment above), so THIS buy cost nothing — but that is a relative fact about one
    // buy, and cannot rule out an EARLIER silent refusal having already diverged the balance. Only
    // `reconcile_coin` (a real OP_PlayerProfile) restores trust; a refusal echo must not (#361
    // review — FIX 1).
    let msg = "Merchant refused the purchase (session ended)";
    gs.log_msg("merchant", msg);
    gs.push_event("merchant", "refused", "", true, msg);
    tracing::info!("EQ: OP_ShopEndConfirm — merchant refused the purchase (session ended)");
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
    // VARIABLE-LENGTH: `count` records, but the count is clamped and a truncated packet must
    // degrade gracefully — non-panicking `try_*` path (break on the first short read).
    let mut r = WireReader::new(p, "OP_CompletedTasks");
    // Each entry is at least 9 bytes (task_id u32 + empty-title null byte + completed_time u32);
    // clamp so a malformed/truncated count can't spin the loop needlessly.
    let count = r.try_u32().unwrap_or(0).min((p.len() as u32 / 9).max(1));
    for _ in 0..count {
        let Some(task_id) = r.try_u32() else { break; };
        let Some(title) = r.try_cstr() else { break; };
        let Some(completed_time) = r.try_u32() else { break; };
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
    // VARIABLE-LENGTH: `task_count` records, count clamped; a truncated/empty packet must degrade
    // gracefully (an empty payload legitimately clears the offers — see the navigation.rs mirror),
    // so this uses the non-panicking `try_*` path throughout.
    let mut r = WireReader::new(p, "OP_TaskSelectWindow");
    // Each entry is at least 23 bytes (task_id u32 + reward_multiplier f32 + duration u32 +
    // duration_code u32 + title cstr≥1 + desc cstr≥1 + has_rewards u8 + element_count u32).
    // Header is 12 bytes (task_count u32 + type u32 + task_giver u32). Clamp the count so a
    // malformed/truncated packet can't request unbounded allocation.
    let task_count = r.try_u32().unwrap_or(0);
    let max_entries = (p.len().saturating_sub(12) as u32) / 23;
    let task_count = task_count.min(max_entries);
    let _sel_type = r.try_u32().unwrap_or(0);
    let task_giver = r.try_u32().unwrap_or(0);
    let mut offers = Vec::with_capacity(task_count as usize);
    for _ in 0..task_count {
        let task_id = r.try_u32().unwrap_or(0);
        r.try_skip(4); // reward_multiplier f32 (unused)
        let _duration = r.try_u32().unwrap_or(0);
        let _duration_code = r.try_u32().unwrap_or(0);
        let title = r.try_cstr().unwrap_or_default();
        let description = r.try_cstr().unwrap_or_default();
        let has_rewards = r.try_u8().unwrap_or(0) != 0;
        let element_count = r.try_u32().unwrap_or(0);
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
        let id = WireReader::new(payload, "OP_DeleteSpawn").u32();
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
    // GameState::set_target also clears target_con_name/target_attitude (not just target_con)
    // and resolves the F1 self-target case — this handler used to duplicate a partial copy of
    // that logic inline, which is exactly how the con_name/attitude clear got missed (#323).
    gs.set_target(id);
}

fn apply_new_zone(gs: &mut GameState, payload: &[u8]) {
    // Length-check BEFORE the one-shot below: a truncated OP_NewZone must not consume the zone-in's
    // single apply, or the real one that follows would be swallowed and we'd keep the PREVIOUS zone's
    // name/id/safe point/underworld clamp (#150) for the whole session.
    if payload.len() < SIZE_NEW_ZONE { return; }
    // Apply at most once per zone-server session (#322). A zone-in delivers OP_NewZone twice — the
    // server sends it while handling OP_ZoneEntry and again in reply to the OP_ReqNewZone we send on
    // OP_Weather — and the second copy arrives AFTER OP_ReqClientSpawn, while the spawn/door stream
    // it requested is landing. Re-running the clears below would wipe that stream (missing NPCs and
    // doors after zoning), and re-log "Entered <zone>" + a second navigate/zone event. Both copies
    // carry identical zone fields, so there is nothing to redo.
    if gs.new_zone_applied { return; }
    gs.new_zone_applied = true;
    // Purge the previous zone's spawns and doors (#270): OP_NewZone fires on EVERY server-driven zone
    // entry — normal travel, a same-zone #zone, AND a death-respawn — and each one arrives on a fresh
    // zone-server session, whose handshake re-arms the flag above via `begin_zone_in`. Without the
    // purge, respawns and re-zones accumulate stale + duplicate cross-zone entities, so name→position
    // resolution (goto/follow/merchant/target-by-name) picks ghosts. The spawn/door stream that
    // follows OP_ReqClientSpawn repopulates the new zone; sync_entities full-replaces the HTTP maps.
    gs.doors.clear();
    gs.entities.clear();
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

/// `ManaChange_Struct` (EQEmu common/eq_packet_structs.h:462 — no RoF2 ENCODE, sent raw):
///   /*00*/ uint32 new_mana;  /*04*/ uint32 stamina;  /*08*/ uint32 spell_id;
///   /*12*/ uint8  keepcasting;  /*13*/ uint8 padding[3];  /*16*/ int32 slot;
/// Returns `(new_mana, spell_id, keepcasting)`. `keepcasting == 0` means "the cast STOPPED" — the
/// server sends it from `Mob::StopCasting` (zone/spells.cpp:1369) and `Mob::SendSpellBarEnable`
/// (zone/spells.cpp:5752) on *every* cast end (completed, interrupted, or fizzled), naming the
/// spell that ended. The 4-byte prefix is still accepted (mana only) so a short packet can't
/// silently drop the mana update. (eqoxide#348)
pub fn parse_mana_change(p: &[u8]) -> Option<(u32, Option<u32>, Option<u8>)> {
    if p.len() < 4 { return None; }
    let new_mana = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    if p.len() < 13 { return Some((new_mana, None, None)); }
    let spell_id = u32::from_le_bytes([p[8], p[9], p[10], p[11]]);
    Some((new_mana, Some(spell_id), Some(p[12])))
}

pub fn parse_memorize_spell(p: &[u8]) -> Option<(u32, u32, u32)> {
    if p.len() < 12 { return None; }
    let r = |o: usize| u32::from_le_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
    Some((r(0), r(4), r(8)))
}

/// `InterruptCast_Struct` (EQEmu common/eq_packet_structs.h:446 — no RoF2 ENCODE, sent raw):
///   /*00*/ uint32 spawnid;  /*04*/ uint32 messageid;  /*08*/ char message[0];
/// `messageid` is an eqstr id that says WHY the cast ended: SPELL_FIZZLE (173) / MISS_NOTE (180) =
/// a fizzle, INTERRUPT_SPELL (439) = a true interrupt (zone/spells.cpp:1299-1314). Returns
/// `(spawnid, messageid)`; the old parse read only `spawnid` and threw the reason away, which is
/// why the client could never tell a fizzle from an interrupt. (eqoxide#348)
pub fn parse_interrupt_cast(p: &[u8]) -> Option<(u32, u32)> {
    if p.len() < 8 { return None; }
    let r = |o: usize| u32::from_le_bytes([p[o], p[o + 1], p[o + 2], p[o + 3]]);
    Some((r(0), r(4)))
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
        // Coin reconciliation against the server's authoritative figure (#361): a merchant buy the
        // server silently refused (inventory-full/LORE — no echo of any kind reaches the client
        // for either, EQEmu zone/client_packet.cpp ~14198-14303) can leave `gs.coin` diverged from
        // reality with nothing else to correct it. Only reconcile when this payload actually
        // carried the coin block (>=13285 — see `parse_player_profile`'s offset comment); a
        // short/legacy-length payload leaves `p.coin` at the parser's zero sentinel, which is not
        // a real reading and must not overwrite (or be compared against) the real balance.
        if payload.len() >= 13285 {
            if let Some(prior) = gs.reconcile_coin(p.coin) {
                let warn = format!(
                    "Coin desync detected on zone-in: local balance was {}p {}g {}s {}c but the \
                     server says {}p {}g {}s {}c (a merchant refusal likely charged or withheld \
                     coin without telling us) — correcting to the server's figure",
                    prior[0], prior[1], prior[2], prior[3],
                    p.coin[0], p.coin[1], p.coin[2], p.coin[3],
                );
                gs.log_msg("merchant", &warn);
                gs.push_event("merchant", "coin_desync", "", true, &warn);
                tracing::warn!("EQ: PlayerProfile coin reconciliation found a desync: {:?} -> {:?}", prior, p.coin);
            }
        } else {
            gs.coin = p.coin; // legacy/short payload: no real coin block, preserve prior behavior
        }
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
        // Sets gs.casting AND pushes a `combat`/`cast_begin` event — casting used to be tracked
        // purely internally, so an agent had no way to learn the cast had even started. (eqoxide#348)
        gs.begin_cast(spell_id, cast_ms);
    }
}

/// The sentinel spell id `Mob::SendSpellBarDisable` puts in OP_MemorizeSpell (zone/client.h:99
/// `SPELLBAR_UNLOCK 0x2bc`). It is a spell-bar command, not a spell: never report it as a completed
/// cast. (Nothing in EQEmu currently calls SendSpellBarDisable, but the sentinel is cheap to honor.)
const SPELLBAR_UNLOCK: u32 = 0x2bc;

pub fn apply_mana_change(gs: &mut GameState, p: &[u8]) {
    // OP_ManaChange carries the player's new *current* mana (ManaChange_Struct.new_mana @0); no max.
    // Apply it so the HUD/API mana bar tracks spending/regen. set_mana keeps max as a high-water-mark
    // (the profile seed sets the true max for a rested caster at zone-in). (eqoxide#27)
    let Some((new_mana, spell_id, keepcasting)) = parse_mana_change(p) else { return };
    gs.set_mana(new_mana as i32);

    // `keepcasting == 1` is the routine mana/endurance update (Client::CheckManaEndUpdate,
    // zone/client.cpp:2427-2432) — regen, not a cast ending. Only 0 means "the cast stopped".
    if keepcasting != Some(0) { return; }

    // OP_ManaChange(keepcasting=0) is the server's UNIVERSAL cast-end signal: StopCasting
    // (zone/spells.cpp:1369) and SendSpellBarEnable (zone/spells.cpp:5752) both send it, on EVERY
    // way a cast can end — completed, interrupted, fizzled, refused, or dropped because
    // SpellFinished returned false. So it is the terminal, deferred by one packet so a following
    // memorize/interrupt/message can still refine WHY it ended.
    //
    // The server also sends it as the TAIL of an interrupt/refusal it has already explained
    // (InterruptSpell → SendSpellBarEnable, zone/spells.cpp:1314). We reported that outcome on the
    // earlier packet, so the trailing one must not re-arm anything — otherwise the next unnamed
    // failure inherits the spell we just finished reporting. (eqoxide#348)
    if gs.suppress_cast_end {
        gs.suppress_cast_end = false;
        return;
    }

    // The spell that ended. This is the only packet that names a FIZZLED spell — the fizzle is
    // decided in DoCastSpell (zone/spells.cpp:320) before OP_BeginCast is ever sent (:450), so
    // gs.casting is None by the time "Your spell fizzles!" arrives.
    // (No SPELLBAR_UNLOCK check: OP_ManaChange always carries a REAL spell id — StopCasting sends
    // `casting_spell_id`, SendSpellBarEnable sends the spell it was called with. The sentinel only
    // ever appears in OP_MemorizeSpell.)
    if let Some(spell_id) = spell_id {
        if !crate::game_state::gem_is_empty(spell_id) {
            gs.ended_cast_spell = Some((spell_id, std::time::Instant::now()));
        }
    }
    // Arms the terminal only for a cast we believe is IN FLIGHT. `ResetAllCastbarCooldowns`
    // (zone/spells.cpp:7246, reachable from Lua quest scripts) fires SendSpellBarEnable for EVERY
    // memorized gem while the player is not casting at all — arming on those would invent a burst
    // of phantom cast outcomes. `end_cast_unexplained` no-ops when nothing is casting.
    gs.end_cast_unexplained();
}

pub fn apply_memorize_spell(gs: &mut GameState, p: &[u8]) {
    if let Some((slot, spell_id, scribing)) = parse_memorize_spell(p) {
        match scribing {
            1 => { if (slot as usize) < 9 { gs.mem_spells[slot as usize] = spell_id; } }
            2 => { if (slot as usize) < 9 { gs.mem_spells[slot as usize] = crate::game_state::EMPTY_GEM; } }
            // memSpellSpellbar (zone/client.h:105) — the server re-enables the spell bar only from
            // the tail of Mob::SpellFinished (zone/spells.cpp:1803/1824), i.e. the cast COMPLETED.
            // An interrupt/fizzle never sends it (InterruptSpell → OP_InterruptCast + OP_ManaChange
            // only). So this is the authoritative "your spell landed" signal. (eqoxide#348)
            //
            // SPELLBAR_UNLOCK (0x2bc = 700) is a bar COMMAND, not a spell — but 700 is also a legal
            // spell id, so we cannot reject it on value alone or a real cast of spell 700 would
            // never report completion. Treat it as the sentinel only when it is NOT the spell we are
            // currently casting. (In practice nothing in EQEmu calls SendSpellBarDisable, so the
            // sentinel never actually arrives; this just keeps the value-collision honest.)
            //
            // KNOWN IMPRECISION (accepted, #348 review): if a completion of spell 700 arrives LATE
            // — after OP_ManaChange already cleared `casting` — `is_sentinel` sees casting == None
            // and swallows it, so the cast resolves as `cast_ended_unexplained` instead of
            // `cast_completed`. That degrades to an explicit "we don't know why it ended", which is
            // imprecision, not a lie. It affects exactly one spell id and only on a late packet.
            3 => {
                let is_sentinel = spell_id == SPELLBAR_UNLOCK
                    && gs.casting.as_ref().map_or(true, |c| c.spell_id != spell_id);
                if is_sentinel {
                    gs.casting = None;
                } else {
                    let text = format!("You have finished casting {}.", crate::spells::name_of(spell_id));
                    gs.finish_cast(spell_id, "cast_completed", &text);
                }
            }
            _ => {}
        }
    }
}

pub fn apply_interrupt_cast(gs: &mut GameState, p: &[u8]) {
    let Some((spawnid, messageid)) = parse_interrupt_cast(p) else { return };
    // The server ALSO broadcasts an OP_InterruptCast to everyone nearby when someone ELSE's cast
    // breaks (zone/spells.cpp:1339-1345, with the caster's name appended). Acting on it would clear
    // OUR cast bar because a passing NPC got interrupted — the same class of bug as eqoxide#222.
    if spawnid != gs.player_id { return; }
    // Only a cast we believe is RUNNING can be interrupted. This gate is what keeps a cast-start
    // refusal from being double-reported: the server says "Insufficient Mana to cast this spell!"
    // (OP_SimpleMessage 199) and THEN calls InterruptSpell() → OP_InterruptCast 439
    // (zone/spells.cpp:484-496). apply_simple_message already published the real reason and cleared
    // `casting`, so the trailing generic "interrupted" here would overwrite a precise outcome with a
    // vague one. (eqoxide#348)
    if gs.casting.is_none() { return; }
    let (kind, text) = match messageid {
        crate::game_state::SPELL_FIZZLE | crate::game_state::MISS_NOTE =>
            ("cast_fizzled", "Your spell fizzles!".to_string()),
        _ => ("cast_interrupted", "Your spell is interrupted.".to_string()),
    };
    gs.finish_cast(0, kind, &text); // 0 = take the spell from gs.casting / the ended-cast hint
    // InterruptSpell sends OP_InterruptCast and THEN SendSpellBarEnable (zone/spells.cpp:1299-1314).
    // We have just reported the outcome, so ignore that trailing OP_ManaChange.
    gs.suppress_cast_end = true;
}

/// A server eqstr id that means the player's cast ended badly (or never started). Returns the event
/// `kind` to publish, or None if the message has nothing to do with casting. (eqoxide#348)
fn cast_outcome_for_string_id(string_id: u32) -> Option<&'static str> {
    use crate::game_state::{CAST_FAILED_STRING_IDS, MISS_NOTE, SPELL_FIZZLE};
    match string_id {
        // A player fizzle reaches us ONLY as this bare message: Client::CheckFizzle fails inside
        // DoCastSpell, which calls StopCasting() + MessageString(Chat::SpellFailure, SPELL_FIZZLE)
        // and returns *before* OP_BeginCast / any OP_InterruptCast (zone/spells.cpp:318-345).
        SPELL_FIZZLE | MISS_NOTE => Some("cast_fizzled"),
        // Cast-start refusals (no mana / recast timer / no target / …). The server sends the eqstr
        // and nothing else — previously the cast just vanished. (zone/spells.cpp:169-241, :484-496)
        id if CAST_FAILED_STRING_IDS.contains(&id) => Some("cast_failed"),
        _ => None,
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
pub(crate) fn apply_death(gs: &mut GameState, payload: &[u8]) {
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
//
// #417: the pure-melee sentinel for `spellid` is `SPELL_UNKNOWN` (0xFFFF = 65535), NOT 0 —
// EQEmu's `Mob::Damage` takes `spell_id` as a `uint16` and every melee call site passes
// SPELL_UNKNOWN explicitly (zone/attack.cpp), which zero-extends onto this wire's `uint32`
// field as 0x0000FFFF. A bare `spellid != 0` check therefore treats every melee swing as a
// spell cast. See docs/eq-technical-knowledgebase/combat-damage-struct.md.
const SPELL_UNKNOWN: u32 = 0xFFFF;
fn apply_combat_damage(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 13 { return; }
    let target_id = u16::from_le_bytes([payload[0], payload[1]]) as u32;
    let source_id = u16::from_le_bytes([payload[2], payload[3]]) as u32;
    // spellid (u32)@5 — non-zero (and not the SPELL_UNKNOWN sentinel) for a SPELL action
    // (heal/buff/nuke/DoT); 0 or SPELL_UNKNOWN (0xFFFF) for a melee swing.
    let spellid   = u32::from_le_bytes([payload[5], payload[6], payload[7], payload[8]]);
    let damage    = i32::from_le_bytes([payload[9], payload[10], payload[11], payload[12]]);
    let target_name = gs.entities.get(&target_id).map(|e| e.name.clone())
        .unwrap_or_else(|| if target_id == gs.player_id { gs.player_name.clone() } else { format!("#{target_id}") });
    let source_name = gs.entities.get(&source_id).map(|e| e.name.clone())
        .unwrap_or_else(|| if source_id == gs.player_id { gs.player_name.clone() } else { format!("#{source_id}") });
    // A `CombatDamage_Struct.damage` of 0 is a plain miss; a POSITIVE value is real damage; a
    // NEGATIVE value is an EQEmu special-outcome sentinel (zone/common.h DMG_*), NOT "negative
    // damage" (#262). Map each to native combat wording instead of leaking "-N damage" / "(type=N)".
    let msg = if spellid != 0 && spellid != SPELL_UNKNOWN {
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
    let beneficial_spell = spellid != 0 && spellid != SPELL_UNKNOWN
        && crate::spells::global().is_some_and(|d| d.is_beneficial(spellid));
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
    // VARIABLE-LENGTH: `count` records; on truncation keep what we already parsed rather than
    // dropping all — non-panicking `try_*` path, break on the first short field.
    let mut r = WireReader::new(&p[64..], "OP_WhoAllResponse");
    let mut roster: Vec<crate::game_state::WhoEntry> = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let parsed = (|| {
            let _fmt = r.try_u32()?;
            let _pad = r.try_u32()?;
            let _pid = r.try_u32()?;
            let name = r.try_cstr()?;
            let _rank = r.try_u32()?;
            let guild = r.try_cstr()?;
            let _u80a = r.try_u32()?;
            let _u80b = r.try_u32()?;
            let zonestr = r.try_u32()?;
            let zone = r.try_u32()?;
            let class = r.try_u32()?;
            let level = r.try_u32()?;
            let race = r.try_u32()?;
            let _acct = r.try_cstr()?;
            let _u100 = r.try_u32()?;
            let anon = zonestr == 0xFFFF_FFFF || (class == 0 && level == 0 && race == 0);
            Some(crate::game_state::WhoEntry { name, level, class, race, zone_id: zone, guild, anon })
        })();
        match parsed {
            Some(entry) => roster.push(entry),
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
    // VARIABLE-LENGTH / hot chat path: sender+target are NUL-terminated and the packet has trailing
    // unknowns, so a defensive early-return (not a panic) is kept for a malformed/short packet — via
    // the cursor's non-panicking `try_cstr` + `remaining` guard.
    let mut r = WireReader::new(payload, "OP_ChannelMessage");
    let Some(sender) = r.try_cstr() else { return; };
    let Some(targetname) = r.try_cstr() else { return; };
    // After the two strings: 4 (unk) + 4 (lang) + 4 (chan) + 4 (unk) + 1 (unk) + 4 (skill) = 21
    // bytes, then the NUL-terminated message.
    if r.remaining() < 21 { return; }
    r.skip(8);              // unknown u32 + language u32
    let chan_num = r.u32(); // chan_num is at offset 8 of the 21-byte block (guarded above)
    r.skip(9);              // remainder of the 21-byte block (unknown u32 + u8 + skill u32)
    let msg = String::from_utf8_lossy(r.rest())
        .split('\0').next().unwrap_or("")
        .to_string();
    if msg.is_empty() { return; }
    // NPC dialogue may embed saylink hyperlinks; show the readable label and capture any clickable
    // choices. Only the Say channel (8) is NPC conversation — a saylink arriving on a player chat
    // channel (tell/OOC/etc.) is not a dialogue prompt, so choices are only adopted for `say`.
    let (msg, choices) = parse_say_links(&msg);
    if chan_num == 8 && !choices.is_empty() { gs.dialogue_choices = choices; }

    // Self-echo filter (#325): EQEmu broadcasts channel messages — say/ooc/shout/group/guild/
    // auction/gmsay — back to the SENDING client too, not just other listeners in range
    // (zone/entity.cpp's ChannelMessageSend delivery loops iterate every client including the
    // sender, with no `client == sender` skip; confirmed against a live server). Tells are the
    // same story via a different mechanism: the world server echoes a tell back to the sender
    // on chan_num 14 (ChatChannel_TellEcho), NOT 7 (zone/worldserver.cpp), so a self-tell doesn't
    // even land in the tell/ooc/... match below — without this filter it silently falls into the
    // generic "chat" bucket as a *second*, differently formatted line.
    //
    // The outgoing-chat code in navigation.rs (the hail/say/dialogue-click/chat_send sites)
    // already writes the "You say/tell/..." line into the log the moment we send, so this
    // inbound bounce-back must never be logged or evented — logging it doubles every outgoing
    // line, and eventing it means an agent polling /v1/events/chat receives its own outbound
    // message as if some other player said it (agent-honesty violation). The check is keyed on
    // sender identity, not channel number, so it catches the chan-14 tell echo along with every
    // other self-broadcast channel uniformly.
    let is_self_echo = !gs.player_name.is_empty()
        && !sender.is_empty()
        && sender.eq_ignore_ascii_case(&gs.player_name);
    if is_self_echo {
        tracing::debug!("chat: dropped self-echo from server (chan {chan_num}): {msg}");
        return;
    }

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
    // VARIABLE-LENGTH: `count` (id, name) records; a short packet stops the loop — non-panicking.
    let mut r = WireReader::new(&payload[68..], "OP_GuildsList");
    let mut names = std::collections::HashMap::with_capacity(count);
    for _ in 0..count {
        let Some(id) = r.try_u32() else { break; };
        let Some(name) = r.try_cstr() else { break; };
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
    // BIG-ENDIAN + VARIABLE-LENGTH: this is the one guild packet sent in network byte order. A short
    // packet stops the loop rather than panicking — non-panicking `try_*_be` path with `has` guards.
    let mut r = WireReader::new(payload, "OP_GuildMemberList");
    let Some(_prefix) = r.try_cstr() else { return; };
    if r.remaining() < 8 { return; }
    r.skip(4); // skip the uninitialized guild_id u32
    let count = r.u32_be() as usize;
    let mut members = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let Some(name) = r.try_cstr() else { break; };
        if r.remaining() < 40 { break; } // 10 × u32 before the public_note cstr
        let level = r.u32_be();
        r.skip(4);                 // banker flags (skipped)
        let class = r.u32_be();
        let rank  = r.u32_be();
        // consume the remaining fixed u32s: time_last_on, tribute_enable, unknown, total_tribute,
        // last_tribute, unknown_one (level/banker/class/rank above make 10 total = 40 bytes).
        r.skip(24);
        let Some(public_note) = r.try_cstr() else { break; };
        if r.remaining() < 12 { break; } // u16 zoneinstance + u16 zone_id + u32 + u32
        r.skip(2);                 // zoneinstance (skipped)
        let zone_id = r.u16_be() as u32;
        r.skip(8);                 // trailing u32 + u32
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
/// + unknown(u32) (`SimpleMessage_Struct`, EQEmu common/eq_packet_structs.h:3832).
///
/// `Client::MessageString(type, string_id)` sends this straight to the caster only
/// (zone/client.cpp:3811-3821 — `QueuePacket`, not a broadcast), so a spell-failure id here is
/// unambiguously OURS. That matters: a fizzle and every cast-start refusal arrive as nothing BUT
/// this message, so if we only log the text the outcome is invisible to an agent. Turn the known
/// spell-failure ids into a real cast outcome. (eqoxide#348)
fn apply_simple_message(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 8 { return; }
    let string_id = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    if let Some(kind) = cast_outcome_for_string_id(string_id) {
        let text = crate::eqstr::format_id(string_id, &[])
            .unwrap_or_else(|| "Your spell failed.".to_string());
        gs.finish_cast(0, kind, &text); // finish_cast logs the line too
        // A cast-start/mid-cast REFUSAL ("Insufficient Mana", "Spell recast time not yet met", …)
        // is followed by StopCastSpell/InterruptSpell → SendSpellBarEnable → OP_ManaChange
        // (zone/spells.cpp:169-241, :484-496). Ignore that trailing terminal; we just explained it.
        // A FIZZLE is the opposite shape — its OP_ManaChange arrives BEFORE this message
        // (StopCasting then MessageString, zone/spells.cpp:326-330) — so it must not suppress.
        // NOTE: the server does NOT always send that trailing ManaChange — an instant item clicky
        // or an AA skips SendSpellBarEnable entirely (zone/spells.cpp:158-161), and
        // SPELL_TOO_POWERFUL reaches that path. `suppress_cast_end` is a bool that begin_cast /
        // begin_zone_in clear, so an unbalanced arm here cannot leak into a later cast.
        if kind == "cast_failed" {
            gs.suppress_cast_end = true;
        }
        return;
    }
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

/// OP_Consider reply — the server's con of a spawn. Consider_Struct: playerid(u32) +
/// targetid(u32) + faction(u32) + level(u32 = con color) + cur_hp + ...
///
/// A consider reply is data ABOUT a spawn, never a target-select (eqoxide#330). It used to write
/// `gs.target_id`, which made it a 5th (uncounted) writer alongside the 4 that PR #327 routed
/// through `set_target`: if the target changed (auto-combat retarget, hail, a second manual
/// target) before a stale reply for the PREVIOUS target landed, this handler snapped
/// `target_id`/`target_con*` back to that stale spawn while `target_name`/`target_hp_pct` kept
/// the CURRENT target's values — a mismatched id+con vs name+hp split-brain. It no longer writes
/// `target_id` at all.
///
/// The reply's spawn is NOT necessarily the current target, so the two halves are handled
/// separately:
///  - The chat line is ALWAYS logged. There are two OP_CONSIDER send sites (navigation.rs): the
///    target path (preceded by `set_target`), and the *standalone* consider at navigation.rs:1932,
///    fed by `POST /v1/combat/consider {"id":N}` — whose whole purpose is conning an arbitrary
///    spawn that is deliberately NOT your target. Gating the log line on "is this the current
///    target" would make that endpoint a silent no-op (200 OK, then nothing).
///  - The `target_con` / `target_con_name` / `target_attitude` HUD+API fields describe the CURRENT
///    target only, so those three writes are gated on the reply actually being about it. That is
///    what closes #330: a stale reply can no longer overwrite the current target's con.
pub(crate) fn apply_consider(gs: &mut GameState, payload: &[u8]) {
    if payload.len() < 16 { return; }
    let target_id = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let faction   = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
    let level     = u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
    let name = gs.entities.get(&target_id).map(|e| e.name.clone())
        .unwrap_or_else(|| "Your target".to_string());

    // Con fields describe the CURRENT target — only apply them when this reply is about it (#330).
    if gs.target_id == Some(target_id) {
        gs.target_con = Some(con_color(level));
        // #292: also record the structured difficulty tier + attitude enum so agents can read "how
        // tough" from /observe/debug instead of scraping the localized chat line or the RGB tint.
        gs.target_con_name = Some(con_level_name(level).to_string());
        gs.target_attitude = Some(attitude_name(faction).to_string());
    }

    // Always logged — a standalone consider of a non-target spawn must still report its result.
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
    let mut r = WireReader::new(payload, "OP_Respawn");
    r.skip(4); // spawn_id / zone_id header (unused here)
    gs.player_x = r.f32();
    gs.player_y = r.f32();
    gs.player_z = r.f32();
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

/// EQEmu's `LootResponse` enum (zone/common.h) — the `response` byte of MoneyOnCorpse_Struct,
/// the server's only ack for a client OP_LootRequest. Verified against zone/corpse.cpp
/// `MakeLootRequestPackets` (sets `Normal`/`LootAll` on success) and `SendLootReqErrorPacket`
/// (sets `SomeoneElse`/`NotAtThisTime`/`Hostiles`/`TooFar` on refusal) — see protocol.rs OP_MONEY_ON_CORPSE.
fn loot_refusal_reason(response: u8) -> Option<&'static str> {
    match response {
        1 | 3 | 6 => None, // Normal / Normal2 / LootAll — accepted, not a refusal.
        0 => Some("someone else is looting it"),
        2 => Some("not at this time"),
        4 => Some("hostiles nearby"),
        5 => Some("too far away"),
        _ => Some("refused"),
    }
}

fn apply_money_on_corpse(gs: &mut GameState, payload: &[u8]) {
    // MoneyOnCorpse_Struct: response(u8) + 3×pad + platinum(u32) + gold(u32) + silver(u32) + copper(u32)
    if payload.len() < 20 { return; }
    let response = payload[0];
    if let Some(reason) = loot_refusal_reason(response) {
        // The corpse never opened — say so honestly (distinct from "Looting complete") instead of
        // letting the auto-loot timer silently declare the session done with zero items (#346).
        let corpse_id = gs.loot_current_corpse.take();
        gs.loot_session_active = false;
        gs.loot_confirmed = false;
        gs.loot_last_activity = None;
        gs.loot_end_requested_at = None;
        let msg = format!("Loot refused: {reason}");
        gs.log_msg("loot", &msg);
        gs.push_event("loot", "refused", "system", true, &msg);
        tracing::warn!(
            "EQ: OP_MoneyOnCorpse refused loot (response={response}, corpse_id={corpse_id:?})"
        );
        // Let the queue move on to the next corpse rather than getting stuck behind this refusal.
        gs.loot_queued_at = gs.pending_loot.front().map(|_| std::time::Instant::now());
        return;
    }
    // Normal(1) / Normal2(3) / LootAll(6) — the server accepted the request.
    gs.loot_confirmed = true;
    gs.loot_last_activity = Some(std::time::Instant::now());

    // ONLY Normal(1)/Normal2(3) carry the corpse's coin: they are the reply to OP_LootRequest built
    // by `Corpse::MakeLootRequestPackets` (zone/corpse.cpp:1139), which is the one place that fills
    // the platinum/gold/silver/copper fields. LootAll(6) is the SoD+ "all items were sent" marker
    // that trails the item packets — it is NOT a fresh accept and must never credit coin.
    //
    // Crediting it happens to be harmless TODAY only because `BasePacket` memsets the buffer
    // (common/base_packet.cpp:31) so a LootAll packet carries zeros. That makes coin correctness
    // load-bearing on a server-side memset — in the exact field whose polarity was already inverted
    // once (see above: every looted coin used to be silently discarded). A two-value match is free;
    // don't bet the purse on someone else's memset (#346 review).
    if !matches!(response, 1 | 3) {
        return;
    }

    let platinum = u32::from_le_bytes([payload[4],  payload[5],  payload[6],  payload[7]]);
    let gold     = u32::from_le_bytes([payload[8],  payload[9],  payload[10], payload[11]]);
    let silver   = u32::from_le_bytes([payload[12], payload[13], payload[14], payload[15]]);
    let copper   = u32::from_le_bytes([payload[16], payload[17], payload[18], payload[19]]);
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

/// OP_LootComplete — a 0-byte packet the server sends for TWO very different outcomes, and it is
/// byte-identical in both, so the packet alone cannot tell them apart (#346 review):
///
///   1. The genuine close: `Corpse::EndLoot` (zone/corpse.cpp:1787), sent in reply to OUR
///      OP_EndLootRequest. This — and only this — means "the loot session finished".
///   2. A server-side ABORT of an item take: `Corpse::LootCorpseItem` (zone/corpse.cpp:1419)
///      answers our OP_LootItem echo with `SendEndLootErrorPacket` (corpse.cpp:50 → a 0-byte
///      OP_LootComplete) on EVERY error path — LORE conflict (corpse.cpp:1535, :1548),
///      cursor-not-empty (corpse.cpp:1459 — a live condition for this client, see #275), loot
///      cooldown, not-the-looter, zoning, and others. The items STAY ON THE CORPSE.
///
/// Reporting (2) as "Looting complete" would be exactly the #346 lie relocated from the timer to
/// the item path: the agent concludes the corpse is done and walks away, leaving the loot behind.
///
/// The discriminator is our own state, not the packet: we only ever send OP_EndLootRequest from
/// `LootTickAction::Close`, which sets `loot_end_requested_at`. So:
///   - `loot_end_requested_at.is_some()` → we asked to close → a genuine completion.
///   - an open, CONFIRMED session that we never asked to close → the server aborted → say so.
///   - anything else (no session, or a session that hasn't been accepted yet) → a stray/late
///     packet (e.g. arriving after our own TimedOut, while the NEXT corpse is already opening).
///     It must not be attributed to that corpse — leave the session untouched.
fn apply_loot_complete(gs: &mut GameState) {
    // (1) Genuine close — this is the ONLY place "Looting complete" may be emitted.
    if gs.loot_end_requested_at.is_some() {
        let corpse_id = gs.loot_current_corpse.take();
        gs.loot_session_active = false;
        gs.loot_confirmed = false;
        gs.loot_last_activity = None;
        gs.loot_end_requested_at = None;
        gs.log_msg("loot", "Looting complete");
        gs.push_event("loot", "complete", "system", true, "Looting complete");
        tracing::info!("EQ: OP_LootComplete received — loot session closed by server (corpse_id={corpse_id:?})");
        gs.loot_queued_at = gs.pending_loot.front().map(|_| std::time::Instant::now());
        return;
    }

    // (2) We never asked to close, but a confirmed session was open → the server aborted an item
    // take (LORE / cursor-not-empty / …). Items remain on the corpse; report it honestly.
    if gs.loot_session_active && gs.loot_confirmed {
        let corpse_id = gs.loot_current_corpse.take();
        gs.loot_session_active = false;
        gs.loot_confirmed = false;
        gs.loot_last_activity = None;
        gs.loot_end_requested_at = None;
        let msg = format!(
            "Loot aborted by the server — items may remain on the corpse (corpse_id={corpse_id:?}). \
             A full cursor or a LORE-conflicting item is the usual cause."
        );
        gs.log_msg("loot", &msg);
        gs.push_event("loot", "aborted", "system", true, &msg);
        tracing::warn!("EQ: {msg}");
        gs.loot_queued_at = gs.pending_loot.front().map(|_| std::time::Instant::now());
        return;
    }

    // (3) Stray/late packet. Do NOT clear state: a session may have just opened for the NEXT
    // corpse (which this packet has nothing to do with — it is 0-byte and carries no corpse id).
    tracing::info!(
        "EQ: stray OP_LootComplete (no close pending, confirmed={}) — ignoring, not attributing it \
         to corpse_id={:?}",
        gs.loot_confirmed, gs.loot_current_corpse,
    );
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
                strip_say_links, SAY_LINK_BODY_SIZE, SIZE_DEATH, SIZE_NEW_ZONE,
                apply_group_update_b, apply_group_join, apply_group_disband_you,
                apply_group_disband_other, apply_group_leader_change, apply_group_invite, apply_group_acknowledge};
    use crate::eq_net::protocol::enc_eq19;
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
        // stale + duplicate cross-zone entities into name→position resolution. The spawn stream
        // that follows repopulates the map for the new zone.
        let mut gs = GameState::new();
        gs.entities.insert(1, test_entity(1, "Fippy_Darkpaw", 100.0));
        gs.entities.insert(2, test_entity(2, "a_gnoll_pup", 100.0));
        assert_eq!(gs.entities.len(), 2);
        super::apply_new_zone(&mut gs, &new_zone_payload("qeynos2", 2));
        assert!(gs.entities.is_empty(), "prior-zone entities must be cleared on zone entry (#270)");
    }

    #[test]
    fn truncated_new_zone_does_not_consume_the_zone_in_apply() {
        // The once-per-zone-in one-shot (#322) must be latched only by a WELL-FORMED OP_NewZone.
        // A truncated one that consumed it would swallow the real NewZone behind it, stranding the
        // client on the PREVIOUS zone's name/id/safe point/underworld clamp (#150) for the whole
        // zone-server session — where before the guard the repeat NewZone self-healed it.
        let mut gs = GameState::new();
        gs.begin_zone_in();
        gs.zone_name = "qeynos2".into();
        gs.zone_id = 2;

        super::apply_new_zone(&mut gs, &[0u8; 16]); // truncated — must be ignored outright
        assert!(!gs.new_zone_applied, "a short OP_NewZone must not latch the one-shot");

        super::apply_new_zone(&mut gs, &new_zone_payload("freporte", 9)); // the real one
        assert_eq!(gs.zone_name, "freporte", "the real OP_NewZone must still be applied");
        assert_eq!(gs.zone_id, 9);
        assert!(gs.zone_underworld.is_some(), "the #150 underworld clamp must be re-seeded");
    }

    /// A RoF2 OP_NewZone payload carrying the fields apply_new_zone reads (short name @64, zone_id @852).
    fn new_zone_payload(short_name: &str, zone_id: u16) -> Vec<u8> {
        let mut p = vec![0u8; SIZE_NEW_ZONE];
        p[64..64 + short_name.len()].copy_from_slice(short_name.as_bytes());
        p[852..854].copy_from_slice(&zone_id.to_le_bytes());
        p
    }

    /// A 100-byte RoF2 Door_Struct record for `door_id`.
    fn door_record(door_id: u8) -> [u8; 100] {
        let mut rec = [0u8; 100];
        rec[..5].copy_from_slice(b"DOOR1");
        rec[52..54].copy_from_slice(&100u16.to_le_bytes()); // size
        rec[60] = door_id;
        rec
    }

    #[test]
    fn second_new_zone_of_a_zone_in_keeps_the_spawn_and_door_stream() {
        // #322: a zone-in delivers OP_NewZone twice (the server sends one on OP_ZoneEntry and
        // answers our OP_ReqNewZone with another). The second copy lands AFTER OP_ReqClientSpawn,
        // so its entity/door purge used to wipe the spawn/door stream already arriving — silently
        // losing the zone's NPCs and doors — and logged "Entered <zone>" + a navigate/zone event a
        // second time. Only the first apply of a zone-server session may clear.
        let mut gs = GameState::new();
        gs.begin_zone_in();
        super::apply_new_zone(&mut gs, &new_zone_payload("qeynos2", 2));

        // OP_ReqClientSpawn is out; the stream it asked for starts landing.
        gs.entities.insert(1, test_entity(1, "Guard_Jordan", 100.0));
        super::apply_spawn_doors(&mut gs, &door_record(7));

        super::apply_new_zone(&mut gs, &new_zone_payload("qeynos2", 2)); // the ReqNewZone reply

        assert_eq!(gs.entities.len(), 1, "the second OP_NewZone must not wipe inbound spawns (#322)");
        assert_eq!(gs.doors.len(), 1, "the second OP_NewZone must not wipe inbound doors (#322)");
        assert_eq!(gs.zone_name, "qeynos2");
        assert_eq!(gs.messages.iter().filter(|m| m.text == "Entered qeynos2").count(), 1,
            "one 'Entered <zone>' message per zone-in");
        assert_eq!(gs.chat_events.iter().filter(|e| e.kind == "zone").count(), 1,
            "one navigate/zone event per zone-in");
    }

    #[test]
    fn new_zone_still_clears_on_the_next_zone_in() {
        // The once-per-zone-in guard (#322) is re-armed by begin_zone_in at the top of each
        // zone-entry handshake, so a real zone change still purges the previous zone (#270) —
        // including a spawn that lands between the handshake's clear and OP_NewZone.
        let mut gs = GameState::new();
        gs.begin_zone_in();
        super::apply_new_zone(&mut gs, &new_zone_payload("qeynos2", 2));
        gs.entities.insert(1, test_entity(1, "Guard_Jordan", 100.0));
        super::apply_spawn_doors(&mut gs, &door_record(7));

        gs.begin_zone_in(); // next zone-server session
        // Stale arrivals between the handshake's clear and OP_NewZone must still be purged by it.
        gs.entities.insert(2, test_entity(2, "Guard_Jordan", 100.0));
        super::apply_spawn_doors(&mut gs, &door_record(4));
        super::apply_new_zone(&mut gs, &new_zone_payload("freporte", 9));

        assert!(gs.entities.is_empty(), "a real zone change must still purge entities (#270)");
        assert!(gs.doors.is_empty(), "a real zone change must still purge doors (#270)");
        assert_eq!(gs.zone_name, "freporte");
        assert_eq!(gs.zone_id, 9);
    }

    #[test]
    fn apply_shop_player_buy_confirmed_echo_spends_coin_and_logs_bought() {
        // #345: a confirmed OP_ShopPlayerBuy echo (32-byte Merchant_Sell_Struct, price@24
        // recomputed server-side) must be the ONLY thing that deducts coin or logs "Bought item" —
        // never the send-time code in navigation.rs. Start with 10pp (10000c) and confirm a 37c buy.
        let mut gs = GameState::new();
        gs.coin = [10, 0, 0, 0]; // 10 platinum = 10000 copper
        let mut echo = [0u8; 32];
        echo[0..4].copy_from_slice(&123u32.to_le_bytes());    // npcid
        echo[4..8].copy_from_slice(&456u32.to_le_bytes());    // playerid
        echo[8..12].copy_from_slice(&7u32.to_le_bytes());     // itemslot
        echo[16..20].copy_from_slice(&1u32.to_le_bytes());    // quantity
        echo[24..28].copy_from_slice(&37u32.to_le_bytes());   // price
        super::apply_shop_player_buy(&mut gs, &echo);
        let total = gs.coin[0] as u64 * 1000 + gs.coin[1] as u64 * 100 + gs.coin[2] as u64 * 10 + gs.coin[3] as u64;
        assert_eq!(total, 10000 - 37, "confirmed buy must deduct the server-echoed price");
        assert!(gs.messages.back().unwrap().text.contains("Bought item"),
            "confirmed buy must log a success message: {}", gs.messages.back().unwrap().text);
    }

    #[test]
    fn apply_shop_end_confirm_reports_refusal_with_no_merchant_session_open() {
        // THE case that matters (#345 review): a fresh client buys from a merchant it is out of
        // range of. Handle_OP_ShopRequest returns SILENTLY for an out-of-range/non-merchant target
        // (zone/client_packet.cpp:14605-14612), so `merchant_open` is NEVER set; the buy then trips
        // the same range check (:14151) and SendMerchantEnd() fires. OP_ShopEndConfirm is thus the
        // ONLY signal the server ever sends — so it must be reported even with no session open.
        // Gating this on `merchant_open` swallowed the refusal entirely and left the agent unable to
        // tell "refused" from "no reply": a quieter lie, but still a lie.
        let mut gs = GameState::new();
        gs.coin = [10, 0, 0, 0];
        assert_eq!(gs.merchant_open, None, "precondition: no merchant session was ever opened");

        super::apply_shop_end_confirm(&mut gs, &[]);

        let last = &gs.messages.back().expect("a refusal must be reported, not swallowed").text;
        assert!(last.contains("refused"), "refusal should say so plainly: {last}");
        assert!(!last.contains("Bought item"), "refusal must not read like a success: {last}");
        assert!(gs.chat_events.iter().any(|e| e.kind == "refused"),
            "a refusal must raise an event the agent can observe, even with no session open");
        assert_eq!(gs.coin, [10, 0, 0, 0], "a refusal must never spend coin");
    }

    #[test]
    fn apply_shop_end_confirm_ends_an_open_session_without_spending() {
        // The same refusal arriving mid-session must also drop the stale merchant state.
        let mut gs = GameState::new();
        gs.coin = [10, 0, 0, 0];
        gs.merchant_open = Some(123);
        super::apply_shop_end_confirm(&mut gs, &[]);
        assert_eq!(gs.merchant_open, None, "refusal ends the merchant session");
        assert_eq!(gs.coin, [10, 0, 0, 0], "a refusal must never spend coin");
    }

    #[test]
    fn apply_shop_player_buy_surfaces_a_coin_desync_instead_of_overstating_the_balance() {
        // spend_coin() deducts NOTHING and returns false when the price exceeds our snapshot. The
        // server has already taken the money (TakeMoneyFromPP), so silently ignoring that would
        // leave `gs.coin` overstated — the client reporting a balance it knows is wrong (#345).
        let mut gs = GameState::new();
        gs.coin = [0, 0, 0, 5]; // 5 copper on hand
        let mut echo = [0u8; 32];
        echo[8..12].copy_from_slice(&7u32.to_le_bytes());   // itemslot
        echo[16..20].copy_from_slice(&1u32.to_le_bytes());  // quantity
        echo[24..28].copy_from_slice(&99u32.to_le_bytes()); // price 99c > 5c on hand

        super::apply_shop_player_buy(&mut gs, &echo);

        assert!(gs.chat_events.iter().any(|e| e.kind == "coin_desync"),
            "an uncoverable confirmed price must be reported as a desync, not silently swallowed");
        assert!(gs.messages.iter().any(|m| m.text.contains("Coin desync")),
            "the desync must be visible in the message log too");
    }

    #[test]
    fn apply_shop_player_buy_confirmed_success_does_not_re_verify_coin() {
        // #361 review (FIX 1): a confirmed, coverable buy applies the correct delta but must NOT
        // flip coin back to verified — an earlier silent refusal could already have diverged the
        // absolute balance, and only a real OP_PlayerProfile can rule that out.
        let mut gs = GameState::new();
        gs.coin = [10, 0, 0, 0];
        gs.coin_confirmed = true; // a real reading had landed at some earlier point
        gs.begin_shop_buy();      // this buy is in flight (as the nav tick does at send time)
        let mut echo = [0u8; 32];
        echo[8..12].copy_from_slice(&7u32.to_le_bytes());
        echo[16..20].copy_from_slice(&1u32.to_le_bytes());
        echo[24..28].copy_from_slice(&37u32.to_le_bytes());
        super::apply_shop_player_buy(&mut gs, &echo);
        assert!(!gs.coin_verified(),
            "a per-buy echo confirms a relative delta only; it cannot restore absolute trust");
    }

    #[test]
    fn apply_shop_player_buy_desync_leaves_coin_unverified() {
        // #361: a confirmed buy whose price we could NOT cover means our balance is known wrong
        // (overstated) — it must stay unverified until the next OP_PlayerProfile reconciles it,
        // not be marked trustworthy just because a packet arrived.
        let mut gs = GameState::new();
        gs.coin = [0, 0, 0, 5];
        gs.coin_confirmed = true;
        gs.begin_shop_buy();
        let mut echo = [0u8; 32];
        echo[8..12].copy_from_slice(&7u32.to_le_bytes());
        echo[16..20].copy_from_slice(&1u32.to_le_bytes());
        echo[24..28].copy_from_slice(&99u32.to_le_bytes()); // > 5c on hand
        super::apply_shop_player_buy(&mut gs, &echo);
        assert!(!gs.coin_verified(), "a detected desync must not be reported as a trustworthy balance");
        assert!(gs.chat_events.iter().any(|e| e.kind == "coin_desync"));
    }

    #[test]
    fn apply_shop_end_confirm_does_not_re_verify_coin() {
        // #361 review (FIX 1): a refusal never spends coin on THIS buy, but that is a relative fact
        // — it cannot rule out an earlier silent refusal, so it must not restore trust. Only a real
        // OP_PlayerProfile (reconcile_coin) may. It still clears merchant_open.
        let mut gs = GameState::new();
        gs.coin = [10, 0, 0, 0];
        gs.coin_confirmed = true;
        gs.merchant_open = Some(111);
        gs.begin_shop_buy();
        super::apply_shop_end_confirm(&mut gs, &[]);
        assert!(!gs.coin_verified(), "a refusal echo confirms one buy's cost only, not absolute trust");
        assert_eq!(gs.merchant_open, None, "the refusal still honestly closes the merchant");
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
        gs.set_target(450);
        let mut reply = [0u8; 20];
        reply[4..8].copy_from_slice(&450u32.to_le_bytes());   // targetid
        reply[8..12].copy_from_slice(&9u32.to_le_bytes());     // faction 9 = scowls, ready to attack
        reply[12..16].copy_from_slice(&2u32.to_le_bytes());    // level (con color)
        super::apply_consider(&mut gs, &reply);
        let m = gs.messages.back().unwrap().text.clone();
        assert!(m.contains("Guard_Phaeton") && m.contains("scowls"), "attitude line: {m}");
        // #355 M1: assert the con color's VALUE, not just is_some(). The con RGB must derive from
        // the difficulty LEVEL (@12), never the faction/attitude (@8). Asserting only is_some()
        // let a `con_color(level) -> con_color(faction)` mutation survive — the /observe/debug tint
        // is the field an agent reads to decide "can I win this fight", so a wrong-but-present value
        // is the worst class of lie.
        assert_eq!(gs.target_con, Some(con_color(2)), "con tint must come from level (2=green), not faction (9)");
    }

    #[test]
    fn apply_consider_con_color_comes_from_level_not_faction() {
        // #355 M1, the lethal-mob scenario made explicit: a red-con (much higher level, would kill
        // you) mob that also happens to hash to a low faction number must still tint RED. If the con
        // color is (mis)computed from the faction field, this dangerous mob reports a SAFE green con
        // and an agent picks a fight it cannot win. level and faction are chosen so con_color differs
        // between them, so the mutation cannot hide.
        let mut gs = GameState::new();
        gs.entities.insert(77, test_entity(77, "a_dragon", 100.0));
        gs.set_target(77);
        let mut reply = [0u8; 20];
        reply[4..8].copy_from_slice(&77u32.to_le_bytes());     // targetid
        reply[8..12].copy_from_slice(&2u32.to_le_bytes());      // faction 2 -> con_color(2) = GREEN (safe)
        reply[12..16].copy_from_slice(&13u32.to_le_bytes());    // level 13 -> con_color(13) = RED (lethal)
        super::apply_consider(&mut gs, &reply);
        assert_ne!(con_color(2), con_color(13), "test premise: the two inputs must map to different colors");
        assert_eq!(gs.target_con, Some(con_color(13)),
            "a lethal (level-13/red) mob must tint RED from its level, never GREEN from its faction (#355 M1)");
    }

    #[test]
    fn apply_consider_standalone_non_target_spawn_still_logs_the_con_line() {
        // POST /v1/combat/consider {"id":N} sends OP_CONSIDER for an ARBITRARY spawn with no
        // set_target (the "Standalone consider" send site, navigation.rs:1932) — conning a spawn
        // that is deliberately NOT your target is the endpoint's whole purpose. The reply must
        // still produce its chat line, or the endpoint is a silent no-op (200 OK, then nothing).
        // The con FIELDS are target-scoped, so they must stay untouched here (#330).
        let mut gs = GameState::new();
        gs.entities.insert(450, test_entity(450, "Guard_Phaeton", 100.0));
        // no set_target — nothing is targeted
        let mut reply = [0u8; 20];
        reply[4..8].copy_from_slice(&450u32.to_le_bytes());   // targetid
        reply[8..12].copy_from_slice(&9u32.to_le_bytes());     // faction 9 = scowls
        reply[12..16].copy_from_slice(&2u32.to_le_bytes());    // level (con color)
        super::apply_consider(&mut gs, &reply);
        let m = gs.messages.back().unwrap().text.clone();
        assert!(m.contains("Guard_Phaeton") && m.contains("scowls"),
            "standalone consider must still log its result line: {m}");
        assert_eq!(gs.target_id, None, "a consider reply must never select a target");
        assert_eq!(gs.target_con, None, "con fields are current-target-scoped; nothing is targeted");
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
        // These three fields describe the CURRENT target (that's how /observe/debug exposes them),
        // so this test targets the spawn first — see the standalone-consider test above for the
        // non-target path, where the line is logged but these fields stay untouched (#330).
        let mut gs = GameState::new();
        gs.entities.insert(450, test_entity(450, "a_guard", 100.0));
        gs.set_target(450);
        let mut reply = [0u8; 20];
        reply[4..8].copy_from_slice(&450u32.to_le_bytes());   // targetid
        reply[8..12].copy_from_slice(&9u32.to_le_bytes());     // faction 9 = scowls
        reply[12..16].copy_from_slice(&13u32.to_le_bytes());   // level 13 = red
        super::apply_consider(&mut gs, &reply);
        assert_eq!(gs.target_con_name.as_deref(), Some("red"), "difficulty tier stored");
        assert_eq!(gs.target_attitude.as_deref(), Some("scowls"), "attitude enum stored");
    }

    #[test]
    fn apply_consider_stale_reply_logs_but_does_not_overwrite_current_target_con() {
        // #330: a manual target on A sends OP_CONSIDER, but the reply can land after we've
        // already retargeted to B (auto-combat retarget, hail, or a second manual target).
        // The stale reply must NOT clobber target_id/con back to A while target_name/target_hp_pct
        // still hold B's values (the split-brain bug) — but it must STILL log its chat line: the
        // reply is a legitimate con result for A, and the same code path serves the standalone
        // /v1/combat/consider endpoint, which cons non-target spawns on purpose.
        let mut gs = GameState::new();
        gs.entities.insert(1, test_entity(1, "mob_a", 80.0));
        gs.entities.insert(2, test_entity(2, "mob_b", 55.0));
        gs.set_target(1); // target A
        gs.set_target(2); // retarget to B before A's consider reply arrives

        let mut reply = [0u8; 20];
        reply[4..8].copy_from_slice(&1u32.to_le_bytes());     // targetid = A (stale)
        reply[8..12].copy_from_slice(&9u32.to_le_bytes());     // faction 9 = scowls
        reply[12..16].copy_from_slice(&13u32.to_le_bytes());   // level 13 = red
        super::apply_consider(&mut gs, &reply);

        assert_eq!(gs.target_id, Some(2), "target_id must still be B, not snap back to A");
        assert_eq!(gs.target_name.as_deref(), Some("mob_b"), "target_name must still be B's");
        assert_eq!(gs.target_hp_pct, Some(55.0), "target_hp_pct must still be B's");
        assert_eq!(gs.target_con, None, "A's stale con must not be applied to B");
        assert_eq!(gs.target_con_name, None, "A's stale con_name must not be applied to B");
        assert_eq!(gs.target_attitude, None, "A's stale attitude must not be applied to B");

        let m = gs.messages.back().unwrap().text.clone();
        assert!(m.contains("mob_a") && m.contains("scowls"),
            "the con result for A must still be logged, not silently swallowed: {m}");
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
    fn apply_combat_damage_melee_swing_is_not_reported_as_a_spellcast() {
        // #417 (AGENT-HONESTY): RoF2's pure-melee sentinel for CombatDamage_Struct.spellid@5 is
        // SPELL_UNKNOWN (0xFFFF = 65535), NOT 0 — EQEmu's Mob::Damage takes spell_id as a uint16
        // and every melee call site passes SPELL_UNKNOWN, which zero-extends onto this wire's
        // uint32 field as 0x0000FFFF. A `spellid != 0` classification therefore misreports every
        // melee hit as "casts a spell on" / "'s spell hits ... for N damage" — a silent wrong
        // answer in the combat log. See docs/eq-technical-knowledgebase/combat-damage-struct.md.
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

        // A melee hit whose spellid is the SPELL_UNKNOWN sentinel (0xFFFF) must render as a
        // melee swing, never as a spellcast.
        apply_combat_damage(&mut gs, &spell(7, 99, 0xFFFF, 24));
        let m = last(&gs);
        assert!(m.contains("hits") && m.contains("for 24 damage"),
            "SPELL_UNKNOWN melee hit should read as melee wording: {m}");
        assert!(!m.contains("casts a spell") && !m.contains("'s spell hits"),
            "SPELL_UNKNOWN melee hit must NOT be reported as a spellcast: {m}");

        // A melee miss with the same sentinel must render as a melee miss, not a spell.
        apply_combat_damage(&mut gs, &spell(7, 99, 0xFFFF, 0));
        let m = last(&gs);
        assert!(m.contains("misses") || m.contains("tries to hit"),
            "SPELL_UNKNOWN melee miss should read as a melee miss: {m}");
        assert!(!m.contains("casts a spell"), "SPELL_UNKNOWN miss must NOT be a spellcast: {m}");

        // Sanity: a GENUINE spellid (real cast, e.g. a nuke) is unaffected by the fix and still
        // renders with spell wording.
        apply_combat_damage(&mut gs, &spell(7, 99, 300, 40));
        let m = last(&gs);
        assert!(m.contains("for 40 damage"), "genuine spell damage still shows the amount: {m}");
        assert!(!m.contains("misses"), "genuine spell must not read as a melee miss: {m}");
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
        apply_spawn_appearance(&mut gs, &crate::eq_net::protocol::build_spawn_appearance_packet(77, 14, 110));
        assert!(gs.sitting, "sit appearance for our player must set render sitting");
        // param 100 (stand) -> not sitting.
        apply_spawn_appearance(&mut gs, &crate::eq_net::protocol::build_spawn_appearance_packet(77, 14, 100));
        assert!(!gs.sitting, "stand appearance clears render sitting");
        // Another spawn's sit must NOT change our flag.
        apply_spawn_appearance(&mut gs, &crate::eq_net::protocol::build_spawn_appearance_packet(77, 14, 110));
        apply_spawn_appearance(&mut gs, &crate::eq_net::protocol::build_spawn_appearance_packet(999, 14, 100));
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

    /// Build a MoneyOnCorpse_Struct payload (20 bytes): response(u8) + 3×pad + pp/gp/sp/cp (u32 LE).
    fn money_on_corpse_payload(response: u8, pp: u32, gp: u32, sp: u32, cp: u32) -> Vec<u8> {
        let mut p = vec![response, 0, 0, 0];
        for v in [pp, gp, sp, cp] { p.extend_from_slice(&v.to_le_bytes()); }
        p
    }

    #[test]
    fn money_on_corpse_adds_looted_coin_on_normal_response() {
        // #346: response=1 (LootResponse::Normal, verified against EQEmu zone/corpse.cpp
        // MakeLootRequestPackets) is the server's ACCEPT — not response=0 (that's SomeoneElse, a
        // REFUSAL). The old code had this backwards.
        let mut gs = GameState::new();
        gs.coin = [10, 0, 5, 0];
        gs.loot_session_active = true;
        let p = money_on_corpse_payload(1, 2, 1, 0, 3);
        apply_money_on_corpse(&mut gs, &p);
        assert_eq!(gs.coin, [12, 1, 5, 3]); // added on top of existing
        assert!(gs.loot_confirmed, "a Normal response must confirm the session opened");
        assert!(gs.loot_session_active, "an accepted request must not close the session");
    }

    #[test]
    fn money_on_corpse_normal2_response_also_confirms() {
        let mut gs = GameState::new();
        let p = money_on_corpse_payload(3, 0, 0, 0, 0);
        apply_money_on_corpse(&mut gs, &p);
        assert!(gs.loot_confirmed, "Normal2(3) is documented as behaving exactly like Normal(1)");
    }

    /// Core #346 regression: response=0 (SomeoneElse) is a REFUSAL, not the "OK, add zero coins"
    /// case the old (inverted) polarity treated it as. A refusal must close the session, leave
    /// `loot_confirmed` false, and emit a distinct message — never anything resembling success.
    #[test]
    fn money_on_corpse_someone_else_response_is_a_refusal_not_a_success() {
        let mut gs = GameState::new();
        gs.coin = [10, 0, 5, 0];
        gs.loot_session_active = true;
        gs.loot_current_corpse = Some(42);
        let p = money_on_corpse_payload(0, 99, 99, 99, 99); // coin fields must be ignored
        apply_money_on_corpse(&mut gs, &p);
        assert_eq!(gs.coin, [10, 0, 5, 0], "a refused loot must not credit any coin");
        assert!(!gs.loot_session_active, "a refusal must close the session");
        assert!(!gs.loot_confirmed);
        assert_eq!(gs.loot_current_corpse, None);
        let last = gs.messages.back().expect("a refusal must log a message");
        assert_eq!(last.kind, "loot");
        assert!(last.text.to_lowercase().contains("refus"), "got: {:?}", last.text);
        assert_ne!(last.text, "Looting complete", "a refusal must never read as success");
        let ev = gs.chat_events.back().expect("a refusal must push an agent-visible event");
        assert_eq!(ev.kind, "refused");
    }

    #[test]
    fn money_on_corpse_too_far_response_is_also_a_refusal() {
        let mut gs = GameState::new();
        let p = money_on_corpse_payload(5, 0, 0, 0, 0); // TooFar
        apply_money_on_corpse(&mut gs, &p);
        assert!(!gs.loot_session_active);
        assert!(!gs.loot_confirmed);
        let last = gs.messages.back().unwrap();
        assert!(last.text.contains("too far"), "got: {:?}", last.text);
    }

    /// LootAll(6) is the SoD+ "all items were sent" marker that follows a SUCCESSFUL loot's item
    /// packets — it must NOT be treated as a refusal (a naive default-branch-is-refusal
    /// implementation would misfire here).
    #[test]
    fn money_on_corpse_loot_all_marker_is_not_a_refusal() {
        let mut gs = GameState::new();
        gs.loot_session_active = true;
        gs.loot_confirmed = true;
        let p = money_on_corpse_payload(6, 0, 0, 0, 0);
        apply_money_on_corpse(&mut gs, &p);
        assert!(gs.loot_session_active, "LootAll must not close the session");
        assert!(gs.loot_confirmed);
    }

    /// #346 REVIEW. Only Normal(1)/Normal2(3) — the reply to OP_LootRequest built by
    /// `Corpse::MakeLootRequestPackets` (zone/corpse.cpp:1139) — carry the corpse's coin. LootAll(6)
    /// is the trailing "all items were sent" marker and must credit NOTHING.
    ///
    /// This is deliberately fed a coin-BEARING LootAll payload, which cannot occur on the wire today
    /// (`BasePacket` memsets the buffer, common/base_packet.cpp:31 — so a real LootAll carries
    /// zeros). That memset is exactly the point: without this guard, coin correctness would be
    /// load-bearing on a server-side memset in the very field whose polarity was already inverted
    /// once. If EQEmu ever populates it, this test is what stops us crediting phantom coin.
    #[test]
    fn money_on_corpse_loot_all_marker_never_credits_coin() {
        let mut gs = GameState::new();
        gs.coin = [10, 0, 5, 0];
        gs.loot_session_active = true;
        gs.loot_confirmed = true;
        let before = gs.messages.len();
        let p = money_on_corpse_payload(6, 99, 99, 99, 99); // coin fields must be ignored outright
        apply_money_on_corpse(&mut gs, &p);
        assert_eq!(gs.coin, [10, 0, 5, 0],
            "LootAll(6) is an end-of-items marker, not a coin-bearing accept — it must credit nothing");
        assert_eq!(gs.messages.len(), before, "and it must not announce looted coins");
        // It is still an accept, not a refusal: the session stays open and confirmed.
        assert!(gs.loot_session_active);
        assert!(gs.loot_confirmed);
    }

    /// The ONLY inbound packet allowed to log "Looting complete" per the agent-honesty invariant
    /// (#346) is OP_LootComplete — verified this is what EQEmu's Corpse::EndLoot actually sends.
    #[test]
    fn loot_complete_logs_and_pushes_completion_only_when_a_session_was_open() {
        let mut gs = GameState::new();
        gs.loot_session_active = true;
        gs.loot_confirmed = true;
        gs.loot_current_corpse = Some(7);
        // We asked the server to close (LootTickAction::Close sent OP_EndLootRequest) — this is the
        // ONLY shape in which OP_LootComplete means "finished".
        gs.loot_end_requested_at = Some(std::time::Instant::now());
        super::apply_loot_complete(&mut gs);
        assert!(!gs.loot_session_active);
        assert!(!gs.loot_confirmed);
        assert_eq!(gs.loot_current_corpse, None);
        let last = gs.messages.back().expect("must log a message");
        assert_eq!(last.text, "Looting complete");
        let ev = gs.chat_events.back().expect("must push an agent-visible event");
        assert_eq!(ev.kind, "complete");
        assert_eq!(ev.text, "Looting complete");
    }

    /// The completion message must be reachable from a REAL inbound packet, not just by calling
    /// the handler directly — i.e. OP_LOOT_COMPLETE must actually be dispatched in `apply_packet`.
    /// Without the dispatch arm, "Looting complete" would be unreachable in the field (#346).
    #[test]
    fn apply_packet_dispatches_op_loot_complete_to_the_completion_handler() {
        use crate::eq_net::transport::AppPacket;
        let mut gs = GameState::new();
        gs.loot_session_active = true;
        gs.loot_confirmed = true;
        gs.loot_current_corpse = Some(7);
        gs.loot_end_requested_at = Some(std::time::Instant::now());
        super::apply_packet(&mut gs, &AppPacket {
            opcode: crate::eq_net::protocol::OP_LOOT_COMPLETE,
            payload: Vec::new(), // OP_LootComplete is a 0-byte packet (corpse.cpp EndLoot)
        });
        assert!(!gs.loot_session_active, "the inbound packet must close the session");
        let last = gs.messages.back().expect("must log a message");
        assert_eq!(last.text, "Looting complete");
    }

    /// #346 REVIEW REGRESSION. EQEmu's `Corpse::LootCorpseItem` (zone/corpse.cpp:1419) answers our
    /// OP_LootItem echo with `SendEndLootErrorPacket` (corpse.cpp:50) on EVERY error path — LORE
    /// conflict (:1535, :1548), cursor-not-empty (:1459, a live condition for this client — #275),
    /// loot cooldown, not-the-looter, zoning… That packet is a 0-byte OP_LootComplete, BYTE-IDENTICAL
    /// to the genuine close from `Corpse::EndLoot` (:1787). The items STAY ON THE CORPSE.
    ///
    /// Reporting it as "Looting complete" would be #346's exact lie, just relocated from the timer
    /// to the item path: the agent would conclude the corpse was done and walk away from the loot.
    /// The only discriminator is our own `loot_end_requested_at` — we never asked to close here.
    #[test]
    fn server_abort_mid_item_take_is_reported_as_an_abort_not_a_completion() {
        let mut gs = GameState::new();
        gs.loot_session_active = true;
        gs.loot_confirmed = true;          // the corpse DID open (OP_MoneyOnCorpse accepted)
        gs.loot_current_corpse = Some(7);
        gs.loot_end_requested_at = None;   // we never sent OP_EndLootRequest — the server aborted
        super::apply_loot_complete(&mut gs);

        let last = gs.messages.back().expect("an abort must log a message");
        assert_ne!(last.text, "Looting complete",
            "a server-side abort must NEVER be reported as a completed loot — the items are still there");
        assert!(last.text.to_lowercase().contains("abort"), "got: {:?}", last.text);

        let ev = gs.chat_events.back().expect("an abort must push an agent-visible event");
        assert_eq!(ev.kind, "aborted",
            "an agent polling /v1/events/loot must be able to tell an abort from a completion");
        assert_ne!(ev.kind, "complete");

        // The session is over either way — but it ended dishonestly-free.
        assert!(!gs.loot_session_active);
        assert_eq!(gs.loot_current_corpse, None);
    }

    /// #346 REVIEW REGRESSION (second hole). OP_LootComplete is 0-byte, so it carries no corpse id
    /// and cannot be correlated. If our own `TimedOut` gave up on corpse 7 and the queue moved on to
    /// corpse 9, a LATE OP_LootComplete for corpse 7 must not be claimed as success for corpse 9 —
    /// a corpse that has not even been accepted by the server yet.
    #[test]
    fn late_loot_complete_after_timeout_is_not_attributed_to_the_next_corpse() {
        let mut gs = GameState::new();
        // State after TimedOut on corpse 7, with corpse 9 just opened (OP_LootRequest sent, no ack).
        gs.loot_session_active = true;
        gs.loot_confirmed = false;         // corpse 9 has NOT been accepted yet
        gs.loot_current_corpse = Some(9);
        gs.loot_end_requested_at = None;   // we have not asked to close corpse 9
        let msgs_before = gs.messages.len();
        let evs_before = gs.chat_events.len();

        super::apply_loot_complete(&mut gs); // the late packet for corpse 7 arrives

        assert_eq!(gs.messages.len(), msgs_before,
            "a stray OP_LootComplete must not announce anything about the next corpse");
        assert_eq!(gs.chat_events.len(), evs_before);
        // …and it must not tear down the in-flight session for corpse 9.
        assert!(gs.loot_session_active, "the next corpse's session must survive a stray packet");
        assert_eq!(gs.loot_current_corpse, Some(9));
    }

    /// A late/duplicate OP_LootComplete after the session was already considered closed (e.g. a
    /// TimedOut gave up first) must not re-announce success.
    #[test]
    fn loot_complete_with_no_active_session_does_not_log_a_duplicate_success() {
        let mut gs = GameState::new();
        gs.loot_session_active = false;
        let before = gs.messages.len();
        super::apply_loot_complete(&mut gs);
        assert_eq!(gs.messages.len(), before, "no session was open — nothing to announce");
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

    // --- self-echo filter (#325) ---
    //
    // EQEmu broadcasts channel messages back to the SENDING client, not just other listeners
    // (zone/entity.cpp ChannelMessageSend loops include the sender with no skip; confirmed
    // live against the running server). The outgoing-chat code in navigation.rs already writes
    // the "You say/tell/..." line into the log when we send, so this inbound bounce must be
    // dropped entirely — it must not double the message log AND it must never surface as a
    // /v1/events/chat event (an agent would otherwise "hear" its own outbound message as if
    // someone else said it).

    #[test]
    fn apply_channel_message_self_say_is_dropped_entirely() {
        let mut gs = GameState::new();
        gs.player_name = "Mordeth".to_string();
        super::apply_channel_message(&mut gs, &make_chan_payload("Mordeth", 8, "echo probe"));
        assert!(gs.messages.is_empty(), "our own say bouncing back must not be logged again");
        assert!(gs.chat_events.is_empty());
    }

    #[test]
    fn apply_channel_message_self_shout_is_dropped_from_log_and_events() {
        let mut gs = GameState::new();
        gs.player_name = "Mordeth".to_string();
        super::apply_channel_message(&mut gs, &make_chan_payload("Mordeth", 3, "shout echo"));
        assert!(gs.messages.is_empty(), "our own shout bouncing back must not be logged again");
        assert!(gs.chat_events.is_empty(), "our own shout must never appear as an inbound chat event");
    }

    #[test]
    fn apply_channel_message_self_ooc_group_guild_gmsay_are_dropped() {
        for chan in [0u32, 2, 5, 11] {
            let mut gs = GameState::new();
            gs.player_name = "Mordeth".to_string();
            super::apply_channel_message(&mut gs, &make_chan_payload("Mordeth", chan, "self bounce"));
            assert!(gs.messages.is_empty(), "chan {chan}: self bounce must not be logged");
            assert!(gs.chat_events.is_empty(), "chan {chan}: self bounce must not be a chat event");
        }
    }

    #[test]
    fn apply_channel_message_self_tell_echo_chan14_is_dropped() {
        // EQEmu echoes a tell back to the SENDER on chan_num 14 (ChatChannel_TellEcho), NOT 7
        // (zone/worldserver.cpp: the world mutates chan_num to 14 before relaying the echo back
        // to the sender's zone). This chan number isn't in the tell/ooc/etc. event_channel match
        // at all, so without the self filter it would fall through to the generic "chat" bucket
        // and log as "<Mordeth> ..." — a second, differently-formatted line for the same tell.
        let mut gs = GameState::new();
        gs.player_name = "Mordeth".to_string();
        super::apply_channel_message(&mut gs, &make_chan_payload_to("Mordeth", "Katie", 14, "you stuck?"));
        assert!(gs.messages.is_empty(), "the tell echo (chan 14) must not be logged again");
        assert!(gs.chat_events.is_empty());
    }

    #[test]
    fn apply_channel_message_self_tell_chan7_sender_is_us_is_dropped() {
        // Defensive: even if a chan-7 tell packet ever arrived with US as the sender (rather
        // than the documented chan-14 echo), the filter is keyed on sender identity, not the
        // channel number, so it still must not be logged or evented.
        let mut gs = GameState::new();
        gs.player_name = "Mordeth".to_string();
        super::apply_channel_message(&mut gs, &make_tell("Mordeth", "Katie", "hi"));
        assert!(gs.messages.is_empty());
        assert!(gs.chat_events.is_empty());
    }

    #[test]
    fn apply_channel_message_self_filter_is_case_insensitive() {
        let mut gs = GameState::new();
        gs.player_name = "Mordeth".to_string();
        super::apply_channel_message(&mut gs, &make_chan_payload("MORDETH", 5, "case check"));
        assert!(gs.messages.is_empty(), "sender name case must not defeat the self filter");
        assert!(gs.chat_events.is_empty());
    }

    #[test]
    fn apply_channel_message_other_player_still_logs_when_player_name_unset() {
        // Guard against an over-eager filter: before we've learned our own player_name (empty
        // string, e.g. mid zone-transition), nothing should be treated as a self-echo.
        let mut gs = GameState::new();
        assert_eq!(gs.player_name, "");
        super::apply_channel_message(&mut gs, &make_chan_payload("Garrik", 3, "hello zone"));
        assert!(gs.messages.iter().any(|m| m.kind == "shout" && m.text == "<Garrik> hello zone"));
    }

    #[test]
    fn apply_channel_message_other_player_same_channel_still_logged_and_evented() {
        // Requirement 3: genuine inbound traffic from OTHER players must be unaffected by the
        // self filter, on every channel the filter touches.
        let mut gs = GameState::new();
        gs.player_name = "Mordeth".to_string();
        super::apply_channel_message(&mut gs, &make_chan_payload("Garrik", 3, "hello zone"));
        assert!(gs.messages.iter().any(|m| m.kind == "shout" && m.text == "<Garrik> hello zone"));
        let e = gs.chat_events.back().expect("a chat event from another player");
        assert_eq!(e.from, "Garrik");
        assert_eq!(e.kind, "shout");
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
        use crate::eq_net::protocol::SIZE_WEAR_CHANGE;
        let mut gs = GameState::new();
        // Craft a packet whose bytes, if the short-packet length guard were removed, would
        // zero-pad (via `safe_read`) into a VALID-looking WearChange for the local player:
        //   spawn_id = 0x0201 (= player_id), material = 17, slot/color = 0 (padded).
        // #355 M2: the guard deletion survived because this test had ZERO assertions and
        // `safe_read`'s zero-padding guarantees no panic — so "must not panic" proved nothing.
        // A truncated packet must be REJECTED, not decoded into garbage equipment.
        // WearChange_S is #[repr(C, packed)] => SIZE_WEAR_CHANGE is exactly 9.
        gs.player_id = 0x0201;
        let short = [0x01u8, 0x02, 17, 0]; // len 4, well below SIZE_WEAR_CHANGE (9)
        assert!(short.len() < SIZE_WEAR_CHANGE, "test premise: packet is genuinely short");
        apply_wear_change(&mut gs, &short); // must not panic AND must not mutate state
        assert_eq!(gs.player_equipment, [0u32; 9],
            "a short WearChange must be rejected by the length guard, not zero-pad-decoded \
             into garbage equipment for the local player (#355 M2)");
        assert_eq!(gs.player_equipment_tint, [[0u8; 3]; 9], "tint must also stay untouched");

        // Boundary case — pin the EXACT cutoff, not just "very short". A packet of
        // SIZE_WEAR_CHANGE-1 (8) bytes already fills every field that matters here
        // (spawn_id@0, material@2, color@4..8 — only the trailing wear_slot_id@8 is
        // missing, so it zero-pads to slot 0). It must STILL be rejected. Without this case
        // the test catches a guard DELETE but not a guard RELAX (e.g. `< SIZE_WEAR_CHANGE`
        // -> `< 5`), which would let lengths 5..=8 decode into valid-looking garbage
        // equipment — the very "silent garbage from a short packet" impact M2 exists to prevent.
        let boundary = [0x01u8, 0x02, 17, 0, 0, 0, 0, 0]; // len 8 = SIZE_WEAR_CHANGE-1; material 17, slot 0 (padded)
        assert_eq!(boundary.len(), SIZE_WEAR_CHANGE - 1, "test premise: one byte short of the full struct");
        apply_wear_change(&mut gs, &boundary);
        assert_eq!(gs.player_equipment, [0u32; 9],
            "a WearChange one byte short of SIZE_WEAR_CHANGE must be rejected too — the guard must \
             pin the exact cutoff, not merely 'very short' (#355 M2, overfit guard)");
        assert_eq!(gs.player_equipment_tint, [[0u8; 3]; 9], "tint must also stay untouched at the boundary");
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

    // ── #348: cast OUTCOMES must reach the agent (events + last_cast), not just `tracing` ────────
    // Wire ground truth (EQEmu):
    //   OP_BeginCast      zone/spells.cpp:497   → cast started
    //   OP_InterruptCast  zone/spells.cpp:1299  → InterruptCast_Struct{spawnid, messageid}
    //   OP_MemorizeSpell  zone/spells.cpp:1824  → scribing=3 (memSpellSpellbar) = cast COMPLETED
    //   OP_SimpleMessage  zone/client.cpp:3811  → the ONLY signal for a player FIZZLE (string 173)
    //   OP_ManaChange     zone/spells.cpp:1369  → keepcasting=0 names the spell that just ended

    /// Build the 10-byte RoF2 BeginCast_Struct.
    fn begin_cast_pkt(caster: u16, spell: u32, ms: u32) -> Vec<u8> {
        let mut b = vec![0u8; 10];
        b[0..4].copy_from_slice(&spell.to_le_bytes());
        b[4..6].copy_from_slice(&caster.to_le_bytes());
        b[6..10].copy_from_slice(&ms.to_le_bytes());
        b
    }
    /// InterruptCast_Struct: spawnid u32@0, messageid u32@4.
    fn interrupt_pkt(spawnid: u32, messageid: u32) -> Vec<u8> {
        let mut b = vec![0u8; 8];
        b[0..4].copy_from_slice(&spawnid.to_le_bytes());
        b[4..8].copy_from_slice(&messageid.to_le_bytes());
        b
    }
    /// MemorizeSpell_Struct: slot u32@0, spell_id u32@4, scribing u32@8.
    fn memorize_pkt(slot: u32, spell: u32, scribing: u32) -> Vec<u8> {
        let mut b = vec![0u8; 12];
        b[0..4].copy_from_slice(&slot.to_le_bytes());
        b[4..8].copy_from_slice(&spell.to_le_bytes());
        b[8..12].copy_from_slice(&scribing.to_le_bytes());
        b
    }
    /// SimpleMessage_Struct: string_id u32@0, color u32@4, unknown u32@8.
    fn simple_msg_pkt(string_id: u32) -> Vec<u8> {
        let mut b = vec![0u8; 12];
        b[0..4].copy_from_slice(&string_id.to_le_bytes());
        b
    }
    /// ManaChange_Struct: new_mana u32@0, stamina u32@4, spell_id u32@8, keepcasting u8@12, slot i32@16.
    fn mana_change_pkt(new_mana: u32, spell: u32, keepcasting: u8) -> Vec<u8> {
        let mut b = vec![0u8; 20];
        b[0..4].copy_from_slice(&new_mana.to_le_bytes());
        b[8..12].copy_from_slice(&spell.to_le_bytes());
        b[12] = keepcasting;
        b
    }
    fn event_kinds(gs: &GameState) -> Vec<String> {
        gs.chat_events.iter().map(|e| e.kind.clone()).collect()
    }

    #[test]
    fn begin_cast_publishes_a_cast_begin_event() {
        let mut gs = GameState::new();
        gs.player_id = 42;
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 202, 2500));
        assert!(gs.casting.is_some(), "cast bar set");
        assert_eq!(event_kinds(&gs), ["cast_begin"],
            "the agent must be able to learn the cast STARTED from /v1/events, not just tracing");
        // An NPC's cast must not manufacture an event for us (eqoxide#222's class of bug).
        let mut gs2 = GameState::new();
        gs2.player_id = 42;
        super::apply_begin_cast(&mut gs2, &begin_cast_pkt(999, 202, 2500));
        assert!(event_kinds(&gs2).is_empty(), "someone else's cast is not our cast");
    }

    #[test]
    fn completed_cast_publishes_cast_completed_with_the_spell_id() {
        // The server re-enables the spell bar via OP_MemorizeSpell scribing=3 ONLY from the tail of
        // SpellFinished (zone/spells.cpp:1824) — i.e. the cast landed.
        let mut gs = GameState::new();
        gs.player_id = 42;
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 202, 2500));
        super::apply_memorize_spell(&mut gs, &memorize_pkt(3, 202, 3));
        assert!(gs.casting.is_none(), "cast bar cleared on completion");
        assert_eq!(event_kinds(&gs), ["cast_begin", "cast_completed"]);
        let last = gs.last_cast.as_ref().expect("outcome recorded for /v1/observe/debug");
        assert_eq!(last.kind, "cast_completed");
        assert_eq!(last.spell_id, 202);
    }

    #[test]
    fn interrupted_cast_publishes_cast_interrupted_and_ignores_other_casters() {
        let mut gs = GameState::new();
        gs.player_id = 42;
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 202, 2500));
        // Another caster's interrupt is broadcast to everyone nearby (zone/spells.cpp:1339): it must
        // neither clear OUR cast bar nor emit an outcome for us.
        super::apply_interrupt_cast(&mut gs, &interrupt_pkt(999, crate::game_state::INTERRUPT_SPELL));
        assert!(gs.casting.is_some(), "a passing NPC's interrupt must not clear our cast bar");
        assert_eq!(event_kinds(&gs), ["cast_begin"]);
        // Ours does.
        super::apply_interrupt_cast(&mut gs, &interrupt_pkt(42, crate::game_state::INTERRUPT_SPELL));
        assert!(gs.casting.is_none());
        assert_eq!(event_kinds(&gs), ["cast_begin", "cast_interrupted"]);
        let last = gs.last_cast.as_ref().unwrap();
        assert_eq!(last.kind, "cast_interrupted");
        assert_eq!(last.spell_id, 202, "the interrupted spell is named from the in-flight cast");
    }

    #[test]
    fn fizzle_is_distinguishable_from_an_interrupt_and_names_the_spell() {
        // A PLAYER fizzle never sends OP_BeginCast or OP_InterruptCast: CheckFizzle fails inside
        // DoCastSpell (zone/spells.cpp:318) *before* SendBeginCast (:499). All the client gets is
        // OP_ManaChange (keepcasting=0, naming the spell) + OP_SimpleMessage{SPELL_FIZZLE=173}.
        let mut gs = GameState::new();
        gs.player_id = 42;
        super::apply_mana_change(&mut gs, &mana_change_pkt(90, 202, 0));
        super::apply_simple_message(&mut gs, &simple_msg_pkt(crate::game_state::SPELL_FIZZLE));
        assert_eq!(event_kinds(&gs), ["cast_fizzled"],
            "a fizzle must be its own outcome — not silence, and not 'interrupted'");
        let last = gs.last_cast.as_ref().expect("fizzle recorded");
        assert_eq!(last.kind, "cast_fizzled");
        assert_eq!(last.spell_id, 202,
            "OP_ManaChange(keepcasting=0) is the only thing that names a fizzled spell");
    }

    #[test]
    fn cast_start_refusal_is_reported_once_not_double_reported() {
        // Insufficient mana: the server sends OP_SimpleMessage{199} and THEN InterruptSpell() →
        // OP_InterruptCast{439} (zone/spells.cpp:484-496). The precise reason must win; the trailing
        // generic "interrupted" must not overwrite it with a second, vaguer event.
        let mut gs = GameState::new();
        gs.player_id = 42;
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 202, 2500));
        super::apply_simple_message(&mut gs, &simple_msg_pkt(199)); // INSUFFICIENT_MANA
        super::apply_interrupt_cast(&mut gs, &interrupt_pkt(42, crate::game_state::INTERRUPT_SPELL));
        assert_eq!(event_kinds(&gs), ["cast_begin", "cast_failed"]);
        assert_eq!(gs.last_cast.as_ref().unwrap().kind, "cast_failed");
    }

    #[test]
    fn a_stale_ended_cast_spell_is_never_pinned_on_a_later_cast() {
        // ended_cast_spell is a one-shot hint consumed by the outcome. If it survived, the NEXT
        // failure would be reported against the PREVIOUS spell — a lie with a plausible spell name.
        let mut gs = GameState::new();
        gs.player_id = 42;
        super::apply_mana_change(&mut gs, &mana_change_pkt(90, 202, 0));
        super::apply_simple_message(&mut gs, &simple_msg_pkt(crate::game_state::SPELL_FIZZLE));
        assert_eq!(gs.last_cast.as_ref().unwrap().spell_id, 202);
        // A later refusal the server never attached a spell to → 0 ("we don't know"), NOT 202.
        super::apply_simple_message(&mut gs, &simple_msg_pkt(236)); // SPELL_RECAST
        let last = gs.last_cast.as_ref().unwrap();
        assert_eq!(last.kind, "cast_failed");
        assert_eq!(last.spell_id, 0, "an unknown spell must read as unknown, not as the last one");
    }

    #[test]
    fn the_manachange_trailing_an_interrupt_does_not_re_arm_the_spell_hint() {
        // REGRESSION (PR #364 review). InterruptSpell sends OP_InterruptCast and THEN
        // SendSpellBarEnable → OP_ManaChange (zone/spells.cpp:1299-1314). That trailing ManaChange
        // used to RE-ARM `ended_cast_spell` *after* finish_cast had consumed it — so the next
        // unnamed failure (a refusal, which sends no OP_BeginCast) was reported against the
        // previous, unrelated spell. The earlier stale-hint test missed this because it never fed
        // the trailing ManaChange.
        let mut gs = GameState::new();
        gs.player_id = 42;
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 202, 2500));
        super::apply_interrupt_cast(&mut gs, &interrupt_pkt(42, crate::game_state::INTERRUPT_SPELL));
        assert_eq!(gs.last_cast.as_ref().unwrap().kind, "cast_interrupted");
        super::apply_mana_change(&mut gs, &mana_change_pkt(90, 202, 0)); // <-- the trailing one
        assert!(gs.ended_cast_spell.is_none(), "the trailing ManaChange must not re-arm the hint");
        assert!(gs.pending_cast_end.is_none(), "nor arm a phantom unexplained-end");

        // A later, unrelated insufficient-mana refusal: no OP_BeginCast, so nothing names the
        // spell. It must read as UNKNOWN (0), not as the interrupted spell 202.
        super::apply_simple_message(&mut gs, &simple_msg_pkt(199)); // INSUFFICIENT_MANA
        let last = gs.last_cast.as_ref().unwrap();
        assert_eq!(last.kind, "cast_failed");
        assert_eq!(last.spell_id, 0, "must not inherit the previously interrupted spell");
    }

    #[test]
    fn a_cast_the_server_ends_without_explaining_does_not_stick_forever() {
        // REGRESSION (PR #364 review) — BLOCKING. When Mob::SpellFinished returns false (the common
        // case: a beneficial buff that won't stack, zone/spells.cpp:2590 → :1744-1751),
        // CastedSpellFinished calls StopCasting(), which sends OP_ManaChange{keepcasting=0} and
        // NOTHING else — no memorize, no interrupt, no message. `casting` therefore hung forever
        // and NO outcome event was ever emitted. Publishing `casting` (this PR) would have turned
        // that into a brand-new agent-facing lie.
        let mut gs = GameState::new();
        gs.player_id = 42;
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 202, 2500));
        super::apply_mana_change(&mut gs, &mana_change_pkt(90, 202, 0));
        // The cast bar clears at once — ManaChange(keepcasting=0) IS the end of the cast.
        assert!(gs.casting.is_none(), "casting must not survive the server's cast-end signal");
        assert!(gs.pending_cast_end.is_some(), "an unexplained end is pending, not forgotten");
        // Within the grace window we still hope for an explaining packet, so no outcome yet.
        gs.resolve_pending_cast_end();
        assert!(gs.last_cast.is_none());
        // Once it lapses with no explanation, say so out loud rather than stay silent.
        gs.pending_cast_end = Some(std::time::Instant::now() - crate::game_state::CAST_END_GRACE);
        gs.resolve_pending_cast_end();
        let last = gs.last_cast.as_ref().expect("an unexplained end is still an outcome");
        assert_eq!(last.kind, "cast_ended_unexplained");
        assert_eq!(last.spell_id, 202, "OP_ManaChange named the spell that ended");
        assert_eq!(event_kinds(&gs), ["cast_begin", "cast_ended_unexplained"]);
    }

    #[test]
    fn an_unbalanced_suppression_cannot_poison_a_later_cast() {
        // REGRESSION (PR #364 re-review) — BLOCKING. `suppress_cast_end` used to be an unbounded,
        // never-reset counter whose correctness rested on a conservation law that is FALSE:
        // "every cast_failed eqstr is followed by exactly one OP_ManaChange{keepcasting=0}".
        //
        // Mob::CastSpell sets send_spellbar_enable = false for an instant item clicky or an AA
        // ((item_slot != -1 && cast_time == 0) || aa_id — zone/spells.cpp:158-161), so
        // StopCastSpell skips SendSpellBarEnable entirely and NO terminal ManaChange is ever sent.
        // SPELL_TOO_POWERFUL (197) reaches exactly that path, and we expose an item-clicky cast.
        //
        // The stale +1 then ate the terminal ManaChange of a LATER cast — so `casting` hung forever
        // with no outcome event. Permanent, session-wide, and caused by something minutes earlier.
        let mut gs = GameState::new();
        gs.player_id = 42;

        // A refusal whose trailing ManaChange NEVER ARRIVES (instant clicky / AA path).
        super::apply_simple_message(&mut gs, &simple_msg_pkt(197)); // SPELL_TOO_POWERFUL
        assert_eq!(gs.last_cast.as_ref().unwrap().kind, "cast_failed");
        // ...and the server sends nothing further. The suppression is left armed.

        // Now a completely separate cast that the server ends WITHOUT explaining (won't-stack).
        // Its terminal ManaChange must NOT be swallowed by the stale suppression.
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 202, 2500));
        super::apply_mana_change(&mut gs, &mana_change_pkt(90, 202, 0));
        assert!(gs.casting.is_none(),
            "a stale suppression must not eat this cast's terminal — that hangs `casting` forever");
        assert!(gs.pending_cast_end.is_some(), "the unexplained end must still be tracked");
        gs.pending_cast_end = Some(std::time::Instant::now() - crate::game_state::CAST_END_GRACE);
        gs.resolve_pending_cast_end();
        let last = gs.last_cast.as_ref().expect("the later cast still reports an outcome");
        assert_eq!(last.kind, "cast_ended_unexplained");
        assert_eq!(last.spell_id, 202);
    }

    #[test]
    fn zoning_clears_cast_state_so_it_cannot_leak_into_the_new_zone() {
        // A cast cannot survive a zone change: the cast bar, the spawn ids and every packet that
        // would have explained it belong to the zone we just left. Carrying `casting` over reports
        // a cast in flight that can never end; carrying `suppress_cast_end` eats the terminal of
        // the FIRST cast in the new zone. (eqoxide#348 review)
        let mut gs = GameState::new();
        gs.player_id = 42;
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 202, 2500));
        super::apply_simple_message(&mut gs, &simple_msg_pkt(197)); // arms suppression
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 202, 2500)); // cast in flight...
        assert!(gs.casting.is_some());

        gs.begin_zone_in(); // ...and we zone.
        assert!(gs.casting.is_none(), "a cast must not survive a zone change");
        assert!(gs.pending_cast_end.is_none());
        assert!(gs.ended_cast_spell.is_none());
        assert!(!gs.suppress_cast_end, "suppression must not leak into the new zone");

        // The first cast in the new zone behaves normally.
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 200, 1000));
        super::apply_mana_change(&mut gs, &mana_change_pkt(90, 200, 0));
        super::apply_memorize_spell(&mut gs, &memorize_pkt(0, 200, 3));
        assert_eq!(gs.last_cast.as_ref().unwrap().kind, "cast_completed");
    }

    #[test]
    fn every_cast_failed_string_id_has_a_real_server_sender() {
        // Dead ids are not harmless: each one is a latent UNBALANCED arm of suppress_cast_end (it
        // can never be matched by a terminal the server never sends for it). 106 and 237 exist in
        // zone/string_ids.h but nothing in zone/*.cpp sends them, so they were removed.
        use crate::game_state::CAST_FAILED_STRING_IDS as IDS;
        assert_eq!(IDS, [197, 199, 214, 236]);
        assert!(!IDS.contains(&106), "SPELL_DOES_NOT_WORK_HERE has no sender in zone/*.cpp");
        assert!(!IDS.contains(&237), "SPELL_RECOVERY has no sender in zone/*.cpp");
    }

    #[test]
    fn an_unexplained_end_is_not_reported_as_a_server_verdict() {
        // The client must never dress its own INFERENCE up as something the server said.
        //
        //   cast_failed             = "the server told us the cast failed"  → knowledge
        //   cast_ended_unexplained  = "the server told us nothing; we inferred it ended" → inference
        //
        // Collapsing them hands the agent a verdict the client does not have. Worse, phrasing the
        // inference in server voice ("Your spell did not take hold") makes our guess
        // indistinguishable from a real server string. Keep the two kinds — and the two VOICES —
        // apart. (eqoxide#348 review)
        let mut gs = GameState::new();
        gs.player_id = 42;

        // (a) The server DID give a verdict: OP_SimpleMessage 199 carries a real eqstr.
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 202, 2500));
        super::apply_simple_message(&mut gs, &simple_msg_pkt(199)); // INSUFFICIENT_MANA
        let verdict = gs.last_cast.clone().expect("server verdict recorded");
        assert_eq!(verdict.kind, "cast_failed");

        // (b) The server said NOTHING — only its cast-end signal, then silence.
        let mut gs2 = GameState::new();
        gs2.player_id = 42;
        super::apply_begin_cast(&mut gs2, &begin_cast_pkt(42, 202, 2500));
        super::apply_mana_change(&mut gs2, &mana_change_pkt(90, 202, 0));
        gs2.pending_cast_end = Some(std::time::Instant::now() - crate::game_state::CAST_END_GRACE);
        gs2.resolve_pending_cast_end();
        let inferred = gs2.last_cast.clone().expect("unexplained end recorded");

        assert_ne!(inferred.kind, verdict.kind,
            "an inference must not be reported under the same kind as a server verdict — an agent \
             has to be able to branch on 'the server said it failed' vs 'we don't know why it ended'");
        assert_eq!(inferred.kind, "cast_ended_unexplained");
        // And it must not be written in the server's voice. The real EQ fizzle/no-hold strings are
        // second-person imperatives ("Your spell ..."); our inference must announce itself as ours.
        assert!(inferred.text.contains("no outcome reported by the server"),
            "the text must say the server reported nothing, not fabricate a server line: {}",
            inferred.text);
        assert!(!inferred.text.starts_with("Your spell"),
            "must not imitate a server string: {}", inferred.text);
    }

    #[test]
    fn an_explaining_packet_inside_the_grace_window_wins_over_the_unexplained_end() {
        // The deferral must not fire for a NORMAL completion: SpellFinished sends
        // SendSpellBarEnable (ManaChange) at zone/spells.cpp:1817 and MemorizeSpell(3) at :1824 —
        // ManaChange arrives FIRST. The memorize that follows must refine the pending end into
        // "completed", not race it into a bogus "did not take hold".
        let mut gs = GameState::new();
        gs.player_id = 42;
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 202, 2500));
        super::apply_mana_change(&mut gs, &mana_change_pkt(90, 202, 0));
        super::apply_memorize_spell(&mut gs, &memorize_pkt(0, 202, 3));
        assert!(gs.pending_cast_end.is_none(), "the memorize consumed the pending end");
        gs.resolve_pending_cast_end(); // must be a no-op now
        assert_eq!(event_kinds(&gs), ["cast_begin", "cast_completed"]);
    }

    #[test]
    fn a_castbar_cooldown_reset_burst_does_not_invent_cast_outcomes() {
        // Client::ResetAllCastbarCooldowns (zone/spells.cpp:7246, callable from Lua quest scripts)
        // fires SendSpellBarEnable — i.e. OP_ManaChange{keepcasting=0} — for EVERY memorized gem
        // while the player is not casting at all. Arming the terminal on those would emit a burst
        // of phantom cast_failed events.
        let mut gs = GameState::new();
        gs.player_id = 42;
        for spell in [202u32, 203, 204] {
            super::apply_mana_change(&mut gs, &mana_change_pkt(300, spell, 0));
        }
        gs.pending_cast_end = gs.pending_cast_end.map(|_| std::time::Instant::now() - crate::game_state::CAST_END_GRACE);
        gs.resolve_pending_cast_end();
        assert!(gs.last_cast.is_none(), "no cast was in flight — inventing an outcome is a lie");
        assert!(event_kinds(&gs).is_empty());
    }

    #[test]
    fn spellbar_unlock_sentinel_is_not_a_completed_cast_but_spell_700_still_is() {
        // SPELLBAR_UNLOCK is 0x2bc = 700, which is ALSO a legal spell id. Rejecting the value
        // outright would silently swallow a real completion of spell 700 (a lie by omission);
        // accepting it outright would invent a completed cast out of a bar command.
        let mut gs = GameState::new();
        gs.player_id = 42;
        // Not casting → 700 is the bar-command sentinel: clears the bar, reports no outcome.
        super::apply_memorize_spell(&mut gs, &memorize_pkt(0, 700, 3));
        assert!(gs.last_cast.is_none() && event_kinds(&gs).is_empty());
        // Actually casting spell 700 → the same packet IS the completion of that cast.
        super::apply_begin_cast(&mut gs, &begin_cast_pkt(42, 700, 1000));
        super::apply_memorize_spell(&mut gs, &memorize_pkt(0, 700, 3));
        assert_eq!(event_kinds(&gs), ["cast_begin", "cast_completed"]);
        assert_eq!(gs.last_cast.as_ref().unwrap().spell_id, 700);
    }

    #[test]
    fn a_stale_spell_hint_expires_rather_than_mis_naming_a_later_failure() {
        // The suppression counter only cancels the ManaChange that TRAILS an outcome we reported.
        // A cooldown-reset burst emits ManaChange(keepcasting=0) with no outcome at all, leaving a
        // hint armed indefinitely. A refusal minutes later carries no spell id of its own, so an
        // un-expiring hint would name it after whatever that burst last mentioned. 0 (unknown) is
        // the only honest answer.
        let mut gs = GameState::new();
        gs.player_id = 42;
        super::apply_mana_change(&mut gs, &mana_change_pkt(300, 202, 0)); // cooldown-reset burst
        // Age the hint past its freshness window.
        gs.ended_cast_spell = gs.ended_cast_spell
            .map(|(id, _)| (id, std::time::Instant::now() - crate::game_state::CAST_HINT_FRESH));
        super::apply_simple_message(&mut gs, &simple_msg_pkt(236)); // SPELL_RECAST, names no spell
        let last = gs.last_cast.as_ref().unwrap();
        assert_eq!(last.kind, "cast_failed");
        assert_eq!(last.spell_id, 0, "a stale hint must expire, not become a plausible-looking lie");
    }

    #[test]
    fn parse_interrupt_cast_reads_spawnid_and_messageid() {
        assert_eq!(super::parse_interrupt_cast(&interrupt_pkt(42, 173)), Some((42, 173)));
        assert_eq!(super::parse_interrupt_cast(&[0u8; 4]), None); // 4 bytes: messageid is missing
    }

    #[test]
    fn parse_mana_change_reads_spell_and_keepcasting() {
        assert_eq!(super::parse_mana_change(&mana_change_pkt(120, 202, 0)), Some((120, Some(202), Some(0))));
        // A short (mana-only) packet still yields the mana update rather than being dropped.
        assert_eq!(super::parse_mana_change(&120u32.to_le_bytes()), Some((120, None, None)));
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
            let yp = enc_eq19(y);
            let xp = enc_eq19(x);
            let zp = enc_eq19(z);
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
            let yp = enc_eq19(y);
            let xp = enc_eq19(x);
            let zp = enc_eq19(z);
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

    /// Builds a minimal-but-full-length RoF2 PlayerProfile payload (>=13285 bytes) with the given
    /// coin at the real wire offsets (@13269 platinum .. @13281 copper), so `apply_player_profile`
    /// takes the coin-reconciliation path rather than the short/legacy-payload fallback.
    fn profile_payload_with_coin(coin: [u32; 4]) -> Vec<u8> {
        let mut buf = vec![0u8; 14000];
        buf[13269..13273].copy_from_slice(&coin[0].to_le_bytes());
        buf[13273..13277].copy_from_slice(&coin[1].to_le_bytes());
        buf[13277..13281].copy_from_slice(&coin[2].to_le_bytes());
        buf[13281..13285].copy_from_slice(&coin[3].to_le_bytes());
        buf
    }

    #[test]
    fn player_profile_reconciles_a_silent_coin_desync() {
        // #361: the inventory-full merchant-buy refusal silently takes the player's coin
        // server-side (EQEmu client_packet.cpp: TakeMoneyFromPP @14261-14278 runs before the
        // free-slot check @14282-14303 that can fail) with no echo at all. The next zone-in's
        // OP_PlayerProfile is the only remaining source of truth — it must correct `gs.coin` and
        // report the divergence, not silently adopt the server's figure with no trace.
        use super::apply_player_profile;
        let mut gs = GameState::new();
        gs.coin = [10, 0, 0, 0];
        gs.coin_confirmed = true; // we already had a real prior reading this session
        gs.begin_shop_buy();      // a buy was sent and never confirmed either way

        apply_player_profile(&mut gs, &profile_payload_with_coin([9, 0, 0, 0]));

        assert_eq!(gs.coin, [9, 0, 0, 0], "must correct to the server's authoritative figure");
        assert!(gs.coin_verified(), "the figure is now fresh from the source of truth");
        assert!(gs.chat_events.iter().any(|e| e.kind == "coin_desync"),
            "a real divergence must be surfaced as an event, not silently absorbed");
        assert!(gs.messages.iter().any(|m| m.text.contains("Coin desync")),
            "the desync must be visible in the message log too");
    }

    #[test]
    fn player_profile_first_login_seeds_coin_without_a_false_desync_report() {
        // A fresh session's first PlayerProfile must seed a real starting balance without ever
        // being misreported as a "desync" against the arbitrary [0,0,0,0] startup default.
        use super::apply_player_profile;
        let mut gs = GameState::new();
        assert!(!gs.coin_confirmed);

        apply_player_profile(&mut gs, &profile_payload_with_coin([12, 3, 4, 5]));

        assert_eq!(gs.coin, [12, 3, 4, 5]);
        assert!(gs.coin_verified());
        assert!(gs.coin_confirmed);
        assert!(!gs.chat_events.iter().any(|e| e.kind == "coin_desync"),
            "seeding the first real balance is not a desync");
    }

    #[test]
    fn player_profile_agreeing_coin_reports_no_desync() {
        use super::apply_player_profile;
        let mut gs = GameState::new();
        gs.coin = [9, 0, 0, 0];
        gs.coin_confirmed = true;

        apply_player_profile(&mut gs, &profile_payload_with_coin([9, 0, 0, 0]));

        assert_eq!(gs.coin, [9, 0, 0, 0]);
        assert!(!gs.chat_events.iter().any(|e| e.kind == "coin_desync"),
            "a matching balance must not be reported as a desync");
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

    /// Behavior-change guard (cleanup #4): a truncated OP_TaskDescription — a fixed-layout,
    /// single-record decoder — now PANICS via the `WireReader` instead of silently decoding a
    /// zeroed/garbage task (the old `rd_u32`/`rd_cstr` silent-0 idiom). The panic names the packet
    /// context so the crash is instantly diagnosable (agent-honesty invariant).
    #[test]
    #[should_panic(expected = "wire[OP_TaskDescription]")]
    fn apply_task_description_truncated_panics() {
        let mut gs = GameState::new();
        // A valid header needs seq(4)+task_id(4)+open(1)+type(4)+reward(4)=17 bytes before the title
        // cstr; give it only 6 so the second u32 read runs off the end.
        apply_task_description(&mut gs, &[1, 0, 0, 0, 2, 0]);
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
