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
//! the third pins the still-open #329 strand, offline and deterministically, at the coordinate the
//! live client wedges on (its mechanism changed when #649 landed — see its doc comment).

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
    c.set_water(Some(std::sync::Arc::new(
        RegionMap::load(&dir.join("maps/water"), name).expect("baked .wtr required"))));
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
        c.step(MoveIntent { wish_dir: dir, wish_vspeed: 0.0, jump: false, want_swim: true,
                            speed: 44.0, climb: 0.0, hop: false }, dt, col);
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
/// swimmer holds its own depth, so from −59/−60 ordinary buoyancy lifts it to the pocket's OWN swim
/// plane (−57.978) *before* it reaches the shaft, where the swimming step-up mounts it on the
/// −55.969 tile floor beside the pocket — the second, separate mechanism at that coordinate, still
/// open (see the strand test below). The start depths were therefore re-derived against the fixed
/// controller, exactly as this note used to instruct.
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

/// **THE #329 STRAND, REPRODUCED OFFLINE TO FOUR DECIMALS — STILL OPEN.**
///
/// Starting at the POCKET's own swim plane (−57.978) the character ends up on the tile floor at
/// **−55.9687**, the coordinate the live client wedges on in the original #329 report. That floor is
/// 0.009 u ABOVE the −55.978 waterline, so `in_water` is false, `swimming` is false, buoyancy never
/// fires again, and it cannot sink back through the slab it is standing on.
///
/// # The mechanism CHANGED when #649 was fixed — read this before touching it
///
/// #649 was the water-blind **depenetration push-out**: `footprint_clear` fails for any swimmer in a
/// ~12 u pocket, which the net read as "embedded in rock" and recovered with
/// `nearest_floor(up = STEP_UP + GROUND_ORIGIN, down = GROUND_DEPTH)` — the NEARER floor in either
/// direction. It mounted the character here in ONE frame (dz +2.0092, `on_ground = true`). That is
/// fixed: an afloat body now holds its own depth (`movement::Recovery`), and the frame-1 teleport is
/// gone — verified by driving this controller (frame 1 now holds −57.97798).
///
/// The end coordinate did **not** change, because a SECOND, independent mechanism reaches it: with
/// the swimmer left in the water, it swims laterally until (frame ~33) the **swimming step-up** —
/// the #191 haul-out branch, `(self.on_ground || swimming) && low_hit → try_step_up` — climbs the
/// 2.009 u onto that tile floor, whose footprint IS clear and which has open headroom. It looks
/// locally identical to hauling out onto a bank, and A* routes out of it to 11 of 12 sampled far
/// goals, so it is not obviously refusable on local geometry alone. That half is deliberately NOT
/// fixed here (it is the issue-body's "angle 1" and needs its own blast-radius pass against #191).
///
/// So: this test is NOT a pin on #649's defect — the #649 pins are the asset-free
/// `movement::tests::depenetration_never_{mounts,drops}_a_swimmer_*` unit tests, which fail on the
/// unfixed controller. This one pins the remaining #329 live wedge, and when the haul-out half is
/// fixed its expectation flips to asserting the escape.
#[test]
#[ignore = "asset-gated: needs baked qcat.glb + qcat.wtr at $EQZONES (#357)"]
fn qcat_pocket_swim_plane_strands_the_swimmer_on_the_tile_floor() {
    let col = zone("qcat");
    let pocket_xy = [-42.3f32, 1036.8];
    let shaft = [-45.75f32, 1030.0625, -42.98];
    let surface = col.water_surface([pocket_xy[0], pocket_xy[1], -60.0]).unwrap();
    let plane = surface - PLAYER_BODY.float_depth;

    let end = swim_toward(&col, [pocket_xy[0], pocket_xy[1], plane], shaft, 12.0);
    assert!((end[2] - (-55.9687)).abs() < 0.01,
        "#329: from the pocket's own swim plane ({plane:.4}) the character ends on the tile floor at \
         −55.9687, the live wedge coordinate — since #649 landed this is the SWIMMING STEP-UP, not \
         the depenetration push-out (see this test's doc comment); got {end:?}");
    assert!(!col.in_water(end),
        "#329: and the mounted position is DRY (surface {surface:.5}), which is why buoyancy never \
         recovers it — got in_water=true at {end:?}");
    assert!((end[0] - shaft[0]).hypot(end[1] - shaft[1]) > 5.0,
        "#329: it never reaches the shaft — that is the strand; got {end:?}");
}

/// **#649 AT THE REAL COORDINATE: the push-out no longer teleports a swimmer vertically.**
///
/// One frame, from the exact position the #649 comment thread measured — the qcat pocket swim plane,
/// where `footprint_clear` is FALSE and `ground_below` is `Some(−69.969)`, so the depenetration net
/// runs and (on the unfixed controller) recovers with the NEARER floor: the tile floor 2.009 u ABOVE,
/// producing `(−42.252, 1037.071, −55.96875)`, `on_ground = true`, DRY. Here the same frame must end
/// at the SAME DEPTH it started, still afloat.
///
/// The asset-free pins for this are `movement::tests::depenetration_never_{mounts,drops}_a_swimmer_*`
/// (they run in CI); this one proves the same thing on the real baked geometry that produced the bug.
#[test]
#[ignore = "asset-gated: needs baked qcat.glb + qcat.wtr at $EQZONES (#357)"]
fn the_depenetration_push_out_holds_a_qcat_swimmer_at_its_own_depth() {
    let col = zone("qcat");
    let start = [-42.634235f32, 1036.1473, -57.977985];
    let mut c = CharacterController::new(start);
    // The walker's intent at a water waypoint: horizontal only, no vertical wish (so any vertical
    // motion observed is the controller's, not a drive's).
    c.step(MoveIntent { wish_dir: [0.55, -0.83], wish_vspeed: 0.0, jump: false, want_swim: true,
                        speed: 44.0, climb: 0.0, hop: false }, 1.0 / 60.0, &col);
    assert!((c.pos[2] - start[2]).abs() < 1e-3,
        "#649: frame 1 must hold the swim plane {:.5}; got {:?} (dz {:+.4} — the unfixed push-out \
         mounts the tile floor at −55.96875, dz +2.0092, which is the live #329 wedge coordinate)",
        start[2], c.pos, c.pos[2] - start[2]);
    assert!(!c.on_ground, "#649: a floating body must not be marked grounded: {:?}", c.pos);
    assert!(col.in_water(c.pos), "#649: and it must still be in the water: {:?}", c.pos);
}
