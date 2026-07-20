//! The sparse **water-span grid** (3D-water-volume navigation design §5, Slice 1).
//!
//! A per-zone, WATER-ONLY structure that gives the planner a vocabulary for the *interior* of a
//! water volume — the states a swimmer can actually hold — without the memory trap of a whole-zone
//! 3D voxel grid (design §4.2: an everfrost-scale 2u voxelization is ~10⁹ nodes, 128× `MAX_NODES`).
//!
//! **This module is PURELY ADDITIVE (Slice 1).** Nothing in the A* search, the walker, or the
//! steering reads it yet — wiring the water-node generator into `astar`/`find_path` is Slice 2, and
//! 3D execution is Slice 3 (design §11). Building this grid changes no existing nav behaviour; it is
//! constructed on demand via [`crate::collision::Collision::build_water_grid`].
//!
//! ## Representation (design §5.1)
//!
//! * A sparse map keyed by a **4u XY column lattice** aligned to the collision grid origin (each 8u
//!   coarse nav cell = 2×2 water columns). Only wet columns are stored; dry land stores nothing.
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
}
