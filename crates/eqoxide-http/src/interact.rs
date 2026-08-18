//! `/v1/interact/*` — NPC/world interaction: hail, say, loot, give (turn-in), doors, sit/stand.

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::Response,
    routing::post,
    Json, Router,
};
use tokio::sync::oneshot;
use std::time::Duration;
use eqoxide_command::{CommandResult, GiveOk};
use super::*;
use crate::refusal::Refusal;

/// HTTP-side await budget for POST /v1/interact/give (#448). Set GREATER than the net-side worst-case
/// verdict time — the two `tick_give` timeouts run in SEQUENCE, so the net side delivers a verdict by
/// ≈ (GIVE_ACK_TIMEOUT_TICKS + GIVE_FINISH_TIMEOUT_TICKS) × ~150ms ≈ 6s (see `action_loop`). Awaiting
/// 8s here guarantees the NET verdict (Resolved/Unconfirmed from the state machine) reaches the caller
/// rather than a vaguer HTTP-elapsed 202 firing first — the two-timeout ordering landmine.
pub const GIVE_HTTP_TIMEOUT_SECS: u64 = 8;

// ── 409 CONFLICT bodies for an occupied command slot (#347 step 2) ───────────────────────────────
// Each `/v1/interact/*` verb queues into a single-slot mailbox the net thread drains once per tick.
// Before #347 a second request inside that window OVERWROTE the pending one and BOTH callers were
// told `200`, so one of the two actions silently never happened. The slot now refuses the second
// write and keeps the first, and these are what the caller is told instead. A 409 here means the
// request was NOT queued and definitively did not happen — retrying after the drain is safe.
const BUSY_HAIL: &str = "a hail is already queued and undrained — retry in a moment (it was NOT queued)";
const BUSY_SAY: &str = "a say is already queued and undrained — retry in a moment (it was NOT queued)";
const BUSY_LOOT: &str = "a loot request is already queued and undrained — retry in a moment (it was NOT queued)";
// N4 (round-1 review): `give` is the ONE slot whose drain sits after `tick()`'s dead-player
// early-return — `tick_give` is called past `walker.nav_halt_if_dead(gs) { return; }` (verified by
// reading `ActionLoop::tick`, `action_loop.rs`). So while the character is dead a queued give is
// never drained, and the SECOND give inside that window gets this 409 with "retry in a moment" —
// truthful about not being queued, misleading about when retrying will help. Not fixed here: the
// honest fix is a dead-player door check on `/v1/interact/give`, which is a new refusal rule rather
// than the two this PR is scoped to, and it needs the same "is `gs` sure the player is dead?"
// analysis the pet door check was deferred for (see `BUSY_PET` in `pet.rs`).
const BUSY_GIVE: &str = "a give is already queued and undrained — retry in a moment (it was NOT queued)";
const BUSY_DOOR: &str = "a door click is already queued and undrained — retry in a moment (it was NOT queued)";
// `sit`/`stand` and `run`/`walk` are TOGGLES and still refuse, where `request_camp` — also a toggle
// — is deliberately last-wins. That asymmetry is not an oversight (round-1 review, N1); the line is
// whether the command carries the caller's intended END STATE:
//   * `/sit` vs `/stand` and `/run` vs `/walk` pass an explicit `true`/`false`. Dropping one leaves
//     the agent believing a posture it does not have, which is #347 exactly — so they refuse, and
//     the caller learns its request did not happen.
//   * `request_camp` passes `CampCmd`, and `CampCmd::Start` (POST /v1/lifecycle/exit) MUST be able
//     to override an in-progress camp — it is the only way to tear down a wedged session. Refusing
//     there would strand it, so that slot is last-wins by design (see `eqoxide_command::lifecycle`).
// Neither is "the toggle rule"; the rule is that a slot may only be last-wins when a later write is
// strictly more authoritative than the one it replaces.
const BUSY_SIT: &str = "a sit/stand is already queued and undrained — retry in a moment (it was NOT queued)";
const BUSY_RUN_MODE: &str = "a run/walk toggle is already queued and undrained — retry in a moment (it was NOT queued)";
const BUSY_DIALOGUE: &str = "a dialogue click is already queued and undrained — retry in a moment (it was NOT queued)";
const BUSY_READ: &str = "a book read is already queued and undrained — retry in a moment (it was NOT queued)";

/// A quick `(StatusCode, String)` plain-text response, so the small error paths stay terse.
fn text(status: StatusCode, body: impl Into<String>) -> Response {
    Response::builder().status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body.into())).unwrap()
}

/// A JSON response with an explicit status.
fn json(status: StatusCode, value: serde_json::Value) -> Response {
    Response::builder().status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string())).unwrap()
}

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/hail", post(post_hail))
        .route("/say", post(post_say))
        .route("/loot", post(post_loot))
        .route("/give", post(post_give))
        .route("/click_door", post(post_door_click))
        .route("/sit", post(post_sit))
        .route("/stand", post(post_stand))
        .route("/run", post(post_run))
        .route("/walk", post(post_walk))
        .route("/dialogue", post(post_dialogue))
        .route("/read", post(post_read))
}

/// POST /v1/interact/read — read a book or note. Body: `{"slot": N}` where N is the item's
/// inventory wire slot (from GET /v1/observe/inventory; the item must have a non-empty `filename`).
/// Sends OP_ReadBook; the server replies with the text, which appears at GET /v1/observe/item_text
/// (and in the message log under the "book" kind). (#288)
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadBody {
    slot: i32,
}

async fn post_read(
    State(s): State<HttpState>,
    body: Result<Json<ReadBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"slot\":N}".into()),
    };
    // Validate against the last-published inventory so a bad slot fails fast with a clear message,
    // rather than being silently dropped by the nav thread.
    let readable = s.inventory_slots.inventory.lock().unwrap().iter()
        .find(|i| i.slot == b.slot)
        .map(|i| !i.filename.is_empty());
    match readable {
        Some(true) => {
            if let Some(busy) = s.command.request_read_book(b.slot).refused(BUSY_READ) { return busy; }
            tracing::info!("read: queued book slot={}", b.slot);
            (StatusCode::OK, format!("reading item in slot {}", b.slot))
        }
        Some(false) => (StatusCode::CONFLICT, format!("item in slot {} is not readable", b.slot)),
        None => (StatusCode::NOT_FOUND, format!("no item in slot {}", b.slot)),
    }
}

/// POST /v1/interact/dialogue — click one of the NPC-dialogue choices from GET
/// /v1/observe/dialogue. Body is either `{"index": N}` (position in the choices list) or
/// `{"text": "..."}` (matched case-insensitively against a choice's label). Sends an
/// OP_ItemLinkClick so the server resolves the saylink and treats it as our reply to the NPC. (#120)
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DialogueBody {
    index: Option<usize>,
    text:  Option<String>,
}

async fn post_dialogue(
    State(s): State<HttpState>,
    body: Result<Json<DialogueBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"index\":N} or {\"text\":\"...\"}".into()),
    };
    // #952/#956 (agent-honesty): `index` and `text` are two ways to name ONE choice, and the
    // `if let`/`else if let` below is a precedence chain: `index` is consulted first and `text` is
    // reached only when `index` is absent, so a body carrying both always resolves to the INDEX and
    // the text is discarded with nothing in the response saying so. Measured on the pre-fix tree,
    // `{"index":0,"text":"bind"}` answered `200 clicking 'bind'` — the fixture's choice 0 happened
    // to BE "bind", which is exactly how this defect hides: the two forms agreeing is the common
    // case, and the response looks identical when they do not. A saylink click is a reply to an NPC;
    // the wrong one advances the wrong dialogue.
    //
    // Deliberately BEFORE the empty-roster 409: with both forms sent the request is malformed
    // whether or not choices exist, and `409 no dialogue choices available` would point an agent at
    // the NPC when the thing to fix is its own body. Destructured exhaustively (no `..`).
    let DialogueBody { index, text } = &b;
    if let Some(msg) = crate::req_form::conflicting_forms(
        "dialogue choice", &[("index", index.is_some()), ("text", text.is_some())],
    ) {
        return (StatusCode::BAD_REQUEST, msg);
    }
    let choices = s.interact.dialogue.lock().unwrap().clone();
    if choices.is_empty() {
        return (StatusCode::CONFLICT, "no dialogue choices available".into());
    }
    let chosen = if let Some(i) = b.index {
        choices.get(i).cloned()
    } else if let Some(t) = &b.text {
        choices.iter().find(|c| c.text.eq_ignore_ascii_case(t.trim())).cloned()
    } else {
        return (StatusCode::BAD_REQUEST, "provide {\"index\":N} or {\"text\":\"...\"}".into());
    };
    match chosen {
        Some(c) => {
            let label = c.text.clone();
            if let Some(busy) = s.command.request_dialogue_click(c).refused(BUSY_DIALOGUE) { return busy; }
            tracing::info!("dialogue: queued click {:?}", label);
            (StatusCode::OK, format!("clicking '{}'", label))
        }
        None => (StatusCode::NOT_FOUND, "no matching dialogue choice".into()),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HailBody {
    /// NPC to hail (fuzzy-matched against /observe/entities). Omit to hail the nearest NPC.
    name: Option<String>,
}

/// POST /v1/interact/hail — say "Hail, <name>" so a nearby NPC fires its hail/quest script.
/// Body: {"name":"Guard Phaeton"} (fuzzy) or {} to hail the nearest NPC.
/// The NPC must be within ~200 units (server-enforced say range).
async fn post_hail(
    State(s): State<HttpState>,
    OptionalJson(body): OptionalJson<HailBody>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let requested = body.and_then(|b| b.name);
    let positions = s.world.entity_positions();

    let resolved: Option<String> = if let Some(name) = &requested {
        // Exact (clean) match first, then fuzzy substring.
        let nl = name.to_lowercase();
        positions.keys()
            .find(|k| clean_entity_name(k).to_lowercase() == nl)
            .or_else(|| positions.keys().find(|k| k.to_lowercase().contains(&nl)))
            .cloned()
    } else {
        // Nearest NPC to the player. Camera focus = [east, north, height] =
        // [server_x, server_y, server_z]; entities stored as (server_x, server_y, z).
        let focus = s.camera.snapshot.lock().unwrap().focus;
        positions.iter()
            .filter(|(k, _)| !k.contains("zone_controller"))
            .map(|(k, &(ex, ny, _))| {
                let de = ex - focus[0];
                let dn = ny - focus[1];
                (k.clone(), de * de + dn * dn)
            })
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(k, _)| k)
    };

    match resolved {
        Some(key) => {
            let display_name = clean_entity_name(&key);
            // Resolve the NPC's spawn_id so the nav thread can target it before saying — the
            // server only fires EVENT_SAY on the player's current target (#130).
            let spawn_id = s.world.entity_ids().get(&key).copied();
            if let Some(busy) = s.command.request_hail(display_name.clone(), spawn_id).refused(BUSY_HAIL) { return busy; }
            tracing::info!("hail: queued hail to {:?} (spawn_id={:?})", display_name, spawn_id);
            (StatusCode::OK, format!("hailing {}", display_name))
        }
        None => {
            let msg = match &requested {
                Some(n) => format!("No NPC matching {:?}", n),
                None => "No NPCs known to hail".to_string(),
            };
            (StatusCode::NOT_FOUND, msg)
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SayBody {
    text: String,
}

/// POST /v1/interact/say {"text":"..."} — say arbitrary text on the Say channel. Used for quest
/// keyword follow-ups (e.g. say "shipment" after an NPC mentions [shipment]).
async fn post_say(
    State(s): State<HttpState>,
    body: Result<Json<SayBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let text = match body {
        Ok(Json(b)) => b.text,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"text\":\"...\"}".into()),
    };
    if text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "empty text".into());
    }
    if let Some(busy) = s.command.request_say(text.clone()).refused(BUSY_SAY) { return busy; }
    tracing::info!("say: queued {:?}", text);
    (StatusCode::OK, format!("saying {}", text))
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct LootBody {
    /// Corpse spawn id to loot directly.
    id:   Option<u32>,
    /// Corpse name to fuzzy-match (corpses are named like "a_rat000's corpse").
    name: Option<String>,
}

/// A spawn's entity-list key names a corpse (the only class this endpoint is allowed to queue —
/// eqoxide#346: a live mob or a nonexistent spawn must never be silently "looted").
fn is_corpse_key(key: &str) -> bool {
    key.to_lowercase().contains("corpse")
}

fn queue_loot(s: &HttpState, name: String, id: u32) -> (StatusCode, String) {
    if let Some(busy) = s.command.request_loot(id).refused(BUSY_LOOT) { return busy; }
    tracing::info!("loot: queued corpse {:?} (spawn_id={})", name, id);
    (StatusCode::OK, format!("looting {} (spawn_id={})", clean_entity_name(&name), id))
}

/// POST /v1/interact/loot — open a corpse and take all its items, reusing the auto-loot machinery
/// (OP_LootRequest → echo each OP_LootItem → OP_EndLootRequest). Must be near the corpse; looted
/// items land in inventory (see GET /v1/observe/inventory). Body: {"id":N} for a specific corpse
/// spawn id, {"name":"..."} to fuzzy-match a corpse name, or {} for the nearest corpse.
///
/// Every path (id / name / nearest) is restricted to entities whose key names a corpse — eqoxide#346
/// found that the explicit `id`/`name` paths had NO such check, so an unknown id defaulted to
/// `format!("spawn {}", id)` and a 200, and a name like "rat" could match a live `a_rat01` standing
/// next to `a_rat00's corpse`. A nonexistent id or a name matching no corpse is 404; a name matching
/// more than one corpse is 409 (ambiguous) rather than silently picking one.
async fn post_loot(
    State(s): State<HttpState>,
    OptionalJson(body): OptionalJson<LootBody>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let b = body.unwrap_or_default();
    // #952/#956 (agent-honesty): `id` and `name` are two ways to name ONE corpse, and the id branch
    // below `return`s before the name branch is ever reached — `{"id":7,"name":"a_rat00"}` used to
    // answer `200 looting <the id's corpse>` even when the name pointed at a different corpse
    // entirely, with nothing in the response saying the name had been discarded. Refused, because a
    // wrong corpse is a wrong loot session. Destructured exhaustively (no `..`).
    let LootBody { id, name } = &b;
    if let Some(msg) = crate::req_form::conflicting_forms(
        "corpse selection", &[("id", id.is_some()), ("name", name.is_some())],
    ) {
        return (StatusCode::BAD_REQUEST, msg);
    }
    if let Some(id) = b.id {
        let ids = s.world.entity_ids();
        let found = ids.iter().find(|(_, &v)| v == id).map(|(k, _)| k.clone());
        drop(ids);
        return match found {
            Some(key) if is_corpse_key(&key) => queue_loot(&s, key, id),
            Some(key) => (StatusCode::NOT_FOUND,
                format!("spawn_id {} is not a corpse ({})", id, clean_entity_name(&key))),
            None => (StatusCode::NOT_FOUND, format!("no spawn with id {}", id)),
        };
    }
    if let Some(name) = &b.name {
        let ids = s.world.entity_ids();
        let nl = name.to_lowercase();
        let matches: Vec<(String, u32)> = ids.iter()
            .filter(|(k, _)| is_corpse_key(k)
                && (k.to_lowercase().contains(&nl) || clean_entity_name(k).to_lowercase().contains(&nl)))
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        drop(ids);
        return match matches.len() {
            0 => (StatusCode::NOT_FOUND, format!("no corpse matching {:?}", name)),
            1 => { let (key, id) = matches[0].clone(); queue_loot(&s, key, id) }
            n => (StatusCode::CONFLICT,
                format!("ambiguous corpse name {:?} matches {} corpses — use {{\"id\":N}}", name, n)),
        };
    }
    // Nearest corpse to the player (camera focus = player pos).
    let focus = s.camera.snapshot.lock().unwrap().focus;
    let positions = s.world.entity_positions();
    let ids = s.world.entity_ids();
    let resolved = positions.iter()
        .filter(|(k, _)| is_corpse_key(k))
        .map(|(k, &(x, y, _))| {
            let (dx, dy) = (x - focus[0], y - focus[1]);
            (k.clone(), dx * dx + dy * dy)
        })
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .and_then(|(k, _)| ids.get(&k).map(|&id| (k, id)));
    drop(positions);
    drop(ids);
    match resolved {
        Some((name, id)) => queue_loot(&s, name, id),
        None => (StatusCode::NOT_FOUND, "no corpse found to loot".into()),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GiveBody {
    /// NPC name to hand the item to (fuzzy-matched, like /merchant/buy and /combat/target/name).
    npc: String,
    /// Inventory slot holding the item to give (e.g. 23 for a general/bag slot, or 30 if it's
    /// already on the cursor).
    from: u32,
}

/// POST /v1/interact/give {"npc":"<name>","from":N} — hand inventory item in slot N to the named NPC
/// and complete an EQ quest turn-in (trade-window flow). Must be within trade range. The nav thread
/// runs a multi-tick state machine: it puts the item on the cursor + sends OP_TradeRequest, waits
/// for OP_TradeRequestAck, then moves the item into the NPC trade slot + sends OP_TradeAcceptClick.
///
/// A3 Migration 2 (#448) — Command-with-result: this no longer returns a premature "queued" 200. It
/// AWAITS the real outcome (up to 8s) and reports it honestly:
///   • 200 — the turn-in was CONFIRMED: OP_FinishTrade arrived AND the item actually LEFT inventory
///     (verify-transfer, #486). Body: `{status:"given", item, npc_id}`.
///   • 409 — REFUSED before sending: a give was already in flight (singleton-in-flight). Body:
///     `{status:"refused", reason}`. No second trade was started.
///   • 202 — the outcome is UNKNOWN or the item did NOT transfer. This covers the no-ack abort, the
///     ITEM-MISMATCH case (item returned on the cursor with NO OP_FinishTrade), a lost reply, a zone
///     change mid-give, AND — the #486 fix — a give where OP_FinishTrade DID arrive but the NPC
///     REJECTED / was OUT OF RANGE, returning the item to the player (OP_FinishTrade only ends the
///     trade SESSION; it does NOT prove acceptance). The body says so explicitly. A 202 MUST NOT be
///     read as success — that is the whole honesty invariant of A3 (see `eqoxide_command::result`).
async fn post_give(
    State(s): State<HttpState>,
    body: Result<Json<GiveBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err((code, msg)) = require_live_session(&s) { return text(code, msg); }
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return text(StatusCode::BAD_REQUEST, "provide {\"npc\":\"...\",\"from\":N}"),
    };
    // Resolve the NPC, then DROP the entity map lock before awaiting — never hold a std Mutex across
    // an `.await`.
    let found = {
        let ids = s.world.entity_ids();
        let nl = b.npc.to_lowercase();
        ids.iter()
            .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
            .map(|(k, &id)| (k.clone(), id))
    };
    let (key, id) = match found {
        Some(hit) => hit,
        None => return text(StatusCode::NOT_FOUND, format!("no NPC matching {:?}", b.npc)),
    };

    // #347 step 1 (reject at the door): an empty `from` slot cannot be given away. The net-side
    // state machine would move nothing to the cursor and then trade whatever was already there —
    // or open an empty trade — and the caller would be told 202 "unconfirmed" at best. Checked
    // against the last published inventory (GET /v1/observe/inventory).
    let occupied = s.inventory_slots.inventory.lock().unwrap().iter().any(|i| i.slot == b.from as i32);
    if !occupied {
        return text(StatusCode::NOT_FOUND, format!("no item in slot {} to give", b.from));
    }

    // Park the give with a result channel and await the TRUE outcome (park → fulfil → timeout).
    let (tx, rx) = oneshot::channel::<CommandResult<GiveOk>>();
    // #347 step 2: if the give mailbox already holds an undrained give, refuse. Before the fix
    // this OVERWROTE it — the first caller's oneshot Sender was dropped, so THAT caller fell into
    // the `_` arm and was told 202 for a give that was never sent. Now the second caller is
    // refused outright and the first give is preserved.
    if let Some(busy) = s.command.request_give_await(id, b.from, tx)
        .refused_json(serde_json::json!({ "status": "refused", "reason": BUSY_GIVE })) { return busy; }
    tracing::info!("give: awaited give queued — npc {:?} (spawn_id={}) from_slot={}", key, id, b.from);

    match tokio::time::timeout(Duration::from_secs(GIVE_HTTP_TIMEOUT_SECS), rx).await {
        // A REAL OP_FinishTrade landed — the NPC accepted the item.
        Ok(Ok(CommandResult::Resolved(GiveOk { npc_id, item_name }))) => json(
            StatusCode::OK,
            serde_json::json!({
                "status": "given",
                "item": item_name,
                "npc_id": npc_id,
            }),
        ),
        // A pre-send rejection: another give was already in flight (singleton-in-flight).
        Ok(Ok(CommandResult::Refused(reason))) => json(
            StatusCode::CONFLICT,
            serde_json::json!({ "status": "refused", "reason": reason }),
        ),
        // Unconfirmed, channel closed (Sender dropped — disconnect), or elapsed: the outcome is
        // genuinely UNKNOWN. The no-ack abort and the ITEM-MISMATCH case (item returned on the cursor,
        // no OP_FinishTrade) both land here. MUST NOT read as success — 202 with an explicit body.
        _ => json(
            StatusCode::ACCEPTED,
            serde_json::json!({
                "status": "unconfirmed",
                "message": "give sent to the NPC, but the outcome is UNKNOWN — no OP_FinishTrade \
                            confirmation arrived. The NPC may not have accepted the item (a quest \
                            turn-in the item doesn't match returns it to you with no confirmation), \
                            the trade may have timed out, or the reply was lost. Re-check GET \
                            /v1/observe/inventory for the item before assuming it succeeded.",
                "npc": clean_entity_name(&key),
                "from": b.from,
            }),
        ),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DoorClickBody { door_id: Option<u8>, name: Option<String> }

/// The 400 for an empty body — the ONLY malformed-request failure this endpoint has. #891 found the
/// `None` arm of the old `if`/`else if` serving this same text for a *name that matched nothing*,
/// which told an agent to send an argument it had just sent. A lookup miss now gets
/// [`door_lookup_miss`] instead, and these two texts never overlap again.
const DOOR_NO_ARGUMENT: &str = "provide {\"door_id\":N} or {\"name\":\"...\"}";

/// The 404 body for a door the roster does not contain (#891, agent-honesty).
///
/// `what` names what was looked for; the two call sites below render `id 250` and
/// `the name "HHCELL"` — those two forms and no other. `known` is the number of entries in
/// `interact.doors_shared`, the same list GET /v1/observe/doors serves. Phrased in the same terms
/// as the dead-net-thread refusal in `require_live_session`: it says outright that the click was
/// NOT sent, so an agent does not read the failure as "queued but unconfirmed".
///
/// **Neither body claims the roster is complete, and neither says "never" (#934 review B2).**
/// `GameState::upsert_door` inserts into a map that only zone-in clears, and
/// `ActionLoop::sync_doors` republishes after every applied packet — so a further `OP_SpawnDoor`
/// GROWS the roster, and *populated* never implies *final*. A refusal that told an agent "do not
/// retry this body, it can never resolve" would be a universal about a list that changes underneath
/// it, and a literal JSON body is not zone-scoped: an agent obeying it would cache a permanent
/// refusal of `{"door_id":250}` for the whole session. Both bodies therefore scope the refusal to
/// *the roster as it stands*, state how the roster changes, and send the caller back to
/// GET /v1/observe/doors.
///
/// **The empty-roster case is worded differently, because it is a different claim.** With no entries
/// the client cannot tell "this zone has no doors" from "this zone's doors have not landed, or have
/// landed and not yet been published, yet". Zoning empties the roster — `GameState::begin_zone_in`
/// for `gs.world.doors`, and the paired clear at the top of `gameplay::run_zone_entry_handshake` for
/// this published copy (#934 review B1) — so a zone-in in progress can read exactly like a doorless
/// zone.
///
/// **This is narrower on one path than the other, and the body does not claim to know which path it
/// is on (#1016 review B5).** `run_zone_entry_handshake` — used only for re-zones, both its
/// production call sites are inside `run_gameplay_phase` in `gameplay.rs` — republishes on the same
/// drain pass that applies a door record (#1016 review B1), so on THAT path the empty reading narrows
/// to "before the first record has landed". The very first zone-in of a session goes through
/// `login.rs`'s own separate state machine instead (`run_login_phase`), which applies `OP_SpawnDoor`
/// via the same `apply_packet` but has no access to `InteractSlots` at all (not in its function
/// signature) and so cannot publish into this roster — on THAT path the empty reading can span the
/// whole login handshake, however long that takes, even after door records have already landed in
/// `gs.world.doors`. This endpoint has no way to tell which of the two zone-entry paths produced the
/// empty roster it is looking at, so the body below does not name a bound on the window — narrowing
/// it to the re-zone path's tighter bound would be confidently wrong on the other path (the #937
/// shape survives there verbatim). See `docs/http-api.md` and `observe.rs`'s `get_doors` doc for the
/// same distinction; this is the one HTTP-visible location it also has to hold in.
///
/// Both bodies are pinned VERBATIM by `door_click_populated_miss_body_is_exactly_this` and
/// `door_click_empty_roster_body_is_exactly_this`: the strings are what this endpoint delivers, so
/// any edit to them — deletion, rewording, or an addition that wraps them — has to go through a
/// test that spells out why each clause is there.
fn door_lookup_miss(what: &str, known: usize) -> (StatusCode, String) {
    let body = if known == 0 {
        format!(
            "no door matching {what}: this client's door roster is EMPTY. That does NOT establish \
             that the door does not exist — an empty roster does not distinguish a genuinely \
             doorless zone from a zone-in still in progress, whose door records may not have \
             reached this client yet, or may have arrived but not yet been published into this \
             roster, and this client cannot tell those cases apart. This click was NOT sent and \
             will not take effect. Re-list with GET /v1/observe/doors: if it is still empty once \
             the zone has finished loading, there is no door here to click."
        )
    } else {
        let doors = if known == 1 { "door" } else { "doors" };
        format!(
            "no door matching {what} among the {known} {doors} this client currently holds. This \
             click was NOT sent and will not take effect. No retry of this body can resolve against \
             the roster as it stands — but the roster is not fixed: it grows as further door \
             records arrive, and zoning empties it. Re-list with GET /v1/observe/doors and use a \
             `door_id` or `name` from there."
        )
    };
    (StatusCode::NOT_FOUND, body)
}

/// POST /v1/interact/click_door {"door_id": N}  or  {"name": "DOOR1"} (case-insensitive name match).
///
/// **Both forms are resolved against the same door roster before anything is queued** (#891). Until
/// that issue the id form did no lookup at all: it echoed the caller's number straight back as
/// `200 "clicking door 250"` even when the client held no such door — measured against both an empty
/// roster and a populated 70-door one — while the name form three lines away consulted the roster
/// correctly. A `door_id` is a `u8`, so an unvalidated wrong id does not merely address nothing; it
/// can address a *different real door*. Both forms now go through the same lookup, against one
/// snapshot of one list.
///
/// That is a check against a published snapshot, and claims no more than one: the roster is cloned
/// before `request_door_click`, so a door can in principle resolve here and be gone by the time the
/// net thread drains the click. What it does rule out is the #891 shape — answering `200` for a
/// door the client has no record of at all.
///
/// Failure modes, each with its own body so the caller can act on them differently:
///   * neither argument given → 400 [`DOOR_NO_ARGUMENT`] (the request shape is wrong)
///   * argument given, no match in the roster → 404 [`door_lookup_miss`] (the request shape is fine)
///   * the command slot is still occupied → 409 [`BUSY_DOOR`]
async fn post_door_click(
    State(s): State<HttpState>,
    body: axum::extract::Json<DoorClickBody>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    // #952/#956 (agent-honesty): `door_id` and `name` are two ways to name ONE door, and the
    // `if`/`else if` below is a precedence chain — `{"door_id":7,"name":"HHCELL"}` used to answer
    // `200 clicking door 7` whether or not HHCELL *was* door 7, discarding the name silently. #891
    // already established for this endpoint that both forms must resolve against the same roster
    // before anything is queued, and that a miss must be named as a miss rather than answered `200`;
    // a discarded second form is the same failure one step earlier. Checked BEFORE the roster snapshot: the request
    // is malformed regardless of what the roster holds, so answering `404 no door matching …` first
    // would send an agent to re-list doors when the thing to fix is its own body. Destructured
    // exhaustively (no `..`).
    let DoorClickBody { door_id, name } = &*body;
    if let Some(msg) = crate::req_form::conflicting_forms(
        "door selection", &[("door_id", door_id.is_some()), ("name", name.is_some())],
    ) {
        return (StatusCode::BAD_REQUEST, msg);
    }
    // Snapshot the roster ONCE so the resolution and the "N doors known" figure in any failure body
    // describe the same list — a second `lock()` could report a count the lookup never saw.
    let roster = s.interact.doors_shared.lock().unwrap().clone();
    let known = roster.len();
    let id = if let Some(want) = body.door_id {
        match roster.iter().find(|d| d.door_id == want) {
            Some(d) => d.door_id,
            None => return door_lookup_miss(&format!("id {want}"), known),
        }
    } else if let Some(name) = &body.name {
        let up = name.to_uppercase();
        match roster.iter().find(|d| d.name.to_uppercase() == up) {
            Some(d) => d.door_id,
            None => return door_lookup_miss(&format!("the name {name:?}"), known),
        }
    } else {
        return (StatusCode::BAD_REQUEST, DOOR_NO_ARGUMENT.into());
    };
    if let Some(busy) = s.command.request_door_click(id).refused(BUSY_DOOR) { return busy; }
    (StatusCode::OK, format!("clicking door {}", id))
}

/// POST /v1/interact/sit — sit down (mana/HP regen).
async fn post_sit(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if let Some(busy) = s.command.request_sit(true).refused(BUSY_SIT) { return busy; }
    (StatusCode::OK, "sit queued".into())
}

/// POST /v1/interact/stand — stand up.
async fn post_stand(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if let Some(busy) = s.command.request_sit(false).refused(BUSY_SIT) { return busy; }
    (StatusCode::OK, "stand queued".into())
}

/// POST /v1/interact/run — switch to run mode: sends `OP_SetRunMode` (#625) and speeds the local
/// nav walker back up to `RUN_SPEED`. See `run_mode` in `/v1/observe/debug` for the last-sent state
/// (a send-time intent — the opcode has no server ack, same epistemic level as sit/auto_attack).
async fn post_run(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if let Some(busy) = s.command.request_run_mode(true).refused(BUSY_RUN_MODE) { return busy; }
    (StatusCode::OK, "run queued".into())
}

/// POST /v1/interact/walk — switch to walk mode: sends `OP_SetRunMode` (#625) and slows the local
/// nav walker to `WALK_SPEED`.
async fn post_walk(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if let Some(busy) = s.command.request_run_mode(false).refused(BUSY_RUN_MODE) { return busy; }
    (StatusCode::OK, "walk queued".into())
}

#[cfg(test)]
mod tests {
    use super::router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use crate::testkit::empty_state;

    fn seed_npc(state: &crate::HttpState, key: &str, id: u32, pos: (f32, f32, f32)) {
        state.world.entity_positions_mut().insert_for_test(key.to_string(), pos);
        state.world.entity_ids_mut().insert_for_test(key.to_string(), id);
    }

    /// Publish one item at a wire slot, exactly as `OP_CharInventory` decoding does, so the #347
    /// step-1 "the `from` slot holds an item" door check sees the same state a live client would.
    fn seed_item(state: &crate::HttpState, slot: i32, name: &str) {
        state.inventory_slots.inventory.lock().unwrap().push(eqoxide_core::game_state::InvItem {
            slot, item_id: 13073, name: name.into(), charges: 1, ..Default::default()
        });
    }

    // --- run/walk (#625): the toggle queues an intent for action_loop to send OP_SetRunMode ----

    #[tokio::test]
    async fn run_endpoint_queues_run_mode_true() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/run").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_run_mode(), Some(true));
    }

    #[tokio::test]
    async fn walk_endpoint_queues_run_mode_false() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/walk").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_run_mode(), Some(false));
    }

    // --- hail: a malformed name must not silently fall back to "nearest NPC" -------------------

    #[tokio::test]
    async fn hail_no_body_hails_nearest_npc() {
        let state = empty_state();
        seed_npc(&state, "Guard_Phaeton000", 5, (1.0, 1.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/hail").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(command.take_hail().is_some());
    }

    #[tokio::test]
    async fn hail_malformed_name_is_400_and_does_not_hail_nearest() {
        let state = empty_state();
        seed_npc(&state, "Guard_Phaeton000", 5, (1.0, 1.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/hail")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":5}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(command.take_hail().is_none(),
            "a malformed name must not silently fall through to hailing the nearest NPC");
    }

    /// eqoxide#341: a typo'd key ("nmae" instead of "name") must 400 — not be silently ignored by
    /// serde (leaving `name` at its default `None`) and fall through to hailing the nearest NPC.
    #[tokio::test]
    async fn hail_unknown_key_is_400_and_does_not_hail_nearest() {
        let state = empty_state();
        seed_npc(&state, "Guard_Phaeton000", 5, (1.0, 1.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/hail")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"nmae":"Guard"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(command.take_hail().is_none(),
            "a typo'd key must not silently fall through to hailing the nearest NPC");
    }

    // --- loot: a malformed id must not silently fall back to "nearest corpse" ------------------

    #[tokio::test]
    async fn loot_no_body_loots_nearest_corpse() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/loot").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_loot(), Some(9));
    }

    #[tokio::test]
    async fn loot_malformed_id_is_400_and_does_not_loot_nearest() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":"not-a-number"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(command.take_loot().is_none(),
            "a malformed id must not silently fall through to looting the nearest corpse");
    }

    /// eqoxide#341: a typo'd key ("idd" instead of "id") must 400 — not be silently ignored by serde
    /// (leaving `id` at its default `None`) and fall through to looting the nearest corpse.
    #[tokio::test]
    async fn loot_unknown_key_is_400_and_does_not_loot_nearest() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"idd":9}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(command.take_loot().is_none(),
            "a typo'd key must not silently fall through to looting the nearest corpse");
    }

    // --- loot: eqoxide#346 — every path must be restricted to an actual corpse -----------------
    //
    // Baseline on `main` before this fix: {"id":999999} (no such spawn) returned 200
    // "looting spawn 999999", and {"name":"<a live mob>"} happily queued that live mob for
    // looting because the id/name paths never checked `.contains("corpse")` (only the
    // zero-body "nearest corpse" path did).

    #[tokio::test]
    async fn loot_nonexistent_id_is_404_not_200() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":999999}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(command.take_loot().is_none());
    }

    #[tokio::test]
    async fn loot_live_mob_id_is_404_not_a_corpse() {
        let state = empty_state();
        // A live mob (non-corpse key) standing near a corpse.
        seed_npc(&state, "a_rat01", 11, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":11}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND,
            "an id that resolves to a live mob (not a corpse) must never be queued for looting");
        assert!(command.take_loot().is_none());
    }

    #[tokio::test]
    async fn loot_live_mob_name_is_404_not_a_corpse() {
        let state = empty_state();
        seed_npc(&state, "a_rat01", 11, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"a_rat01"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND,
            "a name that only matches a live mob (not a corpse) must never be queued for looting");
        assert!(command.take_loot().is_none());
    }

    #[tokio::test]
    async fn loot_ambiguous_name_is_409_not_a_silent_pick() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        seed_npc(&state, "a_rat001's corpse", 10, (3.0, 3.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"rat"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT,
            "a name matching multiple corpses must be reported as ambiguous, not silently resolved");
        assert!(command.take_loot().is_none());
    }

    #[tokio::test]
    async fn loot_id_matching_a_corpse_still_works() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":9}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_loot(), Some(9));
    }

    #[tokio::test]
    async fn loot_unambiguous_name_matching_a_corpse_still_works() {
        let state = empty_state();
        seed_npc(&state, "a_rat000's corpse", 9, (2.0, 2.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/loot")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"a_rat000"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_loot(), Some(9));
    }

    // ── A3 Migration 2 (#448): POST /v1/interact/give reports the TRUE outcome, not a queued 200 ──

    use eqoxide_command::{CommandResult, GiveOk};

    /// A give to a nonexistent NPC 404s before parking anything.
    #[tokio::test]
    async fn give_unknown_npc_is_404() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/give").header("content-type", "application/json")
            .body(Body::from(r#"{"npc":"Nobody","from":23}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(command.take_give_await().is_none(), "a 404 must not park a give");
    }

    /// #347 step 1: giving FROM a slot the published inventory says is empty cannot work — the
    /// net-side state machine has nothing to put on the cursor. Refused at the door with a 404
    /// rather than parked and answered with a vague 202 eight seconds later.
    #[tokio::test]
    async fn give_from_an_empty_slot_is_404_and_parks_nothing() {
        let state = empty_state();
        seed_npc(&state, "Priest_of_Mischief000", 11, (1.0, 1.0, 0.0));
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/give").header("content-type", "application/json")
            .body(Body::from(r#"{"npc":"Mischief","from":23}"#)).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(command.take_give_await().is_none(), "a 404 must not park a give");
    }

    /// #347 step 2 on an AWAITED command. Before the fix the second give overwrote the first, which
    /// DROPPED the first caller's oneshot Sender — so caller #1 fell into the `_` arm and was told
    /// `202 unconfirmed` for a give that was never sent at all. Now caller #2 is refused with a 409
    /// and caller #1's parked Sender is still live.
    #[tokio::test]
    async fn a_second_give_before_the_drain_is_409_and_the_first_sender_survives() {
        let state = empty_state();
        seed_npc(&state, "Priest_of_Mischief000", 11, (1.0, 1.0, 0.0));
        seed_item(&state, 23, "Bone Chips");
        seed_item(&state, 24, "Rat Ears");
        let command = state.command.clone();
        let app = router().with_state(state);

        // Caller #1 parks and then blocks on its 8s await, so run it as a task.
        let app1 = app.clone();
        let mut first = tokio::spawn(async move {
            app1.oneshot(Request::post("/give").header("content-type", "application/json")
                .body(Body::from(r#"{"npc":"Mischief","from":23}"#)).unwrap()).await.unwrap()
        });

        // Wait until #1's Sender is actually parked, racing its JoinHandle (#717 pattern) so an
        // early return fails loudly instead of spinning forever. Peek WITHOUT draining: `take_*`
        // would empty the slot and destroy the very precondition under test, so poll the slot's
        // occupancy through the public `*_pending` predicate instead.
        tokio::select! {
            _ = async { while !command.give_await_pending() { tokio::task::yield_now().await; } } => {}
            res = &mut first => panic!(
                "expected /give to park a Sender, but the handler returned early with status {}",
                res.expect("handler task panicked").status()),
        }

        let second = app.oneshot(Request::post("/give").header("content-type", "application/json")
            .body(Body::from(r#"{"npc":"Mischief","from":24}"#)).unwrap()).await.unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT,
            "the second give must be refused, not overwrite the first");

        // The FIRST give — slot 23, not 24 — is what the net thread drains, and its Sender still
        // reaches its caller.
        let (npc_id, from_slot, tx) = command.take_give_await().expect("the first give must survive");
        assert_eq!((npc_id, from_slot), (11, 23));
        tx.send(CommandResult::Resolved(GiveOk { npc_id: 11, item_name: "Bone Chips".into() })).unwrap();
        let resp = first.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK,
            "caller #1 must get its real receipt, not a 202 for a give that never happened");
    }

    /// SUCCESS: the server confirms the turn-in (OP_FinishTrade, delivered here as `Resolved`) → 200
    /// with the honest receipt body (item/npc_id).
    #[tokio::test]
    async fn give_confirmed_is_200_with_the_receipt() {
        let state = empty_state();
        seed_npc(&state, "Priest_of_Mischief000", 11, (1.0, 1.0, 0.0));
        seed_item(&state, 23, "Bone Chips");
        let command = state.command.clone();
        let app = router().with_state(state);
        let mut task = tokio::spawn(async move {
            app.oneshot(Request::post("/give").header("content-type", "application/json")
                .body(Body::from(r#"{"npc":"Mischief","from":23}"#)).unwrap()).await.unwrap()
        });
        // Wait for the handler to park its Sender, then deliver the confirmed receipt.
        //
        // #717: race the poll against the handler's own JoinHandle (the pattern #710 established
        // in observe.rs, also applied to the merchant/buy, /open, and /cast tests). A naive
        // unbounded poll loop here would hang forever, not fail, if a change made the handler
        // return early without ever parking `give_await`.
        let (npc_id, from_slot, tx) = tokio::select! {
            p = async {
                loop {
                    if let Some(p) = command.take_give_await() { return p; }
                    tokio::task::yield_now().await;
                }
            } => p,
            res = &mut task => {
                let resp = res.expect("handler task panicked");
                panic!(
                    "expected /give to reach the give-await hand-off, but the handler returned \
                     early with status {} instead", resp.status()
                );
            }
        };
        assert_eq!((npc_id, from_slot), (11, 23));
        tx.send(CommandResult::Resolved(GiveOk { npc_id: 11, item_name: "Bone Chips".into() })).unwrap();

        let resp = task.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "given");
        assert_eq!(v["item"], "Bone Chips");
        assert_eq!(v["npc_id"], 11);
    }

    /// REFUSAL: a give already in flight (singleton-in-flight, delivered as `Refused`) → 409.
    #[tokio::test]
    async fn give_refused_is_409() {
        let state = empty_state();
        seed_npc(&state, "Priest_of_Mischief000", 11, (1.0, 1.0, 0.0));
        seed_item(&state, 23, "Bone Chips");
        let command = state.command.clone();
        let app = router().with_state(state);
        let mut task = tokio::spawn(async move {
            app.oneshot(Request::post("/give").header("content-type", "application/json")
                .body(Body::from(r#"{"npc":"Mischief","from":23}"#)).unwrap()).await.unwrap()
        });
        // #717: see the identical comment on `give_confirmed_is_200_with_the_receipt` above.
        let (_n, _s, tx) = tokio::select! {
            p = async {
                loop {
                    if let Some(p) = command.take_give_await() { return p; }
                    tokio::task::yield_now().await;
                }
            } => p,
            res = &mut task => {
                let resp = res.expect("handler task panicked");
                panic!(
                    "expected /give to reach the give-await hand-off, but the handler returned \
                     early with status {} instead", resp.status()
                );
            }
        };
        tx.send(CommandResult::Refused("a give is already in flight; retry".into())).unwrap();

        let resp = task.await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "refused");
    }

    /// NO-CONFIRMATION SILENCE — THE HONESTY PROOF at the HTTP boundary. Nothing ever fires the parked
    /// Sender (the net-side `tick_give` verdict is exercised in the action_loop tests); here the 8s
    /// timeout elapses in virtual time (`start_paused`) → **202**, NOT 200. The body must say the
    /// outcome is UNKNOWN. The Sender is HELD (not dropped) across the wait, so this exercises the
    /// genuine ELAPSED branch, not a channel-closed shortcut.
    #[tokio::test(start_paused = true)]
    async fn give_with_no_confirmation_is_202_unknown_never_success() {
        let state = empty_state();
        seed_npc(&state, "Priest_of_Mischief000", 11, (1.0, 1.0, 0.0));
        seed_item(&state, 23, "Bone Chips");
        let command = state.command.clone();
        let app = router().with_state(state);
        let mut task = tokio::spawn(async move {
            app.oneshot(Request::post("/give").header("content-type", "application/json")
                .body(Body::from(r#"{"npc":"Mischief","from":23}"#)).unwrap()).await.unwrap()
        });
        // Take the parked Sender and HOLD it — the server's silence, faithfully modelled.
        //
        // #717: race against the handler's own JoinHandle — see `buy_with_no_server_reply_is_202_
        // unknown_never_success` in merchant.rs for why a naive loop here is a pure, unrecoverable
        // spin with `task.await` never reached.
        let held = tokio::select! {
            p = async {
                loop {
                    if let Some(p) = command.take_give_await() { return p; }
                    tokio::task::yield_now().await;
                }
            } => p,
            res = &mut task => {
                let resp = res.expect("handler task panicked");
                panic!(
                    "expected /give to reach the give-await hand-off and park a Sender, but the \
                     handler returned early with status {} instead", resp.status()
                );
            }
        };

        let resp = task.await.unwrap(); // 8s timeout elapses in virtual time
        assert_ne!(resp.status(), StatusCode::OK,
            "a give with no OP_FinishTrade MUST NOT be reported as success — the A3 invariant");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "unconfirmed");
        let msg = v["message"].as_str().unwrap();
        assert!(msg.contains("UNKNOWN"), "the body must state the outcome is unknown");
        drop(held);
    }

    // ── #891: click_door resolves BOTH forms against the roster, and names a miss as a miss ──────
    //
    // Baseline on `main` before this fix, measured live by the reporter against a 70-door roster
    // that did not contain ids 28/45/250:
    //     POST /click_door {"door_id":250}            → 200  clicking door 250
    //     POST /click_door {"name":"NO_SUCH_DOOR_XYZ"} → 400  provide {"door_id":N} or {"name":"..."}
    // The id form invented a success; the name form blamed the caller for omitting an argument it
    // had supplied. `door_id` is a `u8`, so an unchecked id can even name a DIFFERENT real door.

    /// Publish one door exactly as `ActionLoop::sync_doors` does, so these tests see the same
    /// roster GET /v1/observe/doors serves.
    fn seed_door(state: &crate::HttpState, door_id: u8, name: &str) {
        state.interact.doors_shared.lock().unwrap().push(eqoxide_ipc::DoorView {
            door_id, name: name.into(),
            x: 0.0, y: 0.0, z: 0.0, heading: 0.0, opentype: 58, is_open: false,
        });
    }

    async fn body_of(resp: axum::response::Response) -> String {
        let b = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(b.to_vec()).unwrap()
    }

    /// THE HONESTY PROOF for the id form: an id absent from a POPULATED roster must never be
    /// reported as an action in progress, and must never be queued.
    #[tokio::test]
    async fn door_click_unknown_id_with_a_populated_roster_is_404_never_200() {
        let state = empty_state();
        seed_door(&state, 6, "HHCELL");
        seed_door(&state, 7, "HHDOOR");
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/click_door").header("content-type", "application/json")
            .body(Body::from(r#"{"door_id":250}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_ne!(resp.status(), StatusCode::OK,
            "an id the client has no record of MUST NOT be answered as success — #891");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(command.take_door_click().is_none(),
            "a 404 must not queue a door click");
        let body = body_of(resp).await;
        assert!(body.contains("250"), "the body must name the id that missed: {body:?}");
        assert!(body.contains("2 doors"),
            "the body must name how many doors the client does hold: {body:?}");
        assert!(body.contains("NOT sent"),
            "the body must say outright that the click was not sent: {body:?}");
        assert!(!body.contains("clicking door"),
            "the body must not read as an action in progress: {body:?}");
    }

    /// The same id form against an EMPTY roster. Still a 404 and still unqueued — but the body must
    /// NOT assert the door does not exist, because with no separate "doors have arrived" observable
    /// the client cannot tell a doorless zone from one whose door records have not landed yet, OR
    /// (#1016 review B5) from one whose door records landed and were applied to game state but never
    /// published into this roster — still possible on the very first zone-in of a session, which runs
    /// through a separate login state machine with no path to publish a door at all.
    #[tokio::test]
    async fn door_click_unknown_id_with_an_empty_roster_says_the_roster_is_empty() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/click_door").header("content-type", "application/json")
            .body(Body::from(r#"{"door_id":250}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_ne!(resp.status(), StatusCode::OK, "#891: no invented success on an empty roster");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(command.take_door_click().is_none());
        let body = body_of(resp).await;
        assert!(body.contains("EMPTY"),
            "the empty-roster body must say the roster is empty, not merely 'no such door': {body:?}");
        assert!(body.contains("reached this client yet"),
            "the empty-roster body must disclose that doors not yet arrived look identical to no \
             doors: {body:?}");
        assert!(body.contains("not yet been published"),
            "the empty-roster body must ALSO disclose that a door already arrived but not yet \
             published looks identical to no doors — the #937 shape that still applies on the \
             first zone-in of a session (#1016 review B5): {body:?}");
        assert!(body.contains("NOT sent"), "{body:?}");
    }

    /// THE HONESTY PROOF for the name form: a name that matches nothing is a LOOKUP MISS, and must
    /// not be reported with the malformed-request text that asks for the argument just supplied.
    #[tokio::test]
    async fn door_click_name_miss_is_404_not_the_missing_argument_message() {
        let state = empty_state();
        seed_door(&state, 6, "HHCELL");
        seed_door(&state, 7, "HHDOOR");
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/click_door").header("content-type", "application/json")
            .body(Body::from(r#"{"name":"NO_SUCH_DOOR_XYZ"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND,
            "a name that matched nothing is a lookup miss, not a malformed request — #891");
        assert!(command.take_door_click().is_none());
        let body = body_of(resp).await;
        assert!(!body.contains("provide {\"door_id\""),
            "a supplied name must never be answered by asking for an argument the caller sent: \
             {body:?}");
        assert!(body.contains("NO_SUCH_DOOR_XYZ"),
            "the body must name what missed: {body:?}");
        assert!(body.contains("2 doors"),
            "the body must name the roster size: {body:?}");
    }

    /// The two failure bodies must be DISTINGUISHABLE — a caller has to be able to tell "you sent
    /// the wrong shape" from "there is no such door" without guessing.
    #[tokio::test]
    async fn door_click_no_argument_and_lookup_miss_do_not_share_a_message() {
        let state = empty_state();
        seed_door(&state, 6, "HHCELL");
        let command = state.command.clone();
        let app = router().with_state(state);

        let empty = app.clone().oneshot(Request::post("/click_door")
            .header("content-type", "application/json")
            .body(Body::from(r#"{}"#)).unwrap()).await.unwrap();
        assert_eq!(empty.status(), StatusCode::BAD_REQUEST,
            "an empty body really IS a malformed request — that 400 stays");
        let empty_body = body_of(empty).await;

        let miss = app.oneshot(Request::post("/click_door")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"NO_SUCH_DOOR_XYZ"}"#)).unwrap()).await.unwrap();
        let miss_body = body_of(miss).await;

        assert_ne!(empty_body, miss_body,
            "#891: two distinct failures must not share one message");
        assert!(command.take_door_click().is_none(), "neither failure may queue a click");
    }

    /// The lookups themselves still work — the fix rejects misses, it does not reject everything.
    #[tokio::test]
    async fn door_click_known_id_still_queues() {
        let state = empty_state();
        seed_door(&state, 6, "HHCELL");
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/click_door").header("content-type", "application/json")
            .body(Body::from(r#"{"door_id":6}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_door_click(), Some(6));
    }

    #[tokio::test]
    async fn door_click_known_name_still_resolves_and_queues() {
        let state = empty_state();
        seed_door(&state, 6, "HHCELL");
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/click_door").header("content-type", "application/json")
            .body(Body::from(r#"{"name":"hhcell"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "case-insensitive name matching must survive");
        assert_eq!(command.take_door_click(), Some(6));
    }

    // ── #934 review B3: the RESPONSE TEXT is the product of #891, so it gets its own pins ────────
    //
    // The tests above all pin the ALGORITHM (which status, which id, whether a click was queued).
    // The independent review measured that with those in place the three most assertive sentences
    // in the 404 bodies could be deleted or re-scoped with the whole suite green — the honesty
    // claims had no pin at all (#799's defect class). The two `*_body_is_exactly_this` tests below
    // close that: they assert the WHOLE delivered body, so deletion, rewording, and any wrap that
    // adds or reorders text are all RED. The narrower tests after them exist so that the failure
    // NAMES which review finding a given clause discharges, instead of just showing a diff.

    /// Drive a real miss through the router and return the delivered body — these pins assert what
    /// an agent RECEIVES, not what is written in the source, so a claim that is present but
    /// unreachable cannot satisfy them.
    async fn miss_body(seed: &[(u8, &str)], request: &'static str) -> String {
        let state = empty_state();
        for (id, name) in seed { seed_door(&state, *id, name); }
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/click_door")
            .header("content-type", "application/json")
            .body(Body::from(request)).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        body_of(resp).await
    }

    /// VERBATIM pin, populated roster. Every clause is load-bearing:
    ///   * `the 2 doors this client currently holds` — the count, with NO completeness or
    ///     zone-provenance claim attached to it (#934 review B1/B2).
    ///   * `This click was NOT sent` — the #891 anti-claim; an agent must not read the 404 as
    ///     "queued but unconfirmed".
    ///   * `against the roster as it stands` — the retry guidance, SCOPED. Its predecessor read
    ///     "Do not retry the same body — it can never resolve", which is a universal about a list
    ///     that grows underneath it, applied to a JSON body that is not zone-scoped (B2).
    ///   * `it grows as further door records arrive, and zoning empties it` — says outright that
    ///     populated does not mean final, so the scoped refusal above cannot be over-read.
    ///   * `GET /v1/observe/doors` — the list this handler actually resolved against.
    #[tokio::test]
    async fn door_click_populated_miss_body_is_exactly_this() {
        let body = miss_body(&[(6, "HHCELL"), (7, "HHDOOR")], r#"{"door_id":250}"#).await;
        assert_eq!(body,
            "no door matching id 250 among the 2 doors this client currently holds. This click was \
             NOT sent and will not take effect. No retry of this body can resolve against the \
             roster as it stands — but the roster is not fixed: it grows as further door records \
             arrive, and zoning empties it. Re-list with GET /v1/observe/doors and use a `door_id` \
             or `name` from there.",
            "the populated-roster 404 body is the product of #891 and is pinned verbatim — if you \
             are changing it, change this string too and say in the commit which claim moved and \
             what measures it");
    }

    /// VERBATIM pin, empty roster. Every clause is load-bearing:
    ///   * `That does NOT establish that the door does not exist` — the client genuinely cannot
    ///     tell, and must not answer as if it could.
    ///   * `an empty roster does not distinguish …` — the ambiguity, stated as an ambiguity. Its
    ///     predecessor added "because an empty roster is the only observable for both", which is
    ///     false (#934 review N1: `/observe/packets?op=` records `op_name`, and `/observe/debug`
    ///     carries `zone_assets.state` and `player.pos`) and self-undermining — it denied any other
    ///     observable and then told the caller to wait for a load state it would need one to see.
    ///   * "whose door records may not have reached this client yet, or may have arrived but not
    ///     yet been published into this roster" — #1016 review B5's correction. A previous revision
    ///     narrowed this to "a zone-in that has not yet delivered its first door record" on the
    ///     strength of `run_zone_entry_handshake` now publishing on the same drain pass that applies
    ///     a door record (#1016 review B2/B1). That fix is real, but `run_zone_entry_handshake` is
    ///     only reached on a RE-ZONE (its two production call sites are both inside
    ///     `run_gameplay_phase`) — the first zone-in of a session goes through `login.rs`'s own
    ///     separate state machine, which applies `OP_SpawnDoor` via `apply_packet` but has no
    ///     `InteractSlots` in scope and so cannot publish into this roster at all. The narrowed
    ///     string was therefore MORE confident than the code earns on that path: the "arrived but
    ///     unpublished" cause it had dropped is exactly #937's original shape, still live on every
    ///     session's first zone. This body restores both causes and says outright that the client
    ///     cannot tell them apart, rather than picking a bound that is only true on one path.
    #[tokio::test]
    async fn door_click_empty_roster_body_is_exactly_this() {
        let body = miss_body(&[], r#"{"name":"NO_SUCH_DOOR_XYZ"}"#).await;
        assert_eq!(body,
            "no door matching the name \"NO_SUCH_DOOR_XYZ\": this client's door roster is EMPTY. \
             That does NOT establish that the door does not exist — an empty roster does not \
             distinguish a genuinely doorless zone from a zone-in still in progress, whose door \
             records may not have reached this client yet, or may have arrived but not yet been \
             published into this roster, and this client cannot tell those cases apart. This click \
             was NOT sent and will not take effect. Re-list with GET /v1/observe/doors: if it is \
             still empty once the zone has finished loading, there is no door here to click.",
            "the empty-roster 404 body is the product of #891 and is pinned verbatim — if you are \
             changing it, change this string too and say in the commit which claim moved and what \
             measures it");
    }

    /// #934 review B2: NEITHER body may make an unbounded claim about a list that changes.
    ///
    /// `GameState::upsert_door` inserts into a map that only zone-in clears and `sync_doors`
    /// republishes after every applied packet, so a later `OP_SpawnDoor` grows the roster —
    /// "populated" never implies "complete". A body is also a literal JSON document, not a
    /// zone-scoped one, so "do not retry the same body" outlives the zone it was said in.
    ///
    /// This is the pin that survives a rewrite: whatever the bodies say next, they may not say it
    /// with a "never"/"always"/"cannot" about the roster.
    #[tokio::test]
    async fn door_click_miss_bodies_make_no_unbounded_claim_about_the_roster() {
        for (seed, req) in [
            (&[(6u8, "HHCELL"), (7, "HHDOOR")][..], r#"{"door_id":250}"#),
            (&[][..],                               r#"{"door_id":250}"#),
        ] {
            let body = miss_body(seed, req).await;
            let lower = body.to_lowercase();
            for word in ["never", "always", "impossible"] {
                assert!(!lower.contains(word),
                    "#934 review B2: a door-miss body may not carry the universal {word:?} — the \
                     roster grows as OP_SpawnDoor records arrive and empties when you zone, so no \
                     refusal phrased about it holds for all time: {body:?}");
            }
            assert!(!lower.contains("only observable"),
                "#934 review N1: an empty roster is NOT the only observable bearing on whether \
                 the door records arrived — /observe/packets and /observe/debug also do: {body:?}");
        }
    }

    /// #934 review B2, the positive half: the populated body must DISCLOSE that the roster is not
    /// final, or its scoped refusal ("against the roster as it stands") reads as an absolute one.
    /// Deleting either disclosure, or re-scoping the refusal to the whole body rather than to the
    /// roster's current contents, is RED here as well as in the verbatim pin above.
    #[tokio::test]
    async fn door_click_populated_miss_discloses_that_the_roster_is_not_final() {
        let body = miss_body(&[(6, "HHCELL"), (7, "HHDOOR")], r#"{"door_id":250}"#).await;
        assert!(body.contains("as it stands"),
            "the refusal must be scoped to the roster's CURRENT contents: {body:?}");
        assert!(body.contains("it grows as further door records arrive"),
            "the body must say the roster can still grow — otherwise the caller reads the refusal \
             as permanent, which is what #891's own fix first shipped: {body:?}");
        assert!(body.contains("zoning empties it"),
            "the body must say the roster is zone-scoped even though the JSON body is not: \
             {body:?}");
    }

    /// The count is prose, and prose has to agree with the number. A one-door roster says
    /// "1 door", not "1 doors" — pinned because the plural is built by hand next to the count.
    #[tokio::test]
    async fn door_click_miss_agrees_with_its_own_count() {
        let body = miss_body(&[(6, "HHCELL")], r#"{"door_id":250}"#).await;
        assert!(body.contains("among the 1 door this client currently holds"),
            "a single-entry roster must not be reported as '1 doors': {body:?}");
    }
}
