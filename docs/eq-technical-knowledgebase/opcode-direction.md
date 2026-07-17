# Opcode direction (server->client vs client->server only), RoF2

Cross-reference for "is it safe to delete the client's inbound handler for
opcode X" questions. All numeric values below are from
`EQEmu/utils/patches/patch_RoF2.conf` (RoF2 wire opcode table) and match
eqoxide's constants. All EQEmu citations are against `/home/dhenry/git/EQEmu`.

## OP_AutoAttack (0x109d) -- STRICTLY client->server

- Only ever appears as an `IN(...)` dispatch entry
  (`common/opcode_dispatch.h:109`) and a `ConnectedOpcodes[OP_AutoAttack] =
  &Client::Handle_OP_AutoAttack` registration
  (`zone/client_packet.cpp:130`). `IN` macro = inbound-only, generates just a
  `Handle_##op` declaration (`common/opcode_dispatch.h:493-494`); there is no
  matching `OUT`/`OUTv` entry for it anywhere in the dispatch tables.
- Confirmed by exhaustive grep across `zone/`, `common/`, `world/`: **no**
  `QueuePacket`/`FastQueuePacket(OP_AutoAttack, ...)` call exists anywhere in
  the codebase. The only non-declaration hits are the `Handle_OP_AutoAttack`
  body and its size-mismatch log line.
- `Client::Handle_OP_AutoAttack` (`zone/client_packet.cpp:3517-3560`) just
  flips the server's own internal `auto_attack` bool and (dis/re)arms
  `attack_timer`/`ranged_timer`/`attack_dw_timer`. On death, root, mez, stun,
  or zoning, the server disables *its own* attack timers (elsewhere, e.g.
  death/CC handling) but never re-sends `OP_AutoAttack` to tell the client's
  UI/state machine to turn auto-attack off.
- This matches known live-EQ behavior: after a root/mez/death the client
  visually keeps "swinging" at nothing until the player manually re-toggles
  auto-attack (or picks a new target), because the server truly never emits
  this opcode outbound.
- **Verdict: strictly inbound.** Safe to assume the RoF2 client never needs to
  parse a server-sent `OP_AutoAttack`; eqoxide's inbound handler for this
  direction can be removed/is dead code if one exists.

## OP_TargetMouse (0x075d) vs OP_TargetCommand (0x58e2)

These are siblings in the dispatch table (`IN(OP_TargetMouse,
ClientTarget_Struct)` / `IN(OP_TargetCommand, ClientTarget_Struct)` at
`common/opcode_dispatch.h:113-114`) but they are **not symmetric** in RoF2:

- **OP_TargetMouse: strictly client->server.** Only a `ConnectedOpcodes`
  registration (`zone/client_packet.cpp:382`) and
  `Client::Handle_OP_TargetMouse` (`zone/client_packet.cpp:15125`) exist. No
  send site anywhere. This is the "player clicked a mob with the mouse"
  packet.
- **OP_TargetCommand: the server DOES send it, server->client, to force the
  client's target.** `Client::SendTargetCommand(uint32 EntityID)`
  (`zone/client.cpp:7004-7010`) builds `new
  EQApplicationPacket(OP_TargetCommand, sizeof(ClientTarget_Struct))` with
  `cts->new_target = EntityID` and `FastQueuePacket`s it -- this is the exact
  same opcode the client also uses inbound (`Handle_OP_TargetCommand`,
  `zone/client_packet.cpp:15085`), i.e. RoF2's `OP_TargetCommand` is
  bidirectional. Confirmed call sites forcing a retarget:
  - `zone/client.cpp:7027` -- `Client::LocateCorpse()` (find-corpse), sets
    target to the closest corpse and pushes `OP_TargetCommand` to sync the
    client's UI/target ring.
  - `zone/spell_effects.cpp:896` -- Sense Undead / Sense Summoned / Sense
    Animal spell effects (`SpellEffect::SenseSummoned` /
    `SenseAnimals` branch), SoD-and-later client mask only
    (`ClientVersionBit() & EQ::versions::maskSoDAndLater`, includes RoF2):
    finds the closest matching-body-type mob, calls `SetTarget()` then
    `CastToClient()->SendTargetCommand(ClosestMob->GetID())`.
  - `zone/client_packet.cpp:10672` -- `Handle_OP_MercenaryHire`:
    `SetHoTT(0); SendTargetCommand(0);` clears the client's target/heroes-of-target
    when hiring a merc.
  - Exposed to quest Perl scripts as `Client::SendTargetCommand` via
    `Perl_Client_SendTargetCommand` (`zone/perl_client.cpp:1580-1582`,
    registered `zone/perl_client.cpp:3823`), so custom quests can also force
    a client retarget server-side (e.g. GM/quest-scripted "look here" or
    force-target effects) using this same opcode.
- No `#target` GM slash-command specifically found wired to
  `SendTargetCommand` (the only `"target"` string hit in `zone/gm_commands/`
  is `evolving_items.cpp:36`, unrelated), but the mechanism it would use if
  one existed is this same `SendTargetCommand`/`OP_TargetCommand` path --
  there is no other server->client target-forcing opcode in the codebase.
- **Verdict:** `OP_TargetMouse` handler is safe to drop for inbound-only
  removal purposes -- server never sends it. `OP_TargetCommand` is NOT
  safe to drop an inbound-parse path for: the server actively sends it to
  force-set the client's target in at least 3 flows (corpse-locate, sense
  spells, merc-hire target clear) plus arbitrary quest scripts. eqoxide's
  client must keep parsing server-sent `OP_TargetCommand` and applying
  `new_target` (0 = clear target) to its target state.

## OP_GMEndTraining (0x4d6b, GMTrainEnd_Struct, 8 bytes) -- STRICTLY client->server

- `ConnectedOpcodes[OP_GMEndTraining] = &Client::Handle_OP_GMEndTraining`
  (`zone/client_packet.cpp:212`); handler at `zone/client_packet.cpp:6630-6639`
  validates size then forwards to `Client::OPGMEndTraining(app)`
  (`zone/client_process.cpp:1672-1699`).
- `OPGMEndTraining` reads `GMTrainEnd_Struct::npcid` from the inbound payload,
  replies with a **different** opcode -- `new
  EQApplicationPacket(OP_GMEndTrainingResponse, 0)` (empty payload,
  `zone/client_process.cpp:1674`, `FastQueuePacket`d at line 1677) -- then
  validates the trainer NPC (class range, cross-class rule, range check) and
  has the trainer NPC say a random goodbye line (`SayString`, no packet to
  the training client itself beyond the ack).
- Exhaustive grep of `zone/` + `common/` for `OP_GMEndTraining` (excluding
  struct definitions in the per-client-version `*_structs.h` headers, the
  opcode table, lua bindings, and dispatch macros) turns up only the
  `Handle_OP_GMEndTraining` registration/definition and its size-mismatch log
  line -- no send site.
- **Verdict: strictly inbound.** The server's only outbound reaction to a
  client closing the trainer window is the distinct, empty-payload
  `OP_GMEndTrainingResponse` (ack) opcode, not a re-send of
  `OP_GMEndTraining` itself. Safe to remove/treat as dead any RoF2 client
  code that expects the server to push `OP_GMEndTraining` to force-close a
  training session; the server has no such mechanism -- training sessions are
  purely client-driven except for the ack.

## Cross-reference

- `docs/eq-technical-knowledgebase/opcodes.md` if/when created -- general
  RoF2 opcode table notes belong there; this file is specifically about
  direction (server->client vs client->server) for opcodes where that's
  ambiguous or asymmetric between siblings.
