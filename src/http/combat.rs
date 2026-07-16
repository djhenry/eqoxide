//! `/v1/combat/*` — targeting, auto-attack, consider, and spell scribe/memorize/cast.

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use super::*;

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
    *s.combat.target.lock().unwrap() = Some(id);
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
            *s.combat.target.lock().unwrap() = Some(id);
            tracing::info!("target_name: {:?} → spawn_id={}", key, id);
            (StatusCode::OK, format!("targeting {} (spawn_id={})", clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no entity matching {:?}", name)),
    }
}

/// POST /v1/combat/attack — enable auto-attack (sends OP_AUTO_ATTACK 1).
async fn post_attack_on(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.combat.attack.lock().unwrap() = Some(true);
    tracing::info!("attack: queued auto-attack ON");
    (StatusCode::OK, "auto-attack ON".into())
}

/// DELETE /v1/combat/attack — disable auto-attack (sends OP_AUTO_ATTACK 0).
async fn post_attack_off(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.combat.attack.lock().unwrap() = Some(false);
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
        Some(id) => { *s.combat.consider.lock().unwrap() = Some(id); (StatusCode::OK, format!("consider {id} queued")) }
        None => (StatusCode::BAD_REQUEST, "no target; provide {\"id\":N}".into()),
    }
}

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct CastBody { gem: Option<u8>, spell_id: Option<u32>, target_id: Option<u32>, item_slot: Option<u32> }

/// POST /v1/combat/cast {"gem":0-8} | {"spell_id":N,"target_id":M?} | {"item_slot":S,"target_id":M?}
/// `item_slot` activates an inventory item's click ("clicky") effect — a teleport ring / port
/// potion, etc. — at the given RoF2 wire slot (from GET /v1/observe/inventory). (eqoxide#193)
async fn post_cast(State(s): State<HttpState>, OptionalJson(body): OptionalJson<CastBody>) -> (StatusCode, String) {
    let b = body.unwrap_or_default();
    // Item clicky cast: validate the slot holds a clickable item (for a clear error), then queue it.
    if let Some(slot) = b.item_slot {
        let clicky = s.inventory_slots.inventory.lock().unwrap().iter()
            .find(|i| i.slot == slot as i32)
            .map(|i| (i.name.clone(), i.click_spell_id));
        match clicky {
            None => return (StatusCode::BAD_REQUEST, format!("no item at slot {slot}")),
            Some((name, 0)) => return (StatusCode::BAD_REQUEST, format!("'{name}' (slot {slot}) has no clicky effect")),
            Some((name, spell)) => {
                *s.combat.cast.lock().unwrap() = Some(CastRequest { gem: 0, target_id: b.target_id, item_slot: Some(slot) });
                return (StatusCode::OK, format!("item cast queued: '{name}' (slot {slot}, spell {spell})"));
            }
        }
    }
    let mem = s.player().mem_spells;
    let gem = if let Some(g) = b.gem {
        g
    } else if let Some(sid) = b.spell_id {
        match mem.iter().position(|&x| x == sid) {
            Some(i) => i as u8,
            None => return (StatusCode::BAD_REQUEST, format!("spell {sid} is not memorized")),
        }
    } else {
        return (StatusCode::BAD_REQUEST, "provide {\"gem\":0-8} or {\"spell_id\":N}".into());
    };
    if gem > 8 { return (StatusCode::BAD_REQUEST, "gem must be 0-8".into()); }
    // An EMPTY gem is not a cast. The `spell_id` path above already refuses an un-memorized spell,
    // but an explicit `gem` used to skip every check: this returned 200 "cast queued" and the nav
    // drain then dropped it with a `tracing::info!` the agent cannot read — 200-then-absolute-
    // silence, indistinguishable from a cast that is simply still in flight. 409 (like
    // /v1/interact/read does for an unreadable slot) says so out loud. (#348)
    if crate::game_state::gem_is_empty(mem[gem as usize]) {
        return (StatusCode::CONFLICT,
                format!("spell gem {gem} is empty — memorize a spell into it first"));
    }
    *s.combat.cast.lock().unwrap() = Some(CastRequest { gem, target_id: b.target_id, item_slot: None });
    (StatusCode::OK, format!("cast queued (gem {gem})"))
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
    *s.combat.mem_spell.lock().unwrap() = Some((b.gem, b.spell_id, 1, None));
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
    *s.combat.mem_spell.lock().unwrap() = Some((slot, b.spell_id, 0, b.from));
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
        let consider = state.combat.consider.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/consider").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*consider.lock().unwrap(), Some(7));
    }

    #[tokio::test]
    async fn consider_malformed_id_is_400_and_does_not_fall_back() {
        let state = empty_state();
        // A current target IS set — the old Option<Json<T>> bug would silently consider IT instead
        // of reporting the malformed "id" field.
        set_gs(&state, |gs| gs.target_id = Some(7));
        let consider = state.combat.consider.clone();
        let app = router().with_state(state);
        let req = Request::post("/consider")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":"not-a-number"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(consider.lock().unwrap().is_none(),
            "a malformed id must not silently fall through to considering the current target");
    }

    /// eqoxide#341: a typo'd key ("idd" instead of "id") must 400 — not be silently ignored by serde
    /// (leaving `id` at its default `None`) and fall through to considering the current target.
    #[tokio::test]
    async fn consider_unknown_key_is_400_and_does_not_fall_back() {
        let state = empty_state();
        set_gs(&state, |gs| gs.target_id = Some(7));
        let consider = state.combat.consider.clone();
        let app = router().with_state(state);
        let req = Request::post("/consider")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"idd":7}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(consider.lock().unwrap().is_none(),
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
        let target = state.combat.target.clone();
        let app = router().with_state(state);
        let req = Request::post("/target")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":999999}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND,
            "an id that is not in the zone must be an explicit failure, not an adopted truth");
        assert!(target.lock().unwrap().is_none(), "nothing may be queued for a bogus id");
    }

    #[tokio::test]
    async fn target_known_spawn_id_still_works() {
        let state = empty_state();
        state.world.entity_ids.lock().unwrap().insert("a rat000".into(), 7);
        let target = state.combat.target.clone();
        let app = router().with_state(state);
        let req = Request::post("/target")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":7}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*target.lock().unwrap(), Some(7));
    }

    #[tokio::test]
    async fn target_own_spawn_id_is_allowed() {
        // The player's own spawn is a legal target (self-cast / F1) but is deliberately absent from
        // the entity list, so it must be allowed explicitly or self-targeting would 404.
        let state = empty_state();
        set_gs(&state, |gs| gs.player_id = 42);
        let target = state.combat.target.clone();
        let app = router().with_state(state);
        let req = Request::post("/target")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"id":42}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*target.lock().unwrap(), Some(42));
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
        let cast = state.combat.cast.clone();
        let app = router().with_state(state);
        let req = Request::post("/cast")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"gem":7}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let text = body_text(resp).await;
        assert!(text.contains("empty"), "message must name the real cause: {text}");
        assert!(cast.lock().unwrap().is_none(), "an empty gem must not be queued as a cast");
    }

    #[tokio::test]
    async fn cast_memorized_gem_still_works() {
        let state = empty_state();
        set_gs(&state, |gs| {
            gs.mem_spells = [crate::game_state::EMPTY_GEM; 9];
            gs.mem_spells[2] = 202;
        });
        let cast = state.combat.cast.clone();
        let app = router().with_state(state);
        let req = Request::post("/cast")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"gem":2}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(cast.lock().unwrap().as_ref().map(|c| c.gem), Some(2));
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
