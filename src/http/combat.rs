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
struct TargetBody {
    id: u32,
}

/// POST /v1/combat/target {"id":<spawn_id>} — target the spawn and auto-consider it. The con
/// result comes back asynchronously as an OP_Consider reply (→ message log).
async fn post_target(
    State(s): State<HttpState>,
    body: Result<Json<TargetBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let id = match body {
        Ok(Json(b)) => b.id,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"id\":<spawn_id>}".into()),
    };
    *s.target.lock().unwrap() = Some(id);
    tracing::info!("target: queued spawn_id={}", id);
    (StatusCode::OK, format!("targeting spawn {}", id))
}

#[derive(serde::Deserialize)]
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
    let ids = s.entity_ids.lock().unwrap();
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
            *s.target.lock().unwrap() = Some(id);
            tracing::info!("target_name: {:?} → spawn_id={}", key, id);
            (StatusCode::OK, format!("targeting {} (spawn_id={})", clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no entity matching {:?}", name)),
    }
}

/// POST /v1/combat/attack — enable auto-attack (sends OP_AUTO_ATTACK 1).
async fn post_attack_on(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.attack.lock().unwrap() = Some(true);
    tracing::info!("attack: queued auto-attack ON");
    (StatusCode::OK, "auto-attack ON".into())
}

/// DELETE /v1/combat/attack — disable auto-attack (sends OP_AUTO_ATTACK 0).
async fn post_attack_off(State(s): State<HttpState>) -> (StatusCode, String) {
    *s.attack.lock().unwrap() = Some(false);
    tracing::info!("attack: queued auto-attack OFF");
    (StatusCode::OK, "auto-attack OFF".into())
}

#[derive(serde::Deserialize)]
struct ConsiderBody { id: Option<u32> }

/// POST /v1/combat/consider {"id":N?} — consider a spawn (con color/faction), default current target.
async fn post_consider(State(s): State<HttpState>, body: Option<Json<ConsiderBody>>) -> (StatusCode, String) {
    let id = body.and_then(|Json(b)| b.id).or(s.player_info.lock().unwrap().target_id);
    match id {
        Some(id) => { *s.consider.lock().unwrap() = Some(id); (StatusCode::OK, format!("consider {id} queued")) }
        None => (StatusCode::BAD_REQUEST, "no target; provide {\"id\":N}".into()),
    }
}

#[derive(serde::Deserialize)]
struct CastBody { gem: Option<u8>, spell_id: Option<u32>, target_id: Option<u32> }

/// POST /v1/combat/cast {"gem":0-8} | {"spell_id":N,"target_id":M?}
async fn post_cast(State(s): State<HttpState>, body: Option<Json<CastBody>>) -> (StatusCode, String) {
    let b = body.map(|Json(b)| b).unwrap_or(CastBody { gem: None, spell_id: None, target_id: None });
    let mem = s.player_info.lock().unwrap().mem_spells;
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
    *s.cast.lock().unwrap() = Some(CastRequest { gem, target_id: b.target_id });
    (StatusCode::OK, format!("cast queued (gem {gem})"))
}

#[derive(serde::Deserialize)]
struct MemorizeBody { spell_id: u32, gem: u32 }

/// POST /v1/combat/memorize {"spell_id":N,"gem":0-8} — memorize a known (scribed) spell into a gem.
/// Sends OP_MemorizeSpell with scribing=1.
async fn post_memorize(
    State(s): State<HttpState>,
    body: Result<Json<MemorizeBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body { Ok(Json(b)) => b, Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"spell_id\":N,\"gem\":0-8}".into()) };
    if b.gem > 8 { return (StatusCode::BAD_REQUEST, "gem must be 0-8".into()); }
    *s.mem_spell.lock().unwrap() = Some((b.gem, b.spell_id, 1));
    (StatusCode::OK, format!("memorizing spell {} into gem {}", b.spell_id, b.gem))
}

#[derive(serde::Deserialize)]
struct ScribeBody { spell_id: u32, slot: Option<u32> }

/// POST /v1/combat/scribe {"spell_id":N,"slot":B?} — scribe a spell scroll (in inventory) into the
/// spellbook at book slot B (default 0). Sends OP_MemorizeSpell with scribing=0. The server
/// validates you hold the scroll and consumes it.
async fn post_scribe(
    State(s): State<HttpState>,
    body: Result<Json<ScribeBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body { Ok(Json(b)) => b, Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"spell_id\":N,\"slot\":B?}".into()) };
    let slot = b.slot.unwrap_or(0);
    *s.mem_spell.lock().unwrap() = Some((slot, b.spell_id, 0));
    (StatusCode::OK, format!("scribing spell {} into book slot {}", b.spell_id, slot))
}
