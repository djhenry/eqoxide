//! The sparse **water-span grid** (3D-water-volume design §5).
//!
//! A per-zone, WATER-ONLY structure that describes the *interior* of a water volume, meaning the
//! states a swimmer can occupy, without the memory cost of a whole-zone 3D voxel grid (design
//! §4.2: an everfrost-scale 2u voxelization is ~10⁹ nodes).
//!
//! The grid is constructed on demand via [`crate::collision::Collision::build_water_grid`].
//!
//! ## Representation (design §5.1)
//!
//! * A sparse map keyed by a **4u XY column lattice** aligned to the collision grid origin (each 8u
//!   coarse 8u cell = 2×2 water columns). Only wet columns are stored; dry land stores nothing.
//! * Each column stores its `surface_z` plus its navigable z-**interval(s)** (`spans`), NOT voxels.
//!   Vertical is intervals, horizontal is sparse — that is the whole compression story. Open water
//!   costs a couple of floats per column regardless of depth; a tunnel under a floor under a pool
//!   shows up as a *second* span only where it exists.
//! * Per span `(nav_lo, nav_hi)` is the range a swimmer's **feet** may occupy, with, per design §5.1:
//!   * `nav_hi = min(surface_z − float_depth, first_solid_above − Body::height − SKIN)` — at most the
//!     buoyancy swim plane, and low enough that the body top clears the ceiling (exactly what the
//!     collided `swim_rise` enforces at run time).
//!   * `nav_lo = max(water_bottom, nearest_solid_floor_below) + ε` — feet stay in water and above the
//!     collision floor.
//!   * A span with `nav_hi < nav_lo` (water shallower than the body, or a slab thinner than
//!     clearance) is not stored — that region is unnavigable-3D.
//!
//! The 3D water NODES within a span are *implicit*, materialized during search expansion at
//! `z ∈ {nav_hi, nav_hi − VRES, …} ∪ {nav_lo}` with `VRES = 2.0` (= the existing `qf` z-bucket), so a
//! water node's key is the SAME `(col, row, qf(z))` the land search already uses. Slice 1 builds and
//! measures the intervals; the node materialization is Slice 2.

use std::collections::HashMap;

/// Vertical node resolution (design §5.1): deliberately equal to the existing `qf` z-bucket
/// (`collision.rs`), so a water node shares the land search's key type. Slice 1 uses it only to size
/// the build-time water-band probe scan.
pub const VRES: f32 = 2.0;

/// One wet 4u column of the span grid.
#[derive(Clone, Debug, PartialEq)]
pub struct WaterColumn {
    /// The water surface height at this column (the highest band's surface). This IS the top span's
    /// reference: `nav_hi` of the surface span = `surface_z − float_depth`.
    pub surface_z: f32,
    /// Navigable feet-intervals `(nav_lo, nav_hi)`, high band first. Almost always length 1 (open
    /// water); ≥ 2 only where submerged geometry (a floor/ceiling inside the volume) carves a gap.
    pub spans: Vec<(f32, f32)>,
}

impl WaterColumn {
    /// The navigable span whose feet-interval contains `z` (a hair of tolerance folds a node sitting
    /// exactly on a boundary in), or `None` if `z` is above the swim plane / below the floor / inside
    /// a carved-out solid gap. This is the "is `(x,y,z)` an INTERIOR water node here?" test the
    /// search uses to tell a water node from a land node at expansion (design §6.1). It is exact
    /// (real stored bounds), so a node it admits is genuinely inside stored, carved water — the
    /// #534/#540 honesty guarantee that no node sits in solid.
    pub fn span_containing(&self, z: f32) -> Option<(f32, f32)> {
        const TOL: f32 = 0.01;
        self.spans.iter().copied().find(|&(lo, hi)| z >= lo - TOL && z <= hi + TOL)
    }

    /// Every materialized water NODE z in this column, high→low: the GLOBAL `VRES` lattice points
    /// inside each span (design §5.1 — nodes are implicit, materialized during expansion).
    ///
    /// A node key is `(col, row, qf(z))` with `qf(z) = round(z / VRES)` (the search's existing
    /// z-bucket). Anchoring the node lattice to the GLOBAL `VRES` grid — `z ∈ {…, −2, 0, 2, …}` —
    /// rather than to each column's own `nav_hi`, is a deliberate, documented refinement of the
    /// design text: it keeps adjacent columns' nodes phase-aligned, so a horizontal "same-z" edge
    /// lands on a REAL neighbour node and the `qf` key is shared with the land search by
    /// construction (a per-column `nav_hi` phase would make two adjacent columns' lattices
    /// incommensurate and the `qf` key ambiguous). The ≤`VRES` offset this trades away from the
    /// exact swim plane is well within the haul-out (`haul_out_up`) and arrival (`GOAL_TIER_TOL`)
    /// tolerances; Slice 3 tunes execution against the surface. A span too thin to contain any
    /// lattice point still yields ONE node, at its swim-plane `hi`, so no navigable water is dropped.
    pub fn node_zs(&self) -> Vec<f32> {
        let mut zs = Vec::new();
        for &(lo, hi) in &self.spans {
            let top = (hi / VRES).floor() * VRES; // highest global lattice point ≤ hi
            if top < lo - 1e-4 {
                zs.push(hi); // span thinner than the lattice spacing: a single node at the swim plane
                continue;
            }
            let mut z = top;
            while z >= lo - 1e-4 {
                zs.push(z);
                z -= VRES;
            }
        }
        zs
    }

    /// The highest water node in the column — the swim-plane node that land↔water transitions
    /// (entry from a shore, haul-out to land) attach to (design §7.1/§7.2).
    pub fn top_node_z(&self) -> Option<f32> {
        self.node_zs().into_iter().max_by(|a, b| a.total_cmp(b))
    }

    /// The materialized node z nearest `z` across all spans, or `None` if the column has no nodes.
    /// Design §6.1: a start/goal in water resolves to the nearest lattice z IN the containing
    /// interval — no surface or bottom projection.
    pub fn nearest_node_z(&self, z: f32) -> Option<f32> {
        self.node_zs().into_iter().min_by(|a, b| (a - z).abs().total_cmp(&(b - z).abs()))
    }
}

/// The per-zone sparse water-span grid. Keyed by integer 4u column indices `(ci, cj)` relative to
/// [`origin`](Self::origin).
#[derive(Clone, Debug, Default)]
pub struct WaterGrid {
    columns: HashMap<(i32, i32), WaterColumn>,
    /// The (east, north) world position of column index `(0, 0)`'s corner — the collision grid
    /// origin (design §5.1: "aligned to the coarse grid origin").
    origin: [f32; 2],
    /// XY column pitch — 4.0 (locked, design §5.1 / owner decision #2).
    col_size: f32,
    /// Count of candidate wet columns whose water volume was UNBOUNDED BELOW (`bottom_z` == None) —
    /// a design-premise honesty signal (§5.2). Expected 0 on real `.wtr`; a nonzero count on the
    /// gate zones is an owner finding, so the harness reports it rather than fabricating a bottom.
    unbounded_below: u32,
}

impl WaterGrid {
    /// An empty grid anchored at `origin` with column pitch `col_size` (4.0). Populated by the
    /// builder in `collision.rs` (which owns the collision internals the build needs).
    pub fn new(origin: [f32; 2], col_size: f32) -> Self {
        WaterGrid { columns: HashMap::new(), origin, col_size, unbounded_below: 0 }
    }

    /// Store a wet column at lattice index `(ci, cj)`.
    pub fn insert(&mut self, ci: i32, cj: i32, col: WaterColumn) {
        self.columns.insert((ci, cj), col);
    }

    /// Record that a candidate column's water volume was unbounded below (`bottom_z` == None).
    pub fn note_unbounded_below(&mut self) {
        self.unbounded_below += 1;
    }

    /// The lattice index containing world point `(east, north)`.
    pub fn column_index(&self, east: f32, north: f32) -> (i32, i32) {
        (
            ((east - self.origin[0]) / self.col_size).floor() as i32,
            ((north - self.origin[1]) / self.col_size).floor() as i32,
        )
    }

    /// The wet column at world `(east, north)`, if any.
    pub fn column_at(&self, east: f32, north: f32) -> Option<&WaterColumn> {
        let (ci, cj) = self.column_index(east, north);
        self.columns.get(&(ci, cj))
    }

    /// The wet column at lattice index `(ci, cj)`, if any.
    pub fn column(&self, ci: i32, cj: i32) -> Option<&WaterColumn> {
        self.columns.get(&(ci, cj))
    }

    /// Number of wet columns stored.
    pub fn wet_column_count(&self) -> usize {
        self.columns.len()
    }

    /// Total number of materialized water NODES across all columns (design §5.1 / Slice 2). A cheap
    /// scalar the lazy-build wiring logs so the first-water-plan cost is attributable to a concrete
    /// node count, not just a column count.
    pub fn node_count(&self) -> usize {
        self.columns.values().map(|c| c.node_zs().len()).sum()
    }

    /// Total number of navigable spans across all columns.
    pub fn span_count(&self) -> usize {
        self.columns.values().map(|c| c.spans.len()).sum()
    }

    /// Count of candidate columns whose volume was unbounded below (design-premise signal, §5.2).
    pub fn unbounded_below_count(&self) -> u32 {
        self.unbounded_below
    }

    pub fn origin(&self) -> [f32; 2] { self.origin }
    pub fn col_size(&self) -> f32 { self.col_size }

    /// Iterate `((ci, cj), &WaterColumn)` over the wet columns (order unspecified).
    pub fn iter(&self) -> impl Iterator<Item = (&(i32, i32), &WaterColumn)> {
        self.columns.iter()
    }

    /// Estimated memory in bytes from the design's own accounting model (§5.4) so the harness
    /// numbers are directly comparable to the doc's predicted budgets. This is a DESIGN-MODEL
    /// ESTIMATE, not measured RSS: it mirrors the doc's derivation rather than
    /// `size_of::<HashMap>()` internals (which vary by allocator/load-factor and are not what the
    /// design budgeted against) and so UNDERCOUNTS real heap RSS by ~25–40%. Named `estimated_bytes`
    /// for exactly that reason — do not read it as a measured resident-set figure.
    ///
    /// per column ≈ 4B surface + 2×8B inline intervals + len/flags ≈ 28B payload, ×2 for
    /// sparse-hash overhead ⇒ ~56B/column; each span BEYOND the inline 2 adds 8B (also ×2).
    pub fn estimated_bytes(&self) -> usize {
        const INLINE_SPANS: usize = 2;
        let mut payload = 0usize;
        for c in self.columns.values() {
            let base = 4 + 4 + INLINE_SPANS * 8; // surface + len/flags + 2 inline intervals
            let extra = c.spans.len().saturating_sub(INLINE_SPANS) * 8;
            payload += base + extra;
        }
        payload * 2 // sparse-hash overhead (design §5.4)
    }
}

// ─────────────────────── water-data PROVENANCE: measured vs UNMEASURED (#762) ───────────────────────

/// One zone's water/region data **together with the reason it is missing when it is** — the value
/// that replaces `RegionMap::load(..).map(Arc::new)` anywhere the caller goes on to report a number.
///
/// # The bug this type exists to make unrepresentable
///
/// `set_water(None)` does not mean "unknown". Once it is on a collision grid it is an **answer**:
/// `in_water` is `false` everywhere, `water_surface` is `None` everywhere, the water-span grid is
/// empty. A corpus run over that grid dutifully reports `wat-route: 0, #423: 0` — a confident claim
/// about water handling, made without ever consulting any water data, and *the false reading is the
/// reassuring one*.
///
/// So a zone is either [`ZoneWater::Measured`] — the region data loaded, and a zero counted against
/// it is a **real** zero — or [`ZoneWater::Unmeasured`], which carries no water map and, crucially,
/// **cannot produce a number at all**: [`ZoneWater::measure`] is the only way to obtain a
/// [`WaterMeasurement`], and it runs its closure only in the `Measured` case.
#[derive(Clone)]
pub enum ZoneWater {
    /// The zone's region data loaded. Water answers off this map are measurements.
    Measured(std::sync::Arc<eqoxide_core::region_map::RegionMap>),
    /// The region data did NOT load, and here is why. No water number about this zone exists.
    Unmeasured(eqoxide_core::region_map::RegionLoadError),
}

// Hand-written so the failure REASON survives into any `{:?}` a caller reaches for (a derive would
// need `Debug` on the whole BSP node list, which is megabytes of noise and not the diagnostic).
impl std::fmt::Debug for ZoneWater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Measured(m) => write!(f, "Measured({} BSP nodes)", m.node_count()),
            Self::Unmeasured(e) => write!(f, "Unmeasured({e})"),
        }
    }
}

impl ZoneWater {
    /// Load `<dir>/<zone>.wtr`, keeping the failure. The direct replacement for
    /// `RegionMap::load(dir, zone).map(Arc::new)`.
    pub fn load(dir: &std::path::Path, zone: &str) -> Self {
        match eqoxide_core::region_map::RegionMap::try_load(dir, zone) {
            Ok(rm) => Self::Measured(std::sync::Arc::new(rm)),
            Err(e) => Self::Unmeasured(e),
        }
    }

    /// Wrap an already-loaded/hand-authored region map (synthetic scenes, fixtures). Test-only,
    /// gated the same way as the `RegionMap` fixture constructors it wraps (`flat_below`,
    /// `water_slab`, …) — round-2 finding N2: this was the whole escape hatch that let a
    /// `WaterMeasurement` be fabricated without a real `.wtr` ever loading, which falsified
    /// [`WaterMeasurement`]'s "no public constructor" doc in exactly the test builds where every
    /// converted consumer lives. Gating it here closes that in the same way the rest of this
    /// codebase gates fixture constructors — see `ZoneAssetState::test_ready` and
    /// `RegionMap::flat_below`.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn from_map(map: eqoxide_core::region_map::RegionMap) -> Self {
        Self::Measured(std::sync::Arc::new(map))
    }

    /// The region map, when there is one.
    pub fn map(&self) -> Option<&std::sync::Arc<eqoxide_core::region_map::RegionMap>> {
        match self { Self::Measured(m) => Some(m), Self::Unmeasured(_) => None }
    }

    /// True only when real region data is present.
    pub fn is_measured(&self) -> bool { matches!(self, Self::Measured(_)) }

    /// Why the data is absent — `None` when it is present.
    pub fn reason(&self) -> Option<&eqoxide_core::region_map::RegionLoadError> {
        match self { Self::Unmeasured(e) => Some(e), Self::Measured(_) => None }
    }

    /// Install this zone's region data onto `col`.
    ///
    /// `#[must_use]`: there is nothing to install in the `Unmeasured` case, and a caller that
    /// ignores that goes on to read fabricated dry answers out of the grid. **Round-2 correction
    /// (N3):** this is a warning, not a gate — the compiler warns unless the caller writes
    /// `let _ = zw.install(&mut col);`, which is the idiomatic Rust spelling of "I see the
    /// must-use value and am discarding it on purpose" and compiles with ZERO diagnostics
    /// regardless of lint level (this workspace also doesn't `deny(unused_must_use)` or set a CI
    /// `RUSTFLAGS` that would). So `#[must_use]` is a speed bump for an attentive reader, not
    /// something the compiler enforces — the actual enforcement that a zone's water was really
    /// consulted is `WaterRollup`/the corpus `assert!`s that consume the `Result`, not this
    /// attribute.
    #[must_use = "an UNMEASURED zone installs no water: every water answer off this grid would be a \
                  fabricated dry (#762). Report it as unmeasured or refuse to score the zone."]
    pub fn install(&self, col: &mut crate::collision::Collision)
        -> Result<(), &eqoxide_core::region_map::RegionLoadError> {
        // #803: BOTH arms now write the grid. An `Unmeasured` zone used to leave the grid's region
        // slot untouched, so the grid could not tell "nobody asked" from "we asked and the `.wtr`
        // did not load" — and `zone_line_indices` answered `[]` for both. Recording the reason keeps
        // the `Err` return (the corpus's must-use contract is unchanged) AND makes the grid itself
        // able to refuse honestly if some later reader asks it about exits.
        match self {
            Self::Measured(m) => { col.set_region_data(Ok(m.clone())); Ok(()) }
            Self::Unmeasured(e) => { col.set_region_data(Err(e.clone())); Err(e) }
        }
    }

    /// **The only way to obtain a [`WaterMeasurement`].** `f` runs only when the region data
    /// actually loaded, so a `WaterMeasurement` holding a value is proof that a water map existed
    /// when it was created. For a plain tally, `zw.measure(|_| 0usize)` opens a counter that a zone
    /// with no `.wtr` simply does not get.
    pub fn measure<T>(&self, f: impl FnOnce(&eqoxide_core::region_map::RegionMap) -> T) -> WaterMeasurement<T> {
        match self {
            Self::Measured(m) => WaterMeasurement { value: Some(f(m)), reason: None },
            Self::Unmeasured(e) => WaterMeasurement { value: None, reason: Some(e.clone()) },
        }
    }

    /// A zero-initialized tally for this zone: `Measured(0)` when there is water data to count
    /// against, `Unmeasured` otherwise. Increment through [`WaterMeasurement::value_mut`].
    pub fn tally(&self) -> WaterMeasurement<usize> { self.measure(|_| 0usize) }
}

/// The word a report prints where a water number would go when the zone was never measured.
/// Deliberately not a number, not `-`, and not `0`.
pub const UNMEASURED: &str = "unmeasured";

/// A water-specific measurement for ONE zone, carrying whether it is a measurement at all.
///
/// The value is private and there is **no public constructor**: the only way to make one is
/// [`ZoneWater::measure`], and `measure`'s closure runs only in the `Measured` arm — so a real
/// `.wtr` load is what puts a value in here in production. That guarantee covers production code
/// only. In a test build, [`ZoneWater::from_map`] and the `RegionMap` fixture constructors (`flat_below`,
/// `water_slab`, …) can put `ZoneWater` into `Measured` without a `.wtr` ever touching disk — on
/// purpose, so tests can exercise `measure` without real assets — and both are gated behind
/// `#[cfg(any(test, feature = "test-fixtures"))]` for exactly that reason. So the precise claim is:
/// outside test code, "a water number for a zone whose `.wtr` did not load" is not a value this
/// type can hold. `Display` prints [`UNMEASURED`] rather than a zero that reads as a perfect score.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaterMeasurement<T> {
    /// `Some` **iff** the zone's region data loaded.
    value: Option<T>,
    /// `Some` iff it did not.
    reason: Option<eqoxide_core::region_map::RegionLoadError>,
}

impl<T> WaterMeasurement<T> {
    /// The measured value — `None` means *no measurement exists*, never *zero*.
    pub fn value(&self) -> Option<&T> { self.value.as_ref() }
    /// Mutable access for accumulating; `None` for an unmeasured zone, so there is nothing to bump
    /// and no number can appear out of nowhere.
    pub fn value_mut(&mut self) -> Option<&mut T> { self.value.as_mut() }
    pub fn is_measured(&self) -> bool { self.value.is_some() }
    /// Why this zone was not measured — `None` when it was.
    pub fn reason(&self) -> Option<&eqoxide_core::region_map::RegionLoadError> { self.reason.as_ref() }
}

impl<T: std::fmt::Display> std::fmt::Display for WaterMeasurement<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.value {
            Some(v) => write!(f, "{v}"),
            None => write!(f, "{UNMEASURED}"),
        }
    }
}

/// A corpus TOTAL over a per-zone water column, with an honest denominator.
///
/// Summing only the zones that loaded and printing the sum would read `wat-route: 0` over "14 of 15
/// zones" exactly like `wat-route: 0` over all 15. The rollup stores the unmeasured zones by name,
/// reports `is_complete` as false while any hole exists, and says so in its `Display`.
///
/// [`begin_zone`](Self::begin_zone) opens a zone, and `add` or `skip` closes it. A zone that is
/// opened and never closed, by any control flow such as `continue`, `break`, `?` or an early
/// `return`, is recorded as `unaccounted` when the next zone opens, or when the rollup is read for
/// the last zone. An `unaccounted` zone counts toward `attempted_zones` and makes `is_complete`
/// false, and `Display` names the zone. `add` and `skip` panic if no matching zone is open.
///
/// A zone dropped before `add` is called is recorded through [`skip`](Self::skip), which gives the
/// zone a third outcome distinct from measured and unmeasured. [`open_corpus_zone`] opens a zone
/// and records each drop path through this type. A loop that uses no `WaterRollup` is invisible to
/// this type.
///
/// A `WaterRollup` renders `— COMPLETE` or `— INCOMPLETE` in its `Display`. A check for the clean
/// marker must match `"— COMPLETE"` with the em dash, because `"INCOMPLETE"` contains
/// `"COMPLETE"`.
#[derive(Clone, Debug, Default)]
pub struct WaterRollup {
    total: usize,
    measured_zones: usize,
    unmeasured: Vec<(String, eqoxide_core::region_map::RegionLoadError)>,
    /// Zones dropped BEFORE the water check ever ran — no `.glb`, an empty collision grid, no
    /// routable pairs, etc. Distinct from `unmeasured`: an unmeasured zone's water check RAN and
    /// failed; a skipped zone's water check never ran at all. Both are holes; neither is a zero.
    skipped: Vec<(String, String)>,
    /// The zone [`begin_zone`](Self::begin_zone) opened that `add`/`skip` has not closed yet.
    open: Option<String>,
    /// Zones that were opened and then abandoned without ever reaching `add` or `skip`. This is the
    /// bucket that catches a drop path nobody wired — the round-3 B1 defect — without the caller
    /// having to enumerate its own `continue`s correctly.
    unaccounted: Vec<String>,
}

impl WaterRollup {
    pub fn new() -> Self { Self::default() }

    /// Open a zone. Call this as the FIRST statement of a corpus loop's body, before anything that
    /// could `continue`.
    ///
    /// The previously-open zone, if any, is closed here: if it was never settled by `add` or `skip`
    /// it becomes an `unaccounted` hole. That is the whole point — the rollup finds the drop itself
    /// rather than trusting the loop to have declared it.
    pub fn begin_zone(&mut self, zone: &str) {
        if let Some(prev) = self.open.take() {
            self.unaccounted.push(prev);
        }
        self.open = Some(zone.to_string());
    }

    /// Close the open zone, or panic. `add`/`skip` are the only two ways to close one.
    fn settle(&mut self, zone: &str) {
        match self.open.take() {
            Some(z) if z == zone => {}
            Some(z) => {
                // The loop settled a different zone than the one it opened: the opened one is a
                // hole, and the mismatch itself is a bug worth being loud about.
                self.unaccounted.push(z.clone());
                panic!("#762: WaterRollup was given a measurement for zone {zone:?} while zone \
                        {z:?} was the one open — begin_zone/add must name the same zone");
            }
            None => panic!("#762: WaterRollup::add/skip called for zone {zone:?} with no zone open \
                            — every corpus loop iteration must start with begin_zone(zone), or the \
                            rollup cannot tell a dropped zone from a zone that was never asked for"),
        }
    }

    /// Fold one zone's column in, closing the zone [`Self::begin_zone`] opened. An unmeasured zone
    /// contributes NO number — it is recorded as a hole in the corpus instead.
    pub fn add(&mut self, zone: &str, m: &WaterMeasurement<usize>) {
        self.settle(zone);
        match (m.value(), m.reason()) {
            (Some(v), _) => { self.total += *v; self.measured_zones += 1; }
            (None, Some(e)) => self.unmeasured.push((zone.to_string(), e.clone())),
            // Unconstructible: `measure` always sets exactly one of the two.
            (None, None) => self.unmeasured.push((zone.to_string(),
                eqoxide_core::region_map::RegionLoadError::Missing)),
        }
    }

    /// Record a zone that was dropped BEFORE the water check ran at all — the corpus never got as
    /// far as [`ZoneWater::load`]/`install` for it, so it can't even say "unmeasured, here's why
    /// the `.wtr` didn't load" (that would be a lie: the `.wtr` may be fine, nobody asked it).
    /// `reason` is a short, free-text tag ("no glb", "no grid", "no routable pairs") for the
    /// printed line — this is not a [`RegionLoadError`](eqoxide_core::region_map::RegionLoadError),
    /// because the failure isn't a region-load failure.
    ///
    /// Closes the open zone, like `add`. Forgetting it at some `continue` no longer erases the zone:
    /// it lands in `unaccounted` instead, which is louder but still honest — see the type doc.
    pub fn skip(&mut self, zone: &str, reason: impl Into<String>) {
        self.settle(zone);
        self.skipped.push((zone.to_string(), reason.into()));
    }

    /// Sum over the zones that WERE measured. Meaningless on its own — read it beside
    /// [`Self::is_complete`], or just print the rollup.
    pub fn measured_total(&self) -> usize { self.total }
    pub fn measured_zones(&self) -> usize { self.measured_zones }
    /// Every zone selected for this run whose water data did not load.
    pub fn unmeasured_zones(&self) -> Vec<&str> {
        self.unmeasured.iter().map(|(z, _)| z.as_str()).collect()
    }
    /// Every zone selected for this run that was dropped before the water check ran at all.
    pub fn skipped_zones(&self) -> Vec<&str> {
        self.skipped.iter().map(|(z, _)| z.as_str()).collect()
    }
    /// Every zone that was opened and then left the loop without `add` or `skip` — including one
    /// that is STILL open when the rollup is read (the last iteration abandoning its zone looks
    /// exactly like the loop not having finished, and both are holes).
    pub fn unaccounted_zones(&self) -> Vec<&str> {
        self.unaccounted.iter().map(String::as_str)
            .chain(self.open.as_deref())
            .collect()
    }
    /// The TRUE denominator: every zone this rollup was told about at all, whether it ended up
    /// measured, unmeasured, skipped, or unaccounted. This is what `Display`'s "N zones" now means —
    /// not "N zones that happened to reach `add`" (the round-1 shape).
    pub fn attempted_zones(&self) -> usize {
        self.measured_zones + self.unmeasured.len() + self.skipped.len()
            + self.unaccounted_zones().len()
    }
    /// True only when EVERY zone folded in was measured — none unmeasured, none skipped, none
    /// unaccounted — AND at least one zone was folded in at all. A water-inclusive gate must assert
    /// this — a run with a hole in it (of any of the three kinds) has no water result, however green
    /// the rest looks.
    ///
    /// The `attempted_zones() > 0` term is round 3 (review N-R2c): a `Default`-constructed rollup
    /// used to answer "complete" and print `0 (over 0/0 zones)`, i.e. a type whose entire job is
    /// refusing to look clean shipped a value that looks clean over nothing. In the flagship corpus
    /// `assert!(tot_walked > 0)` fires first so it was not reachable there, but it was a live trap
    /// for any future consumer without that guard.
    pub fn is_complete(&self) -> bool {
        self.attempted_zones() > 0
            && self.unmeasured.is_empty() && self.skipped.is_empty()
            && self.unaccounted_zones().is_empty()
    }
}

impl std::fmt::Display for WaterRollup {
    /// Renders the total, the measured and attempted zone counts, and a `— COMPLETE` or
    /// `— INCOMPLETE` marker.
    ///
    /// The marker describes this rollup only. A caller with more than one rollup renders them
    /// through [`RollupReport`], which puts each on its own line and adds a verdict for the report.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let zones = self.attempted_zones();
        if self.is_complete() {
            return write!(f, "{} (over {}/{} zones) — COMPLETE", self.total, self.measured_zones, zones);
        }
        if zones == 0 {
            return write!(f, "{} over 0/0 zones — INCOMPLETE, no zone ever reached this rollup",
                self.total);
        }
        let mut parts: Vec<String> = Vec::new();
        if !self.unmeasured.is_empty() {
            let holes: Vec<String> = self.unmeasured.iter().map(|(z, e)| format!("{z}: {e}")).collect();
            parts.push(format!("{} {} [{}]", self.unmeasured.len(), UNMEASURED, holes.join("; ")));
        }
        if !self.skipped.is_empty() {
            let names: Vec<String> = self.skipped.iter().map(|(z, r)| format!("{z} ({r})")).collect();
            parts.push(format!("{} skipped [{}]", self.skipped.len(), names.join("; ")));
        }
        let unacc = self.unaccounted_zones();
        if !unacc.is_empty() {
            parts.push(format!("{} unaccounted [{}] (opened by begin_zone, never reached add/skip)",
                unacc.len(), unacc.join("; ")));
        }
        write!(f, "{} over {}/{} zones — INCOMPLETE, {}",
            self.total, self.measured_zones, zones, parts.join("; "))
    }
}

// ───────── #898: a report that prints MORE THAN ONE rollup is its own honesty surface ─────────

/// The marker a [`RollupReport`] prints when EVERY rollup on it is complete.
///
/// Deliberately not a superstring or substring of any other marker in this module — see
/// [`RollupReport`] for the table and `composite_markers_are_line_safe_against_every_other_898`
/// for the pin.
pub const COMPOSITE_CLEAN: &str = "— ALL-COMPLETE";
/// The marker a [`RollupReport`] prints when ANY rollup on it is incomplete — **and when it holds
/// no rollups at all**, which is the same clean-over-nothing refusal [`WaterRollup::is_complete`]
/// makes with its `attempted_zones() > 0` term.
pub const COMPOSITE_DIRTY: &str = "— NOT-ALL-COMPLETE";

/// Several labelled [`WaterRollup`]s rendered as ONE report, with a verdict of its own.
///
/// ## Why a report type exists
///
/// [`WaterRollup`]'s `Display` marks itself `— COMPLETE` or `— INCOMPLETE`, and that marker is
/// true *of that rollup*. Two rollups printed on one line can carry `— COMPLETE` on a line whose
/// other rollup is incomplete, so a line-level grep for `— COMPLETE` reads the line as clean.
///
/// ## What this type changes
///
/// 1. **One rollup per line.** Each rollup's own `— COMPLETE`/`— INCOMPLETE` is line-safe again,
///    because the only thing on its line is the rollup the marker describes.
/// 2. **The report gets its own line-level verdict**, [`COMPOSITE_CLEAN`] / [`COMPOSITE_DIRTY`],
///    on a line of its own. So "is this whole report clean?" is a line-level grep too, and it is a
///    DIFFERENT string from the per-rollup markers — a grep for one can never match the other.
/// 3. **Empty is not clean.** `RollupReport::new([])` answers `false` from
///    [`all_complete`](Self::all_complete) and prints [`COMPOSITE_DIRTY`], for the same reason
///    [`WaterRollup::is_complete`] carries `attempted_zones() > 0`: a type whose job is refusing to
///    look clean must not have a value that looks clean over nothing.
///
/// ## The marker table, and why these four strings
///
/// | marker | printed by | contains `— COMPLETE`? | contains `— ALL-COMPLETE`? |
/// |---|---|---|---|
/// | `— COMPLETE` | one clean [`WaterRollup`] | yes (itself) | no |
/// | `— INCOMPLETE` | one dirty [`WaterRollup`] | no | no |
/// | [`COMPOSITE_CLEAN`] | a clean report | no | yes (itself) |
/// | [`COMPOSITE_DIRTY`] | a dirty or EMPTY report | no | no |
///
/// The em-dash is load-bearing in every row, exactly as it is for `— INCOMPLETE` vs `COMPLETE`:
/// `"NOT-ALL-COMPLETE"` contains `"ALL-COMPLETE"`, and only the leading `— ` separates them.
/// `composite_markers_are_line_safe_against_every_other_898` asserts the whole table rather than
/// leaving it as prose.
///
/// ## What this does NOT claim
///
/// * It does not stop a caller writing its own `format!("{a}, {b}")` over two rollups. Nothing in
///   Rust can. What it does is give every such call site in this workspace one thing to use
///   instead, and pin the property so a reviewer has a name for what a hand-rolled line loses.
/// * It says nothing about a reader who greps a whole multi-line BLOCK rather than a line. A
///   clean rollup inside an aborted report is still printed, and printing it is the point — the
///   report says which rollup is clean and states, on its own line, that the report is not.
pub struct RollupReport<'a> {
    rolls: Vec<(&'a str, &'a WaterRollup)>,
}

impl<'a> RollupReport<'a> {
    pub fn new(rolls: impl IntoIterator<Item = (&'a str, &'a WaterRollup)>) -> Self {
        Self { rolls: rolls.into_iter().collect() }
    }

    /// Every rollup on the report is complete, **and there is at least one**. See the type doc's
    /// point 3 for why the second term is not decoration.
    pub fn all_complete(&self) -> bool {
        !self.rolls.is_empty() && self.rolls.iter().all(|(_, r)| r.is_complete())
    }

    /// The labels of the rollups that are NOT complete, in report order.
    pub fn incomplete_labels(&self) -> Vec<&'a str> {
        self.rolls.iter().filter(|(_, r)| !r.is_complete()).map(|(l, _)| *l).collect()
    }

    /// The report's own line-level marker: [`COMPOSITE_CLEAN`] or [`COMPOSITE_DIRTY`].
    pub fn marker(&self) -> &'static str {
        if self.all_complete() { COMPOSITE_CLEAN } else { COMPOSITE_DIRTY }
    }
}

impl std::fmt::Display for RollupReport<'_> {
    /// Renders as a leading newline, then one `  <label>: <rollup>` line per rollup, then the
    /// report's own verdict line. Every line carries at most one marker, and the verdict is the
    /// only line that can be read as a claim about the report as a whole.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (label, roll) in &self.rolls {
            writeln!(f)?;
            write!(f, "  {label}: {roll}")?;
        }
        writeln!(f)?;
        let n = self.rolls.len();
        if self.rolls.is_empty() {
            return write!(f, "  0 rollups on this report {COMPOSITE_DIRTY} — no rollup ever \
                              reached this report, which is not a clean result");
        }
        if self.all_complete() {
            return write!(f, "  {n} rollup(s) on this report {COMPOSITE_CLEAN}");
        }
        write!(f, "  {n} rollup(s) on this report {COMPOSITE_DIRTY}, incomplete: {:?}",
            self.incomplete_labels())
    }
}

// ───────── #807: the corpus PROLOGUE, so its drop paths are not each corpus' own to wire ─────────

/// Why [`open_corpus_zone`] handed a zone back unscored.
///
/// It carries only display text for the caller's per-zone table row. **The accounting has already
/// happened** by the time this value exists — the rollup passed to `open_corpus_zone` has the zone
/// closed in its `skipped` or `unmeasured` bucket — so a caller that ignores this value entirely
/// still cannot lose the zone. Printing it is diagnostics, not bookkeeping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZoneDropped(String);

impl std::fmt::Display for ZoneDropped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}

/// Opens one corpus zone against a rollup and returns a grid that is ready to score, or the reason
/// the zone was dropped, already recorded.
///
/// The load, build and install steps each have a drop path that records the zone in `cover` as
/// `skip` or `unmeasured`, so a corpus that calls this function cannot lose a dropped zone from the
/// denominator.
///
/// # What this guarantees, and what it does not
///
/// * **Guaranteed:** every `Err` return out of this function is preceded by a `skip`/`add` that
///   closes the zone this function's own `begin_zone` opened. The three drop paths are covered by
///   named tests in this file that need no baked assets, so CI runs them. The `Ok` tail return is
///   deliberately NOT closed here — see the next bullet; that is the caller's obligation, and
///   `open_corpus_zone_leaves_a_ready_zone_open_for_the_caller_to_close_807` pins it.
/// * **Guaranteed by [`WaterRollup`], not by this function:** the caller must close a zone this
///   returns `Ok` for, by calling `add` when it finishes scoring it. If it forgets — or `continue`s
///   past it for some later reason — the zone lands in `unaccounted` and
///   [`WaterRollup::is_complete`] is false. The caller cannot lose it silently; it can only make
///   the run fail louder.
/// * **NOT guaranteed:** that a corpus calls this function at all. A corpus can still open a zone
///   with its own inline `let Ok(za) = … else { continue }` and drop the zone silently.
/// * **NOT covered by CI:** this function body. Every test that pins the three drop paths calls
///   [`open_corpus_zone_with`] with injected closures, so the tests cover the accounting and not
///   the path construction for `format!("{zone}.glb")`, `"maps/water"` or the `cell` pass-through.
///
/// `cell` is the collision grid cell size (every current caller passes the production zone-load
/// value, 32.0).
pub fn open_corpus_zone(
    cover: &mut WaterRollup,
    dir: &std::path::Path,
    zone: &str,
    cell: f32,
) -> Result<(crate::collision::Collision, ZoneWater), ZoneDropped> {
    let glb = dir.join(format!("{zone}.glb"));
    let water_dir = dir.join("maps/water");
    open_corpus_zone_with(
        cover,
        zone,
        || eqoxide_assets::ZoneAssets::from_glb(&glb).ok(),
        |za| crate::collision::Collision::build(za, cell),
        || ZoneWater::load(&water_dir, zone),
    )
}

/// [`open_corpus_zone`] with its three IO steps injected.
///
/// This split is not decoration: it is what lets the three drop paths be exercised by ordinary unit
/// tests with **no baked `.glb` and no `.wtr` on disk**, so they are pinned by tests CI actually
/// runs. `open_corpus_zone` is the thin wrapper that supplies the real loaders and contains no
/// accounting of its own.
pub fn open_corpus_zone_with(
    cover: &mut WaterRollup,
    zone: &str,
    load_glb: impl FnOnce() -> Option<eqoxide_assets::ZoneAssets>,
    build: impl FnOnce(&eqoxide_assets::ZoneAssets) -> crate::collision::Collision,
    load_water: impl FnOnce() -> ZoneWater,
) -> Result<(crate::collision::Collision, ZoneWater), ZoneDropped> {
    cover.begin_zone(zone);

    // DROP 1 — no baked `.glb`. The water check never ran, so this is `skip`, not `unmeasured`:
    // claiming the `.wtr` failed would be a second falsehood (nobody asked it).
    let Some(za) = load_glb() else {
        cover.skip(zone, "no glb");
        return Err(ZoneDropped("no glb — skipped".into()));
    };

    // DROP 2 — the glb loaded but carries no collision geometry, so there is nothing to sample.
    let mut col = build(&za);
    if col.cols == 0 {
        cover.skip(zone, "no grid");
        return Err(ZoneDropped("no grid — skipped".into()));
    }

    // DROP 3 — the region data did not load. Now the water check DID run and failed, so this is
    // `unmeasured` (it carries the load error), not `skip`. Every current caller uses water at
    // minimum as its wet-pair filter, and a filter fed a fabricated dry silently scores wet pairs
    // as dry land — #762's exhibit.
    let zw = load_water();
    if let Err(e) = zw.install(&mut col) {
        let why = ZoneDropped(format!("{UNMEASURED} — {e}"));
        cover.add(zone, &zw.tally());
        return Err(why);
    }

    Ok((col, zw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounting_matches_the_design_model() {
        // A grid with 3 single-span columns and 1 two-span column. Payload per single-span column =
        // 4 + 4 + 16 = 24B (both inline slots counted whether used or not, per the inline-2 model);
        // ×2 = 48B. The two-span column is also 24B payload (both spans fit inline). 4 columns ⇒
        // 4×24×2 = 192B. Hand-computed, not read back from the model.
        let mut g = WaterGrid::new([0.0, 0.0], 4.0);
        g.insert(0, 0, WaterColumn { surface_z: -4.0, spans: vec![(-40.0, -6.0)] });
        g.insert(1, 0, WaterColumn { surface_z: -4.0, spans: vec![(-40.0, -6.0)] });
        g.insert(0, 1, WaterColumn { surface_z: -4.0, spans: vec![(-40.0, -6.0)] });
        g.insert(1, 1, WaterColumn { surface_z: -4.0, spans: vec![(-40.0, -16.0), (-9.0, -6.0)] });
        assert_eq!(g.wet_column_count(), 4);
        assert_eq!(g.span_count(), 5);
        assert_eq!(g.estimated_bytes(), 4 * (4 + 4 + 16) * 2);
        // A 3-span column adds one extra span beyond the inline 2 → +8B payload (×2 = +16B).
        g.insert(2, 2, WaterColumn { surface_z: -4.0, spans: vec![(-40.0, -30.0), (-20.0, -15.0), (-9.0, -6.0)] });
        assert_eq!(g.estimated_bytes(), 4 * (4 + 4 + 16) * 2 + (4 + 4 + 16 + 8) * 2);
    }

    #[test]
    fn node_zs_are_the_global_vres_lattice_inside_the_span() {
        // Span [-43.95, -6.0]: nodes are the even (VRES=2) z's in it, high→low. Top = floor(-6/2)*2
        // = -6; bottom-most = -42 (the last even ≥ -43.95). Endpoints -43.95/-6.0 themselves are NOT
        // nodes unless they land on the lattice — this is what keeps every column phase-aligned.
        let c = WaterColumn { surface_z: -4.0, spans: vec![(-43.95, -6.0)] };
        let zs = c.node_zs();
        assert_eq!(zs.first().copied(), Some(-6.0), "top node is the swim-plane lattice point");
        assert_eq!(zs.last().copied(), Some(-42.0), "bottom node is the lowest lattice point ≥ nav_lo");
        assert!(zs.iter().all(|z| (z / VRES).fract().abs() < 1e-4), "every node sits on the VRES lattice");
        assert!(zs.windows(2).all(|w| (w[0] - w[1] - VRES).abs() < 1e-4), "spaced by VRES, high→low");
        assert_eq!(c.top_node_z(), Some(-6.0));
    }

    #[test]
    fn span_containing_admits_only_the_navigable_interior() {
        let c = WaterColumn { surface_z: -4.0, spans: vec![(-44.0, -6.0), (-70.0, -55.0)] };
        assert!(c.span_containing(-24.0).is_some(), "mid-water is interior");
        assert_eq!(c.span_containing(-24.0), Some((-44.0, -6.0)));
        assert!(c.span_containing(-4.0).is_none(), "the surface is ABOVE the swim plane — not a node");
        assert!(c.span_containing(-50.0).is_none(), "the carved gap between spans is solid — not a node");
        assert_eq!(c.span_containing(-60.0), Some((-70.0, -55.0)), "the lower band is its own interior");
    }

    #[test]
    fn nearest_node_z_snaps_to_the_interior_lattice() {
        let c = WaterColumn { surface_z: -4.0, spans: vec![(-44.0, -6.0)] };
        // -23.4 → nearest even lattice point -24.
        assert_eq!(c.nearest_node_z(-23.4), Some(-24.0));
        // Asked deeper than the deepest node → clamps to the deepest node (-44 → -44 is even, in span).
        assert_eq!(c.nearest_node_z(-100.0), Some(-44.0));
    }

    #[test]
    fn node_count_sums_the_lattice() {
        let mut g = WaterGrid::new([0.0, 0.0], 4.0);
        g.insert(0, 0, WaterColumn { surface_z: -4.0, spans: vec![(-10.0, -6.0)] }); // -6,-8,-10 = 3
        g.insert(1, 0, WaterColumn { surface_z: -4.0, spans: vec![(-8.0, -6.0)] });  // -6,-8 = 2
        assert_eq!(g.node_count(), 5);
    }

    #[test]
    fn column_index_and_lookup_align_to_origin() {
        let mut g = WaterGrid::new([-128.0, -384.0], 4.0);
        g.insert(0, 0, WaterColumn { surface_z: -4.0, spans: vec![(-40.0, -6.0)] });
        // Column (0,0) covers east [-128,-124), north [-384,-380).
        assert_eq!(g.column_index(-126.0, -382.0), (0, 0));
        assert!(g.column_at(-126.0, -382.0).is_some());
        assert!(g.column_at(0.0, 0.0).is_none());
    }

    // ──────────────────── #762: UNMEASURED is not the same outcome as measured-zero ────────────────────

    use eqoxide_core::region_map::{RegionLoadError, RegionMap};

    /// A directory that certainly holds no `.wtr` — the "build host was missing the file" case,
    /// reproduced without touching the filesystem.
    fn absent() -> ZoneWater {
        ZoneWater::load(std::path::Path::new("/nonexistent-#762-region-dir"), "halas")
    }

    /// A zone whose region data DID load and genuinely contains no water body: a `.wtr` carrying
    /// only a zone-line region. Every water query over it answers a *true* "dry".
    fn dry_but_loaded() -> ZoneWater {
        ZoneWater::from_map(RegionMap::zone_line_box(-100.0, 100.0, -100.0, 100.0, -10.0, 10.0, 7))
    }

    /// **THE #762 CASE, BOTH DIRECTIONS.** A missing `.wtr` and a loaded-but-waterless `.wtr` must
    /// not produce the same report. On the old `RegionMap::load(..).map(Arc::new)` shape both ended
    /// as `set_water(None)` and both printed `0`.
    ///
    /// MUTATION CHECK: make `ZoneWater::measure` return `WaterMeasurement { value: Some(f(&default)),
    /// reason: None }` in the `Unmeasured` arm (i.e. re-fabricate the old dry answer) and the
    /// `is_measured()`/`to_string()` assertions on `absent()` go RED.
    #[test]
    fn a_missing_wtr_is_unmeasured_not_a_measured_zero_762() {
        let missing = absent();
        let dry = dry_but_loaded();

        // Direction 1: the zone whose data did not load yields NO number at all.
        assert!(!missing.is_measured());
        assert_eq!(missing.reason(), Some(&RegionLoadError::Missing));
        let m = missing.tally();
        assert!(!m.is_measured());
        assert_eq!(m.value(), None, "an unmeasured zone has no count — not a zero");
        assert_eq!(m.to_string(), UNMEASURED, "the report says so in words, not with a digit");

        // Direction 2: the zone that really has no water still reports its legitimate zero.
        assert!(dry.is_measured());
        assert_eq!(dry.reason(), None);
        let d = dry.tally();
        assert!(d.is_measured());
        assert_eq!(d.value(), Some(&0usize), "a loaded, waterless zone measures zero");
        assert_eq!(d.to_string(), "0");
        // And the map is really there and really answers "dry".
        assert!(!dry.map().unwrap().is_water(0.0, 0.0, 0.0));

        // The two outcomes are distinguishable in the output, which is the whole point.
        assert_ne!(m.to_string(), d.to_string());
    }

    /// `measure`'s closure is the gate: it does not run for an unmeasured zone, so a value counted
    /// off a region map cannot exist for a zone that has none. This is what makes "a water number
    /// for a zone whose `.wtr` did not load" unrepresentable rather than merely discouraged.
    #[test]
    fn measure_never_runs_its_closure_without_region_data_762() {
        let mut ran = false;
        let m = absent().measure(|_| { ran = true; 41usize });
        assert!(!ran, "no region map ⇒ no measurement is even attempted");
        assert_eq!(m.value(), None);

        let mut ran2 = false;
        let m2 = dry_but_loaded().measure(|_| { ran2 = true; 41usize });
        assert!(ran2);
        assert_eq!(m2.value(), Some(&41));

        // The accumulate path has the same property: there is nothing to bump.
        let mut acc = absent().tally();
        assert!(acc.value_mut().is_none(), "an unmeasured tally cannot be incremented");
        let mut acc2 = dry_but_loaded().tally();
        *acc2.value_mut().unwrap() += 3;
        assert_eq!(acc2.value(), Some(&3));
    }

    /// A corpus TOTAL summed over a run with a hole in it is the same lie one zone bigger. The
    /// rollup refuses to look complete, names the hole, and says why.
    ///
    /// MUTATION CHECK: make `WaterRollup::add` treat `(None, Some(_))` as `self.total += 0;
    /// self.measured_zones += 1` (the pre-fix behaviour) and `is_complete`/`unmeasured_zones`/the
    /// `Display` assertions go RED.
    #[test]
    fn a_corpus_total_with_an_unmeasured_zone_cannot_look_clean_762() {
        // Two zones scored, one of them never actually looked at — exactly the halas/blackburrow run.
        let zones: Vec<(&str, ZoneWater)> = vec![("qeynos2", dry_but_loaded()), ("halas", absent())];
        let mut roll = WaterRollup::new();
        for (name, zw) in &zones {
            roll.begin_zone(name);
            let mut t = zw.tally();
            if let Some(v) = t.value_mut() { *v += 0; } // the corpus' per-zone water counter
            roll.add(name, &t);
        }
        assert!(!roll.is_complete(), "one zone was never measured — this run has no water result");
        assert_eq!(roll.unmeasured_zones(), vec!["halas"]);
        assert_eq!(roll.measured_zones(), 1);
        let line = roll.to_string();
        assert!(line.contains("INCOMPLETE"), "the TOTAL line itself must not read clean: {line}");
        assert!(line.contains("halas") && line.contains(UNMEASURED), "names the hole: {line}");

        // The all-measured run is allowed to print a plain, trustworthy zero.
        let mut clean = WaterRollup::new();
        clean.begin_zone("qeynos2");
        clean.add("qeynos2", &dry_but_loaded().tally());
        assert!(clean.is_complete());
        assert_eq!(clean.measured_total(), 0);
        assert!(!clean.to_string().contains(UNMEASURED), "{}", clean.to_string());
        assert_ne!(clean.to_string(), line, "clean-zero and holed-zero are different outputs");

        // #831 (item 2): the complete form now carries its OWN marker instead of relying on the
        // reader to infer completeness from the absence of `INCOMPLETE`. Match the em-dash too:
        // `"INCOMPLETE".contains("COMPLETE")` is `true`, so a bare `.contains("COMPLETE")` would pass
        // on the dirty line above as well and prove nothing.
        assert!(clean.to_string().contains("— COMPLETE"),
            "the complete form must mark itself COMPLETE, not just omit INCOMPLETE: {}", clean);
        assert!(!line.contains("— COMPLETE"),
            "a rollup with a hole must never carry the COMPLETE marker: {line}");
    }

    /// **#762 ROUND 2 (B1, blocking): a zone dropped BEFORE the water check ever ran must still
    /// land in the denominator.**
    ///
    /// The round-1 rollup only knew about zones that reached `add`. A corpus zone dropped upstream
    /// of `add` — no baked `.glb`, an empty collision grid — was invisible to the rollup entirely,
    /// so it vanished from BOTH the numerator and the denominator and `(over N/N zones)` came out
    /// true by construction over a smaller corpus than the one requested. Measured on real assets
    /// (round-2 review): a 4-zone run with 3 `.glb`s missing printed `0 (over 1/1 zones)` and passed
    /// green. `skip` is the fix: it gives such a zone a THIRD outcome that still counts toward
    /// [`WaterRollup::attempted_zones`] and still makes [`WaterRollup::is_complete`] false.
    ///
    /// MUTATION CHECK (both directions):
    /// 1. Delete the `self.skipped.len()` term from `attempted_zones` (denominator reverts to
    ///    round-1 shape) → `attempted_zones() == 3` and the "must not equal (over 1/1 zones)"
    ///    assertion below go RED.
    /// 2. Delete the `|| !self.skipped.is_empty()` term from `is_complete` → `!roll.is_complete()`
    ///    goes RED.
    #[test]
    fn a_corpus_total_cannot_hide_a_zone_that_never_reached_the_water_check_762() {
        let mut roll = WaterRollup::new();
        roll.begin_zone("qeynos2");
        roll.add("qeynos2", &dry_but_loaded().tally()); // 1 zone genuinely measured, water == 0
        roll.begin_zone("akanon");
        roll.skip("akanon", "no glb");
        roll.begin_zone("crushbone");
        roll.skip("crushbone", "no grid");

        // The skipped zones are not silently absent: they are named and counted.
        assert_eq!(roll.skipped_zones(), vec!["akanon", "crushbone"]);
        assert_eq!(roll.measured_zones(), 1);
        assert_eq!(roll.unmeasured_zones(), Vec::<&str>::new(), "skipped is not the same bucket as unmeasured");

        // THE DENOMINATOR: 3 zones were named to this rollup, not 1. This is the assertion round-1
        // could not make — `attempted_zones` did not exist, and nothing skipped ever reached `add`.
        assert_eq!(roll.attempted_zones(), 3, "a skipped zone must still count toward the total asked for");

        // A rollup with a skip can never call itself complete, exactly like an unmeasured zone.
        assert!(!roll.is_complete(), "a skipped zone must make the rollup incomplete, same as unmeasured");

        let line = roll.to_string();
        // The round-1 bug's exact printed shape — a clean ratio over only the zones that reached
        // `add` — must not appear. 1 measured zone over a 3-zone request must never read as 1/1.
        assert!(!line.contains("(over 1/1 zones)"),
            "must not read as complete coverage over a corpus smaller than the one requested: {line}");
        assert!(line.contains("INCOMPLETE"), "a rollup with any skip must not read clean: {line}");
        assert!(line.contains("akanon") && line.contains("crushbone"),
            "the TOTAL line must name the skipped zones, not just count them: {line}");
        assert!(line.contains('3'), "the printed denominator must include the skipped zones: {line}");

        // A rollup with ONLY a skip (no unmeasured zone at all) must still refuse to look complete —
        // the two holes are independent triggers, not just additive on top of `unmeasured`.
        let mut only_skip = WaterRollup::new();
        only_skip.begin_zone("qeynos2");
        only_skip.add("qeynos2", &dry_but_loaded().tally());
        only_skip.begin_zone("akanon");
        only_skip.skip("akanon", "no glb");
        assert!(!only_skip.is_complete(), "a skip-only rollup must still be incomplete");
        assert_eq!(only_skip.attempted_zones(), 2);
    }

    /// **#762 ROUND 3 (B1 again, blocking): a zone the loop drops WITHOUT calling `skip` must not
    /// vanish either.**
    ///
    /// Round 2 fixed the denominator by wiring `skip` into the two `continue`s it knew about, and
    /// documented that as "every `continue` … now calls one or the other". It was three, not two.
    /// The unwired one printed the word "skipped" and called nothing, and a reviewer reproduced the
    /// original defect string (`0 (over 1/1 zones)`, green) through it on real assets.
    ///
    /// So the denominator no longer trusts the loop's wiring at all. `begin_zone` opens a zone;
    /// `add`/`skip` close it; **anything that leaves the body without closing it lands in
    /// `unaccounted`.** This test drives that shape directly: zone 2 is opened and abandoned exactly
    /// as an unwired `continue` would abandon it, and zone 4 is abandoned as the LAST iteration
    /// (never followed by another `begin_zone`), which is the case a "close the previous one on the
    /// next open" scheme alone would miss.
    ///
    /// MUTATION CHECKS (each independently turns this RED):
    /// 1. Delete `self.unaccounted.push(prev)` from `begin_zone` → the abandoned zones disappear.
    /// 2. Delete `.chain(self.open.as_deref())` from `unaccounted_zones` → the last, still-open zone
    ///    disappears (the `attempted_zones() == 4` and `is_complete()` assertions).
    /// 3. Delete `+ self.unaccounted_zones().len()` from `attempted_zones` → the denominator reverts.
    /// 4. Delete `&& self.unaccounted_zones().is_empty()` from `is_complete` → the run reads clean.
    #[test]
    fn a_zone_the_loop_drops_without_calling_skip_still_counts_762() {
        let mut roll = WaterRollup::new();

        roll.begin_zone("qeynos2");
        roll.add("qeynos2", &dry_but_loaded().tally()); // measured
        roll.begin_zone("tinyzone");
        /* the unwired `continue`: no add, no skip */
        roll.begin_zone("akanon");
        roll.skip("akanon", "no glb"); // correctly wired
        roll.begin_zone("lastzone");
        /* unwired AND last — nothing ever opens after it */

        assert_eq!(roll.unaccounted_zones(), vec!["tinyzone", "lastzone"],
            "a zone opened and abandoned is a named hole, not an absence");
        assert_eq!(roll.measured_zones(), 1);
        assert_eq!(roll.skipped_zones(), vec!["akanon"]);
        assert_eq!(roll.unmeasured_zones(), Vec::<&str>::new(),
            "unaccounted is its own bucket: nothing failed to LOAD here");

        // The denominator is the four zones the loop actually iterated.
        assert_eq!(roll.attempted_zones(), 4,
            "a dropped-without-accounting zone must still count toward the total asked for");
        assert!(!roll.is_complete(), "a run that lost two zones has no water result");

        let line = roll.to_string();
        assert!(!line.contains("(over 1/1 zones)"),
            "the round-1/round-3 defect string must not be producible: {line}");
        assert!(line.contains("INCOMPLETE"), "{line}");
        assert!(line.contains("tinyzone") && line.contains("lastzone"),
            "the TOTAL line must name the zones it lost: {line}");

        // An UNACCOUNTED hole ALONE — no skip, no unmeasured zone anywhere in the run — must be
        // enough on its own. This case is not decoration: with only the mixed rollup above, the
        // `skipped` bucket masks mutation 4 (measured — deleting the `unaccounted_zones().is_empty()`
        // term from `is_complete` left every other assertion in this test green, 15 passed / 0
        // failed). The three holes must be independent triggers, not additive on top of each other.
        let mut only_unaccounted = WaterRollup::new();
        only_unaccounted.begin_zone("qeynos2");
        only_unaccounted.add("qeynos2", &dry_but_loaded().tally());
        only_unaccounted.begin_zone("tinyzone"); // dropped, and nothing opens after it
        assert_eq!(only_unaccounted.unaccounted_zones(), vec!["tinyzone"]);
        assert_eq!(only_unaccounted.skipped_zones(), Vec::<&str>::new());
        assert_eq!(only_unaccounted.unmeasured_zones(), Vec::<&str>::new());
        assert_eq!(only_unaccounted.attempted_zones(), 2);
        assert!(!only_unaccounted.is_complete(),
            "an unaccounted zone with no other hole must still make the run incomplete");
        let solo = only_unaccounted.to_string();
        assert!(solo.contains("INCOMPLETE") && solo.contains("tinyzone"), "{solo}");
        assert!(!solo.contains("(over 1/1 zones)"),
            "the defect string must not be producible from an unaccounted-only run: {solo}");
    }

    /// The wiring is fail-CLOSED: a corpus loop that forgets `begin_zone` cannot silently revert to
    /// the round-1 shape, because there is then no open zone for `add` to close.
    #[test]
    #[should_panic(expected = "with no zone open")]
    fn add_without_begin_zone_panics_rather_than_silently_reverting_762() {
        let mut roll = WaterRollup::new();
        roll.add("qeynos2", &dry_but_loaded().tally());
    }

    /// Same for `skip` — both closers enforce the protocol, not just one.
    #[test]
    #[should_panic(expected = "with no zone open")]
    fn skip_without_begin_zone_panics_too_762() {
        let mut roll = WaterRollup::new();
        roll.skip("qeynos2", "no glb");
    }

    /// Review N-R2c: a rollup that was never told about any zone must not answer "complete". A type
    /// whose job is refusing to look clean must not have a `Default` value that looks clean.
    #[test]
    fn an_empty_rollup_is_not_a_complete_result_762() {
        let empty = WaterRollup::new();
        assert_eq!(empty.attempted_zones(), 0);
        assert!(!empty.is_complete(), "zero zones is not a water result");
        let line = empty.to_string();
        assert!(line.contains("INCOMPLETE"), "an empty rollup must not print a clean total: {line}");
        assert!(!line.contains("(over 0/0 zones)"),
            "the round-2 clean-looking empty string must not be producible: {line}");
    }

    // ───────── #898: the COMPOSITE report's own honesty, pinned without any baked asset ─────────

    /// The exact divergence #898 is about, built by hand: `wat-route` closes clean while `#423`
    /// holds an unaccounted hole for the SAME zone.
    fn diverged_pair_898() -> (WaterRollup, WaterRollup) {
        let (mut wr, mut r423) = (WaterRollup::new(), WaterRollup::new());
        wr.begin_zone("zone_a");
        wr.add("zone_a", &dry_but_loaded().measure(|_| 0usize)); // clean, and its total is 0
        r423.begin_zone("zone_a"); // opened and abandoned — the hole
        (wr, r423)
    }

    /// **#898 — the marker table, asserted rather than described.** Every honesty marker in this
    /// module has to survive a naive `line.contains(..)` against every OTHER marker, in both
    /// directions. `"INCOMPLETE".contains("COMPLETE")` is the original trap (#831) and
    /// `"NOT-ALL-COMPLETE".contains("ALL-COMPLETE")` is the same trap one level up, which is why
    /// the em-dash is part of all four strings and not decoration.
    ///
    /// MUTATION CHECKS (each independently turns this RED):
    /// 1. Drop the `— ` from `COMPOSITE_CLEAN` → `COMPOSITE_DIRTY` now contains it.
    /// 2. Spell `COMPOSITE_CLEAN` as `"— COMPLETE"` → it collides with the per-rollup marker.
    /// 3. Shrink `markers` to a single element → the pair-count control fires: a table that
    ///    compares nothing reports "no collisions" exactly as a clean one does.
    /// 4. Stop [`RollupReport`]'s `Display` emitting [`COMPOSITE_CLEAN`] → the reach control fires.
    ///
    /// Mutant 4 did NOT redden this test as FIRST WRITTEN in this PR, and that was the defect: the
    /// reach control then covered only the two per-rollup markers, so the two `COMPOSITE_*` strings
    /// were table entries with no emitter behind them, and `every_corpus_marker_is_line_safe_…` in
    /// `src/movement.rs` cited this test for a pinning it did not perform. Proved by execution —
    /// under that mutant this test stayed green and only `a_clean_composite_report_states_it_898`
    /// reddened. The reach control below now renders a real report over each rollup, so the
    /// citation is true. (PR #1052 review round 1, R1.)
    #[test]
    fn composite_markers_are_line_safe_against_every_other_898() {
        const ROLLUP_CLEAN: &str = "— COMPLETE";
        const ROLLUP_DIRTY: &str = "— INCOMPLETE";
        // REACH CONTROL, half one: all FOUR markers in the table below are strings real values
        // REALLY print — otherwise this table is a cross-product over strings nothing emits.
        let mut clean = WaterRollup::new();
        clean.begin_zone("z"); clean.add("z", &dry_but_loaded().measure(|_| 1usize));
        let mut dirty = WaterRollup::new();
        dirty.begin_zone("z"); // abandoned
        assert!(clean.to_string().contains(ROLLUP_CLEAN), "{clean}");
        assert!(dirty.to_string().contains(ROLLUP_DIRTY), "{dirty}");
        let clean_report = RollupReport::new([("wat-route", &clean)]).to_string();
        let dirty_report = RollupReport::new([("wat-route", &dirty)]).to_string();
        assert!(clean_report.contains(COMPOSITE_CLEAN), "{clean_report}");
        assert!(dirty_report.contains(COMPOSITE_DIRTY), "{dirty_report}");

        let markers = [ROLLUP_CLEAN, ROLLUP_DIRTY, COMPOSITE_CLEAN, COMPOSITE_DIRTY];
        let mut pairs = 0usize;
        for (i, a) in markers.iter().enumerate() {
            for (j, b) in markers.iter().enumerate() {
                if i == j { continue }
                pairs += 1;
                assert!(!a.contains(b),
                    "marker {a:?} contains marker {b:?} as a substring, so a grep for {b:?} matches \
                     a line that only carries {a:?} — that is #831's `INCOMPLETE`/`COMPLETE` trap, \
                     and the whole point of the leading em-dash");
            }
        }
        // REACH CONTROL, half two: the CORPUS is an item. An exceptions-only scan cannot tell
        // "no collisions" from "nothing compared", which is #927's complaint one file over.
        assert_eq!((markers.len(), pairs), (4, 12),
            "the table must compare every ordered pair of all four markers this module prints; \
             a shrunken array reports a clean scan over a corpus it never looked at");
    }

    /// **#898 — a two-rollup report cannot put a per-rollup `— COMPLETE` on a line that is not
    /// itself clean.** The report under test is the diverged one: one rollup genuinely complete
    /// (and printing the historical defect string `(over 1/1 zones)` while it is at it), the other
    /// holding the hole.
    ///
    /// Four properties, and the first is a REACH CONTROL: if the clean rollup stopped printing
    /// `— COMPLETE` at all the remaining three would pass vacuously, and this test would be
    /// measuring nothing.
    ///
    /// MUTATION CHECKS (each independently turns this RED):
    /// 1. Render the rollups on ONE line (drop the per-rollup `writeln!` from `Display`) →
    ///    property 2 fails: one line carries both per-rollup markers.
    /// 2. Delete the verdict line from `Display` → property 3 fails.
    ///
    /// A third mutation — drop the `!self.rolls.is_empty()` term from `all_complete` — was listed
    /// here as turning this test RED. Measured: it does NOT. This test's report holds two rollups,
    /// so the non-empty term never decides anything here; the mutant is caught by
    /// `an_empty_composite_report_is_not_clean_898` below, which is where the claim now lives.
    #[test]
    fn a_composite_report_keeps_every_marker_line_true_898() {
        let (wr, r423) = diverged_pair_898();
        let report = RollupReport::new([("wat-route", &wr), ("#423", &r423)]);
        let text = report.to_string();
        let lines: Vec<&str> = text.lines().collect();

        // 1. REACH CONTROL. The clean rollup's own marker — and the historical defect string it
        //    prints beside it — must really be in this text, or nothing below is measuring the
        //    rendering.
        assert!(text.contains("— COMPLETE"),
            "reach control: the clean rollup's own marker is absent, so the properties below are \
             vacuous.\n{text}");
        assert!(text.contains("(over 1/1 zones)"),
            "reach control: #898's exhibit is `— COMPLETE` printed beside `0 (over 1/1 zones)`, and \
             this fixture no longer produces that string.\n{text}");

        // 2. THE FIX, stated as the audit a reader would actually run: a naive line-level grep for
        //    the clean marker lands on exactly ONE line, and that line carries nothing else that a
        //    grep could confuse it with.
        //
        //    `— COMPLETE` identifies the per-rollup clean marker ON ITS OWN — `"— INCOMPLETE"`,
        //    `"— ALL-COMPLETE"` and `"— NOT-ALL-COMPLETE"` all fail to contain it, because the
        //    em-dash-and-space is the disambiguator. That is not assumed here: it is the whole
        //    subject of `composite_markers_are_line_safe_against_every_other_898` above, which
        //    asserts all twelve ordered pairs.
        //
        //    do not gate this check on a per-rollup predicate. A one-line rendering puts both
        //    per-rollup markers on one line, so a gated check would skip the state it exists for.
        let clean_lines: Vec<&&str> = lines.iter().filter(|l| l.contains("— COMPLETE")).collect();
        assert_eq!(clean_lines.len(), 1,
            "a line-level grep for the clean marker must land on exactly one line — the rollup that \
             really is clean. Got {clean_lines:?}\n{text}");
        for line in &clean_lines {
            assert!(!line.contains("— INCOMPLETE"),
                "one line carries BOTH per-rollup markers — a line-level grep for either is a \
                 coin flip: {line:?}\n{text}");
            assert!(!line.contains(COMPOSITE_CLEAN) && !line.contains(COMPOSITE_DIRTY),
                "a per-rollup `— COMPLETE` shares a line with the report's own verdict, so the two \
                 claims are indistinguishable by line: {line:?}\n{text}");
        }

        // 3. The report states its own verdict, on exactly one line, and that verdict is DIRTY.
        let verdicts: Vec<&&str> = lines.iter()
            .filter(|l| l.contains(COMPOSITE_CLEAN) || l.contains(COMPOSITE_DIRTY)).collect();
        assert_eq!(verdicts.len(), 1, "exactly one verdict line, got {verdicts:?}\n{text}");
        assert!(verdicts[0].contains(COMPOSITE_DIRTY) && !verdicts[0].contains(COMPOSITE_CLEAN),
            "the report holds an incomplete rollup, so its verdict must be {COMPOSITE_DIRTY}: \
             {:?}", verdicts[0]);
        assert!(!report.all_complete() && report.marker() == COMPOSITE_DIRTY);
        assert_eq!(report.incomplete_labels(), vec!["#423"],
            "the verdict must NAME which half is dirty, not merely say one is");
        assert!(verdicts[0].contains("#423"), "…and print it: {:?}", verdicts[0]);

        // 4. …and the one line that grep lands on is the CLEAN rollup's, not the other one's.
        assert!(clean_lines[0].contains("wat-route"),
            "the only `— COMPLETE` line must be the clean rollup's own: {:?}", clean_lines[0]);
    }

    /// **#898 — a report over zero rollups is not a clean report.** Same refusal as
    /// `an_empty_rollup_is_not_a_complete_result_762`, one level up: a composite that has been told
    /// about nothing has looked at nothing, and must not print the marker a reader greps for.
    ///
    /// MUTATION CHECK: drop the `!self.rolls.is_empty()` term from `RollupReport::all_complete`
    /// → this test goes RED on its first assertion (`zero rollups is not a clean report`). Measured
    /// over the four `_898` tests in this file, it is the only one that reddens: the other three
    /// build reports that HOLD rollups, so the non-empty term never decides anything for them.
    #[test]
    fn an_empty_composite_report_is_not_clean_898() {
        let nothing: [(&str, &WaterRollup); 0] = [];
        let report = RollupReport::new(nothing);
        assert!(!report.all_complete(), "zero rollups is not a clean report");
        assert_eq!(report.marker(), COMPOSITE_DIRTY);
        let text = report.to_string();
        assert!(!text.contains(COMPOSITE_CLEAN),
            "an empty report must not print the clean marker: {text}");
        assert!(text.contains(COMPOSITE_DIRTY) && text.contains("no rollup ever reached"), "{text}");
    }

    /// **#898 — and a report whose rollups are ALL complete says so, on its own line.** Without
    /// this the type could satisfy every assertion above by never printing [`COMPOSITE_CLEAN`] at
    /// all, which would make the marker useless rather than safe.
    #[test]
    fn a_clean_composite_report_states_it_898() {
        let (mut a, mut b) = (WaterRollup::new(), WaterRollup::new());
        a.begin_zone("z"); a.add("z", &dry_but_loaded().measure(|_| 3usize));
        b.begin_zone("z"); b.add("z", &dry_but_loaded().measure(|_| 0usize));
        let report = RollupReport::new([("wat-route", &a), ("#423", &b)]);
        assert!(report.all_complete() && report.marker() == COMPOSITE_CLEAN);
        assert!(report.incomplete_labels().is_empty());
        let text = report.to_string();
        let verdicts: Vec<&str> = text.lines()
            .filter(|l| l.contains(COMPOSITE_CLEAN) || l.contains(COMPOSITE_DIRTY)).collect();
        assert_eq!(verdicts.len(), 1, "{text}");
        assert!(verdicts[0].contains(COMPOSITE_CLEAN) && !verdicts[0].contains(COMPOSITE_DIRTY),
            "{:?}", verdicts[0]);
    }

    /// Installing an unmeasured zone onto a collision grid is an error the caller must handle: a
    /// scorer that ignored the `Result` would read fabricated dry answers out of it.
    /// `#[must_use]` makes ignoring it a compiler warning.
    ///
    /// **#821 review round 2, N1.** Since #803 `install` also RECORDS the reason on the grid, so the
    /// grid itself can refuse instead of answering `[]`. That behaviour change was stated in
    /// `install`'s comment ("BOTH arms now write the grid") and pinned by nothing: the reviewer
    /// severed the `Unmeasured` arm's `col.set_region_data(Err(e.clone()))` and three crates stayed
    /// green. The before/after assertions below are the missing pin — before the install the grid is
    /// `NotAttached` (nobody asked), after it is `LoadFailed(Missing)` (we asked and the file was
    /// not there), and those are DIFFERENT states with different `reason` strings on the wire.
    #[test]
    fn installing_an_unmeasured_zone_is_a_handled_failure_762() {
        use crate::collision::Collision;
        use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};
        let floor = MeshData {
            positions: vec![[-60.0, 0.0, -60.0], [60.0, 0.0, -60.0], [60.0, 0.0, 60.0], [-60.0, 0.0, 60.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 2, 1, 0, 3, 2], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        };
        let mut col = Collision::build(
            &ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 32.0);

        // Nobody has installed anything yet, so the grid says exactly that — NOT "the file failed".
        assert_eq!(col.region_data_absent().map(|a| a.as_str()), Some("region_data_not_attached"),
            "a freshly built grid has had no region data handed to it");

        let err = absent().install(&mut col).unwrap_err().clone();
        assert_eq!(err, RegionLoadError::Missing);
        assert!(!col.in_water([0.0, 0.0, 1.0]),
            "the grid is dry — but that dry is FABRICATED, which is why install() reported Err");
        // …and the GRID now carries the reason too (#803), so a later reader that never sees this
        // `Result` still cannot mistake the failure for an answer. N1: severing the `Unmeasured`
        // arm's `set_region_data` leaves the grid on `region_data_not_attached` and both of these
        // go RED.
        assert_eq!(col.region_data_absent().map(|a| a.as_str()), Some("region_data_missing"),
            "the Unmeasured arm must RECORD why, not leave the grid looking un-asked");
        assert!(matches!(col.zone_line_indices(), Err(ref a) if a.as_str() == "region_data_missing"),
            "…so the exits question refuses with the real cause instead of answering `[]`: {:?}",
            col.zone_line_indices());

        dry_but_loaded().install(&mut col).expect("a loaded map installs");
        assert!(!col.in_water([0.0, 0.0, 1.0]), "loaded and genuinely dry");
        // The Measured arm's write clears the recorded failure rather than leaving it latched.
        assert_eq!(col.region_data_absent().map(|a| a.as_str()), None, "a loaded map is present");
        assert_eq!(col.zone_line_indices().as_deref(), Ok(&[7][..]),
            "and the loaded map's zone line is enumerable");
        ZoneWater::from_map(RegionMap::water_slab(-40.0, 0.0)).install(&mut col).expect("installs");
        assert!(col.in_water([0.0, 0.0, -10.0]), "a real water map really answers wet");
    }

    // ───────── #807: the corpus prologue's three drop paths, pinned WITHOUT baked assets ─────────

    /// A zone whose glb carries one big floor plate — enough that `Collision::build` produces a
    /// non-empty grid, so `open_corpus_zone_with` gets past DROP 2 and reaches the water check.
    fn floor_assets() -> eqoxide_assets::ZoneAssets {
        use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};
        ZoneAssets {
            terrain: vec![MeshData {
                positions: vec![[-60.0, 0.0, -60.0], [60.0, 0.0, -60.0], [60.0, 0.0, 60.0], [-60.0, 0.0, 60.0]],
                normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
                indices: vec![0, 2, 1, 0, 3, 2], texture_name: None, base_color: [1.0; 4],
                center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
            }],
            objects: vec![], textures: vec![],
        }
    }

    /// A glb that parses but has no geometry at all — the DROP 2 condition, reproduced without a
    /// file. Pinned here rather than assumed: `Collision::build` on empty terrain must really give
    /// `cols == 0`, or the "no grid" arm below would be testing nothing.
    fn empty_assets() -> eqoxide_assets::ZoneAssets {
        eqoxide_assets::ZoneAssets { terrain: vec![], objects: vec![], textures: vec![] }
    }

    fn build32(za: &eqoxide_assets::ZoneAssets) -> crate::collision::Collision {
        crate::collision::Collision::build(za, 32.0)
    }

    /// **DROP 1 (#807).** A corpus zone with no baked `.glb` must land in the rollup's `skipped`
    /// bucket and make the run incomplete — not vanish from the denominator, which is what the
    /// five unconverted corpora did (their `else { println!("(no glb — skipped)"); continue }`
    /// touched no ledger at all, so their `TOTAL …` line covered a smaller corpus than it named).
    ///
    /// MUTATION CHECK: delete `cover.skip(zone, "no glb")` from `open_corpus_zone_with`'s DROP 1
    /// arm → the zone becomes `unaccounted` instead of `skipped` and the `skipped_zones` assertion
    /// goes RED. (It stays incomplete either way — `unaccounted` is the fail-closed backstop — so
    /// the bucket assertion, not `is_complete`, is what discriminates.)
    #[test]
    fn open_corpus_zone_records_a_missing_glb_as_skipped_807() {
        let mut cover = WaterRollup::new();
        let out = open_corpus_zone_with(&mut cover, "halas",
            || None, build32, dry_but_loaded);
        assert_eq!(out.err(), Some(ZoneDropped("no glb — skipped".into())));
        assert_eq!(cover.skipped_zones(), vec!["halas"]);
        assert_eq!(cover.unaccounted_zones(), Vec::<&str>::new(),
            "the drop path accounted for itself; nothing should have been left open");
        assert_eq!(cover.attempted_zones(), 1, "the zone is still in the denominator");
        assert!(!cover.is_complete());
    }

    /// **DROP 2 (#807).** A glb that loads but yields an empty collision grid is the same shape:
    /// the water check never runs, so the honest outcome is `skipped`, not a measured zero.
    ///
    /// MUTATION CHECK: delete `cover.skip(zone, "no grid")` from DROP 2 → `skipped_zones` goes RED
    /// (the zone shows up as `unaccounted`).
    #[test]
    fn open_corpus_zone_records_an_empty_collision_grid_as_skipped_807() {
        // The premise this arm rests on, asserted rather than assumed.
        assert_eq!(build32(&empty_assets()).cols, 0, "empty terrain must really build an empty grid");
        assert!(build32(&floor_assets()).cols > 0, "…and a floor plate must really build a non-empty one");

        let mut cover = WaterRollup::new();
        let out = open_corpus_zone_with(&mut cover, "halas",
            || Some(empty_assets()), build32, dry_but_loaded);
        assert_eq!(out.err(), Some(ZoneDropped("no grid — skipped".into())));
        assert_eq!(cover.skipped_zones(), vec!["halas"]);
        assert_eq!(cover.unaccounted_zones(), Vec::<&str>::new());
        assert!(!cover.is_complete());
    }

    /// **DROP 3 (#807 / #762).** A zone whose `.wtr` did not load is `unmeasured` — a DIFFERENT
    /// bucket from `skipped`, because here the water check really did run and really did fail. The
    /// distinction is the whole of #762: `skipped` says "nobody asked this zone's water"; a
    /// measured `0` says "we asked and there is none".
    ///
    /// MUTATION CHECK: delete `cover.add(zone, &zw.tally())` from DROP 3 → `unmeasured_zones` goes
    /// RED (the zone shows up as `unaccounted`). Replace it with `cover.skip(zone, "no wtr")` →
    /// `unmeasured_zones` goes RED too, and the two failure kinds are conflated again.
    #[test]
    fn open_corpus_zone_records_an_unloadable_wtr_as_unmeasured_807() {
        let mut cover = WaterRollup::new();
        let out = open_corpus_zone_with(&mut cover, "halas",
            || Some(floor_assets()), build32, absent);
        let why = out.err().expect("an unloadable .wtr must drop the zone");
        assert!(why.to_string().starts_with(UNMEASURED), "{why}");
        assert_eq!(cover.unmeasured_zones(), vec!["halas"]);
        assert_eq!(cover.skipped_zones(), Vec::<&str>::new(),
            "a .wtr that was read and failed is not the same outcome as one nobody read");
        assert_eq!(cover.unaccounted_zones(), Vec::<&str>::new());
        assert!(!cover.is_complete());
    }

    /// The success path leaves the zone **open**, so closing it is the caller's job and forgetting
    /// is loud. This is the property that makes the corpus body's own later `continue`s — including
    /// ones nobody has written yet — fail the run instead of shrinking it silently.
    #[test]
    fn open_corpus_zone_leaves_a_ready_zone_open_for_the_caller_to_close_807() {
        let mut cover = WaterRollup::new();
        let (col, zw) = open_corpus_zone_with(&mut cover, "gfaydark",
            || Some(floor_assets()), build32, dry_but_loaded)
            .expect("a zone with a grid and loadable region data is ready to score");
        assert!(col.cols > 0);
        assert!(zw.is_measured(), "the caller gets the region data back so it can fold a real number");

        // Still open: the run is NOT complete until the caller closes it.
        assert_eq!(cover.unaccounted_zones(), vec!["gfaydark"]);
        assert!(!cover.is_complete(), "an opened-but-unclosed zone is a hole, not a pass");

        cover.add("gfaydark", &zw.measure(|_| 3usize));
        assert!(cover.is_complete(), "…and closing it is what makes the run a result");
        assert_eq!(cover.measured_total(), 3);
        assert_eq!(cover.unaccounted_zones(), Vec::<&str>::new());
    }

    /// The corpus-shaped end-to-end: a mixed run over four zones, one per outcome, driven through
    /// the real prologue. The two things a corpus publishes — its denominator and its refusal —
    /// must both survive a run in which three of four zones fell down a different drop path.
    #[test]
    fn a_corpus_using_the_prologue_cannot_publish_a_total_over_a_shrunken_zone_list_807() {
        let mut cover = WaterRollup::new();
        for (zone, glb, water) in [
            ("crushbone", Some(floor_assets()), true),  // measured
            ("akanon",    None,                 true),  // no glb        → skipped
            ("qeynos2",   Some(empty_assets()), true),  // no grid       → skipped
            ("halas",     Some(floor_assets()), false), // .wtr missing  → unmeasured
        ] {
            let loader = || if water { dry_but_loaded() } else { absent() };
            let Ok((_col, zw)) = open_corpus_zone_with(&mut cover, zone, || glb, build32, loader)
                else { continue };
            cover.add(zone, &zw.measure(|_| 1usize));
        }
        assert_eq!(cover.measured_zones(), 1);
        assert_eq!(cover.attempted_zones(), 4, "all four zones asked for are in the denominator");
        assert_eq!(cover.skipped_zones(), vec!["akanon", "qeynos2"]);
        assert_eq!(cover.unmeasured_zones(), vec!["halas"]);
        assert_eq!(cover.unaccounted_zones(), Vec::<&str>::new(),
            "no drop path left a zone open — that is what the prologue is for");
        assert!(!cover.is_complete(), "3 of 4 zones were never scored: this run has no total");
        let line = cover.to_string();
        assert!(!line.contains("(over 1/1 zones)"),
            "the #762/#807 defect string — a clean ratio over the zones that happened to survive: {line}");
        assert!(line.contains("INCOMPLETE") && line.contains("akanon") && line.contains("halas"), "{line}");
    }
}
