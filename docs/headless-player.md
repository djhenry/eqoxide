# CPU player candidate

`eqoxide-headless` connects through the ordinary RoF2 login, world, character creation and zone
protocol. It runs the same physical controller as the graphical application on a 20ms CPU clock.
It loads native EQEmu `.map` v2 and `.wtr` v1/v2 files; it needs no display, GPU, textures, display
models, asset service or game string/spell files. Upstream source baseline is
`4bf520f2151976177a757428628995b33a3a3ee9`; the preparation appendix and check script are retained.

This is a candidate for staging qualification. Build and synthetic tests do not prove live play.
The authoritative selection and live results belong to the Operations project; this file documents
the executable and wire format.

## Build and launch

Run `scripts/build-headless` from this checkout. Its pinned Rust container uses the existing
locked dependency cache; `--online` permits missing dependencies to download. The output prints
`HEADLESS_BINARY=...`. Native Rust builds can use `cargo build -p eqoxide-headless --locked` after
disabling this repository's optional sccache/mold configuration if those tools are unavailable.
`scripts/check-headless-crates` runs the CPU tests without connecting a player.

Launch `eqoxide-headless --profile /absolute/private-profile.json`. The profile must be a regular
file with no group/other permissions, at most 64KiB. Invalid JSON errors never echo its contents.
Keep the real account password and control token outside this repository. The required JSON shape:

```json
{
  "login": {
    "login_host": "192.168.1.230", "login_port": 5999, "world_port": 9000,
    "username": "eqoxidepoc", "password": "PRIVATE",
    "character_name": "Eqoxidepoc",
    "character_create": {
      "race": 1, "class": 7, "gender": 0, "deity": 396, "start_zone": 2,
      "str": 80, "sta": 80, "agi": 105, "dex": 85, "wis": 75, "int": 75, "cha": 75
    }
  },
  "expected_world": { "name": "Bump Staging", "host": "192.168.1.220" },
  "api_port": 18765,
  "control_token": "PRIVATE-GENERATED-TOKEN-AT-LEAST-32-CHARACTERS",
  "maps_dir": "/private/native-maps"
}
```

`character_create` is optional if the named character exists. When absent, creation is refused;
when present, ordinary server name/combination/stat validation still applies. Cosmetics `face`,
`hairstyle`, `haircolor`, `beard`, `beardcolor`, `eyecolor1`, `eyecolor2` default to zero.
The example is the selected ordinary human monk combination on staging, not a general creation preset.

The listener binds exactly the specified loopback port before login. Port conflicts abort before
connecting. World selection requires exactly one advertised entry matching **both** name and host.
Login is attempted once; a failure remains readable through the API. No first-world fallback runs.
SIGINT/SIGTERM request normal logout. `camp` starts the ordinary approximately 30-second camp,
then normal logout and process exit; neither operation force-saves or edits the account externally.
A failed session remains observable until the process is signalled.

## HTTP control

Every route requires `Authorization: Bearer <control_token>`. There are no legacy mutation routes.
Read `GET /v2/player/state` first. Use its exact `session` and `zone_epoch` in each request:

```http
PUT /v2/player/commands/walk-1
Authorization: Bearer PRIVATE
Content-Type: application/json

{"session":"FROM-STATE","zone_epoch":1,"operation":{"kind":"move_for","east":1,"north":0,"duration_ms":300}}
```

Query `GET /v2/player/commands/walk-1`; repeat the identical PUT after losing a response.
An identical retry returns the retained result and never renews movement. A changed payload under
the same ID conflicts. IDs remain retained for the process lifetime; the journal rejects new work
at capacity, with one additional reserved identity each for `stop` and `attack_off`.
Do not retry an uncertain transaction under a new ID.

The CPU owner expires movement after 1–2000ms and attack after a 1–5000ms lease independently of
caller silence. A new `attack` command renews the lease. Movement uses the shared 35 units/sec
intent and real collision; a local result does not prove the server accepted the resulting position.
Stop, attack-off, death, connection loss, shutdown and a new zone entry stop applicable control.
`stop` stops movement; use `attack_off` separately to stop attacking.

| Operation kind | Fields / limits |
| --- | --- |
| `move_for` | `east`, `north` in [-1,1], `duration_ms` 1–2000; normalized direction, stand first |
| `stop` | none; priority movement stop |
| `target` | `entity_id` from the current received roster |
| `attack` | `lease_ms` 1–5000; live received NPC target within 30 units |
| `attack_off` | none; priority attack stop |
| `loot` | `entity_id` of a received dead NPC within 20 units; ordinary loot protocol |
| `sit`, `stand` | none; sit stops movement and attack |
| `say` | `text`, maximum 512 bytes |
| `tell` | `recipient`, `text` |
| `merchant_open` | `entity_id`, received within 30 units |
| `merchant_close` | none |
| `buy` | catalog `slot`, `quantity:1`; request also needs current `inventory_revision`, `merchant_revision` |
| `sell` | inventory `slot`, positive `quantity`; both revision fields required |
| `inventory_move` | `from_slot`, `to_slot` 0–32, `quantity:1`; inventory revision required |
| `camp` | none; ordinary camp and process shutdown |

Receipts distinguish `accepted`, `sent`, `applied_local`, `confirmed`, `refused`, `unknown`.
This host leaves queued network work accepted until an actual drain outcome or correlated reply;
it does not manufacture a send receipt. Buy and merchant-open confirmation comes from the real
server echo. Most other network commands can report `unknown` after dispatch because the protocol
provides no correlated receipt. Local attack toggles do not prove landed melee; local chat echoes
do not prove delivery. Sell/inventory writes block further transactions until a fresh process;
an ambiguous buy does too. A buy that exceeds the eight-second receipt wait stays blocked
until a fresh process, even if a later zone fence proves that no packet was sent. Late in-process
transaction recovery is unsupported in this POC. Use normal camp/save/relog to reconcile. This
process provides no exactly-once guarantee across a crash.

State contains one published revision, native geometry digest, network tick/inbound/link ages,
predicted feet position, vitals, inventory and catalog revisions. The roster is received knowledge
within 500 units, sorted by spawn ID and capped at 256. `complete:false` discloses that it is not
an authoritative world roster; truncation is separate. LOS is a local static native-map query.
Unknown instance ID is null. Inventory includes local prediction and does not prove a save.

### Self-position updates

The client streams `OP_ClientUpdate` at most every 280 ms while moving and every 1300 ms
otherwise. When a physics step shows the body at rest, it sends one update with the resting
position, zero delta and animation 0 at once, outside that throttle, so the server learns of a
stop within about one 20 ms physics step. The rest decision is made per physics step, not per
network tick. The graphical client does not publish the step signal and keeps the plain cadence.

`self.last_position_send` is the last update the client attempted, or null before the first:
`sequence` (the wire sequence), `position` (feet datum; the wire z is 3.1 units higher),
`delta` and `animation` exactly as sent, `heading` in the state's convention, `reason`
(`moving`, `keepalive`, `rest` or `correction`), `sent_at_ms`, `age_ms`, and
`delivery_confirmed:false`. The update is an unreliable datagram; this proves the attempt, not
receipt. Top-level `clock_ms` is the host's monotonic clock since process start; every
`*_at_ms` field uses it.

`GET /v2/player/events?after=SESSION:CURSOR&wait=5` long-polls for at most 5 seconds. Use the
state's `event_cursor` verbatim. Gaps are explicit; reread state and outstanding commands after one.
Received chat (including NPC say) carries packet provenance; other game events are labeled client events.
Outbound local messages are not receipt evidence. `received_self_echo` records a server-returned
self message with channel/sender/text and `recipient_delivery_confirmed:false`; it does not confirm
delivery to another player. Legacy self-echo filtering remains unchanged. Received spawn class is exposed as nullable `class_id`.
Automatic corpse looting and automatic zone-line requests are disabled for CPU sessions.
Only explicit `loot` requests begin looting, including for the character's own kills. The ordinary
flow requests each received corpse catalog slot once, one item at a time. An unacknowledged take
is never retried automatically. `confirmed` with `corpse_opened` proves the open handshake only.
Corpse catalog packets and take acknowledgements never credit player inventory; acquisition needs
a separate server inventory packet, and persistence still needs normal save/relog evidence.

## Limits

Initial qualification covers one dry Qeynos2 route. Dynamic doors, native climb metadata,
automatic route planning/zoning, spells, P2P trade, corpse retrieval and graphical visibility are
outside this POC. Native map v1 is rejected. Missing or invalid collision prevents movement/readiness;
water-data failure is disclosed independently. Geometry uses bounded decompression, model/placement
expansion and grid construction. The runtime never accepts privileged observer facts as perception.
