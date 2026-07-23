//! EQ protocol opcodes and struct definitions for RoF2 client.
//!
//! Application opcodes (u16) are sourced from ~/git/EQEmu/utils/patches/patch_RoF2.conf.
//! Transport-layer opcodes (u8) are protocol-layer constants identical across all patches.
//!
//! Packet BUILDERS (payload construction for outbound opcodes) are organized into domain
//! submodules below — split out of this god-module (and out of `navigation.rs`, where a
//! large batch of unrelated `args -> Vec<u8>` builders had accreted; cleanup step 1). Each
//! submodule's public items are re-exported here so existing `protocol::build_*` call sites
//! keep working unchanged.

#![allow(dead_code)]

use std::mem;
use crate::wire::WireReader;

mod combat;
mod spells;
mod inventory;
mod trade;
mod merchant;
mod group;
mod guild;
mod tasks;
mod gm;
mod chat;
mod world;

pub use combat::*;
pub use spells::*;
pub use inventory::*;
pub use trade::*;
pub use merchant::*;
pub use group::*;
pub use guild::*;
pub use tasks::*;
pub use gm::*;
pub use chat::*;
pub use world::*;

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

// ── Login server opcodes (SoD-era / RoF2 client — login_opcodes_sod.conf) ───
// These are login-server-specific opcodes not present in the world/zone opcode table
// (patch_RoF2.conf lists them all as 0x0000 in zone context). eqoxide is an EQEmu (RoF2) client,
// so it speaks the EQEmu loginserver's SoD-and-later listener (default port 5999) — NOT
// the legacy Titanium listener (5998). Ground truth: EQEmu loginserver/login_util/
// login_opcodes_sod.conf and loginserver/client_manager.cpp (CheckSoDOpcodeFile).
//
// Only the SERVER→CLIENT response opcodes differ from Titanium; the client→server request
// opcodes (SessionReady/Login/ServerListRequest/PlayEverquestRequest) are identical. The
// packet STRUCTS and the DES-CBC zero-key encryption are byte-identical across both listeners
// (the loginserver runs one shared Client/struct/crypto codebase and only swaps opcode numbers
// per port). #404.
//
// Titanium → SoD opcode shifts (for reference):
//   OP_ChatMessage          0x0016 → 0x0017
//   OP_LoginAccepted        0x0017 → 0x0018
//   OP_ServerListResponse   0x0018 → 0x0019
//   OP_PlayEverquestResponse 0x0021 → 0x0022

pub const OP_SESSION_READY: u16 = 0x0001;         // C→S; same in Titanium & SoD
pub const OP_LOGIN: u16 = 0x0002;                 // C→S; same in Titanium & SoD
pub const OP_SERVER_LIST_REQUEST: u16 = 0x0004;   // C→S; same in Titanium & SoD
pub const OP_PLAY_EVERQUEST_REQ: u16 = 0x000d;    // C→S; same in Titanium & SoD
pub const OP_CHAT_MESSAGE: u16 = 0x0017;          // S→C handshake reply; SoD (Titanium: 0x0016)
pub const OP_LOGIN_ACCEPTED: u16 = 0x0018;        // S→C login reply; SoD (Titanium: 0x0017)
pub const OP_SERVER_LIST_RESPONSE: u16 = 0x0019;  // S→C server list; SoD (Titanium: 0x0018)
pub const OP_PLAY_EVERQUEST_RESP: u16 = 0x0022;   // S→C play reply; SoD (Titanium: 0x0021)
/// S→C; SoD-only expansion offer data sent just before OP_LoginAccepted. Carries no session
/// state this headless client needs — ignored/silenced. (login_opcodes_sod.conf, #404)
pub const OP_LOGIN_EXPANSION_PACKET_DATA: u16 = 0x0031;

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
pub const OP_GUILD_LIST: u16 = 0x507a;        // RoF2: OP_GuildsList (server-wide guild directory)
// Guild membership/roster (#295). Values from patch_RoF2.conf. NOTE: OP_GuildMemberList is the
// one packet in this subsystem sent in NETWORK byte order (big-endian); the rest are little-endian.
pub const OP_GUILD_MEMBER_LIST: u16 = 0x12a6;   // full roster snapshot (BIG-ENDIAN)
pub const OP_GUILD_MEMBER_UPDATE: u16 = 0x69b9; // presence ping (zone_id 0 = offline)
pub const OP_GUILD_INVITE: u16 = 0x7099;        // invite (also promote/demote) — GuildCommand_Struct
pub const OP_GUILD_INVITE_ACCEPT: u16 = 0x7053; // accept/decline an invite
pub const OP_GUILD_REMOVE: u16 = 0x1444;        // remove a member / self-leave — GuildCommand_Struct

// ── Zone server opcodes ───────────────────────────────────────────────────

pub const OP_ZONE_ENTRY: u16 = 0x5089;        // RoF2: OP_ZoneEntry
pub const OP_ACK_PACKET: u16 = 0x471d;        // RoF2: OP_AckPacket
pub const OP_NEW_ZONE: u16 = 0x1795;          // RoF2: OP_NewZone
pub const OP_REQ_CLIENT_SPAWN: u16 = 0x35fa;  // RoF2: OP_ReqClientSpawn
pub const OP_ZONE_SPAWNS: u16 = 0x5237;       // RoF2: OP_ZoneSpawns
pub const OP_CHAR_INVENTORY: u16 = 0x5ca6;    // RoF2: OP_CharInventory
pub const OP_ITEM_PACKET: u16 = 0x368e;       // RoF2: OP_ItemPacket; single item (loot/trade/give/summon)
pub const OP_READ_BOOK: u16 = 0x72df;         // RoF2: OP_ReadBook; request AND reply share this opcode (#288)
pub const OP_FINISH_WINDOW: u16 = 0x7349;     // RoF2: OP_FinishWindow; empty, follows the ReadBook reply
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
// Run/walk toggle (#625). Client -> zone, sent once per toggle (no ack packet exists — the server's
// Handle_OP_SetRunMode just assigns `runmode` and returns, with no broadcast/reply). Verified
// against the native RoF2 client and the EQEmu server source (generic citation, no local/tool
// paths): `SetRunMode_Struct` is 4 bytes, `{mode: u8, pad: [u8; 3]}`. Distinct per-patch opcode —
// Titanium's OP_SetRunMode is a different wire value; this is the RoF2-patched one.
pub const OP_SET_RUN_MODE: u16 = 0x009f;      // RoF2: OP_SetRunMode
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
// Buff state (#586). Both carry a buff's SPELL ID for a given entity — the only channel on which a
// mid-zone buff cast on OURSELF is observable to us (the type-19 FlyMode appearance is ignore_self).
// OP_Buff: SpellBuffPacket_Struct, EXACTLY 100 bytes on the RoF2 wire (see `apply_buff`).
pub const OP_BUFF: u16 = 0x659c;              // RoF2: OP_Buff
// OP_BuffCreate: variable-length buff-icon list (see `apply_buff_create`).
pub const OP_BUFF_CREATE: u16 = 0x3377;       // RoF2: OP_BuffCreate

// Pet control: PetCommand_Struct { command:u32, target:u32 }. Command values from
// EQEmu zone/common.h: PET_ATTACK=2, PET_GUARDHERE=5, PET_FOLLOWME=4(GetOwner), PET_BACKOFF=28.
// Environmental (fall/lava/drown) damage — CLIENT-COMPUTED in native EQ; the server only validates
// and applies it. EnvDamage2_Struct (31b): id@0, damage(u32)@6, dmgtype(u8)@22 (0xFC=falling),
// constant(u16)@27=0xFFFF. See ~/git/eq_kb/falling-physics.md.
pub const OP_ENV_DAMAGE: u16 = 0x51fd;        // RoF2: OP_EnvDamage
pub const DMGTYPE_FALLING: u8 = 0xFC;

pub const OP_PET_COMMANDS: u16 = 0x0159;      // RoF2: OP_PetCommands
// PET_* command constants moved DOWN into `eqoxide-core::pet` (#544 Step 2h) so the http `pet`
// endpoint can resolve them without up-referencing `eq_net`. Re-exported here so
// `crate::eq_net::protocol::{PET_ATTACK, PET_FOLLOWME, PET_GUARDHERE, PET_SIT, PET_BACKOFF}` keep
// resolving unchanged.
pub use eqoxide_core::pet::{PET_ATTACK, PET_BACKOFF, PET_FOLLOWME, PET_GUARDHERE, PET_SIT};

// Merchant/shop: open a merchant, then buy an item from its inventory slot.
pub const OP_SHOP_REQUEST: u16 = 0x4fed;      // RoF2: OP_ShopRequest; MerchantClick_Struct (open/close)
pub const OP_SHOP_PLAYER_BUY: u16 = 0x0ddd;  // RoF2: OP_ShopPlayerBuy; Merchant_Sell_Struct (buy from slot)
pub const OP_SHOP_PLAYER_SELL: u16 = 0x791b;  // RoF2: OP_ShopPlayerSell; Merchant_Purchase_Struct (sell a player inventory slot)
// RoF2: OP_ShopEnd. Primarily client→server (Handle_OP_ShopEnd), which eqoxide never sends — its
// merchant close is OP_ShopRequest with cmd=0. Inbound it only arrives from the player-trader
// (bazaar) path: zone/trading.cpp TraderEndTrader() :926 / CancelTraderTradeWindow() :3872. It is
// NOT the NPC-merchant refusal signal — that's OP_SHOP_END_CONFIRM, below.
pub const OP_SHOP_END: u16 = 0x30a8;
// Server→client, 0-byte body. EQEmu SendMerchantEnd() (zone/client.cpp:13276-13286). For this
// client it is unambiguously a BUY REFUSAL: every call site is a buy-path early return — bad
// merchant / not-a-merchant / qty<1 / out-of-range (zone/client_packet.cpp:14151), a stale or
// removed item slot (:14194), or a negative price (:14254) — or Handle_OP_ShopEnd (:14123), which
// only fires in response to a client-sent OP_ShopEnd that eqoxide never sends. It carries no reason
// text. A buy the server drops for insufficient funds sends NEITHER this NOR an OP_ShopPlayerBuy
// echo (#345) — that one failure is genuinely silent server-side.
pub const OP_SHOP_END_CONFIRM: u16 = 0x3196;  // RoF2: OP_ShopEndConfirm

// Move/equip/unequip an item between inventory slots.
pub const OP_MOVE_ITEM: u16 = 0x32ee;         // RoF2: OP_MoveItem; MoveItem_Struct (from_slot,to_slot,number_in_stack)
// S->C: destroy an item / reduce a stack's charges at a slot. RoF2 DeleteItem_Struct shares
// MoveItem's 28-byte wire layout (from_slot InventorySlot_Struct@0, to_slot@12, number_in_stack@24).
// The server sends OP_DeleteItem to clear a slot during SwapItemResync (the "Inventory
// Desyncronization" recovery after a rejected move); without handling it the cleared slot's scratch
// token lingers in our inventory forever (#275).
pub const OP_DELETE_ITEM: u16 = 0x18ad;       // RoF2: OP_DeleteItem
pub const OP_DELETE_CHARGE: u16 = 0x01b8;     // RoF2: OP_DeleteCharge (same wire struct)

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
// server's own utils/patches/patch_RoF2.conf. See ~/git/eq_kb/group-protocol.md.
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
/// Server → client with coin amounts on corpse — the ONLY server ack for OP_LootRequest.
/// MoneyOnCorpse_Struct (20 bytes): response(u8) + 3×pad + platinum(u32) + gold(u32) +
/// silver(u32) + copper(u32). `response` is EQEmu's `LootResponse` enum (zone/common.h):
/// SomeoneElse=0, Normal=1 (accepted), NotAtThisTime=2, Normal2=3 (accepted), Hostiles=4,
/// TooFar=5, LootAll=6 (SoD+ "all items sent" marker, follows the item packets on a successful
/// loot — NOT a refusal). Only 1/3/6 mean the corpse actually opened; everything else is a
/// refusal and no items will follow (#346 — verified against EQEmu zone/corpse.cpp
/// `MakeLootRequestPackets` / `SendLootReqErrorPacket`, do not treat this as a guess).
pub const OP_MONEY_ON_CORPSE: u16  = 0x5f44; // RoF2: OP_MoneyOnCorpse
/// Server → client with the player's NEW total coin after any change (buy/sell/loot/etc).
/// MoneyUpdate_Struct (16 bytes): platinum(i32) gold(i32) silver(i32) copper(i32). Without
/// handling this, the HUD coin display stays stuck at the login-profile value.
pub const OP_MONEY_UPDATE: u16     = 0x640c; // RoF2: OP_MoneyUpdate
/// Server → client: one packet per lootable item. Client echoes back to take it.
pub const OP_LOOT_ITEM: u16        = 0x4dc9; // RoF2: OP_LootItem
/// Client → server to close a loot session. Payload: the corpse's spawn_id as a u32 (EQEmu's
/// `Handle_OP_EndLootRequest` requires `app->size == sizeof(uint32)`; an empty payload fails that
/// size check and the server silently drops the request — no response at all, ever — which is
/// the root cause behind #346's timer-only "Looting complete").
pub const OP_END_LOOT_REQUEST: u16 = 0x30f7; // RoF2: OP_EndLootRequest
/// Server → client: the AUTHORITATIVE "loot session closed" signal, sent by
/// `Corpse::EndLoot` unconditionally once a well-formed OP_EndLootRequest resolves to a real
/// corpse entity. Zero-length payload. This — not a client-side timer — is the only place
/// "Looting complete" may honestly be reported (#346).
pub const OP_LOOT_COMPLETE: u16    = 0x55c4; // RoF2: OP_LootComplete

// ── Gameplay: progression ─────────────────────────────────────────────────

pub const OP_EXP_UPDATE: u16 = 0x20ed;    // RoF2: OP_ExpUpdate
pub const OP_LEVEL_UPDATE: u16 = 0x1eec;  // RoF2: OP_LevelUpdate

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

// ── Social: /who all ──────────────────────────────────────────────────────

/// `/who` roster request (client → server). RoF2 `Who_All_Struct` is 156 bytes — WIDER than the
/// generic 76-byte struct by an inserted `unknown088[64]` pad after `whom[64]`; the RoF2 patch
/// enforces `DECODE_LENGTH_EXACT`, so it MUST be exactly 156 bytes or the server drops it
/// (`common/patches/rof2_structs.h` Who_All_Struct, `rof2.cpp` DECODE). See [`build_who_all_request`].
pub const OP_WHO_ALL_REQUEST: u16 = 0x674b;    // RoF2: OP_WhoAllRequest
/// `/who` roster response (server → client). RoF2 has its own ENCODE: 64-byte `WhoAllReturnStruct`
/// header (online count at offset 44) then N player records, each WIDENED by one always-zero u32
/// after `FormatMSGID` (`rof2.cpp` ENCODE(OP_WhoAllResponse)). Parsed in `packet_handler::apply_who_all`.
pub const OP_WHO_ALL_RESPONSE: u16 = 0x578c;   // RoF2: OP_WhoAllResponse

/// Friends presence poll (client → server). RoF2 has NO decode for this opcode — the zone reads the
/// whole packet body as a raw NUL-terminated C string of comma-joined friend names
/// (`Handle_OP_FriendsWho` → `client.cpp` fills FromID/FromName server-side). The server replies with
/// the ONLINE subset reusing the `OP_WhoAllResponse` wire format, so `apply_who_all` parses it
/// unchanged. Per-name cap 64 bytes (an over-long name drops the whole reply). (#301)
pub const OP_FRIENDS_WHO: u16 = 0x3956;        // RoF2: OP_FriendsWho

/// Build an `OP_FriendsWho` payload: the friend names comma-joined into one NUL-terminated ASCII
/// string (no header, no struct). Empty list → a single NUL. Names ≥64 bytes are dropped (the server
/// would otherwise silently discard the entire reply). (#301)
pub fn build_friends_who(names: &[String]) -> Vec<u8> {
    let joined = names.iter()
        .filter(|n| !n.trim().is_empty() && n.len() < 64)
        .map(|n| n.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let mut p = joined.into_bytes();
    p.push(0); // NUL-terminate (server does an unbounded strchr off the buffer)
    p
}

/// Build a 156-byte RoF2 `Who_All_Struct` payload for `OP_WhoAllRequest`.
///   whom[64] | unknown088[64] | wrace u32 | wclass u32 | lvllow u32 | lvlhigh u32
///   | gmlookup u32 | guildid u32 | type u32
/// For an unfiltered roster all filter fields are `0xFFFFFFFF` (= "no filter") and `whom` is empty.
/// `who_type`: 0 = zone-local `/who`, 3 = server-wide `/who all`.
pub fn build_who_all_request(who_type: u32) -> Vec<u8> {
    let mut p = vec![0u8; 156];
    // whom[0..64] and unknown088[64..128] stay zeroed (empty name, RoF2 pad).
    for off in [128, 132, 136, 140, 144, 148] {
        p[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // wrace..guildid = no filter
    }
    p[152..156].copy_from_slice(&who_type.to_le_bytes()); // type
    p
}

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

// ── EQ12 wire-heading ↔ degrees (quantization helpers) ─────────────────────
//
// EQ packs heading as a 12-bit field. Ground truth (EQEmu `common/misc_functions.cpp`
// `FloatToEQ12`/`EQ12toFloat`, confirmed against `zone/mob.cpp` and
// `zone/client_packet.cpp` — see `~/git/eq_kb/position-update-wire-format.md`
// §2-3, and issue #521): BOTH the 24-byte `PlayerPositionUpdateServer_Struct`
// (OP_ClientUpdate relay of other spawns' positions, and the identically-shaped
// RoF2 spawn-stream position block) and the 46-byte `PlayerPositionUpdateClient_Struct`
// (client→server firehose) use the SAME `EQ12toFloat(d) = d/4.0` conversion —
// i.e. wire 0..2047 == 0..360°, scale 2048/360. There is no 512-scale wire
// field; `Mob::m_Position.w`'s 0..512 domain is an EQEmu-internal-only
// representation that is converted to wire units via `FloatToEQ12` before
// ever reaching the wire, so nothing on the wire is actually 0..511==0..360°.
// A prior version of this comment claimed an intentional 512-vs-2048 split
// between the two structs; that was incorrect (#521) — the 512-scale decoder
// was simply a bug, not a second legitimate format.
//
// Rounding convention is preserved per site: decode is an exact conversion of
// an already-integer wire value (no rounding concern); the 24-byte
// server-side encode `.round()`s; the 46-byte client firehose encode
// TRUNCATES (`as` cast, no round). These rounding modes are NOT normalized to
// match one another (unrelated to the scale fix above).

/// Decode a 12-bit EQ heading field from the 24-byte
/// `PlayerPositionUpdateServer_Struct` (OP_ClientUpdate) or the RoF2 spawn
/// stream's `Spawn_Struct_Position` word2 — both use the same 0..2047 = 0..360°
/// CW scale (`EQ12toFloat`, see module comment above) — into CW degrees. Used
/// by `parse_rof2_spawn` and `decode_position_update`, i.e. this feeds both
/// spawn-appearance facing and live position-update facing for every entity
/// other than the local player. Exact conversion of an already-quantized
/// integer; no rounding is applicable on this side.
#[inline]
pub fn eq12_server_to_deg_cw(raw: u32) -> f32 {
    raw as f32 * (360.0 / 2048.0)
}

/// Encode CW degrees into the 12-bit EQ heading field for the 24-byte
/// `PlayerPositionUpdateServer_Struct` (0..2047 = 0..360° scale, `FloatToEQ12`),
/// already masked to 12 bits ready to shift into place. Used by
/// `encode_position_update` (test/loopback helper for this struct shape).
/// Rounds to nearest (`.round()`) — matches this site's historical behavior
/// exactly; do not change to truncation.
#[inline]
pub fn deg_cw_to_eq12_server(deg_cw: f32) -> u32 {
    ((deg_cw * (2048.0 / 360.0)).round() as i32 as u32) & 0xFFF
}

/// Encode CW degrees into the 12-bit EQ heading field for the 46-byte
/// `PlayerPositionUpdateClient_Struct` (client→server position firehose),
/// already masked to 12 bits ready to write into the packet. Same 0..2047 =
/// 0..360° `EQ12`/`FloatToEQ12` scale as `deg_cw_to_eq12_server` — see the
/// module-level comment above. Truncates (`as u32` cast, no round) — matches
/// this site's historical behavior exactly; do not change to rounding.
#[inline]
pub fn deg_cw_to_eq12_client(deg_cw: f32) -> u32 {
    ((deg_cw * 2048.0 / 360.0) as u32) & 0xFFF
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

    // NOTE: this is the LEGACY Titanium protocol's own 360/512 heading scale
    // (a different, older client/server wire format than RoF2 — unrelated to
    // `eq12_server_to_deg_cw`, which was fixed to 360/2048 for RoF2 in #521).
    // This closure also operates on a sign-extended `i32` (the Titanium
    // layout treats the 12-bit heading field as signed), so it cannot share
    // that helper without reinterpreting the bit pattern and changing
    // behavior even if the scale did match. Left as its own local closure —
    // flagged, not forced, per the extract-only scope of the prior refactor.
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
    /// Guild id from the spawn stream: 0xFFFFFFFF (and 0) mean "no guild". For a player's own
    /// self-spawn this is how we learn our guild identity (#295).
    pub guild_id:        u32,
    /// Guild rank (0-8 scale: 0 none, 1 leader … 8 recruit). Only meaningful with a real guild_id.
    pub guild_rank:      u32,
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
    /// (the app crate's `models::HeadPart::Hair`); classic textured hair ignores it (eqoxide#98).
    pub haircolor:       u8,
    pub stand_state:     u8,   // 0x64 = normal standing
    /// EQ `flymode` (GravityBehavior) wire code: Ground=0, Flying=1, Levitating=2, Water=3,
    /// Floating=4. Only `Flying` (1) matters to eqoxide: the server skips its Z-offset, so it must
    /// not be shifted on the wire→foot datum conversion (see `coord::skips_wire_z_offset`, #548).
    pub flymode:         u8,
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
    // Spawn packets are inherently VARIABLE-LENGTH (variable name/lastName cstrs, flag-conditional
    // title/suffix, race-dependent equipment block) and are consumed in a "parse until the buffer
    // runs out / a record won't fit → stop" loop (`apply_zone_spawns`). Truncation therefore means
    // "incomplete packet, not a spawn" → return None (an existing test enforces this for every
    // truncation). So this decoder migrates to the unified `WireReader` MECHANISM but on its
    // NON-panicking (`try_*` → None) path — it does not adopt panic-on-mismatch. Flagged for the
    // orchestrator. The macros below map 1:1 onto the old ones, keeping the body unchanged.
    let mut r = WireReader::new(buf, "OP_ZoneSpawn(Spawn_Struct)");

    macro_rules! need {
        ($n:expr) => {
            if !r.has($n) { return None; }
        };
    }
    macro_rules! rd_u8 {
        () => {{ match r.try_u8() { Some(v) => v, None => return None } }};
    }
    macro_rules! rd_u32 {
        () => {{ match r.try_u32() { Some(v) => v, None => return None } }};
    }
    macro_rules! rd_f32 {
        () => {{ match r.try_f32() { Some(v) => v, None => return None } }};
    }
    // Spawn name/lastName: NUL-terminated, but sanitised to "" when not all-printable-ASCII.
    macro_rules! rd_cstr {
        () => {{ match r.try_cstr_ascii() { Some(v) => v, None => return None } }};
    }
    macro_rules! skip {
        ($n:expr) => {{ if r.try_skip($n).is_none() { return None; } }};
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
    let bitfields = rd_u32!();
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
    // must tint them. Classic textured scalp regions remain untinted regardless (see the app
    // crate's head module and HeadPart::HairstyleVariant vs Hair). beardcolor/eyecolor1/2 stay unused.
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
    // 32. deity (u32)
    skip!(4);
    // 33-34. guildID guildrank (2×u32). NPCs send guildID=0xFFFFFFFF/rank=0; a real player carries
    // its guild here — captured so we can expose the player's own guild identity (#295).
    let guild_id = rd_u32!();
    let guild_rank = rd_u32!();
    // 35. class_ (u8)
    let class_ = rd_u8!();
    // 36. pvp (u8)
    skip!(1);
    // 37. StandState (u8)
    let stand_state = rd_u8!();
    // 38. light (u8)
    skip!(1);
    // 39. flymode (u8) — GravityBehavior wire code; Flying(1) makes the server skip its Z-offset,
    // so eqoxide must not shift such an entity on ingest (#548). Captured for register_spawn.
    let flymode = rd_u8!();

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
        for tint in equipment_tint.iter_mut() {
            let b = r.bytes(4); // guaranteed in-bounds by need!(36)
            // Wire: Blue=b[0], Green=b[1], Red=b[2]; store as RGB
            *tint = [b[2], b[1], b[0]];
        }

        // Equipment: 9 × Texture_Struct (Material u32 + 4×u32 padding = 20 bytes each)
        need!(180);
        for slot in equipment.iter_mut() {
            *slot = r.u32();  // Material (guaranteed in-bounds by need!(180))
            r.skip(16);       // 4×u32 padding
        }
    } else {
        // Non-playable: 3 × Texture_Struct in abbreviated form (only Material fields used).
        // Layout: 5 zeros(u32) | Primary.Material(u32) | 4 zeros(u32)
        //       | Secondary.Material(u32) | 4 zeros(u32)  = 15 u32s = 60 bytes.
        need!(60);
        r.skip(20);
        equipment[7] = r.u32();   // Primary.Material @ +20
        r.skip(16);
        equipment[8] = r.u32();   // Secondary.Material @ +40
        r.skip(16);
    }

    // Position: Spawn_Struct_Position (5×u32 = 20 bytes)
    // word0: angle:12, y:19, pad:1
    // word1: deltaZ:13, deltaX:13, pad:6
    // word2: x:19, heading:12, pad:1
    // word3: deltaHeading:10, z:19, pad:3
    // word4: animation:10, deltaY:13, pad:9
    need!(20);
    let w0 = r.u32();
    let _w1 = r.u32(); // deltaZ/deltaX — unused
    let w2 = r.u32();
    let w3 = r.u32();
    let w4 = r.u32();

    // y: signed 19-bit at bits 12-30 of word0
    let y = sext((w0 >> 12) & 0x7FFFF, 19) as f32 / 8.0;
    // x: signed 19-bit at bits 0-18 of word2
    let x = sext(w2 & 0x7FFFF, 19) as f32 / 8.0;
    // heading: unsigned 12-bit at bits 19-30 of word2 (0..2047 = 0..360° CW, issue #521)
    let heading_cw = eq12_server_to_deg_cw((w2 >> 19) & 0xFFF);
    let heading = cw_to_ccw(heading_cw);
    // z: signed 19-bit at bits 10-28 of word3
    let z = sext((w3 >> 10) & 0x7FFFF, 19) as f32 / 8.0;
    // animation: unsigned 10-bit at bits 0-9 of word4
    let animation = w4 & 0x3FF;

    // OPTIONAL trailing fields — present only when the flag bit is set. These MUST stay on the
    // non-panicking path: reading them as required would panic on a valid packet whose flag is
    // clear (or whose trailing string is unterminated). `try_cstr` consumes the string if a NUL is
    // present; otherwise we consume the remainder (matching the old walk-to-end-of-buffer behaviour).
    // Optional title (OtherData & 0x10 = bit4)
    if other_data & 0x10 != 0 && r.try_cstr().is_none() {
        let rem = r.remaining();
        r.skip(rem);
    }
    // Optional suffix (OtherData & 0x20 = bit5)
    if other_data & 0x20 != 0 && r.try_cstr().is_none() {
        let rem = r.remaining();
        r.skip(rem);
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
        spawn_id, name, last_name, level, npc, gender, race, class_, guild_id, guild_rank,
        body_type, cur_hp, helm, show_helm, face, hairstyle, haircolor, stand_state,
        flymode, pet_owner_id, player_state,
        x, y, z, heading, animation,
        equipment, equipment_tint,
    }, r.pos()))
}

// ── Race ID → renderer code mapping ────────────────────────────────────────

// `is_boat_race` and `eq_race_to_code` moved DOWN into `eqoxide-core::race_class` (#544 Step 2h)
// so the http layer can resolve race codes without up-referencing `eq_net`. Re-exported here so
// `crate::eq_net::protocol::{is_boat_race, eq_race_to_code}` keep resolving unchanged. The tests
// for them stay in this module's `tests` block (they exercise the re-exported symbols).
pub use eqoxide_core::race_class::{eq_race_to_code, is_boat_race};

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
        // Heading (EQ-CCW degrees) round-trips within the 2048-step wire quantization (~0.18°).
        assert!((d.heading - 270.0).abs() < 1.0, "heading={}", d.heading);
    }

    #[test]
    fn eq12_server_to_deg_cw_uses_2048_not_512_scale() {
        // Issue #521: the RoF2 wire heading field is 0..2047 == 0..360° CW
        // (EQEmu FloatToEQ12/EQ12toFloat, ~/git/eq_kb/
        // position-update-wire-format.md §2-3), not 0..511 == 0..360°. The old
        // (buggy) 360/512 scale would alias wire=1024 (true 180°) as 720°,
        // which wraps to 0° — i.e. a due-south spawn would read as due-north.
        assert_eq!(eq12_server_to_deg_cw(0), 0.0);
        assert!((eq12_server_to_deg_cw(512) - 90.0).abs() < 0.001, "512 raw must decode to 90°, not the old formula's 360°");
        assert!((eq12_server_to_deg_cw(1024) - 180.0).abs() < 0.001, "1024 raw must decode to 180°, not alias to 0°");
        assert!((eq12_server_to_deg_cw(1536) - 270.0).abs() < 0.001, "1536 raw must decode to 270°");
        // Round-trips against the (already-correct) client-firehose encoder's inverse scale.
        assert!((eq12_server_to_deg_cw(deg_cw_to_eq12_client(180.0)) - 180.0).abs() < 0.2);
    }

    #[test]
    fn parse_rof2_spawn_heading_uses_2048_scale_not_aliased() {
        // End-to-end check (issue #521) that a due-south (180° CW) spawn heading
        // survives the real spawn-stream decode path without 4x-aliasing to 0°.
        // Under the old (buggy) 360/512 decode, wire=1024 would compute
        // 1024*360/512=720 -> mod 360 = 0 (aliased to due-NORTH); the fix
        // must decode it to the true 180° (due south).
        use super::parse_rof2_spawn;
        let mut buf = build_npc_spawn_buf("Orc_Guard", 42, 54, 100.0, -200.0, 12.5, 0);
        // Spawn_Struct_Position word2 = x:19 | heading:12 << 19 | pad:1.
        // build_npc_spawn_buf lays out the 20-byte position block and then
        // appends 63 trailing bytes (unknown20[8] + IsMercenary[1] +
        // RealEstateItemGuid[17] + RealEstateID[4] + RealEstateItemID[4] +
        // 29 zeros), so the position block starts at len-63-20 and word2 is
        // its 3rd u32 (+8). Overwrite just the heading bits with wire=1024
        // (true 180° CW) without disturbing x.
        let word2_off = buf.len() - 63 - 20 + 8;
        let mut word2 = u32::from_le_bytes(buf[word2_off..word2_off + 4].try_into().unwrap());
        word2 = (word2 & !(0xFFFu32 << 19)) | (1024u32 << 19);
        buf[word2_off..word2_off + 4].copy_from_slice(&word2.to_le_bytes());
        let (info, _consumed) = parse_rof2_spawn(&buf).expect("parse must succeed");
        // SpawnInfo.heading is stored as CCW degrees; cw_to_ccw(180) == 180
        // (self-symmetric), so the expected value is still 180 either way.
        let expected_ccw = cw_to_ccw(180.0);
        assert!((info.heading - expected_ccw).abs() < 1.0,
            "wire=1024 must decode to {expected_ccw}° (true 180° CW), got {} — 4x aliasing bug if near 0°",
            info.heading);
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
                            x: f32, y: f32, z: f32, flymode: u8) -> Vec<u8> {
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
        // class_(u8)=1, pvp(u8)=0, StandState(u8)=100, light(u8)=0, flymode(u8)
        b.extend_from_slice(&[1, 0, 100, 0, flymode]);
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
        let yp = enc_eq19(y);
        let xp = enc_eq19(x);
        let zp = enc_eq19(z);
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
        let buf = build_npc_spawn_buf("Orc_Guard", 42, 54, 100.0, -200.0, 12.5, 0);
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
        assert_eq!(info.flymode, 0, "grounded NPC → flymode Ground(0)");
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
    fn parse_rof2_spawn_captures_flymode() {
        // #548: the flymode byte must be decoded (not skipped) so ingest can except Flying mobs from
        // the wire→foot Z-offset. Flying(1) here proves the byte lands at the right wire offset.
        use super::parse_rof2_spawn;
        let buf = build_npc_spawn_buf("Bat", 7, 54, 0.0, 0.0, 0.0, 1);
        let (info, _) = parse_rof2_spawn(&buf).expect("parse must succeed");
        assert_eq!(info.flymode, 1, "Flying flymode byte must be captured from the wire");
    }

    #[test]
    fn parse_rof2_spawn_rejects_truncated() {
        use super::parse_rof2_spawn;
        let buf = build_npc_spawn_buf("Orc", 1, 54, 0.0, 0.0, 0.0, 0);
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
    fn build_friends_who_is_comma_joined_nul_terminated() {
        // #301: OP_FriendsWho payload is the friend names comma-joined into one NUL-terminated ASCII
        // string — no header/struct (the server reads the whole body as a C string).
        let p = build_friends_who(&["Findissues".into(), "Fixissuesthree".into()]);
        assert_eq!(p, b"Findissues,Fixissuesthree\0");
        // Empty list → a lone NUL.
        assert_eq!(build_friends_who(&[]), b"\0");
        // Blank and over-long (>=64) names are dropped so they can't silently void the server reply.
        let long = "x".repeat(70);
        let p = build_friends_who(&["  ".into(), long, "Ok".into()]);
        assert_eq!(p, b"Ok\0");
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

/// Encode one EQ19 fixed-point coordinate (value/8, wrapped to 19 bits) for the bit-packed
/// PlayerPositionUpdateServer_Struct wire format — the encode-side counterpart of `sext`'s
/// decode (`sext(bits, 19) as f32 / 8.0`). Was duplicated at every position-encode call site;
/// pulled out to keep them in lockstep (identical expression, no behavior change).
#[inline]
pub fn enc_eq19(v: f32) -> u32 {
    ((v * 8.0) as i32 as u32) & 0x7FFFF
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
    // heading: unsigned 12-bit at bits 19-30 of word2 (0..2047 = 0..360° CW, issue #521)
    let heading_cw = eq12_server_to_deg_cw((w2 >> 19) & 0xFFF);
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
    let xp = enc_eq19(x);
    let yp = enc_eq19(y);
    let zp = enc_eq19(z);
    // Heading is sent CW on the wire as a 12-bit-scale value (0..2047 = 0..360°), mirroring
    // the `* 360/2048` in decode_position_update (issue #521).
    let hp = deg_cw_to_eq12_server(ccw_to_cw(heading));
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
    pub _pad_856_916:         [u8; 60],   // 856..916 (remaining fields before fog_density)
    /// Fog blend-intensity cap, applied on top of the linear minclip/maxclip fade (eqoxide#517).
    /// NOT a D3DFOG_EXP/EXP2 density coefficient — the RoF2 client never wires this field to
    /// D3DRS_FOGDENSITY (confirmed against the native RoF2 client's graphics code); it only
    /// ever sets D3DRS_FOGVERTEXMODE = D3DFOG_LINEAR. Typical zones ship 0.33 (rof2_structs.h @916).
    pub fog_density:          f32,        // 916
    pub _pad_920_948:         [u8; 28],   // 920..948 (remaining fields)
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
