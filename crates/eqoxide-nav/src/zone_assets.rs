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
        if !collision.has_geometry() {
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
    *state.lock().unwrap() = ZoneAssetState::pending(zone, status);
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
    *state.lock().unwrap() = verdict;
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

    #[test]
    fn idle_is_not_ready_and_names_no_zone() {
        let st = ZoneAssetState::default();
        assert_eq!(st.tag(), "idle");
        assert!(!st.is_ready());
        assert_eq!(st.zone(), None);
        assert!(st.collision().is_none());
    }
}
