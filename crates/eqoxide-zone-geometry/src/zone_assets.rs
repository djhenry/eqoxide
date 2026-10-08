//! The zone's terrain + collision **load state** — the agent-honesty answer to "is the world I am
//! about to describe actually loaded yet?" (#579).
//!
//! ## Why this type exists
//!
//! A zone's terrain arrives from the asset server as one large GLB (freportw is ~30 MB) and is
//! decoded, collided and uploaded on a background thread over several seconds. Until that finishes
//! the client is standing on a flat placeholder ground plane with **no collision at all** — and
//! before this type existed it reported that state as if it were the world: `/observe/frame` showed
//! an empty plain, `/observe/zone_exits` returned `[]` ("this zone has no exits"), and `/goto`
//! reported `nav_state: "navigating"` while the walker steered in a straight line through geometry
//! that had not been built. An observer in that window read a **false empty world** as the truth —
//! that is exactly what produced the bogus #560 report ("flat plain, 0 collision, 700u
//! unobstructed"), which a later load on the same code refuted.
//!
//! An AI agent has no eyes: whatever the client says IS its world. So a mid-load observation must
//! be an explicit **pending**, never a confident **empty**.
//!
//! ## Why the collision grid lives *inside* `Ready`
//!
//! A bare `zone_assets_loaded: bool` is one careless edit away from reporting "ready" for a zone
//! whose collision never got built — the `connected: true`-with-no-writer bug (#343) all over
//! again. Here [`ZoneAssetState::Ready`] **owns the `Arc<Collision>`**, and its only constructor
//! ([`ZoneAssetState::ready`]) refuses to build it without a collision grid that actually has
//! geometry and at least one terrain mesh — it returns [`ZoneAssetState::Failed`] instead. So
//! "ready, but there is no world" is not a state this type can represent, and every `Ready` an
//! agent ever reads carries its own evidence.
//!
//! `Failed` is a third, distinct state on purpose: a permanent load failure silently reported as
//! "pending forever" is its own lie (the agent would wait for something that is never coming).

use std::sync::Arc;

use crate::collision::Collision;

/// Shared handle to [`ZoneAssetState`]. Written by the render/app thread (which owns the zone
/// loader) and read by the HTTP layer. Cheap to clone — `Ready` holds only an `Arc`.
pub type ZoneAssetStateShared = Arc<std::sync::Mutex<ZoneAssetState>>;

/// Where this process is in loading the current zone's terrain + collision.
#[derive(Clone, Default)]
pub enum ZoneAssetState {
    /// No zone has been loaded and none is loading — e.g. before the first zone-in. Distinct from
    /// `Pending`: nothing is on its way, so waiting for a `Ready` here would hang forever.
    #[default]
    Idle,
    /// A load is in flight for `zone`. `status` is the loader's own live progress line
    /// ("Downloading zone 3/7 (12.4 MB)…", "Building collision grid…", …).
    Pending { zone: String, status: String },
    /// Terrain meshes are uploaded AND the collision grid is built. Only constructible through
    /// [`ZoneAssetState::ready`] (`#[non_exhaustive]` blocks struct-literal construction from other
    /// crates), so this variant cannot exist without the evidence it reports.
    #[non_exhaustive]
    Ready {
        zone: String,
        /// Number of terrain meshes uploaded for this zone.
        terrain_meshes: usize,
        /// The very collision grid the nav planner is using. Its presence here is the proof.
        collision: Arc<Collision>,
    },
    /// The load finished and did NOT produce a usable world (asset-server error, missing GLB,
    /// corrupt GLB, or geometry that built no collision). Terminal until the next zone change —
    /// an agent must not keep waiting for `Ready`.
    Failed { zone: String, reason: String },
}

impl ZoneAssetState {
    /// The ONLY way to build [`ZoneAssetState::Ready`]. Downgrades to `Failed` when the load did
    /// not actually produce a world, so a caller cannot publish an empty "ready".
    pub fn ready(zone: &str, terrain_meshes: usize, collision: Arc<Collision>) -> Self {
        if terrain_meshes == 0 {
            return Self::Failed {
                zone: zone.to_string(),
                reason: "the zone load produced ZERO terrain meshes — there is no world here to \
                         report as ready".to_string(),
            };
        }
        // `has_triangles`, NOT `has_geometry`: the latter is `cols != 0`, a BOUNDS proxy that a
        // single degenerate triangle satisfies. "There is a world here" must not be satisfiable by
        // a grid that can answer nothing (#595 review).
        if !collision.has_triangles() {
            return Self::Failed {
                zone: zone.to_string(),
                reason: "the zone's collision grid was built but contains NO geometry — nav and \
                         collision answers here would be about an empty world".to_string(),
            };
        }
        Self::Ready { zone: zone.to_string(), terrain_meshes, collision }
    }

    /// A load has started (or restarted) for `zone`. Use on every zone change so the state can
    /// never stay stale-`Ready` from the previous zone.
    pub fn pending(zone: &str, status: &str) -> Self {
        Self::Pending { zone: zone.to_string(), status: status.to_string() }
    }

    /// The load ended without a usable world.
    pub fn failed(zone: &str, reason: &str) -> Self {
        Self::Failed { zone: zone.to_string(), reason: reason.to_string() }
    }

    /// Machine-readable state tag: `"idle"`, `"pending"`, `"ready"` or `"failed"`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Idle       => "idle",
            Self::Pending {..} => "pending",
            Self::Ready {..}   => "ready",
            Self::Failed {..}  => "failed",
        }
    }

    /// True only when terrain AND collision are genuinely built for the current zone.
    pub fn is_ready(&self) -> bool { matches!(self, Self::Ready { .. }) }

    /// The zone this state is about (`None` only for `Idle`).
    pub fn zone(&self) -> Option<&str> {
        match self {
            Self::Idle => None,
            Self::Pending { zone, .. } | Self::Ready { zone, .. } | Self::Failed { zone, .. } => Some(zone),
        }
    }

    /// The collision grid — `Some` only in `Ready`.
    ///
    /// **Matched EXHAUSTIVELY, with no `_` wildcard, on purpose (#826).** This function and
    /// [`usability`] must react to a NEW state variant the same way, because
    /// [`usable_collision`] pairs them: `usability` decides whether to bless the state and then
    /// this decides what grid to hand over. A wildcard here breaks that pairing silently — the
    /// author of a FIFTH variant that carries a usable grid would be forced by the compiler to
    /// classify it in `usability` (which has no wildcard), while this function kept answering
    /// `None`, and `usable_collision`'s documented-unreachable `ok_or(NotUsable::Idle)` fallback
    /// would start firing. That converts a usable grid into a REFUSAL with no diagnostic — the
    /// exact confusion between an answer and a refusal that #803 existed to remove.
    ///
    /// Measured, not reasoned: with a wildcard here and a fifth `ProbeRefreshing { zone,
    /// collision }` variant added (the enum has FOUR — `Idle`, `Pending`, `Ready`, `Failed`), the
    /// crate compiled once every arm the compiler ASKED for was filled in, and `usability` then
    /// said `None` (usable) while `usable_collision` returned `Err(Idle)` over a live grid.
    /// `usable_collision_agrees_with_usability_for_every_state` stayed green throughout, because
    /// its `states` vec did not know about the new variant.
    ///
    /// **What spelling the arms out achieves, precisely.** It makes the OMISSION unrepresentable:
    /// this function and `usability` now fail to compile together, so no variant can be classified
    /// in one and silently defaulted in the other. It does NOT make an *inconsistent* pair
    /// unrepresentable — an author who writes `Self::New {..} => None` here and classifies the same
    /// variant as usable in `usability` still compiles, and that re-opens exactly the hole above.
    /// Measured on this tree by the #837 reviewer: the whole crate suite stayed green with that
    /// pair in place. The second line of defence for it is the roll call in
    /// `usable_collision_agrees_with_usability_for_every_state`, which reds when a variant is added
    /// — and it is a forced read, not a proof either. Nor is `collision` the only compile catch
    /// point: a new variant also reds `tag`, `zone`, `detail` and the hand-written `Debug` impl.
    pub fn collision(&self) -> Option<&Arc<Collision>> {
        match self {
            Self::Ready { collision, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            Self::Failed {..}  => None,
        }
    }

    /// A human/agent-readable sentence explaining what this state means for anything the client
    /// reports about the world right now.
    pub fn detail(&self) -> &'static str {
        match self {
            Self::Idle => "no zone has been loaded in this client yet, and no load is running. \
                           Anything reported about zone geometry, collision or navigability is \
                           about NOTHING — do not read it as an empty world.",
            Self::Pending {..} => "the zone's terrain GLB and collision grid are STILL LOADING. The \
                           frame currently shows a placeholder ground plane and there is no \
                           collision, so a flat/empty view, an empty exit list, or an unobstructed \
                           path right now is an artefact of the load — NOT the real zone (#560). \
                           Poll until this reads `ready`.",
            Self::Ready {..} => "terrain meshes are uploaded and the collision grid is built: what \
                           the client reports about this zone's geometry is the real zone.",
            Self::Failed {..} => "the zone's assets FAILED to load and no retry is running. The \
                           client is showing a fallback ground plane with no collision. This is \
                           terminal for this zone — waiting for `ready` will hang. Nav and \
                           geometry answers here are unavailable, not empty.",
        }
    }

    /// A genuine `Ready` over a trivial flat-floor collision grid, for tests in crates that cannot
    /// build a zone (`Ready` is deliberately not fabricable without a real grid). Test-only.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn test_ready() -> Self {
        Self::test_ready_with_water(None)
    }

    /// [`Self::test_ready`], with an optional region map (`.wtr` BSP) installed on the grid — for
    /// tests in crates that cannot build a zone but need zone-line-region behaviour (e.g. the #683
    /// unresolved-index auto-cross in `eqoxide-net`). `None` leaves the grid with NO region data
    /// (the honest "nobody attached any", not a load failure). Test-only.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn test_ready_with_water(water: Option<Arc<eqoxide_core::region_map::RegionMap>>) -> Self {
        let mut col = Self::fixture_grid();
        col.set_water(water);
        Self::ready("testfixture", 1, Arc::new(col))
    }

    /// [`Self::test_ready`], with the zone's region data **or the reason it failed to load**
    /// installed on the grid (#803). The case this adds over `test_ready_with_water` is
    /// `Err(RegionLoadError::…)`: a zone whose terrain GLB loaded fine — so the state genuinely IS
    /// `Ready` — and whose `.wtr` did not. That combination is exactly the one the
    /// `zone_assets_not_ready` gate does not cover, and the one `/v1/observe/zone_exits` used to
    /// answer `[]` with 200 OK for. Test-only.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn test_ready_with_region_data(
        data: Result<Arc<eqoxide_core::region_map::RegionMap>,
                     eqoxide_core::region_map::RegionLoadError>,
    ) -> Self {
        let mut col = Self::fixture_grid();
        col.set_region_data(data);
        Self::ready("testfixture", 1, Arc::new(col))
    }

    /// The trivial 200×200 flat floor both `test_ready_*` fixtures build on — a grid that really
    /// does have geometry, so `ready()` will not downgrade it.
    #[cfg(any(test, feature = "test-fixtures"))]
    fn fixture_grid() -> Collision {
        use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};
        let mesh = MeshData {
            positions: vec![
                [-100.0, 0.0, -100.0], [100.0, 0.0, -100.0],
                [100.0, 0.0, 100.0],   [-100.0, 0.0, 100.0],
            ],
            normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        Collision::build(&ZoneAssets { terrain: vec![mesh], objects: vec![], textures: vec![] }, 32.0)
    }

    /// The live progress line while `Pending`, or the failure reason while `Failed`.
    pub fn status(&self) -> Option<&str> {
        match self {
            Self::Pending { status, .. } => Some(status),
            Self::Failed { reason, .. }  => Some(reason),
            _ => None,
        }
    }
}

/// Why the loaded assets may not be used to describe the world the character is standing in.
/// `None` from [`usability`] means they may. Every variant's `as_str` is the machine-readable
/// `reason` an agent reads off the refusal / off `/v1/observe/debug`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotUsable {
    /// No zone has been loaded and none is loading.
    Idle,
    /// A load is in flight.
    Pending,
    /// The load ended without a usable world. Terminal.
    Failed,
    /// **The loaded world is a DIFFERENT zone than the one the character is in.**
    ///
    /// This is a real, reproducible window, not a theoretical one (#595 review F1): `player.zone`
    /// is published by the NETWORK thread the moment `OP_NewZone` lands, while the render thread
    /// only runs [`begin_zone_load`] on its next frame. In between, the client is in zone B while
    /// the assets — and the collision grid, and the uploaded meshes — are still zone A's, fully
    /// `Ready`. Reporting `ready` there is worse than reporting nothing: it actively vouches for a
    /// confident answer about the WRONG WORLD (exit lists and frames of the zone you just left).
    StaleForPreviousZone,
    /// The client does not know which zone the character is in (pre-zone-in, or a zone-in that
    /// timed out — see `PlayerState::zone_in_failed`), so no assets can be matched to it.
    PlayerZoneUnknown,
}

impl NotUsable {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle                 => "zone_assets_idle",
            Self::Pending              => "zone_assets_pending",
            Self::Failed               => "zone_assets_failed",
            Self::StaleForPreviousZone => "zone_assets_stale_for_previous_zone",
            Self::PlayerZoneUnknown    => "player_zone_unknown",
        }
    }

    /// The observable `state` word this verdict produces. Deliberately NOT the raw state tag:
    /// `ready` must never appear for assets that cannot describe the world the character is in.
    pub fn state_word(self) -> &'static str {
        match self {
            Self::Idle                 => "idle",
            Self::Pending              => "pending",
            Self::Failed               => "failed",
            Self::StaleForPreviousZone => "stale",
            Self::PlayerZoneUnknown    => "unknown_zone",
        }
    }

    /// What this verdict means for anything the client says about the world right now.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Idle => "no zone has been loaded in this client yet, and no load is running. \
                           Anything reported about zone geometry, collision or navigability is \
                           about NOTHING — do not read it as an empty world.",
            Self::Pending => "the zone's terrain GLB and collision grid are STILL LOADING. The \
                           frame currently shows a placeholder ground plane and there is no \
                           collision, so a flat/empty view, an empty exit list, or an unobstructed \
                           path right now is an artefact of the load — NOT the real zone (#560). \
                           Poll until this reads `ready`.",
            Self::Failed => "the zone's assets FAILED to load and no retry is running. The client \
                           is showing a fallback ground plane with no collision. This is terminal \
                           for this zone — waiting for `ready` will hang. Nav and geometry answers \
                           here are unavailable, not empty.",
            Self::StaleForPreviousZone => "the assets that are loaded belong to a DIFFERENT zone \
                           than the one the character is in (`zone` vs `player_zone` below). The \
                           zone change has been received but this client has not started loading \
                           the new zone's assets yet, so any geometry, exit list or frame right now \
                           would describe the zone you just LEFT. Transient (one render frame); \
                           poll until `state` is `ready` and `zone` == `player_zone`.",
            Self::PlayerZoneUnknown => "this client does not know which zone the character is in \
                           (before the first zone-in, or a zone-in that timed out — see \
                           `player.zone_in_failed`), so the loaded assets cannot be matched to it. \
                           Nothing about the world can be answered honestly here.",
        }
    }
}

/// **The one decision function for whether to answer about the world.** May the loaded assets be
/// used to answer a ROUTE or GEOMETRY question about the world the character is standing in? `None` =
/// yes; `Some(reason)` = no, and here is the machine-readable why.
///
/// It is pure and takes the player's zone explicitly, so the zone-identity check can never be
/// forgotten by a caller that only had the state handy — and so the universal claim ("a `ready`
/// observation is never about a zone you are not in") is a property test, not a live run.
///
/// **Which consumers actually go through it (verified by grep, not asserted — #600; recounted in
/// the #821 review round 2, B2, which found this list one short).** **FOUR**, and they are the four
/// that make a world-answering claim (a route/geometry answer or an agent-observable `nav_state`)
/// off the shared collision grid:
///   * the HTTP world-observation endpoints — `/observe/frame`, `/observe/zone_exits`,
///     `/observe/zone_entrances`, `/observe/debug`'s zone block, etc. — via `zone_assets_not_ready`
///     (`crates/eqoxide-http/src/observe.rs`), which early-returns a 503 before any collision read;
///   * `POST /v1/move/goto` (`crates/eqoxide-http/src/move_api.rs`, #579), which accepts the goal
///     but reports `zone_assets_pending` so `"status": "navigating"` is not read as "a walkable
///     route was found". **This one is NOT an `/observe/*` route**, which is exactly how it went
///     uncounted here for four issues;
///   * the nav path-walker's `drive_walk` gate (`crate::walker`, #600), which refuses to route in the
///     stale/loading window instead of steering on the previous zone's grid; and
///   * `ActionLoop::drain_zone_cross` (`crates/eqoxide-net/src/action_loop.rs`, #600 review round 2),
///     which resolves `/v1/move/zone_cross` off the grid and publishes `nav_state` — it answers the
///     honest transient `zone_loading` while not usable instead of a definitive `no_path`.
///     A `None` here therefore guarantees, for ALL FOUR, that the collision grid they go on to read is
///     the current zone's. What does NOT go through it, deliberately (each verified to make no
///     route/geometry claim about the current zone):
///   * the two per-zone DIAGNOSTIC COUNTERS `nav_support` and `nav_tight` read `shared_collision`
///     directly, but publish only cumulative metadata (facing-blind surfaces admitted /
///     minimum-clearance routes planned since zone load) and ride the SAME `/observe/debug` response
///     as the honest `zone_assets`
///     verdict beside them — never a route/geometry answer; and
///   * the other `action_loop` collision reads (combat line-of-sight, swim probing, the physical
///     auto-cross that fires from the character's real position) drive PHYSICAL movement/crossing,
///     server-authoritative — not an observable route/geometry claim an agent reads back.
pub fn usability(state: &ZoneAssetState, player_zone: &str) -> Option<NotUsable> {
    let loaded = match state {
        ZoneAssetState::Idle       => return Some(NotUsable::Idle),
        ZoneAssetState::Pending {..} => return Some(NotUsable::Pending),
        ZoneAssetState::Failed {..}  => return Some(NotUsable::Failed),
        ZoneAssetState::Ready { zone, .. } => zone.as_str(),
    };
    if player_zone.is_empty() { return Some(NotUsable::PlayerZoneUnknown); }
    // Zone short-names are ASCII and case-insensitive on the wire; compare accordingly rather than
    // letting a case difference read as "a different zone".
    //
    // This is LOOSER than `app::zone_needs_reload`, which decides when to start a reload and
    // compares the same two names EXACTLY. That direction is the safe one and is deliberate: the
    // reload trigger being at least as eager as this bless test is what makes "not reloading"
    // imply "the loaded grid is the zone the character is in". Do not equalise the two by making
    // the reload trigger case-insensitive — see the reasoning written up at `zone_needs_reload`
    // (#826).
    if !loaded.eq_ignore_ascii_case(player_zone) { return Some(NotUsable::StaleForPreviousZone); }
    None
}

/// [`usability`] **and the grid it blesses, in one call** — the verdict and the collision it vouches
/// for can never come from two different places (#821 review round 2, B4).
///
/// The bug this closes: a reader used to ask `usability` (or `zone_assets_not_ready`) for permission
/// and then fetch the grid from [`crate::collision::SharedCollision`], a *separate* slot. Nothing —
/// not a type, not a test — coupled them. `/v1/observe/zone_exits` guarded that slot with a bare
/// `if let Some(col)` and no `else`, so a `None` there produced `200 []`, i.e. "this zone has no way
/// out", **having consulted no region map at all**. Production writes both slots together
/// ([`finish_zone_load`] literally stores `verdict.collision().cloned()`, the same `Arc`), but that
/// is a convention, and the HTTP testkit already violated it. With this, the `None` case is not
/// reachable to write: `Ready` OWNS its `Arc<Collision>`, so a blessed verdict comes with a grid.
///
/// Delegates to [`usability`] rather than re-deriving the rule, so the two can never disagree about
/// staleness, zone-name case, or an empty `player_zone`.
pub fn usable_collision<'a>(state: &'a ZoneAssetState, player_zone: &str)
    -> Result<&'a Arc<Collision>, NotUsable>
{
    if let Some(why) = usability(state, player_zone) { return Err(why); }
    // `usability` returns `None` for `Ready` and for nothing else, and `Ready`'s `collision` field is
    // NOT an `Option` — so this fallback is unreachable. It is spelled as a REFUSAL rather than an
    // `unwrap`/`expect` because the honest answer to "we cannot find the grid" is never an empty
    // world; `usable_collision_agrees_with_usability_for_every_state` asserts it does not fire for
    // the five states that test enumerates by hand — not for all states, which no runtime test can
    // reach.
    //
    // THIS is the sentence a new state variant falsifies, which is why `ZoneAssetState::collision`
    // is matched exhaustively (#826): the wildcard it used to have let a variant be blessed by
    // `usability` and answered `None` here, firing this "unreachable" arm over a live grid. The
    // exhaustive arms do NOT keep the sentence true — `Self::New {..} => None` alongside a `usability`
    // that blesses `New` compiles, and falsifies it again. What they buy is that the author of a new
    // variant is made to read this, in `collision`, because the compiler will not let them skip it.
    state.collision().ok_or(NotUsable::Idle)
}

/// Lock a [`ZoneAssetStateShared`], **recovering from poisoning**.
///
/// A panic in the zone-asset loader while it holds this lock must not turn every later read into a
/// panic of its own: the HTTP thread answering `/v1/observe/debug` would then die on the `unwrap`
/// and the agent would get a connection error in place of the honest `failed` this whole type
/// exists to deliver. The state behind a poisoned lock is a plain enum with no broken invariant to
/// protect, so reading it through is safe.
pub fn lock_state(shared: &ZoneAssetStateShared) -> std::sync::MutexGuard<'_, ZoneAssetState> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Begin loading `zone`: drop the world model everything else reads AND publish `Pending` for the
/// new zone, in one call.
///
/// These two writes are coupled on purpose (#579). Clearing [`SharedCollision`] without moving the
/// observable state off `Ready` is exactly the stale-ready lie — the client would be standing in a
/// brand-new, collision-less zone while still reporting the PREVIOUS zone's geometry as loaded. Use
/// this rather than clearing the collision slot by hand.
pub fn begin_zone_load(
    collision_slot: &crate::collision::SharedCollision,
    state: &ZoneAssetStateShared,
    zone: &str,
    status: &str,
) {
    *collision_slot.write().unwrap() = None;
    *lock_state(state) = ZoneAssetState::pending(zone, status);
}

/// Commit a finished zone load: publish the collision grid and the observable verdict together.
///
/// The verdict is DERIVED from what the load actually produced, so it cannot disagree with the
/// collision slot this same call writes: a grid plus terrain meshes ⇒ `Ready` (carrying that grid);
/// anything else ⇒ `Failed` with the loader's reason — never a silent "pending forever".
pub fn finish_zone_load(
    collision_slot: &crate::collision::SharedCollision,
    state: &ZoneAssetStateShared,
    zone: &str,
    collision: Option<Arc<Collision>>,
    terrain_meshes: usize,
    load_error: Option<&str>,
) {
    let verdict = match &collision {
        Some(col) => ZoneAssetState::ready(zone, terrain_meshes, col.clone()),
        None => ZoneAssetState::failed(zone, &format!(
            "the zone's assets did not load — the client is showing a fallback ground plane with \
             NO collision. Geometry and nav answers here are UNAVAILABLE, not empty{}",
            load_error.map(|e| format!(": {e}")).unwrap_or_default())),
    };
    // A load that did not produce a usable world must not leave a collision grid behind for
    // readers to answer from — `Failed` and "here is your collision" cannot both be true.
    *collision_slot.write().unwrap() = verdict.collision().cloned();
    *lock_state(state) = verdict;
}

impl std::fmt::Debug for ZoneAssetState {
    // Hand-written: `Collision` is a huge triangle grid and must never be formatted.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "ZoneAssetState::Idle"),
            Self::Pending { zone, status } => write!(f, "ZoneAssetState::Pending({zone}: {status})"),
            Self::Ready { zone, terrain_meshes, .. } =>
                write!(f, "ZoneAssetState::Ready({zone}: {terrain_meshes} meshes + collision)"),
            Self::Failed { zone, reason } => write!(f, "ZoneAssetState::Failed({zone}: {reason})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collision::Collision;
    use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};

    #[test]
    fn test_ready_fixture_is_a_real_ready() {
        let st = ZoneAssetState::test_ready();
        assert!(st.is_ready() && st.collision().is_some());
    }

    /// A flat 200×200 floor — a collision grid that genuinely has geometry. (Mesh positions are
    /// GLB-space `[east, up, north]`, matching the planner's own fixtures.)
    fn floor_collision() -> Arc<Collision> {
        let mesh = MeshData {
            positions: vec![
                [-100.0, 0.0, -100.0], [100.0, 0.0, -100.0],
                [100.0, 0.0, 100.0],   [-100.0, 0.0, 100.0],
            ],
            normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        Arc::new(Collision::build(&ZoneAssets { terrain: vec![mesh], objects: vec![], textures: vec![] }, 32.0))
    }

    /// A collision grid built from nothing — `has_geometry()` is false.
    fn empty_collision() -> Arc<Collision> {
        Arc::new(Collision::build(&ZoneAssets { terrain: vec![], objects: vec![], textures: vec![] }, 32.0))
    }

    #[test]
    fn ready_carries_the_collision_it_claims() {
        let st = ZoneAssetState::ready("freportw", 412, floor_collision());
        assert!(st.is_ready());
        assert_eq!(st.tag(), "ready");
        assert_eq!(st.zone(), Some("freportw"));
        assert!(st.collision().is_some(), "a Ready that cannot produce its collision is the #579 lie");
    }

    /// Tier 1 (make the bad state unrepresentable): "ready" with no collision geometry must be
    /// impossible to publish. The constructor downgrades it to an explicit `Failed`.
    #[test]
    fn ready_without_collision_geometry_is_not_representable() {
        let st = ZoneAssetState::ready("freportw", 412, empty_collision());
        assert!(!st.is_ready(), "a collision grid with no geometry must NEVER read as ready");
        assert_eq!(st.tag(), "failed");
        assert!(st.status().unwrap().contains("NO geometry"));
    }

    /// Same rule for terrain: a load that produced no meshes is not a ready (empty) world.
    #[test]
    fn ready_without_terrain_meshes_is_not_representable() {
        let st = ZoneAssetState::ready("freportw", 0, floor_collision());
        assert!(!st.is_ready());
        assert_eq!(st.tag(), "failed");
        assert!(st.status().unwrap().contains("ZERO terrain meshes"));
    }

    /// Documents the coupling `has_triangles` is stated independently of: `Collision::build`
    /// currently forces `cols == 0` whenever there are no triangles, so the strict predicate and the
    /// bounds proxy agree. If a future `build` breaks that, `ready()` is already on the strict one —
    /// and this test says so out loud rather than leaving the equivalence as folklore.
    #[test]
    fn has_triangles_and_has_geometry_agree_for_everything_build_produces() {
        for col in [floor_collision(), empty_collision()] {
            assert_eq!(col.has_geometry(), col.has_triangles(),
                "build() is expected to keep these in step; ready() uses the strict one regardless");
        }
    }

    /// `Failed` must be distinguishable from `Pending` — a permanent failure reported as "pending
    /// forever" would make an agent wait for something that is never coming.
    #[test]
    fn failed_is_distinct_from_pending_and_from_ready() {
        let p = ZoneAssetState::pending("freportw", "Downloading zone 1/7 (12.4 MB)…");
        let f = ZoneAssetState::failed("freportw", "asset server unreachable");
        assert_eq!(p.tag(), "pending");
        assert_eq!(f.tag(), "failed");
        assert!(!p.is_ready() && !f.is_ready());
        assert_ne!(p.detail(), f.detail());
        assert_eq!(p.status(), Some("Downloading zone 1/7 (12.4 MB)…"));
        assert_eq!(f.status(), Some("asset server unreachable"));
    }

    fn slots() -> (crate::collision::SharedCollision, ZoneAssetStateShared) {
        (Arc::new(std::sync::RwLock::new(None)),
         Arc::new(std::sync::Mutex::new(ZoneAssetState::Idle)))
    }

    /// The #579 core invariant: a zone change must never leave the observable state `Ready` from
    /// the PREVIOUS zone while the client stands in a new, collision-less one.
    #[test]
    fn begin_zone_load_clears_collision_and_goes_pending() {
        let (col, st) = slots();
        finish_zone_load(&col, &st, "qeynos", Some(floor_collision()), 7, None);
        assert!(st.lock().unwrap().is_ready() && col.read().unwrap().is_some());

        begin_zone_load(&col, &st, "freportw", "Zone change — starting asset load…");
        assert_eq!(st.lock().unwrap().tag(), "pending", "stale-ready across a zone change is the #579 lie");
        assert_eq!(st.lock().unwrap().zone(), Some("freportw"));
        assert!(col.read().unwrap().is_none(), "the previous zone's collision must be dropped");
    }

    /// Repeated zone changes: the state is pending→ready for EACH zone, and never reports the
    /// zone it is not in. (Property over a sequence — the "always clears" claim, not one example.)
    #[test]
    fn every_zone_change_goes_pending_then_ready_for_that_zone() {
        let (col, st) = slots();
        for zone in ["qeynos", "freportw", "gfaydark", "qeynos"] {
            begin_zone_load(&col, &st, zone, "loading…");
            let s = st.lock().unwrap().clone();
            assert!(!s.is_ready(), "{zone}: must be pending while loading");
            assert_eq!(s.zone(), Some(zone));
            assert!(col.read().unwrap().is_none());

            finish_zone_load(&col, &st, zone, Some(floor_collision()), 3, None);
            let s = st.lock().unwrap().clone();
            assert!(s.is_ready(), "{zone}: must be ready once terrain + collision exist");
            assert_eq!(s.zone(), Some(zone));
            assert!(col.read().unwrap().is_some());
        }
    }

    /// A load that produced nothing must land on `Failed` (with the loader's reason), not on a
    /// permanent `Pending` an agent would wait out forever — and must leave NO collision behind.
    #[test]
    fn a_failed_load_is_terminal_and_leaves_no_collision() {
        let (col, st) = slots();
        begin_zone_load(&col, &st, "freportw", "loading…");
        finish_zone_load(&col, &st, "freportw", None, 0, Some("asset server unreachable"));
        let s = st.lock().unwrap().clone();
        assert_eq!(s.tag(), "failed");
        assert!(s.status().unwrap().contains("asset server unreachable"));
        assert!(col.read().unwrap().is_none());
    }

    /// A collision grid that carries no geometry cannot sneak into the collision slot behind a
    /// `Ready` — the verdict and the slot are derived from the same value, so they cannot drift.
    #[test]
    fn finish_never_publishes_collision_for_a_non_ready_verdict() {
        let (col, st) = slots();
        finish_zone_load(&col, &st, "voidzone", Some(empty_collision()), 5, None);
        assert_eq!(st.lock().unwrap().tag(), "failed");
        assert!(col.read().unwrap().is_none(),
            "a Failed verdict must not leave a collision grid for nav to answer from");
    }

    // ─────────── the zone-identity rule (#595 review F1) ───────────
    //
    // `docs/http-api.md` claims a `ready` observation is NEVER about a zone you are not in. That is
    // a universal, so per the verification hierarchy it needs a PROPERTY test — a live run is an
    // existence proof over one trajectory and cannot discharge a "never". These exercise the
    // decision function `usability` at the PREDICATE level; the nav walker — the other consumer that
    // goes through it (#600) — is exercised at the CONSUMER level by
    // `walker::tests::walker_never_routes_on_a_collision_grid_whose_zone_is_not_the_players`.

    /// EXHAUSTIVE over the cross product of every state shape × every player-zone value:
    /// `usability` returns `None` (= may describe the world) **if and only if** the state is `Ready`
    /// AND its zone equals the player's non-empty zone. No ordering, no timing, no exceptions.
    #[test]
    fn usable_iff_ready_for_the_zone_the_player_is_actually_in() {
        let zones = ["qeynos", "freporte", "FREPORTE", "gfaydark", ""];
        let states: Vec<(&str, ZoneAssetState)> = vec![
            ("idle",    ZoneAssetState::Idle),
            ("pendA",   ZoneAssetState::pending("qeynos", "loading…")),
            ("pendB",   ZoneAssetState::pending("freporte", "loading…")),
            ("failA",   ZoneAssetState::failed("qeynos", "boom")),
            ("readyA",  ZoneAssetState::ready("qeynos", 3, floor_collision())),
            ("readyB",  ZoneAssetState::ready("freporte", 3, floor_collision())),
        ];
        for (name, st) in &states {
            for pz in zones {
                let usable = usability(st, pz).is_none();
                let expected = matches!(st, ZoneAssetState::Ready { zone, .. }
                    if !pz.is_empty() && zone.eq_ignore_ascii_case(pz));
                assert_eq!(usable, expected,
                    "state {name} with player_zone {pz:?}: usable={usable}, expected={expected}");
            }
        }
    }

    /// [`usable_collision`] must agree with [`usability`] on EVERY state × zone combination, and
    /// must hand back a grid whenever it agrees — never the unreachable `Idle` fallback.
    ///
    /// This is what makes the `if let Some(col)` fall-through in `/v1/observe/zone_exits`
    /// unrepresentable rather than merely documented (#821 review round 2, B4): "the verdict says
    /// yes" and "here is a grid" are now the same value, so there is no `None` branch left for a
    /// caller to answer `[]` from.
    ///
    /// **Still load-bearing after #826, and not redundant with it.** #826 removed the `_` wildcard
    /// from `ZoneAssetState::collision`, so a new state variant now reds `collision` and `usability`
    /// *together* — that stops one function from silently ignoring a variant the other classified.
    /// It does not stop the two from being filled in INCONSISTENTLY (`None` here, "usable" there),
    /// which compiles fine and re-opens the same hole. This test is what rejects that, for the
    /// combinations it enumerates.
    #[test]
    fn usable_collision_agrees_with_usability_for_every_state() {
        let zones = ["qeynos", "freporte", "FREPORTE", "gfaydark", ""];
        let states: Vec<(&str, ZoneAssetState)> = vec![
            ("idle",   ZoneAssetState::Idle),
            ("pendA",  ZoneAssetState::pending("qeynos", "loading…")),
            ("failA",  ZoneAssetState::failed("qeynos", "boom")),
            ("readyA", ZoneAssetState::ready("qeynos", 3, floor_collision())),
            ("readyB", ZoneAssetState::ready("freporte", 3, floor_collision())),
        ];
        // Compile-time roll call. `states` above is written by HAND, so this test can silently
        // under-cover: measured on the #826 probe branch, a FIFTH variant (the enum has four) that
        // turned a live grid into `Err(Idle)` left this test GREEN, because the vec did not know
        // the variant existed.
        // The match below has no wildcard, so adding a variant to `ZoneAssetState` now reds THIS
        // FILE too, next to the arms in `collision`/`usability`.
        //
        // What it proves: nobody can add a state variant without the compiler pointing at this
        // test. What it does NOT prove: that `states` actually contains one of every variant — the
        // author still has to add the row. It is a forced read, not a coverage proof.
        for (_, st) in &states {
            match st {
                ZoneAssetState::Idle
                | ZoneAssetState::Pending {..}
                | ZoneAssetState::Ready {..}
                | ZoneAssetState::Failed {..} => {}
            }
        }
        for (name, st) in &states {
            for pz in zones {
                match (usability(st, pz), usable_collision(st, pz)) {
                    (Some(why), Err(e)) => assert_eq!(
                        why, e, "{name}/{pz:?}: the two must give the SAME refusal"),
                    (None, Ok(col)) => assert!(
                        std::sync::Arc::ptr_eq(col, st.collision().unwrap()),
                        "{name}/{pz:?}: must hand back the state's OWN grid, not a copy"),
                    (u, c) => panic!(
                        "{name}/{pz:?}: disagreement — usability={u:?}, usable_collision ok={}",
                        c.is_ok()),
                }
                // …and the unreachable fallback never fires: a refusal is never `Idle` unless
                // `usability` genuinely said `Idle`.
                if let Err(NotUsable::Idle) = usable_collision(st, pz) {
                    assert_eq!(usability(st, pz), Some(NotUsable::Idle),
                        "{name}/{pz:?}: `Idle` came from the unreachable fallback, not the verdict");
                }
            }
        }
    }

    // ─────────── the #826 lexical guard on `collision`'s arms ───────────

    /// The anchor, assembled at COMPILE time from two pieces so the assembled literal does not
    /// itself appear anywhere in this file's text — which is what lets the uniqueness check below
    /// be a real reach control rather than a self-match.
    const COLLISION_SIG: &str =
        concat!("pub fn ", "collision(&self) -> Option<&Arc<Collision>> {");
    /// The variants `collision` must name. Adding a variant to `ZoneAssetState` must red this list
    /// too — that is deliberate, and it is the same forced read as the roll call above.
    const COLLISION_VARIANTS: [&str; 4] =
        ["Self::Idle", "Self::Pending", "Self::Ready", "Self::Failed"];

    /// Remove `//` line comments and `/* … */` block comments **without touching either sequence
    /// when it appears inside a string or char literal**.
    ///
    /// The naive `line.split("//").next()` is unsound in this file: a string containing `//` (a URL,
    /// say) would be cut mid-literal, the closing quote would vanish, and every later quote in the
    /// file would be mis-paired — which silently moves the anchor and the walk. So this tracks
    /// literal state:
    ///
    /// * strings, with `\` escapes (including the `\`-at-end-of-line continuations this module's
    ///   long `detail()` strings use, and `\"`);
    /// * char literals, which must be distinguished from LIFETIMES — `'a` is not an opener, while
    ///   `'"'` and `'\\'` are exactly the two shapes that would otherwise invert string state. Both
    ///   occur in this very module, in [`audit_collision_arms`]. A `'` is treated as a literal only
    ///   when it is followed by an escape, or by one character and then a closing `'`;
    /// * block comments, which nest in Rust, so a depth counter rather than a flag.
    ///
    /// Newlines are preserved so line structure survives.
    ///
    /// It does NOT understand raw strings (`r"…"`), byte strings, or an unterminated block comment.
    /// A raw string is a REFUSAL in [`audit_collision_arms`]; an unterminated block comment eats the
    /// rest of the file, which trips that function's anchor reach control. Neither is guessed at.
    fn strip_comments(src: &str) -> String {
        let b: Vec<char> = src.chars().collect();
        let mut out = String::with_capacity(src.len());
        let mut i = 0usize;
        let mut depth = 0usize; // block-comment nesting
        while i < b.len() {
            let c = b[i];
            if depth > 0 {
                if c == '/' && b.get(i + 1) == Some(&'*') { depth += 1; i += 2; continue; }
                if c == '*' && b.get(i + 1) == Some(&'/') { depth -= 1; i += 2; continue; }
                if c == '\n' { out.push('\n'); }
                i += 1;
                continue;
            }
            match c {
                '/' if b.get(i + 1) == Some(&'/') => {
                    while i < b.len() && b[i] != '\n' { i += 1; }
                }
                '/' if b.get(i + 1) == Some(&'*') => { depth = 1; i += 2; }
                '"' => {
                    out.push(c);
                    i += 1;
                    while i < b.len() {
                        out.push(b[i]);
                        if b[i] == '\\' {
                            if let Some(&e) = b.get(i + 1) { out.push(e); }
                            i += 2;
                            continue;
                        }
                        if b[i] == '"' { i += 1; break; }
                        i += 1;
                    }
                }
                '\'' if b.get(i + 1) == Some(&'\\') => {
                    // Escaped char literal. The escaped character is copied BEFORE the search for
                    // the terminator starts, so `'\''` closes on its fourth character and not on
                    // its escaped third one. `'\u{41}'` then closes on the following `'`.
                    out.push(c);
                    i += 1;
                    out.push(b[i]);
                    i += 1;
                    if i < b.len() { out.push(b[i]); i += 1; }
                    while i < b.len() {
                        let ch = b[i];
                        out.push(ch);
                        i += 1;
                        if ch == '\'' { break; }
                    }
                }
                '\'' if b.get(i + 2) == Some(&'\'') => {
                    // Simple char literal `'x'`. Anything else starting with `'` is a lifetime.
                    out.push(c);
                    out.push(b[i + 1]);
                    out.push('\'');
                    i += 3;
                }
                _ => { out.push(c); i += 1; }
            }
        }
        out
    }

    /// True if `src` contains a raw-string opener (`r`, `#`s, `"`) not preceded by an identifier
    /// character.
    ///
    /// Deliberately CONSERVATIVE: it does not track string state, so an ordinary string literal
    /// whose last character is a standalone `r` (the text `"r"`) also reports true. That direction
    /// of error is a refusal, which is loud; the other direction would be a silent mis-scan.
    fn has_raw_string(src: &str) -> bool {
        let b = src.as_bytes();
        for (i, w) in b.iter().enumerate() {
            if *w != b'r' { continue; }
            if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') { continue; }
            let mut j = i + 1;
            while j < b.len() && b[j] == b'#' { j += 1; }
            if j < b.len() && b[j] == b'"' && j > i { return true; }
        }
        false
    }

    /// Split `text` at occurrences of `sep` that sit at brace/paren/bracket depth **zero**.
    ///
    /// This is what makes the scan arm-wise instead of line-wise. Splitting arms on a plain `,`
    /// would cut `Self::Ready { collision, .. }` in half; splitting or-patterns on a plain `|`
    /// would cut inside a nested pattern. Depth is the only thing separating the two cases, and
    /// this function is deliberately the ONLY place that knowledge lives.
    fn split_top_level(text: &str, sep: char) -> Vec<&str> {
        let mut out = Vec::new();
        let mut depth = 0i32;
        let mut start = 0usize;
        for (i, c) in text.char_indices() {
            match c {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth -= 1,
                c if c == sep && depth == 0 => {
                    out.push(&text[start..i]);
                    start = i + c.len_utf8();
                }
                _ => {}
            }
        }
        out.push(&text[start..]);
        out
    }

    /// True if `pat` contains a `_` that stands as a token on its own at brace/paren/bracket depth
    /// **zero** — i.e. a wildcard *binding*, not a `_` used as a field pattern.
    ///
    /// The depth condition is the whole point: `Self::New { grid: _, .. }` is a legitimate arm that
    /// still names its variant, and its `_` sits at depth one. `Self::Failed {..} | _` is a wildcard
    /// arm wearing a named arm's clothes, and its `_` sits at depth zero. The adjacency condition
    /// keeps `_unused` and `some_name` from reading as wildcards.
    fn has_top_level_wildcard_token(pat: &str) -> bool {
        let b: Vec<char> = pat.chars().collect();
        let mut depth = 0i32;
        for i in 0..b.len() {
            match b[i] {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth -= 1,
                '_' if depth == 0 => {
                    let free_before = i == 0 || !(b[i - 1].is_alphanumeric() || b[i - 1] == '_');
                    let free_after =
                        i + 1 >= b.len() || !(b[i + 1].is_alphanumeric() || b[i + 1] == '_');
                    if free_before && free_after {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// **The whole #826 guard, as a pure function over source text, so its evasions are ordinary
    /// test data instead of one-off source mutations** (see
    /// `collision_arm_audit_rejects_the_evasions_measured_against_it`).
    ///
    /// `Ok(())` means: in `src`, every arm of `ZoneAssetState::collision`'s `match` is a `Self::…`
    /// pattern. `Err` carries the reason, and the reasons are deliberately split so a failure names
    /// the cause that actually occurred.
    fn audit_collision_arms(src: &str) -> Result<(), String> {
        // Comments are removed FIRST, and everything below runs on the stripped text. Round 2 of the
        // #837 review defeated the previous version of this guard twice by putting text in comments:
        // `_ => None,  // Self::Failed` satisfied the arm check, and a `}}` in a trailing comment
        // truncated the brace walk past the reach control. Both were GREEN with a live wildcard.
        let src = strip_comments(src);

        // The one construct this scan cannot lex is a REFUSAL, not a guess: the guarantee this
        // function offers is "either the text was something it can lex, or you got an Err" — never
        // "it looked fine". A raw string can hold an unescaped quote, which would invert the
        // stripper's string state and move both the anchor and the walk.
        if has_raw_string(&src) {
            return Err("this guard does not lex raw strings, and one is present. Extend \
                        `strip_comments` or move it (#826/#837).".to_string());
        }

        // Anchor. REACH CONTROL: exactly one occurrence. 0 means the scan would examine NOTHING —
        // the #778 failure mode, where a guard silently covers none of what it claims. This is also
        // the ONLY backstop against an OVER-stripping `strip_comments`: the stripper's own controls
        // in the caller detect a no-op and nothing else, so a stripper that ate its whole input
        // would satisfy them and arrive here with the anchor gone (measured, #837 review round 3).
        let hits = src.matches(COLLISION_SIG).count();
        if hits != 1 {
            return Err(format!(
                "reach control: expected exactly ONE `{COLLISION_SIG}`, found {hits}. 0 means this \
                 scan would have examined nothing — either the signature was reformatted or \
                 renamed, or `strip_comments` OVER-stripped and removed it (that is the one \
                 failure the stripper's own reach controls cannot see). Fix the anchor or the \
                 stripper; do not delete this test. >1 means the anchor is ambiguous."));
        }
        let start = src.find(COLLISION_SIG).unwrap() + COLLISION_SIG.len();

        // Brace walk from just inside the body.
        let mut depth = 1usize;
        let mut end = None;
        for (i, c) in src[start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => { depth -= 1; if depth == 0 { end = Some(start + i); break; } }
                _ => {}
            }
        }
        let body = match end {
            Some(e) => &src[start..e],
            None => return Err("`collision`'s body has no closing brace after comment \
                                stripping".to_string()),
        };

        // The one remaining way to truncate the walk is a `{`/`}` inside a string or char literal in
        // the BODY. `collision` has neither, so refuse rather than mis-scan.
        if body.contains('"') || body.contains('\'') {
            return Err(format!(
                "this guard cannot walk a `collision` body containing string or char literals — a \
                 brace inside one would truncate the scan. Body was:\n{body}"));
        }

        // ── ARM-WISE, NOT LINE-WISE (#837 review round 4) ────────────────────────────────────
        // The previous version of this scan collected whole LINES containing `=>` and asked each
        // for a `Self::` substring. The compiler reasons about ARMS, and three shapes were measured
        // ACCEPTED by that scan on this tree — each one a wildcard riding in on a NAMED arm's own
        // line, so it inherited that line's `Self::` token:
        //
        //     Self::Failed {..} => None, #[allow(unreachable_patterns)] _ => None,
        //     Self::Failed {..} | _ => None,
        //     Self::Failed {..} => None, _ => None,
        //
        // The first and third also left the arm-LINE count at exactly 4, so the equality below did
        // not fire either — both checks evaded at once. All three were carried through to the real
        // property: with a fifth variant on `ZoneAssetState`, each one removed `collision`'s match
        // from rustc's E0004 list (control with the pristine arms: present) with ZERO
        // `unreachable_patterns` warnings. So everything below reasons about arms split at
        // top-level commas, and about the PATTERN half of each arm specifically.

        // Isolate the match's arm region: the first `{` in the body opens it.
        let open = match body.find('{') {
            Some(o) => o,
            None => return Err(format!(
                "`collision`'s body contains no `{{`, so there is no match for this guard to scan. \
                 Body was:\n{body}")),
        };
        if !body[..open].contains("match") {
            return Err(format!(
                "the first `{{` in `collision`'s body does not open a `match` — this guard scans a \
                 single `match self {{ … }}` and the body no longer has that shape. Update the \
                 guard deliberately; do not delete it. Body was:\n{body}"));
        }
        let mut match_depth = 1usize;
        let mut arms_end = None;
        for (i, c) in body[open + 1..].char_indices() {
            match c {
                '{' => match_depth += 1,
                '}' => {
                    match_depth -= 1;
                    if match_depth == 0 { arms_end = Some(open + 1 + i); break; }
                }
                _ => {}
            }
        }
        let arms_region = match arms_end {
            Some(e) => &body[open + 1..e],
            None => return Err(format!(
                "the match in `collision`'s body has no closing brace. Body was:\n{body}")),
        };

        let arms: Vec<&str> = split_top_level(arms_region, ',')
            .into_iter()
            .filter(|a| !a.trim().is_empty())
            .collect();

        // Per-ARM pattern checks FIRST, so the message names the cause that actually occurred.
        // (Round 2 of the #837 review, N1: with the variant check first, the realistic regression
        // shapes — a wildcard replacing or joining named arms — reported "the brace walk truncated,
        // or a variant was renamed", which is not what happened.)
        for arm in &arms {
            let pattern = match arm.split_once("=>") {
                Some((p, _)) => p,
                None => return Err(format!(
                    "`collision` has an arm-shaped fragment with no `=>`, so this guard cannot tell \
                     its pattern from its body:\n    {}\nFix the arm or update this guard \
                     deliberately.", arm.trim())),
            };
            if has_top_level_wildcard_token(pattern) {
                return Err(format!(
                    "`ZoneAssetState::collision` must match every variant BY NAME (#826). This \
                     arm's pattern contains a bare `_` wildcard at the top level:\n    {}\nA `_` \
                     here silently answers `None` for any state added later, while `usability` — \
                     which has no wildcard — is forced by the compiler to classify it. That is how \
                     a blessed, live collision grid turns into `Err(NotUsable::Idle)`. A `_` used \
                     as a FIELD pattern (`Self::New {{ grid: _, .. }}`) is nested and does not trip \
                     this.", arm.trim()));
            }
            for alt in split_top_level(pattern, '|') {
                if !alt.trim().starts_with("Self::") {
                    return Err(format!(
                        "`ZoneAssetState::collision` must match every variant BY NAME (#826). This \
                         arm has an alternative that does not start with `Self::`:\n    {}\n(the \
                         offending alternative: {:?})\nA bare binding or a wildcard here silently \
                         answers `None` for any state added later, while `usability` — which has \
                         no wildcard — is forced by the compiler to classify it. If this fired on \
                         a legitimate guard containing `|`, or on a pattern written some other \
                         way, update this guard deliberately.", arm.trim(), alt.trim()));
                }
            }
        }

        // CONSISTENCY CHECKS — deliberately NOT called reach controls. They are equalities, so
        // unlike the `>= 4` floor they replaced (#837 review round 2, N2) they do detect a walk
        // that reached four arms and stopped; but they are still counts over already-scanned text,
        // which is not the same thing as proving the scan reached the end of the match.
        //
        // The `=>` count is separate from the arm count on purpose: the arm split is comma-driven,
        // so a second `=>` smuggled into one comma-delimited chunk would be one arm by that count
        // and two by this one.
        let fat_arrows = arms_region.matches("=>").count();
        if fat_arrows != COLLISION_VARIANTS.len() {
            return Err(format!(
                "`collision`'s match contains {} `=>` but `ZoneAssetState` has {} variants. Either \
                 an arm is missing/duplicated/added, or the walk stopped early. Body was:\n{body}",
                fat_arrows, COLLISION_VARIANTS.len()));
        }
        if arms.len() != COLLISION_VARIANTS.len() {
            return Err(format!(
                "`collision` splits into {} top-level-comma-separated arm(s) but `ZoneAssetState` \
                 has {} variants. Either an arm is missing/duplicated, or the walk stopped early, \
                 or the arms are no longer comma-separated — a BLOCK-bodied arm \
                 (`Self::Idle => {{ None }}`) may legally omit its comma, which merges it with the \
                 next one. The `=>` count above still holds in that case, so this is a layout \
                 refusal and not a wildcard; update this guard deliberately. Body was:\n{body}",
                arms.len(), COLLISION_VARIANTS.len()));
        }

        // Every variant named. After the equality above this mostly catches a RENAME; it also
        // catches a walk truncated before a variant's arm.
        for variant in COLLISION_VARIANTS {
            if !body.contains(variant) {
                return Err(format!(
                    "`collision`'s scanned body is missing `{variant}` — a variant was renamed, or \
                     the walk truncated. Body was:\n{body}"));
            }
        }
        Ok(())
    }

    /// A **lexical** pin on one function body: every arm of `ZoneAssetState::collision`'s `match`
    /// must be a `Self::…` pattern — no `_ =>`, no bare binding.
    ///
    /// **Why a source-text scan at all, when this repo has seven measured ways one proves a call is
    /// *written* rather than *reached* (#799).** Those seven separate the text from the execution.
    /// This property has no execution to diverge from: "the body contains no wildcard arm" **is** a
    /// statement about text, and what it protects — E0004 firing when a variant is added — is
    /// decided by rustc from that same text. The #837 reviewer measured the point on the roll call
    /// above: wrapping it in `if false { … }` still produced E0004.
    ///
    /// **The gap that DOES exist here is not written-vs-reached but scanned-vs-compiled**, and it
    /// was exploited twice against the first version of this guard (#837 review round 2, probes
    /// M3/M5): `_ => None,  // Self::Failed` satisfied the arm check from inside a comment, and a
    /// `}}` in a trailing comment placed after the last variant name truncated the walk past the
    /// reach control. Both left a live wildcard with the pin GREEN and no compiler warning. The
    /// answer is [`audit_collision_arms`]: comments — line AND block — are stripped before anything
    /// is scanned, and the one construct the stripper cannot lex is a REFUSAL rather than a guess.
    ///
    /// **String literals ARE handled, not scoped around.** A `//`-strip that splits lines is itself
    /// unsound — `//` occurs inside string literals (a URL), and cutting there deletes the closing
    /// quote, inverts string state for everything after it, and silently moves both the anchor and
    /// the brace walk. So [`strip_comments`] is a literal-aware state machine covering strings with
    /// `\` escapes, char literals (distinguished from lifetimes: `'"'` and `'\\'` are the two shapes
    /// that would otherwise invert string state, and both occur a few lines above), and nested block
    /// comments. `collision_arm_audit_rejects_the_evasions_measured_against_it` asserts the URL case
    /// passes, which is what shows this is not a `split("//")`.
    ///
    /// **A third gap was measured in round 4, and it is why this scan is arm-wise.** The previous
    /// version collected whole LINES containing `=>` and asked each line for a `Self::` substring.
    /// The compiler reasons about ARMS. Three shapes were measured ACCEPTED by that scan —
    /// `Self::Failed {..} => None, #[allow(unreachable_patterns)] _ => None,`,
    /// `Self::Failed {..} | _ => None,` and `Self::Failed {..} => None, _ => None,` — each riding in
    /// on a named arm's own `Self::` token, and the first and third also holding the arm-LINE count
    /// at 4 so the equality did not fire either. All three were carried through to the real
    /// property: with a fifth variant on `ZoneAssetState`, each removed `collision`'s match from
    /// rustc's E0004 list, with ZERO `unreachable_patterns` warnings (control, pristine arms:
    /// present). They are now data in
    /// `collision_arm_audit_rejects_the_evasions_measured_against_it`.
    ///
    /// **What it proves**, over text [`audit_collision_arms`] can lex: `collision`'s match splits
    /// into exactly as many top-level-comma-separated arms as `ZoneAssetState` has variants, it
    /// contains exactly that many `=>`, every variant is named in it, and every arm's PATTERN has
    /// no bare `_` at top level and no or-pattern alternative that fails to begin with `Self::`.
    /// **Line layout does not enter into it** — that is the round-4 fix — so a wildcard sharing a
    /// named arm's line, appended as an or-pattern alternative, or hidden in a line or block
    /// comment (comments are stripped first) is caught. And if the text ever contains something
    /// this scan cannot lex (a raw string; a string or char literal inside `collision`'s own body,
    /// whose braces would truncate the walk), the test FAILS instead of passing over it. That last
    /// property is otherwise true of nothing in the merged tree: the E0004 evidence in #837 is not
    /// a test and CI cannot re-run it.
    ///
    /// **What it does NOT prove**, stated so nobody reads it as more: (a) it covers exactly ONE
    /// function body and says nothing about the other wildcards on this enum (`status` here,
    /// `terrain_meshes` in `eqoxide-http`, `lost_load_zone` in the binary crate — see #838);
    /// (b) it is lexical, so it cannot tell a correct arm from an incorrect one — an author who
    /// writes `Self::New {..} => None` for a variant carrying a live grid passes this test and
    /// re-opens #826; (c) it splits arms at top-level commas, and a block-bodied arm
    /// (`Self::Idle => { None }`) may legally omit its comma, so two arms can land in one chunk.
    /// That does not admit a wildcard — merging arms cannot reduce the number of `=>`, and the
    /// arrow-count equality is kept separate from the arm-count equality precisely to close it —
    /// but it does mean a legitimate block-bodied rewrite FAILS this test loudly rather than
    /// passing. **Nothing enforces the layout in CI either way:** `.github/workflows/test.yml` is
    /// this repo's only workflow and it runs neither `cargo fmt` nor `cargo clippy` (grepped, 0
    /// hits, #837 review round 4), so do not read "rustfmt writes one arm per line" as a gate —
    /// it is a habit, and this scan no longer depends on it; and (d) it is a scan, not a parse.
    /// (d) is bounded by refusal rather than by understanding: the unlexable constructs are
    /// enumerated and rejected, so the honest form of the guarantee is *"either the text was
    /// something this scan can lex, or the test failed"* — never *"it looked fine"*.
    ///
    /// **The stripper's own reach control is ONE-DIRECTIONAL.** The three assertions below detect a
    /// stripper that NO-OPS (canary still present, or nothing removed at all). They do not detect
    /// one that OVER-strips: a `strip_comments` returning `String::new()` satisfies all three
    /// (`SRC.contains` passes, `!stripped.contains` passes, `0 < SRC.len()` passes). What catches
    /// that is the anchor-uniqueness check inside [`audit_collision_arms`] — measured in #837
    /// review round 3: the mutation turned both guard tests RED, panicking inside the
    /// `audit_collision_arms(SRC)` call. So the anchor check is the backstop for over-stripping,
    /// and its message names that cause alongside the others.
    #[test]
    fn collision_matches_only_named_variants_no_wildcard_arm() {
        const SRC: &str = include_str!("zone_assets.rs");
        // Each canary is assembled from two pieces, so the joined literal occurs in this file
        // exactly once — in its own comment below — and never in code. That is what makes the
        // `!stripped.contains(..)` assertions below evidence rather than tautology.
        // STRIPPER-REACH-LINE
        /* STRIPPER-REACH-BLOCK */
        const CANARY_LINE: &str = concat!("STRIPPER-REACH", "-LINE");
        const CANARY_BLOCK: &str = concat!("STRIPPER-REACH", "-BLOCK");

        // REACH CONTROL ON THE STRIPPER ITSELF — and it is ONE-DIRECTIONAL, which is worth saying
        // here rather than leaving a reader to infer it from three assertions labelled "reach
        // control". A stripper that silently NO-OPS would return this guard to exactly the
        // behaviour probes M3/M5 defeated, with more confident prose on top; these three catch
        // that. They do NOT catch an OVER-stripping one: `strip_comments` returning `String::new()`
        // satisfies all three (canary present in SRC, canary absent from `stripped`, `0 < len`).
        // The backstop for that is the anchor-uniqueness check inside `audit_collision_arms`, whose
        // message names over-stripping as one of the causes. Measured, #837 review round 3: the
        // `String::new()` mutation turned both guard tests RED from inside that call.
        // One canary per comment syntax, because M3 evaded a line comment and a block comment is
        // the other way to hide a brace.
        for canary in [CANARY_LINE, CANARY_BLOCK] {
            assert!(SRC.contains(canary),
                "reach control: the `{canary}` canary comment is gone, so the check below cannot \
                 prove the comment stripper ran. Restore it — it is a comment containing that \
                 marker and nothing else depends on it.");
        }
        let stripped = strip_comments(SRC);
        for canary in [CANARY_LINE, CANARY_BLOCK] {
            assert!(!stripped.contains(canary),
                "reach control: the comment stripper did NOT remove the `{canary}` comment, which \
                 is definitely present. It is no-opping for that comment syntax, and every claim \
                 this test makes about comments of that kind is void.");
        }
        assert!(stripped.len() < SRC.len(), "reach control: the stripper removed nothing at all");

        if let Err(why) = audit_collision_arms(SRC) { panic!("{why}"); }
    }

    /// The evasions that were measured against the FIRST version of the guard above, plus the ones
    /// its rewrite could plausibly introduce — as data, so they are re-run on every build instead of
    /// living in a review comment.
    ///
    /// Each case is a synthetic source built around the same `COLLISION_SIG` the real pin uses (via
    /// the constant, never a copy of the literal — a copy would break the pin's uniqueness reach
    /// control). `M3` and `M5` are the two probes from #837 review round 2 that were GREEN with a
    /// live wildcard; both must now be `Err`. They were ALSO re-run as real source mutations against
    /// the real file, which is the check this test cannot make.
    #[test]
    fn collision_arm_audit_rejects_the_evasions_measured_against_it() {
        let four_arms = "\
        match self {
            Self::Ready { collision, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            Self::Failed {..}  => None,
        }
    }";
        let src = |body: &str| format!("impl X {{\n    {COLLISION_SIG}\n{body}\n}}\n");

        // The real shape must PASS, or every Err below proves nothing.
        assert!(audit_collision_arms(&src(four_arms)).is_ok(),
            "the real body shape must pass: {:?}", audit_collision_arms(&src(four_arms)));

        // A `//` inside a STRING must not be treated as a comment. If the stripper cut this line at
        // the `//`, the closing quote would vanish, string state would invert for the rest of the
        // file, and the anchor would be swallowed — so this case passing is what shows the stripper
        // is literal-aware rather than a `split("//")`.
        let with_url = format!("impl X {{\n    let _s = \"https://example.invalid/a\"; // trailing\n    {COLLISION_SIG}\n{four_arms}\n}}\n");
        assert!(audit_collision_arms(&with_url).is_ok(),
            "a `//` inside a string literal must not be stripped: {:?}",
            audit_collision_arms(&with_url));

        // The wildcard check must be a check on PATTERN POSITION, not a `_` blocklist: a `_` used
        // as a FIELD pattern names its variant and is legitimate. Without the depth condition in
        // `has_top_level_wildcard_token` this would be a false rejection, which would eventually
        // get the guard deleted rather than fixed.
        let nested_underscore = "\
        match self {
            Self::Ready { collision, terrain_meshes: _, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            Self::Failed {..}  => None,
        }
    }";
        assert!(audit_collision_arms(&src(nested_underscore)).is_ok(),
            "a `_` as a nested FIELD pattern must be accepted: {:?}",
            audit_collision_arms(&src(nested_underscore)));

        let cases: [(&str, &str); 11] = [
            // ── #837 review round 4: the three shapes the LINE-wise scan accepted ───────────
            // All three put the wildcard on a named arm's own line, so it inherited that line's
            // `Self::` token; P1 and P5 also held the arm-LINE count at 4. Each was measured
            // ACCEPTED before the arm-wise rewrite, and each was carried through to a real hole:
            // with a fifth variant added, `collision`'s match left rustc's E0004 list with zero
            // `unreachable_patterns` warnings (control with the pristine arms: present).
            ("P1 wildcard sharing a named arm's line, behind an allow attribute", "\
        match self {
            Self::Ready { collision, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            Self::Failed {..} => None, #[allow(unreachable_patterns)] _ => None,
        }
    }"),
            // P2 adds no `=>` at all — the arrow count cannot see it. Only the top-level `_` in
            // pattern position can.
            ("P2 bare `_` as an or-pattern alternative on a named arm", "\
        match self {
            Self::Ready { collision, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            Self::Failed {..} | _ => None,
        }
    }"),
            ("P5 wildcard sharing a named arm's line, bare", "\
        match self {
            Self::Ready { collision, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            Self::Failed {..} => None, _ => None,
        }
    }"),
            // M1 — the literal pre-#826 one-liner.
            ("M1 pre-#826 one-liner",
             "        match self { Self::Ready { collision, .. } => Some(collision), _ => None }\n    }"),
            // M2 — wildcard replacing a named arm.
            ("M2 wildcard replaces a named arm", "\
        match self {
            Self::Ready { collision, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            _ => None,
        }
    }"),
            // M3 — GREEN against the old guard: `Self::` supplied from inside a comment.
            ("M3 Self:: hidden in a trailing comment", "\
        match self {
            Self::Ready { collision, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            _ => None,  // Self::Failed
        }
    }"),
            // M5 — GREEN against the old guard: a comment brace truncating the walk after the last
            // variant name, leaving the wildcard outside the scanned region.
            ("M5 comment brace truncates the walk", "\
        match self {
            Self::Ready { collision, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            Self::Failed {..}  => None, // end of match and fn: }}
            #[allow(unreachable_patterns)] _ => None,
        }
    }"),
            // M6 — wildcard ADDED alongside all four named arms.
            ("M6 wildcard added alongside the named arms", "\
        match self {
            Self::Ready { collision, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            Self::Failed {..}  => None,
            _ => None,
        }
    }"),
            // M7 — M5's shape moved into a BLOCK comment, which the first stripper rewrite would
            // still have been blind to.
            ("M7 block-comment brace truncates the walk", "\
        match self {
            Self::Ready { collision, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            Self::Failed {..}  => None, /* end of match and fn: }} */
            #[allow(unreachable_patterns)] _ => None,
        }
    }"),
            // M8 — M3's shape in a block comment: `Self::` supplied from inside one.
            ("M8 Self:: hidden in a block comment", "\
        match self {
            Self::Ready { collision, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            _ => None,  /* Self::Failed */
        }
    }"),
            // The equality that replaced the `>= 4` floor: a fifth arm line, every variant still
            // named, no wildcard anywhere. Only the count catches this one.
            ("fifth arm line, no wildcard", "\
        match self {
            Self::Idle if false => None,
            Self::Ready { collision, .. } => Some(collision),
            Self::Idle       => None,
            Self::Pending {..} => None,
            Self::Failed {..}  => None,
        }
    }"),
        ];
        for (name, body) in cases {
            assert!(audit_collision_arms(&src(body)).is_err(),
                "{name}: this must be REJECTED, and it was accepted");
        }

        // A raw string is REFUSED, not guessed at — the one construct the stripper cannot lex. The
        // opener is assembled at run time rather than written, because writing it would put a real
        // raw string in this file and the pin above would then refuse to scan it. (That is the guard
        // behaving correctly, but it would be a confusing way to find out.)
        // Assembled from CHAR literals, not from `"r"`: the text `"r"` is itself an `r` bracketed by
        // quotes, which `has_raw_string` — deliberately conservative, see its doc — reports as an
        // opener. Measured: writing it that way made the pin above refuse to scan this file.
        let raw_open = format!("{}{}{}", 'r', '"', "a raw string\"");
        let with_raw = format!("impl X {{\n    let _s = {raw_open};\n    {COLLISION_SIG}\n{four_arms}\n}}\n");
        let err = audit_collision_arms(&with_raw).unwrap_err();
        assert!(err.contains("raw strings"), "a raw string must be REFUSED by name: {err}");

        // …and the anchor's own reach control: no anchor at all must be an Err naming that, not a
        // vacuous pass over an empty body.
        let err = audit_collision_arms("fn unrelated() {}\n").unwrap_err();
        assert!(err.contains("reach control"), "a missing anchor must be a reach-control Err: {err}");
    }

    /// The specific F1 capture, as an assertion: standing in qeynos while the previous zone's
    /// assets are still fully `Ready` must NOT read as ready — and must name the reason, so an
    /// agent can tell "wrong world" from "no world".
    #[test]
    fn ready_for_the_previous_zone_is_never_usable_in_the_new_one() {
        let st = ZoneAssetState::ready("freporte", 412, floor_collision());
        assert!(st.is_ready(), "the state itself IS ready — that is exactly the trap");
        assert_eq!(usability(&st, "qeynos"), Some(NotUsable::StaleForPreviousZone));
        assert_eq!(NotUsable::StaleForPreviousZone.as_str(), "zone_assets_stale_for_previous_zone");
        assert_eq!(NotUsable::StaleForPreviousZone.state_word(), "stale");
        assert_eq!(usability(&st, "freporte"), None, "…and honest once the zones agree");
    }

    /// The window is created by two independent writers (the NET thread publishes `player.zone` on
    /// OP_NewZone; the RENDER thread calls `begin_zone_load` on its next frame). Simulate EVERY
    /// interleaving of those two writes around a zone change and assert no ordering can produce a
    /// usable verdict for a zone the character is not in.
    #[test]
    fn no_interleaving_of_the_two_writers_yields_a_usable_wrong_zone() {
        for net_first in [true, false] {
            for render_lag_frames in 0..4 {
                let (col, st) = slots();
                finish_zone_load(&col, &st, "freporte", Some(floor_collision()), 9, None);
                let mut player_zone = "freporte".to_string();

                let apply_net    = |pz: &mut String| *pz = "qeynos".to_string();
                let apply_render = |st: &ZoneAssetStateShared, col: &crate::collision::SharedCollision| {
                    begin_zone_load(col, st, "qeynos", "loading…");
                };
                if net_first {
                    apply_net(&mut player_zone);
                    // The render thread lags by N frames; the agent may poll in ANY of them.
                    for _ in 0..render_lag_frames {
                        let s = lock_state(&st).clone();
                        assert!(usability(&s, &player_zone).is_some(),
                            "net-first, lag {render_lag_frames}: reported usable while the loaded \
                             zone is still the one we LEFT");
                    }
                    apply_render(&st, &col);
                } else {
                    apply_render(&st, &col);
                    for _ in 0..render_lag_frames {
                        let s = lock_state(&st).clone();
                        assert!(usability(&s, &player_zone).is_some(),
                            "render-first, lag {render_lag_frames}: reported usable mid-change");
                    }
                    apply_net(&mut player_zone);
                }
                let s = lock_state(&st).clone();
                assert!(usability(&s, &player_zone).is_some(), "still loading the new zone");
                finish_zone_load(&col, &st, "qeynos", Some(floor_collision()), 5, None);
                let s = lock_state(&st).clone();
                assert!(usability(&s, &player_zone).is_none(), "…and usable once it lands");
            }
        }
    }

    /// A poisoned state mutex (a loader panicked holding it) must still be READABLE — otherwise the
    /// HTTP thread panics on the `unwrap` and the agent gets a connection error in place of the
    /// honest `failed` this type exists to deliver (#595 review F3).
    #[test]
    fn a_poisoned_state_lock_is_still_readable() {
        let st: ZoneAssetStateShared =
            Arc::new(std::sync::Mutex::new(ZoneAssetState::pending("qeynos", "loading…")));
        let poisoner = st.clone();
        let _ = std::thread::spawn(move || {
            let _g = poisoner.lock().unwrap();
            panic!("loader died holding the state lock");
        }).join();
        assert!(st.is_poisoned(), "precondition: the lock really is poisoned");
        assert_eq!(lock_state(&st).tag(), "pending", "a poisoned lock must not become a second failure");
    }

    #[test]
    fn idle_is_not_ready_and_names_no_zone() {
        let st = ZoneAssetState::default();
        assert_eq!(st.tag(), "idle");
        assert!(!st.is_ready());
        assert_eq!(st.zone(), None);
        assert!(st.collision().is_none());
    }
}
