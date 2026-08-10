//! Walker-sim integration tests — the nav+movement boundary (#544 Step 2f).
//!
//! These tests step the REAL app-layer `CharacterController` (`eqoxide::movement`) along a route the
//! `eqoxide-nav` planner produced, proving that every route the planner ADMITS the controller can
//! actually WALK (and, for the swim tests, dive-and-hold at depth). Because they depend on the
//! controller — which lives in the app crate, ABOVE `eqoxide-nav` — they cannot live inside the nav
//! crate without creating a dependency cycle. They were relocated here VERBATIM from the nav crate's
//! `collision.rs` / `traversability.rs` `#[cfg(test)]` modules; only the module paths changed
//! (`crate::…` → `eqoxide::…` / `eqoxide_core::…`). The mutation-checked assertions (#359 haul-out,
//! #547 depth-hold, #386 lintel, #420 low-wall) are unchanged and remain load-bearing for nav
//! correctness.

use eqoxide::movement::CharacterController;
use eqoxide::nav::collision::{Collision, LocalOutcome, PlanCtx, PlanOutcome};
use eqoxide::nav::steering::{carrot_along, carrot_along_los, fast_steer_aim, swim_vspeed};
// Production tuning constants, IMPORTED rather than copied. Every name here was a private
// `const … = <literal>;` inside a test fn until #733's review found `LOCAL_REACH` still copied three
// lines under a comment promising the opposite; the rest came out of the same grep. Values were
// identical at the time of the change, so this is a drift-proofing, not a behaviour change.
use eqoxide::nav::steering::{
    LOCAL_CELL, LOCAL_REACH, NAV_BACKOFF_TICKS, NAV_HOP_TICKS, NAV_LOCAL_STUCK_TICKS,
    NAV_STUCK_TICKS, REPLAN_COOLDOWN_TICKS,
};
use eqoxide::traversability::{Point, Traversability, PLAYER_BODY};
use eqoxide::assets::{MeshData, RenderMode, ZoneAssets};
use eqoxide::region_map::RegionMap;
// #762: water/region data is carried as a MEASURED-or-UNMEASURED value, never as an `Option` a
// corpus can silently read as "this zone has no water".
use eqoxide::nav::water_grid::{open_corpus_zone, WaterRollup, ZoneWater, UNMEASURED};
use eqoxide_core::physics::{PLAYER_RADIUS, RUN_SPEED};
use eqoxide_ipc::MoveIntent;

// ── helpers, copied verbatim from the nav crate's test modules (they also still serve the nav unit
//    tests that stayed behind; duplicated here because integration tests cannot see `#[cfg(test)]`
//    items of a dependency) ──

    // from `collision.rs` mod tests:
    fn slab(z: f32, n0: f32, n1: f32, e0: f32, e1: f32, up: bool) -> MeshData {
        MeshData {
            positions: vec![[n0, z, e0], [n0, z, e1], [n1, z, e1], [n1, z, e0]],
            normals: vec![], uvs: vec![],
            indices: if up { vec![0, 1, 2, 0, 2, 3] } else { vec![0, 2, 1, 0, 3, 2] },
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        }
    }
    fn wall_east(e: f32, h0: f32, h1: f32) -> MeshData {
        MeshData {
            positions: vec![[-100.0, h0, e], [100.0, h0, e], [100.0, h1, e], [-100.0, h1, e]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    // from `traversability.rs` mod tests:
    fn mesh(positions: Vec<[f32; 3]>) -> MeshData {
        MeshData {
            positions,
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0; 3],
            render_mode: RenderMode::Opaque,
            anim: None,
        }
    }
    /// Floor at height `z` over east [e0,e1] × north [n0,n1]. libeq pos = [north, height, east].
    fn floor_at(z: f32, e0: f32, e1: f32, n0: f32, n1: f32) -> MeshData {
        mesh(vec![[n0, z, e0], [n1, z, e0], [n1, z, e1], [n0, z, e1]])
    }
    /// Vertical east-facing panel at east=`e`, north [n0,n1], height [h0,h1].
    fn panel(e: f32, n0: f32, n1: f32, h0: f32, h1: f32) -> MeshData {
        mesh(vec![[n0, h0, e], [n1, h0, e], [n1, h1, e], [n0, h1, e]])
    }
    fn col(meshes: Vec<MeshData>) -> Collision {
        Collision::build(&ZoneAssets { terrain: meshes, objects: vec![], textures: vec![] }, 32.0)
    }
    fn lintel_corridor() -> Collision {
        col(vec![
            floor_at(0.0, -40.0, 40.0, -8.0, 8.0),
            panel(0.0, -8.0, 8.0, 3.5, 6.5), // the lintel, sealing the corridor at chest height
            // side walls so no detour exists — the ONLY way east is under the lintel
            mesh(vec![[-8.0, 0.0, -40.0], [-8.0, 10.0, -40.0], [-8.0, 10.0, 40.0], [-8.0, 0.0, 40.0]]),
            mesh(vec![[8.0, 0.0, -40.0], [8.0, 10.0, -40.0], [8.0, 10.0, 40.0], [8.0, 0.0, 40.0]]),
        ])
    }
    fn low_wall_corridor() -> Collision {
        col(vec![
            floor_at(0.0, -40.0, 40.0, -8.0, 8.0),
            panel(0.0, -8.0, 8.0, 0.0, 3.0), // 3u wall: above the 2.5u step reach, below the 4.0u chest
            mesh(vec![[-8.0, 0.0, -40.0], [-8.0, 10.0, -40.0], [-8.0, 10.0, 40.0], [-8.0, 0.0, 40.0]]),
            mesh(vec![[8.0, 0.0, -40.0], [8.0, 10.0, -40.0], [8.0, 10.0, 40.0], [8.0, 0.0, 40.0]]),
        ])
    }

// ── relocated collision.rs walker-sim tests ──

    /// P1 — the #359 drift-apart property (THE HAUL-OUT CONTRACT, water design §9 gate): sweep the
    /// exit-ledge height `h` above the water surface in 0.25 u steps over `[0, 2 × haul_out_up]`
    /// and pin, for every `h`, that
    ///     planner admits the water→land exit  ⟺  h ≤ PLAYER_BODY.haul_out_up
    /// AND that every ADMITTED exit is actually EXECUTABLE by the real `CharacterController`,
    /// driven exactly the way the nav walker drives a swim leg (start floating at
    /// `surface − float_depth`, swim-up wish when the waypoint is above, the swimming step-up at
    /// the lip). A planner-legal haul-out the controller cannot climb is the #359 wedge (the
    /// character bobbed at the waterline forever); a refused exit at `h ≤ haul_out_up` is the
    /// false-`no_path` the exact-surface sizing prevents. The two sides must never disagree.
    ///
    /// (The controller deliberately keeps ~0.5 u of capability margin ABOVE the cap
    /// (`STEP_UP + GROUND_SNAP_TOL = 2.5` vs `haul_out_up = 2.0`, design §4c E3) — the planner may
    /// only under-promise, never over-promise, so the property tested is admission ⟹ execution
    /// plus the exact admission boundary, not capability ⟺ admission.)
    #[test]
    fn p1_haul_out_admission_matches_controller_execution() {
        let mesh = |positions: Vec<[f32; 3]>| MeshData {
            positions, normals: vec![], uvs: vec![],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let body = &PLAYER_BODY;
        let surf = 9.0_f32;
        // Pit floor z=0 (east 0..24), cliff face at east=24 up to the bank lip, bank at
        // z = surf + h (east 24..48). EQ WLD pos = [north, height, east]. Water is a SLAB
        // 0.5..surf — bounded below like real `.wtr` volumes — so there is no water beneath the
        // pit floor and the surface-traversal edge cannot open a side door: admission is decided
        // by the WATER ASCENT haul-out edge alone.
        let scene = |bank_z: f32| {
            let pit_floor = mesh(vec![[0.0, 0.0, 0.0], [0.0, 0.0, 24.0], [24.0, 0.0, 24.0], [24.0, 0.0, 0.0]]);
            let cliff = mesh(vec![[0.0, 0.0, 24.0], [24.0, 0.0, 24.0], [24.0, bank_z, 24.0], [0.0, bank_z, 24.0]]);
            let bank = mesh(vec![[0.0, bank_z, 24.0], [0.0, bank_z, 48.0], [24.0, bank_z, 48.0], [24.0, bank_z, 24.0]]);
            ZoneAssets { terrain: vec![pit_floor, cliff, bank], objects: vec![], textures: vec![] }
        };
        let mut h = 0.0_f32;
        while h <= 2.0 * body.haul_out_up + 1e-3 {
            let bank_z = surf + h;
            let mut col = Collision::build(&scene(bank_z), 4.0);
            col.set_water(Some(std::sync::Arc::new(
                RegionMap::water_slab(0.5, surf))));
            let admitted = col.find_path([8.0, 12.0, 0.0], [40.0, 12.0, bank_z], 1.0, &[], false).is_some();
            assert_eq!(admitted, h <= body.haul_out_up,
                "planner admission must be exactly 'lip ≤ haul_out_up above the surface': \
                 h={h}, admitted={admitted}");
            if admitted {
                // Execute the admitted exit with the real controller, driven like the walker:
                // horizontal wish at the bank, the walker's swim-up rule for the vertical, its
                // body-probe want_swim. Success = standing on the bank, past the lip.
                let mut ctrl = CharacterController::new(
                    [18.0, 12.0, surf - body.float_depth]);
                let mut out = false;
                for _ in 0..1200 {
                    let p = ctrl.pos;
                    let swim = col.in_water(p) || col.in_water([p[0], p[1], p[2] + 3.0]);
                    let intent = eqoxide_ipc::MoveIntent {
                        wish_dir: [1.0, 0.0],
                        wish_vspeed: if swim && bank_z > p[2] + 1.0 { 20.0 } else { 0.0 },
                        jump: false, want_swim: swim, speed: 35.0, climb: 0.0, hop: false,
                    };
                    ctrl.step(intent, 1.0 / 60.0, &col);
                    if ctrl.on_ground && ctrl.pos[0] > 24.0 && (ctrl.pos[2] - bank_z).abs() < 0.6 {
                        out = true;
                        break;
                    }
                }
                assert!(out,
                    "the controller must execute every planner-admitted haul-out (#359): \
                     h={h}, ended at {:?}", ctrl.pos);
            }
            h += 0.25;
        }
    }

    /// **Water-nav Slice 3 (§8, §9a, §10-tier-2): the walker EXECUTES a mid-water route — it swims
    /// DOWN to the planned depth and HOLDS it, never surfacing.** This is the end-to-end proof the
    /// #547 boundary demanded: Slice 2 proved the route is *planned* to −24; here the REAL
    /// `CharacterController` is stepped along it under the REAL depth controller (`swim_vspeed`) and
    /// must arrive at, and hold, that depth.
    ///
    /// Faithful to the live walker's rate split (the thing that makes depth-hold non-trivial): the
    /// vertical wish + `want_swim` are latched on the 150 ms NAV TICK and held for 15 frames, while
    /// the horizontal aim refreshes every ~10 ms frame (fast-steering). So during a hold the wish is
    /// FIXED for 150 ms — and it must stay nonzero the whole time, or the controller's buoyancy
    /// (which fires only on `wish_vspeed == 0`, at 30 u/s) would lift the swimmer ~4.5 u toward the
    /// −6 swim plane before the next tick could correct. The hold survives because below the plane
    /// `swim_vspeed` is never 0 and the controller's `SKIN` clamp zeroes the residual motion.
    ///
    /// Mutation check: revert `swim_vspeed` to the old up-only rule (`carrot > z+1 ? 20 : 0`) and the
    /// dive wish becomes 0 → buoyancy floats the char to ~−6 → it never reaches −24 → the arrival
    /// `expect` panics RED. (This is exactly the #547 live failure, reproduced offline.)
    #[test]
    fn walker_sim_swims_to_and_holds_a_mid_water_depth() {

        // The §9a fixture: a walled pool, surface −4, floor −44, water_slab between.
        let assets = ZoneAssets {
            terrain: vec![
                slab(-44.0, 0.0, 64.0, 0.0, 64.0, true),
                wall_east(0.0, -44.0, 0.0), wall_east(64.0, -44.0, 0.0),
            ],
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(RegionMap::water_slab(-44.0, -4.0))));

        let start = [30.0, 10.0, -4.0];    // at the surface (where a floating start anchors)
        let goal  = [30.0, 46.0, -24.0];   // MID-WATER: 20u below the surface, 20u above the −44 floor
        let route = col.find_path(start, goal, PLAYER_RADIUS, &[], false)
            .expect("Slice 2 plans a route to the mid-water goal");
        // The line the walker steers = start + the planned waypoints (as the real walker does).
        let line: Vec<[f32; 3]> = std::iter::once(start).chain(route.iter().copied()).collect();

        let mut ctrl = CharacterController::new(start);
        const DT: f32 = 0.01;              // ~100 Hz controller
        const FRAMES_PER_TICK: usize = 15; // 150 ms nav tick
        const TOTAL: usize = 1200;         // 12 s
        let mut path_i = 0usize;
        let mut wish_vspeed = 0.0f32;
        let mut want_swim = false;
        let mut arrived_frame: Option<usize> = None;
        let mut max_depth_err_after_arrival = 0.0f32;

        for frame in 0..TOTAL {
            let p = ctrl.pos;
            // ── 150 ms NAV TICK: advance path_i (3D) and LATCH the vertical wish + want_swim ──
            if frame % FRAMES_PER_TICK == 0 {
                while path_i + 2 < line.len() {
                    let (a, b) = (line[path_i], line[path_i + 1]);
                    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                    let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
                    let t = if l2 < 1e-6 { 1.0 }
                        else { ((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1] + (p[2] - a[2]) * ab[2]) / l2 };
                    if t >= 1.0 { path_i += 1; } else { break; }
                }
                let carrot = carrot_along(&line, path_i, p, 5.0).unwrap_or(goal);
                want_swim = col.in_water(p) || col.in_water([p[0], p[1], p[2] + 3.0]);
                let swim_plane = if want_swim {
                    col.water_surface(p).map(|s| s - PLAYER_BODY.float_depth)
                } else { None };
                wish_vspeed = if want_swim { swim_vspeed(carrot[2], p[2], swim_plane) } else { 0.0 };
            }
            // ── ~100 Hz FAST-STEER: refresh only the horizontal aim ──
            let carrot = carrot_along(&line, path_i, p, 5.0).unwrap_or(goal);
            let (dx, dy) = (carrot[0] - p[0], carrot[1] - p[1]);
            let d = (dx * dx + dy * dy).sqrt();
            let wish_dir = if d > 1e-3 { [dx / d, dy / d] } else { [0.0, 0.0] };
            ctrl.step(MoveIntent { wish_dir, wish_vspeed, jump: false, want_swim,
                speed: RUN_SPEED, climb: 0.0, hop: false }, DT, &col);

            let at_goal = (ctrl.pos[0] - goal[0]).hypot(ctrl.pos[1] - goal[1]) < 3.0
                && (ctrl.pos[2] - (-24.0)).abs() < 2.0;
            if arrived_frame.is_none() && at_goal { arrived_frame = Some(frame); }
            if arrived_frame.is_some() {
                max_depth_err_after_arrival = max_depth_err_after_arrival.max((ctrl.pos[2] - (-24.0)).abs());
            }
        }

        let arr = arrived_frame.unwrap_or_else(|| panic!(
            "the swimmer must SWIM to the mid-water goal depth −24 and hold it — instead it ended at \
             {:?} ({:.1}u off the −24 depth). With the retired up-only rule the dive wish is 0, so \
             buoyancy floats it to the ~−6 swim plane and it never arrives at depth (the #547 wedge).",
            ctrl.pos, (ctrl.pos[2] - (-24.0)).abs()));
        assert!(TOTAL - arr >= 500,
            "arrived at frame {arr} — too late to prove a ≥5 s hold in the remaining sim");
        assert!(max_depth_err_after_arrival < 2.5,
            "after arriving, the swimmer DRIFTED off the mid-water depth (max err \
             {max_depth_err_after_arrival:.1}u). A hold must neither surface nor sink — buoyancy must \
             stay suppressed by the nonzero below-plane wish (§8.3), even across the 150 ms latch.");
        assert!(ctrl.pos[2] < -20.0,
            "final feet z {:.1} must be at the mid-water goal, NOT floated back to the ~−6 swim plane",
            ctrl.pos[2]);
    }

    /// **THE FAITHFUL WALKER DRIFT SCANNER (the real per-tick recovery loop).** The static scanner
    /// above drove ONE fine plan with naive pure pursuit and no recovery — which over-counts corner
    /// wedges the real walker recovers from, and cannot measure a planner-cell fix's benefit (the real
    /// walker re-anchors its fine plan every tick, so cleaner cells help it even when a single static
    /// plan wedges). This one mirrors `navigation.rs`'s ACTUAL two-rate loop (post-#399):
    ///
    ///   * a COARSE route committed at goal-change (`find_path_ex`), re-planned on stall/backoff;
    ///   * a ~100 Hz FAST-STEER aim: `fast_steer_aim` toward a 5u carrot on `local_path` (cursor
    ///     `local_i`), refreshed EVERY controller frame — the thing that hugs a bend;
    ///   * a 150 ms NAV TICK that advances `path_i` — monotone advance **then the #673 stale-cursor
    ///     resync**, both halves of `Walker::advance_cursor` — RE-POSTS a fresh `find_path_local`
    ///     from the walker's CURRENT position (1-tick lag, as #399's worker introduces), and runs
    ///     stall detection → downhill backoff → coarse re-plan (capped at 8 attempts), plus the
    ///     #246/#379 proactive coarse re-plan when the fine tier reports `NoWayThrough`.
    ///
    /// ⚠️ **Correction (#727 round 2).** Between #727 round 1 and this commit the claim above was
    /// FALSE: `Walker::advance_cursor` had gained the stale-cursor resync and this loop had not, so
    /// the "faithful" scanner modelled a walker that no longer existed and could not have reproduced
    /// (let alone regressed) the qcat wedge #673 describes. The round-1 review caught it. The resync
    /// is now called here, through the same public `Collision` predicates production uses.
    ///
    /// That change is COMPILE-CHECKED ONLY on the #727 branch: this test is `#[ignore]`d and gated on
    /// baked zone GLBs at `$ZONE_DIR`, so no before/after corpus number from it appears in the PR.
    /// The re-runnable #673 measurement lives in `eqoxide-nav`'s own asset-free hairpin sim
    /// (`steering::cursor_resync_tests`), which is why that sim exists.
    ///
    /// **Still not mirrored, stated so the next reader does not assume otherwise:** this loop has no
    /// #631 route-level no-progress channel, no arrival/follow handling, and no zone-asset gating.
    /// It is a DRIFT scanner, not a walker replica.
    ///
    /// Then it classifies terminal wedges (never arrived, 8 re-paths spent) by face. THIS is the
    /// number that gates a planner-cell fix — run it before/after PR-B.
    ///
    /// ```text
    /// ZONE_DIR=~/.local/share/eqoxide/assets/models \
    ///   cargo test --release --lib faithful_walker_drift_corpus -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires baked zone glbs at $ZONE_DIR; the faithful per-tick-recovery drift baseline"]
    fn faithful_walker_drift_corpus() {

        // ⚠️ **Correction (#733 review).** This block used to open: "Production constants, COPIED
        // from walker.rs. They are copies, not imports (walker.rs is not reachable from an
        // integration test) … Anything with a public home is now CALLED rather than copied". Both
        // halves were wrong by the time they were read. This file already does
        // `use eqoxide::nav::steering::{…}` and calls `steering::resync_cursor`, so `walker.rs`'s
        // module IS reachable; and `LOCAL_REACH` sat three lines under that sentence as a literal
        // `24.0` while `steering::LOCAL_REACH` existed. So did seven more.
        //
        // ⚠️ **Correction, round 2 of the same review — the repair above was itself wrong twice.**
        // It went on to claim "everything with a public home is imported at the top of the file, and
        // only the three below are still literals", and that all three are "private `const`s inside
        // `walker.rs::drive_walk`". Both were falsified by grep:
        //
        //   * `RUN_SPEED` is imported and used by NAME at seven sites, and was ALSO still spelled as
        //     a bare `speed: 44.0` at five others. Fixed — they now read `speed: RUN_SPEED`. The
        //     reason the previous round missed them is worth keeping: the value was grepped in
        //     `steering.rs` and the identifier in this file, so a value-shaped survivor here was
        //     invisible to both. Grep the value AND the identifier AND the concept, in every file.
        //     Then READ each hit: four of this file's nine `44.0`s (lines ~199, 200, 205, 921) are
        //     coordinates and polygon vertices that merely happen to equal the run speed, and
        //     promoting one would assert a relationship that does not exist — a later change to
        //     `RUN_SPEED` would move a wall. `speed: 35.0` at ~160 is likewise a deliberately
        //     different speed, not a stale copy.
        //   * `MAX_REPATHS` does not exist in `walker.rs`. `grep -rn MAX_REPATHS` hits this file and
        //     nothing else; production spells the cap as a bare `if self.nav_repaths < 8` inside
        //     `Walker::drive_walk`. So it is not a copy of a private const — it MIRRORS AN UNNAMED
        //     LITERAL, which is strictly worse, and the fix is to name it there first.
        //
        // So, precisely: everything that HAS a public home is imported at the top of this file and
        // is not restated anywhere in it. The three below have no public home, for two different
        // reasons, and each says which.
        //
        // #919: each cites walker.rs by SOURCE TEXT, not by line number. The line numbers that used
        // to be here were stale on `main` before they were ever reported, and pointed at unrelated
        // code by the time they were fixed. `walker_source_anchors_cited_in_this_file_still_resolve`
        // re-finds each quoted phrase in walker.rs by execution and fails if it is gone.
        const LOOK_AHEAD: f32 = 5.0;   // walker.rs, private in `drive_walk`: `const LOOK_AHEAD: f32 = 5.0;`
        const LOCAL_BOUND: f32 = 40.0; // walker.rs, private in `drive_walk`: `const LOCAL_BOUND: f32 = 40.0;`
        const MAX_REPATHS: u32 = 8;    // NOT a const anywhere: walker.rs spells it `if self.nav_repaths < 8 {`
        const DT: f32 = 1.0 / 100.0;          // ~100 Hz controller, per navigation.rs's fast-steer note
        const FRAMES_PER_TICK: u32 = 15;      // 150 ms / 10 ms

        // The faithful walk. Returns None on arrival, or Some((wedge_pos, aim, route_wet_near_wedge))
        // on a terminal wedge. `route_wet_near_wedge` = did the COMMITTED coarse route carry a water
        // waypoint within 24u of the wedge — the water-routing-vs-#423-clip discriminator (see caller).
        let simulate = |col: &Collision, start: [f32; 3], goal: [f32; 3]| -> Option<([f32; 3], [f32; 2], bool)> {
            let PlanOutcome::Route(mut coarse) = col.find_path_ex(
                start, goal, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()) else { return None };
            if coarse.len() < 2 { return None; }
            let mut ctrl = CharacterController::new(start);
            ctrl.on_ground = true;
            let mut path_i = 0usize;
            let mut local_path: Vec<[f32; 3]> = Vec::new();
            let mut local_i = 0usize;
            // Fine plan requested LAST tick, applied THIS tick (models #399's ~1-tick worker lag).
            let mut pending_local: Option<Vec<[f32; 3]>> = None;
            let mut pending_nwt = false;
            let (mut stuck_i, mut stuck_ticks, mut repaths) = (0usize, 0u32, 0u32);
            let (mut local_stuck, mut replan_cd) = (0u32, 0u32);
            let (mut backoff_ticks, mut backoff_dir) = (0u32, [0.0f32, 0.0]);
            let mut aim = [0.0f32, 0.0];

            // A journey either arrives, or spends its 8 re-paths (~8·NAV_STUCK_TICKS ticks) and wedges.
            // 200 ticks (~30 s sim) is well past both for a ≤400u route at RUN_SPEED — a journey still
            // going at 200 is not making progress and counts as wedged.
            let nav_ticks_budget = 200;
            for _ in 0..nav_ticks_budget {
                let (px, py, pz) = (ctrl.pos[0], ctrl.pos[1], ctrl.pos[2]);
                // ── arrival on the FINAL goal ──
                if (px - goal[0]).hypot(py - goal[1]) < 3.0 { return None; }

                // ── the 150 ms NAV TICK (planning / recovery) ──
                // advance path_i along the coarse route (3D, water-nav Slice 3 — mirrors walker.rs)
                while path_i + 2 < coarse.len() {
                    let (a, b) = (coarse[path_i], coarse[path_i + 1]);
                    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                    let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
                    let t = if l2 < 1e-6 { 1.0 } else { ((px - a[0]) * ab[0] + (py - a[1]) * ab[1] + (pz - a[2]) * ab[2]) / l2 };
                    if t >= 1.0 { path_i += 1; } else { break; }
                }
                // …then the #673 STALE-CURSOR RESYNC, the second half of `Walker::advance_cursor`.
                // Without this the scanner's cursor rule is the PRE-#727 one, so it cannot reproduce
                // (or regress-test) the qcat wedge at all — the gap the #727 round-1 review found.
                // The predicate is the walker's own conjunction, and both halves are the SAME public
                // `Collision` methods `Walker::advance_cursor` calls.
                //
                // ⚠️ Correction (#727 round 4). The clearance IS a divergence, and this comment used
                // to deny it: it said the `PLAYER_RADIUS` below is "the same" value as walker.rs's
                // `STEER_LOS_CLEARANCE` and therefore "nothing here is a copy, so nothing can drift".
                // `STEER_LOS_CLEARANCE` is *defined as* `PLAYER_RADIUS` today, so the two agree by
                // coincidence, not by construction — change that definition and this scanner silently
                // stops modelling the walker. This is an integration test, so it cannot name the
                // `pub(crate)` constant; the in-crate sim in `steering.rs` references it directly and
                // does not have this gap. Until this file can too, the coincidence is disclosed here
                // rather than asserted away.
                {
                    let walked_to = path_i;
                    path_i = eqoxide::nav::steering::resync_cursor(
                        &coarse, path_i, [px, py, pz],
                        |a, b| col.carrot_los_clear(a, b, PLAYER_RADIUS) && col.ground_continuous(a, b));
                    // A resync is NOT progress: raise the stall detector's high-water mark with it,
                    // exactly as the walker does, so a jump can never reset `stuck_ticks`.
                    if path_i > walked_to { stuck_i = stuck_i.max(path_i); }
                }
                if replan_cd > 0 { replan_cd -= 1; }

                // downhill backoff in progress → drive reverse aim, then re-plan when it ends
                if backoff_ticks > 0 {
                    backoff_ticks -= 1;
                    for _ in 0..FRAMES_PER_TICK {
                        // The real walker's downhill-backoff branch — `if self.backoff_ticks > 0 {`
                        // in walker.rs — drives `want_swim: false,` UNCONDITIONALLY, even while
                        // submerged: the backoff is a deliberate non-swim recovery. The sim MUST
                        // match, or it recovers (swim-mode step) where the client sinks (non-swim
                        // step): a false pass. Both quoted phrases are pinned by
                        // `walker_source_anchors_cited_in_this_file_still_resolve`.
                        ctrl.step(MoveIntent { wish_dir: backoff_dir, wish_vspeed: 0.0, jump: false,
                            want_swim: false, speed: RUN_SPEED, climb: 0.0, hop: false }, DT, col);
                    }
                    if backoff_ticks == 0 {
                        if let PlanOutcome::Route(r) = col.find_path_ex(
                            [ctrl.pos[0], ctrl.pos[1], ctrl.pos[2]], goal, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()) {
                            coarse = r; path_i = 0; local_path.clear(); local_i = 0;
                        }
                        stuck_ticks = 0;
                    }
                    continue;
                }

                // apply the fine plan requested last tick (1-tick lag)
                if let Some(lp) = pending_local.take() {
                    local_path = lp; local_i = 0;
                    if pending_nwt {
                        local_stuck += 1;
                        if local_stuck >= NAV_LOCAL_STUCK_TICKS && replan_cd == 0 {
                            if let PlanOutcome::Route(r) = col.find_path_ex(
                                [px, py, ctrl.pos[2]], goal, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()) {
                                coarse = r; path_i = 0; local_path.clear(); local_i = 0;
                            }
                            local_stuck = 0; replan_cd = REPLAN_COOLDOWN_TICKS;
                        }
                    } else {
                        local_stuck = 0;
                    }
                    // pending_nwt is reassigned by the match below every tick, no reset needed here.
                }
                // post a fresh fine plan for NOW (lands next tick)
                let coarse_carrot = carrot_along(&coarse, path_i, [px, py, pz], LOCAL_REACH)
                    .unwrap_or([goal[0], goal[1], ctrl.pos[2]]);
                match col.find_path_local([px, py, ctrl.pos[2]], coarse_carrot, LOCAL_CELL, LOCAL_BOUND, LOCAL_CELL * 2.0) {
                    LocalOutcome::Threaded(s)     => { pending_local = Some(s); pending_nwt = false; }
                    LocalOutcome::NoWayThrough{steer, ..} => { pending_local = Some(steer); pending_nwt = true; }
                    LocalOutcome::Exhausted{steer, ..}    => { pending_local = Some(steer); pending_nwt = false; }
                }

                // stall detection on coarse path_i progress
                if path_i > stuck_i { stuck_i = path_i; stuck_ticks = 0; }
                else {
                    stuck_ticks += 1;
                    if stuck_ticks >= NAV_STUCK_TICKS {
                        stuck_ticks = 0;
                        if repaths < MAX_REPATHS {
                            repaths += 1;
                            backoff_ticks = NAV_BACKOFF_TICKS;
                            let carrot = carrot_along(&coarse, path_i, [px, py, pz], LOOK_AHEAD)
                                .unwrap_or([goal[0], goal[1], ctrl.pos[2]]);
                            let (dx, dy) = (carrot[0] - px, carrot[1] - py);
                            let dl = (dx * dx + dy * dy).sqrt();
                            backoff_dir = if dl > 1e-3 { [-dx / dl, -dy / dl] } else { [0.0, 0.0] };
                            continue;
                        }
                        let wp = ctrl.pos;
                        let wet_near = coarse.iter().any(|w|
                            (w[0] - wp[0]).hypot(w[1] - wp[1]) < 24.0
                            && (col.in_water(*w) || col.in_water([w[0], w[1], w[2] + 3.0])));
                        return Some((wp, aim, wet_near)); // terminal wedge (8 re-paths spent)
                    }
                }

                // ── the ~100 Hz FAST-STEER + controller stepping for this tick ──
                for _ in 0..FRAMES_PER_TICK {
                    let from = [ctrl.pos[0], ctrl.pos[1], ctrl.pos[2]];
                    // fast-steer aim on the fine plan if present, else the coarse carrot
                    let steer_aim = if local_path.len() >= 2 {
                        // Always-clear LOS keeps this drift baseline byte-for-byte pre-#685; the LOS
                        // clamp's own blast radius is measured by `carrot_los_clamp_blast_radius`.
                        fast_steer_aim(&local_path, &mut local_i, from, LOOK_AHEAD, |_, _| true).map(|(d, _)| d)
                    } else { None };
                    aim = steer_aim.unwrap_or_else(|| {
                        let c = carrot_along(&coarse, path_i, from, LOOK_AHEAD)
                            .unwrap_or([goal[0], goal[1], ctrl.pos[2]]);
                        let (dx, dy) = (c[0] - from[0], c[1] - from[1]);
                        let d = (dx * dx + dy * dy).sqrt().max(1e-3);
                        [dx / d, dy / d]
                    });
                    // The REAL walker's swim rule (walker.rs §8.2), driving the SAME controller:
                    // body-probe want_swim, and the water-nav Slice 3 depth controller `swim_vspeed`
                    // toward the active waypoint's DEPTH. CRITICAL faithfulness point (#1b): the
                    // vertical-wish target z must come from the SAME path the walker steers —
                    // `steer_target` returns the FINE local-plan carrot when local.len() >= 2, falling
                    // back to the coarse carrot only when there is no fine plan. `local_i` was already
                    // advanced this frame by `fast_steer_aim` above (when a fine plan exists), so this
                    // reads the identical carrot the horizontal `aim` used.
                    let p = ctrl.pos;
                    let swim = col.in_water(p) || col.in_water([p[0], p[1], p[2] + 3.0]);
                    let coarse_c = carrot_along(&coarse, path_i, from, LOOK_AHEAD).unwrap_or(goal);
                    let tz = if local_path.len() >= 2 {
                        carrot_along(&local_path, local_i, from, LOOK_AHEAD).map(|c| c[2]).unwrap_or(coarse_c[2])
                    } else { coarse_c[2] };
                    // Same depth controller as the walker (calls the production fn directly).
                    let swim_plane = if swim {
                        col.water_surface(p).map(|s| s - PLAYER_BODY.float_depth)
                    } else { None };
                    let wish_vspeed = if swim { swim_vspeed(tz, p[2], swim_plane) } else { 0.0 };
                    ctrl.step(MoveIntent { wish_dir: aim, wish_vspeed, jump: false, want_swim: swim,
                        speed: RUN_SPEED, climb: 0.0, hop: stuck_ticks >= NAV_HOP_TICKS }, DT, col);
                    if (ctrl.pos[0] - goal[0]).hypot(ctrl.pos[1] - goal[1]) < 3.0 { return None; }
                }
            }
            let wp = ctrl.pos;
            let wet_near = coarse.iter().any(|w|
                (w[0] - wp[0]).hypot(w[1] - wp[1]) < 24.0
                && (col.in_water(*w) || col.in_water([w[0], w[1], w[2] + 3.0])));
            Some((wp, aim, wet_near)) // ran out of sim time
        };

        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));

        // `DRIFT_INCLUDE_WATER=1` runs the water-inclusive variant (#378 Phase 2 validation): keep
        // water-adjacent journeys and COUNT waterline wedges (the separate #423 crossing bug) in a
        // `water` column, so the water dimension is measured rather than silently dropped. Default
        // (unset) is the DRY gate that skips water.
        let include_water = std::env::var("DRIFT_INCLUDE_WATER").is_ok();

        // The DRY acceptance corpus is clean dry dungeons/cities. **qcat is deliberately NOT a dry
        // gate zone**: it is confounded by known bugs (#423 walk-through-walls-into-water, #329
        // spawn-pocket dead-end, and unimplemented water nav #359/#197), so a pass/fail there is not
        // clean evidence about this refactor — the owner's call. qcat is added ONLY in the
        // water-inclusive VISIBILITY run (never a pass/fail gate), where measuring water-adjacent
        // behaviour is the whole point and its waterline wedges land in the `water`/#423 column.
        let zones: Vec<String> = std::env::var("ZONES").ok()
            .map(|z| z.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| {
                let mut z: Vec<String> = vec![
                    "akanon", "blackburrow", "qeynos2", "gfaydark", "crushbone", "neriaka",
                    "felwithea", "highpass", "everfrost", "butcher",
                ].into_iter().map(str::to_string).collect();
                if include_water { z.push("qcat".to_string()); }
                z
            });

        let mut seed: u64 = 0xD21F_7A3E; // same seed family as the static scanner
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };

        let (mut tot_pairs, mut tot_walked, mut tot_wedged) = (0usize, 0usize, 0usize);
        let (mut tot_height, mut tot_overlap, mut tot_other) = (0usize, 0usize, 0usize);
        // The water column is SPLIT (Increment 1): `wat-route` = a wedge whose committed coarse route
        // itself carried water waypoints near the wedge — a planner/ROUTING failure, OURS to fix in
        // later increments; `#423` = the route was DRY near the wedge but the character ended up
        // wet/wedged — the pre-existing walk-THROUGH-a-wall-into-water collision bug (#423), NOT a
        // routing failure. They are never lumped.
        // #762: `wat-route` and `#423` are TOTALS OVER A CORPUS, and a corpus with a hole in it
        // reports the hole's zeros as if they were results. A build host holding 2 of 497 `.wtr`
        // files once made every water-inclusive halas/blackburrow run print `0`/`0` here while the
        // `.glb` hashes matched exactly — a perfect score that had never consulted any water data.
        // The rollups carry per-zone provenance, so an unmeasured zone contributes NO number and the
        // TOTAL line cannot come out looking clean. Round 2 (B1): a zone dropped BEFORE the water
        // check even runs (no glb, no grid) is routed through `WaterRollup::skip` below, not left to
        // silently vanish from the denominator too — see the type doc for why that was its own #762-
        // shaped bug. Round 3 (B1 again): round 2 wired two of this loop's FOUR zone-abandoning
        // `continue`s and its doc claimed all of them, so `if pairs.is_empty()` printed the word
        // "skipped" while calling nothing and the original defect string came straight back. The
        // wiring is no longer what makes the denominator honest: `begin_zone` opens each zone and
        // `add`/`skip` close it, so ANY exit that doesn't close it is recorded as `unaccounted` by
        // the rollup itself, including exits nobody has written yet.
        let mut roll_wr = WaterRollup::new();
        let mut roll_423 = WaterRollup::new();
        println!("\n=== faithful walker drift: {} mode ===", if include_water { "WATER-INCLUSIVE" } else { "DRY" });
        println!("{:<12} {:>6} {:>7} {:>8} {:>8} {:>6} {:>9} {:>6}",
            "zone", "walked", "wedged", "height", "overlap", "other", "wat-route", "#423");
        for zone in &zones {
            // #762: OPEN the zone before anything that can abandon it. Every exit from this body
            // must reach `add` or `skip`; one that doesn't is recorded as `unaccounted`.
            //
            // #805: opening goes through `open_zone_checked`, NOT a bare `begin_zone` pair, because
            // it re-checks the `unaccounted` AND `unmeasured` buckets for the zones already opened
            // before it opens this one. #762 left those checks to this function's LAST statement and
            // #763 says this corpus never terminates on blackburrow, so on the one run where a zone
            // is known to misbehave they were never reached at all. Per zone, a run that hangs /
            // returns / dies at zone K has already checked zones 1..K-1 for both. Do not move this
            // line down, and do not replace it with `begin_zone`.
            //
            // Nothing CI runs would notice if you did: this corpus is `#[ignore]`d and CI passes no
            // `--ignored` (#799 / #777). Reverting this one line to `begin_zone` was measured to
            // leave the whole workspace suite byte-identically green.
            open_zone_checked(&mut roll_wr, &mut roll_423, zone);
            let p = std::path::Path::new(&dir).join(format!("{zone}.glb"));
            // #762 round 2 (B1): these `continue`s fire BEFORE the water check ever runs, so the
            // zone is not "unmeasured" (that means the check ran and failed) — it never reached the
            // check at all. `skip` still puts it in the rollup's denominator, so the printed "(over
            // N/N zones)" cannot come out looking like complete coverage of a corpus this run barely
            // touched.
            let Ok(za) = ZoneAssets::from_glb(&p) else {
                println!("{zone:<12}  (no glb — skipped)");
                roll_wr.skip(zone, "no glb"); roll_423.skip(zone, "no glb");
                continue
            };
            let mut col = Collision::build(&za, 32.0);
            if col.cols == 0 {
                println!("{zone:<12}  (no grid — skipped)");
                roll_wr.skip(zone, "no grid"); roll_423.skip(zone, "no grid");
                continue;
            }
            // Water drives BOTH modes here: in water-inclusive mode it is what the run measures, and
            // in DRY mode it is the filter that keeps wet journeys out (`!include_water && in_water`).
            // Either way a zone with no region map is UNMEASURED, never a zone with no water.
            let zw = ZoneWater::load(&std::path::Path::new(&dir).join("maps/water"), zone);
            if let Err(e) = zw.install(&mut col) {
                println!("{zone:<12} {:>6} {:>7} {:>8} {:>8} {:>6} {:>9} {:>6}   ({UNMEASURED}: {e})",
                    "-", "-", "-", "-", "-", UNMEASURED, UNMEASURED);
                roll_wr.add(zone, &zw.tally());
                roll_423.add(zone, &zw.tally());
                continue;
            }

            // Sample full (start, goal) pairs: a random floor point and a goal 120-400u away that a
            // coarse route actually reaches (so we simulate real journeys, not un-routable noise).
            let mut pairs: Vec<([f32; 3], [f32; 3])> = Vec::new();
            let mut tries = 0;
            while pairs.len() < 60 && tries < 2000 {
                tries += 1;
                let e = col.origin[0] + (rnd() as f32 / u32::MAX as f32) * (col.cols as f32 * col.cell_size);
                let n = col.origin[1] + (rnd() as f32 / u32::MAX as f32) * (col.rows as f32 * col.cell_size);
                let Some(z) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { continue };
                let ang = (rnd() as f32 / u32::MAX as f32) * std::f32::consts::TAU;
                let d = 120.0 + (rnd() as f32 / u32::MAX as f32) * 280.0;
                let (ge, gn) = (e + d * ang.cos(), n + d * ang.sin());
                let Some(gz) = col.nearest_floor(ge, gn, z, 400.0, 400.0) else { continue };
                // WATER MODE (#378 Phase 2 validation): `DRIFT_INCLUDE_WATER=1` KEEPS water-adjacent
                // journeys so the water dimension is MEASURED, not silently dropped — the lesson from
                // the earlier miss. The water CROSSING itself is a separate pre-existing bug (#423,
                // out of scope), so those journeys are expected to wedge at the waterline and are
                // COUNTED in a `water` column, never hidden. Dry mode (default) is the original gate.
                let s = [e, n, z]; let g = [ge, gn, gz];
                if !include_water && (col.in_water(s) || col.in_water(g)) { continue; }
                // DRIVABILITY FILTER. This pure-pursuit sim faithfully drives WALK legs only. It does
                // NOT execute A*'s controlled-fall, jump-edge, or swim edges (those need the walker's
                // fall/jump/swim intents, out of scope here — the static scanner skipped them per-PLAN
                // for the same reason). So only accept a journey whose COARSE route is all-walkable: no
                // segment with a big z-drop (controlled fall / jump landing) and (dry mode) no waypoint
                // in water. Without this, multi-level dungeons (blackburrow, neriaka) flood the count
                // with wedges at fall/swim TRANSITIONS the sim structurally cannot cross — a sim
                // artifact, not a walker drift.
                let PlanOutcome::Route(cr) = col.find_path_ex(
                    s, g, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()) else { continue };
                if cr.len() < 3 { continue; }
                let no_fall_jump = cr.windows(2).all(|w| {
                    let dz = w[1][2] - w[0][2];
                    let seg = (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]);
                    // Increment 1: the sim now DRIVES surface swim legs (body-probe want_swim +
                    // swim-up vspeed), so in water mode a water-touching segment is exempt from the
                    // DRY fall bound — entering water legitimately drops up to the step-in height
                    // (~STEP_H=20) to the surface, which the dry `dz > -4` would wrongly filter.
                    // BUT the exemption is NOT unbounded (Hunt 3): the sim never jumps (jump:false at
                    // both call sites) and can't dive against buoyancy, so a wet segment that is ALSO
                    // a deep dive or a long jump-span is still undrivable and stays filtered.
                    let wet_seg = include_water && (
                        col.in_water(w[0]) || col.in_water(w[1])
                        || col.in_water([w[0][0], w[0][1], w[0][2] + 3.0])
                        || col.in_water([w[1][0], w[1][1], w[1][2] + 3.0]));
                    let drop_cap = if wet_seg { -20.0 } else { -4.0 }; // water step-in may drop to ~STEP_H
                    dz > drop_cap && seg < 12.0
                });
                let no_water = !cr.iter().any(|w| col.in_water(*w) || col.in_water([w[0], w[1], w[2] + 3.0]));
                if !no_fall_jump { continue; } // the sim cannot drive dry fall/jump edges in EITHER mode
                if !include_water && !no_water { continue; } // dry mode still excludes water routes
                pairs.push((s, g));
            }

            // FIXED bank-to-bank pairs (Increment 1): force water crossings that random sampling
            // rarely hits (find_path prefers a dry shore when one exists, so sampled pairs seldom
            // actually cross). Coordinates verified to route THROUGH water via a one-off water-extent
            // probe. Only injected in water mode; each pair is a real forced crossing in a gate zone.
            let forced_start = pairs.len();
            if include_water {
                let forced: &[([f32; 3], [f32; 3], &str)] = match zone.as_str() {
                    // halas #197 central pool (surface ~ -3.9). W↔E spans the pool: a 39-wp FULL swim
                    // (every waypoint wet). N→S dips through the south edge of the pool.
                    "halas" => &[
                        ([-150.0, -231.0, -130.94], [150.0, -231.0, -130.94], "#197 pool: W bank -> E bank (full swim across)"),
                        ([6.0, -70.0, 1.0],         [6.0, -454.0, -30.15],    "#197 pool: N shore -> S shore"),
                    ],
                    // qeynos2 moat/canal (surface ~ -2.8): W→E crossings that dip into the moat.
                    "qeynos2" => &[
                        ([-400.0, -115.0, 79.97], [-270.0, -115.0, -2.0], "moat: W rampart -> E bank"),
                        ([-450.0, -115.0, 79.97], [-220.0, -115.0, 0.0],  "moat: wide W -> E"),
                    ],
                    // blackburrow lake (surface ~ -148): W→E crossings straight through the lake.
                    "blackburrow" => &[
                        ([-118.0, 0.0, -170.94], [361.0, 0.0, -128.94], "lake: W bank -> E bank"),
                        ([-60.0, 0.0, -227.91],  [300.0, 0.0, -129.12], "lake: near-W -> E bank"),
                    ],
                    _ => &[],
                };
                for (s, g, what) in forced {
                    println!("  [FORCED PAIR ] {zone}: {what}  {:?} -> {:?}", s, g);
                    pairs.push((*s, *g));
                }
            }
            let n_forced = pairs.len() - forced_start;
            // #762 round 3 (B1): the third zone-abandoning `continue`. Round 2 left it unwired while
            // printing the same word ("skipped") as the two wired ones, so the zone dropped out of
            // both the numerator and the denominator and the corpus printed a clean
            // `wat-route: 0 (over 1/1 zones)` over a two-zone run. Its `.wtr` loaded fine here, so it
            // is NOT "unmeasured" — the water check ran, there was simply nothing to walk.
            if pairs.is_empty() {
                println!("{zone:<12}  (no routable pairs — skipped)");
                roll_wr.skip(zone, "no routable pairs"); roll_423.skip(zone, "no routable pairs");
                continue;
            }

            let (mut walked, mut wedged, mut n_h, mut n_o, mut n_x) = (0usize, 0usize, 0usize, 0usize, 0usize);
            let (mut n_wr, mut n_423) = (0usize, 0usize);
            for (i, (s, g)) in pairs.iter().enumerate() {
                let forced = i >= forced_start;
                walked += 1;
                let Some((w, aim, route_wet_near)) = simulate(&col, *s, *g) else {
                    // simulate returned None: either ARRIVED (success) or the coarse route was
                    // untraversable/too-short. A forced crossing that never routed is itself a
                    // routing finding — flag it so a silently-dropped forced pair can't hide.
                    if forced {
                        let routed = matches!(col.find_path_ex(*s, *g, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()), PlanOutcome::Route(_));
                        if !routed {
                            walked -= 1;
                            println!("  [FORCED NOROUTE] {zone}: forced crossing did not route {:?} -> {:?}", s, g);
                        }
                    }
                    continue;
                };
                // A wedge that ended in/at water. DRY mode drops it (out of scope). WATER mode splits
                // it (Increment 1 classifier): if the committed coarse route carried water waypoints
                // NEAR the wedge, this is a `wat-route` planner/ROUTING failure (ours); otherwise the
                // route was dry near the wedge and the character ended up wet — a `#423` clip (the
                // pre-existing walk-through-wall-into-water collision bug). Never lumped together.
                if col.in_water(w) || col.in_water([w[0], w[1], w[2] + 3.0]) {
                    if include_water {
                        wedged += 1;
                        let (lbl, tag) = if route_wet_near { n_wr += 1; ("WAT-ROUTE", "route was WET near wedge = planner/routing failure (OURS)") }
                            else { n_423 += 1; ("#423 CLIP", "route was DRY near wedge, char ended wet = #423 walk-through-wall (separate)") };
                        println!("  [{lbl:<12}] {zone}:{} wet wedge ({:.1},{:.1},{:.1}) start ({:.1},{:.1},{:.1}) goal ({:.1},{:.1},{:.1}) — {tag}",
                            if forced { " [forced]" } else { "" }, w[0], w[1], w[2], s[0], s[1], s[2], g[0], g[1], g[2]);
                    } else {
                        walked -= 1;
                    }
                    continue;
                }
                wedged += 1;
                let to = [w[0] + aim[0] * 4.0, w[1] + aim[1] * 4.0];
                // Classify against the heights each side ACTUALLY uses, read from the shared Body —
                // not re-hardcoded copies that would themselves drift. HEIGHT counts a wedge where
                // the controller's contact ray is blocked but the planner's probes are clear; with
                // both derived from PLAYER_BODY the class should be structurally empty, so any
                // nonzero count here is a regression alarm (#386).
                let body = &PLAYER_BODY;
                let ctrl_chest_blocked = !col.line_clear(
                    [w[0], w[1], w[2] + body.contact_probes()[1]],
                    [to[0], to[1], w[2] + body.contact_probes()[1]], PLAYER_RADIUS);
                let planner_clear = body.planner_probes().iter().all(|&hz|
                    col.path_clear([w[0], w[1], w[2] + hz], [to[0], to[1], w[2] + hz], PLAYER_RADIUS));
                let overlap = !col.footprint_clear(w[0], w[1], w[2], PLAYER_RADIUS, 8)
                    || !col.footprint_clear(w[0] + aim[0], w[1] + aim[1], w[2], PLAYER_RADIUS, 8);
                let kind = if ctrl_chest_blocked && planner_clear { n_h += 1; "HEIGHT #386" }
                    else if overlap { n_o += 1; "OVERLAP #381" }
                    else { n_x += 1; "OTHER" };
                println!("  [{kind:<12}] {zone}: wedged ({:.1},{:.1},{:.1}) start ({:.1},{:.1},{:.1}) goal ({:.1},{:.1},{:.1})",
                    w[0], w[1], w[2], s[0], s[1], s[2], g[0], g[1], g[2]);
            }
            println!("{zone:<12} {walked:>6} {wedged:>7} {n_h:>8} {n_o:>8} {n_x:>6} {n_wr:>9} {n_423:>6}   ({n_forced} forced)");
            tot_pairs += pairs.len(); tot_walked += walked; tot_wedged += wedged;
            tot_height += n_h; tot_overlap += n_o; tot_other += n_x;
            // `measure` runs its closure only when the region map is really there, so these two
            // numbers cannot exist for a zone whose `.wtr` did not load (#762).
            roll_wr.add(zone, &zw.measure(|_| n_wr));
            roll_423.add(zone, &zw.measure(|_| n_423));
        }
        let rate = if tot_walked > 0 { 100.0 * tot_wedged as f32 / tot_walked as f32 } else { 0.0 };
        println!("\n=== FAITHFUL WALKER DRIFT [{}]: {tot_walked} full journeys walked, {tot_wedged} terminal wedges \
            ({rate:.2}%) — height #386: {tot_height}, overlap #381: {tot_overlap}, other: {tot_other}, \
            wat-route: {roll_wr}, #423: {roll_423} ===",
            if include_water { "WATER-INCLUSIVE" } else { "DRY" });
        if include_water {
            println!("(wat-route = wedge whose COMMITTED coarse route carried water waypoints near the wedge = a \
                      planner/ROUTING failure, OURS to fix in later water-nav increments. #423 = route dry near the \
                      wedge but the character ended up wet = the SEPARATE pre-existing walk-through-wall-into-water \
                      collision bug, not a routing failure. The two are never lumped.)");
        }
        let _ = tot_pairs;
        assert!(tot_walked > 0, "no journeys walked — check $ZONE_DIR");
        // #762: a water column of `0` is only a result if every zone in the run was actually
        // measured. Fail LOUDLY here rather than letting the reader infer a clean water score from a
        // corpus that never opened those zones' region data.
        //
        // Round 3 (review N-R2b): this message used to name only `unmeasured_zones()` and assert
        // "had no loadable .wtr" as the cause, so a run whose only hole was a SKIP printed an empty
        // subject list attached to a false cause (`[] had no loadable .wtr`). `is_complete` now has
        // three independent triggers, so the message names all three buckets and lets the reader see
        // which one is non-empty instead of asserting a cause this assert never established.
        //
        // #805: this assert is still TERMINAL and still unreachable on a run that does not finish
        // the loop above (#763: blackburrow). What changed is that TWO of its three buckets —
        // `unaccounted` and `unmeasured` — are ALSO decided per zone by `open_zone_checked` as each
        // zone closes. This line remains the ONLY check for the `skipped` bucket, for every zone,
        // and the only check of any kind for the last zone opened (nothing closes it, so no later
        // `begin_zone` observes it). It is still load-bearing; it is not a backstop. Why `skipped`
        // is exempt — a diagnostics trade, not a decidability one — is in `open_zone_checked`'s doc.
        assert!(roll_wr.is_complete() && roll_423.is_complete(),
            "#762: this run has a hole in its water coverage, so the wat-route/#423 columns above \
             are NOT a score for this corpus. Holes, by kind (an empty list means that kind did not \
             occur):\n  \
             wat-route — unmeasured (.wtr failed to load, so nothing was measured): {:?}; \
             skipped (dropped before the water check ran, e.g. no glb/no grid/no routable pairs): {:?}; \
             unaccounted (left the loop without reaching add or skip — a corpus wiring bug, not an \
             asset problem): {:?}\n  \
             #423     — unmeasured: {:?}; skipped: {:?}; unaccounted: {:?}\n  \
             Bake or fetch the missing region files / zone glbs and re-run. \
             (wat-route {roll_wr}; #423 {roll_423})",
            roll_wr.unmeasured_zones(), roll_wr.skipped_zones(), roll_wr.unaccounted_zones(),
            roll_423.unmeasured_zones(), roll_423.skipped_zones(), roll_423.unaccounted_zones());
    }

// ── #805: the corpus's zone accounting, enforced per zone instead of only at the end ──

    /// Open `zone` on both of `faithful_walker_drift_corpus`'s rollups — but first refuse the run if
    /// any zone already closed is `unaccounted` or `unmeasured`.
    ///
    /// **Why this exists (#805).** #762 gave the corpus a completeness refusal, and put it in the
    /// corpus's last statement. #763 says the corpus never terminates on blackburrow. So on the one
    /// run where a zone is known to misbehave, the refusal is not reached — a guard that is written
    /// but not run, which is the failure mode this project ranks above a wrong answer. Checking at
    /// the top of every iteration means a run that hangs, returns early, or dies at zone K has
    /// already checked zones 1..K-1 by the time it gets there.
    ///
    /// **Which buckets, and why `skipped` is not one of them.**
    ///
    /// * `unaccounted` — "a zone left the loop body without reaching `add` or `skip`". A corpus
    ///   WIRING bug, never an asset problem.
    /// * `unmeasured` — "the zone's `.glb` and grid loaded, so the water check RAN, and the `.wtr`
    ///   did not load". The zone is then abandoned UNWALKED (`walker_sim.rs`'s `install` arm prints
    ///   a row of dashes and `continue`s), so do not read this bucket as "walked but unscored" — an
    ///   earlier revision of this comment and of the panic below said "the zone was walked", and a
    ///   real two-zone run (`ZONES=crushbone,felwithea` with `crushbone.wtr` moved aside) printed
    ///   `crushbone  -  -  -  -  -  unmeasured unmeasured` and finished in 0.53s, i.e. zero
    ///   journeys. **This is #762's motivating exhibit** (a build host holding 2 of 497 `.wtr`
    ///   files scoring `0`/`0` on a water-inclusive run), so it is the bucket #805 is actually
    ///   about, and leaving it terminal left the exhibit uncovered on exactly the hanging run.
    /// * `skipped` — "the zone was dropped before the water check ran at all: no `.glb`, no grid, no
    ///   routable pairs". **Deliberately NOT checked here, and the reason is DIAGNOSTICS, not
    ///   decidability.** `unmeasured_zones()` is a straight read of a push-only `Vec` and so is
    ///   genuinely monotone. `unaccounted_zones()` — the bucket this gate actually reads — is NOT:
    ///   it chains that push-only `Vec` with the single transient slot `open` (`water_grid.rs:522`),
    ///   and `settle()` (the shared body of `add`/`skip`) `take()`s that slot on every call
    ///   (`water_grid.rs:547`). Measured directly: after `begin_zone("z")`,
    ///   `unaccounted_zones() == ["z"]`; right after `skip("z", …)`, it is `[]` again — the "hole" a
    ///   caller would see mid-zone is not permanent at all. An earlier revision of this comment
    ///   claimed the opposite of that property for `unmeasured`/`skipped` and the PR's reviewer
    ///   measured it false.
    ///
    ///   **So why does this gate never miss a real hole? Not bucket monotonicity — call order.**
    ///   `open_zone_checked` reads `unaccounted_zones()`/`unmeasured_zones()` BEFORE it calls
    ///   `begin_zone` for the new zone, so what it sees is exactly what the PREVIOUS iteration left
    ///   behind: if that zone was closed (`add`/`skip`), `open` is `None` and there is nothing to
    ///   see; if it was abandoned, `open` still names it, and the chain surfaces it right here — via
    ///   the TRANSIENT slot, before anything has cleared it. `begin_zone`'s own permanent record of
    ///   an abandoned zone (`self.unaccounted.push(prev)`, `water_grid.rs:539-540`) is what a LATER
    ///   reader (the terminal `is_complete()` assert, after zones opened after this one) relies on —
    ///   but for THIS gate's own check, that push never even needs to run: the `panic!` below fires
    ///   first, on the transient view, every time.
    ///
    ///   **The revision that replaced it — "a per-zone refusal could never fire where the terminal
    ///   one would not" (#830) — is also not quite right, read literally**, for a simpler reason than
    ///   bucket monotonicity: `faithful_walker_drift_corpus`'s own `assert!(tot_walked > 0)` runs
    ///   BEFORE the terminal completeness assert, so a run that is all-`skipped` (never checked by
    ///   this gate at all) or a single unmeasured zone with nothing opened after it (this gate only
    ///   re-checks a zone's holes when the NEXT zone opens — the corpus's own terminal-assert
    ///   comment, above, already notes this for "the last zone opened") dies at `tot_walked > 0` and
    ///   the terminal assert never runs, full stop. And whenever THIS gate itself panics, that panic
    ///   unwinds past both asserts, so in every run where the per-zone refusal actually fires, the
    ///   terminal one provably does NOT run in that same execution — the opposite of "could never
    ///   fire where the terminal one would not." The corrected claim is narrower: no NEW false
    ///   positive — a hole this gate catches was always a real one (its zone was truly abandoned, or
    ///   truly failed to load) — not that the terminal check is somehow redundant with this one.
    ///
    ///   **Why `skipped` is the one bucket exempt, restated because the old rationale overstated its
    ///   scope (#830).** Not because it is rarer than the other two — #762's own motivating exhibit
    ///   (the `unmeasured` bullet, above) is a build host holding only 2 of 497 `.wtr` files, so
    ///   "`skipped` is the common state" is not a fact that distinguishes it from `unmeasured`, and no
    ///   comparably-measured count exists here for how often each `skipped` cause fires on a real
    ///   corpus — this file has no baked `$ZONE_DIR` to run that measurement against, so that piece
    ///   of the asymmetry is left unmeasured, not assumed clean. What DOES distinguish them is what
    ///   a hole SIGNALS, not how often it occurs:
    ///   `no glb`/`no grid` (two of `skipped`'s three causes) mean the zone was never in scope for
    ///   this run at all — the ROUTINE, expected shape of a `$ZONE_DIR` that only holds a
    ///   caller-chosen subset of zones. `unmeasured` means
    ///   the zone WAS in scope — its `.glb` and grid loaded, so someone deliberately provided it — and
    ///   specifically its paired `.wtr` failed, which is a claim about a broken delivery for a zone
    ///   you asked for; #762 was filed as a bug for exactly that reason, not treated as routine.
    ///   Measured on a two-zone run (`ZONES=akanon,crushbone`) against real baked GLBs: akanon skipped
    ///   for "no routable pairs" (`skipped`'s third cause, unrelated to asset absence, and itself not
    ///   measured against the other two here) at zone 1 of 2, so gating `skipped` here would have
    ///   killed the run before crushbone's 60 journeys ever ran. That cost is paid for a bucket the
    ///   table already prints per zone (`(no glb — skipped)`), so it buys little. `skipped` stays out
    ///   of this gate. That is a judgement call about what a hole signals, not a frequency claim, and
    ///   not a wall.
    ///
    /// **What an abort here costs (N4).** This panics mid-loop, so the zones AFTER `zone` are never
    /// opened and their holes are never seen: the message is a partial picture and names only the
    /// first offending rollup. The per-zone table rows already printed stay; the corpus's own total
    /// line does not run, so this function prints a partial rollup line of its own before panicking.
    ///
    /// **What this does NOT buy, stated so nobody re-derives it.**
    ///
    /// * A hang produces no new output. An operator watching a wedged blackburrow run still sees
    ///   nothing extra; there is no timeout here and #763 is untouched by design.
    /// * The zone that is OPEN when the run stops is not covered — nothing closes it and no later
    ///   `begin_zone` runs — so it is still the terminal assert's job, as is `skipped` for every
    ///   zone. The terminal assert is therefore still load-bearing and still unreachable on a
    ///   non-terminating run.
    /// * **This call site is not covered by any always-run test (#799).** Delete the
    ///   `open_zone_checked(…)` call from the corpus loop, restore the bare `begin_zone` pair, and
    ///   the whole workspace suite stays byte-identically green: `faithful_walker_drift_corpus` is
    ///   `#[ignore]`d and `.github/workflows/test.yml` runs `cargo test --workspace --locked` with
    ///   no `--ignored`. The tests below pin this HELPER's behaviour; nothing that CI runs pins that
    ///   the corpus calls it. That was measured (by running the deletion and comparing figures), not
    ///   assumed, and it is not fixed here — whether `#[ignore]`d tests should run in CI is
    ///   #777/#654/#659, an owner decision.
    /// * The corpus's total line (`wat-route: {roll_wr}`) sits after the same loop, so an
    ///   unterminated run prints no water total — a claim about statement order in THIS file,
    ///   checked by reading it, not an observation of a hung run. No blackburrow run was executed.
    fn open_zone_checked(wr: &mut WaterRollup, r423: &mut WaterRollup, zone: &str) {
        for (label, roll) in [("wat-route", &*wr), ("#423", &*r423)] {
            let orphans = roll.unaccounted_zones();
            let unmeasured = roll.unmeasured_zones();
            if orphans.is_empty() && unmeasured.is_empty() { continue; }
            // N4: the corpus's own total line runs after the loop and this panic is inside it, so
            // print what the run DID establish before dying. PARTIAL by construction — the zones
            // after this one are never opened.
            println!("\n=== FAITHFUL WALKER DRIFT [ABORTED while opening {zone}] — PARTIAL rollups; \
                      the zones after {zone} were never opened, so this is not a corpus score: \
                      wat-route: {}, #423: {} ===", &*wr, &*r423);
            panic!("#805/#762: opening zone {zone:?}, but the {label} rollup already holds a hole \
                    that the terminal assert would only have reported at the END of a run that may \
                    never get there (#763). unaccounted (left the loop body without reaching add or \
                    skip — a corpus WIRING bug, not an asset problem): {orphans:?}; unmeasured (the \
                    water check ran and the .wtr did not load, so the zone was abandoned unwalked \
                    with no number — #762's exhibit): {unmeasured:?}. \
                    The water columns from this run are not a score. Only the FIRST offending \
                    rollup is named and the zones after {zone:?} were never opened, so treat this \
                    as a partial picture. `skipped` is not checked here — see this fn's doc.");
        }
        wr.begin_zone(zone);
        r423.begin_zone(zone);
    }

    /// **#805 REACH CONTROL — RED direction.** Drives `open_zone_checked` over a three-zone corpus
    /// whose SECOND zone is abandoned (the loop body closes neither `add` nor `skip`) and whose
    /// THIRD stands in for #763's blackburrow: a zone the run never gets past, so nothing after the
    /// loop ever executes. The corpus itself needs baked GLBs and hangs; this needs neither.
    ///
    /// This is a reach control for the HELPER, not for the fix: if `open_zone_checked`'s refusal is
    /// disabled, or its call is removed from THIS loop, control reaches the `UNREACHED` panics below
    /// and `should_panic`'s expected substring no longer matches, so the test goes RED. It says
    /// nothing about whether `faithful_walker_drift_corpus` still calls it — see that function's
    /// `#799` paragraph.
    #[test]
    #[should_panic(expected = "unaccounted (left the loop body without reaching add or skip")]
    fn zone_accounting_fires_before_the_corpus_loop_ends() {
        let mut wr = WaterRollup::new();
        let mut r423 = WaterRollup::new();
        for (i, zone) in ["zone_a", "zone_b_abandoned", "zone_c_never_terminates"].iter().enumerate() {
            open_zone_checked(&mut wr, &mut r423, zone);
            match i {
                0 => { wr.skip(zone, "no glb"); r423.skip(zone, "no glb"); }
                // THE DEFECT the guard exists to catch: an exit from the body closing neither.
                1 => continue,
                _ => panic!("UNREACHED: zone {zone:?} opened without the #805 gate firing on \
                             zone_b_abandoned — the gate is dead"),
            }
        }
        panic!("UNREACHED: the corpus loop ran to completion, so only a TERMINAL assert could have \
                caught the abandoned zone — which is exactly the #805 defect");
    }

    /// **#831 — the `[ABORTED …]` report's CONTENT is pinned by an execution-observable, not source
    /// text.** `open_zone_checked` prints a `[ABORTED while opening {zone}]` block before it panics
    /// (see its doc's N4 paragraph) — that block, not the panic message, is the honesty affordance:
    /// it is what tells a reader the run died mid-corpus and the water columns above it are not a
    /// score. The reviewer's R7 mutation on #824 deleted that `println!` entirely and the suite
    /// stayed green (11 passed / 0 failed): the three `should_panic` strings in this file pin only
    /// the PANIC message's bucket names, never the report block, so nothing noticed it was gone.
    ///
    /// A source-text grep for the `println!` call would have the same shape as the eight prior
    /// measured evasions in this project (#799) — it proves the call is WRITTEN, not that it RUNS.
    /// So this test instead runs the real defect fixture
    /// (`zone_accounting_fires_before_the_corpus_loop_ends`, above) as a SUBPROCESS with
    /// `--nocapture`, captures its actual stdout, and asserts the report's own words are IN the
    /// captured bytes: the `[ABORTED …]` tag naming the zone the run died on, `PARTIAL`, and the
    /// "not a corpus score" disclaimer. Only an interpreter that actually executed the `println!`
    /// could have produced them.
    ///
    /// **Mutation check (see PR body for the transcript of all three runs):**
    /// 1. Delete the `println!` in `open_zone_checked` → this test goes RED (the captured stdout no
    ///    longer contains the report), while the fixture test it shells out to still passes (its
    ///    `should_panic` string lives in the `panic!` two lines below the deleted call, untouched).
    /// 2. Wrap that same `println!` in `if false { … }` → RED for the identical reason: the call is
    ///    still fully compiled and present in the source text, so a text-presence pin would stay
    ///    green here, but it is never reached, so the captured stdout is still missing the report.
    /// 3. Restore the call → GREEN.
    #[test]
    fn aborted_report_content_is_pinned_by_execution() {
        let exe = std::env::current_exe().expect("test binary path (for the subprocess re-run)");
        let out = std::process::Command::new(&exe)
            .arg("zone_accounting_fires_before_the_corpus_loop_ends")
            .arg("--exact")
            .arg("--nocapture")
            .output()
            .expect("failed to spawn the inner test as a subprocess");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // The inner test is `#[should_panic(...)]`; libtest reports it "ok" and the process exits 0
        // when the expected panic occurred. A non-zero exit here means the FIXTURE broke — a
        // different failure from the one this test exists to catch — so fail loudly with both
        // streams rather than let a fixture regression masquerade as a report regression.
        assert!(out.status.success(),
            "fixture broke: zone_accounting_fires_before_the_corpus_loop_ends did not pass as a \
             subprocess (status {:?}) — this test cannot judge the report until that fixture is \
             healthy again.\nstdout:\n{stdout}\nstderr:\n{stderr}", out.status);
        assert!(stdout.contains("[ABORTED while opening zone_c_never_terminates]"),
            "the [ABORTED …] tag naming the zone the run died on is missing from the captured \
             stdout — the report was deleted, unreached, or renamed.\ncaptured stdout:\n{stdout}");
        assert!(stdout.contains("PARTIAL"),
            "the PARTIAL tag is missing from the captured stdout.\ncaptured stdout:\n{stdout}");
        assert!(stdout.contains("this is not a corpus score"),
            "the \"this is not a corpus score\" disclaimer is missing from the captured stdout.\n\
             captured stdout:\n{stdout}");
    }

    /// **#805 — GREEN direction, and the pin on `skipped` being exempt.** Every zone here is CLOSED,
    /// three by `skip` (a partial `$ZONE_DIR`, the normal state of a dev box) and one by a MEASURED
    /// `add`. The per-zone refusal must stay silent through all of them — otherwise the corpus would
    /// abort on the first missing `.glb` and never print the rest of its table — while the TERMINAL
    /// reconciliation still refuses to call the run complete. If `skipped` is ever added to
    /// `open_zone_checked`, this test goes RED, which is the intended alarm: that is a real change
    /// in what an operator with a partial asset dir gets to see, not a tidy-up.
    #[test]
    fn zone_accounting_stays_silent_while_every_zone_is_closed() {
        let mut wr = WaterRollup::new();
        let mut r423 = WaterRollup::new();
        let measured = ZoneWater::from_map(RegionMap::water_slab(-44.0, -4.0));
        for zone in ["akanon", "qeynos2", "gfaydark", "crushbone"] {
            open_zone_checked(&mut wr, &mut r423, zone);
            if zone == "crushbone" {
                wr.add(zone, &measured.measure(|_| 7usize));
                r423.add(zone, &measured.measure(|_| 0usize));
            } else {
                wr.skip(zone, "no glb"); r423.skip(zone, "no glb");
            }
        }
        assert!(wr.unaccounted_zones().is_empty(), "closed zones must not land in `unaccounted`");
        assert_eq!(wr.skipped_zones(), ["akanon", "qeynos2", "gfaydark"]);
        assert_eq!(wr.measured_total(), 7, "the `add`-closed zone's number must still count");
        // …and the whole-corpus question is still answered at the end, still in the negative.
        assert!(!wr.is_complete() && !r423.is_complete(),
            "three skipped zones is a hole; the terminal reconciliation must still refuse it");
    }

    /// **#805 — the `#423` arm, which is otherwise entirely unpinned (review N1/M_E).** The two
    /// rollups can DIVERGE: a corpus edit that settles `roll_wr` and forgets `roll_423` is exactly
    /// the wiring bug this refusal exists for, and a gate that only consulted the first rollup would
    /// pass every other test in this file. Here `wat-route` is clean and only `#423` is abandoned,
    /// so the refusal must fire on the SECOND arm — and the expected substring includes the `{label}`
    /// interpolation, so a mutation that hard-codes `"wat-route"` also goes RED.
    #[test]
    #[should_panic(expected = "the #423 rollup already holds a hole")]
    fn zone_accounting_covers_the_423_rollup_not_just_wat_route() {
        let mut wr = WaterRollup::new();
        let mut r423 = WaterRollup::new();
        open_zone_checked(&mut wr, &mut r423, "zone_a");
        wr.skip("zone_a", "no glb"); // wat-route closed — and `#423` deliberately is NOT.
        open_zone_checked(&mut wr, &mut r423, "zone_b");
        panic!("UNREACHED: zone_b opened with `#423` still holding an abandoned zone_a — the gate \
                only consults the wat-route rollup");
    }

    /// **#805 — `unmeasured` is refused per zone too (round-2 B1/B3).** Round 1 checked only
    /// `unaccounted` on the reasoning that an asset hole "is not answerable at zone 3 of 11". The
    /// reviewer measured that false: the buckets are push-only, so `unmeasured` is settled the
    /// instant a zone closes and can never un-fire. `unmeasured` is #762's motivating exhibit (a
    /// `.wtr` that did not load scoring as zero water), so leaving it terminal left the exhibit
    /// uncovered on precisely the non-terminating run #805 is about. This drives the real
    /// `ZoneWater::load` failure path — no fixture constructor, no `.wtr` on disk.
    #[test]
    #[should_panic(expected = "unmeasured (the water check ran and the .wtr did not load")]
    fn zone_accounting_refuses_an_unmeasured_zone_before_the_corpus_ends() {
        let mut wr = WaterRollup::new();
        let mut r423 = WaterRollup::new();
        // A directory that does not exist, so `load` really fails and `tally()` is `Unmeasured`.
        let nowhere = std::env::temp_dir().join("f805-no-such-water-dir");
        let zw = ZoneWater::load(&nowhere, "zone_a");
        assert!(!zw.is_measured(), "fixture precondition: this .wtr must NOT load");
        open_zone_checked(&mut wr, &mut r423, "zone_a");
        wr.add("zone_a", &zw.tally()); r423.add("zone_a", &zw.tally()); // closed, but with no data
        open_zone_checked(&mut wr, &mut r423, "zone_b");
        panic!("UNREACHED: zone_b opened after zone_a closed UNMEASURED — the run would have gone \
                on to report a water column with a hole in it, which is #762's exhibit");
    }

    /// **Rename guard for this file's doc-comment citations.** `open_zone_checked`'s rustdoc names
    /// `faithful_walker_drift_corpus`; listing it here as a `fn` value makes a rename a COMPILE
    /// error instead of a citation that rots silently. The nav crate's citation scan
    /// (`every_test_citation_in_the_five_citation_files_resolves_and_is_listed_in_a_guard`) requires
    /// exactly this and reads this file — measured, not assumed: the workspace suite failed with
    /// "`faithful_walker_drift_corpus` … no `_cited`/`_helpers` guard in this file names it" until
    /// this array existed. `aborted_report_content_is_pinned_by_execution`'s doc comment (#831) names
    /// `zone_accounting_fires_before_the_corpus_loop_ends` the same way — added below after the
    /// workspace suite failed on that citation too, for the identical reason.
    /// **Source-text anchors into `walker.rs`, re-found by execution (#919).**
    ///
    /// `faithful_walker_drift_corpus` restates four things that live in `walker.rs` and that an
    /// integration test cannot name: two `const`s private to `Walker::drive_walk`, an unnamed
    /// literal re-path cap, and the downhill-backoff branch's non-swim wish. Those comments used to
    /// cite `walker.rs` by LINE NUMBER. A line number is a coordinate in one commit: all three of
    /// the numbers #919 reported were already stale on `main` when they were reported, and by the
    /// commit that fixed them every one pointed at unrelated code. They now quote SOURCE TEXT, and
    /// this test re-finds each quoted phrase.
    ///
    /// `collision.rs` quotes the `LOCAL_BOUND` line for the same reason and is covered by the same
    /// anchor — the anchor is a property of `walker.rs`, not of one citing file.
    ///
    /// **What it does NOT do**, so nobody reads it as more:
    ///
    /// * it does not check that a citation is APT, only that the quoted text still exists in
    ///   `walker.rs` exactly once. A change that leaves the phrase standing while inverting what it
    ///   means still passes;
    /// * it does not FIND citations. The list below is hand-written, so a new comment citing
    ///   `walker.rs` is covered only if someone adds its anchor here. The nav crate's mechanical
    ///   citation scan cannot help: it reads lines beginning with three slashes, and these
    ///   citations are ordinary two-slash comments, which that scan structurally never visits;
    /// * it covers `walker.rs` only. Nothing here claims anything about citations into any other
    ///   file, in this test file or elsewhere;
    /// * it pins TEXT, not reachability. A phrase can be present and unreached (#799).
    ///
    /// **Reach control.** #778's source scanner silently covered about an eighth of its corpus and
    /// all twelve mutation probes aimed at it happened to sit inside the window it could still see,
    /// so twelve green cells proved nothing. So this scan reports how far it got: `lines_scanned` is
    /// asserted against the file's own line count and against a floor, and every anchor must be
    /// found. The anchors sit hundreds of lines apart, deep in a file thousands of lines long, so a
    /// scan that stopped early loses the late ones loudly instead of passing quietly.
    #[test]
    fn walker_source_anchors_cited_in_this_file_still_resolve() {
        // `tests/` sits at the workspace root, which is this package's manifest directory.
        let walker = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("crates/eqoxide-nav/src/walker.rs");
        let src = std::fs::read_to_string(&walker)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", walker.display()));

        // Each anchor is the source text a comment quotes, and what that comment claims about it.
        const ANCHORS: &[(&str, &str)] = &[
            ("const LOOK_AHEAD: f32 = 5.0;",
             "the private look-ahead this file's LOOK_AHEAD restates"),
            ("const LOCAL_BOUND: f32 = 40.0;",
             "the private fine-search window this file's LOCAL_BOUND restates (collision.rs too)"),
            ("if self.nav_repaths < 8 {",
             "the unnamed re-path cap this file's MAX_REPATHS restates"),
            ("if self.backoff_ticks > 0 {",
             "the downhill-backoff branch this file's backoff loop mirrors"),
            ("want_swim: false,",
             "that branch's unconditional non-swim wish"),
        ];

        // Interior whitespace is collapsed before matching, so an anchor survives rustfmt
        // re-aligning a struct literal, but not the code being renamed or deleted.
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut hits = vec![0usize; ANCHORS.len()];
        let mut lines_scanned = 0usize;
        for line in src.lines() {
            lines_scanned += 1;
            let n = norm(line);
            for (i, (anchor, _)) in ANCHORS.iter().enumerate() {
                if n.contains(anchor) { hits[i] += 1; }
            }
        }

        // ── reach control, before any anchor verdict is read ──
        assert_eq!(lines_scanned, src.lines().count(),
            "the scan stopped before the end of walker.rs — every anchor verdict below is unsound");
        assert!(lines_scanned >= 1000,
            "walker.rs read as only {lines_scanned} lines; the source tree is not where this test \
             thinks it is, and the anchors would fail for the wrong reason");

        let mut problems: Vec<String> = Vec::new();
        for (i, (anchor, claim)) in ANCHORS.iter().enumerate() {
            match hits[i] {
                1 => {}
                0 => problems.push(format!(
                    "walker.rs no longer contains `{anchor}`, cited here as {claim}. It was moved, \
                     renamed or deleted — re-read walker.rs and rewrite the comment that quotes it.")),
                n => problems.push(format!(
                    "walker.rs contains `{anchor}` on {n} lines, and it is cited here as {claim}, \
                     which reads as one site. Quote a longer, unique phrase.")),
            }
        }
        assert!(problems.is_empty(), "{} stale source-text citation(s) into walker.rs:\n  {}",
            problems.len(), problems.join("\n  "));
    }

    #[test]
    fn doc_comment_citations_in_this_file_are_rename_guarded() {
        let _cited: &[fn()] = &[
            // cited by `open_zone_checked`'s rustdoc (#805)
            faithful_walker_drift_corpus,
            // cited by `aborted_report_content_is_pinned_by_execution`'s rustdoc (#831)
            zone_accounting_fires_before_the_corpus_loop_ends,
        ];
    }


// ── relocated traversability.rs walker-sim tests ──

    /// **THE #386 DRIFT FIXTURE (RED on pre-Body main).** Every route the planner emits must be one
    /// the real controller can actually walk. On main the planner (probes 2.5/3.0) routed straight
    /// under the 3.5–6.5 lintel that the controller's 4.0 chest ray refuses — the walker pressed
    /// into it forever. With the shared [`Body`] the planner probes at the controller's own chest
    /// height and refuses the corridor, so it emits no un-walkable route.
    ///
    /// Mutation check: set the planner's chest probe back below 3.5 (e.g. the old 3.0) and this
    /// MUST go red — verified at authoring time.
    #[test]
    fn planner_never_routes_under_a_lintel_the_walker_collides_with() {
        let c = lintel_corridor();
        let start = [-20.0, 0.0, 0.0];
        let goal = [20.0, 0.0, 0.0];

        // Pin the fixture premise: the controller genuinely cannot cross the lintel.
        let mut ctrl = CharacterController::new(start);
        ctrl.on_ground = true;
        for _ in 0..600 {
            ctrl.step(MoveIntent { wish_dir: [1.0, 0.0], speed: RUN_SPEED, ..Default::default() },
                      1.0 / 60.0, &c);
        }
        assert!(ctrl.pos[0] < 0.0,
            "fixture premise: the controller must be blocked by the lintel (east={})", ctrl.pos[0]);

        // The invariant: whatever the planner answers, it must not be a route through the lintel.
        // (An honest "no route" is fine; a confident route the walker cannot walk is the #386 lie.)
        if let Some(route) = c.find_path(start, goal, PLAYER_RADIUS, &[], false) {
            let crossed = route.iter().any(|w| w[0] > 2.0);
            assert!(!crossed,
                "planner routed through a lintel the controller collides with (#386): {route:?}");
        }
    }

    /// **THE #420 FOOT-AXIS FIXTURE (the foot twin of the lintel test).** A low wall the controller's
    /// step-up cannot mount must ALSO block the planner. Same class as #386, different axis: the
    /// planner probed the foot band at `feet_clr` = 2.5 u while the controller contacts at `foot` =
    /// 0.5 u and recovers ≤ `foot + step_up` = 2.5 u via step-up — so an obstacle in (2.5, chest]
    /// with no walkable top is solid to the walker yet, if the planner ever stopped probing the foot
    /// band, clear to A*.
    ///
    /// Mutation check (verified at authoring time): make the planner skip the foot band — e.g.
    /// `planner_probes` → `[self.chest, self.chest]`, or `feet_clr()` raised above 3.0 — and the
    /// `can_traverse_fast` assertion below goes RED, because the 4.0 u chest ray clears the 3.0 u
    /// wall. The derivation `feet_clr = foot + step_up` is what makes that state unrepresentable.
    #[test]
    fn planner_never_routes_over_a_low_wall_the_walker_cant_step() {
        let c = low_wall_corridor();
        let start = [-20.0, 0.0, 0.0];
        let goal = [20.0, 0.0, 0.0];

        // Pin the fixture premise: the controller genuinely cannot cross (step-up tops out at 2.5 u;
        // default intent has no hop/jump, so a 3 u wall is a hard stop, exactly as a WASD player).
        let mut ctrl = CharacterController::new(start);
        ctrl.on_ground = true;
        for _ in 0..600 {
            ctrl.step(MoveIntent { wish_dir: [1.0, 0.0], speed: RUN_SPEED, ..Default::default() },
                      1.0 / 60.0, &c);
        }
        assert!(ctrl.pos[0] < 0.0,
            "fixture premise: the controller must be blocked by the low wall (east={})", ctrl.pos[0]);

        // The crisp foot-axis invariant: the planner's OWN edge test refuses the wall-crossing
        // segment. This is the assertion a reverted foot probe flips to `true`.
        let t = Traversability::new(&c, PLAYER_RADIUS, 8.0, 0.0, false);
        let west = Point::new([-6.0, 0.0], 0.0);
        let east = Point::new([6.0, 0.0], 0.0);
        assert!(!t.can_traverse_fast(west, east),
            "planner accepted a segment across a 3u wall the walker's step-up can't mount (#420)");

        // And end to end: an honest "no route" is fine; a route across the wall is the #420 lie.
        if let Some(route) = c.find_path(start, goal, PLAYER_RADIUS, &[], false) {
            let crossed = route.iter().any(|w| w[0] > 2.0);
            assert!(!crossed,
                "planner routed over a low wall the controller collides with (#420): {route:?}");
        }
    }

// ── #630: walk-edge acceptance must reflect the MAXIMUM local rise along the hop, not the
//    average grade — the #617 canal bank / #309 moat wall fixture ──

    /// The laundering geometry, synthetic (#630): a plain at z = 0 and a 12.8u mesa whose corner
    /// sits in the last ~15% of a DIAGONAL coarse hop (~11.31u run), so the hop's floor profile is
    /// flat-then-vertical. The whole-hop AVERAGE grade is 12.8/11.31 = 1.13 < MAX_WALK_GRADE = 1.2
    /// (the exact #617 numbers — the same face is rejected orthogonally at 12.8/8 = 1.6), and the
    /// planner's interpolated feet/chest rays have already climbed above the face by the time they
    /// reach it, so on unmodified main this admits a walk edge the controller's 2u step-up cannot
    /// climb. Verified to FAIL on unmodified origin/main (a route is returned) — see PR.
    ///
    /// `ramp` swaps the vertical faces for a planar ramp over [36..44]² with the SAME endpoints and
    /// the SAME total rise (z = 0.8·(e−36) + 0.8·(n−36); steepest ascent, along the hop diagonal,
    /// is grade 1.13): the profile the controller genuinely CAN walk. The pair pins the fix from
    /// both sides — same hop, same rise, only the PROFILE differs, and only the profile may decide.
    ///
    /// **The plain is L-shaped: it deliberately does NOT extend under the mesa** (no floor at z = 0
    /// over east > 42.8 ∧ north > 42.8). That is what makes the goal column `(60, 60)` contain the
    /// mesa top and nothing else, so *the only floor sequence to the goal is the profile under
    /// test.* An earlier revision of this fixture ran the plain under the mesa and sealed it with
    /// walls in the vertical-face branch only; the ramp branch was then routable at z = 0 the whole
    /// way, and its `route.is_some()` assertion passed even with `walk_profile_ok` hard-wired to
    /// reject every rising walk edge — a vacuous over-tightening guard. Keep the goal column
    /// single-floored, or the guard silently stops guarding.
    fn mesa_scene(ramp: bool) -> Collision {
        let mut terrain = vec![
            // The plain, L-shaped — open ground everywhere EXCEPT under the mesa (see above).
            floor_at(0.0, 0.0, 42.8, 0.0, 80.0),
            floor_at(0.0, 42.8, 80.0, 0.0, 42.8),
            floor_at(12.8, 42.8, 80.0, 42.8, 80.0),  // the mesa top
        ];
        if ramp {
            // Planar corner ramp [36..44]², rising 0 → 12.8 toward the mesa corner (same traversal
            // order as floor_at, so the winding — and thus the face normal — is up-facing).
            terrain.push(mesh(vec![
                [36.0, 0.0, 36.0], [44.0, 6.4, 36.0], [44.0, 12.8, 44.0], [36.0, 6.4, 44.0]]));
        } else {
            // Vertical 12.8u faces sealing the mesa's low-side edges (east-facing at east = 42.8,
            // north-facing at north = 42.8) — the canal-bank / moat-wall profile.
            terrain.push(panel(42.8, 42.8, 80.0, 0.0, 12.8));
            terrain.push(mesh(vec![
                [42.8, 0.0, 42.8], [42.8, 0.0, 80.0], [42.8, 12.8, 80.0], [42.8, 12.8, 42.8]]));
        }
        col(terrain)
    }

    /// **The #630 regression fixture, rejecting half.** The controller cannot climb the 12.8u face
    /// (pinned below — capability is the ground truth the planner was contradicting), so the honest
    /// planner answer is `None`/no_path — NOT a confident route up the face that wedges the walker
    /// after 8 re-paths (#617's `blocked`/`walker_stalled`). Also pins the diagnostic: the trace
    /// must show the laundered hop rejected as `local_rise`, and must show NO accepted walk edge
    /// climbing more than the controller's envelope onto the mesa.
    ///
    /// Mutation checks (see PR): (1) removing the `walk_profile_ok` call (unmodified main) → a route
    /// up the face is returned → RED; (2) loosening the envelope's step term to the old STEP_H = 20
    /// → RED. Re-derived after the scene was reshaped (`walk_profile_ok` → `return true`, the
    /// main-equivalent, still RED here) — the earlier evidence was collected on the old scene.
    #[test]
    fn planner_never_routes_up_a_vertical_face_a_diagonal_hop_launders() {
        let c = mesa_scene(false);
        let start = [12.0, 12.0, 0.0];
        let goal = [60.0, 60.0, 12.8];

        // Pin the fixture premise: the controller genuinely cannot climb the face. Drive it
        // straight at the mesa corner exactly like the laundered hop would (diagonal wish).
        let mut ctrl = CharacterController::new([36.0, 36.0, 0.0]);
        ctrl.on_ground = true;
        for _ in 0..600 {
            ctrl.step(MoveIntent { wish_dir: [0.7071, 0.7071], speed: RUN_SPEED, ..Default::default() },
                      1.0 / 60.0, &c);
        }
        assert!(ctrl.pos[2] < 6.0,
            "fixture premise: the controller must be stopped at the 12.8u face (pos={:?})", ctrl.pos);

        // The invariant: no route. Every entry onto the mesa concentrates 12.8u of rise into a
        // near-vertical face; a planner that admits one is lying about walkability (#617/#309).
        let route = c.find_path(start, goal, PLAYER_RADIUS, &[], false);
        assert!(route.is_none(),
            "planner routed up a 12.8u vertical face via a diagonal hop (#630): {route:?}");

        // And the honest WHY (#608 diagnostics): the laundered hop is rejected as `local_rise`,
        // and no accepted walk edge climbs past the controller's envelope onto the mesa.
        use eqoxide::nav::diagnostics::{EdgeKind, EdgeVerdict, RejectReason, SearchTrace, TRACE_EDGE_CAP};
        let trace = std::sync::Arc::new(std::sync::Mutex::new(SearchTrace::with_budget(TRACE_EDGE_CAP)));
        let ctx = PlanCtx::worker().ensure_budget().with_trace(trace.clone());
        let _ = c.find_path_res(start, goal, PLAYER_RADIUS, &[], false, 8.0, None, 0.0, ctx);
        let t = trace.lock().unwrap();
        let local_rise_rejects = t.calls.iter().flat_map(|call| &call.edges)
            .filter(|e| matches!(e.verdict, EdgeVerdict::Rejected { reason: RejectReason::LocalRise }))
            .count();
        assert!(local_rise_rejects > 0,
            "the laundered face hop must be rejected as local_rise (found none in the trace)");
        let bad_walk_accepts: Vec<_> = t.calls.iter().flat_map(|call| &call.edges)
            .filter(|e| matches!(e.verdict, EdgeVerdict::Accepted { kind: EdgeKind::Walk })
                && e.to[2] - e.from[2] > 6.9) // > spacing·MAX_WALK_GRADE + step_up, the envelope cap
            .collect();
        assert!(bad_walk_accepts.is_empty(),
            "walk edges accepted past the controller's climb envelope: {bad_walk_accepts:?}");
    }

    /// **The #630 fixture, accepting half — the over-tightening guard.** Same mesa, same hop, same
    /// 12.8u total rise, but spread uniformly along a planar ramp (grade 1.13 on the steepest
    /// line): the controller CAN walk this (pinned below), so the planner must still admit it.
    /// A fix that turned this into `no_path` would trade the #617 wedge for a "can't leave spawn"
    /// regression — honest, but a different lie about the world.
    ///
    /// The guard only bites because the plain does not run under the mesa (see `mesa_scene`), so
    /// the goal column holds the mesa top alone and a route MUST climb the ramp. `route.is_some()`
    /// on its own would not be enough even so — it cannot tell a climb from a detour — so the
    /// route is also required to contain an intermediate altitude, i.e. to have used the ramp.
    /// Mutation-verified: hard-wiring `walk_profile_ok` to `return false` (reject every rising
    /// walk edge — maximal over-tightening) turns this test RED.
    #[test]
    fn planner_still_routes_up_a_genuinely_walkable_ramp_of_the_same_rise() {
        let c = mesa_scene(true);
        let start = [12.0, 12.0, 0.0];
        let goal = [60.0, 60.0, 12.8];

        // Pin the capability premise: the controller walks the grade-1.13 ramp to the top.
        let mut ctrl = CharacterController::new([36.0, 36.0, 0.0]);
        ctrl.on_ground = true;
        let mut topped = false;
        for _ in 0..900 {
            ctrl.step(MoveIntent { wish_dir: [0.7071, 0.7071], speed: RUN_SPEED, ..Default::default() },
                      1.0 / 60.0, &c);
            if ctrl.on_ground && ctrl.pos[2] > 12.0 {
                topped = true;
                break;
            }
        }
        assert!(topped,
            "capability premise: the controller must walk the grade-1.13 ramp (ended at {:?})", ctrl.pos);

        // The invariant: the planner still admits the walkable profile — no over-tightening.
        let route = c.find_path(start, goal, PLAYER_RADIUS, &[], false);
        assert!(route.is_some(),
            "planner refused a ramp the controller demonstrably walks (#630 over-tightening)");
        assert!(route.as_ref().unwrap().iter().any(|w| w[2] > 0.5 && w[2] < 12.5),
            "route must climb the RAMP (an intermediate z), not sneak to the goal on the flat: \
             {route:?}");
    }


// ─────────────────────────── #639 goal-append BLAST RADIUS ───────────────────────────

/// **#639 goal-append blast radius over baked zones — REAL `CharacterController` verdict.**
///
/// The dominant risk of the goal-append walk-edge check (#639) is OVER-TIGHTENING: a route that used
/// to return complete now returns `goal_not_walkable`. Some losses are the planner CORRECTLY refusing
/// a final hop the walker could never execute (honest); a regression would be a REACHABLE goal now
/// stranded. This distinguishes them WITHOUT a two-build diff:
///
/// **LOST is provable single-build.** On `main`, `Unreachable(GoalNotWalkable)` is returned ONLY when
/// the goal has NO floor anywhere in its column (`collision.rs`, the pre-search immediate fail). The
/// #639 check is the ONLY code path that returns `GoalNotWalkable` for a goal that HAS a column floor
/// (A* reached the goal cell, then the appended final hop failed the walk-edge predicate). So a pair
/// that comes back `GoalNotWalkable` AND whose goal has a column floor is EXACTLY a route #639 newly
/// refuses — one `main` returned as complete. And #639 only ADDS refusals, so GAINED = 0 by
/// construction.
///
/// For each such LOST pair the REAL controller renders the verdict: drive it (faithful walker) to the
/// reachable LOW tier under the goal, then push straight at the goal for a further stretch, and report
/// whether it ever comes to rest at the goal's OWN floor (`arrived_at_goal_tier`). If it cannot, the
/// #639 refusal was HONEST — the walker physically cannot climb onto the goal. An `arrived_at_goal_tier`
/// on a LOST pair would be a genuine over-tightening regression.
///
/// ```text
/// ZONE_DIR=~/.local/share/eqoxide/assets/models \
///   cargo test --release --test walker_sim goal_append_blast_radius -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires baked zone glbs at $ZONE_DIR; the #639 goal-append over-tightening blast radius"]
fn goal_append_blast_radius() {
    use eqoxide::nav::collision::NoRoute;
    const LOOK_AHEAD: f32 = 5.0;
    const DT: f32 = 1.0 / 100.0;
    const FRAMES_PER_TICK: u32 = 15;
    const GOAL_TIER_TOL: f32 = 8.0;

    // Faithful walker to `approach` (a routable point), THEN a straight push at `goal` xy. Returns the
    // controller's final resting position. Drives WALK legs (the #639 losses are all dry land faces).
    let drive_and_push = |col: &Collision, start: [f32; 3], approach: [f32; 3], goal: [f32; 3]| -> [f32; 3] {
        let PlanOutcome::Route(coarse) = col.find_path_ex(
            start, approach, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()) else { return start };
        let mut ctrl = CharacterController::new(start);
        ctrl.on_ground = true;
        let mut path_i = 0usize;
        // Phase 1: follow the coarse route to the low-tier approach (fast pure pursuit, no re-plan —
        // the approach is known-routable, we only need to GET the walker to the goal's foot).
        for _ in 0..180 {
            let (px, py, pz) = (ctrl.pos[0], ctrl.pos[1], ctrl.pos[2]);
            if (px - approach[0]).hypot(py - approach[1]) < 4.0 { break; }
            while path_i + 2 < coarse.len() {
                let (a, b) = (coarse[path_i], coarse[path_i + 1]);
                let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
                let t = if l2 < 1e-6 { 1.0 } else { ((px - a[0]) * ab[0] + (py - a[1]) * ab[1] + (pz - a[2]) * ab[2]) / l2 };
                if t >= 1.0 { path_i += 1; } else { break; }
            }
            let carrot = carrot_along(&coarse, path_i, [px, py, pz], LOOK_AHEAD).unwrap_or(approach);
            let (dx, dy) = (carrot[0] - px, carrot[1] - py);
            let d = (dx * dx + dy * dy).sqrt().max(1e-3);
            for _ in 0..FRAMES_PER_TICK {
                ctrl.step(MoveIntent { wish_dir: [dx / d, dy / d], wish_vspeed: 0.0, jump: false,
                    want_swim: false, speed: RUN_SPEED, climb: 0.0, hop: false }, DT, col);
            }
        }
        // Phase 2: push STRAIGHT at the goal XY for ~4 s, hopping — the walker's honest best effort to
        // mount the final face. If it can climb, it reaches the goal's tier here; if not, it wedges.
        for _ in 0..80 {
            let (px, py) = (ctrl.pos[0], ctrl.pos[1]);
            let (dx, dy) = (goal[0] - px, goal[1] - py);
            let d = (dx * dx + dy * dy).sqrt().max(1e-3);
            for _ in 0..FRAMES_PER_TICK {
                ctrl.step(MoveIntent { wish_dir: [dx / d, dy / d], wish_vspeed: 0.0, jump: false,
                    want_swim: false, speed: RUN_SPEED, climb: 0.0, hop: true }, DT, col);
            }
        }
        ctrl.pos
    };

    let dir = std::env::var("ZONE_DIR")
        .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
    let zones: Vec<String> = std::env::var("ZONES").ok()
        .map(|z| z.split(',').map(str::to_string).collect())
        .unwrap_or_else(|| ["akanon", "blackburrow", "qeynos2", "gfaydark", "crushbone", "neriaka",
            "felwithea", "highpass", "everfrost", "butcher", "cazicthule", "oasis"]
            .into_iter().map(str::to_string).collect());

    let mut seed: u64 = 0x639A_11CE;
    let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };
    let unit = |r: u32| r as f32 / u32::MAX as f32;

    let (mut g_pairs, mut g_routed, mut g_lost, mut g_drove, mut g_over) = (0usize, 0usize, 0usize, 0usize, 0usize);
    println!("\n=== #639 goal-append blast radius (LOST = goal_not_walkable WITH a column floor) ===");
    println!("{:<12} {:>6} {:>7} {:>6} {:>7} {:>9}", "zone", "pairs", "routed", "lost", "drove", "over_tight");
    // #807: a COVERAGE LEDGER for this corpus, not just a list of `.wtr` failures. It folds one
    // real per-zone water number — the count of sampled start/goal pairs the water filter excluded
    // — so the line it prints is a measurement rather than a placeholder, and it refuses to call
    // itself complete while any zone was dropped for any of the three reasons.
    let mut cover = WaterRollup::new();
    for zone in &zones {
        // #807: `open_corpus_zone` owns ALL THREE of this loop's zone-abandoning drop paths — no
        // baked `.glb`, an empty collision grid, a `.wtr` that did not load — and accounts for each
        // of them on `cover` before it returns. `cover.add(...)` at the bottom of the body closes
        // the zone; anything that leaves the body without reaching it is recorded as `unaccounted`
        // by the rollup itself. Before this, only the third was refused: the first two were bare
        // `continue`s that touched no ledger, so a host missing one zone's glb printed a clean
        // TOTAL over a smaller corpus than the one this test names.
        //
        // Water matters here even though this is not a water corpus: it is this corpus' PAIR FILTER
        // (`in_water(s) || in_water(g) → skip`). With no region map the filter silently passes
        // everything and wet pairs get scored as dry land, so the table would describe a different
        // corpus than the one it names — the zone is UNMEASURED, never a zone with no water (#762).
        let (col, zw) = match open_corpus_zone(&mut cover, std::path::Path::new(&dir), zone, 32.0) {
            Ok(ready) => ready,
            Err(why) => { println!("{zone:<12} ({why})"); continue }
        };

        let (mut z_pairs, mut z_routed, mut z_lost, mut z_drove, mut z_over) = (0usize, 0usize, 0usize, 0usize, 0usize);
        let mut z_wet = 0usize; // #807: what the water filter actually excluded — the rollup's number
        let mut tries = 0;
        while z_pairs < 120 && tries < 6000 {
            tries += 1;
            let e = col.origin[0] + unit(rnd()) * (col.cols as f32 * col.cell_size);
            let n = col.origin[1] + unit(rnd()) * (col.rows as f32 * col.cell_size);
            let Some(z) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { continue };
            let ang = unit(rnd()) * std::f32::consts::TAU;
            let d = 120.0 + unit(rnd()) * 280.0;
            let (ge, gn) = (e + d * ang.cos(), n + d * ang.sin());
            let Some(gz) = col.nearest_floor(ge, gn, z, 400.0, 400.0) else { continue };
            let s = [e, n, z];
            let g = [ge, gn, gz];
            if col.in_water(s) || col.in_water(g) { z_wet += 1; continue; }
            z_pairs += 1;

            let outcome = col.find_path_ex(s, g, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker());
            if matches!(outcome, PlanOutcome::Route(_)) { z_routed += 1; continue; }
            // LOST iff GoalNotWalkable AND the goal has a floor in its column (main would have routed).
            let is_gnw = matches!(outcome, PlanOutcome::Unreachable { reason: NoRoute::GoalNotWalkable, .. });
            if !is_gnw || col.snap_goal_to_column_floor(g).is_none() { continue; }
            z_lost += 1;

            // Controller verdict on the first few LOST pairs/zone (driving is the costly part).
            if z_drove < 6 {
                // Approach = the goal XY at the LOWEST column floor A* CAN route to (main's reached tier).
                let mut floors = col.column_floors(g[0], g[1], z, 400.0, 400.0);
                floors.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let approach = floors.iter().map(|&f| [g[0], g[1], f])
                    .find(|&pt| matches!(col.find_path_ex(s, pt, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()), PlanOutcome::Route(_)));
                if let Some(app) = approach {
                    z_drove += 1;
                    let end = drive_and_push(&col, s, app, g);
                    let at_goal_xy = (end[0] - g[0]).hypot(end[1] - g[1]) < 4.0;
                    let at_goal_z = (end[2] - gz).abs() <= GOAL_TIER_TOL;
                    let arrived_at_goal_tier = at_goal_xy && at_goal_z;
                    if arrived_at_goal_tier { z_over += 1; }
                    println!("  LOST {zone} s[{:.0},{:.0},{:.0}] g[{:.0},{:.0},{:.1}] approach_z {:.1} \
                             end[{:.0},{:.0},{:.1}] arrived_at_goal_tier {}",
                        s[0], s[1], s[2], g[0], g[1], g[2], app[2], end[0], end[1], end[2], arrived_at_goal_tier as u8);
                }
            }
        }
        println!("{zone:<12} {z_pairs:>6} {z_routed:>7} {z_lost:>6} {z_drove:>7} {z_over:>9}");
        g_pairs += z_pairs; g_routed += z_routed; g_lost += z_lost; g_drove += z_drove; g_over += z_over;
        cover.add(zone, &zw.measure(|_| z_wet)); // #807: CLOSE the zone — forgetting makes it `unaccounted`
    }
    println!("\nTOTAL pairs {g_pairs}  routed {g_routed}  LOST(newly-refused) {g_lost}  drove {g_drove}  OVER-TIGHTENED {g_over}");
    println!("(gained = 0 by construction: #639 only ADDS refusals. OVER-TIGHTENED must be 0 — any LOST \
             pair where the REAL controller reached the goal's own tier is a regression.)");
    // #839: the accounting assert must fire BEFORE `g_pairs > 0` — a run that skipped/lost every
    // zone is still RED either way, but ordered this way the failure names WHICH zones dropped and
    // why, instead of reporting the pre-#807 "set ZONE_DIR" message over an accounting hole it
    // could have diagnosed.
    println!("wet start/goal pairs excluded by the water filter: {cover}");
    assert!(cover.is_complete(),
        "#807: the TOTALs above are not this corpus — they cover {}/{} of the zones it was asked \
         for, and are not comparable to a run that had them all. unmeasured (the .wtr was read and \
         did not load): {:?}; skipped (dropped before the water check ran — no glb / no grid): \
         {:?}; unaccounted (left the loop body without reaching add or skip — a corpus WIRING bug, \
         not an asset problem): {:?}",
        cover.measured_zones(), cover.attempted_zones(),
        cover.unmeasured_zones(), cover.skipped_zones(), cover.unaccounted_zones());
    assert!(g_pairs > 0, "no zones loaded — set ZONE_DIR to the baked glbs");
    assert_eq!(g_over, 0,
        "#639 over-tightening: {g_over} LOST pair(s) were reachable by the REAL controller — the goal-\
         append check refused a goal the walker can actually stand on. Investigate the printed pairs.");
}

/// **#685 corner-buffer inflation blast radius over baked zones — REAL `CharacterController` A/B.**
///
/// The PRIMARY fix (owner-directed) is `Collision::inflate_route_off_corners`: it pushes coarse-route
/// waypoints OFF convex wall corners by `radius + buffer`, so the walker takes one smooth wider arc
/// instead of hugging/wiggling the apex. Its dominant RISK is OVER-TIGHTENING a corridor — shoving a
/// waypoint into the far wall and BREAKING a narrow-but-passable route. This measures the blast radius
/// by driving the production `CharacterController` over routable start/goal pairs TWICE per pair —
/// once on the PLAIN coarse route, once on the INFLATED route — with the carrot LOS clamp
/// (`carrot_los_clear`) ON in BOTH (the shipped config), so the ONLY variable is the inflation. Reports:
///   * BROKEN   — completed on the plain route but NOT the inflated one (inflation broke a route). Must be 0.
///   * GAINED   — completed on the inflated route but not the plain one (a corner wedge inflation cleared).
///   * SMOOTHED — of pairs that complete BOTH ways, how many turn LESS on the inflated route (smoother),
///                and the mean reduction in total turning (radians) — the anti-wiggle signal.
///   * SLOWDOWN — ticks-inflated / ticks-plain on both-complete pairs. Must be ~1.0 (no crawl on open ground).
///
/// This models COARSE-tier pursuit (the tier the inflation reshapes); the live client also has the fine
/// tier + re-plan, so GAINED here is a coarse-only proxy — but BROKEN, SLOWDOWN and the narrow-corridor
/// safety are a valid A/B regardless.
///
/// ```text
/// ZONE_DIR=~/.local/share/eqoxide/assets/models \
///   cargo test --release --test walker_sim corner_buffer_blast_radius -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires baked zone glbs at $ZONE_DIR; the #685 corner-buffer inflation blast radius"]
fn corner_buffer_blast_radius() {
    const LOOK_AHEAD: f32 = 5.0;
    const STOP_DIST: f32 = 2.0;
    const Z_TOL: f32 = 8.0;
    const DT: f32 = 1.0 / 100.0;
    const FRAMES_PER_TICK: u32 = 15;
    const MAX_TICKS: u32 = 300; // ~45 s of sim per journey — generous headroom over any real route
    const CORNER_BUFFER: f32 = 2.0; // must match walker.rs CORNER_BUFFER
    let pairs_per_zone: usize = std::env::var("PAIRS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);

    // Drive the REAL controller along `route` with LOS-clamped pure pursuit (shipped config). Returns
    // (arrived, ticks, distance_walked, total_turning_radians). `route` is either the plain coarse
    // route or the inflated one — the A/B variable.
    let run = |col: &Collision, route: &[[f32; 3]], goal: [f32; 3]| -> (bool, u32, f32, f32) {
        let r = PLAYER_RADIUS;
        let mut ctrl = CharacterController::new(route[0]);
        ctrl.on_ground = true;
        let mut path_i = 0usize;
        let (mut walked, mut turning) = (0.0f32, 0.0f32);
        let mut prev = ctrl.pos;
        let mut prev_head: Option<f32> = None;
        for tick in 0..MAX_TICKS {
            let (px, py, pz) = (ctrl.pos[0], ctrl.pos[1], ctrl.pos[2]);
            if (px - goal[0]).hypot(py - goal[1]) < STOP_DIST && (pz - goal[2]).abs() <= Z_TOL {
                return (true, tick, walked, turning);
            }
            while path_i + 2 < route.len() {
                let (a, b) = (route[path_i], route[path_i + 1]);
                let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
                let t = if l2 < 1e-6 { 1.0 } else { ((px - a[0]) * ab[0] + (py - a[1]) * ab[1] + (pz - a[2]) * ab[2]) / l2 };
                if t >= 1.0 { path_i += 1; } else { break; }
            }
            let aim = carrot_along_los(route, path_i, [px, py, pz], LOOK_AHEAD, |a, b| col.carrot_los_clear(a, b, r))
                .unwrap_or(goal);
            let (dx, dy) = (aim[0] - px, aim[1] - py);
            let d = (dx * dx + dy * dy).sqrt().max(1e-3);
            for _ in 0..FRAMES_PER_TICK {
                ctrl.step(MoveIntent { wish_dir: [dx / d, dy / d], wish_vspeed: 0.0, jump: false,
                    want_swim: false, speed: RUN_SPEED, climb: 0.0, hop: false }, DT, col);
            }
            // Smoothness: accumulate |Δheading| of ACTUAL movement (wiggle shows up as turning).
            let (mx, my) = (ctrl.pos[0] - prev[0], ctrl.pos[1] - prev[1]);
            if mx.hypot(my) > 1e-3 {
                let h = my.atan2(mx);
                if let Some(ph) = prev_head {
                    let mut dh = h - ph;
                    while dh > std::f32::consts::PI { dh -= std::f32::consts::TAU; }
                    while dh < -std::f32::consts::PI { dh += std::f32::consts::TAU; }
                    turning += dh.abs();
                }
                prev_head = Some(h);
            }
            walked += mx.hypot(my);
            prev = ctrl.pos;
        }
        (false, MAX_TICKS, walked, turning)
    };

    let dir = std::env::var("ZONE_DIR")
        .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
    let zones: Vec<String> = std::env::var("ZONES").ok()
        .map(|z| z.split(',').map(str::to_string).collect())
        .unwrap_or_else(|| ["akanon", "blackburrow", "qeynos2", "gfaydark", "crushbone", "neriaka",
            "felwithea", "highpass", "everfrost", "butcher", "cazicthule", "oasis"]
            .into_iter().map(str::to_string).collect());

    let mut seed: u64 = 0x685A_11CE;
    let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };
    let unit = |r: u32| r as f32 / u32::MAX as f32;

    let (mut g_pairs, mut g_both, mut g_broken, mut g_gained, mut g_smoothed) = (0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut g_ticks_inf, mut g_ticks_plain) = (0u64, 0u64);
    let (mut g_moved_wp, mut g_total_wp, mut g_routes_touched) = (0usize, 0usize, 0usize);
    let (mut g_turn_inf, mut g_turn_plain) = (0.0f64, 0.0f64);
    println!("\n=== #685 corner-buffer inflation blast radius (A/B: inflated route vs plain, LOS clamp on both) ===");
    println!("{:<12} {:>6} {:>5} {:>6} {:>6} {:>8} {:>9}", "zone", "pairs", "both", "broken", "gained", "smoothed", "slowdown");
    // #807: a COVERAGE LEDGER for this corpus, not just a list of `.wtr` failures. It folds one
    // real per-zone water number — the count of sampled start/goal pairs the water filter excluded
    // — so the line it prints is a measurement rather than a placeholder, and it refuses to call
    // itself complete while any zone was dropped for any of the three reasons.
    let mut cover = WaterRollup::new();
    for zone in &zones {
        // #807: `open_corpus_zone` owns ALL THREE of this loop's zone-abandoning drop paths — no
        // baked `.glb`, an empty collision grid, a `.wtr` that did not load — and accounts for each
        // of them on `cover` before it returns. `cover.add(...)` at the bottom of the body closes
        // the zone; anything that leaves the body without reaching it is recorded as `unaccounted`
        // by the rollup itself. Before this, only the third was refused: the first two were bare
        // `continue`s that touched no ledger, so a host missing one zone's glb printed a clean
        // TOTAL over a smaller corpus than the one this test names.
        //
        // Water matters here even though this is not a water corpus: it is this corpus' PAIR FILTER
        // (`in_water(s) || in_water(g) → skip`). With no region map the filter silently passes
        // everything and wet pairs get scored as dry land, so the table would describe a different
        // corpus than the one it names — the zone is UNMEASURED, never a zone with no water (#762).
        let (col, zw) = match open_corpus_zone(&mut cover, std::path::Path::new(&dir), zone, 32.0) {
            Ok(ready) => ready,
            Err(why) => { println!("{zone:<12} ({why})"); continue }
        };

        let (mut z_pairs, mut z_both, mut z_broken, mut z_gained, mut z_smoothed) = (0usize, 0usize, 0usize, 0usize, 0usize);
        let mut z_wet = 0usize; // #807: what the water filter actually excluded — the rollup's number
        let (mut z_ti, mut z_tp) = (0u64, 0u64);
        let mut tries = 0;
        while z_pairs < pairs_per_zone && tries < pairs_per_zone * 70 + 500 {
            tries += 1;
            let e = col.origin[0] + unit(rnd()) * (col.cols as f32 * col.cell_size);
            let n = col.origin[1] + unit(rnd()) * (col.rows as f32 * col.cell_size);
            let Some(z) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { continue };
            let ang = unit(rnd()) * std::f32::consts::TAU;
            let d = 120.0 + unit(rnd()) * 280.0;
            let (ge, gn) = (e + d * ang.cos(), n + d * ang.sin());
            let Some(gz) = col.nearest_floor(ge, gn, z, 400.0, 400.0) else { continue };
            let (s, g) = ([e, n, z], [ge, gn, gz]);
            if col.in_water(s) || col.in_water(g) { z_wet += 1; continue; } // dry-land corners only
            let PlanOutcome::Route(coarse) = col.find_path_ex(s, g, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()) else { continue };
            if coarse.len() < 3 { continue; } // a straight 2-point route has no corner to inflate
            z_pairs += 1;

            let goal = *coarse.last().unwrap();
            let mut inflated = coarse.clone();
            col.inflate_route_off_corners(&mut inflated, PLAYER_RADIUS, CORNER_BUFFER);
            let moved = coarse.iter().zip(inflated.iter())
                .filter(|(a, b)| (a[0] - b[0]).hypot(a[1] - b[1]) > 0.05).count();
            g_moved_wp += moved; g_total_wp += coarse.len();
            if moved > 0 { g_routes_touched += 1; }
            let (arr_p, t_p, _wp, turn_p) = run(&col, &coarse, goal);
            let (arr_i, t_i, _wi, turn_i) = run(&col, &inflated, goal);
            if arr_p && !arr_i { z_broken += 1;
                println!("  BROKEN {zone} s[{:.0},{:.0},{:.0}] g[{:.0},{:.0},{:.0}] wp {} (inflation broke a route plain completed)",
                    s[0], s[1], s[2], g[0], g[1], g[2], coarse.len());
            }
            if arr_i && !arr_p { z_gained += 1; }
            if arr_i && arr_p {
                z_both += 1; z_ti += t_i as u64; z_tp += t_p as u64;
                g_turn_inf += turn_i as f64; g_turn_plain += turn_p as f64;
                if turn_i < turn_p - 0.05 { z_smoothed += 1; }
            }
        }
        let slow = if z_tp > 0 { z_ti as f64 / z_tp as f64 } else { 1.0 };
        println!("{zone:<12} {z_pairs:>6} {z_both:>5} {z_broken:>6} {z_gained:>6} {z_smoothed:>8} {slow:>9.3}");
        g_pairs += z_pairs; g_both += z_both; g_broken += z_broken; g_gained += z_gained; g_smoothed += z_smoothed;
        cover.add(zone, &zw.measure(|_| z_wet)); // #807: CLOSE the zone — forgetting makes it `unaccounted`
        g_ticks_inf += z_ti; g_ticks_plain += z_tp;
    }
    let slowdown = if g_ticks_plain > 0 { g_ticks_inf as f64 / g_ticks_plain as f64 } else { 1.0 };
    let turn_ratio = if g_turn_plain > 0.0 { g_turn_inf / g_turn_plain } else { 1.0 };
    println!("\nTOTAL pairs {g_pairs}  both-complete {g_both}  BROKEN {g_broken}  GAINED {g_gained}  SMOOTHED {g_smoothed}  \
             SLOWDOWN {slowdown:.4}  turning(inflated/plain) {turn_ratio:.3}");
    println!("INFLATION FIRED: {g_routes_touched} of the sampled routes had >=1 waypoint moved; {g_moved_wp}/{g_total_wp} waypoints offset off a wall.");
    println!("(BROKEN must be 0 — a route the plain coarse route completed that inflation broke is a narrow-corridor \
             over-tightening. SLOWDOWN must be ~1.0. turning<1.0 and SMOOTHED>0 is the anti-wiggle win.)");
    // #839: the accounting assert must fire BEFORE `g_pairs > 0` — a run that skipped/lost every
    // zone is still RED either way, but ordered this way the failure names WHICH zones dropped and
    // why, instead of reporting the pre-#807 "set ZONE_DIR" message over an accounting hole it
    // could have diagnosed.
    println!("wet start/goal pairs excluded by the water filter: {cover}");
    assert!(cover.is_complete(),
        "#807: the TOTALs above are not this corpus — they cover {}/{} of the zones it was asked \
         for, and are not comparable to a run that had them all. unmeasured (the .wtr was read and \
         did not load): {:?}; skipped (dropped before the water check ran — no glb / no grid): \
         {:?}; unaccounted (left the loop body without reaching add or skip — a corpus WIRING bug, \
         not an asset problem): {:?}",
        cover.measured_zones(), cover.attempted_zones(),
        cover.unmeasured_zones(), cover.skipped_zones(), cover.unaccounted_zones());
    assert!(g_pairs > 0, "no zones loaded — set ZONE_DIR to the baked glbs");
    assert_eq!(g_broken, 0,
        "#685 over-tightening: {g_broken} route(s) the plain coarse route completed FAILED after inflation — \
         the corner-buffer offset broke a passable route (likely a narrow corridor). Investigate the printed pairs.");
    assert!(slowdown < 1.10,
        "#685: inflation slowed both-completing routes by {:.1}% (ticks_inf/ticks_plain={slowdown:.4}) — the \
         inflated route must not crawl.", (slowdown - 1.0) * 100.0);
}

/// **#693 descent-guard blast radius over baked zones — REAL `CharacterController`, cross-build A/B.**
///
/// The #693 fix adds `descent_corridor_clear` to every descent edge family (controlled fall, water
/// descent, water entry, surface entry, and the same-cell / final-hop goal descents): a descent
/// landing must be the FIRST surface below the takeoff in the destination column — a lower tier
/// stacked beneath solid ground (the qeynos street over the qcat aqueduct) is not reachable by
/// falling through it. Its dominant RISK is OVER-TIGHTENING: refusing a legitimate lip / hole /
/// ramp / water descent.
///
/// This harness measures ONE build. Run it (identical seeds ⇒ identical sampled pairs, since the
/// floor model is untouched by the fix) on unmodified `main` and on the fix branch, then diff the
/// `PAIR` lines:
///   * `route+complete` → `refused`      = a LEGIT route lost — a regression, must be 0;
///   * `route+INCOMPLETE` → `refused`    = a phantom route turned into an honest refusal — the win;
///   * `route+INCOMPLETE` → `route+complete` = re-routed via a real entrance — also the win;
///   * `refused` → `route` = gained.
/// Every planned route is DRIVEN by the production controller (coarse-pursuit proxy, as in the
/// #685 harness), so "complete" is the controller's verdict, not the planner's.
///
/// ```text
/// ZONE_DIR=~/.local/share/eqoxide/assets/models \
///   cargo test --release --test walker_sim descent_guard_blast_radius -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires baked zone glbs at $ZONE_DIR; the #693 descent-guard blast radius"]
fn descent_guard_blast_radius() {
    const LOOK_AHEAD: f32 = 5.0;
    const STOP_DIST: f32 = 2.0;
    const Z_TOL: f32 = 8.0;
    const DT: f32 = 1.0 / 100.0;
    const FRAMES_PER_TICK: u32 = 15;
    const MAX_TICKS: u32 = 300;
    let pairs_per_zone: usize = std::env::var("PAIRS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);

    // The #685 driver, verbatim minus the smoothness accounting: LOS-clamped pure pursuit of the
    // coarse route with the real controller; arrival = XY within STOP_DIST and z within Z_TOL of
    // the route's final waypoint.
    let run = |col: &Collision, route: &[[f32; 3]], goal: [f32; 3]| -> (bool, u32) {
        let r = PLAYER_RADIUS;
        let mut ctrl = CharacterController::new(route[0]);
        ctrl.on_ground = true;
        let mut path_i = 0usize;
        for tick in 0..MAX_TICKS {
            let (px, py, pz) = (ctrl.pos[0], ctrl.pos[1], ctrl.pos[2]);
            if (px - goal[0]).hypot(py - goal[1]) < STOP_DIST && (pz - goal[2]).abs() <= Z_TOL {
                return (true, tick);
            }
            while path_i + 2 < route.len() {
                let (a, b) = (route[path_i], route[path_i + 1]);
                let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
                let t = if l2 < 1e-6 { 1.0 } else { ((px - a[0]) * ab[0] + (py - a[1]) * ab[1] + (pz - a[2]) * ab[2]) / l2 };
                if t >= 1.0 { path_i += 1; } else { break; }
            }
            let aim = carrot_along_los(route, path_i, [px, py, pz], LOOK_AHEAD, |a, b| col.carrot_los_clear(a, b, r))
                .unwrap_or(goal);
            let (dx, dy) = (aim[0] - px, aim[1] - py);
            let d = (dx * dx + dy * dy).sqrt().max(1e-3);
            for _ in 0..FRAMES_PER_TICK {
                ctrl.step(MoveIntent { wish_dir: [dx / d, dy / d], wish_vspeed: 0.0, jump: false,
                    want_swim: false, speed: RUN_SPEED, climb: 0.0, hop: false }, DT, col);
            }
        }
        (false, MAX_TICKS)
    };

    let dir = std::env::var("ZONE_DIR")
        .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
    // The #685 corpus PLUS the stacked-tier zones themselves (qeynos: the live #693 wedge; qcat and
    // halas: the water-descent/entry families' home turf).
    let zones: Vec<String> = std::env::var("ZONES").ok()
        .map(|z| z.split(',').map(str::to_string).collect())
        .unwrap_or_else(|| ["qeynos", "qcat", "halas", "akanon", "blackburrow", "qeynos2", "gfaydark",
            "crushbone", "neriaka", "felwithea", "highpass", "everfrost", "butcher", "cazicthule", "oasis"]
            .into_iter().map(str::to_string).collect());

    let mut seed: u64 = 0x693A_11CE;
    let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };
    let unit = |r: u32| r as f32 / u32::MAX as f32;

    let (mut g_pairs, mut g_routed, mut g_complete, mut g_refused, mut g_descent_routes) = (0usize, 0usize, 0usize, 0usize, 0usize);
    println!("\n=== #693 descent-guard blast radius (one build; diff the PAIR lines across builds) ===");
    println!("{:<12} {:>6} {:>7} {:>9} {:>8} {:>13}", "zone", "pairs", "routed", "complete", "refused", "descent_routes");
    // #807: a COVERAGE LEDGER for this corpus, not just a list of `.wtr` failures. It folds one
    // real per-zone water number — the count of sampled start/goal pairs the water filter excluded
    // — so the line it prints is a measurement rather than a placeholder, and it refuses to call
    // itself complete while any zone was dropped for any of the three reasons.
    let mut cover = WaterRollup::new();
    for zone in &zones {
        // #807: `open_corpus_zone` owns ALL THREE of this loop's zone-abandoning drop paths — no
        // baked `.glb`, an empty collision grid, a `.wtr` that did not load — and accounts for each
        // of them on `cover` before it returns. `cover.add(...)` at the bottom of the body closes
        // the zone; anything that leaves the body without reaching it is recorded as `unaccounted`
        // by the rollup itself. Before this, only the third was refused: the first two were bare
        // `continue`s that touched no ledger, so a host missing one zone's glb printed a clean
        // TOTAL over a smaller corpus than the one this test names.
        //
        // Water matters here even though this is not a water corpus: it is this corpus' PAIR FILTER
        // (`in_water(s) || in_water(g) → skip`). With no region map the filter silently passes
        // everything and wet pairs get scored as dry land, so the table would describe a different
        // corpus than the one it names — the zone is UNMEASURED, never a zone with no water (#762).
        let (col, zw) = match open_corpus_zone(&mut cover, std::path::Path::new(&dir), zone, 32.0) {
            Ok(ready) => ready,
            Err(why) => { println!("{zone:<12} ({why})"); continue }
        };

        let (mut z_pairs, mut z_routed, mut z_complete, mut z_refused, mut z_desc) = (0usize, 0usize, 0usize, 0usize, 0usize);
        let mut z_wet = 0usize; // #807: what the water filter actually excluded — the rollup's number
        let mut tries = 0;
        while z_pairs < pairs_per_zone && tries < pairs_per_zone * 70 + 500 {
            tries += 1;
            let e = col.origin[0] + unit(rnd()) * (col.cols as f32 * col.cell_size);
            let n = col.origin[1] + unit(rnd()) * (col.rows as f32 * col.cell_size);
            let Some(z) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { continue };
            let ang = unit(rnd()) * std::f32::consts::TAU;
            let d = 80.0 + unit(rnd()) * 320.0;
            let (ge, gn) = (e + d * ang.cos(), n + d * ang.sin());
            // Descents are the point here: resolve the goal to the floor NEAREST the sampled ask,
            // wherever in the column it lands (deep tiers included) — but keep the pair dry so the
            // walk-completion verdict is the dry controller's.
            let Some(gz) = col.nearest_floor(ge, gn, z, 400.0, 400.0) else { continue };
            let (s, g) = ([e, n, z], [ge, gn, gz]);
            if col.in_water(s) || col.in_water(g) { z_wet += 1; continue; }
            z_pairs += 1;

            let outcome = col.find_path_ex(s, g, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker());
            match outcome {
                PlanOutcome::Route(route) => {
                    z_routed += 1;
                    let goal = *route.last().unwrap();
                    let max_drop = route.windows(2).map(|w| w[0][2] - w[1][2]).fold(0.0f32, f32::max);
                    if max_drop > PLAYER_BODY.step_up { z_desc += 1; }
                    let (arrived, ticks) = run(&col, &route, goal);
                    if arrived { z_complete += 1; }
                    println!("PAIR {zone} {i} s[{:.1},{:.1},{:.1}] g[{:.1},{:.1},{:.1}] route len={} maxdrop={:.1} complete={} ticks={}",
                        s[0], s[1], s[2], g[0], g[1], g[2], route.len(), max_drop, arrived as u8, ticks, i = z_pairs);
                }
                other => {
                    z_refused += 1;
                    println!("PAIR {zone} {i} s[{:.1},{:.1},{:.1}] g[{:.1},{:.1},{:.1}] {}",
                        s[0], s[1], s[2], g[0], g[1], g[2], other.reason(), i = z_pairs);
                }
            }
        }
        println!("{zone:<12} {z_pairs:>6} {z_routed:>7} {z_complete:>9} {z_refused:>8} {z_desc:>13}");
        g_pairs += z_pairs; g_routed += z_routed; g_complete += z_complete; g_refused += z_refused; g_descent_routes += z_desc;
        cover.add(zone, &zw.measure(|_| z_wet)); // #807: CLOSE the zone — forgetting makes it `unaccounted`
    }
    println!("\nTOTAL pairs {g_pairs}  routed {g_routed}  complete {g_complete}  refused {g_refused}  descent_routes {g_descent_routes}");
    // #839: the accounting assert must fire BEFORE `g_pairs > 0` — a run that skipped/lost every
    // zone is still RED either way, but ordered this way the failure names WHICH zones dropped and
    // why, instead of reporting the pre-#807 "set ZONE_DIR" message over an accounting hole it
    // could have diagnosed.
    println!("wet start/goal pairs excluded by the water filter: {cover}");
    assert!(cover.is_complete(),
        "#807: the TOTALs above are not this corpus — they cover {}/{} of the zones it was asked \
         for, and are not comparable to a run that had them all. unmeasured (the .wtr was read and \
         did not load): {:?}; skipped (dropped before the water check ran — no glb / no grid): \
         {:?}; unaccounted (left the loop body without reaching add or skip — a corpus WIRING bug, \
         not an asset problem): {:?}",
        cover.measured_zones(), cover.attempted_zones(),
        cover.unmeasured_zones(), cover.skipped_zones(), cover.unaccounted_zones());
    assert!(g_pairs > 0, "no zones loaded — set ZONE_DIR to the baked glbs");
}

/// **#381 parallel-wall clearance blast radius over baked zones — REAL `CharacterController`, cross-
/// build A/B.** The #381 fix adds a footprint-ring sample ALONG each swept segment inside
/// `Collision::path_clear`, closing the parallel-wall hole (a wall the segment runs alongside within
/// the body radius). Its dominant RISK is OVER-FIRING: rejecting a legitimate tight passage (a
/// body-width corridor, a doorway) the character can actually walk.
///
/// This routes at the FINE cell (`LOCAL_CELL` = 2 u), where `edge_clear` IS the swept `path_clear`
/// this fix changes — so every edge in every route is validated by the modified test. Each route is
/// then DRIVEN by the production `CharacterController` (the #693/#685 coarse-pursuit proxy), so
/// "complete" is the controller's verdict, not the planner's. Pairs are kept SHORT (fine-tier scale)
/// so a 2 u whole-zone search stays bounded.
///
/// One build measures one column. Run on unmodified `main` and on the fix branch with identical
/// seeds (the floor model / sampling is untouched by the fix ⇒ identical pairs) and diff the PAIR
/// lines. The regression that must be ZERO is `route+complete → refused` (a legitimate walkable route
/// the fix turned into a false no-path). `route+INCOMPLETE → refused` (a route the controller could
/// not walk anyway, now honestly refused) is a WIN, not a loss.
///
/// ```text
/// ZONE_DIR=~/.local/share/eqoxide/assets/models \
///   cargo test --release --test walker_sim parallel_wall_clearance_blast_radius -- --ignored --nocapture
/// ```
#[test]
#[ignore = "requires baked zone glbs at $ZONE_DIR; the #381 parallel-wall blast radius"]
fn parallel_wall_clearance_blast_radius() {
    const LOOK_AHEAD: f32 = 5.0;
    const STOP_DIST: f32 = 2.0;
    const Z_TOL: f32 = 8.0;
    const DT: f32 = 1.0 / 100.0;
    const FRAMES_PER_TICK: u32 = 15;
    const MAX_TICKS: u32 = 300;
    // Aliased, not restated: 2.0 here and 2.0 in `steering` would agree by coincidence.
    const FINE_CELL: f32 = LOCAL_CELL; // the tier where path_clear is the edge test
    let pairs_per_zone: usize = std::env::var("PAIRS").ok().and_then(|s| s.parse().ok()).unwrap_or(80);

    let run = |col: &Collision, route: &[[f32; 3]], goal: [f32; 3]| -> (bool, u32) {
        let r = PLAYER_RADIUS;
        let mut ctrl = CharacterController::new(route[0]);
        ctrl.on_ground = true;
        let mut path_i = 0usize;
        for tick in 0..MAX_TICKS {
            let (px, py, pz) = (ctrl.pos[0], ctrl.pos[1], ctrl.pos[2]);
            if (px - goal[0]).hypot(py - goal[1]) < STOP_DIST && (pz - goal[2]).abs() <= Z_TOL {
                return (true, tick);
            }
            while path_i + 2 < route.len() {
                let (a, b) = (route[path_i], route[path_i + 1]);
                let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
                let t = if l2 < 1e-6 { 1.0 } else { ((px - a[0]) * ab[0] + (py - a[1]) * ab[1] + (pz - a[2]) * ab[2]) / l2 };
                if t >= 1.0 { path_i += 1; } else { break; }
            }
            let aim = carrot_along_los(route, path_i, [px, py, pz], LOOK_AHEAD, |a, b| col.carrot_los_clear(a, b, r))
                .unwrap_or(goal);
            let (dx, dy) = (aim[0] - px, aim[1] - py);
            let d = (dx * dx + dy * dy).sqrt().max(1e-3);
            for _ in 0..FRAMES_PER_TICK {
                ctrl.step(MoveIntent { wish_dir: [dx / d, dy / d], wish_vspeed: 0.0, jump: false,
                    want_swim: false, speed: RUN_SPEED, climb: 0.0, hop: false }, DT, col);
            }
        }
        (false, MAX_TICKS)
    };

    let dir = std::env::var("ZONE_DIR")
        .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
    let zones: Vec<String> = std::env::var("ZONES").ok()
        .map(|z| z.split(',').map(str::to_string).collect())
        .unwrap_or_else(|| ["akanon", "blackburrow", "qeynos", "gfaydark", "crushbone", "neriaka",
            "felwithea", "highpass", "qcat", "oasis"]
            .into_iter().map(str::to_string).collect());

    let mut seed: u64 = 0x381A_11CE;
    let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };
    let unit = |r: u32| r as f32 / u32::MAX as f32;

    let (mut g_pairs, mut g_routed, mut g_complete, mut g_refused) = (0usize, 0usize, 0usize, 0usize);
    println!("\n=== #381 parallel-wall blast radius (one build; diff PAIR lines across builds; route+complete->refused MUST be 0) ===");
    println!("{:<12} {:>6} {:>7} {:>9} {:>8}", "zone", "pairs", "routed", "complete", "refused");
    // #807: a COVERAGE LEDGER for this corpus, not just a list of `.wtr` failures. It folds one
    // real per-zone water number — the count of sampled start/goal pairs the water filter excluded
    // — so the line it prints is a measurement rather than a placeholder, and it refuses to call
    // itself complete while any zone was dropped for any of the three reasons.
    let mut cover = WaterRollup::new();
    for zone in &zones {
        // #807: `open_corpus_zone` owns ALL THREE of this loop's zone-abandoning drop paths — no
        // baked `.glb`, an empty collision grid, a `.wtr` that did not load — and accounts for each
        // of them on `cover` before it returns. `cover.add(...)` at the bottom of the body closes
        // the zone; anything that leaves the body without reaching it is recorded as `unaccounted`
        // by the rollup itself. Before this, only the third was refused: the first two were bare
        // `continue`s that touched no ledger, so a host missing one zone's glb printed a clean
        // TOTAL over a smaller corpus than the one this test names.
        //
        // Water matters here even though this is not a water corpus: it is this corpus' PAIR FILTER
        // (`in_water(s) || in_water(g) → skip`). With no region map the filter silently passes
        // everything and wet pairs get scored as dry land, so the table would describe a different
        // corpus than the one it names — the zone is UNMEASURED, never a zone with no water (#762).
        let (col, zw) = match open_corpus_zone(&mut cover, std::path::Path::new(&dir), zone, 32.0) {
            Ok(ready) => ready,
            Err(why) => { println!("{zone:<12} ({why})"); continue }
        };

        let (mut z_pairs, mut z_routed, mut z_complete, mut z_refused) = (0usize, 0usize, 0usize, 0usize);
        let mut z_wet = 0usize; // #807: what the water filter actually excluded — the rollup's number
        let mut tries = 0;
        while z_pairs < pairs_per_zone && tries < pairs_per_zone * 70 + 500 {
            tries += 1;
            let e = col.origin[0] + unit(rnd()) * (col.cols as f32 * col.cell_size);
            let n = col.origin[1] + unit(rnd()) * (col.rows as f32 * col.cell_size);
            let Some(z) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { continue };
            let ang = unit(rnd()) * std::f32::consts::TAU;
            let d = 20.0 + unit(rnd()) * 70.0; // SHORT, fine-tier scale, so a 2u search stays bounded
            let (ge, gn) = (e + d * ang.cos(), n + d * ang.sin());
            let Some(gz) = col.nearest_floor(ge, gn, z, 60.0, 60.0) else { continue };
            let (s, g) = ([e, n, z], [ge, gn, gz]);
            if col.in_water(s) || col.in_water(g) { z_wet += 1; continue; }
            z_pairs += 1;

            // Bound the frontier to a LOCAL window (like the fine-tier LocalPlanner's ~40-60u reach,
            // padded) so a 2u search stays fast and representative of where path_clear actually runs.
            let outcome = col.find_path_ex(s, g, PLAYER_RADIUS, &[], FINE_CELL, Some(200.0), 0.0, PlanCtx::worker());
            match outcome {
                PlanOutcome::Route(route) => {
                    z_routed += 1;
                    let goal = *route.last().unwrap();
                    let (arrived, ticks) = run(&col, &route, goal);
                    if arrived { z_complete += 1; }
                    println!("PAIR {zone} {i} s[{:.1},{:.1},{:.1}] g[{:.1},{:.1},{:.1}] route len={} complete={} ticks={}",
                        s[0], s[1], s[2], g[0], g[1], g[2], route.len(), arrived as u8, ticks, i = z_pairs);
                }
                other => {
                    z_refused += 1;
                    println!("PAIR {zone} {i} s[{:.1},{:.1},{:.1}] g[{:.1},{:.1},{:.1}] REFUSED {}",
                        s[0], s[1], s[2], g[0], g[1], g[2], other.reason(), i = z_pairs);
                }
            }
        }
        println!("{zone:<12} {z_pairs:>6} {z_routed:>7} {z_complete:>9} {z_refused:>8}");
        g_pairs += z_pairs; g_routed += z_routed; g_complete += z_complete; g_refused += z_refused;
        cover.add(zone, &zw.measure(|_| z_wet)); // #807: CLOSE the zone — forgetting makes it `unaccounted`
    }
    println!("\nTOTAL pairs {g_pairs}  routed {g_routed}  complete {g_complete}  refused {g_refused}");
    // #839: the accounting assert must fire BEFORE `g_pairs > 0` — a run that skipped/lost every
    // zone is still RED either way, but ordered this way the failure names WHICH zones dropped and
    // why, instead of reporting the pre-#807 "set ZONE_DIR" message over an accounting hole it
    // could have diagnosed.
    println!("wet start/goal pairs excluded by the water filter: {cover}");
    assert!(cover.is_complete(),
        "#807: the TOTALs above are not this corpus — they cover {}/{} of the zones it was asked \
         for, and are not comparable to a run that had them all. unmeasured (the .wtr was read and \
         did not load): {:?}; skipped (dropped before the water check ran — no glb / no grid): \
         {:?}; unaccounted (left the loop body without reaching add or skip — a corpus WIRING bug, \
         not an asset problem): {:?}",
        cover.measured_zones(), cover.attempted_zones(),
        cover.unmeasured_zones(), cover.skipped_zones(), cover.unaccounted_zones());
    assert!(g_pairs > 0, "no zones loaded — set ZONE_DIR to the baked glbs");
}
