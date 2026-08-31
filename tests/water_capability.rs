//! **What a swimmer can actually DO** — a capability harness for the water half of navigation.
//!
//! It exists because a plausible-sounding premise about swimming cost a whole PR (#648, withdrawn):
//! *"a swimmer cannot rise more than `haul_out_up` above the surface of the water it is in."* That is
//! false, and nothing in the tree said so. This file says so, in executable form.
//!
//! The rise is **not performed in place**. `movement.rs` recomputes `col.water_surface(water_at)` at
//! the character's OWN position every frame, so a swimmer that moves LATERALLY into a column with a
//! higher surface is floated to *that* column's swim plane by ordinary buoyancy. The source column's
//! surface therefore bounds nothing, and a planner gate keyed on it rejects routes the controller can
//! swim (#648 cost two real `freportw` routes that way before it was withdrawn).
//!
//! These tests drive the REAL [`CharacterController`] at REAL baked geometry with the intent the
//! walker actually sends at a water waypoint (`want_swim: true`, horizontal wish only, no vertical
//! wish) and assert where it ends up. They are asset-gated and `#[ignore]`d like every other
//! baked-asset test (#357):
//!
//! ```text
//! EQZONES=~/eqzones cargo test --release --test water_capability -- --ignored --nocapture
//! ```
//!
//! Two of them pin capability (what a swimmer CAN do, so a future gate cannot quietly forbid it);
//! the strand test's lineage pins the #329/#661 wedge coordinate: it asserted the strand, then
//! (round 2) the escape under the same horizontal drive, and now (round 3) the escape under a
//! DRIVEN dive plus a wet-stall disclosure for the horizontal drive — each flip's history is in
//! the test docs, because the drive that can honestly escape changed as the duck's re-divability
//! bounds landed.

use eqoxide::assets::ZoneAssets;
use eqoxide::movement::CharacterController;
use eqoxide::nav::collision::Collision;
use eqoxide::region_map::RegionMap;
use eqoxide::traversability::PLAYER_BODY;
use eqoxide_ipc::MoveIntent;

fn zones_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("EQZONES").unwrap_or_else(|_| {
        format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap())
    }))
}

fn zone(name: &str) -> Collision {
    let dir = zones_dir();
    let za = ZoneAssets::from_glb(&dir.join(format!("{name}.glb")))
        .unwrap_or_else(|e| panic!("baked {name}.glb required at $EQZONES: {e:?}"));
    let mut c = Collision::build(&za, 32.0);
    c.set_region_data(Ok(std::sync::Arc::new(
        RegionMap::try_load(&dir.join("maps/water"), name).expect("baked .wtr required"))));
    c
}

/// Drive the controller from `from` toward the XY of `to` for `secs`, with exactly the intent the
/// walker sends at a water waypoint: `want_swim`, a horizontal wish, and **no vertical wish** — so
/// every unit of rise observed here is buoyancy's, not a swim-up drive's.
fn swim_toward(col: &Collision, from: [f32; 3], to: [f32; 3], secs: f32) -> [f32; 3] {
    let mut c = CharacterController::new(from);
    let dt = 1.0 / 60.0;
    for _ in 0..((secs / dt) as usize) {
        let d = [to[0] - c.pos[0], to[1] - c.pos[1]];
        let l = (d[0] * d[0] + d[1] * d[1]).sqrt();
        let dir = if l > 0.2 { [d[0] / l, d[1] / l] } else { [0.0, 0.0] };
        c.step(MoveIntent { wish_dir: dir, wish_vspeed: 0.0, jump: false, want_swim: true, want_climb: false,
                            speed: 44.0, hop: false }, dt, col);
    }
    c.pos
}

/// The swim plane of the water column at `p`: `surface − float_depth`, the height buoyancy holds a
/// swimmer at. Derived from the same [`PLAYER_BODY`] field the controller uses, not a literal.
fn swim_plane(col: &Collision, p: [f32; 3]) -> f32 {
    col.water_surface(p).expect("expected a bounded water column here") - PLAYER_BODY.float_depth
}

/// **A SWIMMER RISES TO THE DESTINATION COLUMN'S SURFACE, NOT ITS OWN (#329 / #648).**
///
/// The qcat spawn pocket tops out at −55.978 (a ceiling slab at −55.969 caps it); the shaft one cell
/// away tops out at −42.982 — a 12.996 u difference, and the edge #329's triage flagged as
/// impossible. From anywhere in the pocket's water below the swim plane the controller swims across
/// and buoyancy lifts it onto the SHAFT's plane, performing rises of **+17 u to +23 u** with no
/// vertical input at all.
///
/// Any future cap on water-edge rise must keep this green. A gate keyed on the source column's
/// surface cannot: `haul_out_up` is 2.0.
///
/// # Start depths re-derived when #649 landed (the entanglement is resolved)
///
/// This test used to run from −60, −62, −65 and −68, and its green at −59/−60 DEPENDED on the very
/// defect #649 tracked: the water-blind depenetration push-out DROPPED the swimmer ~10 u to the
/// pool floor on frame 0, and it crossed into the shaft from down there. With the push-out fixed a
/// swimmer held its own depth, so from −59/−60 it ended dry on the −55.969 tile floor beside the
/// pocket — the residual #661 wedge (whose measured writer was the net's dry-candidate beach, not
/// the step-up this note used to blame). The start depths were re-derived against the #649-fixed
/// controller then; since the #661 fix the shallow band escapes too, and the escape test below
/// sweeps it directly.
///
/// The CAPABILITY claim is unchanged and is what must survive: buoyancy lifts a swimmer to the
/// DESTINATION column's swim plane, unbounded by the source column's surface (+17 to +23 u here
/// against a `haul_out_up` of 2.0). The particular z values are not sacred.
#[test]
#[ignore = "asset-gated: needs baked qcat.glb + qcat.wtr at $EQZONES (#357)"]
fn a_swimmer_rises_to_the_destination_columns_surface_not_its_own() {
    let col = zone("qcat");
    let pocket_xy = [-42.3f32, 1036.8];
    let shaft = [-45.75f32, 1030.0625, -42.98];
    let pocket_surface = col.water_surface([pocket_xy[0], pocket_xy[1], -60.0]).unwrap();
    let shaft_plane = swim_plane(&col, [shaft[0], shaft[1], -50.0]);
    assert!((pocket_surface - (-55.978)).abs() < 0.01, "fixture: pocket surface {pocket_surface}");
    assert!((shaft_plane - (-44.982)).abs() < 0.01, "fixture: shaft swim plane {shaft_plane}");

    for z in [-62.0f32, -65.0, -68.0] {
        let from = [pocket_xy[0], pocket_xy[1], z];
        let end = swim_toward(&col, from, shaft, 12.0);
        assert!((end[2] - shaft_plane).abs() < 0.05,
            "from z={z} the swimmer must settle on the SHAFT's swim plane {shaft_plane:.4}, got \
             {end:?}. It rose {:+.2}u — far past the {}u `haul_out_up` measured from its OWN \
             surface ({pocket_surface:.4}). That is the premise #648 got wrong: buoyancy re-reads \
             the surface at the character's position every frame, so the rise happens at the \
             DESTINATION and the source column's surface bounds nothing.",
            end[2] - z, PLAYER_BODY.haul_out_up);
        assert!((end[0] - shaft[0]).hypot(end[1] - shaft[1]) < 1.5,
            "and it must actually arrive at the shaft XY, got {end:?}");
        assert!(end[2] - z > 2.0 * PLAYER_BODY.haul_out_up,
            "sanity: this case is only interesting because the rise exceeds the haul-out reach");
    }
}

/// **THE SAME FACT ON A SECOND ZONE, at the edges a withdrawn gate wrongly refused (#648).**
///
/// Three `freportw` canal steps. Each destination column's surface is ~3 u above the source
/// column's, and in every case the controller arrives and settles on the destination's swim plane —
/// rises of +3.0 to +4.9 u, all above both `haul_out_up` (2.0) and the swimming step-up
/// (`STEP_UP + GROUND_SNAP_TOL` = 2.5). These are real routes; #648 cost two of them.
#[test]
#[ignore = "asset-gated: needs baked freportw.glb + freportw.wtr at $EQZONES (#357)"]
fn stepped_canal_surfaces_are_swimmable_between() {
    let col = zone("freportw");
    for (from, to) in [
        ([-889.3125f32, -403.6875, -66.46878], [-889.3125f32, -395.6875, -60.728962]),
        ([-889.3125, -379.6875, -56.529953], [-889.3125, -371.6875, -51.541473]),
        ([-809.3125, -323.6875, -40.800888], [-801.3125, -323.6875, -33.979805]),
    ] {
        let plane = swim_plane(&col, [to[0], to[1], to[2] - 1.0]);
        let end = swim_toward(&col, from, to, 6.0);
        assert!((end[2] - plane).abs() < 0.25,
            "the swimmer must settle on the DESTINATION column's swim plane {plane:.3}; got {end:?} \
             (from {from:?}, a rise of {:+.2}u)", end[2] - from[2]);
        assert!((end[0] - to[0]).hypot(end[1] - to[1]) < 1.0,
            "and must arrive at the destination XY; got {end:?}");
        assert!(end[2] - from[2] > PLAYER_BODY.haul_out_up,
            "sanity: each of these rises must exceed the haul-out reach, else the case is not the \
             one #648 refused ({:+.2}u)", end[2] - from[2]);
    }
}

/// **THE #329/#661 WEDGE COORDINATE, FIXED: the pocket swimmer now ESCAPES to the shaft.**
///
/// # ⚠️ This test's expectation was FLIPPED by the #661 fix — the history matters, read it
///
/// Under the name `qcat_pocket_swim_plane_strands_the_swimmer_on_the_tile_floor` this test PINNED
/// the strand: from the pocket's own swim plane (−57.978) the character ended on the tile floor at
/// **−55.9687** — the live #329 wedge coordinate, 0.009 u ABOVE the waterline, dry, `on_ground`,
/// `want_swim` inert, unable to sink back through the slab: a one-way soft-lock. Its assertions
/// were `end z ≈ −55.9687`, `!in_water(end)`, and `> 5 u short of the shaft`. They now assert the
/// ESCAPE instead, exactly the flip its own doc said this fix should make.
///
/// # The mechanism, as MEASURED on `main` @ 269dbbf (the issue text misattributed it)
///
/// #661's issue body blames the **swimming step-up** (#191's haul-out branch). Instrumenting both
/// branches showed the step-up NEVER fires on this run — no `low_hit` even occurs before the
/// mount. The measured chain was:
///
/// 1. the depenetration net's footprint ring probes at `feet + 3` — **1 u above the waterline**
///    for a swimmer at the float plane — so pocket-rim geometry in the AIR band read the swimmer
///    as "embedded" on alternate frames while its route was physically open (its slide made full
///    0.733 u/frame progress on every frame it was allowed to run);
/// 2. the ring push-out ate the input on those frames and ping-ponged the body in place (the live
///    `walker_stalled`), until frame 33, when the first footprint-clear candidate fell a fraction
///    of a unit outside the `.wtr` region's XY extent — whereupon `Recovery::at_column`'s
///    dry-candidate fall-through ran the water-blind `nearest_floor` hunt and beached the
///    still-swimming body onto the tile (dxy exactly 1.000 = `PUSHOUT_RADII[0]`, dz +2.0092,
///    `Grounded`, dry);
/// 3. and with both of those removed the swimmer STILL could not finish: the passage to the shaft
///    is only open BELOW the swim plane, and the controller had no downward mirror of its 2.5 u
///    step-up — it pressed into the pocket's south face for ever. That asymmetry is the "one-way
///    transition" of the issue title, measured in its true location.
///
/// The fix is three-sided (`movement.rs`): a floating body never enters the depenetration net;
/// the net's only constructible recovery is `Grounded` and only dry bodies can reach it; and a
/// blocked swimmer tries `try_duck_under` — the step-up's downward mirror — before the haul-out,
/// taking it only when diving measurably gains lateral progress. In round 2 the duck itself
/// carried the horizontal-only swimmer under the lip to the shaft plane; round 3 changed which
/// drive performs the escape — read on.
///
/// # ⚠️ Round 3 changed WHICH drive escapes — the history matters, read it
///
/// Round 2's version of this test escaped under a horizontal-only wish, via the autonomous
/// duck-under crossing from the pocket's swim plane into the shaft's water column. The round-2
/// review then measured that an unconstrained duck is itself a one-way transition over any
/// surface mismatch beyond the duck envelope (R2-B1) — and the pocket→shaft crossing is a ~13 u
/// mismatch, i.e. genuinely one-way for the autonomous driver. `try_duck_under`'s surface bound
/// now refuses it, so the horizontal-only drive stalls wet at the mouth (the test below this one)
/// and the ESCAPE is executed the way the system actually intends: a DRIVEN dive — the explicit
/// down-wish a dive-first route (or a manual down+lateral drive) sends — which passes under the
/// lip with no duck involved and can always be driven back the same way.
///
/// (Also measured this round, filed separately: driving the REAL planner route with the REAL
/// steering still wedges at this pocket — the planner emits a rise-at-destination `swim_surface`
/// hop, `swim_vspeed` turns the high carrot into an up-wish, and the up-wish pins the body under
/// the lid. That walker/steering residue is outside this PR's file set; the offline repro is in
/// the follow-up issue. The controller-level capabilities this test and its sibling pin are the
/// substrate that fix needs either way.)
///
/// The start-depth sweep covers the band #661 measured as STRANDED on the #658 controller
/// (−58.00 … −60.75); every depth in it must reach the shaft under the driven dive.
#[test]
#[ignore = "asset-gated: needs baked qcat.glb + qcat.wtr at $EQZONES (#357)"]
fn qcat_pocket_swimmer_escapes_to_the_shaft_under_a_driven_dive() {
    let col = zone("qcat");
    let pocket_xy = [-42.3f32, 1036.8];
    let shaft = [-45.75f32, 1030.0625, -42.98];
    let surface = col.water_surface([pocket_xy[0], pocket_xy[1], -60.0]).unwrap();
    let plane = surface - PLAYER_BODY.float_depth;
    let shaft_plane = swim_plane(&col, [shaft[0], shaft[1], -50.0]);
    assert!((plane - (-57.978)).abs() < 0.01, "fixture: pocket swim plane {plane}");

    for z in [plane, -59.0, -60.5] {
        // The dive-first driver: an explicit down-wish until below the corridor lid, horizontal
        // toward the shaft throughout, neutral once the dive has cleared the passage — the intent
        // shape a dive-first route (or `/move/manual` down+lateral) sends.
        let mut c = CharacterController::new([pocket_xy[0], pocket_xy[1], z]);
        let dt = 1.0 / 60.0;
        for _ in 0..(12 * 60) {
            let d = [shaft[0] - c.pos[0], shaft[1] - c.pos[1]];
            let l = (d[0] * d[0] + d[1] * d[1]).sqrt();
            let dir = if l > 0.2 { [d[0] / l, d[1] / l] } else { [0.0, 0.0] };
            let vs = if c.pos[2] > -60.0 && l > 4.0 { -20.0 } else { 0.0 };
            c.step(MoveIntent { wish_dir: dir, wish_vspeed: vs, jump: false, want_swim: true, want_climb: false,
                                speed: 44.0, hop: false }, dt, &col);
        }
        let end = c.pos;
        assert!((end[2] - shaft_plane).abs() < 0.05,
            "#661: from z={z:.3} a DRIVEN dive must carry the swimmer out of the pocket and \
             buoyancy must settle it on the SHAFT's swim plane {shaft_plane:.4} — the pre-fix \
             controller stranded these starts dry on the tile floor at −55.9687, the live #329 \
             wedge coordinate; got {end:?}");
        assert!((end[0] - shaft[0]).hypot(end[1] - shaft[1]) < 1.5,
            "#661: and actually arrive at the shaft XY; got {end:?}");
        assert!(col.in_water(end),
            "#661: still swimming — the escape must never pass through a dry mount; got {end:?}");
    }
}

/// **The horizontal-only driver's honest outcome at the pocket: a WET STALL, never a strand.**
///
/// This is what round 2's escape drive now gets, by design: the pocket→shaft crossing is one-way
/// for the autonomous duck (~13 u surface mismatch, beyond the return envelope), so
/// `try_duck_under`'s surface bound refuses it and the body presses at the mouth — wet,
/// unsupported, at its own swim plane, input still honoured. Every part of that end state is the
/// #661 fix: the pre-fix controller turned this exact drive into the dry `on_ground` strand at
/// −55.9687 (via the net's beach) where `want_swim` was inert for ever. A stall is visible to nav
/// (`walker_stalled` → replan); the strand was terminal.
#[test]
#[ignore = "asset-gated: needs baked qcat.glb + qcat.wtr at $EQZONES (#357)"]
fn qcat_pocket_horizontal_wish_alone_stalls_wet_at_the_mouth() {
    let col = zone("qcat");
    let pocket_xy = [-42.3f32, 1036.8];
    let shaft = [-45.75f32, 1030.0625, -42.98];
    let surface = col.water_surface([pocket_xy[0], pocket_xy[1], -60.0]).unwrap();
    let plane = surface - PLAYER_BODY.float_depth;

    let end = swim_toward(&col, [pocket_xy[0], pocket_xy[1], plane], shaft, 12.0);
    assert!(col.in_water(end) && (end[2] - plane).abs() < 0.05,
        "#661: the horizontal-only driver must stall WET at its own swim plane ({plane:.4}) — \
         never the dry −55.9687 strand, never a one-way crossing; got {end:?}");
    assert!((end[2] - (-55.9687)).abs() > 1.0,
        "#661: specifically NOT the old strand coordinate; got {end:?}");
}

/// **#649/#661 AT THE REAL COORDINATE: the first frame never lifts a swimmer dry.**
///
/// One frame, from the exact position the #649 comment thread measured — the qcat pocket swim plane,
/// where the dry embedded predicate is TRUE (`footprint_clear` false) and `ground_below` is
/// `Some(−69.969)`. On the pre-#649 controller the net recovered with the NEARER floor: the tile
/// floor 2.009 u ABOVE, producing `(−42.252, 1037.071, −55.96875)`, `on_ground = true`, DRY.
///
/// # ⚠️ The #661 fix relaxed this test's frame-1 assertion — deliberately
///
/// Under #658 this asserted `dz ≈ 0`: the net "held the swimmer at its own depth", which was true
/// but conflated two claims — *not vertically teleported* (the #649 pin) and *frame frozen by the
/// net* (an artefact of the net still handling the frame and eating the wish). #661 removed the
/// net from floating bodies AND gave a blocked swimmer the duck-under, so frame 1 from this
/// wedged-against-the-mouth start now legitimately DIVES: a collided, bounded descent (≤ the
/// 2.5 u duck envelope) in the wish direction, still wet. What #649 forbids is unchanged and
/// still asserted: never lifted toward the dry tile, never dropped to the pool floor 12 u below,
/// never grounded, never dry.
///
/// The asset-free pins for this are `movement::tests::depenetration_never_{mounts,drops}_a_swimmer_*`
/// (they run in CI); this one proves the same thing on the real baked geometry that produced the bug.
#[test]
#[ignore = "asset-gated: needs baked qcat.glb + qcat.wtr at $EQZONES (#357)"]
fn the_first_frame_never_lifts_a_qcat_pocket_swimmer_toward_the_dry_tile() {
    let col = zone("qcat");
    let start = [-42.634235f32, 1036.1473, -57.977985];
    let mut c = CharacterController::new(start);
    // The walker's intent at a water waypoint: horizontal only, no vertical wish (so any vertical
    // motion observed is the controller's, not a drive's).
    c.step(MoveIntent { wish_dir: [0.55, -0.83], wish_vspeed: 0.0, jump: false, want_swim: true, want_climb: false,
                        speed: 44.0, hop: false }, 1.0 / 60.0, &col);
    let dz = c.pos[2] - start[2];
    assert!(dz < 1e-3,
        "#649: frame 1 must never LIFT the swimmer (the unfixed net mounts the tile floor at \
         −55.96875, dz +2.0092, the live #329 wedge coordinate); got {:?} (dz {dz:+.4})", c.pos);
    assert!(dz > -2.6,
        "#649: nor teleport it downward — any descent is the duck-under's collided ≤2.5 u dive, \
         not the old drop to the pool floor 12 u below; got {:?} (dz {dz:+.4})", c.pos);
    assert!(!c.on_ground, "#649: a floating body must not be marked grounded: {:?}", c.pos);
    assert!(col.in_water(c.pos), "#649: and it must still be in the water: {:?}", c.pos);
}
