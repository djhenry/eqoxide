//! EQ protocol opcodes and struct definitions for RoF2 client.
//!
//! Application opcodes (u16) are sourced from ~/git/EQEmu/utils/patches/patch_RoF2.conf.
//! Transport-layer opcodes (u8) are protocol-layer constants identical across all patches.

#![allow(dead_code)]

use std::mem;

// ── Transport-layer opcodes ────────────────────────────────────────────────

pub const OP_SESSION_REQUEST: u8 = 0x01;
pub const OP_SESSION_RESPONSE: u8 = 0x02;
pub const OP_COMBINED: u8 = 0x03;
pub const OP_SESSION_DISC: u8 = 0x05;
pub const OP_KEEPALIVE: u8 = 0x06;
pub const OP_STAT_REQUEST: u8 = 0x07;
pub const OP_STAT_RESPONSE: u8 = 0x08;
pub const OP_PACKET: u8 = 0x09;
pub const OP_FRAGMENT: u8 = 0x0d;
pub const OP_FRAGMENT_CONT: u8 = 0x0e;
pub const OP_FRAGMENT_CONT2: u8 = 0x0f;
pub const OP_FRAGMENT_CONT3: u8 = 0x10;
pub const OP_OUT_OF_ORDER: u8 = 0x11;
pub const OP_ACK: u8 = 0x15;
pub const OP_APP_COMBINED: u8 = 0x19;
pub const OP_OUT_OF_SESSION: u8 = 0x1d;

// ── Encoding flags ─────────────────────────────────────────────────────────

pub const ENCODE_NONE: u8 = 0;
pub const ENCODE_COMPRESSION: u8 = 1;
pub const ENCODE_XOR: u8 = 4;

// ── Login server opcodes ──────────────────────────────────────────────────
// These are login-server-specific opcodes not present in the world/zone opcode table.
// patch_RoF2.conf lists them all as 0x0000 (unused/unknown in zone context).
// Keeping Titanium login-server values until the login-server layer is separately migrated.

pub const OP_SESSION_READY: u16 = 0x0001; // RoF2: NO MATCH IN CONF — needs manual resolution (login-server protocol opcode)
pub const OP_LOGIN: u16 = 0x0002; // RoF2: NO MATCH IN CONF — needs manual resolution (login-server protocol opcode)
pub const OP_SERVER_LIST_REQUEST: u16 = 0x0004; // RoF2: NO MATCH IN CONF — needs manual resolution (login-server protocol opcode)
pub const OP_PLAY_EVERQUEST_REQ: u16 = 0x000d; // RoF2: NO MATCH IN CONF — needs manual resolution (login-server protocol opcode)
pub const OP_CHAT_MESSAGE: u16 = 0x0016; // RoF2: NO MATCH IN CONF — needs manual resolution (login-server protocol opcode)
pub const OP_LOGIN_ACCEPTED: u16 = 0x0017; // RoF2: NO MATCH IN CONF — needs manual resolution (login-server protocol opcode)
pub const OP_SERVER_LIST_RESPONSE: u16 = 0x0018; // RoF2: NO MATCH IN CONF — needs manual resolution (login-server protocol opcode)
pub const OP_PLAY_EVERQUEST_RESP: u16 = 0x0021; // RoF2: NO MATCH IN CONF — needs manual resolution (login-server protocol opcode)

// ── World server opcodes ──────────────────────────────────────────────────

pub const OP_SEND_LOGIN_INFO: u16 = 0x7a09;   // RoF2: OP_SendLoginInfo
pub const OP_APPROVE_WORLD: u16 = 0x7499;     // RoF2: OP_ApproveWorld
pub const OP_LOG_SERVER: u16 = 0x7ceb;        // RoF2: OP_LogServer
pub const OP_MOTD: u16 = 0x0c22;              // RoF2: OP_MOTD
pub const OP_SEND_CHAR_INFO: u16 = 0x00d2;    // RoF2: OP_SendCharInfo
pub const OP_APPROVE_NAME: u16 = 0x56a2;      // RoF2: OP_ApproveName; C->S NameApproval_Struct (72B); S->C 1 byte (1=ok,0=reject)
pub const OP_CHARACTER_CREATE: u16 = 0x6bbf;  // RoF2: OP_CharacterCreate; C->S CharCreate_Struct (wire = 80B)
pub const OP_ENTER_WORLD: u16 = 0x578f;       // RoF2: OP_EnterWorld
pub const OP_POST_ENTER_WORLD: u16 = 0x6259;  // RoF2: OP_PostEnterWorld
pub const OP_ZONE_SERVER_INFO: u16 = 0x4c44;  // RoF2: OP_ZoneServerInfo
pub const OP_WORLD_COMPLETE: u16 = 0x4493;    // RoF2: OP_WorldComplete
pub const OP_WORLD_CLIENT_READY: u16 = 0x23c1; // RoF2: OP_WorldClientReady
pub const OP_EXPANSION_INFO: u16 = 0x590d;    // RoF2: OP_ExpansionInfo
pub const OP_WORLD_CRC1: u16 = 0x0f13;        // RoF2: OP_World_Client_CRC1
pub const OP_WORLD_CRC2: u16 = 0x4b8d;        // RoF2: OP_World_Client_CRC2
pub const OP_GUILD_LIST: u16 = 0x507a;        // RoF2: OP_GuildsList

// ── Zone server opcodes ───────────────────────────────────────────────────

pub const OP_ZONE_ENTRY: u16 = 0x5089;        // RoF2: OP_ZoneEntry
pub const OP_ACK_PACKET: u16 = 0x471d;        // RoF2: OP_AckPacket
pub const OP_NEW_ZONE: u16 = 0x1795;          // RoF2: OP_NewZone
pub const OP_REQ_CLIENT_SPAWN: u16 = 0x35fa;  // RoF2: OP_ReqClientSpawn
pub const OP_ZONE_SPAWNS: u16 = 0x5237;       // RoF2: OP_ZoneSpawns
pub const OP_CHAR_INVENTORY: u16 = 0x5ca6;    // RoF2: OP_CharInventory
pub const OP_ITEM_PACKET: u16 = 0x368e;       // RoF2: OP_ItemPacket; single item (loot/trade/give/summon)
pub const OP_SET_SERVER_FILTER: u16 = 0x444d; // RoF2: OP_SetServerFilter
pub const OP_REQ_NEW_ZONE: u16 = 0x7887;      // RoF2: OP_ReqNewZone
pub const OP_PLAYER_PROFILE: u16 = 0x6506;    // RoF2: OP_PlayerProfile
pub const OP_TIME_OF_DAY: u16 = 0x5070;       // RoF2: OP_TimeOfDay
pub const OP_WEATHER: u16 = 0x661e;           // RoF2: OP_Weather
pub const OP_SEND_ZONE_POINTS: u16 = 0x69a4;  // RoF2: OP_SendZonepoints
pub const OP_SPAWN_DOOR: u16 = 0x7291;        // RoF2: OP_SpawnDoor
pub const OP_MOVE_DOOR: u16 = 0x08e8;         // RoF2: OP_MoveDoor; S->C MoveDoor_Struct {door_id u8, action u8}
pub const OP_CLICK_DOOR: u16 = 0x3a8f;        // RoF2: OP_ClickDoor; C->S ClickDoor_Struct (16 bytes)
pub const OP_SEND_EXP_ZONE_IN: u16 = 0x5f8e;  // RoF2: OP_SendExpZonein
pub const OP_CLIENT_READY: u16 = 0x345d;      // RoF2: OP_ClientReady

// ── Gameplay: spawns & positions ──────────────────────────────────────────

pub const OP_NEW_SPAWN: u16 = 0x6097;         // RoF2: OP_NewSpawn
pub const OP_DELETE_SPAWN: u16 = 0x7280;      // RoF2: OP_DeleteSpawn
pub const OP_CLIENT_UPDATE: u16 = 0x7dfc;     // RoF2: OP_ClientUpdate
pub const OP_FLOAT_LIST_THING: u16 = 0x46c6;  // RoF2: OP_FloatListThing (movement history; anti-MQGhost)
pub const OP_SPAWN_APPEARANCE: u16 = 0x0971;  // RoF2: OP_SpawnAppearance
/// Server → client: a spawn performs a one-shot animation (melee swing, kick, etc.).
/// Animation_Struct: spawnid(u16) speed(u8) action(u8). action = anim code (1=kick, 2=1HPierce,
/// 3=2HSlash, 4=2HWeapon, 5=1HWeapon, 7=tailrake/slam, 8=hand-to-hand) → combat clip C0{action}.
pub const OP_ANIMATION: u16 = 0x7177;         // RoF2: OP_Animation

// ── Gameplay: equipment ───────────────────────────────────────────────────

pub const OP_WEAR_CHANGE: u16 = 0x7994; // RoF2: OP_WearChange

// ── Gameplay: combat ──────────────────────────────────────────────────────

pub const OP_HP_UPDATE: u16 = 0x2828;         // RoF2: OP_HPUpdate (full cur/max, self+group only)
pub const OP_MOB_HEALTH: u16 = 0x37b1;        // RoF2: OP_MobHealth (percent-only, to everyone targeting the mob)
pub const OP_DEATH: u16 = 0x6517;             // RoF2: OP_Death
pub const OP_DAMAGE: u16 = 0x6f15;            // RoF2: OP_Damage
pub const OP_AUTO_ATTACK: u16 = 0x109d;       // RoF2: OP_AutoAttack
pub const OP_AUTO_ATTACK2: u16 = 0x3526;      // RoF2: OP_AutoAttack2
pub const OP_TARGET_COMMAND: u16 = 0x58e2;    // RoF2: OP_TargetCommand
pub const OP_TARGET_MOUSE: u16   = 0x075d;    // RoF2: OP_TargetMouse; sets server-side m_Target for combat
pub const OP_CONSIDER: u16 = 0x742b;          // RoF2: OP_Consider

// ── Gameplay: spellcasting ────────────────────────────────────────────────

pub const OP_CAST_SPELL: u16 = 0x1287;        // RoF2: OP_CastSpell
pub const OP_BEGIN_CAST: u16 = 0x318f;        // RoF2: OP_BeginCast
pub const OP_MANA_CHANGE: u16 = 0x5467;       // RoF2: OP_ManaChange
pub const OP_MEMORIZE_SPELL: u16 = 0x217c;    // RoF2: OP_MemorizeSpell
pub const OP_INTERRUPT_CAST: u16 = 0x048c;    // RoF2: OP_InterruptCast

// Pet control: PetCommand_Struct { command:u32, target:u32 }. Command values from
// EQEmu zone/common.h: PET_ATTACK=2, PET_GUARDHERE=5, PET_FOLLOWME=4(GetOwner), PET_BACKOFF=28.
// Environmental (fall/lava/drown) damage — CLIENT-COMPUTED in native EQ; the server only validates
// and applies it. EnvDamage2_Struct (31b): id@0, damage(u32)@6, dmgtype(u8)@22 (0xFC=falling),
// constant(u16)@27=0xFFFF. See docs/eq-technical-knowledgebase/falling-physics.md.
pub const OP_ENV_DAMAGE: u16 = 0x51fd;        // RoF2: OP_EnvDamage
pub const DMGTYPE_FALLING: u8 = 0xFC;

pub const OP_PET_COMMANDS: u16 = 0x0159;      // RoF2: OP_PetCommands
pub const PET_ATTACK: u32 = 2;
pub const PET_FOLLOWME: u32 = 4;
pub const PET_GUARDHERE: u32 = 5;
pub const PET_SIT: u32 = 6;
pub const PET_BACKOFF: u32 = 28;

// Merchant/shop: open a merchant, then buy an item from its inventory slot.
pub const OP_SHOP_REQUEST: u16 = 0x4fed;      // RoF2: OP_ShopRequest; MerchantClick_Struct (open/close)
pub const OP_SHOP_PLAYER_BUY: u16 = 0x0ddd;  // RoF2: OP_ShopPlayerBuy; Merchant_Sell_Struct (buy from slot)
pub const OP_SHOP_PLAYER_SELL: u16 = 0x791b;  // RoF2: OP_ShopPlayerSell; Merchant_Purchase_Struct (sell a player inventory slot)
pub const OP_SHOP_END: u16 = 0x30a8;          // RoF2: OP_ShopEnd; server confirms the merchant window closed

// Move/equip/unequip an item between inventory slots.
pub const OP_MOVE_ITEM: u16 = 0x32ee;         // RoF2: OP_MoveItem; MoveItem_Struct (from_slot,to_slot,number_in_stack)

// Trade window: hand an item to an NPC for a quest turn-in. Sequence is
//   C: OP_TradeRequest → S: OP_TradeRequestAck → C: OP_MoveItem(cursor→trade slot) +
//   OP_TradeAcceptClick → S: OP_FinishTrade. See navigation.rs for the state machine.
pub const OP_TRADE_REQUEST: u16      = 0x77b5; // RoF2: OP_TradeRequest; C->S, TradeRequest_Struct {to_mob_id, from_mob_id} (8b)
pub const OP_TRADE_REQUEST_ACK: u16  = 0x14bf; // RoF2: OP_TradeRequestAck; S->C, server auto-sends; same 8-byte struct
pub const OP_TRADE_ACCEPT_CLICK: u16 = 0x69e2; // RoF2: OP_TradeAcceptClick; C->S, TradeAccept_Struct {from_mob_id, unknown4} (8b)
pub const OP_FINISH_TRADE: u16       = 0x3993; // RoF2: OP_FinishTrade; S->C, 0 bytes — turn-in completed
pub const OP_CANCEL_TRADE: u16       = 0x354c; // RoF2: OP_CancelTrade; C->S, abort the trade session (cleanup)

/// Build an 8-byte `TradeRequest_Struct { to_mob_id, from_mob_id }` (also the wire form
/// of OP_TradeRequestAck). Used both to initiate a trade and to accept an incoming one.
pub fn build_trade_request(to_mob_id: u32, from_mob_id: u32) -> [u8; 8] {
    let mut buf = [0u8; 8];
    buf[0..4].copy_from_slice(&to_mob_id.to_le_bytes());
    buf[4..8].copy_from_slice(&from_mob_id.to_le_bytes());
    buf
}
// Wire slot ids: cursor = 30, the NPC's first trade slot begins at 3000.
pub const SLOT_CURSOR: u32           = 33; // RoF2 cursor slot (Titanium was 30)
pub const SLOT_TRADE_BEGIN: u32      = 3000;

// Native Task-system quest journal (server→client). Decoded into GameState.tasks for the quest log.
pub const OP_TASK_DESCRIPTION: u16 = 0x3714; // RoF2: OP_TaskDescription; a task's title/desc/reward (variable length)
pub const OP_TASK_ACTIVITY: u16    = 0x08d3; // RoF2: OP_TaskActivity; one objective + progress (done/goal, variable length)
pub const OP_COMPLETED_TASKS: u16  = 0x4eba; // RoF2: OP_CompletedTasks; full records (id + title + completed_time), not a bare id list
pub const OP_TASK_SELECT_WINDOW:   u16 = 0x705b; // RoF2: OP_TaskSelectWindow; a set of task offers (recv)
pub const OP_ACCEPT_NEW_TASK:      u16 = 0x0a23; // RoF2: OP_AcceptNewTask; AcceptNewTask_Struct (12B, send)
pub const OP_CANCEL_TASK:          u16 = 0x39f0; // RoF2: OP_CancelTask; CancelTask_Struct (8B, send)

// Group management (invite/leave/kick/roster). Opcodes cross-checked against the live EQEmu
// server's own utils/patches/patch_RoF2.conf. See docs/eq-technical-knowledgebase/group-protocol.md.
pub const OP_GROUP_INVITE: u16        = 0x6110; // C→S send / S→C deliver; GroupInvite_Struct (148B)
pub const OP_GROUP_FOLLOW: u16        = 0x1649; // C→S accept; GroupFollow_Struct (152B)
pub const OP_GROUP_FOLLOW2: u16       = 0x2060; // S→C relay of the same struct
pub const OP_GROUP_UPDATE: u16        = 0x3abb; // S→C incremental join notice; GroupJoin_Struct (148B)
pub const OP_GROUP_UPDATE_B: u16      = 0x6194; // S→C full roster snapshot; streamed/variable
// C→S leave/kick/decline-cleanup; 148B RoF2-namespaced struct, same shape as OP_GroupInvite
// (name1[64], name2[64], 5 trailing zero u32s) — confirmed live against a running EQEmu zone
// server (task-6 validation); an earlier 128-byte static-analysis inference was wrong.
pub const OP_GROUP_DISBAND: u16       = 0x4c10;
pub const OP_GROUP_DISBAND_YOU: u16   = 0x1ae5;   // S→C — you left/were kicked; 148B
pub const OP_GROUP_DISBAND_OTHER: u16 = 0x74da;   // S→C — someone else left/was removed; 148B
pub const OP_GROUP_LEADER_CHANGE: u16 = 0x21b4;   // S→C leader name push; 148B common struct
pub const OP_GROUP_ACKNOWLEDGE: u16   = 0x7323;   // S→C only — "you joined" trigger; 4B, no fields
pub const OP_GROUP_MAKE_LEADER: u16   = 0x4229;   // C→S /makeleader; GroupMakeLeader_Struct (456B)

// ── Skill training at guildmasters (eqoxide#99) ──────────────────────────────────────────────
pub const OP_GM_TRAINING: u16      = 0x1966; // C→S open request / S→C reply; GMTrainee_Struct (448B, skills[]=caps)
pub const OP_GM_TRAIN_SKILL: u16   = 0x2a85; // C→S train one skill; GMSkillChange_Struct (12B)
pub const OP_GM_END_TRAINING: u16  = 0x4d6b; // C→S close window; GMTrainEnd_Struct (8B)
pub const OP_SKILL_UPDATE: u16     = 0x004c; // S→C one skill's new value; SkillUpdate_Struct (12B)

// ── Gameplay: looting ─────────────────────────────────────────────────────

// NOTE: OP_BecomeCorpse is 0x0000 (unused) in patch_RoF2.conf — RoF2 never sends it (the old
// 0x4dbc constant was the stale Titanium value). Corpse lootability is signalled instead by
// OP_Death (apply_death), which queues the corpse for auto-loot. There is no OP_BECOME_CORPSE
// constant on purpose; see apply_death in packet_handler.rs.
/// Client → server to open a corpse for looting. Payload: corpse spawn_id (u32).
pub const OP_LOOT_REQUEST: u16     = 0x0adf; // RoF2: OP_LootRequest
/// Server → client with coin amounts on corpse. MoneyOnCorpse_Struct (20 bytes):
/// response(u8) + 3×pad + platinum(u32) + gold(u32) + silver(u32) + copper(u32).
pub const OP_MONEY_ON_CORPSE: u16  = 0x5f44; // RoF2: OP_MoneyOnCorpse
/// Server → client with the player's NEW total coin after any change (buy/sell/loot/etc).
/// MoneyUpdate_Struct (16 bytes): platinum(i32) gold(i32) silver(i32) copper(i32). Without
/// handling this, the HUD coin display stays stuck at the login-profile value.
pub const OP_MONEY_UPDATE: u16     = 0x640c; // RoF2: OP_MoneyUpdate
/// Server → client: one packet per lootable item. Client echoes back to take it.
pub const OP_LOOT_ITEM: u16        = 0x4dc9; // RoF2: OP_LootItem
/// Client → server to close a loot session.
pub const OP_END_LOOT_REQUEST: u16 = 0x30f7; // RoF2: OP_EndLootRequest

// ── Gameplay: progression ─────────────────────────────────────────────────

pub const OP_EXP_UPDATE: u16 = 0x20ed;    // RoF2: OP_ExpUpdate
pub const OP_LEVEL_UPDATE: u16 = 0x1eec;  // RoF2: OP_LevelUpdate

// ── Internal (client-only) synthetic opcodes ──────────────────────────────
// NEVER sent on the wire. The nav/gameplay threads mirror CLIENT-initiated state changes into
// the render thread's separate GameState by sending synthetic AppPackets over app_tx (the same
// channel real server packets arrive on) — see the two-GameState split notes in navigation.rs.
// 0xFFxx sits far above every RoF2 opcode this client handles, so they can't collide.

/// Local echo of the player's own outgoing chat (say/tell/ooc/shout/group), so the chat window —
/// which renders only the RENDER GameState's message log — shows what you said. Payload:
/// `kind` NUL `text` (both UTF-8); applied as `gs.log_msg(kind, text)`.
pub const OP_UI_LOCAL_ECHO: u16 = 0xFFF0;
/// Auto-loot session mirror (the gameplay loop drives looting on the NAV GameState only).
/// Payload: 1 byte — 1 = a loot session is active, 0 = idle (also clears queued corpses).
pub const OP_UI_LOOT_STATE: u16 = 0xFFF1;
/// Clear the pending group invite after the player accepts/declines it (both are client-initiated;
/// a decline produces no server packet at all). Payload: empty.
pub const OP_UI_CLEAR_INVITE: u16 = 0xFFF2;

/// Build an [`OP_UI_LOCAL_ECHO`] payload: `kind` NUL `text`.
pub fn build_ui_local_echo(kind: &str, text: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(kind.len() + 1 + text.len());
    b.extend_from_slice(kind.as_bytes());
    b.push(0);
    b.extend_from_slice(text.as_bytes());
    b
}

// ── Chat ──────────────────────────────────────────────────────────────────

pub const OP_CHANNEL_MESSAGE: u16 = 0x2b2d;   // RoF2: OP_ChannelMessage
/// Sent by the zone at zone-in with the UCS (chat server) address + mail key the client uses to
/// connect to the Universal Chat Service for cross-zone tells/OOC. Payload is a comma string:
/// `"<host>,<port>,<shortname>.<charname>,<connTypeChar><8charKey>"`. (EQEmu zone/client.cpp.)
pub const OP_SET_CHAT_SERVER: u16 = 0x1bc5;    // RoF2: OP_SetChatServer
/// NPC dialogue / emotes (quest text arrives here). SpecialMesg_Struct:
/// header[3] | msg_type(u32) | target_spawn_id(u32) | sayer(\0) | unknown[12] | message(\0)
pub const OP_SPECIAL_MESG: u16 = 0x0083;       // RoF2: OP_SpecialMesg
/// eqstr-table message with %1..%9 args. FormattedMessage_Struct:
/// unknown0(u32) | string_id(u32) | type(u32) | args (null-separated strings)
pub const OP_FORMATTED_MESSAGE: u16 = 0x1024;  // RoF2: OP_FormattedMessage
/// eqstr-table message, no args. SimpleMessage_Struct: string_id(u32) | color(u32) | unknown(u32)
pub const OP_SIMPLE_MESSAGE: u16 = 0x213f;     // RoF2: OP_SimpleMessage
/// World/NPC emote text (some quest flavor). Emote_Struct: type(u32) | message[1024]\0
pub const OP_EMOTE: u16 = 0x373b;              // RoF2: OP_Emote

/// Client → server when the player clicks an item/say link. For a "saylink" (a clickable NPC
/// dialogue choice) the server resolves the phrase from its `saylink` table by the id carried in
/// the augments and processes it as if the player said it to the NPC. See
/// [`build_item_link_click`] and EQEmu `zone/client_packet.cpp` `Handle_OP_ItemLinkClick`.
pub const OP_ITEM_LINK_CLICK: u16 = 0x4cef;    // RoF2: OP_ItemLinkClick

/// Build an `OP_ItemLinkClick` payload (`ItemViewRequest_Struct`, 52 bytes, RoF2
/// `common/patches/rof2_structs.h`) to "click" a parsed saylink:
///   item_id(u32) @0 | augments[6](u32) @4 | link_hash(u32) @28 | unknown028(u32=4) @32
///   | unknown032[12] @36 | icon(u16) @48 | unknown046[2] @50
/// The server reads `item_id` (must be `SAYLINK_ITEM_ID`) and the sayid in `augments[0]`
/// (non-silent) / `augments[1]` (silent); the remaining fields round-trip the link body.
pub fn build_item_link_click(item_id: u32, augments: &[u32; 6], link_hash: u32, icon: u32) -> Vec<u8> {
    let mut p = vec![0u8; 52];
    p[0..4].copy_from_slice(&item_id.to_le_bytes());
    for (i, a) in augments.iter().enumerate() {
        p[4 + i * 4..8 + i * 4].copy_from_slice(&a.to_le_bytes());
    }
    p[28..32].copy_from_slice(&link_hash.to_le_bytes());
    p[32..36].copy_from_slice(&4u32.to_le_bytes()); // unknown028 — always 4 on the live client
    p[48..50].copy_from_slice(&(icon as u16).to_le_bytes());
    p
}

// ── Misc zone→client ──────────────────────────────────────────────────────

pub const OP_ZONE_PLAYER_TO_BIND: u16 = 0x08d8;  // RoF2: OP_ZonePlayerToBind
/// Server → client after OP_Death: opens the "respawn from death" hover window with a list of
/// respawn options (bind, rez, …). The server holds the player as a hovering corpse until the
/// client replies with a 4-byte option index; with the hover auto-respawn rule off/long, never
/// replying leaves the player stuck as a corpse. Reply via `build_respawn_select`.
pub const OP_RESPAWN_WINDOW: u16 = 0x0ecb;       // RoF2: OP_RespawnWindow

/// Build the client's OP_RespawnWindow reply: a 4-byte little-endian option index. The server
/// populates option 0 as "Bind Location" (pushed to the front in SendRespawnBinds), so 0 = respawn
/// at bind. `respawn_window_reply` validates an inbound window before choosing this.
pub fn build_respawn_select(option: u32) -> [u8; 4] {
    option.to_le_bytes()
}

/// Given an inbound OP_RespawnWindow payload, produce the client's reply selecting the bind option
/// (index 0). Returns `None` if the payload is too short to be a valid window (header is four u32:
/// initial_selection, hover_timer_ms, unknown, num_options), so a malformed packet isn't answered.
pub fn respawn_window_reply(payload: &[u8]) -> Option<[u8; 4]> {
    if payload.len() < 16 { return None; }
    Some(build_respawn_select(0))
}
pub const OP_ZONE_CHANGE: u16 = 0x2d18;            // RoF2: OP_ZoneChange
pub const OP_REQUEST_CLIENT_ZONE_CHANGE: u16 = 0x3fcf; // RoF2: OP_RequestClientZoneChange
pub const OP_TRANSLOCATE: u16 = 0x6580;            // RoF2: OP_Translocate (translocate confirm/accept)
pub const OP_LOGOUT: u16 = 0x4ac6;                 // RoF2: OP_Logout
// NOTE: no OP_LOGOUT_REPLY constant — OP_LogoutReply=0x0000 (unused) in patch_RoF2.conf, so RoF2
// never sends a wire logout reply. Clean shutdown (perform_clean_shutdown) sends OP_Logout and
// disconnects after a brief flush window rather than waiting for a reply that never arrives.
pub const OP_GMKICK: u16 = 0x26a7;       // RoF2: OP_GMKick; zone → client, we were booted (e.g. logged in elsewhere)
pub const OP_CAMP: u16 = 0x28ec;         // RoF2: OP_Camp; client → zone, begin camp; server arms a 29s timer then
                                         // removes the char cleanly (no linkdead). Cancelled by a
                                         // Standing OP_SpawnAppearance. See docs / EQEmu Handle_OP_Camp.

// ── Struct definitions ────────────────────────────────────────────────────

/// Read a packed struct from a byte slice. Pads with zeros if data is shorter
/// than the struct size.
pub unsafe fn safe_read<T: Copy>(data: &[u8]) -> T {
    let size = mem::size_of::<T>();
    let mut buf = vec![0u8; size];
    let len = data.len().min(size);
    buf[..len].copy_from_slice(&data[..len]);
    std::ptr::read_unaligned(buf.as_ptr() as *const T)
}

// ── Heading conversion helpers ─────────────────────────────────────────────

/// Convert CW heading (0=north CW, 90=east, i.e. EQ wire convention) to CCW
/// (0=north, 90=west, the internal convention used everywhere in this client).
pub fn cw_to_ccw(cw: f32) -> f32 {
    (360.0 - cw).rem_euclid(360.0)
}

/// Convert CCW heading back to CW (for sending to the EQ server).
pub fn ccw_to_cw(ccw: f32) -> f32 {
    (360.0 - ccw).rem_euclid(360.0)
}

// ── Titanium Spawn_S bitfield position extraction (LEGACY — not used in RoF2) ─

/// Extract (x, y, z, heading) from a **Titanium** Spawn_S's bitfield blocks.
/// Titanium layout (5 words):
///   word1: deltaHeading:10, x:19, pad:3
///   word2: y:19, animation:10, pad:3
///   word3: z:19, deltaY:13
///   word4: deltaX:13, heading:12, pad:7
/// EQ stores coords as 19-bit signed integers scaled by 1/8.
/// Wire heading is EQ12 (0=north CW), converted to CCW degrees internally.
/// NOTE: This function is preserved for the unit tests; RoF2 production code
/// uses `parse_rof2_spawn` which handles position internally.
pub fn extract_spawn_position(
    bitfield_pos1: u32,
    bitfield_pos2: u32,
    bitfield_pos3: u32,
    bitfield_pos4: u32,
) -> (f32, f32, f32, f32) {
    fn s19(bits: u32) -> f32 {
        let bits = bits & 0x7FFFF;
        let val = if bits & 0x40000 != 0 {
            bits as i32 - 0x80000
        } else {
            bits as i32
        };
        val as f32 / 8.0
    }

    fn s12_to_degrees_cw(bits: u32) -> f32 {
        let bits = bits & 0xFFF;
        let val = if bits & 0x800 != 0 {
            bits as i32 - 0x1000
        } else {
            bits as i32
        };
        val as f32 * (360.0 / 512.0)
    }

    let x = s19((bitfield_pos1 >> 10) & 0x7FFFF);
    let y = s19(bitfield_pos2 & 0x7FFFF);
    let z = s19(bitfield_pos3 & 0x7FFFF);
    let heading_cw = s12_to_degrees_cw((bitfield_pos4 >> 13) & 0xFFF);
    let heading = cw_to_ccw(heading_cw);
    (x, y, z, heading)
}

// ── RoF2 spawn stream parser ───────────────────────────────────────────────

/// Parsed fields from a single RoF2 variable-length spawn entry.
/// Returned by `parse_rof2_spawn` from the wire stream; consumed by `register_spawn`.
/// Source: `~/git/EQEmu/common/patches/rof2.cpp` ENCODE(OP_ZoneSpawns) encoding order.
#[derive(Debug, Clone)]
pub struct SpawnInfo {
    pub spawn_id:        u32,
    pub name:            String,
    pub last_name:       String,
    pub level:           u8,
    pub npc:             u8,   // 0=player, 1=npc, 2=pc_corpse, 3=npc_corpse
    pub gender:          u8,
    pub race:            u32,
    pub class_:          u8,
    pub body_type:       u32,
    pub cur_hp:          u8,   // HP percent (100 = full)
    pub helm:            u8,
    pub show_helm:       bool,
    /// Face variant (0-indexed from the `face` wire byte after `size`).
    /// Rendered face has `eq_part_index == face + 1`.
    pub face:            u8,
    /// Hair style (from the `hairstyle` wire byte in the curHp..beard block).
    /// 0 = bald (all hair primitives hidden).
    pub hairstyle:       u8,
    /// Hair color index (0-23; >=24 → no tint). Only used to runtime-tint synthetic hair shells
    /// ([`crate::models::HeadPart::Hair`]); classic textured hair ignores it (eqoxide#98).
    pub haircolor:       u8,
    pub stand_state:     u8,   // 0x64 = normal standing
    pub pet_owner_id:    u32,
    pub player_state:    u32,
    pub x:               f32,
    pub y:               f32,
    pub z:               f32,
    pub heading:         f32,  // degrees, CCW (0=north, 90=west)
    pub animation:       u32,
    pub equipment:       [u32; 9],       // Texture_Struct.Material per slot (0-8)
    pub equipment_tint:  [[u8; 3]; 9],   // RGB tint per slot
}

/// Parse one RoF2 spawn record from the front of `buf`.
/// Returns `Some((info, bytes_consumed))` on success, `None` if the buffer is too
/// short to hold a complete spawn.
///
/// Wire layout (variable-length, from rof2.cpp ENCODE(OP_ZoneSpawns)):
///   name\0 | spawnId(u32) | level(u8) | bounding(f32) | NPC(u8)
///   | Bitfields(u32) | OtherData(u8) | unk3(f32) | unk4(f32)
///   | props_count(u8) [| bodytype(u32) if count>0]
///   | curHp haircolor beardcolor eyecolor1 eyecolor2 hairstyle beard (7×u8)
///   | drakkin_heritage/tattoo/details (3×u32)
///   | equip_chest2 material variation helm (4×u8)
///   | size(f32) face(u8) walkspeed(f32) runspeed(f32) race(u32)
///   | holding(u8) deity(u32) guildID(u32) guildrank(u32)
///   | class_ pvp StandState light flymode (5×u8)
///   | lastName\0 | aatitle(u32) | guild_show(u8) | TempPet(u8)
///   | petOwnerId(u32) | FindBits(u8) | PlayerState(u32)
///   | NpcTintIdx PrimaryTintIdx SecondaryTintIdx unk unk (5×u32)
///   | [TintProfile(36) + Equipment(180) for playable races, or 60 bytes for NPCs]
///   | Position(20) = 5×u32 with RoF2 bit layout
///   | [title\0 if OtherData & 0x10] | [suffix\0 if OtherData & 0x20]
///   | unknown20(8) | IsMercenary(u8) | RealEstateItemGuid(17)
///   | RealEstateID(u32) | RealEstateItemID(u32) | padding(29)
///
/// RoF2 Spawn_Struct_Position (20 bytes, rof2_structs.h):
///   word0: angle:12, y:19, pad:1
///   word1: deltaZ:13, deltaX:13, pad:6
///   word2: x:19, heading:12, pad:1
///   word3: deltaHeading:10, z:19, pad:3
///   word4: animation:10, deltaY:13, pad:9
pub fn parse_rof2_spawn(buf: &[u8]) -> Option<(SpawnInfo, usize)> {
    let mut p = 0usize;

    macro_rules! need {
        ($n:expr) => {
            if p + $n > buf.len() { return None; }
        };
    }
    macro_rules! rd_u8 {
        () => {{ need!(1); let v = buf[p]; p += 1; v }};
    }
    macro_rules! rd_u32 {
        () => {{
            need!(4);
            let v = u32::from_le_bytes([buf[p], buf[p+1], buf[p+2], buf[p+3]]);
            p += 4; v
        }};
    }
    macro_rules! rd_f32 {
        () => {{ let v = f32::from_le_bytes([buf[p], buf[p+1], buf[p+2], buf[p+3]]); p += 4; v }};
    }
    macro_rules! rd_cstr {
        () => {{
            let start = p;
            while p < buf.len() && buf[p] != 0 { p += 1; }
            let s = if buf[p..].first() == Some(&0) {
                let raw = &buf[start..p];
                if raw.iter().all(|&b| b >= 0x20 && b < 0x7f) {
                    String::from_utf8_lossy(raw).into_owned()
                } else { String::new() }
            } else { return None; };
            p += 1; // null terminator
            s
        }};
    }
    macro_rules! skip {
        ($n:expr) => {{ need!($n); p += $n; }};
    }

    // 1. name (null-terminated)
    let name = rd_cstr!();

    // 2. spawnId (u32)
    let spawn_id = rd_u32!();
    // 3. level (u8)
    let level = rd_u8!();
    // 4. bounding_radius (f32) — eye height approximation
    need!(4); rd_f32!();
    // 5. NPC (u8): 0=player,1=npc,2=pc_corpse,3=npc_corpse
    let npc = rd_u8!();

    // 6. Spawn_Struct_Bitfields (u32, 4 bytes):
    //   bits  0-1  : gender
    //   bit   2    : ispet
    //   bit   3    : afk
    //   bits  4-5  : anon
    //   bit   6    : gm
    //   bit   7    : sneak
    //   bit   8    : lfg
    //   bit   9    : betabuffed
    //   bits 10-21 : invis (12-bit)
    //   bit  22    : linkdead
    //   bit  23    : showhelm
    //   bits 24-31 : trader/targetable/etc.
    need!(4);
    let bitfields = u32::from_le_bytes([buf[p], buf[p+1], buf[p+2], buf[p+3]]);
    p += 4;
    let gender   = (bitfields & 0x3) as u8;
    let show_helm = (bitfields >> 23) & 1 != 0;

    // 7. OtherData (u8): bit4=has_title, bit5=has_suffix
    let other_data = rd_u8!();

    // 8-9. unknown3/unknown4 (2×f32)
    skip!(8);

    // 10. properties_count (u8)
    let props_count = rd_u8!();
    // 11. bodytype (u32) — only present if count > 0
    let body_type = if props_count > 0 { rd_u32!() } else { 0 };

    // 12-18. curHp haircolor beardcolor eyecolor1 eyecolor2 hairstyle beard (7×u8)
    let cur_hp = rd_u8!();
    // haircolor is consumed to runtime-tint the synthetic hair SHELLS asset-server #8 emits
    // (eqoxide#98) — those grey shells are NOT baked-color like classic humhe* hair, so the client
    // must tint them. Classic textured scalp regions remain untinted regardless (see crate::head
    // and HeadPart::HairstyleVariant vs Hair). beardcolor/eyecolor1/2 stay unused.
    let haircolor = rd_u8!();
    skip!(3); // beardcolor eyecolor1 eyecolor2
    let hairstyle = rd_u8!(); // hairstyle (0-indexed; 0=bald)
    skip!(1); // beard

    // 19-21. drakkin_heritage/tattoo/details (3×u32)
    skip!(12);

    // 22-25. equip_chest2, material(0), variation(0), helm (4×u8)
    skip!(3); // equip_chest2, material, variation
    let helm = rd_u8!();

    // 26. size (f32)
    skip!(4);
    // 27. face (u8) — 0-indexed; rendered face = eq_part_index face+1
    let face = rd_u8!();
    // 28-29. walkspeed/runspeed (2×f32)
    skip!(8);
    // 30. race (u32)
    let race = rd_u32!();

    // 31. holding (u8)
    skip!(1);
    // 32-34. deity guildID guildrank (3×u32)
    skip!(12);
    // 35. class_ (u8)
    let class_ = rd_u8!();
    // 36. pvp (u8)
    skip!(1);
    // 37. StandState (u8)
    let stand_state = rd_u8!();
    // 38-39. light flymode (2×u8)
    skip!(2);

    // 40. lastName (null-terminated)
    let last_name = rd_cstr!();

    // 41. aatitle (u32)
    skip!(4);
    // 42. guild_show (u8)
    skip!(1);
    // 43. TempPet (u8)
    skip!(1);
    // 44. petOwnerId (u32)
    let pet_owner_id = rd_u32!();
    // 45. FindBits (u8)
    skip!(1);
    // 46. PlayerState (u32)
    let player_state = rd_u32!();
    // 47-51. NpcTintIndex PrimaryTintIndex SecondaryTintIndex unk unk (5×u32)
    skip!(20);

    // Equipment section — format depends on race.
    // Playable condition (from rof2.cpp): NPC==0 || race<=12 || race in {128,130,330,522}
    // Playable → 36B TintProfile + 180B Equipment (9×Texture_Struct@20B each).
    // Non-playable → 60B: 5 u32s (zeroed) + Primary.Material(u32) + 4 u32s(0)
    //                     + Secondary.Material(u32) + 4 u32s(0).
    let is_playable = npc == 0
        || race <= 12
        || race == 128   // Iksar
        || race == 130   // VahShir
        || race == 330   // Froglok2
        || race == 522;  // Drakkin

    let mut equipment       = [0u32; 9];
    let mut equipment_tint  = [[0u8; 3]; 9];

    if is_playable {
        // TintProfile: 9 × Tint_Struct (Blue,Green,Red,UseTint = 4 bytes each = 36 bytes)
        need!(36);
        for i in 0..9usize {
            let b = p + i * 4;
            // Wire: Blue=buf[b], Green=buf[b+1], Red=buf[b+2]; store as RGB
            equipment_tint[i] = [buf[b+2], buf[b+1], buf[b]];
        }
        p += 36;

        // Equipment: 9 × Texture_Struct (Material u32 + 4×u32 padding = 20 bytes each)
        need!(180);
        for i in 0..9usize {
            let b = p + i * 20;
            equipment[i] = u32::from_le_bytes([buf[b], buf[b+1], buf[b+2], buf[b+3]]);
        }
        p += 180;
    } else {
        // Non-playable: 3 × Texture_Struct in abbreviated form (only Material fields used).
        // Layout: 5 zeros(u32) | Primary.Material(u32) | 4 zeros(u32)
        //       | Secondary.Material(u32) | 4 zeros(u32)  = 15 u32s = 60 bytes.
        need!(60);
        equipment[7] = u32::from_le_bytes([buf[p+20], buf[p+21], buf[p+22], buf[p+23]]);
        equipment[8] = u32::from_le_bytes([buf[p+40], buf[p+41], buf[p+42], buf[p+43]]);
        p += 60;
    }

    // Position: Spawn_Struct_Position (5×u32 = 20 bytes)
    // word0: angle:12, y:19, pad:1
    // word1: deltaZ:13, deltaX:13, pad:6
    // word2: x:19, heading:12, pad:1
    // word3: deltaHeading:10, z:19, pad:3
    // word4: animation:10, deltaY:13, pad:9
    need!(20);
    let w0 = u32::from_le_bytes([buf[p],   buf[p+1],  buf[p+2],  buf[p+3]]);
    let w2 = u32::from_le_bytes([buf[p+8], buf[p+9],  buf[p+10], buf[p+11]]);
    let w3 = u32::from_le_bytes([buf[p+12],buf[p+13], buf[p+14], buf[p+15]]);
    let w4 = u32::from_le_bytes([buf[p+16],buf[p+17], buf[p+18], buf[p+19]]);
    p += 20;

    // y: signed 19-bit at bits 12-30 of word0
    let y = sext((w0 >> 12) & 0x7FFFF, 19) as f32 / 8.0;
    // x: signed 19-bit at bits 0-18 of word2
    let x = sext(w2 & 0x7FFFF, 19) as f32 / 8.0;
    // heading: unsigned 12-bit at bits 19-30 of word2 (0..511 = 0..360° CW)
    let heading_cw = ((w2 >> 19) & 0xFFF) as f32 * (360.0 / 512.0);
    let heading = cw_to_ccw(heading_cw);
    // z: signed 19-bit at bits 10-28 of word3
    let z = sext((w3 >> 10) & 0x7FFFF, 19) as f32 / 8.0;
    // animation: unsigned 10-bit at bits 0-9 of word4
    let animation = w4 & 0x3FF;

    // Optional title (OtherData & 0x10 = bit4)
    if other_data & 0x10 != 0 {
        while p < buf.len() && buf[p] != 0 { p += 1; }
        if p < buf.len() { p += 1; }
    }
    // Optional suffix (OtherData & 0x20 = bit5)
    if other_data & 0x20 != 0 {
        while p < buf.len() && buf[p] != 0 { p += 1; }
        if p < buf.len() { p += 1; }
    }

    // unknown20: 2 ints (SplineID etc.)
    skip!(8);
    // IsMercenary (u8)
    skip!(1);
    // RealEstateItemGuid: "0000000000000000\0" = 17 bytes
    skip!(17);
    // RealEstateID (u32) + RealEstateItemID (u32) = 8 bytes
    skip!(8);
    // 29 zero bytes (PhysicsEffects placeholder)
    skip!(29);

    Some((SpawnInfo {
        spawn_id, name, last_name, level, npc, gender, race, class_,
        body_type, cur_hp, helm, show_helm, face, hairstyle, haircolor, stand_state,
        pet_owner_id, player_state,
        x, y, z, heading, animation,
        equipment, equipment_tint,
    }, p))
}

// ── Race ID → renderer code mapping ────────────────────────────────────────

/// True for boat/ship spawn races (EQEmu common/races.h): Ship=72, Launch=73, GhostShip=114,
/// Boat=141, DiscordShip=404, Rowboat=502, Boat2=533, MerchantShip=550, PirateShip=551,
/// GhostShip2=552. These are `GravityBehavior::Floating` server-side — they ride the water surface
/// and must be exempt from the client's floor-snap so they don't sink (the server's `Mob::FixZ`
/// skips them too, zone/waypoints.cpp). #194.
pub fn is_boat_race(race_id: u32) -> bool {
    matches!(race_id, 72 | 73 | 114 | 141 | 404 | 502 | 533 | 550 | 551 | 552)
}

pub fn eq_race_to_code(race_id: u32) -> &'static str {
    // Boats/ships render as the "boat" archetype (a real ship model), not a HUM placeholder (#194).
    if is_boat_race(race_id) {
        return "SHP";
    }
    match race_id {
        // Playable races
        1 => "HUM", 2 => "BAR", 3 => "ERU", 4 => "ELF", 5 => "HIE", 6 => "DKE",
        7 => "HEF", 8 => "DWF", 9 => "TRL", 10 => "OGR", 11 => "HFL", 12 => "GNM",
        128 => "IKS", 130 => "VAH", 330 => "FRG", 522 => "DRK",
        // NPC races 13..=127 — best-fit to an available archetype model
        // (humanoid/elf/dwarf/gnoll/skeleton/zombie/creature/bear/wolf/rat/snake/
        // frog/bat/bird/wasp/worm/fish). Names from EQEmu common/races.h.
        13 => "BRD",  // Aviak
        14 => "WOL",  // Werewolf
        15 => "HUM",  // Brownie
        16 => "HUM",  // Centaur
        17 => "HUM",  // Golem
        18 => "HUM",  // Giant
        19 => "SNA",  // Trakanon (dragon)
        20 => "SKE",  // Venril Sathir (lich)
        21 => "SPI",  // Evil Eye
        22 => "SPI",  // Beetle
        23 => "HUM",  // Kerran (cat-folk)
        24 => "FIS",  // Fish
        25 => "HUM",  // Fairy
        26 => "FRG",  // Froglok
        27 => "FRG",  // Froglok Ghoul
        28 => "HUM",  // Fungusman
        29 => "HUM",  // Gargoyle
        30 => "SPI",  // Gasbag
        31 => "SPI",  // Gelatinous Cube
        32 => "HUM",  // Ghost
        33 => "ZOM",  // Ghoul
        34 => "BAT",  // Giant Bat
        35 => "SNA",  // Giant Eel
        36 => "RAT",  // Giant Rat
        37 => "SNA",  // Giant Snake
        38 => "SPI",  // Giant Spider
        39 => "GNL",  // Gnoll
        40 => "GNL",  // Goblin
        41 => "BEA",  // Gorilla
        42 => "WOL",  // Wolf
        43 => "BEA",  // Bear
        44 => "HUM",  // Freeport Guard
        45 => "SKE",  // Demi Lich
        46 => "HUM",  // Imp
        47 => "BRD",  // Griffin
        48 => "GNL",  // Kobold
        49 => "SNA",  // Lava Dragon
        50 => "WOL",  // Lion
        51 => "HUM",  // Lizard Man
        52 => "SPI",  // Mimic
        53 => "HUM",  // Minotaur
        54 => "GNL",  // Orc
        55 => "HUM",  // Human Beggar
        56 => "HUM",  // Pixie
        57 => "SPI",  // Drachnid
        58 => "HUM",  // Solusek Ro
        59 => "HUM",  // Bloodgill
        60 => "SKE",  // Skeleton
        61 => "FIS",  // Shark
        62 => "HUM",  // Tunare
        63 => "WOL",  // Tiger
        64 => "HUM",  // Treant
        65 => "HUM",  // Vampire
        66 => "HUM",  // Statue of Rallos Zek
        67 => "HUM",  // Highpass Citizen
        68 => "SNA",  // Tentacle Terror
        69 => "SPI",  // Wisp
        70 => "ZOM",  // Zombie
        71 => "HUM",  // Qeynos Citizen
        72 => "HUM",  // Ship
        73 => "HUM",  // Launch
        74 => "FIS",  // Piranha
        75 => "HUM",  // Elemental
        76 => "WOL",  // Puma
        77 => "ELF",  // Neriak Citizen (dark elf)
        78 => "HUM",  // Erudite Citizen
        79 => "WSP",  // Bixie
        80 => "SPI",  // Reanimated Hand
        81 => "HUM",  // Rivervale Citizen
        82 => "HUM",  // Scarecrow
        83 => "RAT",  // Skunk
        84 => "SNA",  // Snake Elemental
        85 => "SKE",  // Spectre
        86 => "BEA",  // Sphinx
        87 => "RAT",  // Armadillo
        88 => "HUM",  // Clockwork Gnome
        89 => "SNA",  // Drake
        90 => "HUM",  // Halas Citizen
        91 => "SNA",  // Alligator
        92 => "HUM",  // Grobb Citizen (troll)
        93 => "HUM",  // Oggok Citizen (ogre)
        94 => "DWF",  // Kaladim Citizen (dwarf)
        95 => "HUM",  // Cazic Thule
        96 => "BRD",  // Cockatrice
        97 => "HUM",  // Daisy Man
        98 => "ELF",  // Elf Vampire
        99 => "HUM",  // Denizen
        100 => "HUM", // Dervish
        101 => "HUM", // Efreeti
        102 => "FRG", // Froglok Tadpole
        103 => "HUM", // Phinigel Autropos
        104 => "WRM", // Leech
        105 => "FIS", // Swordfish
        106 => "HUM", // Felguard
        107 => "BEA", // Mammoth
        108 => "SPI", // Eye of Zomm
        109 => "WSP", // Wasp
        110 => "HUM", // Mermaid
        111 => "BRD", // Harpy
        112 => "ELF", // Fayguard (elf)
        113 => "WSP", // Drixie
        114 => "HUM", // Ghost Ship
        115 => "FIS", // Clam
        116 => "FIS", // Sea Horse
        117 => "DWF", // Dwarf Ghost
        118 => "HUM", // Erudite Ghost
        119 => "WOL", // Sabertooth
        120 => "WOL", // Wolf Elemental
        121 => "SNA", // Gorgon
        122 => "SKE", // Dragon Skeleton
        123 => "HUM", // Innoruuk
        124 => "WOL", // Unicorn
        125 => "BRD", // Pegasus
        126 => "HUM", // Djinn
        127 => "HUM", // Invisible Man
        // Post-Titanium "new model" NPC race IDs. PEQ uses these heavily even in
        // classic-era zones — e.g. restless/decaying skeletons in qeytoqrg are race
        // 367 (Skeleton2), not 60 — so without these they all render as human males.
        // Best-fit to an available archetype model, as above; names from races.h.
        131 => "IKS", // Sarnak (lizardkin)
        137 => "GNL", // Kunark Goblin
        141 => "HUM", // Boat
        145 => "SPI", // Goo
        161 => "SKE", // Undead Iksar
        188 => "HUM", // Frost Giant
        202 => "GNL", // Grimling
        215 => "HUM", // Tegi
        217 => "SNA", // Shissar
        224 => "HUM", // Shade
        240 => "HUM", // Teleport Man
        350 => "FRG", // Undead Froglok
        359 => "ZOM", // Undead Vampire
        360 => "HUM", // Vampire (Luclin)
        361 => "GNL", // Rujarkian Orc
        364 => "ELF", // Sand Elf
        367 => "SKE", // Skeleton (new model)
        368 => "ZOM", // Mummy
        369 => "GNL", // Goblin (new model)
        372 => "HUM", // Dervish (new model)
        373 => "HUM", // Shade (new model)
        374 => "HUM", // Golem (new model)
        394 => "SNA", // Ikaav (snake-woman)
        396 => "HUM", // Kyv
        397 => "HUM", // Noc
        402 => "HUM", // Mastruq
        413 => "HUM", // Dragorn
        415 => "RAT", // Rat (new model)
        432 => "SNA", // Drake (new model)
        433 => "GNL", // Goblin2
        439 => "WOL", // Puma (new model)
        440 => "SPI", // Spider (new model)
        442 => "HUM", // Animated Statue
        456 => "HUM", // Sporali
        457 => "HUM", // Gnomework
        458 => "GNL", // Orc (new model)
        461 => "SPI", // Drachnid (new model)
        467 => "HUM", // Shiliskin
        468 => "SNA", // Snake (new model)
        // Unknown — default to humanoid
        _ => "HUM",
    }
}

// ── Struct sizes ───────────────────────────────────────────────────────────

// RoF2 spawn packets are variable-length; there is no fixed SIZE_SPAWN.
// The minimum possible spawn (empty name + empty lastName + non-playable race) is ~266 bytes;
// any payload below that will return None from parse_rof2_spawn.
pub const SIZE_NEW_ZONE: usize = 948;    // RoF2 NewZone_Struct (rof2_structs.h)
pub const SIZE_ZONE_SERVER_INFO: usize = 130; // ZoneServerInfo_S (ip[128] + port[2])
pub const SIZE_CLIENT_ZONE_ENTRY: usize = 76; // ClientZoneEntry_S (RoF2: u32 + char[64] + u32 + u32)
pub const SIZE_ENTER_WORLD: usize = 72;  // EnterWorld_S: name[64] + tutorial(4) + return_home(4)
pub const SIZE_LOGIN_INFO: usize = 464;  // LoginInfo_S
/// RoF2 PlayerPositionUpdateServer_Struct = 24 bytes (adds vehicle_id u16 vs Titanium's 22).
/// rof2_structs.h: spawn_id(u16)+vehicle_id(u16)+5×bit-packed-u32 = 2+2+20 = 24.
pub const SIZE_SPAWN_POSITION_UPDATE: usize = 24;
pub const SIZE_HP_UPDATE: usize = 10;   // HPUpdate_S
pub const SIZE_MOB_HEALTH: usize = 3;   // MobHealth_S (SpawnHPUpdate_Struct2)
pub const SIZE_DEATH: usize = 32;       // Death_S
pub const SIZE_ZONE_POINT_ENTRY: usize = 32; // RoF2 ZonePoint_Entry (was 24 — misaligned)
pub const SIZE_SPAWN_APPEARANCE: usize = 8; // SpawnAppearance_S
pub const SIZE_CONSIDER: usize = 32;     // Consider_S
pub const SIZE_EXP_UPDATE: usize = 8;   // ExpUpdate_S (exp + aaxp)
pub const SIZE_LEVEL_UPDATE: usize = 12; // LevelUpdate_S
pub const SIZE_MONEY_ON_CORPSE: usize = 20; // MoneyOnCorpse_S
pub const SIZE_ZONE_CHANGE: usize = 100;  // RoF2 ZoneChange_Struct (was 88; success@92)

/// Build a client→server OP_ZoneChange packet (RoF2 ZoneChange_Struct, 100B):
/// char_name@0, zone_id@64, instance_id@66, then y@76, x@80, z@84 (RoF2 puts the
/// coords 8 bytes later than Titanium). Used both for a normal cross-zone transition
/// and to finalize a death/bind respawn (the server holds us in a ZoneToBindPoint
/// zoning state until it receives this — eqoxide#75).
pub fn build_zone_change(name: &str, zone_id: u16, instance_id: u16, x: f32, y: f32, z: f32)
    -> [u8; SIZE_ZONE_CHANGE]
{
    let mut buf = [0u8; SIZE_ZONE_CHANGE];
    let nb = name.as_bytes();
    let n = nb.len().min(64);
    buf[..n].copy_from_slice(&nb[..n]);
    buf[64..66].copy_from_slice(&zone_id.to_le_bytes());
    buf[66..68].copy_from_slice(&instance_id.to_le_bytes());
    buf[76..80].copy_from_slice(&y.to_le_bytes());
    buf[80..84].copy_from_slice(&x.to_le_bytes());
    buf[84..88].copy_from_slice(&z.to_le_bytes());
    buf
}

/// WearChange_Struct (Titanium, 9 bytes). Runtime equip/unequip of one slot.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
#[allow(non_snake_case)]
pub struct WearChange_S {
    pub spawn_id: u16,
    pub material: u16,
    pub color: [u8; 4],   // Tint_Struct: Blue, Green, Red, UseTint
    pub wear_slot_id: u8,
}

pub const SIZE_WEAR_CHANGE: usize = std::mem::size_of::<WearChange_S>();

#[cfg(test)]
mod tests {
    use super::*;

    // ── RoF2 Phase 1: opcode table + handshake size guards ────────────────────

    #[test]
    fn rof2_handshake_opcodes_match_conf() {
        // Verify critical handshake opcodes against patch_RoF2.conf values.
        // If any of these fail, the server will not identify us as RoF2.
        assert_eq!(OP_SEND_LOGIN_INFO, 0x7a09, "OP_SendLoginInfo (world stream identifier)");
        assert_eq!(OP_ZONE_ENTRY,      0x5089, "OP_ZoneEntry (zone stream identifier)");
        assert_eq!(OP_NEW_ZONE,        0x1795, "OP_NewZone");
        assert_eq!(OP_CLIENT_UPDATE,   0x7dfc, "OP_ClientUpdate");
    }

    #[test]
    fn rof2_client_zone_entry_size() {
        // RoF2 ClientZoneEntry_Struct = uint32 + char[64] + uint32 + uint32 = 76 bytes.
        // The server signature check requires exactly 76 bytes; 68 → identify-fail.
        assert_eq!(SIZE_CLIENT_ZONE_ENTRY, 76);
        assert_eq!(std::mem::size_of::<ClientZoneEntry_S>(), 76);
    }

    #[test]
    fn rof2_new_zone_size() {
        // RoF2 NewZone_Struct = 948 bytes (rof2_structs.h).
        assert_eq!(SIZE_NEW_ZONE, 948);
        assert_eq!(std::mem::size_of::<NewZone_S>(), 948);
    }

    #[test]
    fn rof2_enter_world_size() {
        // EnterWorld_Struct = name[64] + tutorial(u32) + return_home(u32) = 72 bytes
        // (common/patches/rof2_structs.h). Sending only 68 bytes drops return_home, so the server
        // reads it from uninitialized memory and intermittently refuses entry → login loop (#140).
        assert_eq!(SIZE_ENTER_WORLD, 72);
        assert_eq!(std::mem::size_of::<EnterWorld_S>(), 72);
    }

    #[test]
    fn rof2_spawn_position_update_size() {
        // RoF2 PlayerPositionUpdateServer_Struct = 24 bytes (spawn_id u16 + vehicle_id u16 + 5×u32).
        assert_eq!(SIZE_SPAWN_POSITION_UPDATE, 24);
    }

    #[test]
    fn rof2_login_info_size() {
        // RoF2 LoginInfo_Struct: login_info[64] + unknown064[124] + zoning(u8) + unknown189[275] = 464.
        assert_eq!(SIZE_LOGIN_INFO, 464);
        assert_eq!(std::mem::size_of::<LoginInfo_S>(), 464);
    }

    #[test]
    fn rof2_zone_entry_builder_writes_name_at_offset_4() {
        // Verify on_zone_connected layout: unknown00(u32) at [0..4], name at [4..68], zeros at [68..76].
        let char_name = "Mordeth";
        let mut cze = vec![0u8; SIZE_CLIENT_ZONE_ENTRY];
        let nb = char_name.as_bytes();
        cze[4..4 + nb.len().min(64)].copy_from_slice(&nb[..nb.len().min(64)]);
        assert_eq!(cze.len(), 76);
        assert_eq!(&cze[0..4], &[0u8; 4], "unknown00 must be zero");
        assert_eq!(&cze[4..11], char_name.as_bytes(), "name at offset 4");
        assert_eq!(cze[11], 0, "NUL terminator after name");
        assert_eq!(&cze[68..76], &[0u8; 8], "unknown68+unknown72 must be zero");
    }

    #[test]
    fn build_zone_change_layout() {
        let p = build_zone_change("Katie", 54, 0, 1.0, 2.0, 3.0);
        assert_eq!(p.len(), SIZE_ZONE_CHANGE);
        assert_eq!(&p[..5], b"Katie");
        assert_eq!(u16::from_le_bytes([p[64], p[65]]), 54);   // zone_id
        assert_eq!(u16::from_le_bytes([p[66], p[67]]), 0);    // instance_id
        assert_eq!(f32::from_le_bytes([p[76], p[77], p[78], p[79]]), 2.0); // y
        assert_eq!(f32::from_le_bytes([p[80], p[81], p[82], p[83]]), 1.0); // x
        assert_eq!(f32::from_le_bytes([p[84], p[85], p[86], p[87]]), 3.0); // z
    }

    #[test]
    fn respawn_reply_selects_bind_for_valid_window() {
        // 4-u32 header (initial_sel, hover_timer, unknown, num_options) + at least one option.
        let window = vec![0u8; 16 + 26];
        // Client must answer with a 4-byte option index; 0 = "Bind Location".
        assert_eq!(respawn_window_reply(&window), Some([0, 0, 0, 0]));
        assert_eq!(build_respawn_select(0), [0, 0, 0, 0]);
        assert_eq!(build_respawn_select(2), [2, 0, 0, 0]);
    }

    #[test]
    fn build_trade_request_packs_ids_le() {
        // to_mob_id at [0..4], from_mob_id at [4..8], little-endian.
        assert_eq!(build_trade_request(0x0102, 0x0304),
                   [0x02, 0x01, 0x00, 0x00, 0x04, 0x03, 0x00, 0x00]);
        // Accepting an incoming request swaps the ids (to = initiator, from = us).
        let (initiator, me) = (260u32, 42u32);
        let ack = build_trade_request(initiator, me);
        assert_eq!(u32::from_le_bytes(ack[0..4].try_into().unwrap()), initiator);
        assert_eq!(u32::from_le_bytes(ack[4..8].try_into().unwrap()), me);
    }

    #[test]
    fn respawn_reply_ignores_malformed_window() {
        // Too short to be a valid window header — don't answer.
        assert_eq!(respawn_window_reply(&[0u8; 8]), None);
    }

    #[test]
    fn position_update_round_trips() {
        // RoF2 PlayerPositionUpdateServer_Struct: 24 bytes.
        let pkt = encode_position_update(0x1234, 125.5, -340.25, 12.0, 270.0);
        assert_eq!(pkt.len(), SIZE_SPAWN_POSITION_UPDATE, "RoF2 position update must be 24 bytes");
        let d = decode_position_update(&pkt).expect("decode");
        assert_eq!(d.spawn_id, 0x1234);
        // EQ19 fixed-point: exact to 1/8 unit.
        assert!((d.x - 125.5).abs() < 0.125, "x={}", d.x);
        assert!((d.y - (-340.25)).abs() < 0.125, "y={}", d.y);
        assert!((d.z - 12.0).abs() < 0.125, "z={}", d.z);
        // Heading (EQ-CCW degrees) round-trips within the 512-step wire quantization (~0.7°).
        assert!((d.heading - 270.0).abs() < 1.0, "heading={}", d.heading);
    }

    #[test]
    fn position_update_decodes_negative_coords() {
        // Negative coordinates must sign-extend out of the 19-bit field.
        let d = decode_position_update(&encode_position_update(1, -500.0, -1.0, -7.5, 0.0)).unwrap();
        assert!((d.x - (-500.0)).abs() < 0.125);
        assert!((d.y - (-1.0)).abs() < 0.125);
        assert!((d.z - (-7.5)).abs() < 0.125);
    }

    #[test]
    fn decode_position_update_rejects_short() {
        // RoF2 struct needs 24 bytes minimum.
        assert!(decode_position_update(&[0u8; 10]).is_none());
        assert!(decode_position_update(&[0u8; 23]).is_none());
        assert!(decode_position_update(&[0u8; 24]).is_some());
    }

    // -- Titanium legacy extract_spawn_position (kept for reference/documentation) --

    #[test]
    fn test_extract_spawn_position_zero() {
        // Titanium bitfield extract: all zeros → origin.
        let (x, y, z, heading) = extract_spawn_position(0, 0, 0, 0);
        assert_eq!(x, 0.0);
        assert_eq!(y, 0.0);
        assert_eq!(z, 0.0);
        assert_eq!(heading, 0.0);
    }

    #[test]
    fn test_extract_spawn_position_known_values() {
        // Titanium bitfield: x@word1[10-28], y@word2[0-18], z@word3[0-18], heading@word4[13-24].
        let x_raw = (100.0 * 8.0) as u32; // 800
        let y_raw = (200.0 * 8.0) as u32; // 1600
        let z_raw = (50.0 * 8.0) as u32;  // 400
        let h_raw = (180.0 * 512.0 / 360.0) as u32; // 256
        let pos1 = x_raw << 10;
        let pos2 = y_raw;
        let pos3 = z_raw;
        let pos4 = h_raw << 13;
        let (x, y, z, heading) = extract_spawn_position(pos1, pos2, pos3, pos4);
        assert!((x - 100.0).abs() < 0.125, "x={}", x);
        assert!((y - 200.0).abs() < 0.125, "y={}", y);
        assert!((z - 50.0).abs() < 0.125, "z={}", z);
        assert!((heading - 180.0).abs() < 1.0, "heading={}", heading);
    }

    // -- RoF2 parse_rof2_spawn round-trip --

    /// Build a minimal valid RoF2 spawn byte buffer for a non-playable NPC.
    fn build_npc_spawn_buf(name: &str, spawn_id: u32, race: u32,
                            x: f32, y: f32, z: f32) -> Vec<u8> {
        let mut b = Vec::new();
        // name\0
        b.extend_from_slice(name.as_bytes()); b.push(0);
        // spawnId(u32) level(u8) bounding(f32) NPC(u8)
        b.extend_from_slice(&spawn_id.to_le_bytes());
        b.push(20); // level
        b.extend_from_slice(&5.0f32.to_le_bytes()); // bounding
        b.push(1); // NPC=1
        // Bitfields(u32): gender=0, showhelm at bit23
        b.extend_from_slice(&(1u32 << 23).to_le_bytes());
        // OtherData(u8)=0, unk3(f32)=-1, unk4(f32)=0
        b.push(0);
        b.extend_from_slice(&(-1.0f32).to_le_bytes());
        b.extend_from_slice(&0.0f32.to_le_bytes());
        // props_count(u8)=1, bodytype(u32)=1
        b.push(1); b.extend_from_slice(&1u32.to_le_bytes());
        // curHp(u8)+6×u8 (hair..beard)
        b.push(80); // curHp = 80%
        b.extend_from_slice(&[0u8; 6]);
        // drakkin_heritage/tattoo/details (3×u32)
        b.extend_from_slice(&[0u8; 12]);
        // equip_chest2, material, variation, helm(u8=5)
        b.extend_from_slice(&[0, 0, 0, 5]);
        // size(f32), face(u8), walkspeed(f32), runspeed(f32), race(u32)
        b.extend_from_slice(&6.0f32.to_le_bytes());
        b.push(0);
        b.extend_from_slice(&0.35f32.to_le_bytes());
        b.extend_from_slice(&0.7f32.to_le_bytes());
        b.extend_from_slice(&race.to_le_bytes());
        // holding(u8), deity(u32), guildID(u32), guildrank(u32)
        b.push(0); b.extend_from_slice(&[0u8; 12]);
        // class_(u8)=1, pvp(u8)=0, StandState(u8)=100, light(u8)=0, flymode(u8)=0
        b.extend_from_slice(&[1, 0, 100, 0, 0]);
        // lastName\0 (empty)
        b.push(0);
        // aatitle(u32)=0, guild_show(u8)=0, TempPet(u8)=0
        b.extend_from_slice(&[0u8; 6]);
        // petOwnerId(u32)=0, FindBits(u8)=0, PlayerState(u32)=64
        b.extend_from_slice(&0u32.to_le_bytes());
        b.push(0);
        b.extend_from_slice(&64u32.to_le_bytes());
        // NpcTintIndex..unk2 (5×u32 = 20 bytes)
        b.extend_from_slice(&[0u8; 20]);
        // Non-playable equipment (60 bytes): 5 zeros + Primary.Material(99) + 4 zeros
        //   + Secondary.Material(88) + 4 zeros
        b.extend_from_slice(&[0u8; 20]);           // 5 u32s zeros
        b.extend_from_slice(&99u32.to_le_bytes()); // Primary.Material = 99
        b.extend_from_slice(&[0u8; 16]);           // 4 u32s zeros
        b.extend_from_slice(&88u32.to_le_bytes()); // Secondary.Material = 88
        b.extend_from_slice(&[0u8; 16]);           // 4 u32s zeros
        // Spawn_Struct_Position (20 bytes = 5×u32):
        //   word0: angle:12=0, y:19, pad:1  → y at bits 12-30
        //   word1: deltas = 0
        //   word2: x:19, heading:12=0, pad:1 → x at bits 0-18
        //   word3: deltaHdg:10=0, z:19, pad:3 → z at bits 10-28
        //   word4: animation:10=100, deltaY:13=0, pad:9
        let yp = ((y * 8.0) as i32 as u32) & 0x7FFFF;
        let xp = ((x * 8.0) as i32 as u32) & 0x7FFFF;
        let zp = ((z * 8.0) as i32 as u32) & 0x7FFFF;
        b.extend_from_slice(&(yp << 12).to_le_bytes()); // word0
        b.extend_from_slice(&0u32.to_le_bytes());        // word1
        b.extend_from_slice(&xp.to_le_bytes());          // word2
        b.extend_from_slice(&(zp << 10).to_le_bytes());  // word3
        b.extend_from_slice(&100u32.to_le_bytes());       // word4: animation=100
        // No title/suffix (OtherData=0)
        // unknown20(8), IsMercenary(u8)=0, RealEstateItemGuid(17)="0000000000000000\0"
        b.extend_from_slice(&[0u8; 8]);
        b.push(0);
        b.extend_from_slice(b"0000000000000000\0");
        // RealEstateID(u32), RealEstateItemID(u32)
        b.extend_from_slice(&0xffffffffu32.to_le_bytes());
        b.extend_from_slice(&0xffffffffu32.to_le_bytes());
        // 29 zero bytes
        b.extend_from_slice(&[0u8; 29]);
        b
    }

    #[test]
    fn parse_rof2_spawn_npc_round_trip() {
        use super::parse_rof2_spawn;
        let buf = build_npc_spawn_buf("Orc_Guard", 42, 54, 100.0, -200.0, 12.5);
        let (info, consumed) = parse_rof2_spawn(&buf).expect("parse must succeed");
        assert_eq!(consumed, buf.len(), "must consume exactly the full buffer");
        assert_eq!(info.spawn_id, 42);
        assert_eq!(info.name, "Orc_Guard");
        assert_eq!(info.level, 20);
        assert_eq!(info.npc, 1);
        assert_eq!(info.race, 54);
        assert_eq!(info.cur_hp, 80);
        assert_eq!(info.helm, 5);
        assert!(info.show_helm, "bit23 in bitfields → showhelm=true");
        assert_eq!(info.stand_state, 100);
        // Coordinates (EQ19 precision, 1/8 unit)
        assert!((info.x - 100.0).abs() < 0.125, "x={}", info.x);
        assert!((info.y - (-200.0)).abs() < 0.125, "y={}", info.y);
        assert!((info.z - 12.5).abs() < 0.125, "z={}", info.z);
        // Non-playable equipment: Primary@[7]=99, Secondary@[8]=88
        assert_eq!(info.equipment[7], 99, "primary weapon material");
        assert_eq!(info.equipment[8], 88, "secondary weapon material");
        assert_eq!(info.equipment[0], 0, "armor slots zero for non-playable");
        // Animation from word4 bits 0-9
        assert_eq!(info.animation, 100);
    }

    #[test]
    fn parse_rof2_spawn_rejects_truncated() {
        use super::parse_rof2_spawn;
        let buf = build_npc_spawn_buf("Orc", 1, 54, 0.0, 0.0, 0.0);
        // Every truncation of the buffer must return None.
        for trunc in 0..buf.len() - 1 {
            assert!(parse_rof2_spawn(&buf[..trunc]).is_none(),
                "should reject buffer truncated to {trunc} bytes");
        }
    }

    #[test]
    fn test_cw_to_ccw_conversions() {
        assert!((cw_to_ccw(0.0) - 0.0).abs() < 1e-5, "north same");
        assert!((cw_to_ccw(90.0) - 270.0).abs() < 1e-5, "CW east → CCW 270 (east)");
        assert!((cw_to_ccw(180.0) - 180.0).abs() < 1e-5, "south same");
        assert!((cw_to_ccw(270.0) - 90.0).abs() < 1e-5, "CW west → CCW 90 (west)");
        assert!((cw_to_ccw(360.0) - 0.0).abs() < 1e-5, "full circle wraps");
        // Round-trip
        for d in [0.0, 45.0, 90.0, 180.0, 270.0, 359.0] {
            let round = cw_to_ccw(ccw_to_cw(d));
            assert!((round - d).abs() < 1e-5, "round-trip failed at {d}: got {round}");
        }
    }

    #[test]
    fn test_eq_race_to_code_playable() {
        assert_eq!(eq_race_to_code(1), "HUM");
        assert_eq!(eq_race_to_code(4), "ELF");
        assert_eq!(eq_race_to_code(128), "IKS");
        // High Elf (5) and Half Elf (7) must be distinct — they have different models.
        assert_eq!(eq_race_to_code(5), "HIE"); // High Elf
        assert_eq!(eq_race_to_code(7), "HEF"); // Half Elf
        assert_ne!(eq_race_to_code(5), eq_race_to_code(7));
    }

    #[test]
    fn test_is_boat_race() {
        // Ferry + rowboat + variant ship races float (#194).
        for r in [72u32, 73, 114, 141, 404, 502, 533, 550, 551, 552] {
            assert!(is_boat_race(r), "race {r} should be a boat");
        }
        // Humanoids and other NPCs don't.
        for r in [1u32, 2, 5, 71, 74, 128, 501, 503, 9999] {
            assert!(!is_boat_race(r), "race {r} should NOT be a boat");
        }
    }

    #[test]
    fn test_eq_race_to_code_unknown() {
        assert_eq!(eq_race_to_code(9999), "HUM");
    }

    #[test]
    fn test_eq_race_to_code_post_titanium_npc_races() {
        // PEQ populates classic-era zones with post-127 "new model" race IDs;
        // these must map to a best-fit archetype code, not the HUM fallback.
        assert_eq!(eq_race_to_code(367), "SKE"); // Skeleton2 (restless/decaying skeletons)
        assert_eq!(eq_race_to_code(161), "SKE"); // Undead Iksar
        assert_eq!(eq_race_to_code(368), "ZOM"); // Mummy
        assert_eq!(eq_race_to_code(350), "FRG"); // Undead Froglok
        assert_eq!(eq_race_to_code(137), "GNL"); // Kunark Goblin
        assert_eq!(eq_race_to_code(433), "GNL"); // Goblin2
        assert_eq!(eq_race_to_code(458), "GNL"); // Orc2
        assert_eq!(eq_race_to_code(364), "ELF"); // Sand Elf
        assert_eq!(eq_race_to_code(415), "RAT"); // Rat (new model)
        assert_eq!(eq_race_to_code(468), "SNA"); // Snake (new model)
        assert_eq!(eq_race_to_code(440), "SPI"); // Spider (new model)
        assert_eq!(eq_race_to_code(439), "WOL"); // Puma2
    }

    #[test]
    fn test_eq_race_to_code_vah_shir_and_drakkin() {
        // Vah Shir is race 130; Drakkin is race 522 (it was previously mapped to VAH).
        assert_eq!(eq_race_to_code(130), "VAH");
        assert_eq!(eq_race_to_code(522), "DRK");
    }

    #[test]
    fn test_safe_read_pads_short_input() {
        #[repr(C, packed)]
        #[derive(Debug, Copy, Clone)]
        struct TestStruct {
            a: u32,
            b: u16,
            c: u8,
        }
        let data = vec![0x01, 0x02]; // only 2 bytes, struct is 7
        let result: TestStruct = unsafe { safe_read(&data) };
        // Read packed fields through copies to avoid unaligned reference UB
        let a = result.a;
        let b = result.b;
        let c = result.c;
        assert_eq!(a, 0x0201); // little-endian
        assert_eq!(b, 0);
        assert_eq!(c, 0);
    }
}

// ── Packed struct definitions ──────────────────────────────────────────────
// Structs below are repr(C, packed) matching EQEmu's RoF2 protocol layout.
// NOTE: The Titanium Spawn_S fixed-size struct has been removed.  RoF2 spawns
// use a variable-length wire format; use `parse_rof2_spawn` to decode them.

/// Decoded fields from the Titanium bit-packed server position update.
pub struct PositionUpdate {
    pub spawn_id: u16,
    pub x: f32,       // server x (north)
    pub y: f32,       // server y (east)
    pub z: f32,       // height
    pub heading: f32, // degrees, 0..360
    pub animation: u32, // Animation::Standing=100, Sitting=110, Crouching=111, etc.
}

#[inline]
fn sext(v: u32, bits: u32) -> i32 {
    let shift = 32 - bits;
    ((v << shift) as i32) >> shift
}

/// Decode the 24-byte bit-packed RoF2 PlayerPositionUpdateServer_Struct (OP_ClientUpdate).
/// Wire layout (rof2_structs.h):
///   spawn_id(u16) | vehicle_id(u16)
///   | word0[padding:12, y:19, pad:1]
///   | word1[deltaZ:13, deltaX:13, pad:6]
///   | word2[x:19, heading:12, pad:1]
///   | word3[deltaHeading:10, z:19, pad:3]
///   | word4[animation:10, deltaY:13, pad:9]
/// Coords are EQ19 fixed-point (value/8); heading is unsigned 12-bit CW (0..511=0..360°).
pub fn decode_position_update(p: &[u8]) -> Option<PositionUpdate> {
    if p.len() < SIZE_SPAWN_POSITION_UPDATE { return None; }
    let spawn_id = u16::from_le_bytes([p[0], p[1]]);
    // p[2..4] = vehicle_id (skip)
    let w0 = u32::from_le_bytes([p[4],  p[5],  p[6],  p[7]]);
    // w1 at p[8..12] — deltas only, not needed for position
    let w2 = u32::from_le_bytes([p[12], p[13], p[14], p[15]]);
    let w3 = u32::from_le_bytes([p[16], p[17], p[18], p[19]]);
    let w4 = u32::from_le_bytes([p[20], p[21], p[22], p[23]]);
    // y: signed 19-bit at bits 12-30 of word0
    let y = sext((w0 >> 12) & 0x7FFFF, 19) as f32 / 8.0;
    // x: signed 19-bit at bits 0-18 of word2
    let x = sext(w2 & 0x7FFFF, 19) as f32 / 8.0;
    // heading: unsigned 12-bit at bits 19-30 of word2 (0..511 = 0..360° CW)
    let heading_cw = ((w2 >> 19) & 0xFFF) as f32 * (360.0 / 512.0);
    let heading = cw_to_ccw(heading_cw);
    // z: signed 19-bit at bits 10-28 of word3
    let z = sext((w3 >> 10) & 0x7FFFF, 19) as f32 / 8.0;
    // animation: unsigned 10-bit at bits 0-9 of word4
    let animation = w4 & 0x3FF;
    Some(PositionUpdate { spawn_id, x, y, z, heading, animation })
}

/// Encode a minimal position update (deltas/animation zero) in the RoF2
/// PlayerPositionUpdateServer_Struct wire format (24 bytes), for the nav thread's
/// synthetic render-follow packet.  `heading` is EQ-CCW degrees (the same convention
/// as `gs.player_heading`); it is packed so `decode_position_update` recovers it,
/// letting the render loop face the player along the nav step direction.
/// Round-trips with `decode_position_update`.
pub fn encode_position_update(spawn_id: u16, x: f32, y: f32, z: f32, heading: f32) -> Vec<u8> {
    let xp = ((x * 8.0) as i32 as u32) & 0x7FFFF;
    let yp = ((y * 8.0) as i32 as u32) & 0x7FFFF;
    let zp = ((z * 8.0) as i32 as u32) & 0x7FFFF;
    // Heading is sent CW on the wire as a 9-bit-scale value (0..512 = 0..360°), mirroring
    // the `* 360/512` in decode_position_update.
    let hp = ((ccw_to_cw(heading) * (512.0 / 360.0)).round() as i32 as u32) & 0xFFF;
    // word0: angle(12)=0, y(19), pad(1)=0  → y at bits 12-30
    let w0 = yp << 12;
    // word1: deltas = 0
    // word2: x(19), heading(12), pad(1)=0 → x at bits 0-18, heading at bits 19-30
    let w2 = xp | (hp << 19);
    // word3: deltaHeading(10)=0, z(19), pad(3)=0 → z at bits 10-28
    let w3 = zp << 10;
    let mut buf = Vec::with_capacity(SIZE_SPAWN_POSITION_UPDATE);
    buf.extend_from_slice(&spawn_id.to_le_bytes()); // bytes 0-1: spawn_id
    buf.extend_from_slice(&0u16.to_le_bytes());      // bytes 2-3: vehicle_id = 0
    buf.extend_from_slice(&w0.to_le_bytes());         // bytes 4-7:  word0 (y)
    buf.extend_from_slice(&0u32.to_le_bytes());       // bytes 8-11: word1 (deltas=0)
    buf.extend_from_slice(&w2.to_le_bytes());         // bytes 12-15: word2 (x)
    buf.extend_from_slice(&w3.to_le_bytes());         // bytes 16-19: word3 (z)
    buf.extend_from_slice(&0u32.to_le_bytes());       // bytes 20-23: word4 (anim/deltaY=0)
    buf
}

/// HP update (10 bytes). Field order matches the RoF2 wire struct
/// `SpawnHPUpdate_Struct` (spawn_id, cur_hp, max_hp) — see eqoxide#45. The three
/// fields total 10 bytes either way, so a byte-length check never caught the
/// earlier mis-ordering; getting the order right is what makes `spawn_id` parse
/// from the correct offset.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct HPUpdate_S {
    pub spawn_id: i16,
    pub cur_hp: u32,
    pub max_hp: i32,
}

/// Percent-only HP update (3 bytes), RoF2 `SpawnHPUpdate_Struct2`
/// (`common/patches/rof2_structs.h`). Sent via OP_MobHealth to every client that
/// has the mob targeted (or on their x-target) — the compact update a client gets
/// for a mob it is merely fighting but not grouped with. `hp` is a 0-100 HP
/// percentage (`Mob::CreateHPPacket` writes `GetHPRatio()`), not an absolute value.
/// See eqoxide#51.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct MobHealth_S {
    pub spawn_id: i16,
    pub hp: u8,
}

/// Death notification (32 bytes).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct Death_S {
    pub spawn_id: u32,
    pub killer_id: u32,
    pub corpseid: u32,
    pub bindzoneid: u32,
    pub spell_id: u32,
    pub attack_skill: u32,
    pub damage: u32,
    pub unknown028: u32,
}

/// Zone info (948 bytes) — RoF2 NewZone_Struct (rof2_structs.h).
/// Only the fields needed by apply_new_zone are named; the rest are padding.
/// Field offsets verified against rof2_structs.h struct definition.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct NewZone_S {
    pub char_name:            [u8; 64],   // 0
    pub zone_short_name:      [u8; 128],  // 64   (was 32 in Titanium)
    pub zone_long_name:       [u8; 128],  // 192
    pub zone_desc:            [u8; 150],  // 320  (5×30)
    pub ztype:                u8,         // 470
    pub fog_red:              [u8; 4],    // 471
    pub fog_green:            [u8; 4],    // 475
    pub fog_blue:             [u8; 4],    // 479
    pub unknown483:           u8,         // 483
    pub fog_minclip:          [f32; 4],   // 484
    pub fog_maxclip:          [f32; 4],   // 500
    pub gravity:              f32,        // 516
    pub time_type:            u8,         // 520
    pub rain_chance:          [u8; 4],    // 521
    pub rain_duration:        [u8; 4],    // 525
    pub snow_chance:          [u8; 4],    // 529
    pub snow_duration:        [u8; 4],    // 533
    pub unknown537:           [u8; 32],   // 537
    pub zone_timezone:        u8,         // 569
    pub sky:                  u8,         // 570
    pub unknown571:           u8,         // 571
    pub water_midi:           u32,        // 572
    pub day_midi:             u32,        // 576
    pub night_midi:           u32,        // 580
    pub zone_exp_multiplier:  f32,        // 584
    pub safe_y:               f32,        // 588
    pub safe_x:               f32,        // 592
    pub safe_z:               f32,        // 596
    pub min_z:                f32,        // 600
    pub max_z:                f32,        // 604
    pub underworld:           f32,        // 608
    pub minclip:              f32,        // 612
    pub maxclip:              f32,        // 616
    pub _pad_620_852:         [u8; 232],  // 620..852 (ForageLow…many fields)
    pub zone_id:              u16,        // 852
    pub zone_instance:        u16,        // 854
    pub _pad_856_948:         [u8; 92],   // 856..948 (remaining fields)
}

const _: () = assert!(
    std::mem::size_of::<NewZone_S>() == 948,
    "NewZone_S must be 948 bytes (RoF2 NewZone_Struct)"
);

impl NewZone_S {
    pub fn zone_short_str(&self) -> String {
        let end = self.zone_short_name.iter().position(|&b| b == 0)
            .unwrap_or(self.zone_short_name.len());
        String::from_utf8_lossy(&self.zone_short_name[..end]).into_owned()
    }
}

/// Zone server address (130 bytes).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct ZoneServerInfo_S {
    pub ip: [u8; 128],
    pub port: u16,
}

/// Zone point entry (24 bytes) — zone exit info.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct ZonePointEntry_S {
    pub iterator: u32,
    pub y: f32,
    pub x: f32,
    pub z: f32,
    pub heading: f32,
    pub zoneid: u16,
    pub zoneinstance: u16,
    // RoF2 ZonePoint_Entry is 32 bytes (two trailing u32s); omitting them misaligned every entry
    // after the first by 8 bytes, scrambling zone ids/coords. See rof2_structs.h ZonePoint_Entry.
    pub unknown024: u32,
    pub unknown028: u32,
}

/// Spawn appearance change (8 bytes).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct SpawnAppearance_S {
    pub spawn_id: u16,
    pub type_: u16,
    pub parameter: u32,
}

/// Consider response (32 bytes) — faction/level/HP info.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct Consider_S {
    pub playerid: u32,
    pub targetid: u32,
    pub faction: u32,
    pub level: u32,
    pub cur_hp: i32,
    pub max_hp: i32,
    pub pvpcon: u8,
    pub unknown3: [u8; 3],
}

/// Experience update (8 bytes), RoF2 `ExpUpdate_Struct`
/// (`common/eq_packet_structs.h`). `exp` is progress through the *current* level
/// as a ratio out of 330 — the server computes it as
/// `330 * (m_pp.exp - EXPForLevel(lvl)) / (EXPForLevel(lvl+1) - EXPForLevel(lvl))`
/// (`Client::SendExpZonein` / `Client::SetEXP`). `aaxp` is AA experience, unused
/// here. Convert to a 0-100 percentage with `exp / 330 * 100`. See eqoxide#48.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct ExpUpdate_S {
    pub exp: u32,
    pub aaxp: u32,
}

/// Level update (12 bytes).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct LevelUpdate_S {
    pub level: u32,
    pub level_old: u32,
    pub exp: u32,
}

/// Money on corpse (20 bytes).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct MoneyOnCorpse_S {
    pub response: u8,
    pub unknown1: u8,
    pub unknown2: u8,
    pub unknown3: u8,
    pub platinum: u32,
    pub gold: u32,
    pub silver: u32,
    pub copper: u32,
}

/// Client zone entry (76 bytes) — sent when entering a zone.
/// RoF2 rof2_structs.h ClientZoneEntry_Struct: unknown00(u32) + char_name[64] + unknown68(u32) + unknown72(u32).
/// The server signature check requires exactly 76 bytes for RoF2 identification.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct ClientZoneEntry_S {
    pub unknown00: u32,
    pub char_name: [u8; 64],
    pub unknown68: u32,
    pub unknown72: u32,
}

const _: () = assert!(std::mem::size_of::<ClientZoneEntry_S>() == 76, "ClientZoneEntry_S must be 76 bytes (RoF2)");

/// Enter world (72 bytes) — character select. `return_home` (offset 68..72) MUST be sent: if the
/// packet is truncated to 68 bytes the server reads `return_home` from uninitialized memory and,
/// when it's non-zero, refuses entry ("trying to go home before they're able"), looping login (#140).
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct EnterWorld_S {
    pub name: [u8; 64],
    pub tutorial: u32,
    pub return_home: u32,
}

/// Login info (464 bytes) — sent to world server.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct LoginInfo_S {
    pub login_info: [u8; 64],
    pub unknown064: [u8; 124],
    pub zoning: u8,
    pub unknown189: [u8; 275],
}
