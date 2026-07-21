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
#[derive(Clone)]
pub enum ZoneAssetState {
    /// No zone has been loaded and none is loading — e.g. before the first zone-in. Distinct from
    /// `Pending`: nothing is on its way, so waiting for a `Ready` here would hang forever.
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
    pub fn collision(&self) -> Option<&Arc<Collision>> {
        match self { Self::Ready { collision, .. } => Some(collision), _ => None }
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
        let col = Collision::build(
            &ZoneAssets { terrain: vec![mesh], objects: vec![], textures: vec![] }, 32.0);
        Self::ready("testfixture", 1, Arc::new(col))
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

/// **The one decision function.** May the loaded assets be used to answer questions about the world
/// the character is standing in? `None` = yes; `Some(reason)` = no, and here is the machine-readable
/// why.
///
/// It is pure and takes the player's zone explicitly, so the zone-identity check can never be
/// forgotten by a caller that only had the state handy — and so the universal claim ("a `ready`
/// observation is never about a zone you are not in") is a property test, not a live run.
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
    if !loaded.eq_ignore_ascii_case(player_zone) { return Some(NotUsable::StaleForPreviousZone); }
    None
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

impl Default for ZoneAssetState {
    fn default() -> Self { Self::Idle }
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
    // existence proof over one trajectory and cannot discharge a "never". These exercise the single
    // decision function every consumer goes through.

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
