//! `/v1/navigate/*` — movement: walk to a target/coords, teleport, cross a zone line.

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/goto", post(post_goto))
        .route("/warp", post(post_warp))
        .route("/zone_cross", post(post_zone_cross))
}

#[derive(serde::Deserialize)]
struct GotoBody {
    name:     Option<String>,
    /// Map coordinates (map_x = server_y, map_y = server_x). Use these when
    /// navigating from a Qeynos map. Alternatively supply raw server x/y/z.
    map_x:    Option<f32>,
    map_y:    Option<f32>,
    /// Raw server coordinates (bypass map conversion)
    x:        Option<f32>,
    y:        Option<f32>,
    z:        Option<f32>,
}

/// POST /v1/navigate/goto  {"name":"Lanhern Firepride"}  or  {"x":1.0,"y":2.0,"z":3.0}
async fn post_goto(
    State(s): State<HttpState>,
    body: Result<Json<GotoBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid JSON".into()),
    };

    let target = if let Some(name) = &b.name {
        let positions = s.entity_positions.lock().unwrap();
        let nl = name.to_lowercase();
        // Exact key match first, then clean-name match, then substring fallback.
        let matched = positions.get(name.as_str()).map(|&p| p)
            .or_else(|| positions.iter()
                .find(|(k, _)| clean_entity_name(k).to_lowercase() == nl)
                .map(|(_, &p)| p))
            .or_else(|| positions.iter()
                .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl)
                    || k.to_lowercase().contains(&nl))
                .map(|(_, &p)| p));
        match matched {
            Some(pos) => pos,
            None => {
                let known: Vec<_> = positions.keys()
                    .filter(|k| k.to_lowercase().contains(&nl))
                    .take(5)
                    .cloned()
                    .collect();
                if known.is_empty() {
                    return (StatusCode::NOT_FOUND, format!("No entity named {:?}", name));
                }
                match positions.get(&known[0]).copied() {
                    Some(pos) => {
                        tracing::info!("goto: fuzzy match {:?} → {:?}", name, known[0]);
                        pos
                    }
                    None => return (StatusCode::NOT_FOUND, format!("No entity named {:?}", name)),
                }
            }
        }
    } else if let (Some(mx), Some(my)) = (b.map_x, b.map_y) {
        // Map coords match the eqmaps/Brewall .txt values, which are the negated
        // server position: map_x = -server_x, map_y = -server_y.
        let server_x = -mx;
        let server_y = -my;
        let server_z = b.z.unwrap_or(3.75);
        tracing::info!("goto: map ({:.1},{:.1}) → server ({:.1},{:.1})", mx, my, server_x, server_y);
        (server_x, server_y, server_z)
    } else if let (Some(x), Some(y), Some(z)) = (b.x, b.y, b.z) {
        (x, y, z)
    } else {
        return (StatusCode::BAD_REQUEST, "provide 'name', 'map_x'+'map_y', or 'x'+'y'+'z'".into());
    };

    *s.goto_target.lock().unwrap() = Some(target);
    tracing::info!("goto: target set to ({:.1},{:.1},{:.1})", target.0, target.1, target.2);
    (StatusCode::OK, format!("navigating to ({:.1},{:.1},{:.1})", target.0, target.1, target.2))
}

#[derive(serde::Deserialize)]
struct WarpBody {
    x: f32,
    y: f32,
    z: f32,
}

/// POST /v1/navigate/warp — teleport directly to coordinates, bypassing collision.
async fn post_warp(
    State(s): State<HttpState>,
    Json(body): Json<WarpBody>,
) -> (StatusCode, String) {
    *s.warp.lock().unwrap() = Some((body.x, body.y, body.z));
    tracing::info!("warp: queued to ({:.1}, {:.1}, {:.1})", body.x, body.y, body.z);
    (StatusCode::OK, format!("warp queued to ({:.1}, {:.1}, {:.1})", body.x, body.y, body.z))
}

#[derive(serde::Deserialize, Default)]
struct ZoneCrossBody {
    /// Destination zone id to cross to. Omit (or 0) to take the nearest zone line.
    zone_id: Option<u16>,
}

/// Sorted, de-duplicated set of zone_ids reachable via a zone line from the current zone.
fn reachable_zone_ids(zps: &[crate::game_state::ZonePoint]) -> Vec<u16> {
    let mut ids: Vec<u16> = zps.iter().map(|zp| zp.zone_id).filter(|&z| z != 0).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// POST /v1/navigate/zone_cross — warp to a zone line and send OP_ZONE_CHANGE.
/// Body: {"zone_id": 1} to cross to a specific zone, or {} for the nearest line.
///
/// A specific `zone_id` that has no zone line from the current zone is REJECTED with 400 (and the
/// list of reachable zone_ids) instead of silently doing nothing / crossing a nearby line — so the
/// caller knows the destination wasn't honored (eqoxide#47).
async fn post_zone_cross(
    State(s): State<HttpState>,
    body: Option<Json<ZoneCrossBody>>,
) -> (StatusCode, String) {
    let zone_id = body.and_then(|Json(b)| b.zone_id).unwrap_or(0);
    if zone_id != 0 {
        let reachable = reachable_zone_ids(&s.zone_points.lock().unwrap());
        if !reachable.contains(&zone_id) {
            let msg = if reachable.is_empty() {
                format!("zone_id {zone_id} is not reachable: no zone lines are known for the current \
                         zone yet (still loading, or this zone has none)")
            } else {
                format!("zone_id {zone_id} is not reachable from the current zone; reachable zone_ids: {reachable:?}")
            };
            tracing::info!("zone_cross: rejected unreachable zone_id={zone_id} (reachable={reachable:?})");
            return (StatusCode::BAD_REQUEST, msg);
        }
    }
    *s.zone_cross.lock().unwrap() = Some(zone_id);
    tracing::info!("zone_cross: flagged for OP_ZONE_CHANGE (target zone_id={zone_id})");
    (StatusCode::OK, format!("zone_cross request queued (zone_id={zone_id})"))
}

#[cfg(test)]
mod zone_cross_tests {
    use super::reachable_zone_ids;
    use crate::game_state::ZonePoint;
    fn zp(zone_id: u16) -> ZonePoint {
        ZonePoint { iterator: 0, server_x: 0.0, server_y: 0.0, server_z: 0.0, heading: 0.0, zone_id }
    }
    #[test]
    fn reachable_ids_are_sorted_deduped_and_drop_zero() {
        // Two lines to zone 9, one to zone 1, and a 0 (nearest-line sentinel) that must be excluded.
        let zps = vec![zp(9), zp(1), zp(9), zp(0)];
        let r = reachable_zone_ids(&zps);
        assert_eq!(r, vec![1, 9], "sorted, de-duplicated, no 0: {r:?}");
        assert!(!r.contains(&24), "an unconnected zone (24) is not reachable");
        assert!(reachable_zone_ids(&[]).is_empty(), "no zone points → nothing reachable");
    }
}
