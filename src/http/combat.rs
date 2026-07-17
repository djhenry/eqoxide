//! `/v1/combat/*` — targeting, auto-attack, consider, and spell scribe/memorize/cast.

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
use crate::command_state::{CastEnd, CommandResult};
use super::*;

/// How long POST /v1/combat/cast AWAITS the cast's true outcome before answering `202` "unknown".
/// This is the SOLE bound on the pure-silence case (the server accepts the cast but never sends a
/// terminal, so `gs.last_cast` never transitions) — mirroring merchant/buy, whose insufficient-funds
/// silence likewise resolves only via the HTTP timeout. Sized to comfortably exceed the longest RoF2
/// cast (~10s for a Complete Heal / gate / port) plus travel and the ~400ms unexplained-end grace, so
/// a NORMAL long cast always resolves via its outcome transition well within the window; only genuine
/// silence rides it out. A resolved/refused/unexplained outcome fires far sooner via `fulfill_cast`.
const CAST_HTTP_TIMEOUT_SECS: u64 = 12;

/// A `text/plain` response (mirrors `http::merchant`'s local helper — combat's cast handler now
/// returns a `Response`, not `(StatusCode, String)`).
fn text(status: StatusCode, body: impl Into<String>) -> Response {
    Response::builder().status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body.into())).unwrap()
}

/// A `application/json` response.
fn json(status: StatusCode, value: serde_json::Value) -> Response {
    Response::builder().status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string())).unwrap()
}

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/target", post(post_target))
        .route("/target/name", post(post_target_name))
        .route("/attack", post(post_attack_on).delete(post_attack_off))
        .route("/consider", post(post_consider))
        .route("/cast", post(post_cast))
        .route("/memorize", post(post_memorize))
        .route("/scribe", post(post_scribe))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetBody {
    id: u32,
}

/// POST /v1/combat/target {"id":<spawn_id>} — target the spawn and auto-consider it. The con
/// result comes back asynchronously as an OP_Consider reply (→ message log).
///
/// 404s on a spawn id that isn't in the zone. It used to adopt ANY id as truth: the nav thread ran
/// `gs.set_target(id)` unconditionally, so `/v1/observe/debug` reported `target_id: <bogus>` with a
/// null name — a well-formed "I have a target whose name I don't know" — while the server, which
/// silently ignores an unknown OP_TargetMouse, left the REAL target untouched. The lie then spread
/// to every endpoint that defaults to "the current target" (/move/goto, /combat/cast,
/// /pet/command attack). `/target/name` already 404'd; the numeric route was the hole. (#348)
async fn post_target(
    State(s): State<HttpState>,
    body: Result<Json<TargetBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let id = match body {
        Ok(Json(b)) => b.id,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"id\":<spawn_id>}".into()),
    };
    // The player's own spawn is a legal target (self-cast / F1) but is deliberately absent from the
    // entity list — `register_spawn` skips the self-spawn (see GameState::set_target).
    let is_self = s.player().player_id == id;
    let known = is_self || s.world.entity_ids.lock().unwrap().values().any(|&v| v == id);
    if !known {
        return (StatusCode::NOT_FOUND, format!("no spawn with id {id} in this zone"));
    }
    s.command.request_target(id);
    tracing::info!("target: queued spawn_id={}", id);
    (StatusCode::OK, format!("targeting spawn {}", id))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetNameBody {
    name: String,
}

/// POST /v1/combat/target/name {"name":"a rat"} — target a mob by (fuzzy) name. The nav thread
/// resolves the name to a spawn_id via gs.entities and sends OP_TargetCommand.
async fn post_target_name(
    State(s): State<HttpState>,
    body: Result<Json<TargetNameBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let name = match body {
        Ok(Json(b)) => b.name,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"name\":\"...\"}".into()),
    };
    let ids = s.world.entity_ids.lock().unwrap();
    let nl = name.to_lowercase();
    let exact = ids.iter()
        .find(|(k, _)| clean_entity_name(k).to_lowercase() == nl)
        .map(|(k, &id)| (k.clone(), id));
    let found = exact.or_else(|| {
        ids.iter()
            .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
            .map(|(k, &id)| (k.clone(), id))
    });
    match found {
        Some((key, id)) => {
            s.command.request_target(id);
            tracing::info!("target_name: {:?} → spawn_id={}", key, id);
            (StatusCode::OK, format!("targeting {} (spawn_id={})", clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no entity matching {:?}", name)),
    }
}

/// POST /v1/combat/attack — enable auto-attack (sends OP_AUTO_ATTACK 1).
async fn post_attack_on(State(s): State<HttpState>) -> (StatusCode, String) {
    s.command.request_attack(true);
    tracing::info!("attack: queued auto-attack ON");
    (StatusCode::OK, "auto-attack ON".into())
}

/// DELETE /v1/combat/attack — disable auto-attack (sends OP_AUTO_ATTACK 0).
async fn post_attack_off(State(s): State<HttpState>) -> (StatusCode, String) {
    s.command.request_attack(false);
    tracing::info!("attack: queued auto-attack OFF");
    (StatusCode::OK, "auto-attack OFF".into())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsiderBody { id: Option<u32> }

/// POST /v1/combat/consider {"id":N?} — consider a spawn (con color/faction), default current target.
async fn post_consider(State(s): State<HttpState>, OptionalJson(body): OptionalJson<ConsiderBody>) -> (StatusCode, String) {
    let id = body.and_then(|b| b.id).or(s.player().target_id);
    match id {
        Some(id) => { s.command.request_consider(id); (StatusCode::OK, format!("consider {id} queued")) }
        None => (StatusCode::BAD_REQUEST, "no target; provide {\"id\":N}".into()),
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CastBody { gem: Option<u8>, spell_id: Option<u32>, target_id: Option<u32>, item_slot: Option<u32> }

/// POST /v1/combat/cast {"gem":0-8} | {"spell_id":N,"target_id":M?} | {"item_slot":S,"target_id":M?}
/// `item_slot` activates an inventory item's click ("clicky") effect — a teleport ring / port
/// potion, etc. — at the given RoF2 wire slot (from GET /v1/observe/inventory). (eqoxide#193)
///
/// A3 Migration 3 (#448) — Command-with-result: this no longer returns a premature "queued" 200. It
/// AWAITS the cast's real outcome (up to `CAST_HTTP_TIMEOUT_SECS`) and reports it honestly:
///   • 200 — the server RESOLVED the cast. Body `{status, spell_id, spell, message}` where `status`
///     is `"completed"` (the spell LANDED), `"fizzled"`, or `"interrupted"`. A fizzle/interrupt is
///     STILL a 200 (the cast has a definite outcome) — the caller MUST read `status`, never assume a
///     200 means the spell took hold. That is the honesty invariant: a non-completed cast can never
///     falsely present as completed.
///   • 409 — the cast was REFUSED and definitively did not happen: a pre-send rejection (empty gem /
///     no clicky / another cast already in flight), or a real server refusal (no mana / no target /
///     recast timer). Body `{status:"refused", reason}`.
///   • 202 — the outcome is UNKNOWN: the server ended the cast without explaining it, or never sent a
///     terminal within the timeout, or a zone change / disconnect intervened. Body says so; MUST NOT
///     be read as success (see `crate::command_state::result`).
async fn post_cast(State(s): State<HttpState>, OptionalJson(body): OptionalJson<CastBody>) -> Response {
    let b = body.unwrap_or_default();
    // Resolve the CastRequest with the same pre-send validation as before (a clear 4xx before we ever
    // park/await). Item clicky cast: validate the slot holds a clickable item.
    let req = if let Some(slot) = b.item_slot {
        let clicky = s.inventory_slots.inventory.lock().unwrap().iter()
            .find(|i| i.slot == slot as i32)
            .map(|i| (i.name.clone(), i.click_spell_id));
        match clicky {
            None => return text(StatusCode::BAD_REQUEST, format!("no item at slot {slot}")),
            Some((name, 0)) => return text(StatusCode::BAD_REQUEST, format!("'{name}' (slot {slot}) has no clicky effect")),
            Some((_name, _spell)) => CastRequest { gem: 0, target_id: b.target_id, item_slot: Some(slot) },
        }
    } else {
        let mem = s.player().mem_spells;
        let gem = if let Some(g) = b.gem {
            g
        } else if let Some(sid) = b.spell_id {
            match mem.iter().position(|&x| x == sid) {
                Some(i) => i as u8,
                None => return text(StatusCode::BAD_REQUEST, format!("spell {sid} is not memorized")),
            }
        } else {
            return text(StatusCode::BAD_REQUEST, "provide {\"gem\":0-8} or {\"spell_id\":N}");
        };
        if gem > 8 { return text(StatusCode::BAD_REQUEST, "gem must be 0-8"); }
        // An EMPTY gem is not a cast — refuse it loudly BEFORE parking, so we never await a cast that
        // cannot happen. 409 (like /v1/interact/read for an unreadable slot). (#348)
        if crate::game_state::gem_is_empty(mem[gem as usize]) {
            return text(StatusCode::CONFLICT,
                format!("spell gem {gem} is empty — memorize a spell into it first"));
        }
        CastRequest { gem, target_id: b.target_id, item_slot: None }
    };

    // Park the cast with a result channel and await the TRUE outcome (park → fulfil → timeout).
    let (tx, rx) = oneshot::channel::<CommandResult<CastEnd>>();
    s.command.request_cast_await(req, tx);
    tracing::info!("cast: awaited cast queued (gem={} item_slot={:?})", req.gem, req.item_slot);

    match tokio::time::timeout(Duration::from_secs(CAST_HTTP_TIMEOUT_SECS), rx).await {
        // A RESOLVED cast — the server gave a definite verdict. `outcome` carries whether the spell
        // landed; a fizzle/interrupt is a 200 with a NON-"completed" status, never a false success.
        Ok(Ok(CommandResult::Resolved(CastEnd { outcome, spell_id, spell_name, text: line }))) => json(
            StatusCode::OK,
            serde_json::json!({
                "status": outcome,
                // Top-level unambiguous "did the spell land?" so a consumer that only checks the HTTP
                // status can't mistake a fizzle/interrupt 200 for a successful land — `landed` is true
                // ONLY for a completed cast (#448 review, Hunt 2).
                "landed": outcome == "completed",
                "spell_id": spell_id,
                "spell": spell_name,
                "message": line,
            }),
        ),
        // A pre-send rejection (empty gem / no clicky / another cast in flight) or a real server
        // refusal (no mana / no target / recast) — the cast definitively did not happen.
        Ok(Ok(CommandResult::Refused(reason))) => json(
            StatusCode::CONFLICT,
            serde_json::json!({ "status": "refused", "landed": false, "reason": reason }),
        ),
        // Unconfirmed, channel closed (Sender dropped — disconnect / zone change), or elapsed: the
        // outcome is genuinely UNKNOWN. The server-ended-but-unexplained case (a buff that won't stack)
        // and pure silence both land here. MUST NOT read as success — 202 with an explicit body.
        _ => json(
            StatusCode::ACCEPTED,
            serde_json::json!({
                "status": "unconfirmed",
                "landed": false,
                "message": "cast sent, but the outcome is UNKNOWN — the server either ended the cast \
                            without explaining it (e.g. a beneficial spell that would not stack) or \
                            sent no confirmation within the timeout, or a zone change intervened. \
                            Re-check GET /v1/observe/debug (last_cast) and your target's state before \
                            assuming the spell took effect.",
            }),
        ),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MemorizeBody { spell_id: u32, gem: u32 }

/// POST /v1/combat/memorize {"spell_id":N,"gem":0-8} — memorize a known (scribed) spell into a gem.
/// Sends OP_MemorizeSpell with scribing=1.
async fn post_memorize(
    State(s): State<HttpState>,
    body: Result<Json<MemorizeBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body { Ok(Json(b)) => b, Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"spell_id\":N,\"gem\":0-8}".into()) };
    if b.gem > 8 { return (StatusCode::BAD_REQUEST, "gem must be 0-8".into()); }
    s.command.request_mem_spell(b.gem, b.spell_id, 1, None);
    (StatusCode::OK, format!("memorizing spell {} into gem {}", b.spell_id, b.gem))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ScribeBody { spell_id: u32, slot: Option<u32>, from: Option<u32> }

/// POST /v1/combat/scribe {"spell_id":N,"from":S,"slot":B?} — scribe a spell scroll into the
/// spellbook at book slot B (default 0). `from` is the scroll's current inventory wire slot (from
/// GET /v1/observe/inventory): the RoF2 server scribes only the scroll on the CURSOR, so the nav
/// thread moves `from` → cursor (OP_MoveItem) before sending OP_MemorizeSpell scribing=0, which
/// consumes the scroll. Omit `from` only if the scroll is already on the cursor. See eqoxide#11.
async fn post_scribe(
    State(s): State<HttpState>,
    body: Result<Json<ScribeBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body { Ok(Json(b)) => b, Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"spell_id\":N,\"from\":S,\"slot\":B?}".into()) };
    let slot = b.slot.unwrap_or(0);
    s.command.request_mem_spell(slot, b.spell_id, 0, b.from);
    (StatusCode::OK, match b.from {
        Some(f) => format!("scribing spell {} into book slot {} (scroll from slot {})", b.spell_id, slot, f),
        None    => format!("scribing spell {} into book slot {} (scroll assumed on cursor)", b.spell_id, slot),
    })
}

#[cfg(test)]
mod tests {
    use super::router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use crate::http::quests::tests::{empty_state, set_gs};

    async fn body_text(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    // --- consider: a malformed body must not silently fall back to "current target" ------------

    #[tokio::test]
    async fn consider_no_body_falls_back_to_current_target() {
        let state = empty_state();
        set_gs(&state, |gs| gs.target_id = Some(7));
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/consider").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_consider(), Some(7));
    }

    #[tokio::test]
    async fn consider_malformed_id_is_400_and_does_not_fall_back() {
        let state = empty_state();
        // A current target IS set — the old Option<Json<T>> bug would silently consider IT instead
        // of reporting the malformed "id" field.
        set_gs(&state, |gs| gs.target_id = Some(7));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/consider")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":"not-a-number"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(command.take_consider().is_none(),
            "a malformed id must not silently fall through to considering the current target");
    }

    /// eqoxide#341: a typo'd key ("idd" instead of "id") must 400 — not be silently ignored by serde
    /// (leaving `id` at its default `None`) and fall through to considering the current target.
    #[tokio::test]
    async fn consider_unknown_key_is_400_and_does_not_fall_back() {
        let state = empty_state();
        set_gs(&state, |gs| gs.target_id = Some(7));
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/consider")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"idd":7}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(command.take_consider().is_none(),
            "a typo'd key must not silently fall through to considering the current target");
    }

    // --- cast: preserve the "no gem/spell_id" 400, but a malformed body must say so honestly ----

    #[tokio::test]
    async fn cast_no_body_is_400_with_provide_message() {
        let state = empty_state();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/cast").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let text = body_text(resp).await;
        assert!(text.contains("provide"), "message: {text}");
    }

    // ── #348: /combat/target must not adopt a spawn id the zone doesn't have ────────────────────

    #[tokio::test]
    async fn target_unknown_spawn_id_is_404_and_queues_nothing() {
        // The bug: ANY id was accepted, the nav thread ran gs.set_target(id), and /observe/debug
        // then reported `target_id: 999999` with a null name while the server (which silently
        // ignores an unknown OP_TargetMouse) kept the REAL target. The bogus id then propagated
        // into /move/goto, /combat/cast and /pet/command, which all default to "the target".
        let state = empty_state();
        state.world.entity_ids.lock().unwrap().insert("a rat000".into(), 7);
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/target")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":999999}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND,
            "an id that is not in the zone must be an explicit failure, not an adopted truth");
        assert!(command.take_target().is_none(), "nothing may be queued for a bogus id");
    }

    #[tokio::test]
    async fn target_known_spawn_id_still_works() {
        let state = empty_state();
        state.world.entity_ids.lock().unwrap().insert("a rat000".into(), 7);
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/target")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":7}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_target(), Some(7));
    }

    #[tokio::test]
    async fn target_own_spawn_id_is_allowed() {
        // The player's own spawn is a legal target (self-cast / F1) but is deliberately absent from
        // the entity list, so it must be allowed explicitly or self-targeting would 404.
        let state = empty_state();
        set_gs(&state, |gs| gs.player_id = 42);
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/target")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":42}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_target(), Some(42));
    }

    // ── #348: /combat/cast on an EMPTY gem must fail loudly, not 200-then-silence ────────────────

    #[tokio::test]
    async fn cast_empty_gem_is_409_and_queues_nothing() {
        // The bug: the gem path skipped every check, returned 200 "cast queued", and the nav drain
        // then dropped it with a `tracing::info!` the agent cannot read. 200 + total silence is
        // indistinguishable from a cast that is still in flight.
        let state = empty_state();
        set_gs(&state, |gs| {
            gs.mem_spells = [crate::game_state::EMPTY_GEM; 9];
            gs.mem_spells[0] = 202; // only gem 0 is memorized
        });
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/cast")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"gem":7}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let text = body_text(resp).await;
        assert!(text.contains("empty"), "message must name the real cause: {text}");
        assert!(command.take_cast().is_none(), "an empty gem must not be queued as a cast");
    }

    // ── A3 Migration 3 (#448): POST /v1/combat/cast reports the TRUE outcome, not a queued 200 ──

    use crate::command_state::{CastEnd, CommandResult};

    /// Drive a `/cast` request, wait for the handler to park its awaited Sender, then deliver
    /// `outcome` on it and return the finished HTTP response. Mirrors the merchant/buy test harness.
    async fn cast_and_deliver(gem: u8, outcome: CommandResult<CastEnd>) -> axum::response::Response {
        let state = empty_state();
        set_gs(&state, |gs| {
            gs.mem_spells = [crate::game_state::EMPTY_GEM; 9];
            gs.mem_spells[gem as usize] = 202;
        });
        let command = state.command.clone();
        let app = router().with_state(state);
        let body = format!("{{\"gem\":{gem}}}");
        let task = tokio::spawn(async move {
            app.oneshot(Request::post("/cast").header("content-type", "application/json")
                .body(Body::from(body)).unwrap()).await.unwrap()
        });
        // The awaited cast must NOT leak into the fire-and-forget slot; it parks `cast_await`.
        let (req, tx) = loop {
            assert!(command.take_cast().is_none(), "an awaited cast must not queue the UI fire-and-forget slot");
            if let Some(p) = command.take_cast_await() { break p; }
            tokio::task::yield_now().await;
        };
        assert_eq!(req.gem, gem);
        tx.send(outcome).unwrap();
        task.await.unwrap()
    }

    /// SUCCESS: a cast the server RESOLVED as completed → 200 with `status:"completed"` and the spell.
    #[tokio::test]
    async fn cast_completed_is_200_with_completed_status() {
        let resp = cast_and_deliver(2, CommandResult::Resolved(CastEnd {
            outcome: "completed".into(), spell_id: 202, spell_name: "Minor Healing".into(),
            text: "You have finished casting Minor Healing.".into(),
        })).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let text = body_text(resp).await;
        assert!(text.contains("\"status\":\"completed\""), "body: {text}");
        assert!(text.contains("\"landed\":true"), "a completed cast must report landed:true: {text}");
        assert!(text.contains("\"spell_id\":202"), "body: {text}");
    }

    /// THE INVARIANT: a fizzle is a 200 (the cast RESOLVED — we know what happened) but its `status`
    /// is "fizzled", NEVER "completed". A caller that reads `status` can never be told the spell
    /// landed when it fizzled — a non-completed cast cannot falsely present as completed.
    #[tokio::test]
    async fn cast_fizzle_is_200_but_status_is_fizzled_not_completed() {
        let resp = cast_and_deliver(2, CommandResult::Resolved(CastEnd {
            outcome: "fizzled".into(), spell_id: 202, spell_name: "Minor Healing".into(),
            text: "Your spell fizzles!".into(),
        })).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let text = body_text(resp).await;
        assert!(text.contains("\"status\":\"fizzled\""), "a fizzle must report fizzled: {text}");
        assert!(text.contains("\"landed\":false"),
            "a fizzle must report landed:false so a status-code-only consumer can't read it as a land: {text}");
        assert!(!text.contains("\"status\":\"completed\""),
            "a fizzle must NEVER present as completed — the whole honesty invariant: {text}");
    }

    /// An interrupt likewise: 200, but `status:"interrupted"`, never "completed".
    #[tokio::test]
    async fn cast_interrupt_is_200_but_status_is_interrupted_not_completed() {
        let resp = cast_and_deliver(2, CommandResult::Resolved(CastEnd {
            outcome: "interrupted".into(), spell_id: 202, spell_name: "Minor Healing".into(),
            text: "Your spell is interrupted.".into(),
        })).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let text = body_text(resp).await;
        assert!(text.contains("\"status\":\"interrupted\""), "body: {text}");
        assert!(!text.contains("\"status\":\"completed\""), "an interrupt must not present as completed: {text}");
    }

    /// A real refusal (no mana / no target / another cast in flight) → 409, never a success.
    #[tokio::test]
    async fn cast_refused_is_409() {
        let resp = cast_and_deliver(2, CommandResult::Refused("Insufficient Mana to cast this spell!".into())).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let text = body_text(resp).await;
        assert!(text.contains("\"status\":\"refused\""), "body: {text}");
        assert!(!text.contains("completed"), "a refusal must never read as a completed cast: {text}");
    }

    /// SILENCE / unexplained end → 202 "unconfirmed". MUST NOT be a 200. This is the honesty invariant
    /// on the unknown side: a cast whose outcome we don't know can never render as success.
    #[tokio::test]
    async fn cast_unconfirmed_is_202_never_200() {
        let resp = cast_and_deliver(2, CommandResult::Unconfirmed).await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let text = body_text(resp).await;
        assert!(text.contains("\"status\":\"unconfirmed\""), "body: {text}");
        assert!(!text.contains("completed"), "an unknown outcome must never read as completed: {text}");
    }

    #[tokio::test]
    async fn cast_malformed_gem_is_400_with_malformed_message() {
        let state = empty_state();
        let app = router().with_state(state);
        let req = Request::post("/cast")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"gem":"not-a-number"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let text = body_text(resp).await;
        assert!(text.contains("malformed JSON body"),
            "message should name the real cause, not the unrelated \"provide gem/spell_id\" default-validation text: {text}");
    }
}
