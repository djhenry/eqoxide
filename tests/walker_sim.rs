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
use eqoxide::traversability::{Point, Traversability, PLAYER_BODY};
use eqoxide::assets::{MeshData, RenderMode, ZoneAssets};
use eqoxide::region_map::RegionMap;
use eqoxide_core::physics::PLAYER_RADIUS;
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
                speed: 44.0, climb: 0.0, hop: false }, DT, &col);

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
    ///   * a 150 ms NAV TICK that advances `path_i`, RE-POSTS a fresh `find_path_local` from the
    ///     walker's CURRENT position (1-tick lag, as #399's worker introduces), and runs stall
    ///     detection → downhill backoff → coarse re-plan (capped at 8 attempts), plus the #246/#379
    ///     proactive coarse re-plan when the fine tier reports `NoWayThrough`.
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

        // Production constants, verbatim from navigation.rs (kept in sync — if these drift, the scanner
        // stops modelling the real walker).
        const RUN_SPEED: f32 = 44.0;
        const LOOK_AHEAD: f32 = 5.0;
        const LOCAL_REACH: f32 = 24.0;
        const LOCAL_BOUND: f32 = 40.0;
        const LOCAL_CELL:  f32 = 2.0;
        const NAV_STUCK_TICKS: u32 = 20;
        const NAV_HOP_TICKS: u32 = 6;
        const NAV_BACKOFF_TICKS: u32 = 3;
        const NAV_LOCAL_STUCK_TICKS: u32 = 3;
        const REPLAN_COOLDOWN_TICKS: u32 = 6;
        const MAX_REPATHS: u32 = 8;
        const DT: f32 = 1.0 / 100.0;          // ~100 Hz controller, per navigation.rs's fast-steer note
        const FRAMES_PER_TICK: u32 = 15;      // 150 ms / 10 ms
        // The swim-up vertical wish is `drift_swim_up_wish` (module fn below), mirroring the walker
        // (walker.rs:819-825) and pinned by `drift_sim_swim_drive_mirrors_walker` so the instrument
        // can't silently diverge from production.

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
                if replan_cd > 0 { replan_cd -= 1; }

                // downhill backoff in progress → drive reverse aim, then re-plan when it ends
                if backoff_ticks > 0 {
                    backoff_ticks -= 1;
                    for _ in 0..FRAMES_PER_TICK {
                        // The real walker's downhill-backoff branch drives want_swim: false
                        // UNCONDITIONALLY (walker.rs:731-742), even while submerged — the backoff is a
                        // deliberate non-swim recovery. The sim MUST match, or it recovers (swim-mode
                        // step) where the client sinks (non-swim step): a false pass.
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
        let (mut tot_wr, mut tot_423) = (0usize, 0usize);
        println!("\n=== faithful walker drift: {} mode ===", if include_water { "WATER-INCLUSIVE" } else { "DRY" });
        println!("{:<12} {:>6} {:>7} {:>8} {:>8} {:>6} {:>9} {:>6}",
            "zone", "walked", "wedged", "height", "overlap", "other", "wat-route", "#423");
        for zone in &zones {
            let p = std::path::Path::new(&dir).join(format!("{zone}.glb"));
            let Ok(za) = ZoneAssets::from_glb(&p) else { println!("{zone:<12}  (no glb — skipped)"); continue };
            let mut col = Collision::build(&za, 32.0);
            if col.cols == 0 { println!("{zone:<12}  (no grid — skipped)"); continue; }
            col.set_water(RegionMap::load(&std::path::Path::new(&dir).join("maps/water"), zone).map(std::sync::Arc::new));

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
            if pairs.is_empty() { println!("{zone:<12}  (no routable pairs — skipped)"); continue; }

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
            tot_height += n_h; tot_overlap += n_o; tot_other += n_x; tot_wr += n_wr; tot_423 += n_423;
        }
        let rate = if tot_walked > 0 { 100.0 * tot_wedged as f32 / tot_walked as f32 } else { 0.0 };
        println!("\n=== FAITHFUL WALKER DRIFT [{}]: {tot_walked} full journeys walked, {tot_wedged} terminal wedges \
            ({rate:.2}%) — height #386: {tot_height}, overlap #381: {tot_overlap}, other: {tot_other}, \
            wat-route: {tot_wr}, #423: {tot_423} ===",
            if include_water { "WATER-INCLUSIVE" } else { "DRY" });
        if include_water {
            println!("(wat-route = wedge whose COMMITTED coarse route carried water waypoints near the wedge = a \
                      planner/ROUTING failure, OURS to fix in later water-nav increments. #423 = route dry near the \
                      wedge but the character ended up wet = the SEPARATE pre-existing walk-through-wall-into-water \
                      collision bug, not a routing failure. The two are never lumped.)");
        }
        let _ = tot_pairs;
        assert!(tot_walked > 0, "no journeys walked — check $ZONE_DIR");
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
            ctrl.step(MoveIntent { wish_dir: [1.0, 0.0], speed: 44.0, ..Default::default() },
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
            ctrl.step(MoveIntent { wish_dir: [1.0, 0.0], speed: 44.0, ..Default::default() },
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
            ctrl.step(MoveIntent { wish_dir: [0.7071, 0.7071], speed: 44.0, ..Default::default() },
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
            ctrl.step(MoveIntent { wish_dir: [0.7071, 0.7071], speed: 44.0, ..Default::default() },
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
    const RUN_SPEED: f32 = 44.0;
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
    for zone in &zones {
        let p = std::path::Path::new(&dir).join(format!("{zone}.glb"));
        let Ok(za) = ZoneAssets::from_glb(&p) else { println!("{zone:<12} (no glb — skipped)"); continue };
        let mut col = Collision::build(&za, 32.0);
        if col.cols == 0 { println!("{zone:<12} (no grid — skipped)"); continue; }
        col.set_water(RegionMap::load(&std::path::Path::new(&dir).join("maps/water"), zone).map(std::sync::Arc::new));

        let (mut z_pairs, mut z_routed, mut z_lost, mut z_drove, mut z_over) = (0usize, 0usize, 0usize, 0usize, 0usize);
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
            if col.in_water(s) || col.in_water(g) { continue; }
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
    }
    println!("\nTOTAL pairs {g_pairs}  routed {g_routed}  LOST(newly-refused) {g_lost}  drove {g_drove}  OVER-TIGHTENED {g_over}");
    println!("(gained = 0 by construction: #639 only ADDS refusals. OVER-TIGHTENED must be 0 — any LOST \
             pair where the REAL controller reached the goal's own tier is a regression.)");
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
    const RUN_SPEED: f32 = 44.0;
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
    for zone in &zones {
        let p = std::path::Path::new(&dir).join(format!("{zone}.glb"));
        let Ok(za) = ZoneAssets::from_glb(&p) else { println!("{zone:<12} (no glb — skipped)"); continue };
        let mut col = Collision::build(&za, 32.0);
        if col.cols == 0 { println!("{zone:<12} (no grid — skipped)"); continue; }
        col.set_water(RegionMap::load(&std::path::Path::new(&dir).join("maps/water"), zone).map(std::sync::Arc::new));

        let (mut z_pairs, mut z_both, mut z_broken, mut z_gained, mut z_smoothed) = (0usize, 0usize, 0usize, 0usize, 0usize);
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
            if col.in_water(s) || col.in_water(g) { continue; } // dry-land corners only
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
        g_ticks_inf += z_ti; g_ticks_plain += z_tp;
    }
    let slowdown = if g_ticks_plain > 0 { g_ticks_inf as f64 / g_ticks_plain as f64 } else { 1.0 };
    let turn_ratio = if g_turn_plain > 0.0 { g_turn_inf / g_turn_plain } else { 1.0 };
    println!("\nTOTAL pairs {g_pairs}  both-complete {g_both}  BROKEN {g_broken}  GAINED {g_gained}  SMOOTHED {g_smoothed}  \
             SLOWDOWN {slowdown:.4}  turning(inflated/plain) {turn_ratio:.3}");
    println!("INFLATION FIRED: {g_routes_touched} of the sampled routes had >=1 waypoint moved; {g_moved_wp}/{g_total_wp} waypoints offset off a wall.");
    println!("(BROKEN must be 0 — a route the plain coarse route completed that inflation broke is a narrow-corridor \
             over-tightening. SLOWDOWN must be ~1.0. turning<1.0 and SMOOTHED>0 is the anti-wiggle win.)");
    assert!(g_pairs > 0, "no zones loaded — set ZONE_DIR to the baked glbs");
    assert_eq!(g_broken, 0,
        "#685 over-tightening: {g_broken} route(s) the plain coarse route completed FAILED after inflation — \
         the corner-buffer offset broke a passable route (likely a narrow corridor). Investigate the printed pairs.");
    assert!(slowdown < 1.10,
        "#685: inflation slowed both-completing routes by {:.1}% (ticks_inf/ticks_plain={slowdown:.4}) — the \
         inflated route must not crawl.", (slowdown - 1.0) * 100.0);
}
