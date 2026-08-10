//! `/v1/observe/*` — read-only world/player state for the agent.

use axum::{
    body::Body,
    extract::{Query, RawQuery, State},
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::collections::{HashMap, HashSet};
use tokio::sync::oneshot;
use super::*;

/// The `zone_assets` object served on `/v1/observe/debug` (#579) — see the call site for why it
/// exists.
///
/// `state` is derived from [`eqoxide_nav::zone_assets::usability`], NOT from the raw state tag, so
/// **`ready` cannot appear unless the loaded assets belong to the zone the character is actually
/// standing in.** That distinction is the #595-review F1 defect: `player.zone` is published by the
/// network thread the instant `OP_NewZone` lands, while the render thread only starts the new
/// zone's load on its next frame — a ~66 ms window (measured live) in which the previous zone's
/// assets are fully `Ready`. Gating on the state alone made the client vouch for a confident answer
/// about the WRONG world (a 200 exit list and a 2 MB frame of the zone just left).
pub(crate) fn zone_assets_json(s: &HttpState) -> serde_json::Value {
    let st = eqoxide_nav::zone_assets::lock_state(&s.zone_assets).clone();
    let player_zone = s.player().zone;
    zone_assets_json_of(&st, &player_zone)
}

/// The pure projection behind [`zone_assets_json`] — takes the two inputs explicitly so the
/// zone-identity rule can be property-tested over every combination.
pub(crate) fn zone_assets_json_of(
    st: &eqoxide_nav::zone_assets::ZoneAssetState,
    player_zone: &str,
) -> serde_json::Value {
    use eqoxide_nav::zone_assets::{usability, ZoneAssetState};
    let verdict = usability(st, player_zone);
    serde_json::json!({
        // "idle" | "pending" | "ready" | "failed" | "stale" | "unknown_zone".
        "state":  verdict.map(|v| v.state_word()).unwrap_or("ready"),
        // The machine-readable WHY behind any non-`ready` state; null when ready.
        "reason": verdict.map(|v| v.as_str()),
        // The zone the loaded/loading assets are FOR …
        "zone":   st.zone(),
        // … and the zone the client believes the character is in. They differ only in the transient
        // `stale` window above; when they do, nothing about the world may be read from this client.
        "player_zone": (!player_zone.is_empty()).then_some(player_zone),
        "status": st.status(),
        "terrain_meshes": match st {
            ZoneAssetState::Ready { terrain_meshes, .. } => Some(*terrain_meshes),
            _ => None,
        },
        // A collision grid IS loaded — but see `state`: while `stale` it is the PREVIOUS zone's.
        "collision_loaded": st.collision().is_some(),
        "detail": verdict.map(|v| v.detail()).unwrap_or_else(|| st.detail()),
    })
}

/// The refusal every WORLD-shaped endpoint returns while the loaded assets cannot honestly describe
/// the zone the character is in (#579; zone-identity added per the #595 review). An explicit,
/// machine-readable failure the caller can distinguish — never a plausible answer about a world this
/// client does not have, and never one about a world it has *left*.
fn zone_assets_not_ready(s: &HttpState) -> Option<Response> {
    let st = eqoxide_nav::zone_assets::lock_state(&s.zone_assets).clone();
    let player_zone = s.player().zone;
    let verdict = eqoxide_nav::zone_assets::usability(&st, &player_zone)?;
    Some(zone_assets_refusal(verdict, &st, &player_zone))
}

/// The body of that refusal, split out so a caller that obtained its verdict from
/// [`eqoxide_nav::zone_assets::usable_collision`] (which hands back the grid as well) serves the
/// byte-identical 503 rather than a second, drifting spelling of it (#821 review round 2, B4).
fn zone_assets_refusal(
    verdict: eqoxide_nav::zone_assets::NotUsable,
    st: &eqoxide_nav::zone_assets::ZoneAssetState,
    player_zone: &str,
) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error":        "zone_assets_not_ready",
            "reason":       verdict.as_str(),
            "zone_assets":  zone_assets_json_of(st, player_zone),
            "message":      "the loaded zone assets cannot describe the zone this character is in, \
                             so this endpoint cannot answer without inventing a world. Poll GET \
                             /v1/observe/debug until `zone_assets.state` is \"ready\" (or handle \
                             \"failed\", which will never become ready).",
        })),
    ).into_response()
}

/// **503 for a question that can only be answered from the zone's `.wtr` region map when that map
/// is not there (#803).**
///
/// The `zone_assets_not_ready` gate above covers the terrain GLB and the collision grid built from
/// it. It does **not** cover the `.wtr`: a zone whose GLB loaded fine and whose region map did not
/// is fully `ready`, and used to answer `/v1/observe/zone_exits` with a bare `[]` and 200 OK. That
/// is indistinguishable from the true, common reading "this zone has no zone lines" — and since
/// exits are the only way out of a zone, an agent reads it as *sealed in*, off a success response.
///
/// Deliberately NOT folded into `NotUsable`/`zone_assets_not_ready`. **`usability()` has THREE
/// direct non-test consumers, plus one that reaches it one call indirect.** By call site:
///   * `observe.rs:38` / `:67` / `:1574` — every `/observe/*` endpoint, plus `/debug`'s zone block;
///   * `move_api.rs:260` — `POST /v1/move/goto`, which turns the verdict into its
///     `zone_assets_pending` note (**not** an `/observe/*` route, which is why it was missed);
///   * `walker.rs:1400` — the nav path-walker's `drive_walk` gate; and, indirectly,
///   * `action_loop.rs` — `ActionLoop::resolve_zone_cross`, which since #827 gates on
///     [`eqoxide_nav::zone_assets::usable_collision`]; that function's first statement calls
///     `usability` and returns its verdict as `Err`, so a new variant still reaches zone-crossing.
///
/// **On the count, because the history is easy to misread** (#821 review round 2, B2; #840): an
/// early revision of this comment said three and omitted `move_api.rs`, which made this argument
/// *understate* the blast radius, and it was corrected to four. The direct count is three again for
/// an unrelated reason — #827 turned `action_loop.rs` from a direct caller into an indirect one —
/// **not** because that omission returned; `move_api.rs` is the bullet that was once missing.
/// Re-deriving this list by grep needs care in both directions: `usability(` still matches in
/// `action_loop.rs`, but the only hit there is #827's test asserting its own premise, not a call
/// site; and no grep for `usability(` alone will find the `resolve_zone_cross` consumer at all.
///
/// So a new `NotUsable` variant would stop routing, stop `/v1/move/goto`, stop zone-crossing
/// (through `usable_collision`, per the bullet above) AND stop rendering frames in any zone with a
/// missing `.wtr` — far past what a region-map failure actually invalidates. The refusal belongs to the questions whose answer really does come out of
/// that file. (Confirmed live in the PR's forced-failure run: with the `.wtr` broken and the zone
/// otherwise `ready`, `/frame`, `/zone_entrances` and `/debug` all still answered `200`.)
///
/// `reason` is [`eqoxide_core::region_map::RegionDataAbsent::as_str`] — distinct per cause, so an
/// agent (or an operator reading its log) can tell "the asset pack never delivered this file" from
/// "the file is truncated" from "this build cannot read that version".
fn region_data_unavailable(absent: &eqoxide_core::region_map::RegionDataAbsent) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error":   "zone_region_data_unavailable",
            "reason":  absent.as_str(),
            "detail":  absent.to_string(),
            "message": "this zone's region map (its `.wtr`) is not loaded, and the answer to this \
                        question comes out of that file. An empty list here would say \"this zone \
                        has no exits\" — a claim this client cannot make, and one you could not \
                        tell apart from the truth. This does NOT resolve itself by polling: it is a \
                        missing/unusable asset, not a load in progress. Re-sync the zone's asset \
                        pack, or use `/v1/observe/zone_entrances` (server-advertised, independent of \
                        the `.wtr`) to reason about where this zone connects.",
        })),
    ).into_response()
}

/// #646 (split out of #634/#647): the read-time freshness age every `/v1/observe/*` endpoint now
/// carries, in one form or another. Only `/debug` (`snapshot_age_ms`) and `/nav_debug`
/// (`published_age_ms`) had ANY such field before this — the other 13 routes served last-known
/// state with no way for a driving agent to tell it was frozen. #647's review watched this bite
/// live: with the `eq-net` thread dead, `GET /v1/observe/entities` kept returning `200` with a
/// frozen map and no marker of any kind.
///
/// **This is the SAME clock as `/debug`'s `snapshot_age_ms`, not a second one** — both are
/// `HttpState::health().snapshot_age_ms`, i.e. `NetHealth::last_tick`'s age against the health clock
/// (`Instant::now()` in every non-test build — see `HealthClock`, #760) measured fresh on
/// every request (#343: an age is only true at the instant it's read, so nothing here is cached).
/// `last_tick` is bumped, unconditionally, once per gameplay tick by the SAME `eq-net` thread loop
/// iteration that publishes `GameState` and drains `ActionLoop::tick` — the single writer of every
/// world table these endpoints read (`entity_positions`, inventory, chat messages, dialogue
/// choices, doors, zone points, `GameState`'s spells/skills/book_text) — see
/// `crates/eqoxide-net/src/gameplay.rs` around `action_loop.tick(..)` immediately followed by
/// `publish_snapshot(..)`. So a wedged/dead net thread freezes the world tables and stops bumping
/// this clock in the SAME instant: the age reported here always reflects whether the data next to
/// it can still change, never merely whether that data happens to have changed recently.
///
/// Most endpoints below carry this in-band as a top-level `"snapshot_age_ms"` JSON key — safe,
/// because none of their existing keys collide with that name. A handful of endpoints predate this
/// change with a response body that is a bare array/map (no room for a new key without breaking an
/// existing consumer — `/entities`' default `{name: [x,y,z]}` shape is documented as
/// backward-compatible for `group_driver.py`'s `ents.items()`), plus `/frame`, whose PNG body can't
/// carry a field at all: those carry the identical value in this HTTP header instead, so a caller
/// never has to guess in advance which channel an endpoint uses.
pub(crate) const SNAPSHOT_AGE_HEADER: &str = "x-snapshot-age-ms";

/// Stamp [`SNAPSHOT_AGE_HEADER`] onto a response whose body cannot safely gain a new top-level JSON
/// key (a bare array/map, or a non-JSON body like `/frame`'s PNG). `age_ms` must come from
/// `HttpState::health().snapshot_age_ms` — see the constant's doc for why that (and not some other
/// clock) is the correct source.
fn with_snapshot_age(mut resp: Response, age_ms: u64) -> Response {
    resp.headers_mut().insert(
        HeaderName::from_static(SNAPSHOT_AGE_HEADER),
        HeaderValue::from_str(&age_ms.to_string())
            .expect("a decimal u64 rendered via to_string() is always a valid header value"),
    );
    resp
}

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/debug", get(get_debug))
        .route("/frame", get(get_frame))
        .route("/entities", get(get_entities))
        .route("/inventory", get(get_inventory))
        .route("/messages", get(get_messages))
        .route("/dialogue", get(get_dialogue))
        .route("/spells", get(get_spells))
        .route("/skills", get(get_skills))
        .route("/doors", get(get_doors))
        .route("/zone_entrances", get(get_zone_entrances))
        // Deprecated alias for /zone_entrances (its content was always the entrance/arrival list).
        .route("/zone_points", get(get_zone_entrances))
        .route("/zone_exits", get(get_zone_exits))
        .route("/item_text", get(get_item_text))
        .route("/packets", get(get_packets))
        .route("/who", get(get_who))
        .route("/nav_debug", get(get_nav_debug))
        .route("/asset_sync", get(get_asset_sync))
}

/// GET /v1/observe/asset_sync (#715) — every asset sync in flight, phase-modelled.
///
/// The driving agent has no eyes on the loading screen. `zone_assets` (#579) already says whether
/// the world is usable; this says whether the load behind it is *progressing*, and how fast — the
/// difference between "still downloading, 3 of 7 chunks, 1.2 MB/s" and "wedged".
///
/// ## The three distinctions this body is shaped to keep
///
/// **Idle is not zero progress.** `active` is `false` and `syncs` is empty when nothing is syncing.
/// A download that has not finished a chunk yet is `active: true` with `downloading.chunks_done: 0`.
/// An agent must branch on `active`, never on a count being zero.
///
/// **The phase is modelled, not flattened.** Transfer data lives in a `downloading` sub-object that
/// EXISTS ONLY in the downloading phase, mirroring the producer enum (#708) where a rate is
/// unrepresentable outside it. A flat body with a nullable `rate` would make "not downloading"
/// indistinguishable from "downloading, rate not yet derivable" — so the two are different
/// structures here, not two spellings of null.
///
/// **Concurrent syncs are a LIST, not a slot.** The client runs three loaders, and a short
/// `charmodel/<key>` sync routinely begins and ends inside a long `zone/<zone>` download. A single
/// last-writer-wins slot answered `active: false` for the rest of that zone download (#726 review
/// finding 1). `syncs` carries every live one, oldest-started first; `active` means *at least one*
/// sync is running, which is what the docs have always promised.
///
/// **A LOGIN is an entry, and it is not a transfer (#731).** Every `sync_set` is preceded by an
/// asset-server `login()`, which used to sit outside the observed window: a hung login answered
/// `{"active": false}` while a loader thread was blocked inside it. It is now an entry with
/// `phase: "connecting"` — but it carries **no `set`** (three of the client's four logins serve
/// several sets, or an unbounded queue of them, so there is no true set to name) and **no
/// `downloading`**. Reporting it through the transfer shape, as a download stalled at 0 bytes,
/// would have swapped one falsehood for a subtler one.
///
/// Within `downloading`, `rate_bytes_per_sec` is present only when a rate can honestly be asserted;
/// otherwise it is OMITTED (never null, never 0) and `rate_unavailable` names the rule that
/// withheld it — `phase_too_young` under the producer's 100 ms minimum, `sample_too_stale` once the
/// sample is older than `MAX_RATE_SAMPLE_AGE`. Its absence is unambiguous precisely because the
/// enclosing `downloading` object is present, with `bytes` and `elapsed_secs` still in it.
///
/// The registry is read here, per request, from the same `Arc` the loader threads write — a live
/// read, not a value captured when the server was constructed.
async fn get_asset_sync(State(s): State<HttpState>) -> Json<serde_json::Value> {
    Json(asset_sync_json(&eqoxide_ipc::asset_sync::snapshot(&s.asset_sync)))
}

/// One ended activity, encoded — the `last_ended` slot.
///
/// `ago_ms` is measured at READ time like every other age (#343), never cached.
fn ended_activity_json(e: &eqoxide_ipc::asset_sync::EndedActivity) -> serde_json::Value {
    let ago_ms = e.at.elapsed().as_millis() as u64;
    match &e.what {
        eqoxide_ipc::EndedWhat::Sync { set } =>
            serde_json::json!({ "set": set, "ago_ms": ago_ms }),
        // #731. A login that has ended carries NO `set` (it never had one) and DOES carry a verdict,
        // which a sync cannot: `login_observed` sees the login's `Result`, whereas the sync guard's
        // `Drop` runs identically on success, error and unwind. Without it a failed login and a
        // successful one are the same `active: false` — #731's falsehood reappearing a moment later.
        eqoxide_ipc::EndedWhat::Connect { purpose, outcome } => serde_json::json!({
            "connecting": { "purpose": purpose, "outcome": outcome.as_str() },
            "ago_ms": ago_ms,
        }),
    }
}

/// The pure projection behind [`get_asset_sync`] — takes the snapshot explicitly so the encoding is
/// testable without an `HttpState`, and so the handler holds no lock while serializing.
fn asset_sync_json(snap: &eqoxide_ipc::AssetSyncSnapshot) -> serde_json::Value {
    const SEMANTICS: &str =
        "active:false means NO asset-server work is running anywhere in this process — that is a \
         different state from a sync sitting at zero progress, which is active:true with \
         downloading.chunks_done:0. `syncs` lists EVERY activity in flight, oldest-started first, \
         because several loaders run at once (a zone download, its door set, the common set, and \
         on-demand charmodel sets) and a short one can begin and end inside a long one. An entry is \
         EITHER a set sync (it has `set`, and phase starting/verifying/downloading) OR an \
         asset-server LOGIN (phase:\"connecting\", a `connecting` object naming the free-text \
         purpose, and NO `set` — a login serves several sets, or an unbounded queue of them, so it \
         has no set to name and carries no bytes, chunks or rate; do not read it as a transfer at \
         0 bytes). A login publishes once and never ticks, so its published_age_ms IS how long it \
         has been blocked. The \
         top-level set/phase/downloading/connecting/published_age_ms/running_ms fields are a copy of syncs[0], \
         the OLDEST-STARTED one, and they describe that activity ALONE — `set` names which, and is \
         ABSENT when syncs[0] is a login. syncs[0] is \
         not necessarily the sync you are waiting on (a charmodel sync begun during the previous \
         zone can outlive it), and a healthy syncs[0] says nothing about the others, so do NOT read \
         the top-level fields as the process's health. To follow a particular set, find it by name \
         in `syncs`. To ask 'is anything wedged' with one field, read \
         stalest_published_age_ms: the largest published_age_ms over every live entry INCLUDING \
         logins, so it is \
         large and growing whenever ANY sync is wedged OR any login is blocked, whatever syncs[0] \
         is doing. It is absent \
         when nothing is running. Transfer data appears ONLY inside the `downloading` object, which exists \
         only in that phase, so a rate can never be read for a phase that has none. Inside \
         `downloading`, an ABSENT rate_bytes_per_sec never means zero: `rate_unavailable` says \
         which rule withheld it — phase_too_young (under the producer's 100 ms minimum elapsed, no \
         honest rate can be divided yet) or sample_too_stale (nothing has ticked for over 2000 ms, \
         so the last rate has stopped being an assertion about NOW; bytes and elapsed_secs are \
         still there, and bytes/(elapsed_secs+published_age_ms/1000) is the honest lower bound). \
         EVERY field except published_age_ms and running_ms is frozen at the producer's last tick, \
         and it ticks only when a chunk completes — including elapsed_secs, so the phase has \
         actually been running elapsed_secs+published_age_ms/1000. running_ms is measured at read \
         time and keeps moving even while a sync is wedged; a large and GROWING published_age_ms \
         means THAT sync is wedged, not progressing — for the process, use \
         stalest_published_age_ms. last_ended names the most recent activity of ANY KIND to LEAVE \
         this list and is how 'nothing is syncing now' is told apart from 'nothing has ever synced' \
         (null). For a SET SYNC it carries `set` and says ended, NOT succeeded: the client cannot \
         tell a success from a failure or a panic there. For a LOGIN it carries `connecting` with a \
         `purpose` and an `outcome` of succeeded/failed/unknown — measured, not guessed, because \
         the login wrapper sees the call's result; `unknown` means a panic unwound through it. But \
         last_ended is a SINGLE last-writer-wins slot: the next activity to end, login or set sync, \
         overwrites it. Several loaders end at once here, so a login's verdict in that slot survives \
         until the very next activity ends and a poller will routinely miss failures entirely. Do \
         NOT ask last_ended whether a login failed. Ask login_outcomes, which counts every ENDED \
         login by outcome {succeeded,failed,unknown}: failed+unknown > 0 means a login did not \
         complete, at ANY polling cadence. These counters are monotonic WITHIN ONE CLIENT PROCESS \
         and start again at zero when the client restarts, which it does routinely — so the \
         difference between two polls is what happened in between only if the client did not restart \
         between them. This body carries no restart marker, so a poller keeping a cursor across \
         restarts must treat a DECREASE as a new process, not as a correction. Alongside each \
         counter, last_login_succeeded / last_login_failed / last_login_unknown name the most recent \
         login that ended THAT way, in full (`connecting.purpose`, `connecting.outcome`, `ago_ms`) \
         — ONE FIELD PER OUTCOME, so a count and the record beside it always describe the same \
         login: login_outcomes.X > 0 if and only if last_login_X is present, for each X in \
         login_outcomes, and last_login_X.connecting.outcome is always X. Read the set of outcomes \
         off login_outcomes' own keys rather than hard-coding it from this sentence. A failure and \
         a panic therefore do not \
         compete for one slot and neither hides the other. Each is ABSENT — never null — until a \
         login ends that way, and each is overwritten only by a LATER login of the SAME outcome: so \
         these are 'most recent per outcome', not a log, and a second failure does replace the first \
         failure's purpose while both stay counted. Logins still in flight are in none of them: they \
         are in `syncs`.";

    let syncs: Vec<serde_json::Value> = snap.live.iter().map(sync_json).collect();
    let mut body = serde_json::json!({
        "active": !syncs.is_empty(),
        "syncs": syncs,
        // #726 review N5: an empty `syncs` used to be the same answer for "a sync just finished" and
        // "no sync has ever run in this process". `null` here is the honest "no sync has ended yet";
        // `ago_ms` is measured at READ time like every other age (#343).
        // #726 review N5 / #743 review B1. `null` is the honest "nothing has ended yet"; anything
        // else is the SINGLE most recent activity of any kind, and the next one to end overwrites it.
        "last_ended": snap.last_ended.as_ref().map(ended_activity_json),
        "semantics": SEMANTICS,
    });
    // #743 review B1 and B3: the fields that actually answer "did a login fail", and "which one".
    // `last_ended` answers neither — it is one slot every activity overwrites, and the reviewer
    // measured a genuinely failed login surviving there for 0 of 75 polls. These are written from
    // the same measured `Result` as the verdict in `last_ended`; they differ only in RETENTION.
    //
    // **Encoded in ONE pass over the per-outcome slots, deliberately (B3).** Round 2 shipped a count
    // per outcome next to a SINGLE `last_login_failure` shared by `failed` and `unknown`, so a
    // caller who read the record on the strength of a counter could be handed the other outcome's
    // login. Here the counter and the record for an outcome are emitted from the same loop
    // iteration, and the field NAME, the `outcome` token inside it and the counter key are all
    // `outcome.as_str()` — one source, so they cannot disagree, and no category can be counted
    // without its record being served beside it.
    //
    // `login_outcomes` is present-with-zeros rather than absent, unlike `stalest_published_age_ms`
    // below: a count of zero here is a real measurement ("no login has ended this way yet"),
    // whereas a max over no samples is not a measurement at all. Each `last_login_<outcome>` follows
    // the opposite, and equally deliberate, rule — ABSENT until such a login ends, never null, which
    // is the same absent-not-null rule the rest of this body follows.
    {
        let obj = body.as_object_mut().expect("json! object");
        let mut counts = serde_json::Map::new();
        // #743 round-3 review B1: the loop is driven by `last_login.slots()`, not by
        // `ConnectOutcome::ALL` directly. Walking `ALL` here would have served whatever `ALL`
        // happened to list, which is exactly the gap the review measured. The counter is still
        // looked up BY OUTCOME rather than zipped, so the pairing of a count with its record does
        // not depend on two arrays being in the same order.
        //
        // ⚠️ [EDIT, round 5 (#755 review): "the crate does not build if it is" was false, measured.]
        // `slots()` forces a NEW FIELD to be named at its destructure site (see its rustdoc in
        // `eqoxide-ipc/src/asset_sync.rs` for what that does and does not cover) — but naming it
        // there is not the same as this loop serving it. rustc's own suggested fix for the E0027 the
        // pin raises is `refused: _`, and taking it verbatim builds clean, with zero warnings, while
        // that outcome stays absent from `login_outcomes` and from every `last_login_<outcome>` this
        // loop emits. Separately: THIS LOOP is not the only thing that puts an outcome on the wire
        // [EDIT, round 6: "not what makes an outcome reach the wire at all" read as "this loop puts
        // nothing on the wire", which is false — it is the sole producer of every `login_outcomes`
        // key and every `last_login_<outcome>` record]. `last_ended`, encoded above via
        // `ended_activity_json`, calls `ConnectOutcome::as_str()` on the raw stored outcome directly
        // — it never touches `ALL` or `slots()`. So a login that ends with an outcome missing from
        // `ALL` can be simultaneously absent here and present in `last_ended`: one body, two answers
        // about one login.
        for (outcome, record) in snap.last_login.slots() {
            let token = outcome.as_str();
            counts.insert(token.into(), snap.login_outcomes.count(outcome).into());
            if let Some(r) = record {
                obj.insert(format!("last_login_{token}"), serde_json::json!({
                    // Same shape as a `last_ended` login, so a caller that can parse one can parse
                    // the other. `outcome` is derived from the slot, not stored in the record, so it
                    // cannot contradict the field it appears under.
                    "connecting": { "purpose": r.purpose, "outcome": token },
                    // Measured at READ time like every other age (#343), never cached.
                    "ago_ms": r.at.elapsed().as_millis() as u64,
                }));
            }
        }
        obj.insert("login_outcomes".into(), serde_json::Value::Object(counts));
    }
    // The process-wide wedge signal (#726 review round 2, finding 1). Derived from the ages ALREADY
    // ENCODED in `syncs`, not re-measured, for the same reason the mirror below is a copy: two
    // `elapsed()` calls on one sample can straddle a threshold, and a top-level age that matched no
    // entry in its own body would be a response contradicting itself.
    //
    // This exists because the mirror is a scalar view of a plural fact. `syncs[0]` is a real sync
    // and every one of its numbers is true, but a caller who reads only the top level learns
    // nothing about the others — and the previous wording sent them there for exactly the question
    // the top level cannot answer. The max is the honest one-field answer: it is large whenever ANY
    // live sync is wedged, so a caller following it can never be told "healthy" while this process
    // holds evidence to the contrary. Which sync is stale still requires `syncs`; that is a lookup,
    // not a false all-clear.
    //
    // Absent, not zero, when nothing is running: a max over no samples is not a measurement, and 0
    // would read as "everything is perfectly fresh".
    let stalest = syncs_encoded_max_age(&body);
    if let Some(ms) = stalest {
        body.as_object_mut().expect("json! object")
            .insert("stalest_published_age_ms".into(), serde_json::json!(ms));
    }
    // The PRIMARY sync, mirrored to the top level: `syncs[0]`, the oldest-STARTED one. Copied from
    // the very same encoded object the array carries — not re-derived — so the two cannot drift.
    //
    // It is deliberately NOT chosen as "the sync that matters", because the client cannot know
    // which sync the caller is waiting on and a guess dressed as an answer is the defect this
    // endpoint exists to avoid. Oldest-started is chosen for STABILITY: it does not hop between
    // syncs from poll to poll, so a caller tracking the common single-sync case sees one identity.
    // The previous comment here claimed it was "the long zone download in every overlap the client
    // actually produces"; that universal is false — a `charmodel/<key>` sync begun during the
    // previous zone can still be live when the next zone load starts, and is then older.
    if let Some(primary) = body["syncs"].as_array().and_then(|a| a.first()).cloned() {
        let (obj, primary) = (body.as_object_mut().expect("json! object"), primary);
        for (k, v) in primary.as_object().expect("sync_json builds an object") {
            obj.insert(k.clone(), v.clone());
        }
    }
    body
}

/// The largest `published_age_ms` among the ALREADY-ENCODED `syncs` entries, or `None` if there are
/// none. Reads the encoded array rather than the snapshot on purpose — see the call site.
///
/// #731: this needs no special case for a login. Every entry carries `published_age_ms`, and a
/// login's is its whole blocked duration (it publishes once, at begin, and never ticks), so a hung
/// login makes the documented one-field wedge check large and growing exactly like a wedged sync.
fn syncs_encoded_max_age(body: &serde_json::Value) -> Option<u64> {
    body["syncs"].as_array()?.iter().filter_map(|s| s["published_age_ms"].as_u64()).max()
}

/// One live activity, encoded. Used for BOTH the `syncs` array and the mirrored top-level primary.
fn sync_json(a: &eqoxide_ipc::AssetSyncActivity) -> serde_json::Value {
    // Measured ONCE, here, and used for both the reported age and the rate-staleness decision. Two
    // separate `elapsed()` calls could straddle the threshold and produce a body that omits the rate
    // as `sample_too_stale` while reporting a `published_age_ms` under the bound — a response that
    // contradicts its own documented rule.
    let sample_age = a.published_at.elapsed();
    let mut body = serde_json::json!({
        // Freshness, computed AT READ TIME (the #343 discipline — an age must never be cached).
        // Every other field here is frozen at the producer's last tick, and the producer ticks only
        // when a chunk completes: a WEDGED download leaves them all in place with nobody left to
        // update them. This is the field that tells a stalled sync apart from a progressing one.
        "published_age_ms": sample_age.as_millis() as u64,
        // How long the whole `sync_set` call has been running, also at read time. Unlike
        // `downloading.elapsed_secs` this keeps MOVING while a sync is wedged, and it is the only
        // duration that exists at all in the `starting` phase — a hung manifest fetch has no
        // chunks, no bytes and no elapsed, just an age.
        "running_ms": a.started_at.elapsed().as_millis() as u64,
    });
    let obj = body.as_object_mut().expect("json! object");
    let phase = match &a.work {
        // #731. A login gets NO `set` key — it is not a sync of any set, and three of the client's
        // four logins serve several sets (or an unbounded queue), so there is nothing true to put
        // there. It also gets no `downloading` object, so `bytes`/`rate`/`chunks` are as
        // unreachable here as they are in `verifying`. Reporting a login through the transfer shape
        // — "downloading, 0 bytes" — would have replaced #731's falsehood with a subtler one.
        eqoxide_ipc::AssetSyncWork::Connecting { purpose } => {
            obj.insert("phase".into(), serde_json::json!("connecting"));
            obj.insert("connecting".into(), serde_json::json!({ "purpose": purpose }));
            return body;
        }
        eqoxide_ipc::AssetSyncWork::Sync { set, phase } => {
            obj.insert("set".into(), serde_json::json!(set));
            phase
        }
    };
    match phase {
        eqoxide_ipc::AssetSyncPhase::Starting => {
            obj.insert("phase".into(), serde_json::json!("starting"));
        }
        eqoxide_ipc::AssetSyncPhase::Verifying => {
            obj.insert("phase".into(), serde_json::json!("verifying"));
        }
        eqoxide_ipc::AssetSyncPhase::Downloading { chunks_done, chunks_total, bytes, elapsed } => {
            obj.insert("phase".into(), serde_json::json!("downloading"));
            let mut dl = serde_json::json!({
                "chunks_done":  chunks_done,
                "chunks_total": chunks_total,
                "bytes":        bytes,
                "elapsed_secs": elapsed.as_secs_f64(),
            });
            let dl_obj = dl.as_object_mut().expect("json! object");
            // The producer's own 100 ms guard, PLUS the read-time staleness rule the HUD does not
            // need (it redraws from the tick that produced the sample; this endpoint can be polled
            // minutes later). Either way the key is omitted rather than faked, and the reason is
            // named — see `eqoxide_ipc::asset_sync::observed_download_rate`.
            match eqoxide_ipc::asset_sync::observed_download_rate(*bytes, *elapsed, sample_age) {
                Ok(rate) => { dl_obj.insert("rate_bytes_per_sec".into(), serde_json::json!(rate)); }
                Err(why) => { dl_obj.insert("rate_unavailable".into(), serde_json::json!(why)); }
            }
            obj.insert("downloading".into(), dl);
        }
    }
    body
}

/// GET /v1/observe/nav_debug (#608) — the nav diagnostics snapshot the walker PUBLISHES, in
/// structured form. The driving agent has no eyes: this is the same single source of truth the
/// depth-tested 3D overlay draws, in the agent's encoding. **This layer encodes; it derives
/// nothing** — the JSON body is a structural serde projection of
/// `eqoxide_nav::diagnostics::NavDebugSnapshot` (nav-owned types), so a field cannot silently
/// diverge from what nav published. The only additions are the composed `zone_assets` load-state
/// object (the same published #579 source `/debug` serves) and the `semantics` note.
///
/// Honesty contract, verbatim from the snapshot's docs: **absence means unevaluated** — a cell or
/// edge missing from `plan.trace` was never evaluated by the planner, and must not be treated as
/// walkable OR blocked.
/// ⚠️ No-re-derivation hazard (#615 review F6): unlike the renderer's encoder — whose signature
/// cannot even NAME the collision grid — this handler runs inside `HttpState`, which carries
/// `shared_collision` for other endpoints. The no-second-derivation property here is therefore a
/// CONVENTION this function must keep, not a structural impossibility: do not consult
/// `s.shared_collision` (or any other world source) to "check" or "fix" a published value. The
/// verbatim test below runs with BOTH an absent and a PRESENT collision grid, so a re-derivation
/// hidden behind `if let Some(col) = …` cannot pass as a no-op.
async fn get_nav_debug(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let snap = s.nav_debug_view.lock().unwrap().clone();
    match snap {
        None => Json(serde_json::json!({
            "available": false,
            "note": "no nav diagnostics snapshot published yet (the walker has not ticked — \
                     no /goto issued and no zone loaded since launch)",
            "zone_assets": zone_assets_json(&s),
        })),
        Some(snap) => {
            let mut v = serde_json::to_value(&*snap)
                .unwrap_or_else(|e| serde_json::json!({ "encode_error": e.to_string() }));
            if let Some(obj) = v.as_object_mut() {
                obj.insert("available".into(), serde_json::json!(true));
                // Freshness, computed AT READ TIME (the #343 discipline — an age must never be
                // cached): how long ago the walker published this snapshot. A consumer must treat
                // a large value as stale-as-of-then, exactly like `snapshot_age_ms` on /debug.
                obj.insert("published_age_ms".into(),
                    serde_json::json!(snap.published_at.elapsed().as_millis() as u64));
                obj.insert("zone_assets".into(), zone_assets_json(&s));
                obj.insert("semantics".into(), serde_json::json!(
                    "plan.trace records what the planner EVALUATED, with per-edge verdicts \
                     (accepted kind / rejected reason). Absence means UNEVALUATED — never walkable, \
                     never blocked. trace.outcome_calls marks the DECIDING call; calls outside it \
                     are tier/anchor retries that lost. A call with truncated:true stopped RECORDING \
                     (not searching) at its edge budget. committed_coarse/committed_fine are the \
                     walker's actual committed routes, verbatim; player is null when the position \
                     was unknown at publish time. clearance is TWO different questions, not one \
                     (#885): clearance.body is the movement controller's own placement verdict at \
                     the CHARACTER's position, and anything but \"placeable\" means the rest of \
                     this clearance sample describes a point the character does not occupy. \
                     clearance.body is NOT a claim about whether the character can move — that is \
                     player.hold on /v1/observe/debug. clearance.wall_spokes / footprint_ok / \
                     field_* are the PLANNER's model sampled at clearance.anchor, which may be a \
                     different height: anchor.reference_z is always the character's own z, while \
                     anchor.z is present ONLY when anchor.kind == \"floor\" (kind \
                     \"no_floor_in_band\" means no floor was found in the band and the rays were \
                     cast from reference_z). A spoke reading of \"clear_to_cap\" is a LOWER BOUND \
                     (nothing within cap), not a distance of cap."));
            }
            Json(v)
        }
    }
}

/// GET /v1/observe/item_text — the text of the most recently read book/note (from
/// POST /v1/interact/read). Returns `{"text": "..."}` once a book has been read this session, or
/// `{"text": null}` if none has. Newlines are decoded from RoF2's backtick marker. (#288)
async fn get_item_text(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let text = s.player().book_text;
    // #646: read-time freshness — see `SNAPSHOT_AGE_HEADER`'s doc for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    Json(serde_json::json!({ "text": text, "snapshot_age_ms": snapshot_age_ms }))
}

/// Query params for GET /v1/observe/packets. All optional; every value arrives as a string and is
/// parsed leniently so an agent can hand-write the URL.
#[derive(serde::Deserialize, Default)]
struct PacketsQuery {
    /// Only records with capture index `n >= since` (page-forward cursor).
    since: Option<u64>,
    /// Cap the number of records returned (the most RECENT matching ones).
    limit: Option<usize>,
    /// `in` | `out` — filter by direction.
    dir: Option<String>,
    /// Filter by opcode. Accepts hex (`0x7dfc`) or decimal.
    op: Option<String>,
    /// `?summary=1` → return the analysis (histogram + seq-gaps) instead of the raw record list.
    summary: Option<String>,
    /// `?enable=1|0` → toggle capture at runtime before reading. Returned in the payload.
    enable: Option<String>,
    /// `?clear=1` → drop the buffered records (and reset the epoch) before reading.
    clear: Option<String>,
}

fn truthy(v: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    v == "1" || v == "true" || v == "on" || v == "yes"
}

/// Parse an opcode filter as hex (`0x…`) or decimal.
fn parse_op(v: &str) -> Option<u16> {
    let v = v.trim();
    if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).ok()
    } else {
        v.parse::<u16>().ok()
    }
}

/// GET /v1/observe/packets — dump the packet-telemetry ring as JSON (#525).
///
/// Capture is DEFAULT-OFF (enable at startup with `EQOXIDE_PKTLOG=1`, or per-request with
/// `?enable=1`). Filters: `?since=`, `?limit=`, `?dir=in|out`, `?op=0x7dfc`. `?summary=1` returns
/// the opcode histogram + reliable-sequence-gap analysis (the #463 diagnostic) instead of raw
/// records. `?clear=1` resets the buffer. Controls apply BEFORE the read, so
/// `?enable=1` on a first call just turns capture on (the buffer is still empty).
async fn get_packets(State(s): State<HttpState>, Query(q): Query<PacketsQuery>) -> Json<serde_json::Value> {
    use eqoxide_telemetry as pkt;
    // #646: capture itself runs inside the same `eq-net` thread loop as `NetHealth::last_tick` —
    // see `SNAPSHOT_AGE_HEADER`'s doc. A dead thread stops capturing AND stops bumping this clock
    // in the same instant, so a large value here means "no new packets are coming", not merely
    // "none arrived recently".
    let snapshot_age_ms = s.health().snapshot_age_ms;

    if let Some(e) = q.enable.as_deref() {
        pkt::set_enabled(truthy(e));
    }
    if q.clear.as_deref().is_some_and(truthy) {
        pkt::clear();
    }

    let query = pkt::Query {
        since: q.since,
        dir: q.dir.as_deref().and_then(pkt::Dir::parse),
        op: q.op.as_deref().and_then(parse_op),
        limit: q.limit,
    };
    let records = pkt::query(&query);

    if q.summary.as_deref().is_some_and(truthy) {
        // Reliable-seq gap detection MUST run over the dir-filtered but NOT op-filtered stream.
        // `rel_seq` is a single per-direction counter shared across ALL opcodes, so feeding an
        // op-filtered set to the gap detector would drop the intervening reliable packets of other
        // opcodes that legitimately consumed sequence numbers and FABRICATE "lost packets" — an
        // agent-honesty violation, and exactly what `scripts/packet-analysis.py --dir in --op 0x5089`
        // (its documented #463 example, which defaults to summary=1) would otherwise do during a
        // zone-in (#532 review). The histogram/rate still honor `op` (the view the caller asked for);
        // only the gap stream ignores it. `limit` is dropped too so gaps see the full direction.
        let gap_records = pkt::query(&pkt::Query { op: None, limit: None, ..query.clone() });
        let analysis = pkt::analyze_with_gaps(&records, &gap_records);
        Json(serde_json::json!({
            "enabled": pkt::enabled(),
            "summary": analysis,
            "snapshot_age_ms": snapshot_age_ms,
        }))
    } else {
        Json(serde_json::json!({
            "enabled": pkt::enabled(),
            "count": records.len(),
            "packets": records,
            "snapshot_age_ms": snapshot_age_ms,
        }))
    }
}

async fn get_debug(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let cam   = s.camera.snapshot.lock().unwrap().clone();
    // Projected from the network thread's GameState, and freshness measured RIGHT NOW — not read
    // out of a struct some other loop published whenever it last felt like running (#343).
    let player = s.player();
    let prov = player.position_provisional_since;
    let health = s.health();
    let frame_profile = *s.frame_profile.lock().unwrap();
    // #797 — the STATIC-arm skin-cap downgrades the renderer has recorded so far this session,
    // keyed by loaded file base name (never a full local path — see `downgrade_key`). Cloned out
    // from behind the lock so the JSON literal below can move it in without holding the mutex
    // across serialization.
    let skin_cap_downgrades = s.skin_cap_downgrades.lock().unwrap().clone();
    let nav = s.nav.nav_state.lock().unwrap().clone();
    // Is nav answering from WINDING-BLIND (inverted-art) ground in this zone? (#375, D-2)
    //
    // D-2 (#375) made the floor predicate `is_standable` FACING-BLIND: a surface is ground on its
    // flatness + headroom, whichever way its art is wound — because some zones bake real, walkable
    // ground from INVERTED (down-facing) art (the qcat live wedge stood on exactly such a walkway,
    // which the old facing filter deleted). That is correct, but it means nav can no longer VERIFY a
    // floor's facing there — it is standing on unverified-winding ground. `facing_blind_hits` counts
    // each query answered from a down-facing surface, so the agent can SEE it.
    //
    // (This REPLACES the old `nav_degraded`/`inverted_floor_art` signal, which counted the
    // `column_bottom` recovery valve firing. D-2 deleted that valve — so if this were left reading the
    // dead counter it would always be `null`, i.e. "every nav query answered from PROPERLY WOUND
    // floors," which is a confident falsehood in exactly the inverted-art zones (permafrost/highpass/
    // neriakc/qcat) where nav is now on winding-blind ground. A degraded/unverified mode must never be
    // silent, so the signal moves with the mechanism.)
    //
    // `null` = every standable surface answered so far faced UP (properly wound). Non-null = nav has
    // answered `queries` times from down-facing (inverted-art) ground since zone load.
    let nav_support = s.shared_collision.read().unwrap().as_ref().and_then(|col| {
        let hits = col.facing_blind_hits();
        (hits > 0).then(|| serde_json::json!({
            "reason": "facing_blind_ground",
            "queries": hits,
            "detail": "parts of this zone's collision mesh are wound INVERTED (down-facing where \
                       ground should face up). Since D-2 (#375) nav accepts such surfaces as floor on \
                       flatness + headroom (they ARE walkable — the qcat wedge proved it), but their \
                       true facing is unverified, so routes through those areas are planned on \
                       winding-blind ground. Not an error; an honest 'this footing is unverified'.",
        }))
    });
    // Tiered clearance (#358): routes are normally planned with a body-width of margin from walls
    // and drops. When the ONLY route to a goal is one that threads a narrow door or a tight bridge
    // with no margin to spare, the planner falls back to the minimum clearance (exactly the
    // character's own collision radius) — still genuinely walkable, but riskier. Report it: an agent
    // that is being handed tight routes deserves to know it is, rather than just noticing it falls
    // off things more often.
    let nav_tight = s.shared_collision.read().unwrap().as_ref().and_then(|col| {
        let n = col.tight_plans();
        (n > 0).then(|| serde_json::json!({
            "reason": "minimum_clearance_fallback",
            "routes": n,
            "detail": "no route existed at the preferred clearance, so these routes were planned at \
                       the MINIMUM (the character's own collision radius) — they fit, but with no \
                       margin from the walls and drops they pass. Expect tight doorways/bridges.",
        }))
    });
    // The fine 2u steering tier's last honest word (#382). `null` when it is threading cleanly — a
    // healthy tier says nothing, exactly like `nav_support` / `nav_tight`.
    let nav_local = nav.local.as_ref().filter(|l| l.state != "threaded").map(|l| serde_json::json!({
        "state":       l.state,
        "reason":      l.reason,
        "stuck_ticks": l.stuck_ticks,
        "plan_us":     l.plan_us,
        "detail": match l.state.as_str() {
            "no_way_through" => "the FINE 2u planner CLOSED its whole 40u window without finding a way \
                                 along the committed coarse route. The corridor is not threadable from \
                                 here. This is a LOCAL fact — it does NOT mean the goal is unreachable \
                                 (the coarse route is being re-planned around it, #246).",
            "exhausted"      => "the FINE 2u planner was CUT SHORT before closing its window (node cap). \
                                 This is 'I don't know', NOT 'there is no way through' — the walker is \
                                 steering on the best partial it has.",
            "planner_dead"   => "the fine-tier worker thread has DIED. Steering has degraded to the \
                                 COARSE 8u route only for the rest of this session: the character keeps \
                                 walking, but without 2u detail it will handle thin ramps and narrow \
                                 openings worse. This is a client fault; restart to recover it.",
            _                => "",
        },
    }));
    // #851 — the calibration data behind `nav_state: "navigating_stalled"`. `null` whenever the
    // walker is not stalled, so a healthy walk says nothing here, exactly like `nav_local` above.
    // The pair is written from ONE verdict in ONE call (`Walker::publish_drive_state`), under one
    // lock hold, so the word and this payload cannot disagree; an agent may read either.
    //
    // That covers the walker's own publication. Every route OUT of a nav state goes through one of
    // exactly three writers on `NavStatus` — `retire_to_idle` (the `idle` retirement),
    // `stamp_fresh_goal` (a new goal arriving from the command side) and `transition_within_goal`
    // (the walker's mid-route word change) — and none of them can leave this payload behind, because
    // all three destructure `NavStatus` exhaustively (no `..`) and so cannot forget a field (#851
    // review round 1, B1: only `retire_to_idle` existed then, and the flat assignment list that
    // `stamp_new_goal` used instead published the dead goal's `nav_stall` beside the next goal's
    // `pending`).
    let nav_stall = nav.stall.map(|s| serde_json::json!({
        "quiet_ticks": s.quiet_ticks,
        "quiet_ms":    s.quiet_ms,
        "repaths":     s.repaths,
        "route":       s.route,
        "detail": "the walker HAS a committed route and is NOT executing it: neither progress \
                   channel — the route cursor advancing by walking, nor the closest 3-D approach to \
                   the goal improving — has fired for `quiet_ticks` nav ticks. It is still in \
                   stall/back-off/re-path recovery and may escape; it gives up at 8 re-paths with \
                   `blocked` — reason `local_no_way_through` when the fine planner also says there \
                   is no way through, `walker_stalled` otherwise. Do NOT read this as terminal, and \
                   do NOT read it as progress. If `route` is `partial` the committed route did not \
                   reach your goal in the first place. This payload is about the goal in \
                   `nav_goal_id` and no other.",
    }));
    // The agent-honesty blockage payload behind a terminal `no_path` (#378 Phase 2). `null` when
    // there is nothing to report (not a terminal no_path, or the diagnosis could not be computed —
    // honest silence, never a fabricated hazard). `goal` is the DEFINITIVE "your goal itself cannot
    // be stood at"; `frontier` is "I got as close as here and THIS is the obstruction between me and
    // the goal" — ONE blocking fact, not necessarily the only one, and named as such (not `reason`).
    let blk_json = |b: &crate::NavBlockage| serde_json::json!({
        "hazard": b.hazard, "at": b.at });
    let nav_blocked_by = (nav.blocked_goal.is_some() || nav.blocked_frontier.is_some()).then(|| {
        serde_json::json!({
            "goal":     nav.blocked_goal.as_ref().map(blk_json),
            "frontier": nav.blocked_frontier.as_ref().map(blk_json),
            "detail": "the obstruction behind this no_path. `goal` (if present) is definitive — the \
                       goal itself cannot be stood at. `frontier` is the hazard at the search's \
                       CLOSEST APPROACH to the goal — one blocking fact, not necessarily the only one \
                       and not necessarily the one to fix.",
        })
    });
    // THE #543 DISCLOSURE. Nav found no verifiable route AND it declined one or more teleport pads
    // that are physically right here (see `walker::TRUST_ADVERTISED_SAME_ZONE_CROSSINGS`). Reporting
    // only the `no_path` would be its own quiet falsehood — "there is nothing here" — when what the
    // client actually knows is "there is something here and I cannot vouch for it". So the declined
    // pads ride out with the failure, on the SAME response the driver already polls for
    // `nav_state`/`nav_reason`: an agent must not have to know to ask a second endpoint for a fact
    // the client withheld from the first (the full per-pad record is also on /v1/observe/nav_debug).
    //
    // The wording is load-bearing. Every destination here is ADVERTISED, never verified — the client
    // has no way to confirm one and does NOT remember where any pad landed before, so nothing in this
    // payload may read as knowledge of where a pad goes. Taking one is the AGENT's decision.
    //
    // Not folded into `nav_reason`: that is a single machine token with an established vocabulary
    // drivers already branch on (#337), and the reason for the failure is genuinely unchanged — the
    // route really is unreachable by walking. This is an additional fact ALONGSIDE it.
    let nav_terminal_no_route = matches!(nav.state.as_str(), "no_path" | "search_exhausted");
    let nav_declined_pads = nav_terminal_no_route.then(|| {
        let snap = s.nav_debug_view.lock().unwrap().clone();
        let pads: Vec<serde_json::Value> = snap.iter().flat_map(|snap| snap.pads.iter()).filter_map(|p| {
            match p.knowledge {
                eqoxide_nav::diagnostics::PadKnowledge::AdvertisedSameZoneDeclined {
                    footprint, footprint_count, ref alternates, region_at, advertised_dest,
                    advertised_dest_floor,
                } => Some(serde_json::json!({
                    "index": p.index,
                    // The spot to TRY first: the nearest standable point inside the pad's trigger
                    // region, measured in this client's own collision mesh. A CANDIDATE, not a
                    // promise — verified live that one leaf of a pad fired nothing while another
                    // leaf of the same pad crossed immediately. `null` = no standable point found at
                    // all: a warning, not a reason to withhold the pad (#660 review B1).
                    "footprint": footprint,
                    // How many standable spots exist in total, and the next few to try — a count
                    // without the alternates is a number the agent cannot act on (#660 review NB2).
                    "footprint_count": footprint_count,
                    "alternates": alternates,
                    // Where the region IS, standable or not — a pad is never reduced to "somewhere".
                    "region_at": region_at,
                    // What the server ADVERTISED, VERBATIM off the wire (wire z datum). NOT where the
                    // pad goes; the client cannot know that. `null` = the pad advertises no arrival
                    // at all (keep-position sentinel) — which does NOT make it un-takeable.
                    "advertised_dest": advertised_dest,
                    // Our own floor model's answer for that advertised column, or `null` if it has no
                    // floor. Kept separate from the verbatim value so a client derivation is never
                    // passed off as the server's claim (#660 review NB3).
                    "advertised_dest_floor": advertised_dest_floor,
                    "advertised_same_zone": true,
                    "destination_verified": false,
                })),
                _ => None,
            }
        }).collect();
        (!pads.is_empty()).then(|| serde_json::json!({
            "reason": "advertised_same_zone_unverifiable",
            "pads": pads,
            "detail": "nav found no verifiable route, but these teleport pads are within this zone's \
                       loaded geometry and nav DECLINED to route you through them. The server \
                       ADVERTISED each as leading somewhere inside THIS zone; the client cannot \
                       verify that, because the server resolves a crossing from trigger data the \
                       wire never carries — so a pad advertised as same-zone may in fact be a real \
                       cross-zone line, and walking onto it can land you in another zone. There is \
                       no such thing as a VERIFIED same-zone pad here, and this client does not \
                       remember where any pad landed before. `footprint` is measured geometry: the spot \
                       to TRY. `advertised_dest` is the server's ADVERTISEMENT, not a \
                       known destination, and `advertised_dest_floor` is this client's own snap of \
                       it, not a second source. Taking one is YOUR decision — after arriving, read \
                       `player.zone` and `player.pos` to find out where it actually went, and note \
                       that both are PROVISIONAL for a moment after a crossing (the client applies \
                       the advertised arrival locally so you leave the pad, and the server's echo \
                       supersedes it) — so re-read them until they settle before concluding \
                       anything. A pad with `advertised_dest: null` advertises no arrival at all; \
                       it is still takeable, you just have no claim to compare against. \
                       `footprint` is the spot to TRY, not a promise: walking to one spot on a pad \
                       can fire nothing while another spot on the SAME pad crosses immediately, and \
                       a goto stops within its arrival tolerance, which can leave you just outside \
                       a small trigger. If nothing happens, try `alternates` before concluding the \
                       pad is inert.",
        }))
    }).flatten();
    // The PER-ROUTE clearance tier the CURRENT route was found at (#378 Phase 2 / design §4c).
    // `minimum` = threaded a tight gap at the character's own collision radius (riskier — no margin);
    // `preferred` = the roomy tier carried it. Distinct from the zone-lifetime `nav_tight` counter:
    // this is the fact for the route the character is walking RIGHT NOW.
    let nav_tier = nav.tier;
    // #579 (agent-honesty): is the world this response describes actually LOADED? A zone's terrain
    // GLB (freportw: ~30 MB) decodes + collides on a background thread for several seconds, during
    // which the client stands on a placeholder ground plane with no collision. Without this field an
    // observer in that window reads a flat, exit-less, unobstructed void as the truth (the false
    // #560 report). `state` is `idle` | `pending` | `ready` | `failed` — `failed` is deliberately
    // NOT folded into `pending`: a permanent failure reported as "pending" would make an agent wait
    // forever. `ready` cannot be published without a terrain mesh count AND a collision grid with
    // geometry (see `ZoneAssetState::ready`), so it always carries its own evidence.
    let zone_assets = zone_assets_json(&s);
    // #713 items 1 & 2 — the two zone-cross disclosures. Both `null` while there is nothing to say,
    // like every other disclosure object here. Both are RELAYED from the net thread's `GameState`,
    // not re-derived: `zone_cross_stopped` reads `CrossAttempts::blocks`, which is the very
    // predicate the auto-cross gate consults, so "stopped trying" and "says it stopped trying"
    // cannot disagree.
    let (zone_cross_stopped, zone_cross_best_effort) = {
        let gs = s.game_state.load();
        let stopped = gs.zone_cross_attempts
            .filter(|t| t.blocks())
            .map(|t| serde_json::json!({
                "reason": "cross_attempt_limit",
                "region_index": t.last_index(),
                "attempts": t.count(),
                "detail": "the client sent this many OP_ZoneChange requests without leaving \
                           zone-line geometry, so it has stopped requesting and will NOT retry on \
                           its own. This is a TERMINAL state, not a crossing in progress — do not \
                           wait on it. It is not a claim about why: the server may be refusing, or \
                           answering nothing at all; the client cannot tell those apart. \
                           `region_index` is the LAST region tried, not necessarily the only one — \
                           the allowance is per continuous stand, so a stand spanning several \
                           regions shares it. To retry, walk OFF every zone-line region and back on \
                           (that clears the tally on the first tick the client sees you off every \
                           region — roughly 100x/second, not once per cooldown, see #713 B2), or \
                           use another exit — see GET /v1/observe/zone_exits. Note the crossing \
                           itself is still rate-limited to one attempt per ~10s cooldown, so \
                           stepping back on does not fire instantly. Re-POSTing \
                           /v1/move/zone_cross does NOT clear this: only stepping off every \
                           zone-line region, or zoning, does.",
            }));
        let best_effort = gs.zone_cross_plan
            .filter(|p| p.resolution.is_best_effort())
            .map(|p| serde_json::json!({
                "reason": p.resolution.token(),
                "requested_zone_id": p.requested_zone_id,
                "region_index": p.index,
                "detail": "the zone line this cross is walking to carries no advertised \
                           destination, so the DESTINATION IS THE SERVER'S TO PICK and the client \
                           does not know it — including when you named a zone_id in the request. \
                           The crossing is best effort: it may land you somewhere other than the \
                           zone you asked for. Read player.zone after arriving to find out where \
                           you actually went, and note it is PROVISIONAL for a moment \
                           (player.position_provisional). Describes the MOST RECENT zone_cross \
                           resolution in this zone — NOT necessarily one still in flight. It is \
                           cleared only by the next /v1/move/zone_cross resolution and by zoning; \
                           nothing retires it when the walk arrives, is blocked, or is stopped, so \
                           it CAN be non-null at the same time as zone_cross_stopped (#713 review \
                           round 2, B1 — measured). Do not read it as 'a crossing is in progress': \
                           read nav_state for that.",
            }));
        (stopped, best_effort)
    };
    // #616 (agent-honesty): terminal background-worker failures. `null` while healthy. Before this
    // wiring, a panic in either worker was made honest INTERNALLY (App stopped lying to itself about
    // its own state) but never reached this endpoint — so a driving agent polling here saw nothing
    // different from a worker that was still quietly working, exactly the failure mode #616 exists to
    // remove, just one hop further out. These are the SAME `Arc`s the app thread writes (see their
    // doc comments on `HttpState`), not a re-derivation — nothing here computes a verdict, it only
    // relays the one the app already reached.
    //   - `common_assets_failed`: a panic in the common-asset-loader, OR (independent of #616,
    //     pre-existing behavior) the loader finishing normally with no usable asset set and no cached
    //     fallback. Either way the client is stuck showing this on the loading screen — see
    //     `poll_sync` in `src/app.rs` for why only the panic case additionally clears `loading`.
    //   - `model_sync_dead`: the model-sync worker has stopped for any reason (panic, login failure,
    //     or its channel closing) and will not run again this session — on-demand race-model syncing
    //     is over.
    let common_assets_failed = s.common_assets_failed.lock().unwrap().clone();
    let model_sync_dead = s.model_sync_dead.lock().unwrap().clone();
    // #634 (agent-honesty): the `eq-net` thread — the Model, and the sole writer of every world field
    // in this response — has ended. `null` while it is running. Non-null means the `player`/`zone`/
    // entity/health values above are a FROZEN snapshot that will never update again, no matter how
    // plausible they look. `snapshot_age_ms` already exposes the staleness; this exposes its
    // TERMINALITY, which no age can: a 5-second-old tick is equally consistent with a busy loop and
    // with a thread that no longer exists. Read them together — age says "is this stale?",
    // `net_thread_dead` says "will it ever un-stale?" (no).
    // Only the PROSE half of the typed state reaches the wire (#890): the JSON contract is
    // "`null` while running, a reason string once it has ended", unchanged. The discriminant is
    // for code that must branch (see `lifecycle::exit_body`), not for an agent to parse.
    let net_thread_dead = s.net_thread_dead.lock().unwrap_or_else(|e| e.into_inner())
        .as_ref().map(|d| d.reason().to_string());
    // #816 (agent-honesty): the client-synthesized "to " map-label fallback entries
    // `ActionLoop::sync_zone_points` merges into `zone_points` (`/v1/observe/zone_entrances`) come
    // from a `.txt` file read that can fail the same way `.wtr` region-data reads can (#762/#803):
    // missing file, or present-but-unreadable. `null` while the last load for the CURRENT zone
    // succeeded (or none has run yet). Non-null means this zone's map-label fallback exits are
    // UNKNOWN, not confirmed absent — `zone_points`/`zone_entrances` may be missing entries a
    // successful load would have contributed. This is deliberately NOT a 503 gate the way
    // `zone_region_data_unavailable` is for `/zone_exits`: server-advertised zone points (the
    // primary source `zone_entrances` serves) are unaffected, so refusing the whole endpoint over a
    // purely-additive fallback failing would be a strictly worse answer than disclosing the gap
    // here and still serving what IS known.
    let zone_map_load = s.world.zone_map_load.lock().unwrap().clone().map(|e| serde_json::json!({
        "reason": e.as_str(),
        "detail": e.to_string(),
    }));
    let (guild_name, guild_id, guild_rank) = {
        let g = s.guild_slots.guild.lock().unwrap();
        (g.guild_name.clone(), g.guild_id, g.guild_rank)
    };
    // Built here as locals (not inline in the big `json!` below) so the player object's macro
    // expansion stays under serde_json's recursion limit — the object is large and each inline
    // nested `json!` deepens it. `elapsed_ms` / `ago_secs` are still measured at read time (#343).
    let casting = player.casting.as_ref().map(|c| serde_json::json!({
        "spell_id":   c.spell_id,
        "spell_name": c.spell_name,
        "cast_ms":    c.cast_ms,
        "elapsed_ms": c.started.elapsed().as_millis() as u64,
    }));
    let last_cast = player.last_cast.as_ref().map(|o| serde_json::json!({
        "spell_id":   o.spell_id,
        "spell_name": o.spell_name,
        "outcome":    o.outcome,
        "text":       o.text,
        "ago_secs":   o.at.elapsed().as_secs(),
    }));
    // #336: the last consider of ANY spawn (target or not) — see `last_consider` field doc on
    // `LastConsiderView`. `ago_secs` measured at read time, same rule as `casting`/`last_cast`.
    let last_consider = player.last_consider.as_ref().map(|c| serde_json::json!({
        "spawn_id": c.spawn_id,
        "name":     c.name,
        "con_name": c.con_name,
        "attitude": c.attitude,
        "level":    c.level,
        "ago_secs": c.at.elapsed().as_secs(),
    }));
    let player_levitating = player.levitating;
    let player_run_mode = player.run_mode;
    // #724/#817 — the stuck-and-cannot-free disclosure. `PlayerHoldView` is not `Copy` (it carries
    // a `&'static str` reason plus a `f32`/`&'static str` detail, both trivially `Clone`), so this
    // is a clone rather than the `Copy` bind `player_levitating`/`player_run_mode` use above; bound
    // here for the same reason as those — the `json!` literal below is already at serde_json's
    // recursion limit, so this is attached with `player.insert` after it, not inlined into it.
    let player_hold = player.hold.clone();
    // #776/#801 — the trapped-swimmer disclosure. Bound here rather than inlined below for
    // the same reason `levitating`/`run_mode` are: the `json!` literal is at its recursion
    // limit. Serialised through its own `PlayerAfloatStallView` so the field names, the two
    // published thresholds and the `detail` string are defined in ONE place (that type), not
    // re-spelled here where they could drift from it.
    let player_afloat_stall = player.afloat_stall.clone();
    let mut out = serde_json::json!({
        "player": {
            "name":       player.name,
            "zone":       player.zone,
            // #335/agent-honesty: true means the last zone change timed out and `zone` above is empty
            // on purpose — we are not confidently in any zone (see PlayerState::zone_in_failed).
            "zone_in_failed": player.zone_in_failed,
            // Guild identity (#295): empty name / id 0 = not in a guild.
            "guild":      guild_name,
            "guild_id":   guild_id,
            "guild_rank": guild_rank,
            "race":       player.race,
            "class":      player.class,
            "level":      player.level,
            "pos":        [player.pos_east, player.pos_north, player.pos_up],
            "heading_ccw": player.heading_ccw,
            "heading_cw":  player.heading_cw,
            "server_corrections": player.server_corrections,
            "currency":    currency_json(player.coin),
            // #361: false means a merchant buy is in flight/unconfirmed or a detected desync hasn't
            // been re-verified yet — `currency` above may not match the server's real balance.
            "coin_verified": player.coin_verified,
            "hp_pct":      player.hp_pct,
            "hp":          player.cur_hp,
            "hp_max":      player.max_hp,
            "mana_pct":    player.mana_pct,
            "mana":        player.cur_mana,
            "mana_max":    player.max_mana,
            "xp_pct":      player.xp_pct,
            "spawn_id":    player.player_id,
            "target_id":   player.target_id,
            "target_name": player.target_name,
            "target_hp_pct": player.target_hp_pct,
            // #292/#409: the last consider's result for the CURRENT target — difficulty tier,
            // attitude enum, and the target's actual level. The PlayerState projection already
            // computes these (gated on a live target_id); they MUST be surfaced here or an agent
            // asking "how tough / what attitude is my target" gets a confident null even though the
            // consider succeeded (the exact #409 agent-honesty regression — the con reached the
            // GameState but never the JSON).
            "target_con":      player.target_con,
            "target_attitude": player.target_attitude,
            "target_level":    player.target_level,
            // Death state for a headless agent (#284/#406). `dead` = currently slain (held until
            // POST /v1/lifecycle/respawn); `killed_by` + `died_ago_secs` persist for a window after
            // death (through a respawn). These are the documented way an agent detects it died and
            // must revive — omitting them let a slain character report `dead: null` forever while the
            // "You have been slain" chat line fired, i.e. a lie by omission (#406). All computed in
            // the projection at read time.
            "dead":          player.dead,
            "killed_by":     player.killed_by,
            "died_ago_secs": player.died_ago_secs,
            // Connection health, all computed at READ time (#8, #343). Three independent failures,
            // three independent signals — a frozen world can no longer masquerade as a live one,
            // because nothing has to be RUNNING for these to be right:
            //   connected          — is the LINK up? (false after CONN_STALE_SECS with no datagram)
            //   link_age_ms        — since any inbound datagram, session ACKs included.
            //   last_packet_age_ms — since the last WORLD update. Reaches 40s+ on an idle session
            //                        with a perfectly healthy link, so do NOT read it as a
            //                        disconnect — that's what `connected` is for.
            //   snapshot_age_ms    — since OUR network thread last ticked. If this is large, every
            //                        other field in this payload is stale and must not be trusted.
            //   world_responsive / last_world_response_ms — #371: is the WORLD alive, not just the
            //                        socket? Attached to this "player" object just below (kept out of
            //                        this literal only because it is already at the json! recursion
            //                        limit). See there for the contract.
            "connected":          health.connected,
            "link_age_ms":        health.link_age_ms,
            "last_packet_age_ms": health.last_packet_age_ms,
            "snapshot_age_ms":    health.snapshot_age_ms,
            // Navigation (#166, #337). `nav_state` is the state; `nav_reason` is the machine-readable
            // WHY behind a terminal one. The pair exists because the old single overloaded `blocked`
            // could not tell an agent whether the goal was unreachable, whether the planner had
            // simply given up, or whether the walker was physically wedged — so an unreachable goal
            // presented as a silent permanent freeze, which disguised the real nav root cause for
            // months. See docs/http-api.md ("Navigation state") for the full contract.
            //   navigating_stalled — #851: a route IS committed and the walker is NOT executing it
            //                      (no progress on either channel for >= ~3s). In-progress, not
            //                      terminal: it is in stall/back-off/re-path recovery. Read
            //                      `nav_stall` for how long and how many re-paths are left. This
            //                      state exists because `navigating` used to cover it for ~32s.
            //   no_path          — DEFINITIVE: no route exists (nav_reason: goal_not_walkable |
            //                      search_closed | start_isolated | no_geometry). Pick another goal.
            //   search_exhausted — the planner GAVE UP (search_node_cap). This is
            //                      "I don't know", NOT "no". Try a nearer waypoint.
            //   blocked          — a route exists but the walker physically cannot follow it.
            //   blocked          — a route exists but the walker physically cannot follow it
            //                      (nav_reason: walker_stalled | local_no_way_through |
            //                      fall_would_be_lethal). `local_no_way_through` means the FINE 2u
            //                      tier CLOSED its 40u window without finding a way along the coarse
            //                      corridor — the corridor is genuinely not threadable here, which is
            //                      a different fact from "the walker slid into something" (#382).
            "nav_state":   nav.state,
            "nav_reason":  nav.reason,
            // Spellcasting (#348). `casting` is non-null ONLY while our own cast bar is running;
            // `last_cast` is how the previous cast ended (cast_completed / cast_interrupted /
            // cast_fizzled / cast_failed, plus cast_ended_unexplained — the client's INFERENCE when
            // the server ended the cast without ever saying why) and survives it. Before this,
            // casting was tracked internally and published NOWHERE — an agent could not tell a spell
            // that landed from one that fizzled, was interrupted, or never started. The same
            // transitions are pushed onto /v1/events/combat as they happen.
            //
            // `elapsed_ms` / `ago_secs` are measured HERE, at read time — the projection above
            // carries the raw `Instant`s and never measures them. Same rule as `health()`: an age is
            // only true at the moment it is read (#343).
            "casting":     casting,
            "last_cast":   last_cast,
        },
        // Nav footing verification for THIS zone (#375, D-2). `null` when every standable surface so
        // far faced UP (properly wound); an object naming the winding-blind (inverted-art) ground when
        // nav has answered from a down-facing surface (see `nav_support` above). RENAMED from the old
        // `nav_degraded`/`inverted_floor_art`, whose mechanism (the column_bottom valve) D-2 deleted.
        "nav_support": nav_support,
        "nav_tight": nav_tight,
        // Is this client's model of the CURRENT ZONE actually loaded? Gate every world-shaped
        // conclusion on `zone_assets.state == "ready"` (#579). See the comment where it's built.
        "zone_assets": zone_assets,
        // #713 item 1 — the auto-cross has STOPPED retrying at a zone-line region (`null` while it
        // has not). The bound exists because an unbounded refire is a server-side anomaly event
        // every 10s for as long as the character stands there; this field exists because replacing
        // an infinite retry with a SILENT stop is worse — an agent would wait forever for a
        // crossing that will never be attempted again. See where it is built.
        "zone_cross_stopped": zone_cross_stopped,
        // #713 item 2 — the last zone_cross resolution DEGRADED to the #683 best-effort fallback:
        // it is walking to a line whose destination only the server knows. `null` when the last
        // resolution had an advertised destination (or there has been none since zoning). Nothing
        // false was ever asserted here — the degradation was simply undetectable, being prose in
        // the message log. See where it is built.
        "zone_cross_best_effort": zone_cross_best_effort,
        // Terminal background-worker failures (#616). `null` while healthy — see the comment where
        // these are built, above.
        "common_assets_failed": common_assets_failed,
        "model_sync_dead": model_sync_dead,
        // #634: the network thread itself is dead — `null` while it is alive. When this is non-null,
        // EVERY other field in this payload is a frozen final snapshot. See where it is built.
        "net_thread_dead": net_thread_dead,
        // #816 — see where it is built, above. `null` while this zone's map-label fallback exits
        // (a purely-additive contribution to `zone_points`/`zone_entrances`) loaded fine.
        "zone_map_load": zone_map_load,
        // The FINE 2u STEERING tier (#382). `null` while it is healthy (a complete fine route to its
        // carrot) or has not yet answered. Non-null when the tier that is actually steering the
        // character cannot see a way through the next 40u — and it says WHICH kind of cannot:
        //
        //   no_way_through — the 40u window's frontier CLOSED. There is genuinely no way along the
        //                    committed coarse corridor from here (the 8u grid skimmed something).
        //                    Falsifiable, and *local*: it says nothing about whether the GOAL is
        //                    reachable. The walker keeps steering on the coarse route and re-plans it
        //                    (#246).
        //   exhausted      — the search was CUT SHORT (node cap). "I DON'T KNOW", not "no".
        //   planner_dead   — the fine worker thread died. Steering has degraded to coarse-only for
        //                    the rest of the session; the walker keeps walking, but with 8u detail.
        //
        // This field exists because until #382 the fine tier's failure was INVISIBLE: it ran under a
        // 150ms wall clock, so "did not reach the carrot" meant either "impassable" or "ran out of
        // clock" with no way to ask which, and `nav_state` said a confident `navigating` throughout.
        // The clock is gone; the ambiguity went with it.
        "nav_local": nav_local,
        // #851. See where it is built, above. Non-null EXACTLY when `nav_state` is
        // `navigating_stalled` — the honest middle state between "walking your route" and "gave up".
        "nav_stall": nav_stall,
        // WORKER-scoped fine-planner liveness (#766 review B3; scope corrected from "session" by
        // round-6 review B12 — the latch is cleared by `Walker::new` as it spawns a replacement, and
        // it reads as session-scoped from outside only because exactly one fine worker is built per
        // process — a premise with a TRIPWIRE under it rather than a pin: `eqoxide-nav`'s
        // `walker::tests::exactly_one_production_fine_worker_is_built_in_the_tree_787` (#787) scans
        // the tracked tree and names THIS comment among the four sentences that go false when a
        // second construction SITE is added unmarked. It counts sites, not workers. It stays green
        // for one site executed twice — the relogin shape — which was measured, not assumed, and it
        // is evadable by a handful of documented spellings listed on the test. Read it as a cheap
        // alarm on the likely edit, not as proof. `docs/http-api.md` keeps the
        // agent-facing "session-scoped" name and says why).
        // `nav_local` above is a PER-GOAL
        // verdict and #766 retires it with the goal — correct for `no_way_through` / `exhausted`, and
        // wrong for `planner_dead`, which is a latched client fault, not a statement about a goal.
        // Carried here it survives every retirement, so an agent BETWEEN goals — which is when it
        // polls this endpoint to decide what to do next — can still see that its steering has
        // degraded to the coarse 8u route and that nothing on any nav route recovers it (a claim
        // about WRITERS, which is what the tree guarantees; "permanently" was strictly stronger and
        // false in the same breath as this block's own "the lifetime is the WORKER's" — round-6
        // review B14). `planner_dead` still appears in `nav_local`
        // while a route is committed; this is the channel that does not vanish when the route does.
        //
        // Always present, in BOTH states, unlike the `null`-when-healthy fields around it: checking
        // your own health needs a readable "alive", not merely the absence of "dead" — which is
        // indistinguishable from an older client that never had the field.
        "nav_local_planner_dead": nav.local_planner_dead,
        // The agent-honesty payload behind a terminal `no_path` (#378 Phase 2). `null` when there is
        // nothing to report. `goal` (if present) is the DEFINITIVE "your goal itself cannot be stood
        // at"; `frontier` is the hazard at the search's CLOSEST APPROACH — one blocking fact, named
        // as such (not `reason`), not necessarily the only one. Top-level (not under `player`) so the
        // large player object stays within serde_json's macro recursion limit.
        // GOAL IDENTITY (#349). `nav_state`/`nav_reason` (under `player`) are the status *of this
        // goal* — never of an earlier one. `nav_goal_id` is the monotonic generation stamped by the
        // accepting POST (echoed in its response body); `nav_goal` is that goal's `[x,y,z]` (null for
        // idle/stop, or a zone_cross whose concrete line isn't resolved yet). A terminal
        // `arrived`/`no_path`/`blocked` is trustworthy ONLY for the `nav_goal_id` reported here: a
        // fresh `POST /goto` bumps this and resets `nav_state` to `pending` atomically, so a read can
        // never attribute the previous goto's outcome to the new one. Top-level (not under `player`)
        // because that object is already at serde_json's macro recursion limit.
        "nav_goal_id": nav.goal_id,
        "nav_goal": nav.goal,
        "nav_blocked_by": nav_blocked_by,
        // #543 — the pads nav DECLINED to route through, disclosed alongside the failure that made
        // them relevant. `null` unless nav is in a terminal no-route state AND there is at least one
        // such pad. Every destination in it is ADVERTISED, never verified, and the client keeps no
        // memory of where a pad landed: this is an OPTION offered to the agent, not a route. See the
        // comment where it is built for why it is not folded into `nav_reason`.
        "nav_declined_pads": nav_declined_pads,
        // #336: the last consider of ANY spawn (target or not) — `{spawn_id, name, con_name
        // (difficulty tier), attitude, level, ago_secs}`, or null if nothing has been considered
        // this session. Top-level (not under `player`, which is already at serde_json's macro
        // recursion limit) — same reason as `nav_blocked_by` above. This is what lets a standalone
        // `POST /v1/combat/consider {"id":N}` on a spawn that is deliberately NOT the current target
        // be read back: `player.target_con`/`target_attitude`/`target_level` only ever describe the
        // CURRENT target (#330) and stay null for a non-target consider.
        "last_consider": last_consider,
        // The PER-ROUTE clearance tier the CURRENT route needed (#378 Phase 2 / design §4c):
        // `minimum` (tight, no margin — riskier) | `preferred` (roomy) | null (no route committed).
        // Distinct from the zone-lifetime `nav_tight` counter — this is the route being walked now.
        "nav_tier": nav_tier,
        // Per-phase frame timings (ms, EMA-smoothed); all zero unless --profile / EQ_PROFILE=1.
        "frame_profile": frame_profile,
        // #797 — models whose skin joint count EXCEEDED the renderer's animation cap and were
        // downgraded to the static (unskinned) render arm this session. Keyed by loaded file base
        // name. Always present, `{}` rather than `null`, so a caller that greps and finds the key
        // MISSING knows it is talking to a client too old to report this at all — the same
        // distinction `nav_local_planner_dead` draws above.
        //
        // What `{}` does and does not say (#900 review r1, finding 6): it is an exact mirror of the
        // renderer's own map, which is written only by `ensure_character_model` (via
        // `observe_skin_fit`) inside `render_frame`, is insert/update-only, and is published
        // immediately after that same `render_frame` call — so `{}` is never a STALE non-empty
        // state going unreported. But it does conflate two situations an agent cannot separate from
        // this field alone: "every character model loaded so far fits the cap" and "no character
        // model has been loaded yet" (loading screen, zoning, nothing in view). Read `{}` as "no
        // downgrade has been recorded so far this session", NOT as "the model you are asking about
        // animates" — that second reading is only justified once you know the model is on screen.
        // Each entry is `{joint_count, key_collision}`:
        //   joint_count   — the joint count that triggered the downgrade (the MOST RECENT one, if
        //                   this key has been (re)loaded more than once this session).
        //   key_collision — true iff two files that hash to the SAME base-name key were BOTH loaded
        //                   this session (eqoxide#848). When true, `joint_count` is not reliably
        //                   attributable to either file — see docs/http-api.md.
        // See `eqoxide_renderer::renderer::record_skin_cap_downgrade` for how this map is built.
        "skin_cap_downgrades": skin_cap_downgrades,
        // EVERY field in this block describes ONE frame — the one named by `drawn_frame` — and not
        // "now" (#867). The render loop publishes the whole struct in a single write, only after
        // `render_frame` has actually encoded a frame; on a tick that returns early from the
        // `wgpu::SurfaceError` match (`Lost`/`Outdated`/`Timeout`) nothing is published and the
        // previous values stay. `drawn_frame` is `null` before the first frame is ever drawn — the
        // startup seed `main.rs` publishes, which the API serves through GPU init and the first zone
        // load.
        //
        // READ `drawn_frame`/`drawn_age_ms` BEFORE TRUSTING ANYTHING ELSE HERE. The staleness is
        // unbounded: `about_to_wait` stops requesting redraws 300 ms after the last activity, so a
        // surface that keeps returning `Outdated` (a minimised or occluded window, not just a
        // resize) freezes this whole block indefinitely, with every field looking exactly as it does
        // on a healthy tick. `snapshot_age_ms` above is the NETWORK clock and does NOT age this —
        // it reads fresh throughout a rendering stall. `drawn_age_ms` is computed when this response
        // is encoded; `drawn_frame` unchanged across two reads means nothing was drawn in between.
        //
        // #852: `azimuth_deg`/`elevation_deg`/`radius`/`focus`/`mode` are the orbit's DESIRED
        // framing as of that frame — NOT necessarily where it was rendered from. In tight geometry
        // the render loop pulls the eye in toward `focus` until it clears collision, and that
        // pull-in does not touch those fields. `eye` is the position that frame was actually
        // rendered from — see `camera_state::resolve_camera_eye` and
        // `eqoxide_ipc::CameraSnapshot`. Use `eye`, not a `radius`-derived distance, for anything
        // about what was drawn. `occluded` says whether pull-in fired on the frame `eye` describes;
        // `still_blocked` says whether, even after pull-in, `eye` was still not fully clear of
        // collision (a degenerate case that must be reported, not silently rendered).
        "camera": {
            "azimuth_deg":   cam.azimuth.to_degrees(),
            "elevation_deg": cam.elevation.to_degrees(),
            "radius":        cam.radius,
            "focus":         cam.focus,
            "mode":          cam.mode,
            "eye":           cam.eye,
            "occluded":      cam.occluded,
            "still_blocked": cam.still_blocked,
            "drawn_frame":   cam.drawn_frame,
            "drawn_age_ms":  cam.drawn_at.map(|t| t.elapsed().as_millis() as u64),
        },
    });
    // #371 — attached here (not inside the literal above, which is already at the json! recursion
    // limit). `connected: true` only proves the SOCKET ACKs; a zone that is still ticking but not
    // servicing our packets (a stuck per-client dispatch / script, or a very slow tick) keeps ACKing
    // while producing no application output for us, which is indistinguishable from a quiet zone by
    // the passive clocks. An active liveness probe (a request the zone main loop must service)
    // settles it:
    //   world_responsive        — false ONLY when a probe went unanswered past PROBE_TIMEOUT_SECS on
    //                             a still-ACKing link. An idle-but-alive zone stays true (the probe
    //                             is answered). USE THIS, not last_packet_age_ms, to judge whether the
    //                             world is unresponsive. True before the first probe fires (no verdict
    //                             yet). NOTE: this catches the still-ticking-but-unresponsive case; a
    //                             TOTAL zone freeze stops ACKs too and is already `connected: false`.
    //   last_world_response_ms  — since the world last PROVED it processed something for us (a probe
    //                             reply or spontaneous packet), whichever is fresher.
    if let Some(player) = out.get_mut("player").and_then(|p| p.as_object_mut()) {
        // #543/#660 B2 — is `pos` (and, during a crossing, `zone`) actually the SERVER's word?
        // `false` normally. `true` from the moment a zone-line crossing applies a locally-derived
        // position — the advertised arrival, written before the server has said anything so the
        // character leaves the trigger region — until the server tells us where we really are.
        //
        // This exists because the crossing window served a well-formed, confident, mutually
        // inconsistent answer: `zone: "qeynos"` beside a qeynos2 `pos`, with nothing marking it. The
        // OP_ZoneChange echo settles WHICH ZONE (and flips `zone` there); the position does not
        // arrive until the new zone's first update, and that gap is the falsehood. A prose warning in
        // the message log is not enough — one was watched being evicted from the ring by ambient
        // chatter while the fields stayed wrong and unmarked.
        //
        // `nav_declined_pads` tells an agent to take a pad and then read `zone`/`pos` to find out
        // where it went. THIS is the field that says "not yet". `crossing_pending_ms` is measured
        // HERE, at read time (#343: never cache an age); `null` when not provisional.
        // (Attached by insert, not in the literal above, which is at serde_json's recursion limit.)
        player.insert("position_provisional".into(),
            serde_json::json!(prov.is_some()));
        player.insert("crossing_pending_ms".into(),
            serde_json::json!(prov.map(|t| t.elapsed().as_millis() as u64)));
        player.insert("world_responsive".into(),       serde_json::json!(health.world_responsive));
        player.insert("last_world_response_ms".into(), serde_json::json!(health.last_world_response_ms));
        // #529/#586/#598: Levitate buff up = gravity off. It changes what movement means (`pos` is a
        // height the character will NOT fall from, and the controller stops applying gravity), so it
        // must be readable here — a projection field that never reaches the JSON is the #409 failure
        // mode all over again. THREE-VALUED (`player_levitating` is `Option<bool>`): `true`/`false`/
        // `null`, where `null` = UNKNOWN (unresolved buff, spell table missing/truncated) — serde
        // renders `None` as an explicit `null`, never `false` and never an omitted key, so the
        // agent-honesty contract now holds at the API boundary, not just the type boundary (#598).
        // NOTE: this is the levitate *buff* state, NOT a general gravity flag (GM `#flymode 1` reads
        // `false`). Attached here, not in the literal above, which is at the json! recursion limit.
        player.insert("levitating".into(),             serde_json::json!(player_levitating));
        // #625 — our own last-SENT run/walk toggle intent (`true` = run, `false` = walk).
        // `OP_SetRunMode` has no server ack, so this is NOT a confirmation of what the server
        // granted — exactly the same epistemic level as `sitting`/`auto_attack` elsewhere in this
        // payload. Attached here (not in the literal above, which is at the recursion limit).
        player.insert("run_mode".into(),               serde_json::json!(player_run_mode));
        // #724/#817 — HOLD: the movement controller has stopped the body and cannot resume (embedded
        // in geometry with push-out exhausted, or hanging at the underworld floor with no recovery
        // position). `null` for a healthy character, INCLUDING one simply standing still — `pos` and
        // `nav_state` read identically in both cases, and `nav_state.stuck_ticks` only advances while
        // a `/goto` is actively driving, so a summoned-into-rock character produced no observable at
        // all through this API before this insert existed (#724).
        //
        // `PlayerState::hold` (see its doc) was computed, mirrored into `GameState` every NET tick
        // (from a view the render thread republishes per rendered frame — see its doc), and
        // covered by tests since #724 landed — and reached NO response body, because `PlayerState` is
        // an internal projection `get_debug` never serialises whole. That is #817: the type existed,
        // the value was correct, and an agent grepping this response for `hold` found nothing and
        // could only conclude it was not stuck, which is the exact confident-falsehood shape this
        // project ranks above a crash. This insert is what makes it reachable.
        //
        // ALWAYS PRESENT, `null` when there is no hold (`PlayerState::hold` carries no
        // `skip_serializing_if`, and `serde_json::json!(None::<T>)` renders an explicit null) — same
        // contract as `levitating`/`afloat_stall`. Be precise about what that buys, because this
        // PR's own measurement bounds it: an omitted key reads as "this client is too old to report
        // the state" ONLY to a reader that checks key PRESENCE or greps the raw body. It does not
        // read that way through `v["player"]["hold"]`, the obvious access path — `serde_json`
        // returns `Value::Null` for an absent key just as it does for an explicit one, which is
        // exactly what made the original `is_null()` assertion in
        // `afloat_stall_reaches_the_debug_json_801` VACUOUS (#810 round 2) and why the test below
        // has to use `contains_key`. So the guarantee this insert provides is to a grepping or
        // presence-checking agent; `docs/http-api.md` states it that way too.
        player.insert("hold".into(),                   serde_json::json!(player_hold));
        // #776/#801 — AFLOAT STALL. The character is in water, a driver is asking it to swim
        // horizontally, and it has not moved. Before this the state had NO observable at all: a
        // floating body never enters the depenetration net, so a swimmer sealed in a pocket was
        // indistinguishable, in THIS response, from one crossing a lake: barely-moving `pos` and
        // nothing else. (`in_water`/`on_ground` are controller state and are not keys here, so they
        // could not be consulted either.) Every field an agent could GET said "swimming normally"
        // for ever, which is the silent-wrong-answer class this project ranks above crashes.
        //
        // ALWAYS PRESENT, `null` when there is no stall (`serde_json::json!(None::<T>)` renders an
        // explicit null, and `PlayerState::afloat_stall` carries no `skip_serializing_if`). An
        // omitted key is a lie too — the agent cannot tell "no stall" from "client too old to
        // know" — which is exactly the `levitating` contract three lines up.
        //
        // This insert is the SEVENTH file on the path, and #810's round-1 review is the reason it
        // exists: the other six were all correct and the value still reached no response body,
        // because `PlayerState` is an internal projection that no handler serialises whole. A
        // projection field that never reaches the JSON is the #409 failure mode, and a test that
        // serialises `PlayerState` directly cannot see it. `afloat_stall_reaches_the_debug_json_801`
        // goes through this handler for that reason.
        player.insert("afloat_stall".into(),           serde_json::json!(player_afloat_stall));
        // #612 — OUTBOUND honesty. Everything else in this payload is about what the server told us;
        // these four are about what WE failed to say. Every send error used to be discarded
        // (`let _ = self.socket.try_send(..)`), so a datagram that never left the machine was
        // indistinguishable from one the server received — an agent issuing a command had no way,
        // even in principle, to learn that it had not gone out.
        //   send_failures            — datagrams BUILT but not put on the wire, since process start.
        //                              0 IS the expected healthy reading since #641, which gave the
        //                              qeynos zone-in burst of 283 two recovery paths (an immediate
        //                              direct send(2) retry, and a deferral queue for control
        //                              datagrams), both counted BELOW rather than here. So this now
        //                              means what its name says: THE DATAGRAM NEVER REACHED THE WIRE
        //                              AND NOTHING WILL RE-SEND IT. That covers more than a kernel
        //                              refusal — non-transient errors (EMSGSIZE, a dead socket),
        //                              queue-overflow evictions, and datagrams still queued when a
        //                              session ends all land here too.
        //                              WHICH mechanism refused a send is NOT knowable from these
        //                              counters — see send_wouldblock_rescued below. What IS
        //                              established is the trigger: CPU starvation of the client's
        //                              tokio io driver.
        //   send_wouldblock_rescued  — datagrams a WouldBlock refused that an immediate direct
        //                              send(2) then accepted (#641). NOT failures — they reached the
        //                              wire. An UPPER BOUND on tokio's synthetic-WouldBlock case,
        //                              NOT a measurement of it: a kernel refusal whose transmit
        //                              buffer drained in between looks identical. Load signal.
        //   send_deferred            — how many DATAGRAMS (not refusal events) a transient refusal
        //                              caused to be queued for retry on a later tick instead of
        //                              being dropped; control datagrams only (#641). Normally they
        //                              go out ~10ms late — but this is NOT disjoint from
        //                              send_failures: one later abandoned (queue overflow, session
        //                              end) is counted in both. send_failures stays the loss number.
        //   send_failures_unretried  — the subset with no client-side retransmit of that datagram.
        //                              TWO very different classes share it:
        //                                * session-layer control (ACK / OutOfOrderAck / keepalive /
        //                                  SessionRequest / SessionDisconnect) — 7-byte datagrams.
        //                                  Lost ACKs stall the server's ordered window, not our
        //                                  position. (This was the whole of the #641 burst.)
        //                                * unreliable OP_ClientUpdate position updates — only these
        //                                  mean the server's idea of where you are may be stale.
        //                              The size distribution is the discriminator; the counter alone
        //                              cannot tell them apart, so do not diagnose from it alone.
        //                              The complement is the reliable stream, which `poll_resend`
        //                              re-sends verbatim until ACKed — but ONLY while the session
        //                              lives; see reliable_abandoned. So this is NOT a complete count
        //                              of lost commands, and must not be read as one.
        //   reliable_abandoned       — un-ACKed reliables left outstanding when a session ENDED
        //                              (zone handoff, world reconnect, zone-in failure, clean
        //                              shutdown). The next session's window starts empty, so these
        //                              are not retransmitted. This is the reliable stream's loss
        //                              channel, and the one `send_failures_unretried` cannot see.
        //                              MEASURED 0 across three clean zone handoffs → a nonzero value
        //                              DURING PLAY is signal, not routine noise. Clean shutdown is
        //                              the measured exception (4 and 8 on two live exits), which no
        //                              agent can observe anyway. The CAUSE of that count is not
        //                              established — see NetHealth::reliable_abandoned; do not
        //                              invent one.
        //                              Since #642 this ALSO covers a server-side drop: the client now
        //                              notices one (OP_SessionDisconnect/OP_OutOfSession or a closed
        //                              socket → `session_drop`) and tears the gameplay phase down,
        //                              which drops the stream → abandon_outstanding. See `session_drop`.
        //   session_drop             — #642: null while live; a snake_case cause string
        //                              (server_disconnect / out_of_session / socket_closed) once the
        //                              client has POSITIVELY OBSERVED a server-side drop. When set,
        //                              `connected` above is already forced false — the immediate,
        //                              explicit signal, vs `connected`'s CONN_STALE_SECS silence timer.
        //   last_send_error          — ErrorKind of the most recent one ("WouldBlock", …), or null.
        //   last_send_error_age_ms   — ms since it, measured at read time. Distinguishes a single
        //                              old blip from an ongoing failure.
        player.insert("send_failures".into(),           serde_json::json!(health.send_failures));
        player.insert("send_wouldblock_rescued".into(), serde_json::json!(health.send_wouldblock_rescued));
        player.insert("send_deferred".into(),           serde_json::json!(health.send_deferred));
        // #656: the ALERT the two counters above were missing — send_wouldblock_rescued/
        // send_deferred are cumulative since process start and can only grow, so nothing before
        // this could tell an agent "starved right now" from "starved once, an hour ago". `true`
        // only while a real send_wouldblock_rescued/send_deferred burst is ongoing (see
        // eqoxide_ipc::send_starved for the exact fire/clear rule); CLEARS on its own once the
        // burst ends, even though send_wouldblock_rescued/send_deferred themselves never go down.
        player.insert("send_starved".into(),            serde_json::json!(health.send_starved));
        player.insert("send_failures_unretried".into(), serde_json::json!(health.send_failures_unretried));
        player.insert("last_send_error".into(),
            serde_json::json!(health.last_send_error.map(|k| format!("{k:?}"))));
        player.insert("last_send_error_age_ms".into(),  serde_json::json!(health.last_send_error_age_ms));
        player.insert("reliable_abandoned".into(),      serde_json::json!(health.reliable_abandoned));
        // #642: the machine-readable cause of a POSITIVELY-OBSERVED server-side session drop, or null
        // while the session is live. Attached here, not in the literal above, which is at the json!
        // recursion limit.
        player.insert("session_drop".into(),
            serde_json::json!(health.session_drop.map(|c| c.as_str())));
    }
    Json(out)
}

/// GET /v1/observe/frame — returns the current rendered frame as a PNG.
/// Query params for GET /v1/observe/frame.
#[derive(serde::Deserialize, Default)]
struct FrameQuery {
    /// Opt in to a frame captured while the zone's assets are still loading (#579). Without it, a
    /// mid-load capture is refused with 503 rather than handed over as if it were the zone — a
    /// placeholder ground plane in a PNG is indistinguishable from a genuinely empty zone, and an
    /// agent acted on exactly that confusion in #560. Pass `?allow_pending=1` when the loading
    /// screen itself is what you want to see.
    allow_pending: Option<String>,
    /// #422: a named diagnostic camera angle for THIS capture only — see `resolve_camera_override`
    /// for the full preset table. Mutually exclusive with `pitch`/`yaw`/`distance`.
    preset: Option<String>,
    /// #422: explicit pitch override in degrees (elevation above the horizon; positive looks down,
    /// negative looks up), range -85.0..=85.0. Omitted fields fall back to the live camera's current
    /// value. Mutually exclusive with `preset`.
    pitch: Option<String>,
    /// #422: explicit yaw override in degrees, EQ heading convention (0=north, increasing CCW) — the
    /// SAME convention as `heading_ccw` on `GET /v1/observe/debug` (there is no `/v1/observe/state`
    /// route — `heading_ccw` is emitted by `get_debug` at the `"heading_ccw": player.heading_ccw`
    /// line above, and `docs/http-api.md`'s `yaw` row already cites `/v1/observe/debug`), and
    /// applied ABSOLUTELY (not
    /// relative to the character's current facing, unlike the presets below), so a fixed `yaw` value
    /// always frames the same world direction regardless of which way the character happens to be
    /// facing at capture time. Range -360.0..=360.0. Mutually exclusive with `preset`.
    yaw: Option<String>,
    /// #422: explicit distance override in world units, range 1.0..=2000.0. Mutually exclusive with
    /// `preset`.
    distance: Option<String>,
}

/// The `FrameQuery` field names that must each appear at most once — used by
/// [`parse_frame_query`]'s duplicate-key check (#701). This is exactly the recognized-field set;
/// a duplicated *unrecognized* key (e.g. `?foo=1&foo=2`) is unaffected by this change and keeps
/// its pre-#701 behavior of being silently ignored (`FrameQuery` has no `deny_unknown_fields`,
/// unlike `MessagesQuery`/`EntitiesQuery`).
///
/// NOTE this is a mix of two different subsystems (#701 review, B1): `allow_pending` is the #579
/// zone-assets-readiness bypass flag, not a camera parameter, unlike the other four. It's still
/// listed here — this array only decides which keys the duplicate-detection loop below applies
/// to — but [`duplicate_field_error`] gives it a *different error code* than the camera fields so
/// the JSON response doesn't misclassify which subsystem the caller's mistake was in.
const FRAME_QUERY_FIELDS: [&str; 5] = ["allow_pending", "preset", "pitch", "yaw", "distance"];

/// Build the `(error_code, message)` pair for a duplicated `key` among [`FRAME_QUERY_FIELDS`]
/// (#701 review finding B1).
///
/// `allow_pending` is the #579 zone-assets-readiness bypass flag, not one of the camera-angle
/// params (`preset`/`pitch`/`yaw`/`distance`). Before this function existed, duplicating it got
/// the same `invalid_camera_override` code as duplicating `pitch` — telling an agent its *camera
/// angle* was invalid on a request that contained no camera params at all, sending it hunting
/// through pitch/yaw/preset/distance for a problem that isn't there. That's also a narrow
/// *regression* against pre-#701 behavior, which returned a generic, honestly-unclassified
/// text error for this case and never claimed to know it was about the camera.
///
/// So `allow_pending` gets its own `invalid_query_param` code, distinct from
/// `invalid_camera_override`, which now means exactly what its name says: one of the four camera
/// params. This is additive — every existing consumer keying off `invalid_camera_override` for a
/// camera-field problem keeps seeing exactly that.
fn duplicate_field_error(key: &str) -> (&'static str, String) {
    if key == "allow_pending" {
        (
            "invalid_query_param",
            "duplicate query parameter \"allow_pending\" — pass it at most once".to_string(),
        )
    } else {
        (
            "invalid_camera_override",
            format!(
                "duplicate query parameter \"{key}\" — pass preset/pitch/yaw/distance at most \
                 once each; got \"{key}\" more than once"
            ),
        )
    }
}

/// Parse `GET /frame`'s raw query string into a [`FrameQuery`], by hand rather than via axum's
/// `Query<FrameQuery>` extractor (#701).
///
/// **Why not `Query<FrameQuery>`:** axum's extractor calls `serde_urlencoded::from_str`, whose
/// derived-`Deserialize` visitor rejects a REPEATED key for a scalar field
/// (`?pitch=10&pitch=200`) with `serde::de::Error::duplicate_field` — confirmed by reading
/// `serde_urlencoded-0.7.1/src/de.rs` (it defers to `serde::de::value::MapDeserializer`, whose
/// `visit_map` call lands in `FrameQuery`'s derived visitor, which tracks a `seen` flag per field
/// exactly the way `serde_derive` generates for every struct). Axum surfaces that as its own
/// `QueryRejection` → `FailedToDeserializeQueryString`, which renders as a generic `text/plain`
/// 400 — NOT the JSON shape every other malformed-*value* case on this endpoint returns (see
/// `resolve_camera_override`). An agent parsing the JSON `error` field would get a non-JSON body
/// for this one input class.
///
/// This function detects exactly that one failure mode itself — a duplicated KEY among
/// [`FRAME_QUERY_FIELDS`] — and turns it into a JSON-shaped error via [`duplicate_field_error`]
/// (which also picks the *correct* error code — see B1 there), then falls through to
/// `serde_urlencoded::from_str` (byte-for-byte the same deserialization axum's `Query` would have
/// done) for everything else, so every OTHER malformed-value case (unparseable numbers,
/// out-of-range numbers, unknown presets, unrecognized keys) is completely unchanged.
///
/// Scoped to `/frame` only: no other route's extractor is touched, so no other endpoint's 400/200
/// boundary can move because of this change.
fn parse_frame_query(raw: &str) -> Result<FrameQuery, (&'static str, String)> {
    // `&'static str`, not `String`: each matched key borrows straight from `FRAME_QUERY_FIELDS`
    // (a `'static` array), not from the per-iteration `Cow` — no per-key allocation (#701 review
    // nit).
    let mut seen: HashSet<&'static str> = HashSet::new();
    for (key, _value) in form_urlencoded::parse(raw.as_bytes()) {
        let key: &str = key.as_ref();
        if let Some(&field) = FRAME_QUERY_FIELDS.iter().find(|&&f| f == key) {
            if !seen.insert(field) {
                return Err(duplicate_field_error(field));
            }
        }
    }
    // Defensive fallback: with every duplicate among the recognized fields already caught above,
    // and every `FrameQuery` field an `Option<String>` (so almost any input deserializes), this is
    // not expected to trigger in practice. It isn't guaranteed to be camera-specific either (it
    // isn't tied to any particular field), so — same honesty reasoning as B1 above — it gets the
    // field-agnostic `invalid_query_param` code rather than assuming `invalid_camera_override`.
    serde_urlencoded::from_str::<FrameQuery>(raw)
        .map_err(|e| ("invalid_query_param", format!("could not parse query string: {e}")))
}

/// The state word every `/frame` response carries in `X-Zone-Assets-State` (#595 review nit): a PNG
/// fetched with `?allow_pending=1` is a 200 `image/png` like any other, so without this header a
/// mid-load (or wrong-zone) capture is indistinguishable downstream from a real one.
pub(crate) const ZONE_ASSETS_STATE_HEADER: &str = "x-zone-assets-state";

/// #422: pitch/elevation validation bound, degrees. Symmetric, and short of the true ±90° pole by
/// enough margin (`camera_state::CameraState::apply_orbit_delta`'s own `POLE` guard sits at 89.94°)
/// that `camera_state::eye_and_look`'s `look_at_rh`-style math — which the override path drives
/// DIRECTLY, bypassing every clamp `CameraState` normally applies — cannot degenerate into a
/// parallel eye/up vector and produce a garbage (or NaN) frame.
const PITCH_DEG_RANGE: std::ops::RangeInclusive<f32> = -85.0..=85.0;
/// #422: yaw validation bound, degrees. Generous (a full turn either way) since azimuth is periodic
/// and out-of-range here can only mean a typo, not a geometry hazard.
const YAW_DEG_RANGE: std::ops::RangeInclusive<f32> = -360.0..=360.0;
/// #422: distance validation bound, world units. 1.0 floor keeps the eye off the focus point
/// (degenerate look-at); 2000.0 ceiling is generously past `camera_state::RADIUS_MAX` (500.0) while
/// still catching an obvious typo (e.g. a stray extra digit).
const DISTANCE_RANGE: std::ops::RangeInclusive<f32> = 1.0..=2000.0;

/// Resolve `FrameQuery`'s preset/pitch/yaw/distance fields into a `CameraOverride`, or `None` for
/// "no override" (no params given, or `?preset=default`) — the exact pre-#422 behavior of reading
/// back the already-rendered on-screen frame. `Err` carries a human-readable reason for a 400,
/// returned to the caller BEFORE anything is registered on `s.camera.frame_req` (#422: an invalid
/// request must never produce a capture at all, let alone a silently-wrong-angle one).
///
/// Pure — no I/O, no lock — so every case (valid preset, valid numeric, partial numeric, invalid
/// range, unknown preset, non-numeric, preset+numeric combined, no params) is unit-testable without
/// a running renderer or HTTP stack.
fn resolve_camera_override(
    q: &FrameQuery,
    heading_deg: f32,
    live: &CameraSnapshot,
) -> Result<Option<CameraOverride>, String> {
    let numeric_given = q.pitch.is_some() || q.yaw.is_some() || q.distance.is_some();
    if q.preset.is_some() && numeric_given {
        return Err("preset is mutually exclusive with pitch/yaw/distance — pass one or the other, \
                     not both".to_string());
    }

    if let Some(preset) = q.preset.as_deref() {
        // (pitch_deg, yaw_offset_deg relative to the character's CURRENT heading, distance).
        return Ok(match preset {
            "default"      => None,
            "top_down"     => Some(preset_override(heading_deg,  85.0,   0.0, 200.0)),
            "behind_above" => Some(preset_override(heading_deg,  45.0,   0.0,  70.0)),
            "front"        => Some(preset_override(heading_deg,  20.0, 180.0,  70.0)),
            other => return Err(format!(
                "unknown preset \"{other}\" — valid presets: default, top_down, behind_above, front"
            )),
        });
    }

    if !numeric_given {
        return Ok(None);
    }

    let pitch_deg = q.pitch.as_deref().map(|v| parse_ranged(v, "pitch", PITCH_DEG_RANGE)).transpose()?;
    let yaw_deg   = q.yaw.as_deref().map(|v| parse_ranged(v, "yaw", YAW_DEG_RANGE)).transpose()?;
    let distance  = q.distance.as_deref().map(|v| parse_ranged(v, "distance", DISTANCE_RANGE)).transpose()?;

    Ok(Some(CameraOverride {
        azimuth:   yaw_deg.map(desired_azimuth).unwrap_or(live.azimuth),
        elevation: pitch_deg.map(f32::to_radians).unwrap_or(live.elevation),
        radius:    distance.unwrap_or(live.radius),
    }))
}

/// A named preset expressed the same way the numeric path is: pitch in degrees, a yaw OFFSET in
/// degrees added to `desired_azimuth(heading_deg)` (i.e. relative to the character's current
/// facing — presets are meant to stay correctly oriented no matter which way the character is
/// facing at capture time, unlike the absolute numeric `yaw` param), and a distance in world units.
fn preset_override(heading_deg: f32, pitch_deg: f32, yaw_offset_deg: f32, distance: f32) -> CameraOverride {
    CameraOverride {
        azimuth:   desired_azimuth(heading_deg) + yaw_offset_deg.to_radians(),
        elevation: pitch_deg.to_radians(),
        radius:    distance,
    }
}

/// Parse `v` as an `f32` and check it falls within `range`, with a message naming `field` either
/// way — the 400 body must tell the caller exactly what was wrong, never just "bad request".
fn parse_ranged(v: &str, field: &str, range: std::ops::RangeInclusive<f32>) -> Result<f32, String> {
    let parsed: f32 = v.trim().parse().map_err(|_| {
        format!("{field}=\"{v}\" is not a number")
    })?;
    if !range.contains(&parsed) {
        return Err(format!(
            "{field}={parsed} is out of range [{}, {}]", range.start(), range.end()
        ));
    }
    Ok(parsed)
}

async fn get_frame(State(s): State<HttpState>, RawQuery(raw): RawQuery) -> Response {
    // #701: parsed by hand via `parse_frame_query` (see its doc), NOT axum's `Query<FrameQuery>`
    // extractor — so a duplicated `?pitch=10&pitch=200` gets the same `invalid_camera_override`
    // JSON 400 as every other malformed-value case below, instead of axum's generic text/plain
    // rejection. Done first, same as an extractor would run, so a malformed query string still
    // short-circuits before the zone-assets-readiness gate just below (unchanged ordering).
    let q = match parse_frame_query(raw.as_deref().unwrap_or("")) {
        Ok(q) => q,
        // #701 review (B1): the error code comes FROM `parse_frame_query`/`duplicate_field_error`
        // now, not a hardcoded "invalid_camera_override" — a duplicated `allow_pending` (not a
        // camera param) must not be mislabeled as a camera-override problem.
        Err((error, message)) => return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error, "message": message })),
        ).into_response(),
    };

    let state_word = {
        let st = eqoxide_nav::zone_assets::lock_state(&s.zone_assets).clone();
        eqoxide_nav::zone_assets::usability(&st, &s.player().zone)
            .map(|v| v.state_word()).unwrap_or("ready")
    };
    if !q.allow_pending.as_deref().is_some_and(truthy) {
        if let Some(refusal) = zone_assets_not_ready(&s) { return refusal; }
    }

    // #422: resolved BEFORE touching `frame_req` — an invalid override must 400 outright, never
    // register a request that would otherwise capture (at best a default, at worst a garbled) frame.
    let heading_deg = s.player().heading_ccw;
    let live_camera = s.camera.snapshot.lock().unwrap().clone();
    let camera_override = match resolve_camera_override(&q, heading_deg, &live_camera) {
        Ok(ov) => ov,
        Err(message) => return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid_camera_override", "message": message })),
        ).into_response(),
    };

    let (tx, rx) = oneshot::channel::<Vec<u8>>();
    *s.camera.frame_req.lock().unwrap() = Some(FrameCaptureRequest { camera_override, tx });

    // 10s: a debug build's readback + 1024px PNG encode can exceed 2s when the
    // render loop is saturated, which made captures 503 while frames were fine.
    // #646: a PNG body can't carry an in-band field, so — like `ZONE_ASSETS_STATE_HEADER` above —
    // freshness rides a response header. See `SNAPSHOT_AGE_HEADER`'s doc for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    match tokio::time::timeout(std::time::Duration::from_secs(10), rx).await {
        Ok(Ok(png_bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/png")
            .header(header::CACHE_CONTROL, "no-store")
            // Always present, so a caller never has to know whether the gate was bypassed: only
            // `ready` means this frame shows the zone the character is actually in.
            .header(ZONE_ASSETS_STATE_HEADER, state_word)
            .header(SNAPSHOT_AGE_HEADER, snapshot_age_ms)
            .body(Body::from(png_bytes))
            .unwrap(),
        _ => Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("renderer not ready"))
            .unwrap(),
    }
}

/// One enriched player row for GET /v1/observe/who.
#[derive(serde::Serialize)]
struct WhoView {
    name:  String,
    level: u32,
    /// Class name (e.g. "Wizard"), empty when the player is anonymous.
    class: String,
    /// Race code (e.g. "HUM"), empty when the player is anonymous.
    race:  String,
    /// Numeric zone id the player is in (0 when anonymous).
    zone_id: u32,
    /// Guild name, empty if none.
    guild: String,
    anon:  bool,
}

/// GET /v1/observe/who — server-wide `/who all` roster of everyone currently online. Triggers an
/// OP_WhoAllRequest and awaits the OP_WhoAllResponse (so an agent can see which fellow agents/players
/// are online before coordinating). Returns `{online: [{name, level, class, race, zone_id, guild,
/// anon}]}`. 503 if no response arrives in time. (#300)
async fn get_who(State(s): State<HttpState>) -> Response {
    let (tx, rx) = oneshot::channel::<Vec<eqoxide_core::game_state::WhoEntry>>();
    s.command.request_who(tx);
    match tokio::time::timeout(std::time::Duration::from_secs(6), rx).await {
        Ok(Ok(roster)) => {
            let online: Vec<WhoView> = roster.into_iter().map(|e| WhoView {
                class:   if e.anon { String::new() } else { eqoxide_core::race_class::class_name(e.class).to_string() },
                race:    if e.anon { String::new() } else { eqoxide_core::race_class::eq_race_to_code(e.race).to_string() },
                name: e.name, level: e.level, zone_id: e.zone_id, guild: e.guild, anon: e.anon,
            }).collect();
            // #646: read-time freshness — see `SNAPSHOT_AGE_HEADER`'s doc for the clock this
            // reuses. Note this only reflects whether the net thread is alive/ticking; the roster
            // itself is always a fresh request/reply, not a cached snapshot.
            let snapshot_age_ms = s.health().snapshot_age_ms;
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({ "online": online, "snapshot_age_ms": snapshot_age_ms }).to_string()))
                .unwrap()
        }
        _ => Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Body::from("no /who response (not connected, or server did not reply in time)"))
            .unwrap(),
    }
}

/// Strip a trailing run of ASCII digits off an EQEmu spawn name. When two mobs in a zone share a
/// name the server disambiguates them by appending a zero-padded numeric index — "a_bat" becomes
/// "a_bat00","a_bat01"…, and a duplicated *unique* NPC "Geeda" becomes "Geeda"/"Geeda00". So two
/// placements of the SAME underlying mob differ only by this suffix; grouping on the digit-stripped
/// base name is what lets the observe-boundary dedup (#471) recognize them as one logical entity
/// while leaving genuinely different names (which have different bases) untouched.
fn base_name(name: &str) -> &str {
    name.trim_end_matches(|c: char| c.is_ascii_digit())
}

/// One cluster of same-base-name entities the server placed at a byte-identical position (#471).
/// Surfaced (not silently dropped) so an agent can see the collapse happened and still knows the
/// full set of names — each remains individually targetable via the other APIs (`entity_ids` is
/// NOT deduped, only this read-only view is).
#[derive(serde::Serialize)]
struct DuplicateGroup {
    position: [f32; 3],
    /// Every full (suffixed) name the server reported at this exact position, sorted.
    names: Vec<String>,
    /// Which of `names` survives in the `entities` map (the un-suffixed spelling when present).
    kept: String,
}

/// Response for GET /v1/observe/entities. `entities` is the name→pos roster with same-name +
/// identical-position duplicates collapsed; `deduped`/`duplicate_groups`/`note` LABEL any collapse
/// so nothing is hidden silently (agent-honesty invariant, #471).
#[derive(serde::Serialize)]
struct EntitiesView {
    /// Number of entries in `entities` after the dedup.
    count: usize,
    /// name → [x,y,z] for all known entities, positional duplicates collapsed to one.
    entities: HashMap<String, [f32; 3]>,
    /// How many entries were collapsed out. 0 = the roster had no positional duplicates.
    deduped: usize,
    /// The collapsed clusters (empty when `deduped == 0`).
    duplicate_groups: Vec<DuplicateGroup>,
    /// Human-readable explanation, present only when `deduped > 0`.
    note: Option<String>,
    /// #643 — name → server-published `{pose, gait}`. Its key set is **exactly** `entities`'s:
    /// both are projected inside one critical section over the shared world tables, and every
    /// publisher of `entity_positions` (`ActionLoop::sync_entities` and `login.rs`'s zone-in seed)
    /// writes both maps together, so `body["poses"][name]` is safe for any `name` in `entities`.
    ///
    /// `pose` is the discrete body state (`standing`/`sitting`/`crouching`/`lying`/`looting`/
    /// `freeze`) and `gait` is the locomotion speed code from the last position update (`null`
    /// = the entity has not sent one, which is NOT "standing still"). A pose code this client
    /// does not recognise is reported as **`unknown(<raw>)`** — never silently defaulted.
    ///
    /// Before #643 these two wire signals shared ONE `u32` on the entity, so whichever packet
    /// arrived last decided what it meant, and the renderer's catch-all turned everything it
    /// could not classify into "idle". Nothing agent-visible reported the pose at all, so the
    /// confusion was completely invisible to a driving agent; this field is that missing channel.
    poses: HashMap<String, eqoxide_ipc::EntityPoseView>,
    /// #646: read-time freshness — see `SNAPSHOT_AGE_HEADER`'s doc. Only on the `?labeled=1` shape;
    /// the default bare map keeps its exact historical shape and carries the same value in the
    /// `SNAPSHOT_AGE_HEADER` response header instead.
    snapshot_age_ms: u64,
}

/// Collapse suspected server-side duplicate spawns (#471) for the read-only /observe/entities view.
///
/// Groups entries that share BOTH the same digit-stripped base name AND a byte-identical position
/// (exact f32 bits — independently-placed mobs practically never collide exactly, and a live
/// pathing mob has moved off its spawn point, so an exact match is the duplication fingerprint).
/// Any group with more than one member is collapsed to a single representative, preferring the
/// un-suffixed spelling. Returns the deduped name→pos map, the count removed, and a description of
/// every collapsed cluster so the drop is NEVER silent. The underlying `gs.world.entities`/`entity_ids`
/// maps are left untouched, so both physical instances stay individually targetable by their full
/// names — this is a display-layer honesty mitigation, not a change to the world model.
fn dedup_entities(
    positions: &HashMap<String, (f32, f32, f32)>,
) -> (HashMap<String, [f32; 3]>, usize, Vec<DuplicateGroup>) {
    // key: (base name, position bit-pattern) → all full names placed there.
    let mut groups: HashMap<(String, (u32, u32, u32)), Vec<String>> = HashMap::new();
    for (name, &(x, y, z)) in positions {
        let key = (base_name(name).to_string(), (x.to_bits(), y.to_bits(), z.to_bits()));
        groups.entry(key).or_default().push(name.clone());
    }
    let mut out: HashMap<String, [f32; 3]> = HashMap::new();
    let mut deduped = 0usize;
    let mut dup_groups = Vec::new();
    for ((_, (xb, yb, zb)), mut names) in groups {
        let pos = [f32::from_bits(xb), f32::from_bits(yb), f32::from_bits(zb)];
        names.sort();
        // Prefer the un-suffixed spelling (e.g. "Geeda" over "Geeda00") as the survivor, else the
        // lexicographically-first name — deterministic regardless of HashMap iteration order.
        let kept = names.iter().find(|n| base_name(n) == n.as_str())
            .cloned().unwrap_or_else(|| names[0].clone());
        out.insert(kept.clone(), pos);
        if names.len() > 1 {
            deduped += names.len() - 1;
            dup_groups.push(DuplicateGroup { position: pos, names, kept });
        }
    }
    dup_groups.sort_by(|a, b| a.kept.cmp(&b.kept));
    (out, deduped, dup_groups)
}

// `deny_unknown_fields`: same rationale as `MessagesQuery` (eqoxide#363) — a typo'd `?labled=1`
// must fail loudly (400) instead of silently degrading to the default view.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EntitiesQuery {
    /// `?labeled=1` (or `true`) opts into the rich `EntitiesView` that exposes WHICH duplicates were
    /// collapsed. Omitted / any other value → the default bare `{name:[x,y,z]}` map (still deduped).
    labeled: Option<String>,
}

/// GET /v1/observe/entities — the name→position roster of all known entities.
///
/// #471 (agent-honesty): live play saw the roster report ~2× duplicate spawns — byte-identical
/// name+position with consecutive server spawn_ids (e.g. 526/527), including unique named NPCs that
/// exist once per server, which also leaked into chat as doubled zone-in greetings. The client
/// cannot manufacture a second spawn_id (`register_spawn` upserts `gs.world.entities` by the verbatim
/// server id, `packet_handler.rs`), it clears the roster on every zone-in (`apply_new_zone`), and
/// both name→pos publishers full-replace their maps (`action_loop::sync_entities`, `login.rs`) — so
/// two distinct ids at one position can only be two genuine server `Mob`s (duplicated `spawn2`
/// content, whose names the wire disambiguates with a numeric suffix). A packet capture is still
/// needed to confirm two distinct spawn_ids on the wire vs. a client artifact.
///
/// Two response shapes, so the dedup fixes the doubling for EVERY existing consumer with ZERO shape
/// change:
/// - **default** → the historical bare `{ "<name>": [x,y,z], … }` map, now with same-base-name +
///   byte-identical-position duplicates collapsed. Backward-compatible (e.g. `group_driver.py`'s
///   `ents.get(name)` / `ents.items()` keep working) and its world model is corrected for free.
/// - **`?labeled=1`** → the rich `EntitiesView` (`count`/`entities`/`deduped`/`duplicate_groups`/
///   `note`) that LABELS the collapse for agents that want to SEE which duplicates were removed —
///   nothing is dropped silently (the honesty invariant), just moved off the default shape.
///
/// The underlying `gs.world.entities`/`entity_ids` model is untouched in either case, so every instance
/// stays targetable by its full (suffixed) name.
async fn get_entities(State(s): State<HttpState>, Query(q): Query<EntitiesQuery>) -> Response {
    let labeled = q.labeled.as_deref()
        .is_some_and(|v| v.eq_ignore_ascii_case("1") || v.eq_ignore_ascii_case("true"));
    // #643: `entities` and `poses` are read under ONE critical section, so a concurrent
    // `sync_entities` (which full-replaces positions/ids/poses together while holding all three)
    // cannot interleave between them. An earlier revision took the two locks sequentially and then
    // documented that the key sets "always" match — which was not true: a zone change landing in
    // the gap would have produced a `poses` map missing keys that `entities` still had, so an agent
    // doing `body["poses"][name]` could KeyError on a race it had been told could not happen.
    //
    // ⚠️ LOCK ORDER is `entity_positions` → `entity_poses`, matching `sync_entities`'
    // `entity_positions` → `entity_ids` → `entity_poses` (poses last in both, positions first in
    // both). See the canonical-order note in `name_match.rs`. Do not reverse these.
    let (entities, deduped, duplicate_groups, poses) = {
        let positions = s.world.entity_positions();
        let (entities, deduped, duplicate_groups) = dedup_entities(&positions);
        // Only pay for the pose projection on the labeled shape; the bare map does not carry it.
        let poses = if labeled {
            let all = s.world.entity_poses();
            entities.keys()
                .filter_map(|n| all.get(n).map(|p| (n.clone(), p.clone())))
                .collect::<HashMap<_, _>>()
        } else {
            HashMap::new()
        };
        (entities, deduped, duplicate_groups, poses)
    };
    // #646: read-time freshness — see `SNAPSHOT_AGE_HEADER`'s doc.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    if labeled {
        let note = (deduped > 0).then(|| format!(
            "{deduped} entry(ies) collapsed as same-name + byte-identical-position duplicates \
             (suspected server-side spawn2 duplication, #471). The underlying entity model is \
             untouched and every instance is still targetable by its full name; see duplicate_groups. \
             A live packet capture is still needed to confirm this is server-sent (two distinct \
             spawn_ids on the wire) rather than a client artifact."
        ));
        let resp = Json(EntitiesView {
            count: entities.len(), entities, deduped, duplicate_groups, note, poses, snapshot_age_ms,
        }).into_response();
        with_snapshot_age(resp, snapshot_age_ms)
    } else {
        // Default: the bare, backward-compatible name→pos map — deduped, but same shape as before.
        // No room for a new JSON key here without breaking existing consumers (`ents.items()`), so
        // the freshness age rides the header instead (#646).
        with_snapshot_age(Json(entities).into_response(), snapshot_age_ms)
    }
}

/// GET /v1/observe/inventory — the player's current inventory + equipment, published each tick by
/// the nav thread. Each item carries its Titanium **wire** slot (the number to pass to /interact/give
/// and /inventory/move — note general slots are one less than the EQEmu DB `inventory.slot_id`: DB
/// 23-30 → wire 22-29), plus item_id, name, charges, icon, and idfile. Use this to discover which
/// slot holds an item before giving/equipping it.
async fn get_inventory(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let items  = s.inventory_slots.inventory.lock().unwrap().clone();
    let player = s.player();
    Json(serde_json::json!({
        "count": items.len(),
        "items": items,
        "currency": currency_json(player.coin),
        // #361: see the /debug field of the same name — false means `currency` may not match the
        // server's real balance right now (a merchant buy in flight, or an unreconciled desync).
        "coin_verified": player.coin_verified,
        // #646: read-time freshness — see `SNAPSHOT_AGE_HEADER`'s doc for the clock this reuses.
        "snapshot_age_ms": s.health().snapshot_age_ms,
    }))
}

// `deny_unknown_fields`: same rationale as `EventsQuery` in events.rs (eqoxide#363) — a typo'd
// `?kidn=npc` must fail loudly instead of silently degrading `kind` to `None` (i.e. "no filter",
// returning the whole log) and reporting a plain 200.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MessagesQuery {
    /// Filter to a single message channel, e.g. ?kind=npc for NPC dialogue only.
    kind: Option<String>,
}

/// GET /v1/observe/messages — the in-game message log as machine-readable text (oldest→newest, last
/// ~50 lines), published each tick by the nav thread. This is how an agent reads **NPC dialogue**:
/// each line has a `kind` ("npc" = NPC say/emote, plus "chat", "combat", "system", "exp", "loot",
/// "trade", "zone"), the `text`, any `[bracketed]` quest `keywords` to say back via POST
/// /v1/interact/say, and any `item_links` embedded in the text. `text` never contains the raw EQ
/// item/say-link hex body — only the clean display name — and `item_links` carries the resolvable
/// `item_id` (plus `is_saylink`) behind each link name, so an item mentioned in dialogue (e.g.
/// "[rat whiskers]") can be looked up rather than only read as text (eqoxide#256). Filter with
/// `?kind=npc` for dialogue only.
async fn get_messages(
    State(s): State<HttpState>,
    Query(q): Query<MessagesQuery>,
) -> Json<serde_json::Value> {
    let all = s.chat.messages.lock().unwrap();
    let filtered: Vec<&MessageEntry> = match &q.kind {
        Some(k) => all.iter().filter(|m| m.kind == *k).collect(),
        None    => all.iter().collect(),
    };
    // #646: read-time freshness — see `SNAPSHOT_AGE_HEADER`'s doc for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    Json(serde_json::json!({ "count": filtered.len(), "messages": filtered, "snapshot_age_ms": snapshot_age_ms }))
}

/// GET /v1/observe/dialogue — the current clickable NPC-dialogue choices (saylinks from the most
/// recent NPC message, e.g. a Soulbinder's "[bind your soul]"). `index` is the argument POSTed to
/// /v1/interact/dialogue to click that choice. Empty when no NPC has offered choices. (#120)
async fn get_dialogue(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let choices = s.interact.dialogue.lock().unwrap();
    let list: Vec<_> = choices.iter().enumerate()
        .map(|(i, c)| serde_json::json!({ "index": i, "text": c.text }))
        .collect();
    // #646: read-time freshness — see `SNAPSHOT_AGE_HEADER`'s doc for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    Json(serde_json::json!({ "count": list.len(), "choices": list, "snapshot_age_ms": snapshot_age_ms }))
}

/// GET /v1/observe/spells — the 9 memorized gems with names. Empty gem = spell id 0 or 0xFFFFFFFF.
async fn get_spells(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let mem = s.player().mem_spells;
    let gems: Vec<_> = mem.iter().enumerate().map(|(i, &id)| {
        if id == 0 || id == 0xFFFF_FFFF {
            serde_json::json!({ "gem": i, "spell_id": null, "name": null })
        } else {
            let name = s.spells.get(id).map(|x| x.name.clone());
            serde_json::json!({ "gem": i, "spell_id": id, "name": name })
        }
    }).collect();
    // #646: read-time freshness — see `SNAPSHOT_AGE_HEADER`'s doc for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    Json(serde_json::json!({ "gems": gems, "snapshot_age_ms": snapshot_age_ms }))
}

/// GET /v1/observe/skills — the player's skills with current values (eqoxide#99). `value == 0`
/// means untrained. Ids/names are the RoF2 skill enum (`eqoxide_core::skills`); an agent uses this to
/// decide what to train at a guildmaster and to notice when a skill is capped.
async fn get_skills(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let skills = s.player().skills;
    let list: Vec<_> = (0..eqoxide_core::skills::NUM_SKILLS).map(|id| {
        let value = skills.get(id).copied().unwrap_or(0);
        serde_json::json!({ "id": id, "name": eqoxide_core::skills::skill_name(id as u32), "value": value })
    }).collect();
    // #646: read-time freshness — see `SNAPSHOT_AGE_HEADER`'s doc for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    Json(serde_json::json!({ "skills": list, "snapshot_age_ms": snapshot_age_ms }))
}

/// GET /v1/observe/doors — list the current zone's doors (id, name, position, opentype, open state).
///
/// **`[]` does NOT mean "this zone has no doors" (#939).** The roster is emptied on zone entry
/// (#891) and refills from `OP_SpawnDoor` records once they have both **arrived and been
/// published** — so during a zone-in this endpoint returns a confident empty list for a zone that
/// does have doors, and nothing served here tells the two apart.
///
/// Those are two different failures and only one of them is about the network. During the zone-entry
/// handshake the records are applied to game state but **not published**: `sync_doors` has exactly
/// one call site, in the gameplay drain (`eqoxide_net::gameplay`, `run_gameplay_phase`), which the
/// handshake loop is not. So a record can be in hand and still absent here — that is #937, and it is
/// a missing publication, not a missing packet. Separately, records genuinely not yet sent arrive on
/// no schedule this client publishes.
///
/// There is no completeness observable to wait on for either: `zone_assets.state` reaching
/// `"ready"` gates on terrain meshes and collision triangles, so it is about geometry, not about
/// which door packets have landed.
///
/// The same limit binds the other direction, and this is the reason `/v1/interact/click_door`
/// answers a miss with `404` *unknown* rather than *disproved*: a **populated** roster is not a
/// closed set either, so "not among the doors held right now" is the strongest claim available at
/// any roster size. Do not let a non-empty list here read as a complete one.
async fn get_doors(State(s): State<HttpState>) -> Response {
    // #646: bare array body (backward-compatible shape) — freshness rides `SNAPSHOT_AGE_HEADER`
    // instead of a JSON key. See that const's doc for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    let doors = s.interact.doors_shared.lock().unwrap().clone();
    with_snapshot_age(Json(doors).into_response(), snapshot_age_ms)
}

/// GET /v1/observe/zone_entrances — the zone **entrances** advertised by the server
/// (`OP_SendZonepoints`): where you *arrive* (in the destination zone's coordinate space) and your
/// heading when you cross into a zone, keyed by destination `zone_id` + `iterator`. This is NOT
/// where you go to *leave* the current zone — for that, see `/zone_exits`. (Also served at the
/// deprecated alias `/zone_points`.)
///
/// The list can also carry a handful of client-synthesized entries read from this zone's map `.txt`
/// (labels starting `"to "`, currently only recognized for North/South Qeynos) — a fallback for
/// zones the server doesn't fully advertise. **#816:** if that `.txt` read fails, those fallback
/// entries are simply absent from this list rather than announced as missing — check
/// `zone_map_load` on `GET /v1/observe/debug` (`null` = this zone's map-label fallback loaded fine
/// or hasn't been needed yet; non-null names why it didn't, so an empty/short list here can be told
/// apart from "this zone's map genuinely has no fallback labels"). The SERVER-advertised entries in
/// this list are never affected — this only ever narrows the additive fallback.
async fn get_zone_entrances(State(s): State<HttpState>) -> Response {
    // #646: bare array body (backward-compatible shape, also served under the deprecated
    // `/zone_points` alias) — freshness rides `SNAPSHOT_AGE_HEADER` instead of a JSON key.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    let points = s.world.zone_points.lock().unwrap().clone();
    with_snapshot_age(Json(points).into_response(), snapshot_age_ms)
}

/// GET /v1/observe/zone_exits — the current zone's **exits**: the WLD zone-line regions you navigate
/// *toward* to leave, in the current zone's coordinate space. Each exit is the same region
/// `/v1/move/zone_cross` walks to. Per exit: `location` `[x,y,z]` (a point inside the region nearest
/// the player — position-relative), `zone_id` (destination, or `null` if the WLD region's index
/// isn't advertised in the entrance list), and `index` (the link to the matching entrance's
/// `iterator`). Advertised entrances with no WLD region are omitted.
///
/// **`[]` means exactly one thing: this zone's region map loaded and contains no zone-line regions
/// (#803).** It never means "the file that would list them did not load" — that is a
/// **503 `zone_region_data_unavailable`** with a machine-readable `reason`
/// (`region_data_missing` / `_unreadable` / `_not_region_data` / `_unsupported_version` /
/// `_truncated`). Before #803 both produced `[]` with 200 OK, and because exits
/// are the only way out of a zone, an agent read a failed read as "sealed in". Unlike the
/// `zone_assets_not_ready` 503 below, this one does **not** clear by polling — the asset is missing
/// or unusable, not loading.
///
/// **What makes that "never" true is the shape of the code, not this sentence** (#821 review round
/// 2, B4). This handler used to take permission from the zone-assets verdict and then read the grid
/// out of the separate `shared_collision` slot behind an `if let Some(col)` with **no `else`** — so
/// a `None` there returned `200 []` having consulted no region map at all, and nothing coupled the
/// two slots. It now gets verdict AND grid from one call
/// ([`eqoxide_nav::zone_assets::usable_collision`]), whose `Ok` arm carries the `Arc<Collision>` the
/// `Ready` state owns. There is no longer a branch that can reach the response builder without a
/// grid, so every `[]` this endpoint emits has been through `Collision::zone_line_indices()`.
///
/// `gated` (#713 item 3) is `true` when this exit's destination is unadvertised **and** this ZONE's
/// #679/#683 unresolved-cross gate refuses server-resolved crossings. It is a **zone-level** verdict:
/// **the player's position is not an input** (see the comment where it is computed), so it does not
/// mean "cannot be crossed from where you are standing" — an earlier revision of this sentence said
/// that, and it described a field that does not exist (#713 review round 2, B3).
///
/// `gated: false` is therefore **not a promise the auto-cross will fire** when you stand there. The
/// #713 attempt bound is stand-scoped and this field never consults it, so once the bound has been
/// reached, standing on a `gated: false` exit does not cross either. Cross-check `zone_cross_stopped`
/// on `/v1/observe/debug` before concluding that a non-crossing exit is a data problem.
///
/// **503 `zone_assets_not_ready` while the zone's assets are still loading (#579)** — the exits come
/// out of the collision grid, so before it is built this returned a confident `[]`, i.e. "this zone
/// has no exits at all". That is a falsehood an agent cannot detect; an explicit refusal is the
/// honest answer. Poll `/v1/observe/debug` → `zone_assets` until it reads `ready`.
async fn get_zone_exits(State(s): State<HttpState>) -> Response {
    // The #579 gate AND the grid it vouches for, from ONE read of ONE slot (#821 review round 2,
    // B4). Deliberately not `zone_assets_not_ready(&s)` followed by `s.shared_collision`: those are
    // two slots, and the fall-through when the second was `None` answered `[]` — see this
    // function's doc comment.
    let st = eqoxide_nav::zone_assets::lock_state(&s.zone_assets).clone();
    let player_zone = s.player().zone;
    let col = match eqoxide_nav::zone_assets::usable_collision(&st, &player_zone) {
        Ok(col) => col.clone(),
        Err(verdict) => return zone_assets_refusal(verdict, &st, &player_zone),
    };
    // #646: bare array body (backward-compatible shape) — freshness rides `SNAPSHOT_AGE_HEADER`
    // instead of a JSON key. See that const's doc for the clock this reuses.
    let snapshot_age_ms = s.health().snapshot_age_ms;
    let player = s.player();
    // `pos_up` is already the FOOT datum (#522), the same datum as the collision geometry
    // (zone-line regions) it's tested against — no conversion needed.
    let pos = [player.pos_east, player.pos_north, player.pos_up];
    // index -> destination zone_id, from the advertised entrance list. `zone_id == 0` entries are
    // the "server resolves from position" SENTINEL, not a destination (the same filter
    // `resolve_cross_destination` applies) — dropping them makes such an exit report
    // `"zone_id": null` (destination honestly unknown, #683) instead of a nonexistent "zone 0".
    // #713 item 3 — the UNRESOLVED-EXIT GATE VERDICT, reported rather than re-derived.
    //
    // An exit with `zone_id: null` is one whose baked WLD index matches no advertised entrance. The
    // client can still cross it, by asking the server to resolve the destination from position
    // (the #683 fallback) — but ONLY when `classify_unresolved_cross` allows it in this zone. Where
    // it does not, standing on that exit does nothing, and before this the agent learned that from
    // a message-log line AFTER it had already walked there. `gated: true` says it beforehand.
    //
    // This calls the SAME function the net thread's `cross_unresolved` calls, from the crate both
    // sides depend on — deliberately not a second copy of the rule, because a `gated` flag that
    // drifted from the gate would be a confident falsehood the agent has no way to check (#713 moved
    // the function into `eqoxide-core` for exactly this). Nothing here changes what the gate
    // decides; the same-zone-pad question (#543/#266/#582) is untouched.
    //
    // NOT MEASURED (reasoned): the two inputs come from two places — `zone_points` from the IPC
    // slot the net thread syncs, `zone_id` from the `ArcSwap` GameState snapshot — so a read that
    // lands between those two publishes can pair one zone's points with the other's id, and the
    // rule is "any reported verdict computed from inputs sampled at two instants can straddle an
    // event that changes both". The instance here is a zone change. I did not observe it: the
    // window is one publish wide and this endpoint already refuses with 503 until the new zone's
    // assets are ready, which lags a zone change by seconds — but that is an argument about
    // likelihood, not an impossibility proof, so it is disclosed rather than claimed closed.
    let zone_points = s.world.zone_points.lock().unwrap().clone();
    // FIRST advertised entry wins per index, matching `ActionLoop::resolve_cross_destination`'s
    // `.find()` over the same filtered list (#713 review round 2, N1). A `.collect()` into a
    // `HashMap` takes the LAST, so where a zone advertises two points under one iterator the
    // reported `zone_id` named a destination the client would not actually take. `gated` never
    // depended on which one won (it only asks `is_none()`), but the `zone_id` printed beside it did.
    let mut dest_of: std::collections::HashMap<i32, u16> = std::collections::HashMap::new();
    for zp in zone_points.iter().filter(|zp| zp.zone_id != 0) {
        dest_of.entry(zp.iterator as i32).or_insert(zp.zone_id);
    }
    let unresolved_gated = eqoxide_core::zone_cross::classify_unresolved_cross(
        &zone_points, s.game_state.load().world.zone_id,
    ) == eqoxide_core::zone_cross::UnresolvedCross::Ignore;
    // #803: `[]` here must only ever mean "this zone's region map loaded and has no zone-line
    // regions". When the `.wtr` did NOT load, the exits are UNKNOWN, and publishing the empty
    // list would be a confident falsehood the agent cannot detect — and the falsehood is
    // specifically "there is no way out of this zone". Refuse, and name the cause.
    let indices = match col.zone_line_indices() {
        Ok(ix) => ix,
        Err(absent) => return region_data_unavailable(&absent),
    };
    let mut exits = Vec::new();
    for index in indices {
        let location = col
            .find_zone_line_near(Some(index), pos)
            .map(|(_, p)| serde_json::json!([p[0], p[1], p[2]]));
        // An exit with a known destination is crossed by `perform_cross`, which never consults
        // the unresolved gate — so the gate can only ever close a `zone_id: null` exit.
        let gated = dest_of.get(&index).is_none() && unresolved_gated;
        exits.push(serde_json::json!({
            "index": index,
            "zone_id": dest_of.get(&index),
            "location": location,
            "gated": gated,
        }));
    }
    with_snapshot_age(Json(serde_json::json!(exits)).into_response(), snapshot_age_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{ago, empty_state, empty_state_wall_clock, set_gs};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn debug_json(state: HttpState) -> serde_json::Value {
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get("/debug").body(Body::empty()).unwrap()).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn nav_debug_json(state: HttpState) -> serde_json::Value {
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get("/nav_debug").body(Body::empty()).unwrap()).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn asset_sync_json_body(state: &HttpState) -> serde_json::Value {
        let app = router().with_state(state.clone());
        let resp = app.oneshot(Request::get("/asset_sync").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn dl(chunks_done: usize, chunks_total: usize, bytes: u64, ms: u64) -> eqoxide_ipc::AssetSyncPhase {
        eqoxide_ipc::AssetSyncPhase::Downloading {
            chunks_done, chunks_total, bytes, elapsed: std::time::Duration::from_millis(ms),
        }
    }

    /// Open a sync on the state's OWN registry, the only way production writes it. The returned
    /// guard must be held for the sync to stay live — dropping it is how a sync ends.
    fn begin(state: &HttpState, set: &str) -> eqoxide_ipc::AssetSyncGuard {
        eqoxide_ipc::AssetSyncGuard::begin(&crate::testkit::asset_sync_slot(state), set)
    }

    /// #715 trap 2 — "no sync in progress" and "a sync sitting at zero progress" are different
    /// states an agent acts on differently, so they must be different BODIES. A zero-initialized
    /// default that makes an idle client look like a download stalled at 0% is the failure class
    /// this project ranks worst.
    #[tokio::test]
    async fn asset_sync_idle_is_not_the_same_body_as_a_download_at_zero_progress() {
        let state = empty_state();
        let idle = asset_sync_json_body(&state).await;
        assert_eq!(idle["active"], serde_json::json!(false),
            "a client with no sync running must say so explicitly");
        assert!(idle.get("downloading").is_none(),
            "an idle client must carry NO transfer data at all: {idle}");
        assert!(idle.get("set").is_none(), "an idle client is not syncing any set: {idle}");
        assert_eq!(idle["syncs"], serde_json::json!([]), "…and no sync is listed: {idle}");

        let g = begin(&state, "zone/qeynos2");
        g.tick(dl(0, 7, 0, 0));
        let zero = asset_sync_json_body(&state).await;
        assert_eq!(zero["active"], serde_json::json!(true),
            "a download that has not finished a chunk yet is still a RUNNING sync");
        assert_eq!(zero["downloading"]["chunks_done"], serde_json::json!(0),
            "a real zero must be reported as a real zero");
        assert_eq!(zero["downloading"]["chunks_total"], serde_json::json!(7));
        assert_ne!(idle, zero,
            "idle and stalled-at-zero must not serialize to the same thing — an agent cannot \
             distinguish them if they do");
    }

    /// #715 trap 1 — the phase enum must NOT be collapsed at the API boundary. Transfer data lives
    /// in a `downloading` object that exists ONLY in that phase; a flat body with a nullable
    /// `rate`/`bytes` would make "not downloading" indistinguishable from "downloading, rate not
    /// yet derivable".
    #[tokio::test]
    async fn non_downloading_phases_carry_no_transfer_data_at_all() {
        let state = empty_state();
        let g = begin(&state, "zone/qeynos2");

        for (phase, tag) in [
            (eqoxide_ipc::AssetSyncPhase::Starting,  "starting"),
            (eqoxide_ipc::AssetSyncPhase::Verifying, "verifying"),
        ] {
            g.tick(phase);
            let v = asset_sync_json_body(&state).await;
            assert_eq!(v["active"], serde_json::json!(true));
            assert_eq!(v["phase"], serde_json::json!(tag));
            assert_eq!(v["set"], serde_json::json!("zone/qeynos2"));
            assert!(v.get("downloading").is_none(),
                "phase {tag} has no transfer data — the key must be ABSENT, not null: {v}");
            // Belt and braces: no rate/bytes anywhere in the DATA, at any nesting. `semantics` is
            // prose that names both fields to explain them, so it is excluded from the scan.
            let mut data = v.clone();
            data.as_object_mut().unwrap().remove("semantics");
            let text = data.to_string();
            assert!(!text.contains("rate_bytes_per_sec"),
                "a rate must be unreachable outside the downloading phase: {v}");
            assert!(!text.contains("\"bytes\""),
                "byte counts must be unreachable outside the downloading phase: {v}");
        }

        g.tick(dl(3, 7, 2_000_000, 2_000));
        let v = asset_sync_json_body(&state).await;
        assert_eq!(v["phase"], serde_json::json!("downloading"));
        assert_eq!(v["downloading"]["bytes"], serde_json::json!(2_000_000u64));
        assert_eq!(v["downloading"]["elapsed_secs"], serde_json::json!(2.0));
    }

    /// The rate is derived with the producer's own 100 ms guard (#708 req 4). Under it there is no
    /// honest rate, and the key is OMITTED — not reported as null and certainly not as 0, which
    /// would read as "the download has stopped".
    #[tokio::test]
    async fn the_rate_is_omitted_rather_than_faked_while_the_phase_is_too_young() {
        let state = empty_state();
        let g = begin(&state, "common");

        g.tick(dl(1, 7, 1_048_576, 50));
        let young = asset_sync_json_body(&state).await;
        assert!(young["downloading"].get("rate_bytes_per_sec").is_none(),
            "50 ms is under the producer's minimum — no rate can be derived honestly: {young}");
        assert_eq!(young["downloading"]["rate_unavailable"], serde_json::json!("phase_too_young"),
            "…and an omission with no stated reason is its own small ambiguity: {young}");
        assert_eq!(young["downloading"]["bytes"], serde_json::json!(1_048_576u64),
            "…while the raw evidence is still reported");

        g.tick(dl(1, 7, 2_000_000, 2_000));
        let v = asset_sync_json_body(&state).await;
        let rate = v["downloading"]["rate_bytes_per_sec"].as_f64()
            .unwrap_or_else(|| panic!("a 2s-old phase must carry a rate: {v}"));
        assert!((rate - 1_000_000.0).abs() < 1.0, "2 MB over 2 s is 1 MB/s, got {rate}");
        assert!(v["downloading"].get("rate_unavailable").is_none(),
            "a published rate must not also carry a reason it is missing: {v}");
    }

    /// #726 review finding 2 — a WEDGED download's rate is not a stale truth, it is a falsehood.
    ///
    /// The reviewer's live numbers: the last tick of a real cold zone load was 31,294,024 B over
    /// 20.65 s = 1,515,404 B/s. Wedge that transfer for five minutes and the endpoint kept asserting
    /// 1,515,404 while the true phase average was ~97,600 B/s (15× lower) and the instantaneous
    /// rate was zero. `published_age_ms` DOCUMENTS the staleness but does not remove it, and there
    /// was no threshold anywhere for a caller to apply. The precedent already in this file — the
    /// 100 ms guard, which omits the key rather than publishing a fake number — is the fix.
    #[tokio::test]
    async fn a_wedged_syncs_rate_is_withheld_rather_than_asserted_from_a_five_minute_old_sample() {
        let state = empty_state();
        let g = begin(&state, "zone/neriakc");
        let wedged = dl(374, 374, 31_294_024, 20_650);

        // Healthy: the sample is current and the rate is real.
        g.tick(wedged.clone());
        let live = asset_sync_json_body(&state).await;
        let rate = live["downloading"]["rate_bytes_per_sec"].as_f64()
            .unwrap_or_else(|| panic!("a fresh sample must carry its rate: {live}"));
        assert!((rate - 1_515_449.0).abs() < 1_000.0, "1.5 MB/s, got {rate}");

        // …and the SAME sample, five minutes later with nothing having ticked since.
        g.tick_stamped(wedged, ago(300));
        let v = asset_sync_json_body(&state).await;
        assert!(v["downloading"].get("rate_bytes_per_sec").is_none(),
            "a rate is an assertion about NOW, and this transfer has moved nothing for five \
             minutes — publishing 1.5 MB/s beside a 300000 ms age is still a confident \
             falsehood: {v}");
        assert_eq!(v["downloading"]["rate_unavailable"], serde_json::json!("sample_too_stale"),
            "…and the caller must be told WHICH rule withheld it, not left to infer: {v}");
        // The measurements stay — they are still the last known truth, which is exactly why the age
        // has to sit beside them. Only the derived assertion is withheld.
        assert_eq!(v["downloading"]["bytes"], serde_json::json!(31_294_024u64));
        assert_eq!(v["downloading"]["elapsed_secs"], serde_json::json!(20.65));
        assert!(v["published_age_ms"].as_u64().unwrap() >= 300_000, "{v}");
        assert_eq!(v["active"], serde_json::json!(true),
            "a wedged sync is still RUNNING — withholding the rate must not be mistaken for \
             clearing the sync: {v}");
    }

    /// The threshold must not fire during a healthy load. The reviewer measured live inter-tick
    /// gaps at median 41 ms, p95 185 ms, max 469 ms; a rule that suppressed the rate at 469 ms
    /// would be its own false alarm, and would make the field useless exactly when it works.
    #[tokio::test]
    async fn the_slowest_healthy_inter_tick_gap_measured_live_still_publishes_a_rate() {
        let state = empty_state();
        let g = begin(&state, "zone/neriakc");
        g.tick_stamped(dl(200, 374, 16_000_000, 10_000),
            std::time::Instant::now() - std::time::Duration::from_millis(469));
        let v = asset_sync_json_body(&state).await;
        assert!(v["downloading"]["rate_bytes_per_sec"].is_f64(),
            "469 ms is the worst gap measured on a HEALTHY link — the rate must survive it: {v}");
    }

    /// #715 / the #343 class, the half a guard cannot fix — a WEDGED sync. The producer ticks only
    /// when a chunk completes, so a hung transfer leaves `chunks_done`/`bytes`/`rate` frozen at
    /// their last values with no live writer. Clearing would be wrong (the sync really is still in
    /// progress), so the honest answer is to say how OLD the frozen sample is — computed at read
    /// time, never cached.
    #[tokio::test]
    async fn a_frozen_sample_reports_its_age_so_a_wedged_sync_is_not_read_as_a_healthy_one() {
        let state = empty_state();
        // A sync whose last tick was 30 s ago: mid-download, nothing published since.
        let g = begin(&state, "zone/freportw");
        g.tick_stamped(dl(3, 7, 2_000_000, 2_000), ago(30));

        let v = asset_sync_json_body(&state).await;
        let age = v["published_age_ms"].as_u64()
            .unwrap_or_else(|| panic!("an in-flight sync must report how old its sample is: {v}"));
        assert!(age >= 30_000,
            "a sample last published 30 s ago must report ~30000 ms, got {age} — an agent cannot \
             tell a wedged download from a live one if the frozen numbers carry no age");
        // The frozen numbers are still served (they are the last known truth), which is exactly why
        // the age has to be there beside them.
        assert_eq!(v["downloading"]["chunks_done"], serde_json::json!(3));

        // …and a sample published just now must NOT read as old, so the field is a real read-time
        // measurement rather than a constant that happens to satisfy the assertion above.
        g.tick(dl(3, 7, 2_000_000, 2_000));
        let fresh = asset_sync_json_body(&state).await["published_age_ms"].as_u64().unwrap();
        assert!(fresh < 5_000, "a just-published sample must read as fresh, got {fresh} ms");
    }

    /// `running_ms` is the one duration that keeps MOVING while a sync is wedged: `elapsed_secs` is
    /// frozen at the last tick like everything else in `downloading`, and in the `starting` phase
    /// (a hung manifest fetch) there is no duration at all. Without it an agent asking "how long
    /// has this been going?" reads a number that stopped advancing when the sync stopped.
    #[tokio::test]
    async fn running_ms_measures_the_whole_call_at_read_time_not_the_frozen_phase_elapsed() {
        let state = empty_state();
        // A sync wedged in `starting` — one sample, at begin, and no producer tick will ever come.
        // There is no phase duration to fall back on here, so the field can only come from a
        // read-time measurement; the sleep is a real lower bound on a monotonic clock, which is what
        // makes this assertion able to FAIL rather than merely able to pass. (It could: the first
        // version of this test asserted only `is_u64()` and `< 5_000`, and a mutation reporting the
        // frozen phase elapsed survived both.)
        let g = begin(&state, "zone/neriakc");
        std::thread::sleep(std::time::Duration::from_millis(30));
        let v = asset_sync_json_body(&state).await;
        assert_eq!(v["phase"], serde_json::json!("starting"));
        assert!(v["running_ms"].as_u64().expect("running_ms") >= 25,
            "a sync wedged before its first tick still has an age, and it is the only number \
             it has: {v}");
        assert!(v.get("downloading").is_none(), "…and no transfer data whatsoever: {v}");

        // A frozen downloading sample claiming TEN MINUTES of phase elapsed, published THIRTY
        // SECONDS ago, on a call that began milliseconds ago. Three durations, all different, and
        // `running_ms` must be none of the other two: reporting the producer's frozen `elapsed`
        // would answer "10 minutes", and reporting the SAMPLE's age would answer "30 s" — the
        // latter being `published_age_ms`, which is right there in the same body.
        //
        // The bound below is 5 s, not 60 s. #726 review round 2 found the 60 s version was a
        // passenger sibling of the one already fixed above: it excluded the 600 s frozen elapsed but
        // admitted the 30 s sample age, so a mutation swapping `started_at.elapsed()` for
        // `published_at.elapsed()` — collapsing the exact distinction this field exists to make —
        // survived it with the whole suite green.
        g.tick_stamped(dl(3, 7, 2_000_000, 600_000), ago(30));
        let v = asset_sync_json_body(&state).await;
        assert_eq!(v["downloading"]["elapsed_secs"], serde_json::json!(600.0),
            "the producer's own frozen measurement, reported verbatim");
        assert!(v["published_age_ms"].as_u64().unwrap() >= 30_000,
            "…the sample really is 30 s old, which is what makes the next assertion mean \
             something: {v}");
        assert!(v["running_ms"].as_u64().unwrap() < 5_000,
            "…while running_ms is measured from the CALL's start at read time: not the frozen \
             phase elapsed, and not the sample's age, wearing a different name: {v}");
    }

    /// #726 review N5 — an empty list used to be the same answer for "a sync just finished" and
    /// "nothing has ever synced in this process", which is the known-empty-vs-unknown collapse this
    /// repo has been bitten by before.
    #[tokio::test]
    async fn idle_says_whether_a_sync_has_ever_run_rather_than_collapsing_the_two() {
        let state = empty_state();
        let never = asset_sync_json_body(&state).await;
        assert_eq!(never["last_ended"], serde_json::json!(null),
            "before any sync has run there is nothing to report, and that is not a completion: \
             {never}");

        drop(begin(&state, "zone/neriakc"));
        let after = asset_sync_json_body(&state).await;
        assert_eq!(after["active"], serde_json::json!(false), "…the sync really is over: {after}");
        assert_eq!(after["last_ended"]["set"], serde_json::json!("zone/neriakc"),
            "…but 'idle after a zone sync' must be distinguishable from 'idle, nothing ever \
             ran': {after}");
        assert!(after["last_ended"]["ago_ms"].as_u64().unwrap() < 5_000, "{after}");
    }

    /// #715 trap 3 / the #343 class — the handler must read the LIVE cell every request. A value
    /// captured when the state was built (or cached in the handler) would report a client that has
    /// been downloading for a minute as idle forever, which is exactly the bug this endpoint is
    /// supposed to close.
    #[tokio::test]
    async fn the_handler_re_reads_the_live_slot_on_every_request() {
        let state = empty_state();
        assert_eq!(asset_sync_json_body(&state).await["active"], serde_json::json!(false));

        let g = begin(&state, "zone/freportw");
        g.tick(dl(2, 9, 4096, 1_000));
        let mid = asset_sync_json_body(&state).await;
        assert_eq!(mid["active"], serde_json::json!(true),
            "a sync started AFTER the state was built must still be visible");
        assert_eq!(mid["set"], serde_json::json!("zone/freportw"));

        // …and the transition back OUT, which is the half that gets forgotten: a finished sync
        // that keeps reading as live is a confident lie about work that is over.
        drop(g);
        let after = asset_sync_json_body(&state).await;
        assert_eq!(after["active"], serde_json::json!(false),
            "a finished sync must stop being reported as in-flight: {after}");
        assert!(after.get("downloading").is_none(),
            "…and must leave no transfer data behind: {after}");
    }

    /// #726 review finding 1, AT THE API BOUNDARY. The client runs three loaders, and the
    /// model-sync worker's short `charmodel/<key>` sync routinely begins and ends inside the zone
    /// loader's long `zone/<zone>` download. With one last-writer-wins slot the nested sync's exit
    /// cleared it, and this endpoint answered `{"active": false}` while a 31 MB download was still
    /// in flight — for the rest of that download, and forever if it was wedged.
    #[tokio::test]
    async fn a_nested_sync_finishing_does_not_make_the_endpoint_deny_the_long_one_still_running() {
        let state = empty_state();
        let zone = begin(&state, "zone/neriakc");
        zone.tick(dl(120, 374, 10_000_000, 8_000));

        // Both live: the endpoint must show BOTH, not interleave them into one slot.
        let model = begin(&state, "charmodel/hum");
        let both = asset_sync_json_body(&state).await;
        let sets: Vec<&str> = both["syncs"].as_array().unwrap().iter()
            .map(|s| s["set"].as_str().unwrap()).collect();
        assert_eq!(sets, ["zone/neriakc", "charmodel/hum"],
            "every sync in flight must be listed, oldest-started first: {both}");
        assert_eq!(both["set"], serde_json::json!("zone/neriakc"),
            "…and the mirrored primary is syncs[0], the oldest-started sync — which happens to be \
             the zone download HERE only because it started first; see \
             `the_primary_is_not_the_zone_download_when_a_model_sync_outlived_the_last_zone`: \
             {both}");

        // The short one ends inside the long one.
        drop(model);
        let v = asset_sync_json_body(&state).await;
        assert_eq!(v["active"], serde_json::json!(true),
            "zone/neriakc is STILL DOWNLOADING, but the endpoint now reports 'no asset sync is \
             running' — a confident falsehood, not a stale truth: {v}");
        assert_eq!(v["set"], serde_json::json!("zone/neriakc"));
        assert_eq!(v["downloading"]["chunks_done"], serde_json::json!(120),
            "…with its progress intact, not reset: {v}");
        assert_eq!(v["syncs"].as_array().unwrap().len(), 1,
            "…and the nested sync, which really is over, must be gone: {v}");
    }

    /// The same defect in its worst form: the outer sync is wedged in `starting` — a hung manifest
    /// request — so it has published exactly once and will never publish again. A single slot that
    /// a nested sync deleted stayed empty for the entire wedge, so the failure this endpoint exists
    /// to expose became invisible while it reported the healthiest possible answer.
    #[tokio::test]
    async fn a_nested_sync_cannot_permanently_hide_a_zone_sync_wedged_in_starting() {
        let state = empty_state();
        let _wedged = begin(&state, "zone/neriakc"); // one sample, at begin; no tick ever follows
        drop(begin(&state, "charmodel/hum"));

        let v = asset_sync_json_body(&state).await;
        assert_eq!(v["active"], serde_json::json!(true),
            "a manifest fetch hung in `starting` is exactly the wedge this endpoint was built to \
             surface, and it must not be erased by an unrelated sync finishing: {v}");
        assert_eq!(v["set"], serde_json::json!("zone/neriakc"));
        assert_eq!(v["phase"], serde_json::json!("starting"));
    }

    // ── #726 review ROUND 2, finding 1: the primary is one sync, not the process ────────────────
    //
    // The registry made every field true of the sync it names. What was still false was the
    // GUIDANCE — carried in `semantics`, so an agent reads it — that the top-level copy of syncs[0]
    // answered "is the load I am waiting on alive?" and that a caller "need not iterate". Both of
    // the sequences below are ones the client really produces, and both are the reviewer's.

    /// The universal claim that syncs[0] is "the long zone download in every overlap the client
    /// actually produces" is false. The model-sync worker is a long-lived thread with a queue, so a
    /// `charmodel/<key>` requested during the PREVIOUS zone can still be in flight when the next
    /// zone load opens `zone/<zone>` — and is then the older of the two.
    ///
    /// This test pins the corrected contract rather than a repaired guess: the primary is whichever
    /// sync started first, stated as such, and a caller who wants a NAMED set looks it up in `syncs`.
    #[tokio::test]
    async fn the_primary_is_not_the_zone_download_when_a_model_sync_outlived_the_last_zone() {
        let state = empty_state();
        let model = begin(&state, "charmodel/hum"); // queued during the previous zone
        model.tick(dl(4, 149, 400_000, 900));
        let zone = begin(&state, "zone/neriakc"); // the load the agent is actually waiting on
        zone.tick(dl(2, 374, 900_000, 800));

        let v = asset_sync_json_body(&state).await;
        assert_eq!(v["set"], serde_json::json!("charmodel/hum"),
            "the primary is the oldest-STARTED sync, which here is the small model set — the \
             endpoint must not pretend it is the zone download: {v}");

        // …and the route to the sync the caller actually cares about is by NAME, which is why the
        // list is the contract and the mirror is only a convenience.
        let by_name: Vec<&str> = v["syncs"].as_array().unwrap().iter()
            .map(|s| s["set"].as_str().unwrap()).collect();
        assert_eq!(by_name, ["charmodel/hum", "zone/neriakc"],
            "every live sync is listed, oldest-started first: {v}");
    }

    /// The consequence, and the reason this was blocking: a caller following the documented
    /// one-field wedge check reads the top level, sees a fresh age and a healthy rate, and concludes
    /// "not wedged" — while a different sync in the same process has published nothing for five
    /// minutes and the client is holding the evidence.
    ///
    /// Sequence: a model sync opens; the zone load's door set opens after it and is what the agent
    /// is now waiting on; the door set wedges while the model sibling keeps ticking.
    #[tokio::test]
    async fn a_healthy_primary_cannot_report_an_all_clear_while_a_sibling_is_wedged() {
        let state = empty_state();
        let model = begin(&state, "charmodel/hum");
        let doors = begin(&state, "zonedoors/neriakc");
        // The door set published once and then stopped — five minutes ago.
        doors.tick_stamped(dl(12, 88, 1_200_000, 3_000), ago(300));
        // The model sibling is perfectly healthy and is the primary, because it started first.
        model.tick(dl(140, 149, 12_000_000, 13_000));

        let v = asset_sync_json_body(&state).await;

        // Nothing here is a WRONG VALUE — that is the point. The primary really is fresh.
        assert_eq!(v["set"], serde_json::json!("charmodel/hum"));
        assert!(v["published_age_ms"].as_u64().unwrap() < 1_000,
            "the primary genuinely is fresh — no field is lying: {v}");
        assert!(v["downloading"]["rate_bytes_per_sec"].is_number(),
            "…and it genuinely does have a current rate: {v}");

        // What must not happen is that a single-field check on this body returns an all-clear.
        assert!(v["stalest_published_age_ms"].as_u64().unwrap() >= 300_000,
            "a caller following the documented one-field wedge check must not read 'healthy' \
             while a sync in this very process has published nothing for five minutes: {v}");

        // And the wedged sibling is fully described where it lives.
        let doors_entry = v["syncs"].as_array().unwrap().iter()
            .find(|s| s["set"] == serde_json::json!("zonedoors/neriakc"))
            .expect("the wedged sync must be listed");
        assert_eq!(doors_entry["downloading"]["rate_unavailable"],
            serde_json::json!("sample_too_stale"),
            "…and its own rate is withheld, as round 1 required: {doors_entry}");
    }

    /// The aggregate must be a VIEW of the ages in the same body, not a second measurement of the
    /// same clocks. Two independent `elapsed()` calls can straddle a threshold, and a top-level age
    /// matching no entry in its own response is a body that contradicts itself.
    #[tokio::test]
    async fn the_stalest_age_is_one_of_the_ages_the_body_already_reports() {
        let state = empty_state();
        let a = begin(&state, "zone/neriakc");
        a.tick_stamped(dl(1, 9, 100, 500), ago(42));
        let b = begin(&state, "common");
        b.tick(dl(1, 9, 100, 500));

        let v = asset_sync_json_body(&state).await;
        let ages: Vec<u64> = v["syncs"].as_array().unwrap().iter()
            .map(|s| s["published_age_ms"].as_u64().unwrap()).collect();
        let stalest = v["stalest_published_age_ms"].as_u64().unwrap();
        assert_eq!(Some(stalest), ages.iter().copied().max(),
            "the stalest age must be the largest age the body itself reports: {v}");
        assert!(ages.contains(&stalest), "…and must be one of them verbatim: {v}");
    }

    /// A max over no samples is not a measurement. `0` would read as "everything is perfectly
    /// fresh", which is the confident-falsehood shape; absence is the honest answer, and `active`
    /// already explains it.
    #[tokio::test]
    async fn the_stalest_age_is_absent_rather_than_zero_when_nothing_is_running() {
        let state = empty_state();
        let v = asset_sync_json_body(&state).await;
        assert_eq!(v["active"], serde_json::json!(false));
        assert!(v.get("stalest_published_age_ms").is_none(),
            "an aggregate over an empty list must be absent, not 0: {v}");

        // …and it appears as soon as there is something to aggregate over.
        let _g = begin(&state, "common");
        let v = asset_sync_json_body(&state).await;
        assert!(v["stalest_published_age_ms"].is_u64(),
            "a live sync always has an age, so the aggregate always exists: {v}");
    }

    // ── #731: the asset-server login is inside the observed window ──────────────────────────────

    /// Open a LOGIN on the state's own registry — the #731 half of `begin` above.
    fn begin_login(state: &HttpState, purpose: &str) -> eqoxide_ipc::AssetConnectGuard {
        eqoxide_ipc::AssetConnectGuard::begin(&crate::testkit::asset_sync_slot(state), purpose)
    }

    /// #731, the bug. `AssetSync::login()` precedes every `sync_set` and sat OUTSIDE the guarded
    /// window, so a login that is slow, hung, or retrying against an unreachable asset server made
    /// this endpoint answer `{"active": false}` — "no asset sync is running" — while a loader thread
    /// was blocked inside it. The HUD said "Verifying zone assets…"; the API said idle. An agent
    /// polling this has no other channel to reality: it concludes the client is idle and healthy.
    #[tokio::test]
    async fn a_login_in_flight_is_not_reported_as_an_idle_client() {
        let state = empty_state();
        let idle = asset_sync_json_body(&state).await;
        assert_eq!(idle["active"], serde_json::json!(false));

        let _g = begin_login(&state, "zone load: qeynos2");
        let v = asset_sync_json_body(&state).await;
        assert_eq!(v["active"], serde_json::json!(true),
            "the client is BLOCKED inside login() — reporting 'no asset sync is running' here is \
             the #731 falsehood: {v}");
        assert_eq!(v["syncs"].as_array().unwrap().len(), 1, "…and it must be listed: {v}");
        assert_eq!(v["phase"], serde_json::json!("connecting"));
        assert_eq!(v["connecting"]["purpose"], serde_json::json!("zone load: qeynos2"));
        assert_ne!(idle, v, "idle and logging-in must not serialize to the same thing: {v}");
    }

    /// The subtler falsehood the NAIVE fix would have shipped. A login is not a transfer: it has no
    /// set, no chunk count, no byte total and no rate. Reporting it through the sync shape would
    /// make it read as a download stalled at 0 bytes — plausible, well-formed, and false, which is
    /// the failure class this project ranks worst. Zero is a lie; absent is honest.
    #[tokio::test]
    async fn a_login_never_masquerades_as_a_transfer_stalled_at_zero() {
        let state = empty_state();
        let _g = begin_login(&state, "model-sync worker (charmodel sets)");
        let v = asset_sync_json_body(&state).await;

        let entry = &v["syncs"][0];
        assert!(entry.get("downloading").is_none(),
            "a login has no transfer data — the key must be ABSENT, not a zeroed object: {v}");
        assert!(entry.get("set").is_none(),
            "a login serves several sets (or an unbounded queue of them); naming one would be \
             found by a caller looking that set up in `syncs`: {v}");
        assert!(v.get("set").is_none(), "…and the same at the mirrored top level: {v}");

        // Belt and braces, the same scan the phase test uses: no transfer field anywhere in the
        // DATA at any nesting. `semantics` is prose that names them to explain them.
        let mut data = v.clone();
        data.as_object_mut().unwrap().remove("semantics");
        let text = data.to_string();
        for forbidden in ["rate_bytes_per_sec", "\"bytes\"", "chunks_done", "chunks_total",
                          "elapsed_secs"] {
            assert!(!text.contains(forbidden),
                "`{forbidden}` must be unreachable for a login: {v}");
        }
    }

    /// #731's aggregate question, with the sequence that makes it matter: a healthy sync is running
    /// and a login has been blocked for five minutes behind it. A caller following the documented
    /// one-field wedge check must not be told everything is fresh.
    ///
    /// This works with no special case because a login publishes exactly ONCE, at begin, and never
    /// ticks — structurally identical to a sync wedged in `starting` — so its `published_age_ms` is
    /// the whole time it has been blocked.
    #[tokio::test]
    async fn a_hung_login_is_visible_in_the_one_field_wedge_check() {
        let state = empty_state();
        // A login opened five minutes ago and still blocked.
        let _hung = eqoxide_ipc::AssetConnectGuard::begin_stamped(
            &crate::testkit::asset_sync_slot(&state), "zone load: neriakc", ago(300));
        // …and a perfectly healthy sync alongside it, ticking now.
        let sync = begin(&state, "common");
        sync.tick(dl(140, 149, 12_000_000, 13_000));

        let v = asset_sync_json_body(&state).await;
        assert!(v["stalest_published_age_ms"].as_u64().unwrap() >= 300_000,
            "a login blocked for five minutes must make the process-wide wedge check large — \
             otherwise an agent reads 'healthy' while the client holds the evidence: {v}");

        let login = v["syncs"].as_array().unwrap().iter()
            .find(|s| s["phase"] == serde_json::json!("connecting"))
            .expect("the login must be listed");
        assert!(login["published_age_ms"].as_u64().unwrap() >= 300_000, "{v}");
        assert!(login["running_ms"].as_u64().unwrap() >= 300_000,
            "a login's running_ms and published_age_ms are the same duration — it publishes once \
             and never ticks: {v}");
    }

    /// #731: "not started", "in progress", "failed" and "succeeded" must be four distinguishable
    /// answers. A failed login that simply returns the endpoint to `active: false` reproduces the
    /// original bug one moment later — the agent is told nothing happened.
    ///
    /// The verdict is available here and NOT for a set sync because the login wrapper sees the
    /// call's `Result`, whereas the sync guard's `Drop` runs identically on success, error and a
    /// panic unwind (#715's documented limit, unchanged).
    #[tokio::test]
    async fn a_failed_login_is_distinguishable_from_a_client_that_never_tried() {
        let state = empty_state();

        // NOT STARTED.
        let never = asset_sync_json_body(&state).await;
        assert_eq!(never["active"], serde_json::json!(false));
        assert_eq!(never["last_ended"], serde_json::json!(null));

        // IN PROGRESS.
        let g = begin_login(&state, "common asset load");
        let during = asset_sync_json_body(&state).await;
        assert_eq!(during["active"], serde_json::json!(true));

        // FAILED.
        g.finish(eqoxide_ipc::ConnectOutcome::Failed);
        let failed = asset_sync_json_body(&state).await;
        assert_eq!(failed["active"], serde_json::json!(false),
            "nothing is running any more, and saying otherwise would be its own lie");
        assert_eq!(failed["last_ended"]["connecting"]["outcome"], serde_json::json!("failed"),
            "…but an agent must be able to see WHY it is idle: {failed}");
        assert_eq!(failed["last_ended"]["connecting"]["purpose"],
            serde_json::json!("common asset load"));
        assert!(failed["last_ended"].get("set").is_none(),
            "a login has no set, so the ended record must not invent one: {failed}");
        assert_ne!(never["last_ended"], failed["last_ended"],
            "'never tried' and 'tried and failed' must not serialize the same: {failed}");

        // SUCCEEDED — the same shape with the opposite verdict, so the field is a real reading and
        // not a constant that happens to satisfy the assertion above.
        begin_login(&state, "common asset load").finish(eqoxide_ipc::ConnectOutcome::Succeeded);
        let ok = asset_sync_json_body(&state).await;
        assert_eq!(ok["last_ended"]["connecting"]["outcome"], serde_json::json!("succeeded"),
            "{ok}");
        assert_ne!(ok["last_ended"], failed["last_ended"],
            "a successful login and a failed one must not read identically: {ok}");
    }

    /// **#743 review B1, at the wire.** The reviewer blackholed the asset server, all four logins
    /// failed, and across **75 polls at 1.5 s** the genuinely-failed `common asset load` login
    /// appeared in `last_ended` **0 times** — buried by `startup game data`, then `model-sync
    /// worker`, then `zone load: neriakc`. The documented recipe ("had a login fail →
    /// `last_ended.connecting.outcome == 'failed'`") therefore answered *no login failed* while three
    /// had. Accurate fields, false guidance — #731's own shape one level in.
    ///
    /// This replays that sequence through the encoder and asserts what a poller arriving LATE — the
    /// normal case — can still learn.
    #[tokio::test]
    async fn a_failed_login_is_still_answerable_after_later_activity_buries_last_ended() {
        let state = empty_state();

        // The measured order. `common asset load` fails first and is then overwritten three times.
        begin_login(&state, "common asset load").finish(eqoxide_ipc::ConnectOutcome::Failed);
        begin_login(&state, "startup game data (gamedata, gameequip)")
            .finish(eqoxide_ipc::ConnectOutcome::Failed);
        begin_login(&state, "model-sync worker (charmodel sets)")
            .finish(eqoxide_ipc::ConnectOutcome::Failed);
        drop(begin(&state, "zonedoors/neriakc"));   // a SET SYNC is enough to bury a login verdict

        let v = asset_sync_json_body(&state).await;

        // The behaviour the retracted guidance mis-described, pinned as what it really is.
        assert_eq!(v["last_ended"]["set"], serde_json::json!("zonedoors/neriakc"),
            "last_ended is one slot shared by logins and syncs; this is the measured overwrite: {v}");
        assert!(v["last_ended"].get("connecting").is_none(),
            "…so at this instant last_ended says nothing about any login at all: {v}");

        // What a caller polling here CAN learn — the fields the fix adds.
        assert_eq!(v["login_outcomes"]["failed"], serde_json::json!(3),
            "three logins failed and the body must say so however late the poll arrives: {v}");
        assert_eq!(v["login_outcomes"]["succeeded"], serde_json::json!(0), "{v}");
        assert_eq!(v["last_login_failed"]["connecting"]["purpose"],
            serde_json::json!("model-sync worker (charmodel sets)"),
            "the most recent FAILED login, not the most recent activity: {v}");
        assert_eq!(v["last_login_failed"]["connecting"]["outcome"], serde_json::json!("failed"));
        assert!(v["last_login_failed"].get("set").is_none(),
            "a login never had a set, so the retained record must not invent one: {v}");
        assert!(v["last_login_failed"]["ago_ms"].is_u64(), "{v}");

        // And a later SUCCESS must not walk any of it back — a "last login outcome" field would.
        begin_login(&state, "zone load: qeynos2").finish(eqoxide_ipc::ConnectOutcome::Succeeded);
        let after = asset_sync_json_body(&state).await;
        assert_eq!(after["login_outcomes"]["failed"], serde_json::json!(3),
            "counters are monotonic: a success is not the absence of a failure: {after}");
        assert_eq!(after["login_outcomes"]["succeeded"], serde_json::json!(1), "{after}");
        assert_eq!(after["last_login_failed"]["connecting"]["purpose"],
            serde_json::json!("model-sync worker (charmodel sets)"),
            "…and only another FAILED login may overwrite this: {after}");
        assert_eq!(after["last_login_succeeded"]["connecting"]["purpose"],
            serde_json::json!("zone load: qeynos2"),
            "…the success goes in its own slot, where it displaces nothing: {after}");
        assert_eq!(after["last_ended"]["connecting"]["outcome"], serde_json::json!("succeeded"),
            "…while last_ended honestly reports the most recent thing, the success: {after}");
    }

    /// **#743 review B3, at the wire — the reviewer's two probes, verbatim in substance.**
    ///
    /// Round 2 served one `last_login_failure` holding the most recent non-success of EITHER kind,
    /// and documented it as the answer to two questions. The reviewer wrote two probes, one per
    /// documented row, and both were RED against that body: a panic followed by a failure left
    /// `unknown == 1` while the one slot named the FAILED login, so a caller following the row for
    /// panics read a different login's purpose — and the panicked login's identity was in the body
    /// nowhere at all.
    ///
    /// Both scenarios are reproduced here unchanged, down to the purposes. What changed is the field
    /// each row points at: there is now one slot PER OUTCOME, so both rows are true simultaneously
    /// and nothing is lost. The two assertions the probes could not make against round 2 — that the
    /// EARLIER login is still fully recoverable — are the point of this test.
    #[tokio::test]
    async fn a_failure_and_a_panic_are_each_recoverable_in_full_because_neither_shares_a_slot() {
        // Probe 1: panic, THEN failure. `unknown == 1`, and the row for panics must lead to the
        // login that actually panicked.
        //
        // A panic unwinding through a login is `Unknown`; the guard's `Drop` records it with no
        // `finish`, so dropping an unfinished guard is that path exactly.
        let state = empty_state();
        drop(begin_login(&state, "model-sync worker (charmodel sets)"));      // unknown
        begin_login(&state, "common asset load").finish(eqoxide_ipc::ConnectOutcome::Failed);

        let v = asset_sync_json_body(&state).await;
        assert_eq!(v["login_outcomes"], serde_json::json!({
            "succeeded": 0, "failed": 1, "unknown": 1,
        }), "both kinds happened and both counts must say so: {v}");
        assert_eq!(v["last_login_unknown"]["connecting"], serde_json::json!({
            "purpose": "model-sync worker (charmodel sets)", "outcome": "unknown",
        }), "unknown == 1, so the record beside that counter must be the login that PANICKED — \
             round 2 handed back the failed login's purpose here: {v}");
        assert_eq!(v["last_login_failed"]["connecting"], serde_json::json!({
            "purpose": "common asset load", "outcome": "failed",
        }), "…and the later failure did not displace it, because it has its own slot: {v}");

        // Probe 2, the mirror: failure, THEN panic. Round 2 lost the failure's identity here.
        let state = empty_state();
        begin_login(&state, "common asset load").finish(eqoxide_ipc::ConnectOutcome::Failed);
        drop(begin_login(&state, "zone load: neriakc"));                      // unknown
        let v = asset_sync_json_body(&state).await;
        assert_eq!(v["login_outcomes"]["failed"], serde_json::json!(1), "{v}");
        assert_eq!(v["last_login_failed"]["connecting"], serde_json::json!({
            "purpose": "common asset load", "outcome": "failed",
        }), "failed == 1, and the record beside that counter is the login that FAILED — a later \
             panic cannot overwrite it: {v}");
        assert_eq!(v["last_login_unknown"]["connecting"], serde_json::json!({
            "purpose": "zone load: neriakc", "outcome": "unknown",
        }), "{v}");

        // The rule that makes the two rows independently true, asserted as a rule and not just for
        // this pair: for EVERY outcome, the count and the record agree about the same login.
        for token in ["succeeded", "failed", "unknown"] {
            let counted = v["login_outcomes"][token].as_u64().expect("present with zeros");
            let record = v.get(format!("last_login_{token}"));
            assert_eq!(counted > 0, record.is_some(),
                "login_outcomes.{token} > 0 must be exactly when last_login_{token} is present: {v}");
            if let Some(r) = record {
                assert_eq!(r["connecting"]["outcome"], serde_json::json!(token),
                    "…and last_login_{token} must name a login whose outcome IS {token}, which is \
                     what round 2's single shared slot could not promise: {v}");
            }
        }
    }

    /// The absent-not-null rule, PER OUTCOME (#743, tightened for review B3). A client that has never
    /// had a login end a given way must get a real negative answer for that way — not a `null`, and
    /// not another outcome's record standing in for it.
    #[tokio::test]
    async fn a_client_with_no_login_failure_says_so_by_absence_and_by_zero() {
        let state = empty_state();
        let v = asset_sync_json_body(&state).await;
        for token in ["succeeded", "failed", "unknown"] {
            assert!(v.get(format!("last_login_{token}")).is_none(),
                "absent, never null: no login has ended {token}: {v}");
        }
        assert_eq!(v["login_outcomes"],
            serde_json::json!({ "succeeded": 0, "failed": 0, "unknown": 0 }),
            "the counters are PRESENT with zeros — unlike stalest_published_age_ms, a count of zero \
             here is a real measurement, not a max over no samples: {v}");

        // A succeeded login moves one counter and one slot, and fabricates neither of the others.
        begin_login(&state, "common asset load").finish(eqoxide_ipc::ConnectOutcome::Succeeded);
        let ok = asset_sync_json_body(&state).await;
        assert_eq!(ok["login_outcomes"]["succeeded"], serde_json::json!(1), "{ok}");
        assert_eq!(ok["last_login_succeeded"]["connecting"]["purpose"],
            serde_json::json!("common asset load"), "{ok}");
        assert!(ok.get("last_login_failed").is_none(),
            "a successful login must not fabricate a failure record: {ok}");
        assert!(ok.get("last_login_unknown").is_none(), "{ok}");

        // A panic through a login is a non-success and is retained as one, under its own outcome —
        // and, since B3, in its own field, where no failure can overwrite it.
        let state2 = empty_state();
        let s2 = crate::testkit::asset_sync_slot(&state2);
        let _ = std::panic::catch_unwind(move || {
            let _g = eqoxide_ipc::AssetConnectGuard::begin(&s2, "model-sync worker (charmodel sets)");
            panic!("boom");
        });
        let p = asset_sync_json_body(&state2).await;
        assert_eq!(p["login_outcomes"],
            serde_json::json!({ "succeeded": 0, "failed": 0, "unknown": 1 }),
            "a panic is neither Ok nor Err and must be counted as neither: {p}");
        assert_eq!(p["last_login_unknown"]["connecting"]["outcome"], serde_json::json!("unknown"),
            "…but it did NOT succeed, so it is retained — under its own outcome: {p}");
        assert!(p.get("last_login_failed").is_none(),
            "…and NOT under `failed`, which is the counter an alarm is most likely wired to: {p}");
    }

    /// A login is one entry among many and must obey the same ownership rules — a login finishing
    /// cannot blank a zone download still in flight, and vice versa (#726's property, extended to
    /// the new entry kind).
    #[tokio::test]
    async fn a_login_finishing_does_not_erase_a_sync_that_is_still_running() {
        let state = empty_state();
        let zone = begin(&state, "zone/neriakc");
        zone.tick(dl(120, 374, 10_000_000, 8_000));
        let login = begin_login(&state, "model-sync worker (charmodel sets)");

        let both = asset_sync_json_body(&state).await;
        assert_eq!(both["syncs"].as_array().unwrap().len(), 2,
            "a login opened during a zone download is a second activity, not a replacement: {both}");
        assert_eq!(both["set"], serde_json::json!("zone/neriakc"),
            "…and the oldest-started primary is still the zone download: {both}");

        login.finish(eqoxide_ipc::ConnectOutcome::Succeeded);
        let v = asset_sync_json_body(&state).await;
        assert_eq!(v["active"], serde_json::json!(true), "{v}");
        assert_eq!(v["set"], serde_json::json!("zone/neriakc"));
        assert_eq!(v["downloading"]["chunks_done"], serde_json::json!(120),
            "…with its progress intact, not reset: {v}");
        assert_eq!(v["syncs"].as_array().unwrap().len(), 1);
    }

    /// The round-2 defect was not a wrong value — it was wrong GUIDANCE, and it shipped inside the
    /// response. So the guidance is now pinned to the behaviour: every field `semantics` sends a
    /// caller to must actually exist in the body it is describing. A later change that drops or
    /// renames one fails here, instead of leaving the string quietly recommending a field the
    /// endpoint no longer serves — which is the same class of defect in a new place.
    #[tokio::test]
    async fn every_field_the_semantics_string_sends_a_caller_to_exists_in_the_body() {
        let state = empty_state();
        let g = begin(&state, "zone/neriakc");
        g.tick(dl(3, 7, 12_451_840, 10_400));
        let v = asset_sync_json_body(&state).await;
        let semantics = v["semantics"].as_str().expect("semantics is served").to_string();

        for field in ["active", "syncs", "last_ended", "phase", "downloading",
                      "published_age_ms", "running_ms", "stalest_published_age_ms",
                      "login_outcomes"] {
            assert!(semantics.contains(field), "semantics must document `{field}`: {semantics}");
            assert!(v.get(field).is_some(),
                "semantics sends the caller to `{field}`, so the body must carry it: {v}");
        }

        // #743 review N2: `get(..).is_some()` above is satisfied by JSON `null`, so on its own it
        // would pass for a field the encoder emits as null. Every field named here except
        // `last_ended` must carry a REAL value; `last_ended` is legitimately null in this state
        // (nothing has ended yet) and that null IS the documented answer, which is why it is the one
        // exception rather than a gap in the loop.
        for field in ["active", "syncs", "phase", "downloading", "published_age_ms",
                      "running_ms", "stalest_published_age_ms", "login_outcomes"] {
            assert!(!v[field].is_null(),
                "`{field}` must be a real value, not null — an assertion that only checks the key \
                 exists passes vacuously on null: {v}");
        }
        assert!(v["last_ended"].is_null(),
            "…and `last_ended` really is null here, so the exception above is exercised, not \
             assumed: {v}");

        // #743 review N6. The wire string offers a monotonic counter and a between-polls delta;
        // clients restart routinely and the counters restart with them, so an unscoped monotonicity
        // claim makes a persisted-cursor poller compute a false delta. The scope has to be IN THE
        // STRING — that is what an agent reads; prose in docs/http-api.md it will never see does not
        // discharge the claim.
        assert!(semantics.contains("WITHIN ONE CLIENT PROCESS"),
            "the monotonicity of login_outcomes must be scoped to the process on the wire: \
             {semantics}");
        assert!(semantics.contains("restart"),
            "…and a poller must be told what a decrease means, or it will read one as a \
             correction: {semantics}");

        // The retracted claim, pinned so it cannot come back: the mirror is not a process-health
        // check and a caller asking "is anything wedged" does have to look past it.
        assert!(!semantics.contains("need not iterate"),
            "the top-level mirror describes syncs[0] alone; telling a caller otherwise is the \
             finding this test exists to keep closed: {semantics}");
    }

    /// The same both-directions pin, for the state #731 adds — because a new state that slips past
    /// the guidance test is exactly how round 3's defect (accurate values, false advice) would come
    /// back. A `connecting` entry has a DIFFERENT field set from a sync — no `set`, no
    /// `downloading` — so the sync-state test above cannot cover it, and a body that documents
    /// fields it does not serve is a lie an agent cannot detect.
    #[tokio::test]
    async fn the_semantics_string_and_the_body_agree_in_the_connecting_state_too() {
        let state = empty_state();
        let _g = begin_login(&state, "zone load: neriakc");
        let v = asset_sync_json_body(&state).await;
        let semantics = v["semantics"].as_str().expect("semantics is served").to_string();

        // Direction 1: everything the guidance names for THIS state must be in the body.
        for field in ["active", "syncs", "last_ended", "phase", "connecting",
                      "published_age_ms", "running_ms", "stalest_published_age_ms",
                      "login_outcomes"] {
            assert!(semantics.contains(field), "semantics must document `{field}`: {semantics}");
            assert!(v.get(field).is_some(),
                "semantics sends the caller to `{field}`, so a connecting body must carry it: {v}");
        }

        // #743 review N2, the finding that motivated this: `is_some()` is satisfied by JSON `null`,
        // and in the connecting state `last_ended` IS null — so that iteration of the loop above was
        // passing vacuously. Everything else must be a real value; `last_ended` is the one field
        // whose null is itself the honest answer here, and it is asserted as such rather than left
        // silently exempt.
        for field in ["active", "syncs", "phase", "connecting", "published_age_ms",
                      "running_ms", "stalest_published_age_ms", "login_outcomes"] {
            assert!(!v[field].is_null(),
                "`{field}` must be a real value, not null: {v}");
        }
        assert!(v["last_ended"].is_null(),
            "nothing has ended yet, so `last_ended` is null — the case the loop above could not \
             have distinguished from a missing value: {v}");

        // Direction 1b (#743 review B1): the guidance must send the caller to the field that can
        // actually answer "did a login fail", and must NOT still be sending them to `last_ended`
        // for it. The retracted recipe is pinned closed the same way #726's was.
        assert!(semantics.contains("login_outcomes"),
            "the guidance must name the fields that survive later activity: {semantics}");
        assert!(semantics.contains("Do NOT ask last_ended whether a login failed"),
            "…and must say plainly that the single-slot field cannot answer it — the measured \
             defect was guidance, not a value: {semantics}");
        // #743 review B3: every retained slot the guidance names must be a real field name, and
        // there must be one PER OUTCOME. A string that names a slot the encoder does not emit is the
        // same class of defect as a slot the string does not name.
        // #743 round-3 review N2: driven by `ConnectOutcome::ALL`, not by a hard-coded list of three
        // tokens. The wire string is a SECOND enumeration of the outcomes and it cannot follow `ALL`
        // on its own, so an outcome added to `ALL` would otherwise be emitted on the wire while this
        // string still promised the old set, with no test noticing.
        for outcome in eqoxide_ipc::ConnectOutcome::ALL {
            let token = outcome.as_str();
            assert!(semantics.contains(&format!("last_login_{token}")),
                "the guidance must name `last_login_{token}` — one slot per outcome is the whole \
                 B3 fix, and guidance that names only some of them re-opens it: {semantics}");
            assert!(v["login_outcomes"].get(token).is_some(),
                "…and `login_outcomes` must carry a counter for every outcome in `ALL`: {v}");
            assert!(v.get(format!("last_login_{token}")).is_none(),
                "no login has ended {token} in this state, so it is ABSENT, not null: {v}");
        }
        assert!(!semantics.contains("last_login_failure"),
            "…and the retracted shared slot must not be named at all: it was ONE field promised to \
             two outcomes, and a caller who greps for it must not find guidance for it: {semantics}");

        // #743 review N7: the guidance must carry the MECHANISM, not one run's sample count. A rate
        // read off a single blackholed run ("0 of 75 polls") is a result, not a property of the
        // endpoint, and a reader can take it as guidance about how often this happens. The number
        // lives in the rustdoc on `AssetSyncSlots::last_ended` with its context.
        assert!(semantics.contains("the next activity to end, login or set sync, overwrites it"),
            "the wire guidance must state WHY last_ended cannot answer it: {semantics}");
        for digits in ["75", "0 of"] {
            assert!(!semantics.contains(digits),
                "the wire string must not cite one run's sample count as guidance (`{digits}`): \
                 {semantics}");
        }

        // Direction 2: the guidance must state the two ABSENCES, because a caller who does not know
        // `set` can be missing will read its absence as a bug or, worse, as the previous entry's.
        assert!(semantics.contains("NO `set`") || semantics.contains("ABSENT when syncs[0] is a login"),
            "the guidance must say a login carries no set, since the body omits it: {semantics}");
        assert!(v["syncs"][0].get("set").is_none(), "…and it really is omitted: {v}");
        assert!(v["syncs"][0].get("downloading").is_none(), "…as is the transfer object: {v}");

        // Direction 3: the phase tag the guidance quotes must be the one the encoder emits. A
        // rename on either side fails here rather than leaving the string recommending a value the
        // endpoint never produces.
        assert!(semantics.contains("connecting"), "{semantics}");
        assert_eq!(v["syncs"][0]["phase"], serde_json::json!("connecting"), "{v}");

        // Direction 4: the outcome vocabulary. Every outcome must be both documented and producible,
        // so a caller branching on them cannot hit a value that was documented but never emitted (or
        // a value emitted but never documented).
        //
        // #743 round-3 review N2: iterated from `ConnectOutcome::ALL` rather than from a hard-coded
        // three-row table, so this is a check on the alphabet and not a restatement of it. The table
        // could not have caught a fourth outcome; this can.
        for outcome in eqoxide_ipc::ConnectOutcome::ALL {
            let tag = outcome.as_str();
            assert!(semantics.contains(tag), "semantics must document outcome `{tag}`: {semantics}");
            let s2 = empty_state();
            begin_login(&s2, "p").finish(outcome);
            let b = asset_sync_json_body(&s2).await;
            assert_eq!(b["last_ended"]["connecting"]["outcome"], serde_json::json!(tag),
                "the documented token `{tag}` must be the one actually served: {b}");
        }
    }

    /// #776/#801 — the trapped-swimmer disclosure must reach a RESPONSE BODY, not just a struct.
    ///
    /// **This test exists because #810's round-1 review found the value unreachable.** Six files of
    /// the publication path were correct — controller, `ControllerView`, `stream_position`,
    /// `GameState`, `PlayerState`, docs — and `GET /v1/observe/debug` still had no `afloat_stall`
    /// key, because `PlayerState` is an internal projection that no handler serialises whole:
    /// `get_debug` hand-builds its `player` object and patches extras in with `player.insert`. The
    /// three tests originally shipped for this called `serde_json::to_value(&player_state)`
    /// directly — a body no client ever emits — so all three were green against a dead end.
    ///
    /// So this one goes through `debug_json`, which drives the REAL router with a REAL request and
    /// parses the REAL bytes. The lesson is worth stating in the file rather than the PR: a test
    /// that constructs the value it asserts on cannot tell you the value is reachable.
    ///
    /// **Axes deliberately varied:** stall present vs absent, and the anchor on all three
    /// components including a negative `up` — #800's live false alarm was a z-axis bug that seven
    /// tests missed because every one pinned `z = 0.0`, and a fixture flattening `anchor_up` here
    /// would be repeating it. **Axis NOT varied:** `secs` is whatever the real clock produced; the
    /// assertion is against the published threshold, not a hard-coded duration.
    ///
    /// MUTATION CHECKS (#801 round 2, each run independently on the remote builder, restored from
    /// an `md5sum`-verified copy between runs, `-p eqoxide-http --lib`; both were RED against a
    /// 263-passed/0-failed control):
    ///
    /// * delete the `player.insert("afloat_stall".into(), ..)` from `get_debug` → **262 passed,
    ///   1 failed**, at the `contains_key` assertion. The failure message printed the 55 keys that
    ///   remain, which is exactly the key set the round-1 reviewer observed live;
    /// * wrap that same insert in `if player_afloat_stall.is_some()`, i.e. omit the key instead of
    ///   serving `null` → **262 passed, 1 failed**, at the SAME `contains_key` assertion, not at the
    ///   null one. (This line first said "at the null assertion", which is where it would fail if
    ///   `contains_key` were not checked first; the transcript says `contains_key`. Corrected from
    ///   the observed panic location rather than re-reasoned — #810 round-2 review, N2, in a repo
    ///   where reasoned-not-transcribed is the dominant defect class.) This is still the mutation
    ///   worth having: it leaves the stall case fully working and breaks only the
    ///   always-present contract, which is the half a hand-written happy-path test would miss.
    ///
    /// The `hold` assertion at the tail is NOT part of either mutation's coverage; see the comment
    /// on it for why it was rewritten.
    ///
    /// NOT a useful mutation here, contrary to the obvious guess: adding `skip_serializing_if` to
    /// `PlayerState::afloat_stall` changes nothing this test can see, because `get_debug` never
    /// serialises `PlayerState` — it reads the field and inserts it. Recorded so the next person
    /// does not run it and conclude the test is weak.
    #[tokio::test]
    async fn afloat_stall_reaches_the_debug_json_801() {
        use eqoxide_core::afloat::{AfloatFrame, AfloatStallClock, AFLOAT_STALL_SECS};

        // ── Absent: an explicit `null` that IS in the object, never an omitted key. ──────────────
        let v = debug_json(empty_state()).await;
        let player = v["player"].as_object().expect("player object");
        assert!(player.contains_key("afloat_stall"),
            "the afloat_stall key must be PRESENT in the served body — an agent that greps for it \
             and finds nothing cannot tell \"no stall\" from \"this client cannot report one\", and \
             the docs promise the key is always there. Keys served: {:?}",
            player.keys().collect::<Vec<_>>());
        assert!(player["afloat_stall"].is_null(),
            "no stall must serialise as an explicit null, got {}", player["afloat_stall"]);

        // ── Present: a REAL matured stall — `AfloatStall` has no public constructor, so this
        // fixture cannot lie about what the runtime would produce. ───────────────────────────────
        let anchor = [-812.5_f32, 43.0, -119.75];
        let mut clock = AfloatStallClock::default();
        for _ in 0..((AFLOAT_STALL_SECS / 0.05).ceil() as usize + 3) {
            clock.observe(AfloatFrame::Wished, anchor, 0.05);
        }
        let stall = clock.stall().expect("fixture must actually reach the disclosure threshold");

        let state = empty_state();
        set_gs(&state, |gs| gs.player_afloat_stall = Some(stall));
        let v = debug_json(state).await;
        let a = &v["player"]["afloat_stall"];
        assert!(a.is_object(), "a stall in the GameState must reach the body, got {a}");
        assert_eq!(a["anchor_east"],  serde_json::json!(anchor[0]));
        assert_eq!(a["anchor_north"], serde_json::json!(anchor[1]));
        assert_eq!(a["anchor_up"],    serde_json::json!(anchor[2]),
            "a submerged pocket is BELOW the waterline — an anchor flattened to 0.0 would publish \
             a point the agent cannot navigate to");
        assert!(a["secs"].as_f64().expect("secs is a number") >= AFLOAT_STALL_SECS as f64,
            "secs must never be served below the threshold that earned it, got {}", a["secs"]);
        assert_eq!(a["stall_threshold_secs"], serde_json::json!(AFLOAT_STALL_SECS),
            "the threshold is published so the agent can calibrate rather than guess");
        assert!(a["detail"].as_str().expect("detail is a string").to_ascii_lowercase().contains("dive"),
            "the detail must name the escape that usually works — an agent told only \"stalled\" \
             has no reason to try a vertical wish: {}", a["detail"]);

        // ── And a stall is NOT published as a `hold` — two different claims (#801/#817). ──────────
        //
        // This was written as `assert!(v["player"]["hold"].is_null())` and #810's round-2 review
        // measured it VACUOUS: `hold` was not a key in this body at all (#817), and `serde_json`
        // returns `Value::Null` for a key that is absent, so the assertion could not fail and could
        // not catch the mutation it named. #817 landed the `player.insert("hold".into(), …)` in
        // `get_debug` (see the comment there) — the key is now genuinely served, so this checks the
        // real claim: a stall in force with no hold in force reads `player.hold` as an explicit
        // `null`, not the stall's own value miscast as a hold.
        let player = v["player"].as_object().expect("player object");
        assert!(player.contains_key("hold"),
            "the hold key must be PRESENT in the served body (#817) — an agent that greps for it \
             and finds nothing cannot tell \"not stuck\" from \"this client cannot report one\". \
             Keys served: {:?}",
            player.keys().collect::<Vec<_>>());
        assert!(player["hold"].is_null(),
            "a stall in force with no hold in force must serialise player.hold as an explicit \
             null, not the stall's own value — collapsing the two tells an agent to give up on a \
             body it could free itself with a driven dive, got {}", player["hold"]);
    }

    /// #852 — `/v1/observe/debug`'s `camera` object must publish the RENDERED eye, not just the
    /// desired orbit `radius`/`focus`/`azimuth`/`elevation`. Before this fix those four were the
    /// only camera fields served, and none of them reflect the collision pull-in the render loop
    /// applies each frame — an agent reading them cannot tell an occluded frame from a clear one
    /// (measured: 88% of pull-in frames disagreed — see #852). Same failure shape as
    /// `afloat_stall_reaches_the_debug_json_801`/`hold_reaches_the_debug_json_817`: this drives
    /// the REAL router with a REAL request and parses the REAL bytes, because a struct field that
    /// is correct but never reaches the served JSON is exactly what those two exist to catch too.
    ///
    /// `radius`/`focus` are deliberately set FAR from the fixture `eye` below, so a test that
    /// accidentally read a radius-derived position instead of the new `eye` field would fail loud
    /// rather than passing by coincidence.
    ///
    /// MUTATION CHECK (#852, executed): deleting the `"eye"`/`"occluded"`/`"still_blocked"`
    /// entries from the `"camera"` object literal in `get_debug` turns the first `contains_key`
    /// assertion here RED; restoring them turns it green again.
    #[tokio::test]
    async fn camera_eye_reaches_the_debug_json_852() {
        let state = empty_state();
        {
            let mut snap = state.camera.snapshot.lock().unwrap();
            snap.eye           = [12.5, -34.0, 56.75];
            snap.occluded      = true;
            snap.still_blocked = true;
            snap.radius        = 80.0;
            snap.focus         = [0.0, 0.0, 0.0];
        }
        let v = debug_json(state).await;
        let cam = v["camera"].as_object().expect("camera object");
        assert!(cam.contains_key("eye"),
            "camera.eye must be PRESENT in the served body — an agent that greps for it and finds \
             nothing cannot tell \"unoccluded\" from \"this client cannot report one\". Keys \
             served: {:?}", cam.keys().collect::<Vec<_>>());
        assert_eq!(cam["eye"], serde_json::json!([12.5, -34.0, 56.75]),
            "camera.eye must be the RENDERED eye carried through from the snapshot, not a position \
             derived from radius/focus (that recomputation is the #852 bug)");
        assert_eq!(cam["occluded"], serde_json::json!(true),
            "camera.occluded must say a pull-in fired this frame");
        assert_eq!(cam["still_blocked"], serde_json::json!(true),
            "camera.still_blocked must say the eye is still not fully clear of collision");
    }

    /// #797 (added in #900's review round 2) — the renderer's skin-cap downgrade report must reach
    /// a RESPONSE BODY, not merely an `HttpState` field. This is #797's own failure mode one layer
    /// up: `EqRenderer::skin_cap_downgrades` existed since #795/#820 and was perfectly public, and
    /// the driving agent still could not read it, because nothing carried it to a response. A
    /// serving path that quietly stops serving reproduces exactly that, and the round-1 reviewer
    /// measured that it could: deleting the `"skin_cap_downgrades"` entry from `get_debug`'s JSON
    /// literal was compile-clean and left the whole workspace green.
    ///
    /// Same shape as `camera_eye_reaches_the_debug_json_852` above and
    /// `hold_reaches_the_debug_json_817` below: drive the REAL router through `debug_json`, parse
    /// the REAL bytes, and assert on both the empty and the populated case — the empty case pins
    /// that the key is always PRESENT (an agent that greps and finds nothing cannot tell "nothing
    /// downgraded" from "this client predates the field"), the populated case pins that a
    /// downgrade the renderer actually recorded is carried through with both sub-fields intact.
    ///
    /// MUTATION CHECK (remote builder, both directions, run for #900 round 2 and reported in that
    /// PR comment — not reasoned):
    /// - **DELETE** the `"skin_cap_downgrades": skin_cap_downgrades,` entry from `get_debug`'s JSON
    ///   literal → RED at the `contains_key` assertion below.
    /// - **WRAP** the `let skin_cap_downgrades = …lock().unwrap().clone();` read in `if false { … }`
    ///   (leaving an empty `BTreeMap` behind) → RED at the populated-case assertions. The WRAP is
    ///   run because DELETE alone does not distinguish "this check runs" from "this check is merely
    ///   written" — eqoxide#799, eight measured cases in this repo.
    #[tokio::test]
    async fn skin_cap_downgrades_reaches_the_debug_json_797() {
        // ── Empty: an explicit `{}` that IS in the object, never an omitted key. ────────────────
        let v = debug_json(empty_state()).await;
        let obj = v.as_object().expect("debug object");
        assert!(obj.contains_key("skin_cap_downgrades"),
            "skin_cap_downgrades must be PRESENT in the served body even when empty — an agent \
             that greps for it and finds nothing cannot tell \"nothing has downgraded\" from \
             \"this client cannot report downgrades at all\". Top-level keys served: {:?}",
            obj.keys().collect::<Vec<_>>());
        assert_eq!(v["skin_cap_downgrades"], serde_json::json!({}),
            "with nothing recorded it must serialise as an empty OBJECT, not null and not omitted, \
             got {}", v["skin_cap_downgrades"]);

        // ── Populated: what the renderer recorded, carried through with both sub-fields. ────────
        let state = empty_state();
        {
            let mut recorded = state.skin_cap_downgrades.lock().unwrap();
            recorded.insert("race_hum.glb".to_string(),
                eqoxide_ipc::SkinCapDowngradeView { joint_count: 190, key_collision: false });
            // The #848 case: one key two different files have written. `key_collision` is the only
            // thing in the response that discloses it, so it has to survive the trip.
            recorded.insert("race_pcfroglok.glb".to_string(),
                eqoxide_ipc::SkinCapDowngradeView { joint_count: 204, key_collision: true });
        }
        let v = debug_json(state).await;
        let served = v["skin_cap_downgrades"].as_object()
            .expect("skin_cap_downgrades must be an OBJECT in the served body");
        assert_eq!(served.len(), 2,
            "every recorded downgrade must be served, not just the first — served: {:?}",
            served.keys().collect::<Vec<_>>());
        assert_eq!(v["skin_cap_downgrades"]["race_hum.glb"],
            serde_json::json!({ "joint_count": 190, "key_collision": false }),
            "the recorded joint count and collision flag must both reach the body verbatim");
        assert_eq!(v["skin_cap_downgrades"]["race_pcfroglok.glb"]["key_collision"],
            serde_json::json!(true),
            "key_collision: true must reach the body — it is the ONLY signal an agent has that this \
             entry's joint_count is not reliably attributable to one file (eqoxide#848)");
        assert_eq!(v["skin_cap_downgrades"]["race_pcfroglok.glb"]["joint_count"],
            serde_json::json!(204),
            "a colliding entry still carries its joint count; the flag qualifies it, not replaces it");
    }

    /// #867 — the camera block's FRESHNESS signal must reach the served body, in both states.
    ///
    /// Every other field in `camera` looks identical on a snapshot published this tick and on one
    /// frozen since the window was minimised (`about_to_wait` stops requesting redraws 300 ms after
    /// the last activity, so a persistently `Outdated` surface freezes it indefinitely). The
    /// neighbouring `snapshot_age_ms` is the NETWORK clock and stays fresh right through such a
    /// stall, so an agent that reaches for it is actively misled. `drawn_frame`/`drawn_age_ms` are
    /// the only way to tell — which makes their PRESENCE in the response the thing worth pinning,
    /// exactly as `camera_eye_reaches_the_debug_json_852` above pins `eye`'s.
    ///
    /// Both states are asserted because they fail differently: a `null` `drawn_frame` that never
    /// reaches the JSON reads as "this client cannot report one", while a `Some` that never reaches
    /// it reads as "nothing has been drawn" — opposite wrong answers from the same omission.
    ///
    /// MUTATION CHECK (executed, both directions): deleting the `"drawn_frame"`/`"drawn_age_ms"`
    /// entries from `get_debug`'s `"camera"` literal turns the first `contains_key` RED; leaving
    /// them but publishing `drawn_frame: None` in the drawn case turns the `Some(4242)` assertion
    /// RED.
    #[tokio::test]
    async fn camera_freshness_reaches_the_debug_json_867() {
        // (a) never drawn — the state `main.rs` seeds before the event loop exists.
        let state = empty_state();
        let v = debug_json(state).await;
        let cam = v["camera"].as_object().expect("camera object");
        assert!(cam.contains_key("drawn_frame") && cam.contains_key("drawn_age_ms"),
            "camera.drawn_frame/drawn_age_ms must be PRESENT in the served body — without them an \
             agent cannot tell a snapshot published this tick from one frozen since the window was \
             minimised. Keys served: {:?}", cam.keys().collect::<Vec<_>>());
        assert_eq!(cam["drawn_frame"], serde_json::json!(null),
            "before any frame is drawn, camera.drawn_frame must be null — the startup seed is a \
             plausible-looking orbit position that nothing was ever rendered from (#867)");
        assert_eq!(cam["drawn_age_ms"], serde_json::json!(null),
            "…and drawn_age_ms must be null rather than an age since process start, which would \
             read as a recent draw");

        // (b) drawn — a snapshot the render loop would publish after a real `render_frame`.
        let state = empty_state();
        {
            let mut snap = state.camera.snapshot.lock().unwrap();
            snap.drawn_frame = Some(4242);
            snap.drawn_at    = Some(std::time::Instant::now());
        }
        let v = debug_json(state).await;
        let cam = v["camera"].as_object().expect("camera object");
        assert_eq!(cam["drawn_frame"], serde_json::json!(4242),
            "camera.drawn_frame must carry the published frame index through verbatim — polling it \
             for change is how a caller detects that drawing has stopped");
        assert!(cam["drawn_age_ms"].as_u64().is_some(),
            "camera.drawn_age_ms must be a number once a frame has been drawn, computed at READ \
             time (an age computed at publish time would itself go stale). Got: {:?}",
            cam["drawn_age_ms"]);
    }

    /// #724/#817 — the stuck-and-cannot-free disclosure must reach a RESPONSE BODY, not just a
    /// struct. Same failure shape as `afloat_stall_reaches_the_debug_json_801` just above:
    /// `PlayerState::hold` was computed, mirrored into `GameState` on every **net tick** by
    /// `ActionLoop::stream_position` (that function's own rustdoc: "Runs every tick"), from a
    /// `ControllerView` snapshot the **render** thread republishes on every rendered frame — so
    /// the mirror is as fresh as the last *published* frame, not as fresh as the last *net* tick —
    /// and covered by tests since #724 landed, and reached NO
    /// response body, because `get_debug` hand-builds its `player` object and never serialises
    /// `PlayerState` whole. This goes through `debug_json`, which drives the REAL router with a
    /// REAL request and parses the REAL bytes, for the same reason the afloat_stall test does.
    ///
    /// Both `ControllerHoldReason` variants are exercised (not just one), so the `reason`/`detail`
    /// match arms aren't covered by a single fluke case. Those arms live in `PlayerHoldView::of`
    /// since #884 moved them out of `PlayerState::from_game_state`, which is the whole point of
    /// that move: one constructor, one text, read by both this endpoint and the `/v1/move/*`
    /// held-refusal.
    ///
    /// MUTATION CHECK reported live in the #817 PR body (build + run on the remote builder, not
    /// reasoned): delete the `player.insert("hold".into(), …)` line from `get_debug` and confirm
    /// this test's first `contains_key` assertion goes RED.
    #[tokio::test]
    async fn hold_reaches_the_debug_json_817() {
        use eqoxide_core::game_state::{ControllerHold, ControllerHoldReason};

        // ── Absent: an explicit `null` that IS in the object, never an omitted key. ──────────────
        let v = debug_json(empty_state()).await;
        let player = v["player"].as_object().expect("player object");
        assert!(player.contains_key("hold"),
            "the hold key must be PRESENT in the served body — an agent that greps for it and \
             finds nothing cannot tell \"not stuck\" from \"this client cannot report one\". Keys \
             served: {:?}", player.keys().collect::<Vec<_>>());
        assert!(player["hold"].is_null(),
            "no hold must serialise as an explicit null, got {}", player["hold"]);

        // ── Present: EmbeddedNoRecovery. ──────────────────────────────────────────────────────────
        let state = empty_state();
        // 12.5, not 12.4: exactly representable in binary floating point (12 + 1/2) at both f32 and
        // f64, so the JSON round-trip through the real router cannot introduce a spurious ULP
        // mismatch the way a decimal fraction like 12.4 does (measured: it did, harmlessly, on the
        // first draft of this test — f32→f64 widening plus serde_json's shortest-round-trip printer
        // took two different paths to the value and landed one bit apart in the last decimal digit,
        // which is a float-formatting artifact, not a bug in the field this test is pinning).
        set_gs(&state, |gs| gs.player_hold = Some(ControllerHold {
            reason: ControllerHoldReason::EmbeddedNoRecovery,
            secs: 12.5,
        }));
        let v = debug_json(state).await;
        let h = &v["player"]["hold"];
        assert!(h.is_object(), "a hold in the GameState must reach the body, got {h}");
        assert_eq!(h["reason"], serde_json::json!("embedded_no_recovery"));
        assert_eq!(h["held_secs"], serde_json::json!(12.5_f32),
            "held_secs must round-trip the controller frame time the hold carries, got {}", h["held_secs"]);
        assert!(h["detail"].as_str().expect("detail is a string").to_ascii_lowercase().contains("embedded"),
            "the detail must name what is actually true about this reason, got {}", h["detail"]);

        // ── Present: UnderworldNoRecovery — the other reason, so the match arm above isn't a fluke.
        let state = empty_state();
        set_gs(&state, |gs| gs.player_hold = Some(ControllerHold {
            reason: ControllerHoldReason::UnderworldNoRecovery,
            secs: 3.0,
        }));
        let v = debug_json(state).await;
        let h = &v["player"]["hold"];
        assert_eq!(h["reason"], serde_json::json!("underworld_no_recovery"));
        assert!(h["detail"].as_str().expect("detail is a string").to_ascii_lowercase().contains("underworld"),
            "the detail must name what is actually true about this reason, got {}", h["detail"]);
    }

    /// #598 finding 1, at the API BOUNDARY — the honesty contract must hold in the SERIALIZED body,
    /// not just in the type. `player.levitating` is three-valued: `true` / `false` / `null`, where
    /// `null` = UNKNOWN. The assertions below check the exact wire form an agent parses:
    /// - Unknown serializes as an explicit `null` that IS present in the object — never `false`
    ///   (the pre-#598 confident-falsehood) and never an OMITTED key (which JSON `["levitating"]`
    ///   would also read as null — an absent key is itself a lie, so we assert `contains_key`).
    /// - Yes → `true`, No → `false`.
    ///
    /// MUTATION-CHECK: map `Levitating::Unknown => Some(false)` in `PlayerState::from_game_state`
    /// (the pre-#598 collapse) → the Unknown case here goes RED. Add `skip_serializing_if` to the
    /// field → the `contains_key` (present-not-omitted) assertion goes RED.
    #[tokio::test]
    async fn levitating_is_three_valued_in_the_debug_json_never_a_confident_false() {
        // UNKNOWN: an unresolvable buff (spell table missing/truncated), no positive evidence.
        let state = empty_state();
        set_gs(&state, |gs| gs.levitate.buff_slot_changed(4, None, false));
        let v = debug_json(state).await;
        let player = v["player"].as_object().expect("player object");
        assert!(player.contains_key("levitating"),
                "the levitating key must be PRESENT for Unknown — an omitted key is a lie too");
        assert!(player["levitating"].is_null(),
                "Unknown must serialize as JSON null, got {}", player["levitating"]);
        assert_ne!(player["levitating"], serde_json::json!(false),
                   "Unknown must NEVER be the confident-false the invariant forbids");

        // YES: a positive flymode reading.
        let state = empty_state();
        set_gs(&state, |gs| gs.levitate.set_flymode(true));
        let v = debug_json(state).await;
        assert_eq!(v["player"]["levitating"], serde_json::json!(true), "levitating flymode → true");

        // NO: complete information, no evidence, nothing unresolved → a trustworthy false.
        let v = debug_json(empty_state()).await;
        assert_eq!(v["player"]["levitating"], serde_json::json!(false),
                   "a fully-known non-levitating state → an honest false (not null)");
    }

    /// **#822 — position is served as ONE `pos` array, and there is no `pos_up` key to read.**
    ///
    /// Three tracked files told an API reader to read a key called `pos_up`: the `levitating` table
    /// in `docs/http-api.md`, the rustdoc on `PlayerState::levitating`, and the datum-discipline
    /// paragraph on `eqoxide_core::coord::WIRE_Z_OFFSET` (which also named a `/player` endpoint that
    /// does not exist). `pos_east`/`pos_north`/`pos_up` are `PlayerState` field names; `get_debug`
    /// hand-builds its `player` object and emits them as the single `"pos": [east, north, up]`.
    ///
    /// **What this test does and does not cover.** It pins the WIRE FACT the corrected sentences now
    /// assert — `pos` present as a 3-array, no `pos_*` key beside it — so a future change that adds
    /// such a key, or renames `pos`, goes red and the prose stops being silently true-by-luck. It
    /// does NOT check the prose: the wording edits in those three files have no test behind them,
    /// and reverting any of them leaves this test and the suite green. Said plainly rather than
    /// implied, because the acceptance bar on #822 asks for exactly that distinction.
    ///
    /// `up` is deliberately negative and distinct from `east`/`north`: a fixture that flattens the
    /// third component, or repeats one value, cannot detect a transposed or dropped axis (#800).
    ///
    /// MUTATION CHECK (run on the remote builder, `-p eqoxide-http --lib`, file restored from an
    /// `md5sum`-verified copy afterwards): rename the `"pos"` key in `get_debug` to `"pos_up"` →
    /// RED here. Result recorded in the PR body.
    #[tokio::test]
    async fn position_is_served_as_one_pos_array_with_no_pos_up_key_822() {
        let state = empty_state();
        set_gs(&state, |gs| {
            gs.player_x = 812.5;
            gs.player_y = 43.0;
            gs.player_z = -119.75;
        });
        let v = debug_json(state).await;
        let player = v["player"].as_object().expect("player object");

        assert_eq!(player.get("pos"), Some(&serde_json::json!([812.5, 43.0, -119.75])),
            "position must be served as the one array [east, north, up]. Keys served: {:?}",
            player.keys().collect::<Vec<_>>());

        for absent in ["pos_east", "pos_north", "pos_up"] {
            assert!(!player.contains_key(absent),
                "`{absent}` is now a served key. That is not automatically wrong, but the docs and \
                 rustdoc corrected by #822 currently tell agents this key does NOT exist and to read \
                 the `pos` array instead — update them in the same change, then update this test. \
                 Keys served: {:?}",
                player.keys().collect::<Vec<_>>());
        }
    }

    /// **#608, the no-second-derivation pin for the AGENT consumer.** `/nav_debug` is a structural
    /// serde projection of whatever nav PUBLISHED — verbatim. The fabricated snapshot below is
    /// deliberately inconsistent with any geometry (the state holds NO collision at all, and the
    /// "accepted" edge goes somewhere no floor exists): the endpoint must serve it AS PUBLISHED,
    /// because it has no way to re-derive or "correct" a verdict. If someone reintroduces a
    /// derivation in this layer (consulting `shared_collision` to fix up edges), the verbatim
    /// assertions here go RED.
    ///
    /// Also pins: nothing published yet → an EXPLICIT `available: false`, never an empty-but-
    /// plausible snapshot; and the unevaluated-semantics note is present.
    #[tokio::test]
    async fn nav_debug_serves_the_published_snapshot_verbatim_and_absence_is_explicit() {
        use eqoxide_nav::diagnostics::*;
        let state = empty_state();

        // 1. Nothing published: explicit unavailability.
        let v = nav_debug_json(state.clone()).await;
        assert_eq!(v["available"], false, "no snapshot yet must be an explicit 'not available'");
        assert!(v.get("committed_coarse").is_none(), "no fields may be invented for an absent snapshot");

        // 2. A fabricated snapshot, inconsistent with any real geometry, served verbatim.
        let mut trace = SearchTrace::with_budget(16);
        trace.begin_call(2.0, 8.0, true);
        trace.edge([0.0, 0.0, 0.0], [8.0, 0.0, 0.0], EdgeVerdict::Accepted { kind: EdgeKind::Walk });
        trace.edge([0.0, 0.0, 0.0], [0.0, 8.0, 4.0], EdgeVerdict::Rejected { reason: RejectReason::Grade });
        trace.outcome_calls = (0, 1);
        let snap = NavDebugSnapshot {
            seq: 7,
            zone_model_loaded: true,
            nav_state: "navigating".into(),
            nav_reason: None,
            goal_id: 3,
            player: Some([1.0, 2.0, 3.0]),
            published_at: std::time::Instant::now(),
            goal: Some([100.0, 0.0, 0.0]),
            committed_coarse: vec![[0.0, 0.0, 0.0], [8.0, 0.0, 0.0]],
            committed_fine: vec![[0.0, 0.0, 0.0]],
            plan: Some(std::sync::Arc::new(PlanDebug {
                gen: 3, goal_id: 3, start: [0.0; 3], goal: [100.0, 0.0, 0.0],
                outcome: "route".into(), reason: "route".into(), route_len: 2,
                plan_ms: 4, tight: false, goal_snapped: false, goal_offset: 0.0, trace,
            })),
            pads: vec![PadDebug { index: 9, knowledge: PadKnowledge::Unknown }],
            clearance: None,
            water: None,
        };
        *state.nav_debug_view.lock().unwrap() = Some(std::sync::Arc::new(snap));

        let assert_verbatim = |v: &serde_json::Value| {
            assert_eq!(v["available"], true);
            assert_eq!(v["seq"], 7);
            assert_eq!(v["nav_state"], "navigating");
            assert_eq!(v["player"], serde_json::json!([1.0, 2.0, 3.0]));
            assert_eq!(v["committed_coarse"], serde_json::json!([[0.0, 0.0, 0.0], [8.0, 0.0, 0.0]]),
                "the committed route must be served verbatim — it is the walker's actual path (#246)");
            let edges = &v["plan"]["trace"]["calls"][0]["edges"];
            assert_eq!(edges[0]["verdict"], "accepted");
            assert_eq!(edges[0]["kind"], "walk");
            assert_eq!(edges[1]["verdict"], "rejected");
            assert_eq!(edges[1]["reason"], "grade",
                "the published reject reason must be served verbatim — corrupting it in the publisher \
                 (the #608 mutation check) must surface HERE");
            assert_eq!(v["plan"]["trace"]["outcome_calls"], serde_json::json!([0, 1]));
            assert_eq!(v["pads"][0]["index"], 9);
            assert_eq!(v["pads"][0]["knowledge"], "unknown",
                "a pad's 'not yet observed' must reach the agent as exactly that");
            assert!(v["semantics"].as_str().unwrap().contains("UNEVALUATED"),
                "the absence-means-unevaluated contract must be stated on the wire");
            // Freshness is computed at read time and present (#615 review F1).
            assert!(v["published_age_ms"].is_u64(), "the snapshot's age must be reported");
            assert!(v["published_age_ms"].as_u64().unwrap() < 60_000, "…and computed from now");
            // The composed zone-assets load state rides along (same published #579 source as /debug).
            assert!(v["zone_assets"]["state"].is_string());
        };
        let v = nav_debug_json(state.clone()).await;
        assert_verbatim(&v);

        // #615 review F6: repeat with a COLLISION GRID PRESENT in the state. The renderer's
        // encoder cannot re-derive by signature; this handler could (HttpState carries
        // `shared_collision` for other endpoints), so the verbatim property must hold when the
        // grid is actually there — a re-derivation hidden behind `if let Some(col) = …` was a
        // silent no-op in the empty-state run above.
        let ready = eqoxide_nav::zone_assets::ZoneAssetState::test_ready();
        *state.shared_collision.write().unwrap() = ready.collision().cloned();
        let v = nav_debug_json(state).await;
        assert_verbatim(&v);
    }

    /// **#885 review round 1, B1 + B5 — the `semantics` string must not be a confident falsehood
    /// about its own payload.**
    ///
    /// Round 1 shipped two claims in this string that measurement refuted:
    ///
    /// * that `clearance.body` "is authoritative for whether it can move at all (anything but
    ///   `placeable` means it cannot)". The reviewer drove the real `CharacterController` for 3 s
    ///   on two `FootprintPierced` scenes: wet travelled 101.10 u, dry 101.01 u, both with
    ///   `hold() == None`. I re-measured a dry one myself: 131.98 u, `hold() == None`. The verdict
    ///   is the ENTRY CONDITION to the depenetration net, not a freeze. The sentence is deleted,
    ///   not qualified;
    /// * an instruction to "compare `anchor.z` against `anchor.reference_z`" — unperformable on a
    ///   `no_floor_in_band` anchor, which carries no `z` key at all.
    ///
    /// Both are pinned here on the REAL response body, with the second one's payload actually
    /// present so the instruction is checked against the JSON rather than against prose. The
    /// negative assertions are the same shape as
    /// `every_field_the_semantics_string_sends_a_caller_to_exists_in_the_body`'s "need not
    /// iterate" pin: a re-added overclaim goes RED here.
    #[tokio::test]
    async fn the_nav_semantics_string_does_not_overclaim_the_clearance_sample() {
        use eqoxide_nav::diagnostics::*;
        let state = empty_state();
        *state.nav_debug_view.lock().unwrap() = Some(std::sync::Arc::new(NavDebugSnapshot {
            seq: 1,
            zone_model_loaded: true,
            nav_state: "navigating".into(),
            nav_reason: None,
            goal_id: 1,
            player: Some([1.0, 2.0, 3.5]),
            published_at: std::time::Instant::now(),
            goal: None,
            committed_coarse: vec![],
            committed_fine: vec![],
            plan: None,
            pads: vec![],
            clearance: Some(ClearanceProbe {
                at: [1.0, 2.0],
                // The variant the round-1 instruction could not be performed on.
                anchor: ProbeAnchor::NoFloorInBand { reference_z: 3.5 },
                body: Placement::NoFloorBelow,
                wall_spokes: vec![SpokeReading::ClearToCap, SpokeReading::Hit { at: 2.5 }],
                cap: 4.0,
                footprint_ok: vec![true],
                footprint_radius: 1.0,
                footprint_ring_z: 6.5,
                field_wall: 3.0,
                field_ground: 2.0,
            }),
            water: None,
        }));
        let v = nav_debug_json(state).await;
        let semantics = v["semantics"].as_str().expect("semantics is served").to_string();

        // B1: the string must not tell an agent a non-`placeable` body means the character is stuck.
        assert!(!semantics.contains("can move at all"),
            "B1: `body` is the entry condition to the depenetration net, not a freeze — a dry \
             FootprintPierced body I drove for 3.0 s travelled 131.98 u with hold() == None. \
             Deleting this claim, not hedging it, is the fix: {semantics}");
        assert!(semantics.contains("player.hold"),
            "the string must send the caller to the field that DOES answer 'can it move': {semantics}");

        // B5: the string's instruction about `anchor.z` must be performable on the payload it is
        // describing. It is not an unconditional compare — `z` is a `floor`-only key.
        assert_eq!(v["clearance"]["anchor"]["kind"], "no_floor_in_band");
        assert!(v["clearance"]["anchor"].get("z").is_none(),
            "B5: this anchor carries no `z` key, so guidance to read one unconditionally is \
             unperformable: {}", v["clearance"]["anchor"]);
        assert_eq!(v["clearance"]["anchor"]["reference_z"], serde_json::json!(3.5));
        assert!(semantics.contains("anchor.z is present ONLY when anchor.kind == \"floor\""),
            "the guidance must state the condition, since the key is conditional: {semantics}");

        // The tagged spoke union survives the HTTP layer, not just nav's own serializer.
        assert_eq!(v["clearance"]["wall_spokes"][0], serde_json::json!("clear_to_cap"));
        assert_eq!(v["clearance"]["wall_spokes"][1], serde_json::json!({ "hit": { "at": 2.5 } }));
        assert_eq!(v["clearance"]["at"], serde_json::json!([1.0, 2.0]),
            "`at` is horizontal-only since #885 — two elements, not three");
    }

    /// Publish a nav snapshot whose `pads` are exactly `pads`, and set the walker's nav state.
    fn publish_pads(state: &HttpState, nav_state: &str, pads: Vec<eqoxide_nav::diagnostics::PadDebug>) {
        {
            let mut s = state.nav.nav_state.lock().unwrap();
            s.state = nav_state.into();
            s.reason = Some("search_closed".into());
        }
        *state.nav_debug_view.lock().unwrap() = Some(std::sync::Arc::new(
            eqoxide_nav::diagnostics::NavDebugSnapshot {
                seq: 1,
                zone_model_loaded: true,
                nav_state: nav_state.into(),
                nav_reason: Some("search_closed".into()),
                goal_id: 1,
                player: Some([-677.0, -187.0, -14.0]),
                published_at: std::time::Instant::now(),
                goal: Some([-74.0, 428.0, 0.0]),
                committed_coarse: vec![],
                committed_fine: vec![],
                plan: None,
                pads,
                clearance: None,
                water: None,
            }));
    }

    /// **#543 — the disclosure, on the REAL response body an agent polls.**
    ///
    /// Nav will not auto-route through an advertised same-zone teleport pad, because it cannot
    /// verify one (the server picks a crossing's destination from trigger data the wire never
    /// carries). That refusal is correct. But answering a bare `no_path` while a usable pad sits
    /// right there is the same agent-honesty failure in a new place — the client would be implying
    /// "there is nothing here" when what it knows is "there is something here I cannot vouch for".
    ///
    /// So `/v1/observe/debug` — the SAME response the driver already polls for `nav_state` — must
    /// carry the declined pads, and must frame them as ADVERTISED, never verified. This pins both
    /// the presence and the framing; getting the wording wrong recreates the original bug.
    ///
    /// The fixture moves BOTH ways: the identical pad list under a non-terminal nav state, and a
    /// terminal state with no declined pad, must both report `null` — so the assertion cannot be
    /// satisfied by a field that is simply always populated.
    #[tokio::test]
    async fn debug_discloses_the_pads_nav_declined_to_route_through_543() {
        use eqoxide_nav::diagnostics::{PadDebug, PadKnowledge};
        let state = empty_state();
        let declined = || vec![PadDebug {
            index: 2,
            knowledge: PadKnowledge::AdvertisedSameZoneDeclined {
                footprint: Some([-615.0, -83.0, -14.0]),
                footprint_count: 58,
                alternates: vec![[-606.0, -70.0, -14.0]],
                region_at: [-615.0, -83.0, -14.0],
                advertised_dest: Some([-153.0, -30.0, 9.0]),
                advertised_dest_floor: Some([-153.0, -30.0, 6.0]),
            },
        }];

        // ── OFF: nav is walking fine. There is no failure to disclose against. ──
        publish_pads(&state, "navigating", declined());
        assert!(debug_json(state.clone()).await["nav_declined_pads"].is_null(),
            "the disclosure belongs to a nav FAILURE — it must not ride along on a healthy route");

        // ── OFF: nav failed, but nothing was declined (the ordinary unreachable goal). ──
        publish_pads(&state, "no_path", vec![PadDebug { index: 9, knowledge: PadKnowledge::Unknown }]);
        let v = debug_json(state.clone()).await;
        assert!(v["nav_declined_pads"].is_null(),
            "a pad nav never declined must not be dressed up as an offer — got {v:#}");

        // ── ON: the OTHER terminal no-route state. `search_exhausted` ("I don't know") is just as
        //    much a failure to find a verifiable route as `no_path`, and the pad is just as relevant
        //    — arguably more so. This leg was UNPINNED in the first revision: dropping
        //    "search_exhausted" from the terminal predicate stayed green (#660 review NB1).
        publish_pads(&state, "search_exhausted", declined());
        let v = debug_json(state.clone()).await;
        assert!(!v["nav_declined_pads"].is_null(),
            "search_exhausted is also 'no verifiable route' — the pad must be offered there too");
        assert_eq!(v["nav_declined_pads"]["pads"][0]["index"], 2);

        // ── ON: nav failed AND declined a real pad. This is the #543 case. ──
        publish_pads(&state, "no_path", declined());
        let v = debug_json(state.clone()).await;
        let d = &v["nav_declined_pads"];
        assert!(!d.is_null(),
            "#543: a no_path with a declined pad right there must DISCLOSE it, not withhold it");
        assert_eq!(d["reason"], "advertised_same_zone_unverifiable");
        assert_eq!(v["player"]["nav_state"], "no_path",
            "the disclosure is ADDITIONAL — the honest no_path itself must be unchanged");

        let pad = &d["pads"][0];
        assert_eq!(pad["index"], 2);
        assert_eq!(pad["footprint"], serde_json::json!([-615.0, -83.0, -14.0]),
            "the agent needs the measured footprint to be able to walk onto the pad at all");
        assert_eq!(pad["footprint_count"], 58,
            "a real DRNTP index has many leaves — ONE offer, and say how many spots it has (#660 NB2)");
        assert_eq!(pad["alternates"], serde_json::json!([[-606.0, -70.0, -14.0]]),
            "…and hand over the OTHER spots to try: verified live that one leaf of a pad fires \
             nothing while another leaf of the same pad crosses, so a bare count is unactionable");
        assert_eq!(pad["advertised_dest"], serde_json::json!([-153.0, -30.0, 9.0]),
            "the ADVERTISED destination must be the VERBATIM wire value (z=9.0), not the client's \
             floor snap of it — a derivation presented as the server's claim is a second source");
        assert_eq!(pad["advertised_dest_floor"], serde_json::json!([-153.0, -30.0, 6.0]),
            "the client's own snap (z=6.0) is reported, and reported SEPARATELY (#660 review NB3)");

        // THE FRAMING. An agent must be able to tell "advertised same-zone" from "verified
        // same-zone" — and the latter does not exist. A field named `dest`, or a
        // `destination_verified: true`, would be the original lie wearing a new label.
        assert_eq!(pad["advertised_same_zone"], true);
        assert_eq!(pad["destination_verified"], false,
            "the client cannot verify where a pad lands and must say so in machine-readable form");
        assert!(pad.get("dest").is_none(),
            "no unqualified `dest`: every destination here is an ADVERTISEMENT, and the key must say so");
        let detail = d["detail"].as_str().unwrap();
        assert!(detail.contains("ADVERTISED") && detail.contains("cannot verify"),
            "the prose must state that the destination is advertised and unverifiable: {detail}");
        assert!(detail.contains("does not remember"),
            "the client keeps NO memory of where a pad landed (owner decision) — say so, so the \
             agent knows the remembering is its job: {detail}");
        assert!(detail.contains("try `alternates`"),
            "a spot that fires nothing must not read as 'this pad is inert' — point at the rest");
        assert!(detail.contains("YOUR decision"),
            "the pad is offered as an OPTION for the agent to weigh, not a route nav is taking");
        assert!(detail.contains("PROVISIONAL"),
            "#660 review B2: this disclosure tells the agent to verify with player.zone/player.pos, \
             and those two fields are briefly the client's optimistic guess right after a crossing. \
             Sending an agent to read them without saying so routes it straight into the lie: {detail}");

        // A pad with NO advertised arrival at all (keep-position sentinel) is still TAKEABLE, and is
        // still offered — with an explicit `null`, never omitted and never a fabricated destination.
        publish_pads(&state, "no_path", vec![PadDebug {
            index: 1,
            knowledge: PadKnowledge::AdvertisedSameZoneDeclined {
                footprint: None, footprint_count: 0, alternates: vec![],
                region_at: [-476.0, -161.0, 33.5],
                advertised_dest: None, advertised_dest_floor: None,
            },
        }]);
        let v = debug_json(state.clone()).await;
        let pad = &v["nav_declined_pads"]["pads"][0];
        assert!(pad["footprint"].is_null() && pad["footprint_count"] == 0,
            "no standable point was found — say so with an explicit null, never invent one");
        assert_eq!(pad["region_at"], serde_json::json!([-476.0, -161.0, 33.5]),
            "…but the pad is still LOCATED: a pad in the map is never reduced to 'somewhere here'");
        assert!(pad["advertised_dest"].is_null() && pad["advertised_dest_floor"].is_null(),
            "an unadvertised arrival is an explicit null — never omitted, never invented");

        // The full per-pad record is also on /nav_debug, verbatim — one published source, two views.
        publish_pads(&state, "no_path", declined());
        let n = nav_debug_json(state).await;
        assert_eq!(n["pads"][0]["knowledge"], "advertised_same_zone_declined");
        assert_eq!(n["pads"][0]["advertised_dest"], serde_json::json!([-153.0, -30.0, 9.0]));
    }

    /// **#660 review B2 — the caveat must live on the FIELD, not in prose.**
    ///
    /// Polling `/debug` through a crossing caught `zone: "qeynos"` beside a qeynos2 `pos`:
    /// well-formed, confident, mutually inconsistent, on the exact endpoint `nav_declined_pads`
    /// sends the agent to, with nothing marking it. The prose warning was not enough — it lives in
    /// the message-log ring and was observed being evicted by ambient chatter ~10s later, while the
    /// fields stayed wrong.
    ///
    /// So `/debug` carries `position_provisional` + `crossing_pending_ms`, and BOTH directions are
    /// pinned: a settled client must not cry provisional, or the marker becomes noise an agent
    /// learns to ignore.
    #[tokio::test]
    async fn debug_marks_the_position_provisional_through_a_crossing_543() {
        let state = empty_state();

        // Settled: no marker, and no age to misread.
        set_gs(&state, |gs| { gs.position_provisional_since = None; });
        let v = debug_json(state.clone()).await;
        assert_eq!(v["player"]["position_provisional"], false,
            "a settled position must NOT be flagged — a marker that is always on is not a marker");
        assert!(v["player"]["crossing_pending_ms"].is_null());

        // Mid-crossing: the client applied its own guess and the server has not placed us yet.
        set_gs(&state, |gs| {
            gs.position_provisional_since = Some(std::time::Instant::now());
            gs.world.zone_name = "qeynos".into();   // the echo already flipped the zone…
            gs.player_x = -142.4; gs.player_y = -25.8; gs.player_z = 5.1; // …but this is qeynos2
        });
        let v = debug_json(state.clone()).await;
        assert_eq!(v["player"]["position_provisional"], true,
            "#660 B2: `zone` and `pos` can disagree here — the response must SAY so, on the field");
        assert!(v["player"]["crossing_pending_ms"].as_u64().is_some_and(|ms| ms < 60_000),
            "…and how long it has been unsettled, measured at read time (#343), not cached");
        // The inconsistency itself is still served — marking it is the fix, hiding it is not.
        assert_eq!(v["player"]["zone"], "qeynos");
    }

    /// #343 regression — THE lie. The connection is dead: no packet has arrived for a minute, and
    /// consequently NOTHING has re-published anything (this is precisely the state the old code
    /// could not represent, because `connected` was computed inside `render_frame` and the render
    /// loop's wake signal is "a packet arrived"). `/debug` must still say `connected: false`, with a
    /// `last_packet_age_ms` that reflects real elapsed time — derived when the agent ASKS, so no
    /// publisher has to be alive for the answer to be honest.
    #[tokio::test]
    async fn debug_reports_disconnected_when_the_world_froze_and_nothing_republished() {
        let state = empty_state_wall_clock(); // #760: this test's subject IS the wall clock
        // The world as it was when the link was still up — a sitting character, full HP.
        set_gs(&state, |gs| {
            gs.player_name = "Gmkblr".into();
            gs.world.zone_name   = "qeynos".into();
            gs.hp_pct      = 100.0;
            gs.sitting     = true;
        });
        // ...and then silence: 60s with NO datagram at all (not even a session ACK — the link is
        // genuinely gone), and no publish of any kind.
        {
            let mut h = state.net_health.lock().unwrap();
            let c = h.clock;
            h.last_datagram = c.ago(60);
            h.last_packet   = c.ago(60);
            h.last_tick     = c.ago(60);
        }

        let v = debug_json(state).await;
        let p = &v["player"];
        assert_eq!(p["connected"], serde_json::json!(false),
            "a session with no server packet for 60s must NOT report connected:true (#343)");
        assert!(p["last_packet_age_ms"].as_u64().unwrap() >= 60_000,
            "last_packet_age_ms must track real elapsed time, got {}", p["last_packet_age_ms"]);
        assert!(p["snapshot_age_ms"].as_u64().unwrap() >= 60_000,
            "snapshot_age_ms must expose that our own publisher stopped, got {}", p["snapshot_age_ms"]);
        // The stale world is still served (last known good) — but it is now clearly LABELLED stale.
        assert_eq!(p["hp_pct"], serde_json::json!(100.0));
    }

    /// #642: a POSITIVELY-OBSERVED server-side session drop must flip `connected` to false and expose
    /// the cause IMMEDIATELY — even while the link clock is fresh (a datagram landed <1s ago), which
    /// is exactly the up-to-CONN_STALE_SECS window in which the old code reported a dropped session as
    /// still connected. This is the type-level guarantee: `session_drop.is_some()` forces
    /// `connected: false`. Mutation check: delete `&& h.session_drop.is_none()` from `health()` and
    /// this goes RED (connected stays true on a fresh link).
    #[tokio::test]
    async fn debug_reports_disconnected_immediately_on_an_observed_session_drop() {
        let state = empty_state();
        set_gs(&state, |gs| gs.player_name = "Gmkblr".into());
        {
            let mut h = state.net_health.lock().unwrap();
            // The link is demonstrably alive by the SILENCE metric — a datagram just landed...
            h.last_datagram = std::time::Instant::now();
            h.last_tick     = std::time::Instant::now();
            // ...but the server explicitly ended the session (OP_SessionDisconnect).
            h.session_drop  = Some(eqoxide_ipc::SessionDropCause::ServerDisconnect);
        }

        let v = debug_json(state).await;
        let p = &v["player"];
        assert_eq!(p["connected"], serde_json::json!(false),
            "an observed session drop must force connected:false even with a fresh link (#642)");
        assert_eq!(p["session_drop"], serde_json::json!("server_disconnect"),
            "the drop cause must be surfaced as a machine-readable string (#642)");
        // link_age is still small — proving the false verdict came from session_drop, not silence.
        assert!(p["link_age_ms"].as_u64().unwrap() < 1_000,
            "the link clock is fresh; connected:false here is driven by session_drop, not staleness");
    }

    /// The healthy case must not regress: `session_drop` is null on a live session and `connected`
    /// stays true. Guards the new field against a stuck-observable regression (a signal that never
    /// clears is worse than the prose it replaced).
    #[tokio::test]
    async fn debug_reports_null_session_drop_on_a_live_session() {
        let state = empty_state();
        set_gs(&state, |gs| gs.player_name = "Gmkblr".into());

        let v = debug_json(state).await;
        let p = &v["player"];
        assert_eq!(p["connected"], serde_json::json!(true));
        assert!(p["session_drop"].is_null(),
            "a live session must report session_drop:null, not a stale cause (#642)");
    }

    /// The healthy case must not regress: a packet just landed → connected, ages near zero.
    #[tokio::test]
    async fn debug_reports_connected_while_packets_are_flowing() {
        let state = empty_state();
        set_gs(&state, |gs| gs.player_name = "Gmkblr".into());

        let v = debug_json(state).await;
        let p = &v["player"];
        assert_eq!(p["connected"], serde_json::json!(true));
        assert!(p["last_packet_age_ms"].as_u64().unwrap() < 1_000);
        assert!(p["snapshot_age_ms"].as_u64().unwrap() < 1_000);
    }

    /// `last_packet_age_ms` must ADVANCE between two reads of an otherwise-idle client. This is the
    /// exact live symptom of #343: with the value computed at publish time it stayed frozen at the
    /// same number across consecutive polls whenever the render loop slept.
    #[tokio::test]
    async fn last_packet_age_advances_between_reads_with_no_publisher_running() {
        let state = empty_state_wall_clock(); // #760: this test's subject IS the wall clock
        { let mut h = state.net_health.lock().unwrap(); let c = h.clock; h.last_packet = c.ago(5); }
        let first = debug_json(state.clone()).await["player"]["last_packet_age_ms"].as_u64().unwrap();
        // Nothing renders, nothing publishes, no packet arrives — just time passing.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let second = debug_json(state).await["player"]["last_packet_age_ms"].as_u64().unwrap();
        assert!(second > first,
            "last_packet_age_ms froze at {first} across two reads — it is not being derived at read time (#343)");
    }

    /// The two clocks are independent signals and must be reported as such: a live client whose
    /// SERVER went quiet is not the same failure as a client whose own network thread wedged.
    #[tokio::test]
    async fn server_silence_and_publisher_stall_are_distinguishable() {
        let state = empty_state_wall_clock(); // #760: this test's subject IS the wall clock
        // The link is dead (no datagrams at all), but our network thread is fine and still ticking.
        {
            let mut h = state.net_health.lock().unwrap();
            let c = h.clock;
            h.last_datagram = c.ago(30);
            h.last_packet   = c.ago(30);
        }
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["connected"], serde_json::json!(false));
        assert!(p["last_packet_age_ms"].as_u64().unwrap() >= 30_000);
        assert!(p["snapshot_age_ms"].as_u64().unwrap() < 1_000,
            "our own publisher is fine — snapshot_age_ms must not blame it for the link's silence");
    }

    /// The OTHER half of honesty, found by live-testing #343: a character sitting alone in an empty
    /// zone receives NO application packet for 40+ seconds while the session layer keeps ACKing
    /// away. That is an IDLE session, not a dead one. Deriving `connected` from application traffic
    /// would report it as disconnected — swapping #343's false `true` for an equally damaging false
    /// `false`, and sending an agent into a pointless reconnect loop. `connected` therefore tracks
    /// the LINK clock, and `last_packet_age_ms` is left free to say "the world is quiet".
    #[tokio::test]
    async fn a_quiet_world_on_a_live_link_is_still_connected() {
        let state = empty_state_wall_clock(); // #760: this test's subject IS the wall clock
        {
            let mut h = state.net_health.lock().unwrap();
            let c = h.clock;
            h.last_packet   = c.ago(45);                  // the world has nothing to say...
            h.last_datagram = c.now();                    // ...but the link is demonstrably alive.
        }
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["connected"], serde_json::json!(true),
            "a quiet world on a live link must NOT be reported as disconnected (#343)");
        assert!(p["last_packet_age_ms"].as_u64().unwrap() >= 45_000,
            "...while still honestly reporting that no world update has arrived for 45s");
        assert!(p["link_age_ms"].as_u64().unwrap() < 1_000);
    }

    /// #371 — THE unresponsive-world lie, end to end through the real `health()` projection. The link
    /// is ACKing (a datagram just landed → `connected: true`), the world has been application-silent
    /// for 30s, and an active liveness probe sent 15s ago was never answered (bound is 10s).
    /// `connected` alone would tell the agent the world is fine; `world_responsive: false` is the
    /// honest signal that the zone is not servicing our packets (still-ticking-but-unresponsive; a
    /// total freeze would already be `connected: false`).
    #[tokio::test]
    async fn debug_reports_world_unresponsive_when_a_probe_goes_unanswered_while_the_link_acks() {
        let state = empty_state();
        {
            let mut h = state.net_health.lock().unwrap();
            // #760/B1: every stamp below is derived from the fixture's OWN health clock (`c`), which
            // is what `health()` will read them back against. Written as `ago(15)` — the wall clock —
            // this test's probe age became `15s − (time since empty_state())` and the assertion below
            // needed it to clear a 10s bound: a 5s margin that machine load ate. Measured, with a
            // 5.1s sleep injected after `empty_state()`: `ago(15)` → world_responsive `true`
            // (assertion FAILS), `c.ago(15)` → `false` (passes). Same sleep, opposite outcome.
            let c = h.clock;
            h.last_datagram = c.now();                    // link is demonstrably alive (ACKing)...
            h.last_packet   = c.ago(30);                  // ...but the world has produced nothing...
            h.last_probe_sent = Some(c.ago(15));          // ...and our probe (15s ago) went...
            h.last_probe_reply = None;                    // ...unanswered, past the 10s bound.
            // #371 wedge-flicker fix: `health()` reads the wedge-timeout clock off
            // `first_unanswered_probe_sent`, not `last_probe_sent` — this is the first (and, in this
            // scenario, only) unanswered send of the streak, so in production `record_probe_sent`
            // would have stamped both together. Mirror that here.
            h.first_unanswered_probe_sent = Some(c.ago(15));
        }
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["connected"], serde_json::json!(true),
            "the socket is still ACKing — connected must stay honest about the LINK");
        assert_eq!(p["world_responsive"], serde_json::json!(false),
            "an unanswered probe on a live link is a WEDGED world — the #371 signal must fire");
    }

    /// #760/B1 — the rule about past-dated net-health stamps, as a TEST rather than as a doc comment.
    ///
    /// A stamp written `ago(N)` comes from the wall clock; `health()` reads it back against
    /// `NetHealth::clock`. On a frozen fixture those are different clocks, so the age is
    /// `N − (time since the fixture was built)` — it SHRINKS with machine load, silently, and any
    /// assertion needing it to stay above a bound has a margin that load eats. That is #760's own
    /// failure mode one level down, and it is what review found in
    /// `debug_reports_world_unresponsive_when_a_probe_goes_unanswered_while_the_link_acks`.
    ///
    /// A prose rule did not prevent it (the rule named only "an age that must move", not "an age
    /// that must exceed a bound"), so this scans the source instead.
    ///
    /// # What this matches
    ///
    /// The unit is a **statement**, not a physical line: comments and literals are removed by a
    /// character scanner, and what is left is cut at `;`, `{` and `}`. A statement offends when it
    /// mentions one of the eight [`STAMP_FIELDS`] *and* past-dates through a receiver that is not
    /// the clock reading it back (see `receiver_is_the_reading_clock`). Flagged forms: `ago(`, and
    /// `now()` followed by `-` or `.checked_sub`.
    ///
    /// # Why the `now()` spelling is matched, stated honestly
    ///
    /// It was added after a sweep for the CONCEPT rather than the field name found past-dated
    /// `NetHealth` stamps a bare `ago(` grep does not see. That sweep is worth keeping as a
    /// methodological result — **grep the concept, not the spelling** — but its sites do NOT
    /// justify the widening on their own terms, and an earlier version of this comment implied they
    /// did. Two corrections:
    ///
    /// 1. There are **seven**, not the three previously published — re-enumerated by sweeping
    ///    `eqoxide-net` for all eight [`STAMP_FIELDS`] against all three past-dating spellings:
    ///    `crates/eqoxide-net/src/gameplay.rs:1569, 1831, 1835, 1839` and
    ///    `crates/eqoxide-net/src/transport.rs:1885, 1914, 2679`.
    /// 2. Not one of them is reachable by this test — they are all in `eqoxide-net`, which is not in
    ///    the scanned set below and cannot be (`include_str!` reaches only this crate's own files).
    ///    And if the scan were extended to them they would all be **false positives**: each site's
    ///    `NetHealth` is built by `Default` (checked at every one of the seven), so its stamps are
    ///    past-dated from the wall clock and read back by the wall clock — the same clock — which is
    ///    exactly the property this test exists to require.
    ///
    /// What the widening actually buys is confined to the four files below: the spelling is now
    /// caught *there* if it ever appears, where today it does not.
    ///
    /// # How to probe this guard, and why it is written down
    ///
    /// A source scanner has TWO failure modes, and only one of them is obvious. It can fail to
    /// recognise a bad *shape* — and it can silently fail to *arrive* at the code in the first
    /// place. Round 3 of #760 had a twelve-cell shape table that was entirely void, because every
    /// probe was placed inside this test's own body, which happened to be the only region of
    /// `observe.rs` the scanner still reached: a `/*` inside a route glob in a doc comment on line 1
    /// latched the block-comment state and blinded **87% of the corpus**, and the probes were sitting
    /// in the window that this test's own `/* */`-containing doc had re-opened. The instrument was
    /// measured only where it worked.
    ///
    /// So: **any probe of this guard must be placed at several depths in EVERY scanned file** — near
    /// the top, mid-file, and near the end — and must be reported with two controls:
    /// 1. a **positive control**, proving the injected shape is detectable at all, and
    /// 2. a **reach control**, proving the scanner actually arrives at that location.
    ///
    /// Both controls are now permanent rather than ad-hoc: `scan_ended_clean` fails loudly if a file
    /// ends mid-comment or mid-string (a real source file never does), and every scanned file ends
    /// with a `GUARD_REACH_SENTINEL_*` const that this test asserts it can see.
    ///
    /// # What it still cannot see
    ///
    /// **Aliasing through a local.** `let s = ago(15); h.last_probe_sent = Some(s);` is two
    /// statements, neither carrying both halves, so it lands unflagged. Closing that needs dataflow,
    /// not text, and no amount of scanner care will do it.
    ///
    /// **A value produced inside a nested block or a `match` arm.** The cut at `{` and `}` puts
    /// `h.last_tick =` and the `ago(30)` in `h.last_tick = { ago(30) };` into different statements,
    /// so neither carries both halves. Same for a `match` that yields the stamp. Measured, with the
    /// controls this doc demands: both spellings were injected at three depths in all four scanned
    /// files (twelve locations) alongside a plain `now() -` positive control in the same function.
    /// All twelve controls were flagged — so the scanner reached every location and the shape is
    /// detectable there — and not one of the twenty-four brace/`match` forms was.
    ///
    /// Two lesser gaps: files outside the four named below, and a ninth `Instant` field added to
    /// `NetHealth` and to `health()` but not to [`STAMP_FIELDS`] — that list is hand-maintained with
    /// no cross-check against the struct.
    ///
    /// The rule is a SPELLING rule and errs toward flagging: it recognises the receivers `c.` and
    /// `….clock.`, so a *correct* stamp taken from a binding named anything else is flagged too, and
    /// so is a correct `self.now().checked_sub(…)` — which is the body of `HealthClock::ago` itself.
    /// That method lives in `eqoxide-ipc` and is not scanned, but the point stands: a flagged line is
    /// not automatically a wrong line.
    #[test]
    fn no_past_dated_net_health_stamp_is_taken_from_a_clock_other_than_the_one_that_reads_it() {
        /// Exactly the `NetHealth` fields `HttpState::health()` turns into an age. Adding a stamp
        /// field to that projection without adding it here leaves the new field unguarded.
        const STAMP_FIELDS: [&str; 8] = [
            "last_datagram", "last_packet", "last_tick",
            "last_probe_sent", "last_probe_reply", "first_unanswered_probe_sent",
            "last_send_pressure_at", "last_send_error_at",
        ];

        /// Code text per source line, with comments and literal contents removed, plus whether the
        /// scan **ended clean** — i.e. not stranded inside a block comment or a string.
        ///
        /// The line-at-a-time version this replaces looked for `/*` before it truncated at `//`, so
        /// a `/*` occurring *inside* a line or doc comment latched the block state permanently. That
        /// is not a corner case: two of the four scanned files open with a `//!` doc line containing
        /// a route glob (`/v1/observe/*`), which blinded the scanner from line 1. Literals are
        /// skipped for the same family of reason — a `//` or `/*` inside a string is not a comment.
        fn strip_to_code(src: &str) -> (Vec<String>, bool) {
            #[derive(PartialEq, Clone, Copy)]
            enum S { Code, Line, Block(u32), Str, Raw(usize), Chr }
            let b: Vec<char> = src.chars().collect();
            let at = |k: usize| -> char { b.get(k).copied().unwrap_or('\0') };
            let ident = |ch: char| ch.is_alphanumeric() || ch == '_';
            let mut out = vec![String::new()];
            let (mut st, mut i) = (S::Code, 0usize);
            while i < b.len() {
                let c = b[i];
                if c == '\n' {
                    if st == S::Line { st = S::Code; }
                    out.push(String::new());
                    i += 1;
                    continue;
                }
                match st {
                    S::Line => i += 1,
                    S::Block(d) => {
                        if c == '*' && at(i + 1) == '/' {
                            st = if d == 1 { S::Code } else { S::Block(d - 1) };
                            i += 2;
                        } else if c == '/' && at(i + 1) == '*' {
                            st = S::Block(d + 1); // Rust block comments nest.
                            i += 2;
                        } else { i += 1; }
                    }
                    S::Str => {
                        if c == '\\' { i += 2; } else { if c == '"' { st = S::Code; } i += 1; }
                    }
                    S::Raw(h) => {
                        if c == '"' && (1..=h).all(|k| at(i + k) == '#') { st = S::Code; i += h + 1; }
                        else { i += 1; }
                    }
                    S::Chr => {
                        if c == '\\' { i += 2; } else { if c == '\'' { st = S::Code; } i += 1; }
                    }
                    S::Code => {
                        let prev_is_ident = i > 0 && ident(b[i - 1]);
                        // NB the branch ORDER here is not what fixes C1 — `//` and `/*` differ in
                        // their second character, so they can never both match. The fix is that
                        // `S::Line` skips to end-of-line, so a `/*` sitting inside a line or doc
                        // comment is never read as an opener at all. The version this replaced
                        // stripped block comments in a whole-line pass BEFORE truncating at `//`,
                        // which is exactly how a route glob in a `//!` line latched it forever.
                        if c == '/' && at(i + 1) == '/' { st = S::Line; i += 2; }
                        else if c == '/' && at(i + 1) == '*' { st = S::Block(1); i += 2; }
                        else if c == '"' { st = S::Str; i += 1; }
                        else if (c == 'r' || (c == 'b' && at(i + 1) == 'r')) && !prev_is_ident && {
                            let j = i + if c == 'b' { 2 } else { 1 };
                            let mut h = 0;
                            while at(j + h) == '#' { h += 1; }
                            at(j + h) == '"'
                        } {
                            let j = i + if c == 'b' { 2 } else { 1 };
                            let mut h = 0;
                            while at(j + h) == '#' { h += 1; }
                            st = S::Raw(h);
                            i = j + h + 1;
                        }
                        // `'` is a char literal only if it closes; otherwise it is a lifetime.
                        else if c == '\'' && (at(i + 1) == '\\' || at(i + 2) == '\'') { st = S::Chr; i += 1; }
                        else { out.last_mut().unwrap().push(c); i += 1; }
                    }
                }
            }
            (out, matches!(st, S::Code | S::Line))
        }

        /// Statements, each kept as its (1-based line, text) fragments so an offender is still
        /// reported at the exact line that names the field.
        fn statements(code: &[String]) -> Vec<Vec<(usize, String)>> {
            let mut out: Vec<Vec<(usize, String)>> = Vec::new();
            let mut cur: Vec<(usize, String)> = Vec::new();
            for (i, line) in code.iter().enumerate() {
                // Cut at statement AND block boundaries, so unrelated code never merges into one
                // "statement" and invents a co-occurrence that fires falsely.
                for (n, piece) in line.split([';', '{', '}']).enumerate() {
                    if n > 0 && !cur.is_empty() { out.push(std::mem::take(&mut cur)); }
                    if !piece.trim().is_empty() { cur.push((i + 1, piece.to_string())); }
                }
            }
            if !cur.is_empty() { out.push(cur); }
            out
        }

        /// Is this call's RECEIVER the health clock that will read the stamp back?
        ///
        /// Exempt spellings are `c.` (the convention every call site uses, `let c = h.clock;`) and
        /// `….clock.`. Everything else is flagged — a free `ago(`, and equally `HealthClock::WALL`
        /// or any other receiver. Exempting *any* `.ago(` regardless of receiver was review's
        /// sharpest miss: `wall.ago(30)` stamped into a pinned fixture is #760 exactly.
        fn receiver_is_the_reading_clock(prefix: &str) -> bool {
            if prefix.ends_with(".clock.") { return true; }
            let Some(rest) = prefix.strip_suffix("c.") else { return false };
            !rest.chars().next_back().is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
        }

        // Naming the sentinels as VALUES, not just as strings to grep for, is what makes deleting
        // one a compile error rather than a silently weakened reach control.
        let _sentinels_must_exist = (
            super::GUARD_REACH_SENTINEL_OBSERVE,
            crate::GUARD_REACH_SENTINEL_LIB,
            crate::combat::GUARD_REACH_SENTINEL_COMBAT,
            crate::testkit::GUARD_REACH_SENTINEL_TESTKIT,
        );
        let sources = [
            ("observe.rs",  include_str!("observe.rs"),  "GUARD_REACH_SENTINEL_OBSERVE"),
            ("lib.rs",      include_str!("lib.rs"),      "GUARD_REACH_SENTINEL_LIB"),
            ("combat.rs",   include_str!("combat.rs"),   "GUARD_REACH_SENTINEL_COMBAT"),
            ("testkit.rs",  include_str!("testkit.rs"),  "GUARD_REACH_SENTINEL_TESTKIT"),
        ];
        let mut offenders = Vec::new();
        for (file, src, sentinel) in sources {
            let (code, ended_clean) = strip_to_code(src);
            // REACH CONTROL 1 — loud failure instead of a silent early stop. A real source file
            // never ends inside a block comment or a string, so this cannot false-positive.
            assert!(ended_clean,
                "{file}: the scanner ended stranded inside a comment or string literal, so it stopped \
                 seeing code partway through and every 'clean' result below is worthless (#760/C1)");
            let stmts = statements(&code);
            // REACH CONTROL 2 — the sentinel is the LAST item in each scanned file, so seeing it
            // proves the scan arrived at the end rather than merely ending without complaint.
            assert!(stmts.iter().flatten().any(|(_, t)| t.contains(sentinel)),
                "{file}: the scanner never reached `{sentinel}` at the end of the file, so it covered \
                 only a prefix of it — a clean scan of a corpus that was never read (#760/C1)");
            for stmt in stmts {
                let joined: String = stmt.iter().map(|(_, t)| t.as_str()).collect();
                let Some((line, _)) = stmt.iter().find(|(_, t)| STAMP_FIELDS.iter().any(|f| t.contains(f)))
                else { continue };
                let bad_ago = joined.match_indices("ago(")
                    .any(|(k, _)| !receiver_is_the_reading_clock(&joined[..k]));
                // `now()` is fine on its own — it dates a stamp to the present, which saturates to
                // age 0 against either clock. It is past-dating, and so a #760 shape, only when it
                // is followed by a subtraction. The receiver test applies here too: `c.now() - d` is
                // the correct clock and must NOT be flagged (review finding C2).
                let bad_now = joined.match_indices("now()").any(|(k, _)| {
                    if receiver_is_the_reading_clock(&joined[..k]) { return false; }
                    let tail = joined[k + "now()".len()..].trim_start();
                    tail.starts_with('-') || tail.starts_with(".checked_sub")
                });
                if !(bad_ago || bad_now) { continue; }
                offenders.push(format!("{file}:{line}: {}", joined.trim()));
            }
        }
        assert!(offenders.is_empty(),
            "these past-date a net-health stamp through a receiver that is not the clock `health()` \
             reads it back against — use `let c = h.clock; … = c.ago(N)` (#760). A flagged line is \
             not automatically wrong: the receiver test is a spelling rule, so a correct stamp under \
             a binding named other than `c` lands here too.\n{}",
            offenders.join("\n"));
    }

    /// #371, the false-alarm we must NOT raise (the #343 trap in reverse): a legitimately idle world
    /// — 45s with no spontaneous packet — whose probe IS answered stays `world_responsive: true`,
    /// while `last_packet_age_ms` still honestly reports the 45s of app-silence (the probe reply does
    /// NOT reset it).
    #[tokio::test]
    async fn debug_reports_idle_but_answered_world_as_responsive() {
        let state = empty_state_wall_clock(); // #760: this test's subject IS the wall clock
        {
            let mut h = state.net_health.lock().unwrap();
            let c = h.clock;
            h.last_datagram    = c.now();
            h.last_packet      = c.ago(45);        // no spontaneous world output for 45s (normal idle)
            h.last_probe_sent  = Some(c.ago(20));
            h.last_probe_reply = Some(c.ago(2));   // ...but the probe was answered 2s ago → alive
            // `first_unanswered_probe_sent` deliberately left `None` (the `empty_state()` default): in
            // production `record_probe_reply` clears it the instant a genuine reply lands, so an
            // ANSWERED probe's real state has no outstanding streak at all — this is what makes
            // `world_responsive` read `true` here (the "no verdict yet" branch), not the reply-vs-send
            // comparison branch (see `wedge_timeline_tests` for why that branch is otherwise dead from
            // this call site).
        }
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["connected"], serde_json::json!(true));
        assert_eq!(p["world_responsive"], serde_json::json!(true),
            "an idle world that answers the probe is alive — must not false-alarm on app-silence");
        assert!(p["last_packet_age_ms"].as_u64().unwrap() >= 45_000,
            "the probe reply must NOT reset last_packet_age_ms — its 'world quiet' meaning is preserved");
        assert!(p["last_world_response_ms"].as_u64().unwrap() < 3_000,
            "proof-of-life is fresh (probe answered 2s ago), even though spontaneous traffic is 45s stale");
    }

    /// Before any probe has fired, `world_responsive` defers to the passive signals rather than
    /// asserting a liveness it never measured — it must default to true, not a phantom wedge.
    #[tokio::test]
    async fn debug_defaults_world_responsive_true_before_the_first_probe() {
        let state = empty_state();
        { let mut h = state.net_health.lock().unwrap(); let c = h.clock; h.last_packet = c.ago(20); }
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["world_responsive"], serde_json::json!(true),
            "no probe sent yet → no verdict → true (read connected/last_packet_age_ms instead)");
    }

    /// The player block is a projection of the NETWORK thread's GameState, with no render loop in
    /// the path: a state change published by the network thread is visible to the very next read
    /// even though no frame was ever drawn (#343).
    #[tokio::test]
    async fn player_view_tracks_the_network_snapshot_without_any_render() {
        let state = empty_state();
        set_gs(&state, |gs| { gs.hp_pct = 100.0; gs.target_id = Some(7); });
        let v = debug_json(state.clone()).await;
        assert_eq!(v["player"]["hp_pct"], serde_json::json!(100.0));
        assert_eq!(v["player"]["target_id"], serde_json::json!(7));

        set_gs(&state, |gs| { gs.hp_pct = 12.0; gs.target_id = None; });
        let v = debug_json(state).await;
        assert_eq!(v["player"]["hp_pct"], serde_json::json!(12.0));
        assert_eq!(v["player"]["target_id"], serde_json::json!(null));
        assert_eq!(v["player"]["target_name"], serde_json::json!(null));
    }

    /// #408: the target pointer must clear on a zone change. Target a spawn in kaladimb, then zone
    /// (death-respawn to qeynos → `begin_zone_in`). On main `begin_zone_in` purges the entity map but
    /// NOT the target, so `/observe/debug` reports the old zone's spawn (id 66, cached name, 100% HP)
    /// — a spawn that doesn't exist in the new zone. RED on main (target leaks), GREEN after.
    #[tokio::test]
    async fn debug_clears_target_on_zone_change_408() {
        let state = empty_state();
        set_gs(&state, |gs| {
            gs.world.zone_name = "kaladimb".into();
            gs.upsert_entity(eqoxide_core::game_state::make_entity(66, "Guard_Dalammer000", 0.0, 0.0, 0.0, true));
            gs.set_target(66);
        });
        assert_eq!(debug_json(state.clone()).await["player"]["target_id"], serde_json::json!(66),
            "precondition: the spawn is the target before zoning");

        set_gs(&state, |gs| { gs.begin_zone_in(); gs.world.zone_name = "qeynos".into(); });
        let p = debug_json(state).await["player"].clone();
        assert_eq!(p["zone"], serde_json::json!("qeynos"));
        assert_eq!(p["target_id"], serde_json::json!(null),
            "target must clear on zone change — an old-zone spawn is not a valid target (#408)");
        assert_eq!(p["target_name"], serde_json::json!(null),
            "stale target_name must not leak into the new zone (#408)");
        assert_eq!(p["target_hp_pct"], serde_json::json!(null),
            "stale target_hp_pct must not leak into the new zone (#408)");
    }

    /// **#732 (agent-honesty) — the OBSERVER half: a retired goal must not be published.**
    ///
    /// `nav_goal` is read straight off the shared `NavStatus` (`"nav_goal": nav.goal`, from a plain
    /// `nav_state.lock().clone()` at the top of this handler) with nothing between the walker's
    /// write and the JSON body — no filter, no gate on `nav_state`. So whatever the retirement path
    /// leaves in that field is what an agent reads.
    ///
    /// The first half of this test is the PREMISE, asserted rather than assumed (#759's lesson: a
    /// fixture whose planted value a downstream filter discards proves nothing). It pins that an
    /// un-retired `goal` really does reach the response body — if that ever stops being true, this
    /// fails here rather than passing vacuously below.
    ///
    /// Coordinates are the values measured live on #732, rounded to whole units so the `f32`→JSON
    /// widening is exact and the comparison is not a float-formatting test.
    ///
    /// Mutation check: delete `*goal = None;` from `NavStatus::retire_to_idle` → the `nav_goal`
    /// assertion below goes RED with the previous zone's coordinates in the diff.
    #[tokio::test]
    async fn debug_publishes_no_nav_goal_once_the_goal_is_retired_to_idle_732() {
        let state = empty_state();
        {
            let mut s = state.nav.nav_state.lock().unwrap();
            s.goal_id = 4;
            s.state   = "navigating".into();
            s.goal    = Some([2216.0, 579.0, -113.0]);
            s.tier    = Some("preferred");
        }
        let v = debug_json(state.clone()).await;
        assert_eq!(v["nav_goal"], serde_json::json!([2216.0, 579.0, -113.0]),
            "PREMISE: an un-retired goal is published verbatim — the harm this test is about is \
             reachable by a reader of GET /v1/observe/debug, not merely resident in memory");
        assert_eq!(v["nav_goal_id"], serde_json::json!(4), "PREMISE: the identity stamp is published too");

        // What every route to `idle` now calls — a zone change (`zoned`) among them.
        state.nav.nav_state.lock().unwrap().retire_to_idle(Some("zoned"));

        let v = debug_json(state).await;
        assert_eq!(v["player"]["nav_state"], serde_json::json!("idle"));
        assert_eq!(v["player"]["nav_reason"], serde_json::json!("zoned"));
        assert_eq!(v["nav_goal"], serde_json::json!(null),
            "#732: `idle` owns no goal. Coordinates are a per-zone namespace and carry no zone tag, \
             so a goal left standing beside `idle` after a zone change is a well-formed answer about \
             a world the reader is no longer in (docs/http-api.md: nav_goal is null for idle/stop)");
        assert_eq!(v["nav_tier"], serde_json::json!(null),
            "the per-route clearance tier goes with the route it described");
        assert_eq!(v["nav_goal_id"], serde_json::json!(4),
            "the IDENTITY stamp survives on purpose (#349) — it is what lets a caller match this \
             `idle` to the goal it asked for; only the per-goal FACTS are retired");
    }

    /// **#766 (agent-honesty) — the OBSERVER half of the fine tier's retirement.**
    ///
    /// The sibling of the test above, for the field #732 left standing. `nav_local` is read off the
    /// same cloned `NavStatus` and passed through exactly one filter —
    /// `.filter(|l| l.state != "threaded")` — so an UNHEALTHY verdict reaches the response body
    /// verbatim, and an unhealthy verdict is the only kind an agent can ever see. (Kept whole on one
    /// line: a code span broken across a `///` wrap renders the break inside the span and is
    /// un-greppable — #773, round-6 review B15.)
    /// That makes `no_way_through` the right fixture
    /// and makes the first half of this test a real premise: it asserts that the planted verdict
    /// genuinely publishes, so the `null` below is the retirement's doing and not the filter's.
    ///
    /// Mutation check: delete `*local = None;` from `NavStatus::retire_to_idle`
    /// (`crates/eqoxide-ipc/src/lib.rs`) → the `nav_local` assertion goes RED with the previous
    /// zone's `no_way_through` object in the diff.
    #[tokio::test]
    async fn debug_publishes_no_nav_local_once_the_goal_is_retired_to_idle_766() {
        let state = empty_state();
        {
            let mut s = state.nav.nav_state.lock().unwrap();
            s.goal_id = 4;
            s.state   = "navigating".into();
            s.local   = Some(eqoxide_ipc::NavLocal {
                state: "no_way_through".into(), reason: "search_closed".into(),
                stuck_ticks: 7, plan_us: 1234,
            });
        }
        let v = debug_json(state.clone()).await;
        assert_eq!(v["nav_local"]["state"], serde_json::json!("no_way_through"),
            "PREMISE: an un-retired UNHEALTHY fine verdict is published — the harm is reachable by a \
             reader of GET /v1/observe/debug, not merely resident in memory");
        assert_eq!(v["nav_local"]["stuck_ticks"], serde_json::json!(7),
            "PREMISE: and the whole object comes through, not just a state string");

        // The zone-change retirement, which is where #766 was reported.
        state.nav.nav_state.lock().unwrap().retire_to_idle(Some("zoned"));

        let v = debug_json(state).await;
        assert_eq!(v["player"]["nav_state"], serde_json::json!("idle"));
        assert_eq!(v["player"]["nav_reason"], serde_json::json!("zoned"));
        assert_eq!(v["nav_local"], serde_json::json!(null),
            "#766: `no_way_through` beside `idle`/`zoned` describes a corridor in the zone the \
             reader has LEFT, computed against a collision grid that no longer exists — the fine \
             tier's verdict is about threading toward a goal, so it retires with the goal");
    }

    /// **#851 (agent-honesty) — the OBSERVER half of the stall publication.**
    ///
    /// The walker-side tests in `eqoxide-nav` prove that a stalled walker stops publishing
    /// `navigating` into `NavStatus`. They cannot prove an agent can READ it: this crate is where
    /// `NavStatus` becomes JSON, and a field that is computed but never serialized is exactly the
    /// written-but-not-reached shape #799 tracks. So this drives the real `/debug` handler.
    ///
    /// Both directions, because either one alone is satisfiable by a constant: `nav_stall` is `null`
    /// on an ordinary `navigating`, and carries the whole object on `navigating_stalled`.
    ///
    /// Mutation check: delete `"nav_stall": nav_stall,` from the `/debug` body → the second half
    /// goes RED; hard-code `let nav_stall = None;` → also RED, and the `null` half stays green,
    /// which is why the `null` half is not the test.
    #[tokio::test]
    async fn debug_publishes_the_nav_stall_calibration_only_while_stalled_851() {
        let state = empty_state();
        {
            let mut s = state.nav.nav_state.lock().unwrap();
            s.goal_id = 4;
            s.state   = "navigating".into();
            s.goal    = Some([100.0, 200.0, 0.0]);
        }
        let v = debug_json(state.clone()).await;
        assert_eq!(v["player"]["nav_state"], serde_json::json!("navigating"),
            "PREMISE: an ordinary walk is being published at all");
        assert_eq!(v["nav_stall"], serde_json::json!(null),
            "#851: a walker that is executing its route has no stall to disclose — a non-null here \
             would make the field noise an agent learns to ignore");

        // What `Walker::publish_drive_state` writes once the verdict has flipped.
        //
        // The pair is REACHABLE, and that is load bearing (#851 review round 1, B2a). The earlier
        // fixture said `quiet_ticks: 34, quiet_ms: 5100` — exactly `34 × 150`, the one arithmetic
        // `docs/http-api.md` says never to do — and the implementation cannot produce it, because
        // `quiet_ms` is a measured wall clock over the window `quiet_ticks` counts and the 150 ms
        // nav tick is a floor. So the "pin" pinned a row that could not occur. 34 ticks in 5310 ms
        // is a tick running ~6% slow, which is an ordinary loaded frame.
        {
            let mut s = state.nav.nav_state.lock().unwrap();
            s.state = "navigating_stalled".into();
            s.stall = Some(eqoxide_ipc::NavStall {
                quiet_ticks: 34, quiet_ms: 5310, repaths: 2, route: "complete",
            });
        }
        let v = debug_json(state).await;
        assert_eq!(v["player"]["nav_state"], serde_json::json!("navigating_stalled"));
        assert_eq!(v["nav_stall"]["quiet_ticks"], serde_json::json!(34),
            "#851: the evidence count must reach the reader — `navigating_stalled` on its own says \
             THAT the walker is stuck, not for how long, and 3 s of stall reads very differently \
             from 30 s");
        assert_eq!(v["nav_stall"]["quiet_ms"], serde_json::json!(5310));
        assert_eq!(v["nav_stall"]["repaths"], serde_json::json!(2),
            "the re-path count is how an agent tells a stall that is about to recover from one \
             approaching the 8-attempt give-up");
        assert_eq!(v["nav_stall"]["route"], serde_json::json!("complete"),
            "and whether the committed route even ends at the goal");
        assert!(v["nav_stall"]["detail"].as_str().is_some_and(|d| !d.is_empty()),
            "the prose half of the disclosure must be present too");
    }

    /// **#766 review B3 — the worker-scoped fault must NOT retire with the goal.**
    ///
    /// The counterweight to the test above, and the reason this PR did not simply narrow a sentence.
    /// `planner_dead` is the third publishable `nav_local.state`, and it is not a verdict about a
    /// goal: it is a latched client fault meaning steering has degraded to the coarse 8 u route
    /// with nothing on any nav route to recover it. Retiring `nav_local` on every `idle` — which is #766's whole point — therefore hid
    /// it from an agent BETWEEN goals, which is precisely when an agent polls this endpoint. So the
    /// fault moved to its own field and this measures the split at the JSON surface, where an agent
    /// actually reads it: after the same retirement, the per-goal verdict is `null` and the
    /// worker-scoped fault is still `true`. (The field's lifetime is the fine WORKER's, not the
    /// session's — round-6 review B12. Nothing in this test turns on the difference; it retires a
    /// goal, and retiring a goal does not replace a worker. What it would catch is a clear placed on
    /// a retirement route, which is the defect it was written for.)
    ///
    /// It also pins the always-present shape. A `null`-when-healthy field would make "alive"
    /// indistinguishable from "this client is too old to have the field", and a health check you
    /// cannot distinguish from a missing feature is not a health check.
    ///
    /// Mutation check, RUN: clear `local_planner_dead` in `NavStatus::retire_to_idle` instead of
    /// keeping it → the final assertion here goes RED (`246 passed; 1 failed` in this crate), and
    /// `eqoxide-nav` goes red separately on its own row-level assertion. Named by assertion, not by
    /// line number, deliberately (review B8): a line locator drifts on the next edit above it. The
    /// always-present shape is pinned by the first assertion but NOT mutation-checked — omitting a
    /// key from a `json!` literal is a shape change, not a behaviour one, and I did not run it.
    #[tokio::test]
    async fn debug_keeps_publishing_a_dead_fine_planner_after_the_goal_is_retired_766() {
        let state = empty_state();
        let v = debug_json(state.clone()).await;
        assert_eq!(v["nav_local_planner_dead"], serde_json::json!(false),
            "PREMISE: liveness is published in BOTH states — a healthy client says so out loud, so \
             the `true` below is a change this test caused and not a key that only ever appears \
             when something is wrong");

        {
            let mut s = state.nav.nav_state.lock().unwrap();
            s.goal_id = 4;
            s.state   = "navigating".into();
            s.local   = Some(eqoxide_ipc::NavLocal {
                state: "planner_dead".into(), reason: "local_planner_dead".into(),
                stuck_ticks: 0, plan_us: 0,
            });
            s.local_planner_dead = true;   // what `Walker::latch_local_planner_liveness` writes
        }
        let v = debug_json(state.clone()).await;
        assert_eq!(v["nav_local"]["state"], serde_json::json!("planner_dead"),
            "PREMISE: while a route is committed the fault is visible in BOTH channels, so the \
             assertions below measure which one survives rather than which one exists");
        assert_eq!(v["nav_local_planner_dead"], serde_json::json!(true));

        state.nav.nav_state.lock().unwrap().retire_to_idle(Some("zoned"));

        let v = debug_json(state).await;
        assert_eq!(v["nav_local"], serde_json::json!(null),
            "#766 is unchanged by B3: the per-goal channel still retires with the goal. The \
             liveness field is an addition, not a hole in that guarantee");
        assert_eq!(v["nav_local_planner_dead"], serde_json::json!(true),
            "#766 B3: the worker thread does not come back — recovering it needs a client restart — \
             so an agent between goals must still be able to read that its steering has degraded. \
             Clearing this on retirement would report a recovery that never happened, which is the \
             agent-honesty defect class #766 exists to close, not one it may create");
    }

    /// #471 (agent-honesty): the server placed two Mobs (consecutive spawn_ids, e.g. 526/527) at a
    /// byte-identical position; the wire disambiguates their names with a numeric suffix
    /// ("Geeda"/"Geeda00"), so in the name-keyed roster they survive as TWO entries. The observe
    /// boundary must collapse them to one AND say it did — never silently drop (the honesty
    /// invariant). A no-op dedup leaves two entries with deduped==0, so this pins the collapse.
    #[test]
    fn dedup_collapses_consecutive_id_name_position_pair_471() {
        let mut m = HashMap::new();
        m.insert("Geeda".to_string(),        (100.0f32, 200.0, 5.0)); // spawn_id 526
        m.insert("Geeda00".to_string(),      (100.0f32, 200.0, 5.0)); // spawn_id 527, identical pos
        m.insert("Bidl_Frugrin".to_string(), (10.0f32,  20.0,  3.0)); // a genuine singleton
        let (out, deduped, groups) = dedup_entities(&m);
        assert_eq!(deduped, 1, "the duplicate pair must collapse to exactly one removed entry");
        assert_eq!(out.len(), 2, "Geeda (one of two) + Bidl_Frugrin");
        assert!(out.contains_key("Geeda"), "the un-suffixed spelling is kept as the representative");
        assert!(!out.contains_key("Geeda00"), "the suffixed duplicate is collapsed out of the view");
        assert!(out.contains_key("Bidl_Frugrin"), "the singleton is untouched");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].names, vec!["Geeda".to_string(), "Geeda00".to_string()],
            "the collapsed cluster surfaces BOTH names — nothing is hidden");
        assert_eq!(groups[0].kept, "Geeda");
        assert_eq!(groups[0].position, [100.0, 200.0, 5.0]);
    }

    /// Same base name at DIFFERENT positions = two real mobs; never collapse them.
    #[test]
    fn dedup_keeps_same_name_at_distinct_positions_471() {
        let mut m = HashMap::new();
        m.insert("a_bat00".to_string(), (1.0f32, 2.0, 3.0));
        m.insert("a_bat01".to_string(), (9.0f32, 8.0, 7.0));
        let (out, deduped, groups) = dedup_entities(&m);
        assert_eq!(deduped, 0);
        assert_eq!(out.len(), 2);
        assert!(groups.is_empty());
    }

    /// Two genuinely-different mobs sharing an exact position (astronomically rare) must NOT merge —
    /// different base names, so collapsing them would hide a real entity.
    #[test]
    fn dedup_keeps_different_names_sharing_a_position_471() {
        let mut m = HashMap::new();
        m.insert("a_rat00".to_string(),   (5.0f32, 5.0, 5.0));
        m.insert("a_snake00".to_string(), (5.0f32, 5.0, 5.0));
        let (out, deduped, _groups) = dedup_entities(&m);
        assert_eq!(deduped, 0);
        assert_eq!(out.len(), 2);
    }

    /// Default `/observe/entities` (no query) must stay the BARE `{name:[x,y,z]}` map so existing
    /// consumers (e.g. group_driver.py's `ents.get(name)` / `ents.items()`) keep working — but the
    /// positional duplicate is collapsed, so their world model is corrected with zero shape change.
    #[tokio::test]
    async fn entities_default_returns_bare_deduped_map_471() {
        let state = empty_state();
        {
            let mut pos = state.world.entity_positions_mut();
            pos.insert_for_test("Geeda".to_string(),        (100.0, 200.0, 5.0));
            pos.insert_for_test("Geeda00".to_string(),      (100.0, 200.0, 5.0)); // the duplicate
            pos.insert_for_test("Bidl_Frugrin".to_string(), (10.0,  20.0,  3.0));
        }
        let resp = get(state, "/entities").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Bare map: top-level keys are names whose values are [x,y,z] arrays — group_driver's contract.
        assert!(v.is_object() && v.get("entities").is_none() && v.get("deduped").is_none(),
            "default must be the historical bare map, not the labeled wrapper, got: {v}");
        assert!(v["Geeda"].is_array(), "ents.get('Geeda') must still return an [x,y,z] list");
        assert_eq!(v["Geeda"], serde_json::json!([100.0, 200.0, 5.0]));
        assert!(v.get("Geeda00").is_none(), "the positional duplicate is collapsed out of the map");
        assert!(v["Bidl_Frugrin"].is_array());
        assert_eq!(v.as_object().unwrap().len(), 2, "Geeda + Bidl_Frugrin (duplicate collapsed)");
    }

    /// `?labeled=1` opts into the rich shape with a non-zero `deduped` and an explanatory `note`.
    #[tokio::test]
    async fn entities_labeled_param_returns_rich_shape_471() {
        let state = empty_state();
        {
            let mut pos = state.world.entity_positions_mut();
            pos.insert_for_test("Geeda".to_string(),   (100.0, 200.0, 5.0));
            pos.insert_for_test("Geeda00".to_string(), (100.0, 200.0, 5.0));
        }
        let resp = get(state, "/entities?labeled=1").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["deduped"], 1, "the duplicate must be surfaced as a count, not silently dropped");
        assert!(v["note"].as_str().unwrap().contains("#471"),
            "the collapse must be labeled with an explanation, got: {}", v["note"]);
        assert_eq!(v["duplicate_groups"][0]["kept"], "Geeda");
        assert!(v["entities"]["Geeda"].is_array());
    }

    /// A typo'd query param must fail loudly (#363 honesty), not silently fall back to the default.
    #[tokio::test]
    async fn entities_typoed_param_is_rejected_471() {
        let state = empty_state();
        let resp = get(state, "/entities?labled=1").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST,
            "an unknown query param must be an explicit 400, not a silent default");
    }

    fn push_message(state: &HttpState, kind: &str, text: &str) {
        state.chat.messages.lock().unwrap().push(MessageEntry {
            kind: kind.to_string(), text: text.to_string(), keywords: vec![], item_links: vec![],
        });
    }

    async fn get(state: HttpState, uri: &str) -> axum::response::Response {
        let app = router().with_state(state);
        app.oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap()
    }

    /// eqoxide#363: a typo'd query param (`?kidn=npc` instead of `?kind=npc`) must be rejected with
    /// an explicit 400 naming the bad field, NOT silently ignored so `kind` falls back to `None`
    /// (no filter) and the caller gets the whole message log back looking like a normal 200.
    #[tokio::test]
    async fn typoed_query_param_is_rejected_not_silently_dropped() {
        let state = empty_state();
        push_message(&state, "npc", "Well met, traveler.");
        push_message(&state, "chat", "someone: hi");
        let resp = get(state, "/messages?kidn=npc").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST,
            "a typo'd/unknown query param must be an explicit failure, not a silent 200 over the whole log");
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let msg = String::from_utf8_lossy(&bytes);
        assert!(msg.contains("kidn"), "the 400 body should name the offending field, got: {msg}");
    }

    /// The happy path must not regress: a correctly-spelled `kind` still filters normally.
    #[tokio::test]
    async fn valid_kind_param_still_works() {
        let state = empty_state();
        push_message(&state, "npc", "Well met, traveler.");
        push_message(&state, "chat", "someone: hi");
        let resp = get(state, "/messages?kind=npc").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["messages"][0]["kind"], "npc");
    }

    /// #532 review (agent-honesty, BLOCKING): `GET /v1/observe/packets?summary=1&op=` must NOT
    /// fabricate reliable-seq gaps. `rel_seq` is a single per-direction counter shared across ALL
    /// opcodes, so the gap detector must see the dir-filtered but NOT op-filtered stream — otherwise
    /// the intervening reliable packets of other opcodes (which legitimately consumed sequence
    /// numbers) go missing and it reports phantom "lost packets". This is the exact
    /// `scripts/packet-analysis.py --dir in --op 0x5089` (#463) workflow, which defaults to summary=1.
    #[tokio::test]
    async fn packets_summary_with_op_filter_does_not_fabricate_seq_gaps() {
        use eqoxide_telemetry as pkt;
        let _guard = pkt::test_capture_lock();
        pkt::set_enabled(true);
        pkt::clear();
        // A CONTIGUOUS inbound reliable stream mixing two opcodes: 0x5089 @seq0, 0x6097 @seq1,
        // 0x5089 @seq2. Nothing is lost. Filtering to op 0x5089 alone leaves seqs {0, 2}.
        pkt::capture(pkt::Dir::In, 0x5089, &[], true, Some(0));
        pkt::capture(pkt::Dir::In, 0x6097, &[], true, Some(1));
        pkt::capture(pkt::Dir::In, 0x5089, &[], true, Some(2));
        pkt::set_enabled(false);

        let resp = get(empty_state(), "/packets?summary=1&dir=in&op=0x5089").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        let gaps = v["summary"]["seq_gaps"].as_array().unwrap();
        assert!(gaps.is_empty(),
            "op-filtered summary must NOT fabricate a gap over a contiguous stream, got: {gaps:?}");
        // The histogram still honors the op filter (only 0x5089 shown, count 2).
        let hist = v["summary"]["histogram"].as_array().unwrap();
        assert_eq!(hist.len(), 1, "histogram is op-filtered");
        assert_eq!(hist[0]["opcode"], 0x5089);
        assert_eq!(hist[0]["count"], 2);
        assert_eq!(v["summary"]["total"], 2, "totals describe the op-filtered view");

        // Control: a REAL gap in the underlying stream is still reported through the same endpoint.
        pkt::set_enabled(true);
        pkt::clear();
        pkt::capture(pkt::Dir::In, 0x5089, &[], true, Some(0));
        pkt::capture(pkt::Dir::In, 0x5089, &[], true, Some(2)); // seq 1 genuinely missing
        pkt::set_enabled(false);
        let resp = get(empty_state(), "/packets?summary=1&dir=in&op=0x5089").await;
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["summary"]["seq_gaps"].as_array().unwrap().len(), 1,
            "a real gap in the underlying stream must still be reported");
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn header_age_ms(resp: &axum::response::Response) -> u64 {
        resp.headers().get(SNAPSHOT_AGE_HEADER)
            .unwrap_or_else(|| panic!("{SNAPSHOT_AGE_HEADER} header missing"))
            .to_str().unwrap().parse().unwrap()
    }

    /// #646 — the no-freshness-marker bug this issue fixes, pinned against the REAL router body.
    /// Before this change only `/debug` and `/nav_debug` carried any freshness field at all; every
    /// route enumerated here served last-known state with no way for a driving agent to tell it was
    /// frozen (the motivating case: with the net thread dead, `/entities` kept returning `200` with a
    /// frozen map and no marker of any kind — #634/#647). This test fails on unmodified `origin/main`
    /// because none of these keys/headers exist there.
    ///
    /// JSON-field endpoints get `"snapshot_age_ms"` as a literal top-level key — asserted PRESENT
    /// (not merely non-panicking: `serde_json` renders a missing key and an explicit `null` the
    /// same way under naive indexing, so presence is checked explicitly).
    #[tokio::test]
    async fn every_previously_age_less_json_endpoint_now_carries_snapshot_age_ms() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();

        for uri in [
            "/item_text", "/packets", "/inventory", "/messages", "/dialogue", "/spells",
            "/skills", "/entities?labeled=1",
        ] {
            let resp = get(state.clone(), uri).await;
            assert_eq!(resp.status(), StatusCode::OK, "GET {uri} must succeed against a healthy state");
            let v = body_json(resp).await;
            assert!(v.get("snapshot_age_ms").is_some_and(|x| !x.is_null()),
                "GET {uri} must carry a PRESENT, non-null snapshot_age_ms key, got body: {v}");
            assert!(v["snapshot_age_ms"].as_u64().unwrap() < 1_000,
                "GET {uri} snapshot_age_ms should read near-zero on a freshly-ticked state, got {}",
                v["snapshot_age_ms"]);
        }
    }

    /// #646 — the header-carried half: endpoints whose body is a bare array/map that must keep its
    /// exact historical shape (no room for a new JSON key without breaking an existing consumer, e.g.
    /// `/entities`' default map for `group_driver.py`'s `ents.items()`) carry the identical value in
    /// `X-Snapshot-Age-Ms` instead. Also fails on unmodified `origin/main` — the header did not exist.
    #[tokio::test]
    async fn every_previously_age_less_bare_body_endpoint_carries_the_header() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();
        // `/zone_exits` is gated by the #579 zone-assets check (`zone_assets_not_ready`), which
        // refuses with 503 whenever the player's zone doesn't match the loaded assets' zone —
        // `empty_state()`'s player zone defaults to "" (unknown), which the gate always refuses
        // (`NotUsable::PlayerZoneUnknown`), not just a same-vs-different-zone mismatch. Match it to
        // `test_ready()`'s `"testfixture"` so this loop exercises the header on a genuinely-served
        // 200, the same way `zone_asset_gate_tests` does.
        set_gs(&state, |gs| gs.world.zone_name = "testfixture".to_string());
        // …and give that grid a region map that LOADED and simply has no zone-line regions, so
        // `/zone_exits` serves an honest `[]`/200 (#821 review round 2, B4). `empty_state()`'s
        // `test_ready()` grid has no region data attached at all, which since B4 is a
        // `503 zone_region_data_unavailable` — correctly, because the endpoint now reads the grid
        // the `Ready` state OWNS instead of falling through an unset `shared_collision` slot and
        // answering `[]` off nothing. This test is about the freshness header on a 200, so it needs
        // a state that genuinely earns one.
        *eqoxide_nav::zone_assets::lock_state(&state.zone_assets) =
            eqoxide_nav::zone_assets::ZoneAssetState::test_ready_with_water(Some(
                std::sync::Arc::new(eqoxide_core::region_map::RegionMap::flat_below(-10.0))));

        for uri in ["/entities", "/doors", "/zone_entrances", "/zone_points", "/zone_exits"] {
            let resp = get(state.clone(), uri).await;
            assert_eq!(resp.status(), StatusCode::OK, "GET {uri} must succeed against a healthy state");
            assert!(header_age_ms(&resp) < 1_000,
                "GET {uri} X-Snapshot-Age-Ms should read near-zero on a freshly-ticked state");
        }
    }

    /// `/who` and `/frame` need a simulated reply from the model layer (the who-roster oneshot / the
    /// camera frame channel) before they resolve, so they are pinned separately rather than folded
    /// into the loop above.
    #[tokio::test]
    async fn who_carries_snapshot_age_ms() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();
        let command = state.command.clone();
        let mut handle = tokio::spawn({
            let state = state.clone();
            async move { get(state, "/who").await }
        });
        // Play the model layer's part: drain the request and reply, exactly like
        // `ActionLoop::drain_who_friends` would once the server's OP_WhoAllResponse lands.
        //
        // #717: race the poll against the handler's own JoinHandle — see the identical comment on
        // `frame_carries_snapshot_age_header` just below. A naive unbounded poll loop here would
        // hang forever, not fail, if `get_who` ever returned early without registering a who_req.
        let tx = tokio::select! {
            tx = async {
                loop {
                    if let Some(tx) = command.take_who_req() { return tx; }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            } => tx,
            res = &mut handle => {
                let resp = res.expect("handler task panicked");
                panic!(
                    "expected /who to reach the who-request hand-off, but the handler returned \
                     early with status {} instead", resp.status()
                );
            }
        };
        tx.send(vec![]).unwrap();
        let resp = handle.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert!(v.get("snapshot_age_ms").is_some_and(|x| !x.is_null()),
            "GET /who must carry a PRESENT, non-null snapshot_age_ms key, got body: {v}");
        assert!(v["snapshot_age_ms"].as_u64().unwrap() < 1_000);
    }

    /// `/frame`'s body is a PNG — it cannot carry an in-band field at all, so (like
    /// `X-Zone-Assets-State` already does) freshness rides `X-Snapshot-Age-Ms` on the response.
    #[tokio::test]
    async fn frame_carries_snapshot_age_header() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();
        // Same #579 gate as above: without a matching zone, `/frame` refuses (503) before it ever
        // creates the frame-request oneshot, which would otherwise leave this test's poll loop
        // spinning forever waiting for a `tx` that is never placed — a genuine hang, not a flaky
        // slow box. Match the player's zone to the fixture's so the request actually reaches the
        // render-hand-off path this test means to exercise.
        set_gs(&state, |gs| gs.world.zone_name = "testfixture".to_string());
        let frame_req = state.camera.frame_req.clone();
        let mut handle = tokio::spawn({
            let state = state.clone();
            async move { get(state, "/frame").await }
        });
        // Play the render thread's part: satisfy the frame-request channel, exactly like the
        // renderer would once a frame is captured.
        //
        // #717: race the poll against the handler's own JoinHandle (the pattern #710 established
        // for the sibling `/frame` tests below). A naive unbounded poll loop here would hang
        // forever — not fail — if a change makes `get_frame` return EARLY, before ever populating
        // `frame_req`; that exact shape jammed the shared remote builder during #710's mutation
        // check. Racing against `handle` turns that into a clean, immediate assertion failure.
        let req = tokio::select! {
            req = async {
                loop {
                    if let Some(req) = frame_req.lock().unwrap().take() { return req; }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            } => req,
            res = &mut handle => {
                let resp = res.expect("handler task panicked");
                panic!(
                    "expected /frame to reach the frame-request hand-off, but the handler \
                     returned early with status {} instead", resp.status()
                );
            }
        };
        assert_eq!(req.camera_override, None, "no query params were passed — must be the \
            byte-for-byte pre-#422 on-screen readback path, not an override (#422)");
        req.tx.send(vec![0x89, b'P', b'N', b'G']).unwrap();
        let resp = handle.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(header_age_ms(&resp) < 1_000);
    }

    // ─────────── #701: /frame duplicate-query-param must get the JSON error shape ───────────

    /// eqoxide#701: a DUPLICATED recognized param (`?pitch=10&pitch=200`) must fail with the SAME
    /// `{"error":"invalid_camera_override","message":"…"}` JSON shape as a malformed *value* does
    /// (see the next test), not axum's generic `Query<FrameQuery>` plain-text rejection. Mutation-
    /// checked: reverting `get_frame`/`parse_frame_query` back to `Query(q): Query<FrameQuery>`
    /// makes this test fail on BOTH assertions — status stays 400, but content-type becomes
    /// `text/plain; charset=utf-8` and the body is `serde_urlencoded`'s raw
    /// `Failed to deserialize query string: duplicate field \`pitch\`` text, which is not valid JSON
    /// at all (`serde_json::from_slice` on it errors), so the shape assertions below are a real
    /// discriminator, not a tautology.
    #[tokio::test]
    async fn frame_duplicate_pitch_param_gets_invalid_camera_override_json() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();
        let resp = get(state, "/frame?pitch=10&pitch=200").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let content_type = resp.headers().get(header::CONTENT_TYPE).cloned();
        assert_eq!(
            content_type.as_ref().and_then(|v| v.to_str().ok()),
            Some("application/json"),
            "a duplicated query param must get the JSON error shape, not axum's plain-text \
             rejection (#701)"
        );
        let v = body_json(resp).await;
        assert_eq!(v["error"], "invalid_camera_override");
        let message = v["message"].as_str().expect("message must be a string");
        assert!(message.contains("pitch"),
            "the message must name the actually-duplicated field, got: {message}");
    }

    /// Same duplicate-key check, but on `yaw` instead of `pitch` — pins that the field name is
    /// genuinely read off the wire (not a hardcoded "pitch" string that would lie about `yaw`).
    #[tokio::test]
    async fn frame_duplicate_yaw_param_names_yaw_not_pitch() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();
        let resp = get(state, "/frame?yaw=10&yaw=20").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert_eq!(v["error"], "invalid_camera_override");
        let message = v["message"].as_str().unwrap();
        assert!(message.contains("yaw"), "message should name \"yaw\", got: {message}");
        assert!(!message.contains("\"pitch\""), "message should not blame pitch, got: {message}");
    }

    /// Control: a malformed *value* (not a duplicated key) must keep getting the exact same JSON
    /// shape it already had before #701 — this pins that routing the parse through
    /// `parse_frame_query`/`serde_urlencoded::from_str` by hand didn't change behavior for the case
    /// #422 already covered.
    #[tokio::test]
    async fn frame_malformed_pitch_value_still_gets_invalid_camera_override_json() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();
        // Unlike the duplicate-KEY tests above, `pitch=999` parses fine structurally — the failure
        // is a range check inside `resolve_camera_override`, which (per #422) only runs AFTER the
        // #579 zone-assets gate. Match the zone so this test reaches that code path instead of
        // getting a 503 from the unrelated gate first.
        set_gs(&state, |gs| gs.world.zone_name = "testfixture".to_string());
        let resp = get(state, "/frame?pitch=999").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let v = body_json(resp).await;
        assert_eq!(v["error"], "invalid_camera_override");
        assert!(v["message"].as_str().unwrap().contains("pitch"));
    }

    /// eqoxide#701 review finding B1: duplicating `allow_pending` (the #579 zone-assets-readiness
    /// bypass flag, NOT a camera param) must NOT come back as `invalid_camera_override` — that
    /// would tell the caller its camera angle was invalid on a request with no camera params at
    /// all. It gets its own `invalid_query_param` code instead, and the message must name
    /// `allow_pending`, not claim anything about the camera.
    ///
    /// Mutation-checked: reverting `duplicate_field_error` to always return
    /// `"invalid_camera_override"` (i.e. collapsing back to the pre-B1-fix behavior) makes the
    /// `error` assertion below fail — actual value is `"invalid_camera_override"`, not
    /// `"invalid_query_param"` — so this is a real discriminator between the two codes, not a
    /// tautology.
    #[tokio::test]
    async fn frame_duplicate_allow_pending_gets_invalid_query_param_not_camera_override() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();
        let resp = get(state, "/frame?allow_pending=1&allow_pending=0").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let v = body_json(resp).await;
        assert_eq!(
            v["error"], "invalid_query_param",
            "allow_pending is not a camera param — must not be mislabeled invalid_camera_override"
        );
        let message = v["message"].as_str().expect("message must be a string");
        assert!(message.contains("allow_pending"),
            "message must name allow_pending, got: {message}");
        assert!(!message.contains("camera"),
            "message must not falsely characterize this as a camera problem, got: {message}");
    }

    /// Control: the four actual camera fields (`preset`/`pitch`/`yaw`/`distance`) still get
    /// `invalid_camera_override` when duplicated — B1's fix only carved out `allow_pending`, it
    /// didn't change the code for the fields the code name is actually about.
    #[tokio::test]
    async fn frame_duplicate_preset_still_gets_invalid_camera_override() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();
        let resp = get(state, "/frame?preset=front&preset=side").await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert_eq!(v["error"], "invalid_camera_override");
        assert!(v["message"].as_str().unwrap().contains("preset"));
    }

    /// Blast-radius control: a duplicated key that is NOT one of `FRAME_QUERY_FIELDS` is untouched
    /// by #701 — it keeps behaving exactly like before (silently ignored, 200), since it was never
    /// part of the failure this issue is about and `FrameQuery` has no `deny_unknown_fields`.
    #[tokio::test]
    async fn frame_duplicate_unrecognized_key_is_still_silently_ignored() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();
        set_gs(&state, |gs| gs.world.zone_name = "testfixture".to_string());
        let frame_req = state.camera.frame_req.clone();
        let mut handle = tokio::spawn({
            let state = state.clone();
            async move { get(state, "/frame?foo=1&foo=2").await }
        });
        // Hardened during #701 review round 2: a naive unbounded poll-loop here would hang forever
        // (rather than fail) under any mutation that makes this input a 400 before ever
        // registering a frame_req — that exact pattern jammed the shared remote builder for
        // several minutes during this review round. Racing against `handle` bounds it.
        let req = tokio::select! {
            req = async {
                loop {
                    if let Some(req) = frame_req.lock().unwrap().take() { return req; }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            } => req,
            res = &mut handle => {
                let resp = res.expect("handler task panicked");
                panic!(
                    "expected an unrecognized duplicated key to be silently ignored and reach the \
                     frame-request hand-off, but the handler returned early with status {} instead",
                    resp.status()
                );
            }
        };
        assert_eq!(req.camera_override, None,
            "an unrecognized duplicated key is not a camera-override field — no override, no 400");
        req.tx.send(vec![0x89, b'P', b'N', b'G']).unwrap();
        let resp = handle.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Regression pin for the B1 rework of `parse_frame_query`: a bare `?` (empty query string,
    /// `RawQuery` yields `Some("")`) must still resolve to "no override" and 200, same as no query
    /// string at all. `form_urlencoded::parse("".as_bytes())` yields zero pairs, so the
    /// duplicate-key loop never runs and `serde_urlencoded::from_str("")` deserializes to an
    /// all-`None` `FrameQuery` — this pins that path stays reachable after B1 touched the function.
    #[tokio::test]
    async fn frame_empty_query_string_still_resolves_to_no_override() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();
        set_gs(&state, |gs| gs.world.zone_name = "testfixture".to_string());
        let frame_req = state.camera.frame_req.clone();
        let mut handle = tokio::spawn({
            let state = state.clone();
            async move { get(state, "/frame?").await }
        });
        // Race the frame_req poll against the handler task itself: if a mutation makes
        // `parse_frame_query` reject this input, `get_frame` returns EARLY (never registering a
        // frame_req at all), and the naive poll-loop this was copied from would spin forever
        // waiting for a frame_req that's never coming — hanging the whole test binary instead of
        // failing it. Racing against `handle` turns that into a clean, immediate assertion failure.
        let req = tokio::select! {
            req = async {
                loop {
                    if let Some(req) = frame_req.lock().unwrap().take() { return req; }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            } => req,
            res = &mut handle => {
                let resp = res.expect("handler task panicked");
                panic!(
                    "expected an empty `?` to resolve to no override and reach the frame-request \
                     hand-off, but the handler returned early with status {} instead",
                    resp.status()
                );
            }
        };
        assert_eq!(req.camera_override, None, "an empty `?` carries no params — no override");
        req.tx.send(vec![0x89, b'P', b'N', b'G']).unwrap();
        let resp = handle.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Regression pin for the B1 rework: a doubled `&` separator (`?pitch=10&&yaw=20`) produces an
    /// empty-string key/value pair between the two real ones — that empty key isn't in
    /// `FRAME_QUERY_FIELDS`, so it's ignored by the duplicate-key loop exactly like any other
    /// unrecognized key, and the two real, non-duplicated fields still resolve to a valid override
    /// (200), not a 400.
    #[tokio::test]
    async fn frame_double_ampersand_separator_still_resolves_normally() {
        let state = empty_state();
        state.net_health.lock().unwrap().last_tick = std::time::Instant::now();
        set_gs(&state, |gs| gs.world.zone_name = "testfixture".to_string());
        let frame_req = state.camera.frame_req.clone();
        let mut handle = tokio::spawn({
            let state = state.clone();
            async move { get(state, "/frame?pitch=10&&yaw=20").await }
        });
        // See the comment in `frame_empty_query_string_still_resolves_to_no_override`: racing
        // against `handle` turns a mutation that makes this input a 400 into a clean assertion
        // failure instead of a hang (the naive poll-loop this was copied from would spin forever
        // if `get_frame` returns early without ever registering a frame_req).
        let req = tokio::select! {
            req = async {
                loop {
                    if let Some(req) = frame_req.lock().unwrap().take() { return req; }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            } => req,
            res = &mut handle => {
                let resp = res.expect("handler task panicked");
                panic!(
                    "expected pitch/yaw (neither missing nor duplicated) to resolve to a valid \
                     override and reach the frame-request hand-off, but the handler returned \
                     early with status {} instead — a doubled `&` must not turn this into a 400",
                    resp.status()
                );
            }
        };
        assert!(req.camera_override.is_some(),
            "pitch/yaw are neither missing nor duplicated — a doubled `&` must not turn this into \
             a 400");
        req.tx.send(vec![0x89, b'P', b'N', b'G']).unwrap();
        let resp = handle.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // ─────────── #422: /frame camera-override resolution (pure, no renderer/HTTP needed) ───────────

    fn some_live_camera() -> CameraSnapshot {
        // Deliberately NOT all-zero/all-default, so a test that fell back to "whatever's already
        // there" instead of a genuinely-resolved value is caught rather than accidentally passing.
        CameraSnapshot {
            mode: CameraMode::AutoFollow,
            azimuth: 1.111,
            elevation: 0.222,
            radius: 99.0,
            focus: [0.0, 0.0, 0.0],
            eye: [99.0, 0.0, 0.0],
            occluded: false,
            still_blocked: false,
            drawn_frame: Some(7),
            drawn_at: Some(std::time::Instant::now()),
        }
    }

    #[test]
    fn no_params_resolve_to_no_override() {
        let q = FrameQuery::default();
        assert_eq!(resolve_camera_override(&q, 0.0, &some_live_camera()), Ok(None));
    }

    #[test]
    fn preset_default_resolves_to_no_override_same_as_no_params() {
        let q = FrameQuery { preset: Some("default".into()), ..Default::default() };
        assert_eq!(resolve_camera_override(&q, 123.0, &some_live_camera()), Ok(None));
    }

    #[test]
    fn preset_top_down_looks_nearly_straight_down_at_any_heading() {
        for heading in [0.0_f32, 90.0, 271.0] {
            let q = FrameQuery { preset: Some("top_down".into()), ..Default::default() };
            let ov = resolve_camera_override(&q, heading, &some_live_camera()).unwrap().unwrap();
            assert!((ov.elevation - 85.0_f32.to_radians()).abs() < 1e-5, "heading={heading}");
            assert!((ov.radius - 200.0).abs() < 1e-5);
        }
    }

    #[test]
    fn preset_behind_above_stays_relative_to_current_heading() {
        // The whole point of a preset (vs. the absolute numeric `yaw`) is that it tracks whichever
        // way the character is currently facing — pin that the resolved azimuth actually moves with
        // `heading_deg` rather than landing on some fixed world direction.
        let q = FrameQuery { preset: Some("behind_above".into()), ..Default::default() };
        let ov_a = resolve_camera_override(&q, 0.0, &some_live_camera()).unwrap().unwrap();
        let ov_b = resolve_camera_override(&q, 90.0, &some_live_camera()).unwrap().unwrap();
        assert!((ov_a.azimuth - ov_b.azimuth).abs() > 1e-3, "azimuth did not move with heading");
        assert_eq!(ov_a.azimuth, desired_azimuth(0.0));
        assert_eq!(ov_a.elevation, 45.0_f32.to_radians());
        assert_eq!(ov_a.radius, 70.0);
    }

    #[test]
    fn preset_front_looks_from_the_opposite_side_of_behind_above() {
        let live = some_live_camera();
        let behind = resolve_camera_override(
            &FrameQuery { preset: Some("behind_above".into()), ..Default::default() }, 0.0, &live,
        ).unwrap().unwrap();
        let front = resolve_camera_override(
            &FrameQuery { preset: Some("front".into()), ..Default::default() }, 0.0, &live,
        ).unwrap().unwrap();
        let diff = (front.azimuth - behind.azimuth).rem_euclid(std::f32::consts::TAU);
        assert!((diff - std::f32::consts::PI).abs() < 1e-4, "front should be ~180° from behind_above, diff={diff}");
    }

    #[test]
    fn unknown_preset_is_a_400_not_a_silent_fallback() {
        let q = FrameQuery { preset: Some("orbit-cam".into()), ..Default::default() };
        let err = resolve_camera_override(&q, 0.0, &some_live_camera()).unwrap_err();
        assert!(err.contains("orbit-cam"), "error should name the bad value, got: {err}");
    }

    #[test]
    fn preset_combined_with_numeric_is_rejected() {
        let q = FrameQuery {
            preset: Some("top_down".into()), pitch: Some("10".into()), ..Default::default()
        };
        assert!(resolve_camera_override(&q, 0.0, &some_live_camera()).is_err());
    }

    #[test]
    fn full_numeric_override_uses_every_given_field() {
        let q = FrameQuery {
            pitch: Some("30".into()), yaw: Some("90".into()), distance: Some("150".into()),
            ..Default::default()
        };
        let ov = resolve_camera_override(&q, 0.0, &some_live_camera()).unwrap().unwrap();
        assert!((ov.elevation - 30.0_f32.to_radians()).abs() < 1e-5);
        assert_eq!(ov.azimuth, desired_azimuth(90.0));
        assert_eq!(ov.radius, 150.0);
    }

    #[test]
    fn numeric_yaw_is_absolute_not_relative_to_current_heading() {
        // Unlike the presets, an explicit numeric `yaw` must land on the SAME world direction
        // regardless of the character's current heading — an agent scripting a fixed diagnostic
        // angle must get a reproducible frame, not one that silently depends on unobserved state.
        let q = FrameQuery { yaw: Some("45".into()), ..Default::default() };
        let ov_a = resolve_camera_override(&q, 0.0, &some_live_camera()).unwrap().unwrap();
        let ov_b = resolve_camera_override(&q, 200.0, &some_live_camera()).unwrap().unwrap();
        assert_eq!(ov_a.azimuth, ov_b.azimuth);
        assert_eq!(ov_a.azimuth, desired_azimuth(45.0));
    }

    #[test]
    fn partial_numeric_override_falls_back_to_the_live_camera_for_omitted_fields() {
        let live = some_live_camera();
        let q = FrameQuery { pitch: Some("10".into()), ..Default::default() }; // yaw/distance omitted
        let ov = resolve_camera_override(&q, 0.0, &live).unwrap().unwrap();
        assert!((ov.elevation - 10.0_f32.to_radians()).abs() < 1e-5);
        assert_eq!(ov.azimuth, live.azimuth, "omitted yaw must fall back to the LIVE camera, not 0");
        assert_eq!(ov.radius, live.radius, "omitted distance must fall back to the LIVE camera, not 0");
    }

    #[test]
    fn out_of_range_pitch_is_rejected() {
        for bad in ["86", "-86", "90"] {
            let q = FrameQuery { pitch: Some(bad.into()), ..Default::default() };
            assert!(resolve_camera_override(&q, 0.0, &some_live_camera()).is_err(), "pitch={bad}");
        }
    }

    #[test]
    fn boundary_pitch_is_accepted() {
        for ok in ["85", "-85", "0"] {
            let q = FrameQuery { pitch: Some(ok.into()), ..Default::default() };
            assert!(resolve_camera_override(&q, 0.0, &some_live_camera()).is_ok(), "pitch={ok}");
        }
    }

    #[test]
    fn out_of_range_yaw_is_rejected() {
        let q = FrameQuery { yaw: Some("361".into()), ..Default::default() };
        assert!(resolve_camera_override(&q, 0.0, &some_live_camera()).is_err());
    }

    #[test]
    fn out_of_range_distance_is_rejected() {
        for bad in ["0.5", "2001"] {
            let q = FrameQuery { distance: Some(bad.into()), ..Default::default() };
            assert!(resolve_camera_override(&q, 0.0, &some_live_camera()).is_err(), "distance={bad}");
        }
    }

    #[test]
    fn non_numeric_field_is_a_400_not_a_silent_zero() {
        for q in [
            FrameQuery { pitch: Some("north".into()), ..Default::default() },
            FrameQuery { yaw: Some("NaN-ish".into()), ..Default::default() },
            FrameQuery { distance: Some("far".into()), ..Default::default() },
        ] {
            let err = resolve_camera_override(&q, 0.0, &some_live_camera()).unwrap_err();
            assert!(err.contains("not a number"), "got: {err}");
        }
    }

    /// The age must reflect REALITY: it must keep CLIMBING for a source that has gone stale, driven
    /// by a read-time `now - last_tick`, never by anything the (in this test, deliberately silent)
    /// publisher itself updates. Mirrors `last_packet_age_advances_between_reads_with_no_publisher_
    /// running` above, over one of the newly-added JSON fields instead of the pre-existing `/debug`
    /// one.
    #[tokio::test]
    async fn snapshot_age_ms_climbs_across_reads_of_a_frozen_source() {
        let state = empty_state_wall_clock(); // #760: this test's subject IS the wall clock
        { let mut h = state.net_health.lock().unwrap(); let c = h.clock; h.last_tick = c.ago(5); }
        let first = body_json(get(state.clone(), "/messages").await).await["snapshot_age_ms"].as_u64().unwrap();
        // Nothing ticks, nothing republishes — just time passing, exactly like a wedged net thread.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let second = body_json(get(state, "/messages").await).await["snapshot_age_ms"].as_u64().unwrap();
        assert!(second > first,
            "snapshot_age_ms froze at {first} across two reads of a stale source — it must be \
             derived at READ time (#343/#646), not cached or driven by the dead publisher");
    }

    /// Same climb, over the header channel this time (`/doors`, a bare-array endpoint).
    #[tokio::test]
    async fn snapshot_age_header_climbs_across_reads_of_a_frozen_source() {
        let state = empty_state_wall_clock(); // #760: this test's subject IS the wall clock
        { let mut h = state.net_health.lock().unwrap(); let c = h.clock; h.last_tick = c.ago(5); }
        let first = header_age_ms(&get(state.clone(), "/doors").await);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let second = header_age_ms(&get(state, "/doors").await);
        assert!(second > first,
            "X-Snapshot-Age-Ms froze at {first} across two reads of a stale source — it must be \
             derived at READ time (#343/#646), not cached or driven by the dead publisher");
    }
}

/// #579 — the zone-assets loading gate. A mid-load observation must be an explicit *pending*,
/// never a confident *empty world*.
#[cfg(test)]
mod zone_asset_gate_tests {
    use super::*;
    use crate::testkit::{empty_state, set_gs};
    use axum::body::Body;
    use axum::http::Request;
    use eqoxide_nav::zone_assets::ZoneAssetState;
    use tower::ServiceExt;

    /// The zone `ZoneAssetState::test_ready()` is built for. The gate compares the loaded zone
    /// against the PLAYER's zone (#595 review F1), so a fixture that only sets one of the two is a
    /// `stale`/`unknown_zone` state, not a ready one.
    const FIXTURE_ZONE: &str = "testfixture";

    /// A state whose assets are loaded AND belong to the zone the character is standing in.
    ///
    /// The region map is `Ok` (a loaded map with no zone-line regions) rather than
    /// `test_ready()`'s never-attached grid, because a `ready` zone in production has ALWAYS had
    /// `set_region_data` called on it — with an `Ok` or with the loader's `Err`, never with nothing
    /// (#821 review round 2, B4). Before that round `/zone_exits` read the *other* slot,
    /// `shared_collision`, which `empty_state()` leaves `None`, so this fixture's grid was never
    /// consulted at all and the endpoint answered `[]` off a fall-through. It is consulted now.
    fn ready_state() -> HttpState {
        let s = empty_state();
        set_gs(&s, |gs| gs.world.zone_name = FIXTURE_ZONE.to_string());
        *eqoxide_nav::zone_assets::lock_state(&s.zone_assets) =
            ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
                eqoxide_core::region_map::RegionMap::flat_below(-10.0))));
        s
    }

    /// A state in the F1 window: the character is in `qeynos`, but the loaded assets (collision
    /// grid and all) are still the previous zone's.
    fn stale_state() -> HttpState {
        let s = empty_state();
        set_gs(&s, |gs| gs.world.zone_name = "qeynos".to_string());
        *eqoxide_nav::zone_assets::lock_state(&s.zone_assets) = ZoneAssetState::test_ready();
        s
    }

    fn with_state(st: ZoneAssetState) -> HttpState {
        let s = empty_state();
        set_gs(&s, |gs| gs.world.zone_name = FIXTURE_ZONE.to_string());
        *eqoxide_nav::zone_assets::lock_state(&s.zone_assets) = st;
        s
    }

    async fn get(state: HttpState, uri: &str) -> (StatusCode, serde_json::Value) {
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let code = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    /// Every state that is NOT ready — including `Idle` and the terminal `Failed` — must be
    /// reported as such, and must never be mistaken for a loaded world.
    fn not_ready_states() -> Vec<ZoneAssetState> {
        vec![
            ZoneAssetState::Idle,
            ZoneAssetState::pending("freportw", "Downloading zone 3/7 (12.4 MB)…"),
            ZoneAssetState::failed("freportw", "asset server unreachable"),
        ]
    }

    #[tokio::test]
    async fn debug_reports_the_live_pending_progress_while_the_zone_loads() {
        let s = with_state(ZoneAssetState::pending("freportw", "Downloading zone 3/7 (12.4 MB)…"));
        let (_, j) = get(s, "/debug").await;
        assert_eq!(j["zone_assets"]["state"], "pending");
        assert_eq!(j["zone_assets"]["zone"], "freportw");
        assert_eq!(j["zone_assets"]["status"], "Downloading zone 3/7 (12.4 MB)…");
        assert_eq!(j["zone_assets"]["collision_loaded"], false);
        assert!(j["zone_assets"]["detail"].as_str().unwrap().contains("STILL LOADING"));
    }

    /// A permanent failure must be distinguishable from "still loading" — reported as pending, an
    /// agent would wait forever for a load that is never coming.
    #[tokio::test]
    async fn debug_distinguishes_a_failed_load_from_a_pending_one() {
        let s = with_state(ZoneAssetState::failed("freportw", "GLB is corrupt"));
        let (_, j) = get(s, "/debug").await;
        assert_eq!(j["zone_assets"]["state"], "failed");
        assert_eq!(j["zone_assets"]["status"], "GLB is corrupt");
        assert!(j["zone_assets"]["detail"].as_str().unwrap().contains("terminal"));
    }

    /// #616 (agent-honesty): a terminal background-worker failure must reach the agent through this
    /// endpoint, not just flip an internal `App` field nothing ever reads. Healthy-by-default first —
    /// the field must not appear as a failure when nothing has gone wrong.
    #[tokio::test]
    async fn debug_reports_no_worker_failures_when_healthy() {
        let (_, j) = get(ready_state(), "/debug").await;
        assert_eq!(j["common_assets_failed"], serde_json::Value::Null);
        assert_eq!(j["model_sync_dead"], serde_json::Value::Null);
    }

    /// #816 (agent-honesty): `zone_map_load` must be PRESENT-and-null by default, same discipline as
    /// `net_thread_dead` above (#647 review F3) — a missing key and an explicit null both render as
    /// `Value::Null` through plain `assert_eq!`, so the presence check is a separate assertion.
    #[tokio::test]
    async fn debug_reports_zone_map_load_as_null_when_healthy() {
        let (_, j) = get(ready_state(), "/debug").await;
        assert!(j.get("zone_map_load").is_some(), "the field must be present, not omitted");
        assert_eq!(j["zone_map_load"], serde_json::Value::Null);
    }

    /// #816: once `sync_zone_points` records a failed `.txt` load, `/debug` must surface it — with a
    /// machine-readable `reason` (matching `ZoneMapLoadError::as_str()`) as well as a human `detail`.
    #[tokio::test]
    async fn debug_surfaces_a_failed_zone_map_load() {
        let s = ready_state();
        *s.world.zone_map_load.lock().unwrap() =
            Some(eqoxide_core::zone_map::ZoneMapLoadError::Missing);
        let (_, j) = get(s, "/debug").await;
        assert_eq!(j["zone_map_load"]["reason"], "zone_map_missing");
        assert_eq!(j["zone_map_load"]["detail"], "no .txt map file for this zone");
    }

    /// The `Unreadable` variant must be distinguishable from `Missing` — "the file is there but I
    /// could not read it" is a materially different diagnosis (permissions, a directory where a file
    /// was expected, disk trouble) than "there is no map for this zone at all".
    #[tokio::test]
    async fn debug_distinguishes_unreadable_zone_map_from_missing() {
        let s = ready_state();
        *s.world.zone_map_load.lock().unwrap() = Some(
            eqoxide_core::zone_map::ZoneMapLoadError::Unreadable(std::io::ErrorKind::PermissionDenied),
        );
        let (_, j) = get(s, "/debug").await;
        assert_eq!(j["zone_map_load"]["reason"], "zone_map_unreadable");
        assert_ne!(
            j["zone_map_load"]["reason"], "zone_map_missing",
            "an unreadable file must not be reported the same way as a confirmed-absent one"
        );
    }

    #[tokio::test]
    async fn debug_surfaces_a_common_asset_loader_failure() {
        let s = ready_state();
        *s.common_assets_failed.lock().unwrap() =
            Some("the common-asset-loader thread PANICKED while syncing assets".to_string());
        let (_, j) = get(s, "/debug").await;
        assert_eq!(
            j["common_assets_failed"],
            "the common-asset-loader thread PANICKED while syncing assets"
        );
    }

    #[tokio::test]
    async fn debug_surfaces_a_dead_model_sync_worker() {
        let s = ready_state();
        *s.model_sync_dead.lock().unwrap() =
            Some("the model-sync-worker thread PANICKED".to_string());
        let (_, j) = get(s, "/debug").await;
        assert_eq!(j["model_sync_dead"], "the model-sync-worker thread PANICKED");
    }

    /// #634 (agent-honesty): the `eq-net` thread's death must be visible in the REAL `/debug` body.
    /// Healthy-by-default first — if this field were non-null on a live session it could not
    /// discriminate anything.
    #[tokio::test]
    async fn debug_reports_a_live_net_thread_as_null() {
        let (_, j) = get(ready_state(), "/debug").await;
        // PRESENT-and-null, not merely absent: `j["missing_key"]` also renders as `Null`, so without
        // this the test would stay green if the field were dropped from the payload entirely
        // (#647 review, F3). Absence of trouble must be STATED, not inferred from a missing key.
        assert!(j.get("net_thread_dead").is_some(), "the field must be present, not omitted");
        assert_eq!(j["net_thread_dead"], serde_json::Value::Null);
    }

    /// The whole point of #634: the world fields are still fully populated and plausible, and the
    /// ONLY thing distinguishing this response from a healthy one is `net_thread_dead`. The assertion
    /// on `player.zone` is deliberate — it pins that the frozen-but-plausible payload is exactly what
    /// an agent would otherwise have believed.
    #[tokio::test]
    async fn debug_surfaces_a_dead_net_thread_alongside_the_frozen_world_it_invalidates() {
        let s = ready_state();
        set_gs(&s, |gs| gs.player_x = 100.0);
        *s.net_thread_dead.lock().unwrap() = Some(eqoxide_ipc::NetThreadDeath::new(
            eqoxide_ipc::NetThreadEnd::Panicked,
            "the eq-net thread PANICKED (boom) — the client is no longer talking to the server.",
        ));
        let (_, j) = get(s, "/debug").await;
        assert_eq!(j["player"]["zone"], FIXTURE_ZONE, "the stale world is still served, as before");
        assert!(
            j["net_thread_dead"].as_str().unwrap().contains("PANICKED"),
            "…but it is now explicitly marked dead: {}", j["net_thread_dead"]
        );
        // #890 typed the slot; the WIRE form must not have changed with it. A JSON object here
        // (`{"end":…,"reason":…}`) would break every agent and every doc paragraph about this field.
        assert!(j["net_thread_dead"].is_string(),
            "the served value must stay a plain reason string: {}", j["net_thread_dead"]);
    }

    #[tokio::test]
    async fn debug_reports_ready_with_the_evidence_once_the_zone_is_loaded() {
        let (_, j) = get(ready_state(), "/debug").await;
        assert_eq!(j["zone_assets"]["state"], "ready");
        assert_eq!(j["zone_assets"]["collision_loaded"], true);
        assert_eq!(j["zone_assets"]["terrain_meshes"], 1);
    }

    /// THE #560 falsehood: mid-load, `/zone_exits` answered out of a collision grid that did not
    /// exist yet and returned `[]` — "this zone has no exits at all". It must refuse instead.
    #[tokio::test]
    async fn zone_exits_refuses_instead_of_claiming_the_zone_has_none() {
        for st in not_ready_states() {
            let tag = st.tag();
            let (code, j) = get(with_state(st), "/zone_exits").await;
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE,
                "{tag}: an empty exit list here is a confident falsehood, not an answer");
            assert_eq!(j["error"], "zone_assets_not_ready");
            assert_eq!(j["zone_assets"]["state"], tag);
        }
    }

    #[tokio::test]
    async fn zone_exits_answers_normally_once_ready() {
        let (code, j) = get(ready_state(), "/zone_exits").await;
        assert_eq!(code, StatusCode::OK);
        assert!(j.is_array(), "a ready zone must still get the plain exits array");
    }

    /// A PNG of the placeholder ground plane is indistinguishable from a genuinely empty zone, so
    /// a mid-load capture is refused rather than handed over as if it were the world.
    #[tokio::test]
    async fn frame_refuses_a_mid_load_capture() {
        for st in not_ready_states() {
            let tag = st.tag();
            let (code, j) = get(with_state(st), "/frame").await;
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{tag}: a mid-load frame is not the zone");
            assert_eq!(j["error"], "zone_assets_not_ready");
        }
    }

    // ─────────── #595 review F1: the wrong-world window ───────────

    /// **The F1 capture, as a test.** The character is in `qeynos`; the previous zone's assets are
    /// still fully `Ready` because the render thread has not run `begin_zone_load` yet. `/debug`
    /// must NOT say `ready` — it used to, and `zone_exits` then returned the PREVIOUS zone's exit
    /// list with a 200 and the gate's blessing. "Wrong world" is the same lie class as "empty
    /// world", and a `ready` flag vouching for it is worse than saying nothing.
    #[tokio::test]
    async fn debug_reports_stale_not_ready_while_the_loaded_zone_is_the_one_we_left() {
        let (_, j) = get(stale_state(), "/debug").await;
        assert_eq!(j["zone_assets"]["state"], "stale");
        assert_eq!(j["zone_assets"]["reason"], "zone_assets_stale_for_previous_zone");
        assert_eq!(j["zone_assets"]["zone"], FIXTURE_ZONE, "the assets that ARE loaded");
        assert_eq!(j["zone_assets"]["player_zone"], "qeynos", "where the character actually is");
        assert!(j["zone_assets"]["detail"].as_str().unwrap().contains("DIFFERENT zone"));
    }

    /// …and every world-shaped endpoint refuses in that window rather than answering about the
    /// zone we left.
    #[tokio::test]
    async fn world_endpoints_refuse_in_the_wrong_zone_window() {
        for uri in ["/zone_exits", "/frame"] {
            let (code, j) = get(stale_state(), uri).await;
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE,
                "{uri}: answered about the PREVIOUS zone's world");
            assert_eq!(j["reason"], "zone_assets_stale_for_previous_zone");
        }
    }

    /// The other half of the identity rule: assets loaded but the client does not know what zone
    /// the character is in (pre-zone-in, or a zone-in that timed out) is also not an answer.
    #[tokio::test]
    async fn an_unknown_player_zone_is_not_ready() {
        let s = empty_state();   // player zone is ""
        *eqoxide_nav::zone_assets::lock_state(&s.zone_assets) = ZoneAssetState::test_ready();
        let (_, j) = get(s, "/debug").await;
        assert_eq!(j["zone_assets"]["state"], "unknown_zone");
        assert_eq!(j["zone_assets"]["reason"], "player_zone_unknown");
        assert_eq!(j["zone_assets"]["player_zone"], serde_json::Value::Null);
    }

    /// A `/goto` in the wrong-zone window must disclose it, exactly as in the loading window —
    /// nothing can be routed against another zone's collision grid.
    #[tokio::test]
    async fn goto_discloses_the_wrong_zone_window() {
        let s = stale_state();
        let st = eqoxide_nav::zone_assets::lock_state(&s.zone_assets).clone();
        let why = eqoxide_nav::zone_assets::usability(&st, &s.player().zone);
        assert_eq!(why.map(|w| w.as_str()), Some("zone_assets_stale_for_previous_zone"));
    }

    /// Every `/frame` 200 carries `X-Zone-Assets-State`, so a PNG fetched with `?allow_pending=1`
    /// cannot be mistaken downstream for one of the real zone (#595 review nit).
    #[tokio::test(start_paused = true)]
    async fn frame_declares_its_zone_asset_state_in_a_header() {
        let s = with_state(ZoneAssetState::pending("freportw", "loading…"));
        let app = router().with_state(s);
        let resp = app.oneshot(Request::get("/frame?allow_pending=1").body(Body::empty()).unwrap())
            .await.unwrap();
        // No renderer is attached, so the capture itself 503s — but the header is computed from the
        // zone-asset state before that and is what this test is about.
        let hdr = resp.headers().get(ZONE_ASSETS_STATE_HEADER).map(|v| v.to_str().unwrap().to_string());
        assert!(hdr.is_none() || hdr.as_deref() == Some("pending"));
        assert_eq!(
            eqoxide_nav::zone_assets::usability(
                &ZoneAssetState::pending("freportw", "loading…"), "freportw").unwrap().state_word(),
            "pending");
    }

    /// …but the loading screen is still reachable on purpose, for a caller that asks for it
    /// explicitly. (No renderer is attached here, so this 503s from the capture timeout instead —
    /// what matters is that it is NOT the `zone_assets_not_ready` refusal.)
    #[tokio::test(start_paused = true)] // no renderer is attached; elapse the capture timeout instantly
    async fn frame_allow_pending_opts_past_the_gate() {
        let s = with_state(ZoneAssetState::pending("freportw", "loading…"));
        let app = router().with_state(s);
        let resp = app.oneshot(Request::get("/frame?allow_pending=1").body(Body::empty()).unwrap())
            .await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(!body.contains("zone_assets_not_ready"),
            "?allow_pending must bypass the #579 gate, got: {body}");
    }
}

/// **#713 — the zone-cross observables at the HTTP boundary.**
///
/// These are the agent-facing half of #713's three items. Each pins that a fact the net thread
/// decided actually REACHES something an agent can poll: the #409 failure mode is a value that is
/// computed correctly, stored correctly, and published nowhere.
#[cfg(test)]
mod zone_cross_observables_713 {
    use super::*;
    use crate::testkit::{empty_state, set_gs};
    use axum::body::Body;
    use axum::http::Request;
    use eqoxide_core::game_state::ZonePoint;
    use eqoxide_core::zone_cross::{CrossAttempts, ZoneCrossPlan, ZoneCrossResolution, MAX_CROSS_ATTEMPTS};
    use eqoxide_nav::zone_assets::ZoneAssetState;
    use tower::ServiceExt;

    const FIXTURE_ZONE: &str = "testfixture";
    /// The baked region index the fixture's DRNTP box carries.
    const IDX: i32 = 7;
    const HERE: u16 = 54;
    const OTHER: u16 = 181;

    fn zp(iterator: u32, zone_id: u16) -> ZonePoint {
        ZonePoint { iterator, server_x: 1.0, server_y: 2.0, server_z: 3.0, heading: 0.0, zone_id }
    }

    async fn get(state: HttpState, uri: &str) -> (StatusCode, serde_json::Value) {
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let code = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    /// A ready state whose collision grid carries ONE zone-line region (index [`IDX`]), plus the
    /// advertised entrance list the gate reads.
    fn exits_state(points: Vec<ZonePoint>) -> HttpState {
        let s = empty_state();
        set_gs(&s, |gs| {
            gs.world.zone_name = FIXTURE_ZONE.to_string();
            gs.world.zone_id = HERE;
        });
        let ready = ZoneAssetState::test_ready_with_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, IDX),
        )));
        *s.shared_collision.write().unwrap() = ready.collision().cloned();
        *eqoxide_nav::zone_assets::lock_state(&s.zone_assets) = ready;
        *s.world.zone_points.lock().unwrap() = points;
        s
    }

    /// **#713 item 3: an exit that CANNOT be auto-crossed says so BEFORE the agent walks there.**
    ///
    /// A `zone_id: null` exit in a zone where the #679/#683 gate is closed is inert: standing on it
    /// does nothing. The only signal used to be a message-log line emitted once the character had
    /// already arrived — an answer to a question the agent could only ask by making the trip. The
    /// `gated` flag is that verdict, reported from the SAME function the net thread acts on.
    ///
    /// Note what is NOT claimed: `gated: false` is not a promise that crossing will succeed (the
    /// server still decides), and this changes nothing about what the gate decides.
    ///
    /// **Mutation checks:** hardcode `gated: false` → the first two cases go RED; hardcode `true` →
    /// the other two go RED; drop the `dest_of.get(&index).is_none()` conjunct → the
    /// `advertised_exit_in_a_gated_zone` case goes RED (an exit with a known destination is crossed
    /// by `perform_cross`, which never consults this gate).
    #[tokio::test]
    async fn a_zone_exit_that_cannot_be_auto_crossed_is_marked_gated_713() {
        // (label, advertised points, expected `gated` for the region-IDX exit)
        let cases: Vec<(&str, Vec<ZonePoint>, bool)> = vec![
            // No server adverts have arrived: every index is unresolved, so the fallback is shut.
            ("no_adverts_yet", vec![], true),
            // A same-zone pad is advertised here → the fallback refuses in this zone (#679).
            ("same_zone_pad_advertised", vec![zp(9, HERE)], true),
            // Cross-zone adverts only (the qrg shape) → the fallback is open, the exit IS crossable.
            ("cross_zone_adverts_only", vec![zp(99, OTHER)], false),
            // The exit's OWN index is advertised → it has a known destination and is crossed by the
            // resolved path, which never consults the unresolved gate — even though the gate is shut.
            ("advertised_exit_in_a_gated_zone", vec![zp(IDX as u32, OTHER), zp(5, HERE)], false),
        ];
        for (label, points, want_gated) in cases {
            let advertised_dest = points.iter().any(|p| p.iterator as i32 == IDX);
            let (code, j) = get(exits_state(points), "/zone_exits").await;
            assert_eq!(code, StatusCode::OK, "{label}");
            let exits = j.as_array().unwrap_or_else(|| panic!("{label}: expected an array, got {j}"));
            let exit = exits.iter().find(|e| e["index"] == IDX)
                .unwrap_or_else(|| panic!("{label}: the fixture bakes exactly one exit: {j}"));
            assert_eq!(exit["gated"], want_gated,
                "{label}: `gated` must let an agent see BEFORE walking there whether this exit can \
                 be auto-crossed at all");
            assert_eq!(exit["zone_id"].is_null(), !advertised_dest,
                "{label}: the pre-existing honest-unknown destination is unchanged");
        }
    }

    /// **#713 review round 2, N1: when a zone advertises TWO points under one iterator, the
    /// `zone_id` reported beside `gated` must name the one the client would actually take.**
    ///
    /// `zone_exits` built its index→destination map with `.collect()` into a `HashMap`, which keeps
    /// the LAST duplicate; the net thread's `ActionLoop::resolve_cross_destination` uses `.find()`,
    /// which takes the FIRST. `gated` was never affected (it only asks `is_none()`), but the
    /// destination printed next to it could name a zone the client would not go to — a confident
    /// wrong answer with no way for the agent to check it. Both sides now take the first match.
    ///
    /// **Mutation check:** restore the `.collect()` (or make the loop overwrite instead of
    /// `or_insert`) → this goes RED.
    #[tokio::test]
    async fn duplicate_advertised_indices_report_the_destination_the_client_would_take_713() {
        // Two adverts under the SAME iterator as the baked region. `resolve_cross_destination`
        // takes the first (`OTHER`); the observable must not disagree with it.
        let s = exits_state(vec![zp(IDX as u32, OTHER), zp(IDX as u32, OTHER + 1)]);
        let (code, j) = get(s, "/zone_exits").await;
        assert_eq!(code, StatusCode::OK);
        let exit = j.as_array().unwrap().iter().find(|e| e["index"] == IDX)
            .unwrap_or_else(|| panic!("the fixture bakes exactly one exit: {j}"));
        assert_eq!(exit["zone_id"], OTHER,
            "the reported destination must be the FIRST advertised match — the one \
             `resolve_cross_destination`'s `.find()` picks. Reporting the last one names a zone the \
             client would never take us to: {j}");
    }

    /// **#713 item 1: after the bound is hit, an agent polling the client sees a TERMINAL state —
    /// not silence.** Silence would be worse than the retry storm it replaced: the agent would wait
    /// forever for a crossing that will never be attempted again.
    ///
    /// **Mutation checks:** publish `null` unconditionally → the non-null assertion goes RED; drop
    /// the `.filter(|t| t.blocks())` → the "a tally below the bound says nothing" case goes RED
    /// (the client would announce it had given up while it is still trying).
    #[tokio::test]
    async fn debug_publishes_the_terminal_zone_cross_stop_713() {
        // Below the bound: still trying, so there is nothing to disclose.
        let s = empty_state();
        set_gs(&s, |gs| gs.zone_cross_attempts = Some(CrossAttempts::record(None, IDX)));
        let (_, j) = get(s, "/debug").await;
        assert!(j["zone_cross_stopped"].is_null(),
            "a client that is still retrying must not report that it stopped");

        // At the bound: terminal, and it says so with the region and the count.
        let s = empty_state();
        set_gs(&s, |gs| {
            let mut t = CrossAttempts::record(None, IDX);
            while !t.blocks() { t = CrossAttempts::record(Some(t), IDX); }
            gs.zone_cross_attempts = Some(t);
        });
        let (_, j) = get(s, "/debug").await;
        let stopped = &j["zone_cross_stopped"];
        assert!(!stopped.is_null(), "the terminal state must be OBSERVABLE, not silent: {j}");
        assert_eq!(stopped["reason"], "cross_attempt_limit");
        assert_eq!(stopped["region_index"], IDX);
        assert_eq!(stopped["attempts"], MAX_CROSS_ATTEMPTS);
        let detail = stopped["detail"].as_str().unwrap();
        assert!(detail.contains("TERMINAL"), "and it must be distinguishable from a crossing in flight");
        assert!(detail.contains("cannot tell"),
            "it must NOT claim to know WHY — a denial and a server that answered nothing look identical");
    }

    /// **#713 item 2: the best-effort degradation is machine-readable.** `POST /v1/move/zone_cross`
    /// returning 200 and then walking to a line whose destination only the server knows is not a
    /// lie, but before this it was undetectable — prose in a message log is not a signal.
    ///
    /// **Mutation checks:** publish the object for `Advertised` too (drop the `is_best_effort`
    /// filter) → the null case goes RED; publish `null` always → the non-null case goes RED.
    #[tokio::test]
    async fn debug_publishes_the_best_effort_zone_cross_marker_713() {
        // A resolution with an advertised destination is NOT a degradation — stay quiet.
        let s = empty_state();
        set_gs(&s, |gs| gs.zone_cross_plan = Some(ZoneCrossPlan {
            requested_zone_id: Some(OTHER), index: IDX,
            resolution: ZoneCrossResolution::Advertised { zone_id: OTHER },
        }));
        let (_, j) = get(s, "/debug").await;
        assert!(j["zone_cross_best_effort"].is_null(),
            "a cross to an advertised line is the normal case and must not read as degraded");

        let s = empty_state();
        set_gs(&s, |gs| gs.zone_cross_plan = Some(ZoneCrossPlan {
            requested_zone_id: Some(OTHER), index: IDX,
            resolution: ZoneCrossResolution::ServerResolved,
        }));
        let (_, j) = get(s, "/debug").await;
        let be = &j["zone_cross_best_effort"];
        assert!(!be.is_null(), "the degradation must be detectable without diffing after the fact: {j}");
        assert_eq!(be["reason"], "server_resolved_destination");
        assert_eq!(be["requested_zone_id"], OTHER);
        assert_eq!(be["region_index"], IDX);
        assert!(be["detail"].as_str().unwrap().contains("may land you somewhere other than"),
            "and it must say what the caller actually risks");
    }
}
/// **#803 — `/v1/observe/zone_exits` must not publish a failed file read as "this zone has no way
/// out".**
///
/// The endpoint answers out of the zone's region map (`maps/water/<zone>.wtr`), whose DRNTP
/// zone-line regions ARE the exits. When that file was missing, truncated, of an unsupported
/// version, or simply never attached, the loader's failure was discarded into a `None`,
/// `Collision::zone_line_indices()` `.unwrap_or_default()`-ed it to an empty vec, and this endpoint
/// served `[]` with **200 OK** — byte-identical to the true, common answer for a zone that
/// genuinely has no zone lines. Exits are the only way out of a zone, so the agent concluded it was
/// sealed in, from a success response it had no way to doubt.
///
/// The two halves below are deliberately separate tests with different names: one pins that the
/// honest `[]` is STILL served (so the fix cannot degenerate into "refuse whenever the list is
/// empty"), the other that a load failure is a 503 naming its cause. Neither alone is the property.
#[cfg(test)]
mod zone_exits_never_publishes_a_failed_read_as_empty_803 {
    use super::*;
    use crate::testkit::{empty_state, set_gs};
    use axum::body::Body;
    use axum::http::Request;
    use eqoxide_core::region_map::{RegionLoadError, RegionMap};
    use eqoxide_nav::zone_assets::ZoneAssetState;
    use tower::ServiceExt;

    async fn get(state: HttpState, uri: &str) -> (StatusCode, serde_json::Value) {
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let code = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (code, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    /// A `Ready` zone — terrain loaded, so the #579 gate is open and cannot be what refuses — whose
    /// grid carries the given region-data outcome. This is the combination the bug lives in: the
    /// zone is genuinely usable, and only the `.wtr` is not.
    fn state_with_region_data(
        data: Result<std::sync::Arc<RegionMap>, RegionLoadError>,
    ) -> HttpState {
        let s = empty_state();
        set_gs(&s, |gs| gs.world.zone_name = "testfixture".to_string());
        let ready = ZoneAssetState::test_ready_with_region_data(data);
        *s.shared_collision.write().unwrap() = ready.collision().cloned();
        *eqoxide_nav::zone_assets::lock_state(&s.zone_assets) = ready;
        s
    }

    /// **Half one: the honest empty stays green.** A zone whose region map LOADED and contains no
    /// zone-line regions still gets `[]` with 200 — that is a real reading of the world and the
    /// overwhelmingly common one. A "fix" that refused on an empty list would break every zone.
    #[tokio::test]
    async fn a_zone_that_genuinely_has_no_zone_lines_still_answers_the_empty_list() {
        let (code, j) = get(
            state_with_region_data(Ok(std::sync::Arc::new(RegionMap::flat_below(-10.0)))),
            "/zone_exits",
        ).await;
        assert_eq!(code, StatusCode::OK,
            "a loaded map with no zone-line regions is an ANSWER; refusing here would break the \
             common case the empty list legitimately describes");
        assert_eq!(j, serde_json::json!([]));
    }

    /// …and a loaded map WITH an exit still lists it, so half one is not passing because the
    /// endpoint stopped reporting exits altogether.
    #[tokio::test]
    async fn a_zone_with_a_zone_line_still_lists_it() {
        let (code, j) = get(
            state_with_region_data(Ok(std::sync::Arc::new(
                RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, 7)))),
            "/zone_exits",
        ).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(j.as_array().map(|a| a.len()), Some(1), "the fixture bakes exactly one exit: {j}");
        assert_eq!(j[0]["index"], 7);
    }

    /// **Half two: THE #803 FALSEHOOD.** Every way the `.wtr` can fail to load, plus the
    /// never-attached case, must come back as an explicit refusal naming its cause — never `[]`,
    /// never 200. All five are looped because the old code collapsed them into one `None`; a fix
    /// that only handled the failure in the issue title would rebuild the same lie for the rest.
    #[tokio::test]
    async fn a_region_map_that_did_not_load_refuses_instead_of_reporting_no_exits() {
        let cases: Vec<(Result<std::sync::Arc<RegionMap>, RegionLoadError>, &str)> = vec![
            (Err(RegionLoadError::Missing), "region_data_missing"),
            (Err(RegionLoadError::Unreadable(std::io::ErrorKind::PermissionDenied)),
                "region_data_unreadable"),
            (Err(RegionLoadError::NotRegionData), "region_data_not_region_data"),
            (Err(RegionLoadError::UnsupportedVersion(99)), "region_data_unsupported_version"),
            (Err(RegionLoadError::Truncated { declared_nodes: 400, bytes: 12 }),
                "region_data_truncated"),
        ];
        for (data, want_reason) in cases {
            let (code, j) = get(state_with_region_data(data), "/zone_exits").await;
            assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE,
                "{want_reason}: a failed read served as `[]`/200 tells the agent this zone has no \
                 way out — a confident falsehood it cannot detect (#803). Got {code}: {j}");
            assert_eq!(j["error"], "zone_region_data_unavailable", "body: {j}");
            assert_eq!(j["reason"], want_reason,
                "the reason must name the ACTUAL failure — 'the file is absent' and 'the file is \
                 truncated' call for different operator action");
            assert!(!j["detail"].as_str().unwrap_or("").is_empty(), "body: {j}");
        }
    }

    /// **#821 review round 2, B4: the `shared_collision` slot is no longer an input, so an unset one
    /// cannot produce `[]`.**
    ///
    /// The handler used to take permission from the zone-assets verdict and then read the grid from
    /// the separate `shared_collision` slot behind `if let Some(col)` with no `else`. A `None` there
    /// returned `200 []` — "this zone has no way out" — having consulted no region map at all. That
    /// path was live in-tree and green: the HTTP testkit builds exactly that combination.
    ///
    /// Both directions are asserted from ONE fixture, so this cannot pass by the endpoint having
    /// stopped reading exits: with the slot empty and the region map holding an exit, the exit is
    /// still listed; with the slot empty and the region map failed, it is still a 503.
    #[tokio::test]
    async fn an_unset_shared_collision_slot_can_no_longer_produce_an_empty_exit_list() {
        for (label, data, want) in [
            ("a loaded map WITH an exit",
             Ok(std::sync::Arc::new(RegionMap::zone_line_box(-4.0, 4.0, -4.0, 4.0, -2.0, 2.0, 7))),
             StatusCode::OK),
            ("a map that did not load",
             Err(RegionLoadError::Missing),
             StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let s = state_with_region_data(data);
            // The exact fixture shape the old fall-through fired on: `Ready` for this zone, and
            // NOTHING in the shared slot.
            *s.shared_collision.write().unwrap() = None;
            let (code, j) = get(s, "/zone_exits").await;
            assert_eq!(code, want,
                "{label}: with the shared slot empty the endpoint must still answer out of the \
                 grid the `Ready` state OWNS, never out of a fall-through. Got {code}: {j}");
            assert_ne!(j, serde_json::json!([]),
                "{label}: `[]` off an unread region map is the #803 falsehood with a new cause");
        }
    }

    /// The refusal is deliberately NOT the `zone_assets_not_ready` verdict: that one is also read by
    /// the nav walker's drive gate and the net thread's zone-cross drain, and a missing `.wtr` does
    /// not invalidate the terrain those consume. Pinned so a later "simplification" that folds the
    /// two together has to change this line and say why.
    ///
    /// **Asserted positively** (#821 review round 2, minor M1). This used to be only
    /// `assert_ne!(j["error"], "zone_assets_not_ready")`, which is vacuously true whenever the body
    /// is an ARRAY — `j["error"]` is then `Value::Null` — so the endpoint restored to `200 []` (the
    /// exact regression this test is named after) passed it. Measured, not reasoned: the reviewer
    /// ran that mutation and this test survived it.
    #[tokio::test]
    async fn the_refusal_is_distinct_from_the_zone_assets_gate() {
        let (code, j) = get(state_with_region_data(Err(RegionLoadError::Missing)), "/zone_exits").await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "body: {j}");
        assert_eq!(j["error"], "zone_region_data_unavailable",
            "region data is a separate readiness question from the zone's terrain assets, and the \
             refusal has to SAY so — an array body would make a bare `assert_ne!` pass: {j}");
        assert_ne!(j["error"], "zone_assets_not_ready", "body: {j}");
    }
}

/// Reach control for `observe::tests::no_past_dated_net_health_stamp_is_taken_from_a_clock_other_than_the_one_that_reads_it` (#760/C1).
///
/// That guard scans this file's source text. A scanner that silently stops early — as it did, when
/// a `/*` inside a route glob in a doc comment latched its block-comment state on line 1 — reports
/// a clean scan of a corpus it never read, which is a confident falsehood. The guard asserts it can
/// SEE this constant; because it is the last item in the file, seeing it proves the scan arrived at
/// the end. **Keep it last.**
#[cfg(test)]
pub(crate) const GUARD_REACH_SENTINEL_OBSERVE: u8 = 0;
