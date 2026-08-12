//! Unified character controller (design §2-3).
//!
//! The [`CharacterController`] is the SOLE owner of the local player's physical state — position,
//! vertical velocity, on-ground and in-water flags. Whoever drives (WASD on the render thread, or
//! the `/goto` planner on the nav thread) writes a [`MoveIntent`]; `step` integrates it against the
//! zone [`Collision`] using swept-cylinder collide-and-slide, native-parity ground/step handling,
//! and a depenetration / unstuck net, and returns the one authoritative position used for both the
//! render and the server stream. This replaces the old `override_pos` dual-authority artifact.

use crate::nav::collision::Collision;

// `MoveIntent` (the driver's per-frame wish) and `ControllerView` (the render→nav position snapshot)
// are pure inter-thread contract data — they moved DOWN into `eqoxide-ipc` (#544 Step 2c) so that
// crate's `NavIntent`/`ControllerShared` slots no longer up-reference `movement`. The BEHAVIOR that
// operates on them (`CharacterController::step`) stays here. Re-exported so every existing
// `crate::movement::{MoveIntent,ControllerView}` path across the tree keeps resolving unchanged.
pub use eqoxide_ipc::{ControllerView, MoveIntent};

// The controller's "I have stopped the body and cannot resume" disclosure (#724 review B1). Lives
// in `eqoxide-core` because it has to be nameable by BOTH `eqoxide-ipc` (`ControllerView::hold`)
// and `GameState::player_hold`, and core is the only crate below both. Re-exported here so the rest
// of the app crate can keep saying `crate::movement::ControllerHold`.
pub use eqoxide_core::game_state::{ControllerHold, ControllerHoldReason};

// Pure physics constants + kinematics moved DOWN into `eqoxide-core::physics` (#544 Step 2d) so nav
// stops up-referencing this app-layer module for them. Re-exported here so every existing
// `crate::movement::{PLAYER_RADIUS,STEP_UP,JUMP_VELOCITY,running_jump_reach}` path keeps resolving.
// `GRAVITY` was module-private (used only by `step` below) so it is `use`d, not re-exported.
pub use eqoxide_core::physics::{running_jump_reach, JUMP_VELOCITY, PLAYER_RADIUS, STEP_UP};
use eqoxide_core::physics::GRAVITY;

/// Skin width kept between the cylinder and the surface after a swept hit.
const SKIN: f32 = 0.05;
// The ground-probe geometry moved DOWN into `eqoxide-nav::collision` (#885) so that
// `Collision::body_placement` — the ONE definition of "can a body be placed here", read by
// `is_embedded` below AND by the published `/v1/observe/nav_debug` clearance probe — is stated
// once. Imported under the same names, so every use site in this module is unchanged.
use eqoxide_nav::collision::{GROUND_DEPTH, GROUND_ORIGIN};
const MAX_FALL: f32 = 128.0;

/// Vertical impulse for a nav auto-hop over a low fence/cart rail. Peak height = v²/(2·GRAVITY);
/// at 44 that clears ~8u, enough for the low pen fences that block `/goto` (#41). Only used in nav
/// mode (`MoveIntent::allow_hop`), so it never affects the native WASD jump feel.
const NAV_HOP_VELOCITY: f32 = 44.0;
/// How far ahead (in the move direction) a nav-hop probes for walkable floor beyond the barrier.
const HOP_REACH: f32 = 5.0;
/// Vertical band for the "floor just beyond" probe: the far floor must be within `+UP/-DOWN` of the
/// current foot height — a low fence (≈ level both sides), not a wall (far floor much higher, no
/// floor in band) or a ledge/cliff (far floor far below → would launch us off; don't hop).
const HOP_PROBE_UP: f32 = 3.0;
const HOP_PROBE_DOWN: f32 = 4.0;
/// Min seconds between nav auto-hops, so a barrier we can't actually clear doesn't become a
/// jump-in-place loop (the nav stuck-skip then routes around it instead).
const HOP_COOLDOWN: f32 = 0.8;
/// Max collide-and-slide iterations per move.
const MAX_SLIDE_ITERS: usize = 3;
/// Vertical tolerance for "still standing on the same floor".
const GROUND_SNAP_TOL: f32 = 0.5;
/// Slack on [`CharacterController::try_duck_under`]'s float-plane envelope bound (#870). The bound
/// it slackens is `STEP_UP + GROUND_SNAP_TOL` = 2.5 u; this is 1e-3 u of it. See the ⚠️ paragraph
/// on that clause for the measurement — without it the bound is decided by ~1e-5 u of float noise.
const DUCK_ENVELOPE_TOL: f32 = 1e-3;
/// Seconds embedded with no push-out before falling back to the last good grounded position.
const STUCK_FALLBACK_SECS: f32 = 0.5;
/// How often (seconds) a good grounded position is sampled into the ring buffer.
const GOOD_SAMPLE_SECS: f32 = 0.5;
/// Capacity of the last-good ring. Was a bare literal `8` at the one push site; #720's round-2
/// review then cited a `GOOD_RING` constant that did not exist, and #724 asked for the name so the
/// next citation is checkable.
///
/// **This number is DEAD, not merely untested.** The only production reads of the ring are
/// `self.good.back()` at the two recovery sites (the #150 fall-through guard in
/// [`CharacterController::step`] and the stuck fallback in [`CharacterController::depenetrate`]);
/// the push site below is its only other use, and it reads this constant solely to decide when to
/// drop the *oldest* entry — which nothing will ever look at. `GOOD_RING_LEN = 1` is therefore
/// behaviourally identical to `8`, the whole `VecDeque` could be an `Option<[f32; 3]>`, and #724's
/// M3 mutation (8 → 3, suite green) does not measure a tolerance — all it measures is that no test
/// reaches past the newest sample. What makes the depth *dead* is the read-site enumeration above,
/// which is a code fact, not a measurement. Do not read this as a tuned value, or "tune" it:
/// changing it cannot change behaviour. It is kept as a ring because a future recovery that
/// *chooses* among candidates is the obvious next step, and collapsing the type would have to be
/// undone to take it.
/// (#724 round-2 review, N3 — which established this, and was sharper than the PR's own caveat.)
const GOOD_RING_LEN: usize = 8;
/// Minimum spacing (seconds) between "the controller is HOLDING the body and cannot resume" log
/// lines. Such a branch changes nothing, so it re-runs every frame and would otherwise log at frame
/// rate for ever.
///
/// Introduced by #720's review as `UNDERWORLD_HOLD_LOG_SECS` for the fall-through guard alone.
/// Renamed and generalised by #724's review (B1): #724 clears the recovery ring on every position
/// discontinuity, which makes an empty ring routine on the DEPENETRATION path too — and that path
/// had no log at all, because its only `tracing::info!` sits inside `if let Some(&g) = good.back()`.
/// One constant, one throttle, both paths; a duplicated second copy is how #266/#543-class drift
/// starts. The throttle is re-armed whenever the hold's REASON changes, so a transition from one
/// hold to the other is never swallowed by the previous one's cooldown.
const HOLD_LOG_SECS: f32 = 5.0;

/// Ring push-out search radii (units).
const PUSHOUT_RADII: [f32; 6] = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];
/// Directions sampled per push-out ring.
const PUSHOUT_DIRS: usize = 16;
/// The placement test's ring is half this, and lives in nav now (#885). Static-asserted rather than
/// re-derived, so a change to either number is a compile error and not a silent divergence between
/// what the controller refuses and what the diagnostic reports.
const _: () = assert!(PUSHOUT_DIRS / 2 == eqoxide_nav::collision::PLACEMENT_RING_DIRS);

/// #845 — reach of the LAST-RESORT placement search ([`nearest_standing_place`]), in units.
///
/// Deliberately an order of magnitude past [`PUSHOUT_RADII`], because the two searches answer
/// different questions. The push-out asks *"can this body be nudged out of the thing it is inside"*,
/// which is a local question and is right to be local. This one asks *"is there anywhere at all in
/// this zone this body could stand"*, and it runs only when the answer to the first was no AND
/// there is no banked history — i.e. only from a state that was, before #845, permanently frozen.
///
/// **Sized against a measurement, not a guess.** The live #845 casualty was at
/// `(-2190.5, 902.125, 3.5)` in steamfont; a scan of that zone's baked GLB (the `__collision__`
/// mesh, the rendered terrain and every placed object, 63 391 triangles) found **zero** triangles
/// over that column, the nearest vertex of any kind 15.7 u away at h ≈ -32652 (invisible-boundary
/// art), the nearest terrain vertex 121.5 u away at h = 101.0, and the nearest column holding a
/// floor within ±200 u of the feet **133 u away**. That independently reproduces the number
/// `CharacterController::forget_recovery_history` records from #712 ("nearest standable floor 133 u
/// away"). 32 u cannot reach it; 512 u can, with room for a worse case.
///
/// This is a REACH, not a tuning: raising it lets more bodies be rescued and rescues them from
/// further away, lowering it strands more of them. There is no "correct" value to converge on —
/// it is bounded by what a client-side relocation is worth, which is why the log line reports the
/// distance actually travelled.
const RESCUE_RADII: [f32; 15] = [4.0, 8.0, 16.0, 32.0, 48.0, 64.0, 96.0, 128.0,
                                 160.0, 192.0, 256.0, 320.0, 384.0, 448.0, 512.0];
/// Directions sampled per last-resort ring. Twice [`PUSHOUT_DIRS`] because the rings are far wider:
/// at 512 u, 16 spokes leave 200 u gaps between samples.
const RESCUE_DIRS: usize = 32;
/// Vertical band the last-resort search looks through, above AND below the body's feet.
///
/// Wider than [`GROUND_DEPTH`] and symmetric, because the body it serves has no column of its own
/// to anchor on: in the measured steamfont case the nearest real ground is ~81 u *above* the feet,
/// and a body already held at the underworld floor is below everything. `GROUND_DEPTH`'s
/// down-only band is the right question for "what am I standing over"; it is the wrong question
/// for "where could I stand instead".
const RESCUE_BAND: f32 = 1000.0;
/// Minimum seconds between last-resort searches that FAIL. A successful one moves the body and so
/// cannot repeat; a failing one is ~500 column probes that will fail again next frame for the same
/// reason, and this branch re-runs at frame rate for as long as the hold lasts.
const RESCUE_RETRY_SECS: f32 = 1.0;
/// Buoyancy: vertical settle rate toward the swim plane (u/s). The plane itself —
/// `surface − float_depth` — comes from the shared [`crate::traversability::PLAYER_BODY`]
/// (#359/#386: the planner sizes water exits from the same `float_depth`/`haul_out_up` fields,
/// so the two sides cannot drift apart again). Was two duplicated locals in the two swim branches.
const BUOY_RATE: f32 = 30.0;

// `MoveIntent` moved to `eqoxide-ipc` (#544 Step 2c) — re-exported at the top of this module.

/// Convert a world `(east, north)` movement request into a unit `wish_dir` plus the EQ heading
/// (CCW degrees, 0 = north) to face while moving it. Returns `None` heading when the request is
/// ~zero (stand in place — e.g. a jump with no direction). Used by the HTTP manual-move escape
/// hatch (#188) to drive the controller directly, like WASD, when A* has stranded the character.
pub fn manual_wish(dir: [f32; 2]) -> ([f32; 2], Option<f32>) {
    let len = (dir[0] * dir[0] + dir[1] * dir[1]).sqrt();
    if len > 1e-4 {
        let wish = [dir[0] / len, dir[1] / len];
        // The render loop's forward vector is (-sin h, cos h), so h = atan2(-east, north).
        let heading = crate::coord::eq_heading(wish[0], wish[1]);
        (wish, Some(heading))
    } else {
        ([0.0, 0.0], None)
    }
}

// `ControllerView` moved to `eqoxide-ipc` (#544 Step 2c) — re-exported at the top of this module.

// ── #776: the trapped-swimmer disclosure — DEFINED IN `eqoxide_core::afloat` ────────────────────
//
// The signal, its two thresholds, the frame classification and the window clock all live in
// `crates/eqoxide-core/src/afloat.rs`, together with the whole design record that used to sit here:
// why it is a module and not just private fields, why the WISH half is horizontal while the
// PROGRESS half is 3-D, and the false-negative classes it does not cover. It moved DOWN in #801
// because publishing it over HTTP means `eqoxide_ipc::ControllerView` and `GameState` have to name
// the type, and neither can depend on this crate. That module also records what the move widened.
//
// This file now holds only the CALL SITES: `step` folds one classified frame in, `clear_hold` and
// `teleport` drop the window, `afloat_stall()` reports it, and `app.rs` publishes what it reports.

// Re-exported so every existing `crate::movement::AfloatStall` path keeps resolving — the same
// treatment `ControllerHold` gets above, and for the same reason.
pub use eqoxide_core::afloat::AfloatStall;
use eqoxide_core::afloat::{AfloatFrame, AfloatStallClock, AFLOAT_PROGRESS};
/// Only the tests in this file read the maturity threshold — the runtime path asks
/// `AfloatStallClock::stall()` rather than re-deriving the comparison, which is the whole point of
/// keeping the clock's state private. A plain `use` here is an unused import in a non-test build.
#[cfg(test)]
use eqoxide_core::afloat::AFLOAT_STALL_SECS;

/// Minimum spacing (seconds) between afloat-stall log lines. Its own throttle rather than a share of
/// `hold_log_cooldown`: the two disclosures are independent, and one silently swallowing the other's
/// first line is exactly the drift a shared clock invites.
const AFLOAT_STALL_LOG_SECS: f32 = 5.0;

/// Sole owner of the local player's physical state. Position is `[east, north, z]` (server coords,
/// `z` = feet).
pub struct CharacterController {
    pub pos:       [f32; 3],
    pub vel_z:     f32,
    pub on_ground: bool,
    pub in_water:  bool,
    /// Recent grounded, non-embedded positions for the last-good fallback (§3.3).
    good:          std::collections::VecDeque<[f32; 3]>,
    good_timer:    f32,
    /// Seconds until the hold log line may be emitted again (see `HOLD_LOG_SECS`). Diagnostics
    /// only — no physics reads this.
    hold_log_cooldown: f32,
    /// The hold in force THIS frame, or `None` (#724 review B1). Cleared unconditionally at the top
    /// of every [`Self::step`] and re-set only by a branch that is actively holding the body, so it
    /// is level-triggered by construction and cannot outlive its cause. Read by `app.rs` into
    /// `ControllerView::hold` → `GameState::player_hold` → `GET /v1/observe/debug` `player.hold`
    /// (#817 — the last hop was missing until then; `GET /v1/observe` is not a registered route).
    ///
    /// Physics never reads this; it is purely the disclosure that the body is frozen. `secs` is the
    /// controller's own accumulated frame time for the current, unbroken hold.
    hold:          Option<ControllerHold>,
    stuck_time:    f32,
    /// #845: seconds until another FAILED last-resort placement search may be attempted (see
    /// [`RESCUE_RETRY_SECS`]). Diagnostics/cost only — no physics reads this, and a value of 0
    /// changes nothing but how often ~500 column probes are spent on a question that just failed.
    /// Reset by [`Self::teleport`], because a relocation is a new predicament and deserves an
    /// immediate answer rather than the tail of the previous one's cooldown.
    rescue_cooldown: f32,
    /// Seconds until another nav auto-hop is allowed (prevents jump-spamming a wall we can't clear).
    hop_cooldown:  f32,
    /// Zone "underworld" floor from OP_NewZone (`GameState::zone_underworld`), or NEG_INFINITY when
    /// unknown. The step never lets the character descend to/below this Z: a collision gap that
    /// would drop us onto deep below-world boundary geometry (Nektulos river bottom ≈ -199, below
    /// the zone's -189 underworld) instead recovers to the last good grounded position, so the
    /// server never sees a below-world position and doesn't ZoneToBindPoint + CLE-drop us (#150).
    underworld:    f32,
    /// Airborne-height tracking for the driver-agnostic fall-damage signal (§442, #442). `airborne_start_z`
    /// is the feet Z captured the frame `on_ground` goes true→false (a genuine airborne stretch
    /// begins); it is `None` while grounded and is CLEARED by `teleport` and by the depenetration net
    /// / underworld recovery, so neither a server correction nor a mid-fall push-out is misread as a
    /// fall. On the landing frame (`on_ground` false→true via the gravity path) `landed_fall_height`
    /// latches `Some(start − landing_z)` — a one-shot the nav thread take-and-clears once to apply
    /// fall damage. Height ALWAYS comes from this tracked airborne start, never a nav waypoint z.
    airborne_start_z:   Option<f32>,
    landed_fall_height: Option<f32>,
    /// #529: the self-player currently has a Levitate effect — gravity is OFF and the character
    /// HOVERS at altitude, free-floating over land/gaps/water instead of falling. Set each frame
    /// from the server-authoritative buff/appearance state via [`Self::set_levitating`] (like
    /// `underworld`), so EVERY driver — WASD, manual, nav — gets gravity-off with no per-`MoveIntent`
    /// plumbing. Non-levitate physics is byte-identical (this only adds a branch when the flag is set).
    levitating: bool,
    /// #444: set while THIS frame's `swim_sink` produced a GENUINE downward delta (the character
    /// actually descended through open water — resolved delta `< -SKIN` — not just held a down-wish
    /// that clamped to ~0 against a submerged floor, and not passive buoyancy). Read at the top of
    /// the NEXT frame, before being overwritten, to tell a genuine "swam down and out the bottom of a
    /// suspended water volume" exit apart from water merely disappearing out from under a character
    /// (a zone/collision swap, walking onto shore, or drifting LATERALLY out of a pond that sits on a
    /// flush floor) — only the former should re-arm a fresh airborne start; #442 DEFECT-1 ("water
    /// breaks a fall") must still hold for every other water-exit path. Keying this off actual
    /// descent (not `wish_vspeed`'s sign) is what keeps a sideways exit from false-arming a fall.
    swim_sinking: bool,
    /// #776: the afloat no-progress window. See `eqoxide_core::afloat`'s module docs for why the
    /// trapped swimmer needs its own signal instead of a `ControllerHoldReason`. Recomputed every
    /// stepped frame from that frame's own facts; reset by [`Self::teleport`] and
    /// [`Self::clear_hold`] for the same reasons those reset the hold.
    afloat: AfloatStallClock,
    /// Seconds until the afloat-stall log line may be emitted again (see `AFLOAT_STALL_LOG_SECS`).
    /// Diagnostics only — no physics reads this.
    afloat_log_cooldown: f32,
}

#[inline]
fn hlen(d: [f32; 3]) -> f32 { (d[0] * d[0] + d[1] * d[1]).sqrt() }

/// Chest height above the feet — where a body is probed for water when its FEET are dry.
const WATER_BODY: f32 = 3.0;

/// The point every water query about a body whose feet are at `p` must use: the feet when they are
/// already in water (so wading is unchanged), else chest height (#329).
///
/// `pos` is the character's FEET. A character standing on the bottom of a pool can have its feet a
/// hair BELOW the water region's lower bound while its whole body is submerged — the water volume is
/// baked from the `.wtr` BSP and does not have to meet the floor exactly. The qcat spawn shaft is
/// exactly this: the floor is at z=-69.97 and the water spans -69.5 … -43.0, so a character standing
/// there is under 26 UNITS of water while a feet-only probe reports it bone dry.
#[inline]
fn water_probe(col: &Collision, p: [f32; 3]) -> [f32; 3] {
    if col.in_water(p) { p } else { [p[0], p[1], p[2] + WATER_BODY] }
}

/// Is the BODY (not just the feet) at `p` in water? The single predicate `step` and the
/// depenetration net share, so the two cannot disagree about who is swimming (#649).
#[inline]
fn body_in_water(col: &Collision, p: [f32; 3]) -> bool {
    col.in_water(water_probe(col, p))
}

/// The depenetration net's OWN definition of "this body is not in a legal place": its footprint is
/// pierced by geometry, or there is no floor anywhere in the column beneath it (it has fallen out of
/// the world). Hoisted out of [`CharacterController::depenetrate`] so the net and the RECOVERY it
/// picks are decided by the same predicate.
///
/// That sharing is load-bearing, not tidiness (#649 review, finding 1). A recovery that is itself
/// embedded is not a recovery: the next frame re-enters the net from the new position and picks the
/// next candidate, and the body walks off across the zone one ring-radius at a time, ignoring input.
/// The `floor.is_none()` half is how that happened — a swimmer in deep water with a CLEAR footprint
/// and no floor within `GROUND_DEPTH` below is "embedded" by this predicate, and every ring candidate
/// around it is equally so.
///
/// #885 moved the disjunction itself DOWN to [`Collision::body_placement`], which names the two
/// halves instead of or-ing them into a bool. This function is now a thin read of that verdict, and
/// so is `/v1/observe/nav_debug`'s `clearance.body` — one predicate, two readers. Before, the
/// diagnostic never asked this question at all and published "open in every direction" for a body
/// this function was returning `true` for.
fn is_embedded(col: &Collision, p: [f32; 3]) -> bool {
    col.body_placement(p).is_embedded()
}

/// #845 — **the nearest place in THIS zone where this body could legally stand**, or `None` if the
/// zone offers none within [`RESCUE_RADII`] × [`RESCUE_BAND`] of `from`.
///
/// # Why this exists at all: recovery used to be a fact about the body's PAST
///
/// Both of the controller's "no recovery" branches — the stuck fallback in
/// [`CharacterController::depenetrate`] and the #150 fall-through guard in
/// [`CharacterController::step`] — recovered by restoring a banked position from the `good` ring.
/// #724 then made the ring cleared on every position discontinuity and on every zone change, for
/// good reasons (a restored sample from another zone names a point in a different coordinate
/// space). The two facts compose into a trap: **the events that most often put a body somewhere it
/// cannot be are exactly the events that erase the only thing that could get it out.** After a
/// relocation the ring is empty by construction, so both branches take their `None` arm, and the
/// `None` arm of the depenetration one changes nothing at all — same `pos`, same `on_ground`, same
/// empty ring, `depenetrate` returns `true`, `step` early-returns. The next frame is bit-identical.
/// It is an absorbing state of the controller's state machine: nothing the driver can do — WASD,
/// `/move`, `/goto`, jump, swim — writes any of the variables the branch reads. Only an external
/// [`CharacterController::teleport`] (a GM `#summon`, a large server correction) or a change of
/// collision can leave it. That is issue #845, and it cost two live validation runs.
///
/// The fix is not a wider push-out and not a bigger ring: it is to make the recovery a fact about
/// the WORLD instead of a fact about the body's history. "Somewhere in this zone this body could
/// stand" is available whenever the zone's collision is, no matter what the body did before, and it
/// is what a GM does by hand when they `#goto` a wedged character to real ground.
///
/// # What counts as a place
///
/// A candidate column `(e, n)` supplies a floor `f` (nearest to the feet within `±RESCUE_BAND` —
/// UP as well as down, see [`RESCUE_BAND`]), and `[e, n, f]` is accepted only if:
///
/// * `f > underworld` — the #150 guard would refuse to let the body rest there, so putting it there
///   accomplishes nothing but a second predicament (the same test [`zone_in_reground`] applies);
/// * `!is_embedded(col, [e, n, f])` — **the net's own door predicate**, so the destination is by
///   construction not a place the net will immediately take custody of again. This is what stops
///   #649's "a recovery that is itself embedded is not a recovery … the body walks off across the
///   zone one ring-radius at a time, ignoring input". Checking the door's exact predicate rather
///   than a look-alike is deliberate: a look-alike is how that bug happened (the old check tested
///   the footprint and not `ground_below`'s nav-headroom filter);
/// * `!body_in_water(col, [e, n, f])` — a dry standing place. #649 made "afloat in water AND
///   `on_ground`" unrepresentable through [`Recovery`]; this search must not smuggle it back in by
///   handing [`CharacterController::recover`] a submerged floor. **The cost is stated, not hidden:**
///   a body whose only nearby ground is under water is not rescued and keeps its hold.
///
/// Rings are tried nearest-first and the search stops at the first radius that yields anything, so
/// the body is moved the least distance the zone allows; within a ring the smallest `|f - from[2]|`
/// wins. This is nearest-in-the-sampled-set, not a true nearest — the sampling is polar
/// (`RESCUE_DIRS` spokes), so a place between two spokes at ring `r` can lose to one on a spoke at
/// ring `r`. It is a placement search, not a metric.
fn nearest_standing_place(col: &Collision, from: [f32; 3], underworld: f32) -> Option<[f32; 3]> {
    // Probe from the feet, but never look BELOW the underworld: `nearest_floor` returns the nearest
    // floor and nothing else, so a column whose nearest surface is below-world boundary art would
    // answer with that one and be discarded — hiding a perfectly good floor higher up the same
    // column. That is the #712 shape and it is not hypothetical here: the zone whose bake put the
    // #845 casualty over a void also carries invisible-boundary art ~32 000 u down, which is what
    // an unclamped probe on those columns would return and then throw away. Without the clamp the
    // search's answer depends on how deep the below-world art happens to sit. With the default
    // underworld (−∞) both values fall back to the plain symmetric band, so zones that declare no
    // underworld are unaffected.
    let ref_z = from[2].max(underworld);
    let down = (ref_z - underworld).max(0.0).min(RESCUE_BAND);
    for &r in &RESCUE_RADII {
        let mut best: Option<([f32; 3], f32)> = None;
        for i in 0..RESCUE_DIRS {
            let a = (i as f32) / (RESCUE_DIRS as f32) * std::f32::consts::TAU;
            let (e, n) = (from[0] + a.cos() * r, from[1] + a.sin() * r);
            let Some(f) = col.nearest_floor(e, n, ref_z, RESCUE_BAND, down) else { continue };
            if f <= underworld { continue; }
            let q = [e, n, f];
            if is_embedded(col, q) || body_in_water(col, q) { continue; }
            let dz = (f - from[2]).abs();
            if best.map_or(true, |(_, b)| dz < b) { best = Some((q, dz)); }
        }
        if let Some((q, _)) = best { return Some(q); }
    }
    None
}

/// What the one-shot zone-in reground should do with a freshly-arrived body (#712).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reground {
    /// Lift the body onto this floor height and mark it grounded.
    Lift(f32),
    /// Do nothing, and retire the one-shot. This is a control-flow verdict, NOT a claim that the
    /// body is settled on anything: a swimmer gets `Retire` because the swim branch owns it, not
    /// because it is standing on a floor.
    Retire,
    /// Nothing to stand on anywhere — leave the one-shot armed and look again next frame.
    Wait,
}

/// Decide the one-shot reground applied after a zone change, once the new zone's collision is in.
///
/// The server's arrival coordinate and our baked collision are two independent models of the same
/// zone, so the spawn z routinely lands a fraction UNDER the arrival surface. That is invisible to
/// the ground clamp, which probes downward from `foot + GROUND_ORIGIN` (1 u): a floor even 1.1 u
/// overhead sits above the probe origin and is not seen, so the body free-falls to whatever the
/// next surface down happens to be.
///
/// #712, measured live (lfaydark → steamfont): the server corrected the character to
/// `(2205, 579, -114.4)`; our baked floor in that column is `-113.25` — 1.15 u overhead — with the
/// next floor down at `-232.0` and the zone underworld at `-222.0`. The body fell past the
/// underworld, the #150 fall-through guard fired, and it recovered onto a stale PREVIOUS-zone
/// coordinate (see [`CharacterController::forget_recovery_history`]) and wedged permanently.
///
/// **The discriminator is what is BELOW, not how far above.** An earlier revision of this function
/// lifted whenever a floor sat 1–3 u overhead, and #720 review measured what that costs: a body
/// arriving 3 u above ordinary ground with a walkable slab 2.4 u overhead was teleported onto the
/// slab, because [`Collision::nearest_floor`] anchors on distance to the body and the body's own
/// floor falls outside the band as soon as it is more than 1 u away. Height above is simply not the
/// signal — many arrivals sit a few units above their floor, and that is an ordinary drop.
///
/// What actually distinguished #712 is that the body had **nowhere legal to land**: the only
/// surface under it was 117 u down and *below the zone's underworld*, which the fall-through guard
/// was going to refuse. So the gate here is that guard's *underworld* conjunct — `landing_valid` is
/// `cand <= f && f > underworld`, and this applies only the second half, because there is no
/// candidate step height to compare against at zone-in. (An earlier revision of this sentence
/// claimed the gate was "exactly that guard's own `landing_valid` test"; #720 review measured that
/// it is not, and the narrower claim is the true one.) If a
/// floor below would be accepted, the body just falls onto it and we do nothing. Only when there is
/// no such floor — void, or nothing but sub-underworld geometry — do we look up, and then we take
/// the nearest floor above wherever it is, because the alternative is falling out of the world. The
/// same landability test applies to the destination: a floor that is itself below the underworld is
/// not worth lifting onto, because the body would simply meet the guard from there instead.
///
/// A body in water is never touched. #649 made "in water AND `on_ground`" unrepresentable through
/// [`Recovery`], and the caller marks every `Lift` `on_ground`; the swim/buoyancy branch owns a
/// swimmer, so the one-shot retires without acting rather than inventing a water recovery here.
///
/// With no underworld known (`None` — before `OP_NewZone`) every floor is landable, so this reduces
/// to the original "nothing below at all" behaviour. That is the right conservative answer, because
/// the fall-through guard this exists to pre-empt is itself disabled in that state
/// (`fall_through_guard_disabled_when_underworld_unknown`).
///
/// Known and NOT changed here: `ground_below` inherits the nav headroom filter, so a body standing
/// on ground with less than `NAV_AGENT_HEIGHT` of clearance reports nothing below and takes the
/// upward branch — it can be lifted onto the low deck above it. #720 review measured that
/// (floor 0, deck 4 u up, body at 0 → `Lift(4.0)`). It is pre-existing (the original code did the
/// same) and is recorded here rather than claimed away.
pub fn zone_in_reground(col: &Collision, p: [f32; 3], underworld: Option<f32>) -> Reground {
    if body_in_water(col, p) { return Reground::Retire; }
    let underworld = underworld.unwrap_or(f32::NEG_INFINITY);
    // The same predicate as the fall-through guard's `landing_valid` in `step`: a floor at or below
    // the underworld is not somewhere the body is allowed to come to rest.
    if col.ground_below(p[0], p[1], p[2] + GROUND_ORIGIN, GROUND_DEPTH)
          .is_some_and(|f| f > underworld) {
        return Reground::Retire;
    }
    // `down = GROUND_ORIGIN` keeps a floor the body is already resting on inside the band, so
    // `nearest_floor`'s distance anchoring returns THAT rather than something higher up, and the
    // `f > p[2]` arm below then declines to move.
    match col.nearest_floor(p[0], p[1], p[2], GROUND_DEPTH, GROUND_ORIGIN) {
        // The target has to be somewhere the guard would ACCEPT, or the lift accomplishes nothing —
        // moving a body from one sub-underworld position to another still ends in the guard.
        // (`f > p[2]` is a rail rather than a behaviour: any landable floor at or below the feet
        // would have been returned by `ground_below` above and retired us already.)
        Some(f) if f > p[2] && f > underworld => Reground::Lift(f),
        _ => Reground::Wait,
    }
}

/// Where the depenetration net is allowed to put a body — **and in what support state**.
///
/// The net used to write `pos` and `on_ground` inline, which made an illegal state trivially
/// expressible and, in qcat, actually expressed: a body IN WATER placed on a FLOOR and marked
/// `on_ground` (#649). The net recovered any "embedded" body by hunting the NEAREST floor with
/// `nearest_floor(up = STEP_UP + GROUND_ORIGIN, down = GROUND_DEPTH)` — whichever floor is closer,
/// not one the character can occupy — so it teleported swimmers in BOTH directions: UP onto the
/// tile floor 2.009 u above the qcat pocket's swim plane (0.009 u above the waterline, hence dry,
/// hence buoyancy never fires again — the live #329 wedge coordinate), and DOWN 10–12 u onto the
/// pool floor from anywhere below it.
///
/// #649/#658 answered that with a second, `Afloat` variant: the net still ran for a swimmer, but
/// recovered it at its own depth whenever the ring candidate was still water. **#661 measured the
/// two ways that HALF-measure still failed at the same coordinate** (see
/// [`CharacterController::depenetrate`]) **and removed the swimmer from the net entirely: a body
/// afloat in water never enters the net, so there is no afloat recovery to get wrong and the
/// `Afloat` variant is GONE.** What remains is the invariant in its strongest form: constructing a
/// `Recovery` is the ONLY way the net moves the character, the only constructible recovery is a
/// grounded one, and the only bodies that can reach the constructor are dry.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Recovery {
    /// Standing on solid floor at this z. Feet supported: `on_ground = true`.
    Grounded(f32),
}

impl Recovery {
    /// The recovery available in the candidate column `(e, n)` for a DRY body whose feet are at
    /// `z`: the nearest floor within the step band above / `GROUND_DEPTH` below. Byte-identical to
    /// the behaviour the net has always had for a dry body.
    ///
    /// > ### ⚠️ Correction (#661)
    /// > Until #661 this took an `afloat` flag, and its doc said an afloat body whose candidate
    /// > column was NOT water had "left the water laterally" and so "takes the original floor
    /// > search, byte-identical" — and the code did. Both halves were wrong. The body had not left
    /// > the water — it was still afloat at its own position; only the RING CANDIDATE was outside
    /// > the `.wtr` region's XY extent. Handing that case the water-blind `nearest_floor` hunt was
    /// > the #649 defect through a side door, and it was the measured writer of the #661 strand:
    /// > at the qcat spawn pocket, wet candidates held the swimmer `Afloat` through 16 push-outs
    /// > and then one candidate fell a fraction of a unit outside the water region and the
    /// > fall-through beached the still-swimming body onto the tile floor at −55.96875 — 0.009 u
    /// > above the waterline, DRY, `on_ground` — where `want_swim` is inert and the transition is
    /// > one-way. The `afloat` arm is not "fixed"; the call can no longer happen (see
    /// > [`CharacterController::depenetrate`]).
    fn at_column(col: &Collision, e: f32, n: f32, z: f32) -> Option<Self> {
        col.nearest_floor(e, n, z, STEP_UP + GROUND_ORIGIN, GROUND_DEPTH).map(Recovery::Grounded)
    }

    fn z(self) -> f32 { match self { Recovery::Grounded(z) => z } }
    fn on_ground(self) -> bool { matches!(self, Recovery::Grounded(_)) }
}

impl CharacterController {
    pub fn new(pos: [f32; 3]) -> Self {
        Self { pos, vel_z: 0.0, on_ground: false, in_water: false,
               good: std::collections::VecDeque::new(), good_timer: 0.0, hold_log_cooldown: 0.0,
               hold: None,
               stuck_time: 0.0, rescue_cooldown: 0.0,
               hop_cooldown: 0.0, underworld: f32::NEG_INFINITY,
               airborne_start_z: None, landed_fall_height: None, levitating: false,
               swim_sinking: false,
               afloat: AfloatStallClock::default(), afloat_log_cooldown: 0.0 }
    }

    /// Take-and-clear the one-shot fall height (feet dropped during the airborne stretch just
    /// landed). Driver-agnostic (§442, #442): the nav thread reads this each tick and applies fall
    /// damage — WASD and nav alike. Edge-triggered and consumed exactly once; `None` on every frame
    /// except the one right after a genuine landing (never after a teleport / depenetration recovery).
    pub fn take_landed_fall_height(&mut self) -> Option<f32> {
        self.landed_fall_height.take()
    }

    /// Set the zone underworld floor (from `GameState::zone_underworld`); `None` disables the clamp.
    /// Called on zone load so the fall-through guard in `step` uses the current zone's threshold (#150).
    pub fn set_underworld(&mut self, underworld: Option<f32>) {
        self.underworld = underworld.unwrap_or(f32::NEG_INFINITY);
    }

    /// #529: set the self-player's Levitate state (from the server-authoritative buff/appearance
    /// state, mirrored into `GameState::player_levitating`). Called each frame from the render loop,
    /// like [`Self::set_underworld`], so the gravity-off hover branch in `step` follows the live buff
    /// as it is cast and fades. Toggling it does NOT teleport or zero velocity — the next `step`
    /// simply stops (or resumes) applying gravity; a buff fading mid-air resumes a normal fall.
    pub fn set_levitating(&mut self, levitating: bool) {
        self.levitating = levitating;
    }

    /// Forget the last-good recovery ring. **Call this on every zone change.**
    ///
    /// `good` holds bare `[east, north, height]` samples with no zone tag, and both paths that read
    /// it — the underworld fall-through guard in [`Self::step`] and the stuck fallback in
    /// [`Self::depenetrate`] — restore `pos` from it verbatim. Carried across a zone change those
    /// numbers name a point in a DIFFERENT zone's coordinate space, so "recovery" drops the body
    /// wherever they happen to land in the new one. There is no way for either caller to tell: the
    /// ring is just three floats, and they are always finite and always plausible.
    ///
    /// Measured in #712 (lfaydark → steamfont): the ring's newest sample was the lfaydark position
    /// `(-2190.08, 911.27, -4.78)`; steamfont has no geometry AT ALL in that column (nearest
    /// standable floor 133 u away), so [`is_embedded`] is permanently true there, no ring candidate
    /// yields a [`Recovery`], and the 0.5 s stuck fallback restored that same point every 0.5 s
    /// indefinitely — while the nav planner, correctly, reported `start_isolated`.
    ///
    /// A SAME-zone relocation is not safe either, even though the ring then names points in the
    /// current zone: not-nonsense is not correct, and #724's controller-level tests measure the
    /// same-zone failure end to end
    /// (`a_large_same_zone_relocation_forgets_the_pre_relocation_recovery_ring` and the
    /// stuck-fallback twin). [`Self::teleport`] therefore clears the ring itself.
    ///
    /// > ### ⚠️ Correction (#724 round-2 review, B2)
    /// > The stale window is **unbounded**, but not for the reason first written here. The claim
    /// > was "the ring banks ONLY while `on_ground`, so a body relocated into a column it can only
    /// > fall out of never banks again" — reasoned from the banking site's `on_ground` gate under a
    /// > **"Measured:"** label it had not earned. Instrumenting the pre-fix behaviour (this method's
    /// > own `zone_with_a_hole` fixture, driven by a `teleport` that does not clear, printing the
    /// > ring every 30 frames) measured it FALSE:
    /// >
    /// > ```text
    /// > f0 : pos=[80.0, 0.0, -114.53]  on_ground=false ring_len=4
    /// > f30: pos=[80.0, 0.0, -180.53]  on_ground=false ring_len=4
    /// > f60: pos=[-80.0, 0.0, 0.0]     on_ground=TRUE  ring_len=6
    /// > f90: pos=[-80.0, 0.0, 0.0]     on_ground=true  ring_len=8
    /// > ```
    /// >
    /// > The body IS grounded again ~1.5 s later and the ring DOES bank again (4 → 6 → 8). The real
    /// > mechanism is the fall-through guard's recovering arm in [`Self::step`]: it does
    /// > `self.pos = g; self.on_ground = true;` — it re-grounds the body **onto the stale sample**,
    /// > so every fresh bank is a copy of that same stale point. The window is unbounded because the
    /// > loop **self-reinforces**, not because banking stops.
    /// >
    /// > Not an inert error: it is what hid the hold disclosure now implemented as
    /// > [`ControllerHold`]. Had `step`'s recovering arm been read instead of reasoned about, the
    /// > next question was what its `None` arm does *after* the fix — leave `on_ground` false for
    /// > ever — and what the depenetration twin does, which is nothing at all, silently. "Never
    /// > grounded again" made the hold look self-announcing when it was mute.
    ///
    /// **The `app.rs` zone-change call is NOT made redundant by the fold, and must not be deleted.**
    /// A zone change must drop the ring whether or not a [`Self::teleport`] happens to accompany it,
    /// and there is a known arrival shape where none does: the #593 note in `eqoxide-net`'s
    /// `action_loop.rs` (`stream_position`) describes a cross-zone arrival landing *within*
    /// `CORRECTION_SQ` (12 u, 2-D) of the last position we streamed from the OLD zone, in which case
    /// the streamer's correction branch is skipped entirely and no `teleport` is ever called.
    /// `app.rs` also calls this at the moment the old zone's collision is dropped — earlier than,
    /// and independent of, the arrival reground.
    ///
    /// That #593 gap is **read off the branch structure, not measured on the wire** — no run has
    /// been captured landing in it. The independent reason above (the collision-drop clear happens
    /// earlier than any arrival, so it cannot be the arrival's job) needs no measurement and is the
    /// one to lean on.
    ///
    /// Round-2 review (N4) measured that the call site had no test holding it in place: deleting it
    /// from `app.rs`, together with the #712 test's own direct call and `is_empty` assert, left the
    /// suite green (154 passed), because that test's *behavioural* assertions are now satisfied by
    /// the `teleport` two lines below them rather than by the zone-change clear. The call site is
    /// pinned now — see `the_zone_change_reload_block_still_forgets_the_recovery_ring` in this
    /// module's tests, which reads `app.rs`'s own source and fails by name if the call leaves the
    /// `zone_needs_reload` block.
    pub fn forget_recovery_history(&mut self) {
        self.good.clear();
    }

    /// The hold currently in force, or `None` — see [`ControllerHold`]. `None` includes the ordinary
    /// "standing still because nothing asked me to move", which is exactly the state a hold is
    /// otherwise indistinguishable from.
    ///
    /// Published by `app.rs` into `ControllerView::hold` on every RENDERED frame; never latched.
    /// On frames that render but do not step (no collision, mid zone-load) `app.rs` calls
    /// [`Self::clear_hold`] instead; an idle render loop does not publish at all, and cannot
    /// manufacture a false hold because freeing the body also requires a stepped frame.
    /// (#724 round-3 review, N1 — this used to say "every frame" flatly.)
    ///
    /// **Why "freeing the body also requires a stepped frame" is a fact about THIS value, and what
    /// it does not cover (#846).** The obvious attack is a GM `#summon`, which lands on the NET
    /// thread while the render loop idles. It cannot free the body: `CharacterController` lives in
    /// this crate, the root binary crate, and `eqoxide-net` cannot depend on it without a dependency
    /// cycle — so the net thread has no handle to reach. All it can do is publish coordinates into
    /// `ipc::PosCorrection` and wait; `app.rs`'s rendered frame is what takes them, and it takes
    /// them by calling [`Self::teleport`], which drops the hold before the `step` beside it
    /// recomputes. Adopting the summon and clearing the hold are therefore the same frame.
    ///
    /// **Scope that claim carefully — #846's round-1 review overturned the wider reading of it.**
    /// The crate graph makes "the net thread calls a method on `CharacterController`"
    /// unrepresentable. It does NOT make "the net side cannot publish a wrong hold" unrepresentable:
    /// the published field is `GameState::player_hold`, not this one, and the net thread owns that
    /// copy. Two net paths change it deliberately — `GameState::begin_zone_in` (paired with a clear
    /// of the `ControllerView` through `eqoxide_ipc::ControllerSlots::begin_zone_in`, because
    /// clearing only the copy was measured to survive exactly one net tick) and the correction
    /// branch of `ActionLoop::stream_position`, which withdraws the disclosure on the tick it hands
    /// the jump over rather than pairing a fresh position with the old predicament. Both withdraw;
    /// neither invents. That "never invents" property is what
    /// `no_net_tick_can_free_or_manufacture_a_hold_846` in `eqoxide-net`'s `action_loop` tests
    /// property-tests, with `the_hold_mirror_tracks_the_render_thread_over_time_846` beside it for
    /// the withdrawal axis.
    ///
    /// What none of that establishes is how long an idle loop can stay idle. `app.rs`'s
    /// `poll_external` bounds it — a pending `pos_correction`, or any `GameState` change, marks the
    /// loop active — but that call site needs a GPU and a window and is reachable by no test here;
    /// wrapping it dead (`if false && …`, the stronger form — the line still compiles and is still
    /// reached by `--all-targets`) was measured to leave the workspace green, with five figures
    /// byte-identical to a clean run (#846). It is the latency bound, not
    /// the honesty guarantee.
    pub fn hold(&self) -> Option<ControllerHold> { self.hold }

    /// #776: the afloat stall in force, or `None` — see [`AfloatStall`] and `eqoxide_core::afloat`'s
    /// module docs. `None` includes every ordinary floating character: a body with no horizontal
    /// wish never opens a window at all, so a resting floater is `None` for as long as it floats.
    ///
    /// Level-triggered exactly like [`Self::hold`]: recomputed at the end of every stepped frame
    /// from that frame's own classification, so it clears the frame the body makes progress or the
    /// driver stops asking.
    ///
    /// # Published (#801) — but read [`Self::disclosures`], not this, if you are a publisher
    ///
    /// #800 shipped this as a controller-only signal reaching a throttled `tracing::info!` and
    /// nothing else, which served a log-reading operator and not the HTTP-driving agent the
    /// honesty invariant is about. #801 wired it through to `GET /v1/observe/debug` as
    /// `player.afloat_stall`, on a **seven**-file path — six of which [`Self::hold`] also travels,
    /// and one which #801's round-1 review had to find the hard way:
    ///
    /// 1. `src/app.rs` — the render thread's publisher, which takes BOTH disclosures in one call
    ///    ([`Self::disclosures`]) so neither can be written without the other, plus the
    ///    [`Self::clear_hold`] beside it on frames that render without stepping;
    /// 2. `crates/eqoxide-ipc/src/lib.rs` — `ControllerView::afloat_stall`;
    /// 3. `crates/eqoxide-net/src/action_loop.rs` — `ActionLoop::stream_position`, which mirrors the
    ///    view into `GameState` on the same tick as the position;
    /// 4. `crates/eqoxide-core/src/game_state.rs` — `GameState::player_afloat_stall`, plus the
    ///    `begin_zone_in` clear that stops a departed zone's claim surviving a zone load;
    /// 5. `crates/eqoxide-http/src/lib.rs` — `player.afloat_stall`, a view of its OWN, deliberately
    ///    not folded into `player.hold`;
    /// 6. `crates/eqoxide-http/src/observe.rs` — the `player.insert("afloat_stall", …)` in
    ///    `get_debug`. **This is the hop that actually reaches an agent, and the one #801 shipped
    ///    for review without.** `PlayerState` is an internal projection; no handler serialises it
    ///    whole, so a field can be correct in all five files above and still appear in no response
    ///    body anywhere. It did: the reviewer ran the release build against a live character and
    ///    found `player` carrying 55 keys, none of them this one;
    /// 7. `docs/http-api.md`.
    ///
    /// Two constraints carried over from #800 and still hold: an `AfloatStall` is not a
    /// `ControllerHold` — a hold says the body cannot move at all, this says only that *this wish*
    /// produced no motion — and the false-alarm direction is the one that matters. The seven
    /// false-alarm tests in this module's test block are unchanged by #801 and must not be weakened
    /// by the surface built on top of them.
    pub fn afloat_stall(&self) -> Option<AfloatStall> { self.afloat.stall() }

    /// Both level-triggered controller disclosures, in ONE call — the publisher's entry point.
    ///
    /// # Why this exists rather than two separate reads (#801)
    ///
    /// `app.rs`'s render-thread publisher needs to write both into `ControllerView` every rendered
    /// frame. Written as two independent statements, dropping one of them is a *silent* edit: the
    /// remaining field keeps its previous value, `stream_position` keeps mirroring it, and the API
    /// keeps answering — with a stale, confident value that nothing recomputes. That is the #343 /
    /// #792 shape, and it is exactly what a reviewer reading a diff is least likely to notice.
    ///
    /// Returning them as a tuple makes the omission awkward at this end; the other end is what makes
    /// it impossible. `eqoxide_ipc::ControllerView` keeps both fields PRIVATE and takes them only
    /// through `publish_disclosures((hold, stall))`, so a publisher in this crate cannot write one
    /// without naming the other — it is a compile error, not a code-review catch. That was measured
    /// on #801: with the fields public, replacing the paired write with a lone `v.hold = …` left the
    /// whole workspace green, because `app.rs`'s frame loop needs a GPU and a window and no unit test
    /// can reach that statement. See `ControllerView::publish_disclosures` for the transcript.
    ///
    /// **What this does NOT do:** it does not prove the publisher is *called*, and it does not stop
    /// a caller passing a deliberate `None`. Nothing here reaches into `app.rs`'s event loop. It
    /// removes one specific failure — publishing one disclosure and silently forgetting the other —
    /// and no other.
    pub fn disclosures(&self) -> (Option<ControllerHold>, Option<AfloatStall>) {
        (self.hold(), self.afloat_stall())
    }

    /// Drop any hold WITHOUT stepping (`app.rs` calls this on the frames it does not step the
    /// controller — no collision loaded, i.e. mid zone-load). The last hold described geometry that
    /// has been dropped, and nothing is going to recompute it until the new zone lands, so the
    /// honest published value is "not holding" rather than a stale alarm about a zone we have left.
    ///
    /// **This call site is load-bearing and must not be deleted.** It is the ONLY thing making the
    /// published "nothing here latches" true on frames that do not step — `step`'s `take()` argument
    /// reaches stepping frames only. Round-3 review MEASURED it unpinned (deleting it, with
    /// `GameState::begin_zone_in`'s `player_hold = None`, left the whole workspace green). Both are
    /// pinned now: `the_frames_that_do_not_step_still_clear_the_hold` (a source scan of `app.rs`'s
    /// not-stepped arm) and `clear_hold_drops_a_hold_without_stepping` in this module's tests, and
    /// `begin_zone_in_clears_the_previous_zones_hold_724` in `eqoxide-core`.
    ///
    /// #776: this drops the afloat no-progress window too, for the identical reason — the window
    /// describes a body failing to cross specific geometry, and on a frame with no collision loaded
    /// there is no such geometry. Doing it here rather than at a new call site is what keeps #801's
    /// publication of [`Self::afloat_stall`] correct with NO change to `app.rs`'s not-stepped arm —
    /// and it means `the_frames_that_do_not_step_still_clear_the_hold`, which scans that arm for
    /// this exact call, pins BOTH disclosures against the stale-across-a-zone-load failure.
    pub fn clear_hold(&mut self) { self.hold = None; self.afloat = AfloatStallClock::default(); }

    /// Record — and, throttled, log — that a recovery branch has stopped the body this frame with
    /// nothing to restore it onto.
    ///
    /// `prev` is the hold taken at the top of this `step`, which is how `secs` accumulates without
    /// the field ever being able to survive the condition ending: the ONLY way a hold persists is
    /// for the same branch to re-set it on the very next frame. A change of reason restarts both the
    /// clock and the log throttle, so the transition is always disclosed.
    fn enter_hold(&mut self, reason: ControllerHoldReason, dt: f32, prev: Option<ControllerHold>) {
        let secs = match prev {
            Some(p) if p.reason == reason => p.secs + dt,
            // New hold (or a different one): restart the clock and re-arm the log immediately.
            _ => { self.hold_log_cooldown = 0.0; dt }
        };
        self.hold = Some(ControllerHold { reason, secs });
        if self.hold_log_cooldown <= 0.0 {
            self.hold_log_cooldown = HOLD_LOG_SECS;
            match reason {
                // ⚠️ #845/#920 review B2: this line used to open "embedded at {pos}". It is the
                // channel #845 was reported through, and it was asserting the half of the
                // disjunction the reported case was NOT in — `is_embedded` is
                // `!footprint_clear(..) || ground_below(..).is_none()`, and the live casualty was
                // the void disjunct (zero triangles over the column). It states the disjunction now,
                // like the published `detail` string already did.
                ControllerHoldReason::EmbeddedNoRecovery => tracing::info!(
                    "controller HOLD [embedded_no_recovery]: cannot place the body at {:?} for \
                     {:.1}s — it is EITHER pierced by geometry OR standing over a void with no \
                     floor within {:.0}u below its feet (the test is a disjunction; this line \
                     cannot tell you which, and #845's live casualty was the void half). Push-out \
                     found nowhere to go, there is no recovery history to fall back to, and the \
                     zone-wide last-resort search found nowhere either — the body is FROZEN (every \
                     step is skipped) until something relocates it. Published as player.hold; this \
                     line is throttled to one per {:.0}s while it lasts.",
                    self.pos, secs, GROUND_DEPTH, HOLD_LOG_SECS),
                ControllerHoldReason::UnderworldNoRecovery => tracing::info!(
                    "controller HOLD [underworld_no_recovery]: blocked descent below underworld \
                     {:.1} → holding at {:?} for {:.1}s (no recovery history to restore; the body \
                     is not grounded). Published as player.hold; this line is throttled to one per \
                     {:.0}s while it lasts.",
                    self.underworld, self.pos, secs, HOLD_LOG_SECS),
            }
        }
    }

    /// Hard-set the position (zone-in, teleport, large server correction). Clears velocity & stuck.
    pub fn teleport(&mut self, pos: [f32; 3]) {
        // #724: a position discontinuity SUPERSEDES the recovery history. Every sample in the ring
        // describes where the body was before this write, so any recovery that restores one undoes
        // the relocation the server just performed — silently, with a perfectly plausible in-zone
        // coordinate. Both recovery paths read the ring (the #150 fall-through guard and the
        // depenetration stuck fallback) and both are reachable in the window; see the two example
        // tests and the sweep at the bottom of this file.
        //
        // Cost, stated plainly: for as long as it takes the body to become grounded again after a
        // relocation, neither recovery path has anything to restore, so a body that lands somewhere
        // unrecoverable HOLDS rather than rubber-bands. The restore is a wrong answer the client
        // reports as success, and #712's live record is that it "wedged permanently" — so the hold
        // is the better of the two. (#712's record carries NO cadence — it is the stale
        // PREVIOUS-zone recovery and the permanent wedge. 0.5 s is `GOOD_SAMPLE_SECS`, this file's
        // ring-BANKING interval, not a server re-fire rate; a draft here once promoted it into one.)
        // The hold is NOT free:
        //
        //   ⚠️ RETRACTED (#724 round-2 review, B1). "Holding is a visible failure a further server
        //   correction can fix" was the old justification. Both halves were
        //   wrong on the depenetration path, and measured wrong. (a) NOT VISIBLE: `depenetrate`'s
        //   only `tracing::info!` sat inside `if let Some(&g) = self.good.back()`, so with the ring
        //   cleared — which this very fix makes the normal post-relocation state — the branch
        //   emitted nothing, changed nothing and returned `true` every frame; re-measured on this
        //   module's own stuck fixture, post-fix: `pos=[40.0, 40.0, 0.0] stuck_time=2.0
        //   on_ground=false`, 0 of 40 frames moved the body, and `hold_log_cooldown` was still 0 —
        //   i.e. not even the underworld hold log had ever armed. Zero output for the whole episode,
        //   and no agent-visible field said anything either. (b) NO AUTOMATIC SECOND CORRECTION: the
        //   client goes on streaming its own unchanged position and the server agrees with it, so
        //   nothing generates one; the wedge clears only if a human or a GM acts.
        //
        // The trade stands, but it is paid for by DISCLOSURE, not by an imaginary rescue: both
        // recovery paths now record a `ControllerHold` (see `enter_hold`), which is logged on a
        // throttle and published to agents as `player.hold` on `GET /v1/observe/debug` (#817 —
        // `GET /v1/observe` is not a registered route). A hold is a reported failure, which is what
        // makes it better than a silent wrong answer.
        //
        // Small corrections do not pay any of this: under `CORRECTION_SQ` (12 u, 2D) the net never
        // calls `teleport` at all.
        self.forget_recovery_history();
        // A discontinuity supersedes the hold too: whatever predicament the body was in, it is not
        // in it at this new position. `step` recomputes from scratch next frame.
        self.hold = None;
        self.pos = pos;
        self.vel_z = 0.0;
        self.on_ground = false;
        self.stuck_time = 0.0;
        // #845: a relocation is a NEW predicament, so it must not inherit the tail of the previous
        // one's last-resort-search cooldown. (Cost only: `rescue_cooldown` gates how often a FAILED
        // ~500-probe search is retried; it can never make a rescue happen that would not otherwise.)
        self.rescue_cooldown = 0.0;
        // A teleport / large server correction is a position discontinuity, NOT a fall: drop any
        // airborne tracking and any not-yet-consumed landing so a correction is never misread as a
        // fall landing (§442 hazard 2b — `app.rs` calls this from the `pos_correction` handler).
        self.airborne_start_z = None;
        self.landed_fall_height = None;
        self.swim_sinking = false; // #444: a teleport isn't a swim-down exit either
        // #776: a position discontinuity supersedes the afloat window as well. The anchor describes
        // a point THIS body failed to get away from; after a relocation it is a point the body is no
        // longer at, and carrying the accumulated seconds across would make the first frames at the
        // new position inherit an alarm they did not earn.
        self.afloat = AfloatStallClock::default();
    }

    /// Advance one frame. Returns the new authoritative position.
    pub fn step(&mut self, intent: MoveIntent, dt: f32, col: &Collision) -> [f32; 3] {
        self.hold_log_cooldown = (self.hold_log_cooldown - dt).max(0.0);
        // #845: the failed-search retry throttle (cost only — see `rescue_cooldown`).
        self.rescue_cooldown = (self.rescue_cooldown - dt).max(0.0);
        // #724 review B1 — THE CLEAR PATH, and the whole reason this is a `take` and not a read.
        // The hold is dropped here, unconditionally, before anything can look at it. The only code
        // that can put one back is a branch that is actively holding the body on THIS frame (there
        // are exactly two, both routed through `enter_hold`). So a hold cannot outlive its cause:
        // the frame the push-out succeeds, or the fall finds a floor, or a correction relocates the
        // body, nothing re-sets it and `hold()` reads `None` again. `prev` carries the previous
        // frame's value forward only so `enter_hold` can accumulate `secs` and decide whether the
        // reason changed — an observable with no clear-path is its own honesty bug (#343/#679).
        let prev_hold = self.hold.take();
        // Depenetration / unstuck net runs first (§3.3). If it handled an embedded frame, freeze
        // the rest of the step so we neither slide deeper nor fall through void.
        self.afloat_log_cooldown = (self.afloat_log_cooldown - dt).max(0.0);
        if self.depenetrate(dt, col, prev_hold) {
            // #776: reaching here means the net HANDLED the frame, and the net's door (see
            // `depenetrate`) hands every wet body straight back to physics — so a frame the net
            // handled is a frame with a DRY body, by construction, not by coincidence. `NotAfloat`
            // is therefore the true classification, not a convenient default, and this closes the
            // window rather than leaving it to drift across the frames the net owns.
            self.afloat.observe(AfloatFrame::NotAfloat, self.pos, dt);
            return self.pos;
        }
        // §444: remember whether we were in water AND actively swim-sinking LAST frame, before
        // either is overwritten below, so the gravity branch can detect a genuine swim-down-and-exit
        // (see the re-arm below). `swim_sinking` defaults false again this frame; only the swim_sink
        // call further down sets it true.
        let was_in_water = self.in_water;
        let was_swim_sinking = self.swim_sinking;
        self.swim_sinking = false;

        // Is the character in water? Probe the BODY, not just the origin (#329).
        //
        // `self.pos` is the character's FEET. A character standing on the bottom of a pool can have
        // its feet a hair BELOW the water region's lower bound while its whole body is submerged —
        // the water volume is baked from the `.wtr` BSP and does not have to meet the floor exactly.
        // The qcat spawn shaft is precisely this: the floor is at z=-69.97 and the water spans
        // -69.5 … -43.0, so a character standing there is under 26 UNITS of water while a feet-only
        // probe reports it bone dry. Everything downstream then goes wrong at once — `swimming` is
        // false, `submerged_on_floor` is false, buoyancy never fires, and the character is pinned to
        // the shaft floor for ever. That is the qcat spawn pocket: a level-1 character could not
        // swim up and out of the water it was standing in, so it could never leave the zone.
        //
        // Probe the feet first (so wading is unchanged), then chest height. `water_at` is then used
        // for every water query in this step, so the surface we float toward is the one above the
        // BODY rather than one that doesn't exist at the feet. The probe itself is the module-level
        // [`water_probe`] so the depenetration net asks the SAME question (#649) — it used to be
        // inlined here, and the net's water-blindness is exactly what that private copy allowed.
        let water_at = water_probe(col, self.pos);
        self.in_water = col.in_water(water_at);
        let swimming = intent.want_swim && self.in_water;
        if self.hop_cooldown > 0.0 { self.hop_cooldown = (self.hop_cooldown - dt).max(0.0); }

        // ── Horizontal: collide-and-slide, with step-up when blocked on the ground. ──
        let throttle = (intent.wish_dir[0] * intent.wish_dir[0] + intent.wish_dir[1] * intent.wish_dir[1]).sqrt();
        if throttle > 1e-4 {
            let wish = [
                intent.wish_dir[0] / throttle * intent.speed * dt,
                intent.wish_dir[1] / throttle * intent.speed * dt,
                0.0,
            ];
            let (low_pos, low_hit) = self.slide(self.pos, wish, col);
            let low_prog = hlen([low_pos[0] - self.pos[0], low_pos[1] - self.pos[1], 0.0]);
            let mut applied = [low_pos[0], low_pos[1], self.pos[2]];
            let mut stepped = false;
            // Step-up is the native 2u for BOTH free WASD and nav (#239): nav must not be able to
            // climb anything a WASD player can't. Fence/cart lips taller than 2u are crossed the way
            // a real player does — via `hop`, below — not climbed. (`intent.climb` no longer raises
            // this; it used to carry the super-human NAV_CLIMB=20.)
            let _ = intent.climb;
            let max_step = STEP_UP;
            // Allow step-up while SWIMMING too, not just when grounded: that's how a character hauls
            // OUT of water onto the shore (swimming clears on_ground, so without this it just presses
            // into the bank lip at the surface and can't climb the last few units, #191).
            let mut ducked = false;
            if (self.on_ground || swimming) && low_hit && low_prog + 0.01 < hlen(wish) {
                // #661: a blocked SWIMMER first tries to pass UNDER the obstruction — the exact
                // mirror of the step-up below, pointing down.
                //
                // The ordering is NOT a reversibility argument. Two earlier versions of this
                // comment justified it with universals ("a swimmer that dives can always surface
                // again"; "a dry haul-out is irreversible under every driver") and review
                // measurement falsified BOTH: a duck could be one-way over a shallow shelf or a
                // higher far surface (both now refused by `try_duck_under`'s two re-divability
                // bounds), and a hauled-out body can walk back off the very lip it climbed. The
                // honest, measured basis for the ordering is narrower:
                //
                //   * at every measured legitimate bank the duck refuses ITSELF (a face that is
                //     solid to the bottom gives a dive no extra progress), so trying it first
                //     costs the haul-out nothing — pinned by
                //     `a_swimmer_at_a_solid_bank_still_hauls_out_the_duck_does_not_override_191`
                //     and walker_sim's P1 sweep;
                //   * where both moves genuinely pass, the duck keeps the body in the medium its
                //     current driver is steering it through, and an admitted duck is round-trip-
                //     capable by construction (the bounds in `try_duck_under`), while a haul-out
                //     changes medium and hands the vertical to gravity.
                //
                // Gated on `wish_vspeed <= 0`: an explicit upward swim wish is the walker's
                // haul-out drive (water design §4c) and must never be countermanded by an
                // autonomous dive (`an_upward_haul_out_drive_is_never_countermanded_by_the_duck`
                // goes RED if this gate is deleted).
                if swimming && intent.wish_vspeed <= 0.0 {
                    if let Some(duck) = self.try_duck_under(wish, col) {
                        if hlen([duck[0] - self.pos[0], duck[1] - self.pos[1], 0.0]) > low_prog + 0.05 {
                            applied = duck;
                            ducked = true;
                        }
                    }
                }
                if !ducked {
                    if let Some(step) = self.try_step_up(wish, max_step, col) {
                        if hlen([step[0] - self.pos[0], step[1] - self.pos[1], 0.0]) > low_prog + 0.05 {
                            applied = step;
                            stepped = true;
                        }
                    }
                }
                // Step-up couldn't cross it. If nav allows, and we're wedged ~head-on (not sliding
                // along a wall) against a thin barrier with walkable floor just beyond, hop over it
                // (a fence has flat floor both sides, so there's nothing to step UP onto). The
                // airborne collide-and-slide below carries us forward over the rail (#41).
                if !stepped && !ducked
                    && intent.hop
                    && self.hop_cooldown <= 0.0
                    && self.can_hop(wish, col)
                {
                    self.vel_z = NAV_HOP_VELOCITY;
                    self.on_ground = false;
                    self.airborne_start_z = Some(self.pos[2]); // §442: a hop begins an airborne stretch
                    self.hop_cooldown = HOP_COOLDOWN;
                }
            }
            self.pos[0] = applied[0];
            self.pos[1] = applied[1];
            if stepped {
                self.pos[2] = applied[2];
                self.vel_z = 0.0;
                self.on_ground = true;
            } else if ducked {
                // The dive half of the crossing: feet dropped to the duck depth, still in water.
                // The body is mid-water by construction (`try_duck_under` requires the lowered
                // start AND the destination to be in water), so support state is the swim
                // branch's: not grounded, buoyancy owns the vertical from here.
                self.pos[2] = applied[2];
                self.vel_z = 0.0;
                self.on_ground = false;
            }
        }

        // A character that nav-pathed DOWN to a pool floor becomes on_ground on the bottom; the
        // passive-buoyancy branch below only fired while airborne, so it used to sit there
        // submerged forever. Treat "on the floor but well below the water surface" as submerged so
        // it floats back up (a body resting underwater is still buoyant). (eqoxide#197)
        let float_depth = crate::traversability::PLAYER_BODY.float_depth;
        let submerged_on_floor = self.in_water && !swimming
            && col.water_surface(water_at).is_some_and(|surf| self.pos[2] < surf - float_depth);

        // ── Vertical: swim / buoyancy / jump / gravity + ground clamp. ──
        if swimming {
            self.on_ground = false;
            self.vel_z = 0.0;
            // §442 (#442) DEFECT-1: water BREAKS a fall — the airborne episode is over the moment the
            // body is in water. Drop any tracked airborne start so a later dry-ground step-out cannot
            // latch a stale phantom fall height (fall off a cliff into a lake → swim to shore → NO
            // fall damage). Matches the old `drive_controlled_fall` water-landing guard + WASD (no dmg).
            self.airborne_start_z = None;
            if intent.wish_vspeed != 0.0 {
                // Explicit vertical swim input (the nav swim-up drive, or a human swimming along
                // the look direction). COLLIDED (#359, second mechanism): this used to be a raw
                // `pos[2] +=` write — no sweep, no ceiling clamp — so in water flush with a ceiling
                // (the qcat spawn corridor) the rise embedded the character in rock and the
                // depenetration net slammed it back to the last good GROUNDED spot, the shaft
                // floor: rising CAUSED the very strand it was meant to fix. An upward wish is also
                // clamped at the water surface — the feet never leave the water column mid-swim;
                // a haul-out lip is mounted by the swimming step-up above, not by flying out of
                // the pool.
                //
                // The clamp stops `SKIN` UNDER the surface, not exactly ON it (#661 review round,
                // found by pinning the duck's up-wish gate): `in_water` is a strict inequality, so
                // feet clamped to exactly `surf` read as DRY for the frame — the body probe sees
                // feet-at-boundary + chest-in-air and calls the whole body dry — and the DRY
                // depenetration net then owns a body that is actually swimming at the surface.
                // The mechanism is confirmed on `main` too (review round 2: feet reach z = 3.7e-7,
                // body reads dry, the dry net takes it) — pre-existing, not introduced here. The
                // magnitude is geometry-dependent: in the
                // `an_upward_haul_out_drive_is_never_countermanded_by_the_duck` scene (lintel at
                // east 4, pool floor −40, `water_slab(-40, 0)`, up-wish 5), the clamp-less frame
                // trace measured `f28 z=0.0 wet=0` → `f29 pos=(2.81, 0.38, -40.0) Grounded` — a
                // one-frame `nearest_floor` teleport to the pool bottom, because no nearer floor
                // exists in any candidate column there; in shallow-slot scenes the net shoves
                // laterally instead. One `SKIN` of depth keeps the clamped swimmer in its own
                // medium, so the sentence above about the water column stays true in the probe's
                // own terms; that scene's test pins the clamp by name.
                let want = intent.wish_vspeed * dt;
                if want > 0.0 {
                    let mut rise = self.swim_rise(want, col);
                    if let Some(surf) = col.water_surface(water_at) {
                        rise = rise.min((surf - SKIN - self.pos[2]).max(0.0));
                    }
                    self.pos[2] += rise;
                } else {
                    // #444: track swim_sinking off the ACTUAL descent, not the down-INTENT. A
                    // down-wish is common while merely swimming FORWARD looking slightly down (the
                    // WASD `wish_vspeed = dz*speed` couples pitch into vertical), so keying the
                    // water-exit re-arm off `want < 0` alone false-positives for a character resting
                    // on / near a submerged floor that drifts LATERALLY out of the water region —
                    // `swim_sink` clamps to ~0 against the floor, yet the sign said "sinking". Only a
                    // genuinely negative resolved delta (moved down past the SKIN clamp, i.e. real
                    // open water below the feet) counts, so the §442 DEFECT-1 invariant still holds
                    // for a sideways water-exit.
                    let sink = self.swim_sink(want, col);
                    self.pos[2] += sink;
                    if sink < -SKIN { self.swim_sinking = true; }
                }
            } else if let Some(surf) = col.water_surface(water_at) {
                // Nav-driven swim with no vertical wish: float toward the swim plane
                // (`surface − float_depth`, from the shared Body) so the character swims ACROSS at
                // the top instead of sitting on / crawling along the pool bottom the path may
                // route to (#191). Without this, want_swim just froze it at its current z.
                let target = surf - float_depth;
                if self.pos[2] < target {
                    let want = (BUOY_RATE * dt).min(target - self.pos[2]);
                    self.pos[2] += self.swim_rise(want, col);
                }
            }
        } else if self.in_water && (!self.on_ground || submerged_on_floor) {
            // Submerged but NOT actively swimming (walked / nav-pathed into water, incl. resting on
            // the pool bottom): float toward the surface instead of applying gravity and free-falling
            // through the passable water plane to the riverbed — or, in open deep water with no
            // bottom, to the zone boundary (#172) — or sitting on the pool floor (#197).
            // Rise-only: buoyancy never accelerates the character downward.
            // Detach from the floor so buoyancy owns the vertical (we only get here on_ground when
            // submerged_on_floor, i.e. genuinely below the surface and about to rise).
            self.on_ground = false;
            self.vel_z = 0.0;
            // §442 (#442) DEFECT-1: water broke the fall — end the airborne episode (see the swim
            // branch). Without this, a cliff-drop into water then a walk onto shore latches a
            // lethal phantom `landed_fall_height` (the stale pre-water start minus the shore z).
            self.airborne_start_z = None;
            if let Some(surf) = col.water_surface(water_at) {
                let target = surf - float_depth;
                if self.pos[2] < target {
                    let want = (BUOY_RATE * dt).min(target - self.pos[2]);
                    self.pos[2] += self.swim_rise(want, col);
                }
                // At/above the float line: hold — don't sink (no gravity while submerged).
            }
            // No bounded surface found: hold altitude rather than free-fall, because free-falling
            // inside a water volume we cannot measure would drive the body down onto the #150
            // underworld guard for no reason.
            //
            // The stated reason names only OUR code, deliberately. An earlier form added "a server
            // correction … would otherwise have to recover us": whether a free-falling swimmer
            // sinking past the world draws a server-side relocation is SERVER behaviour, unmeasured
            // here — not established false, just not established. Naming an unmeasured server rescue
            // as a known consequence is the habit #724 exists to break (see
            // `forget_recovery_history` and `teleport`).
            //
            // Residual, labelled: the retained "would drive the body down onto the #150 underworld
            // guard" is REASONED FROM THE BRANCH STRUCTURE, not captured in a run. It is a claim
            // about code in this repository, which is why it survives and the server half does not.
            //
            // This is NOT a `ControllerHold`, and #724 round-2 review (N4) was right to ask why not.
            // The reason is that it is not distinguishable from correct behaviour: the branch three
            // lines above — a swimmer at or above its float line — also holds altitude and also
            // leaves `on_ground` false. Neutral buoyancy IS what a swimmer does. Reporting this
            // shape would raise `player.hold` on every ordinary floating character, which is a false
            // alarm, and a false alarm in an honesty observable is the same defect as a silence.
            // The state is also not a predicament: lateral movement and swim input work normally and
            // leaving the volume resumes ordinary physics, whereas both real hold shapes are states
            // the body cannot leave under any driver. If this is ever made reportable it needs its
            // own reason and a way to tell it apart from an ordinary float — not this branch.
        } else if self.levitating {
            // §529: Levitate — gravity OFF. The self-player HOVERS at altitude and free-floats over
            // land, gaps, and water instead of being pulled down. A floor that has risen to/above the
            // feet (walking UP a slope) still lifts us so we don't clip into terrain; a floor that
            // dropped away, or a gap/void with no floor below, is glided OVER at height, never fallen.
            // Hovering is not a fall — clear any airborne tracking so no phantom fall damage latches
            // when the buff fades. Jump / vertical input is ignored (native levitate has no vertical
            // control). Reconciliation is unaffected: our hover Z is truthful and the server (which set
            // the buff) agrees, and the nav streamer's correction keys off HORIZONTAL delta only.
            self.vel_z = 0.0;
            self.airborne_start_z = None;
            let foot = self.pos[2];
            let floor = col.ground_below(self.pos[0], self.pos[1], foot + GROUND_ORIGIN, GROUND_DEPTH);
            match floor {
                // RISING floor ONLY (walking UP a slope/ramp): stand on it so we rise with the
                // terrain instead of clipping through it. Strictly `f > foot` — a floor at OR BELOW
                // the feet must NOT snap us down. This is the #587 fix: the old `f >= foot -
                // GROUND_SNAP_TOL` reused the 0.5u DOWNWARD ground-snap allowance, so on any gentle
                // downslope (per-frame descent < 0.5u — i.e. essentially all walkable terrain at run
                // speed) it found the floor just below the feet and snapped the feet DOWN to it every
                // frame. The levitator then tracked the hill down, indistinguishable from walking; only
                // a >0.5u/frame cliff or a true no-floor gap ever "held altitude".
                Some(f) if f > foot => { self.pos[2] = f; self.on_ground = true; }
                // Floor at/below the feet, or none at all (downslope, drop, gap, void): HOLD altitude —
                // hover. The levitator floats out over the descent and only lands when levitate ends.
                _ => { self.on_ground = false; }
            }
        } else {
            // §444: exiting a suspended water volume through its BOTTOM via a deliberate swim_sink
            // (a floating slab over an open pit — pathological but real `.wtr` geometry) resumes a
            // normal fall, but the swim branch unconditionally clears `airborne_start_z` (water
            // breaks a fall, §442 DEFECT-1), and this branch otherwise only re-arms it from
            // jump/floor-drop-away while GROUNDED. A water exit lands here already `on_ground =
            // false` (set by the swim branch), so neither of those two arms would ever fire: the
            // drop to the real floor below was silently untracked (no landing height, no
            // fall-damage signal — the character would still read as swimming while actually
            // plummeting). Re-arm a fresh airborne start at the water-exit z the first frame water
            // reads false while still airborne.
            //
            // Gated on `was_swim_sinking` (not just `was_in_water`), so this does NOT reopen §442
            // DEFECT-1: a character that was merely floating (passive buoyancy, never sank below the
            // water's own bottom) and then has its water disappear out from under it — e.g. crossing
            // into shore geometry with no water map, `water_breaks_a_fall_no_phantom_damage_on_shore`
            // — must still get NO phantom fall height. Only an ACTIVE downward swim through the
            // volume's floor counts as a genuine new airborne stretch.
            if was_in_water && was_swim_sinking && !self.in_water && !self.on_ground
                && self.airborne_start_z.is_none()
            {
                self.airborne_start_z = Some(self.pos[2]);
            }
            if intent.jump && self.on_ground {
                self.vel_z = JUMP_VELOCITY;
                self.on_ground = false;
                self.airborne_start_z = Some(self.pos[2]); // §442: a jump begins an airborne stretch
            }
            let foot = self.pos[2];
            let floor = col.ground_below(self.pos[0], self.pos[1], foot + GROUND_ORIGIN, GROUND_DEPTH);
            if self.on_ground {
                match floor {
                    Some(f) if (f - foot).abs() <= GROUND_SNAP_TOL || f > foot => self.pos[2] = f,
                    _ => {
                        // Floor dropped away / vanished → a genuine airborne stretch begins. Record
                        // where we left the ground so the landing can report the fall height (§442).
                        self.on_ground = false;
                        self.airborne_start_z = Some(self.pos[2]);
                    }
                }
            }
            if !self.on_ground {
                self.vel_z = (self.vel_z - GRAVITY * dt).max(-MAX_FALL);
                let cand = self.pos[2] + self.vel_z * dt;
                // Never descend to/below the zone's underworld floor. A collision gap can otherwise
                // drop us onto deep below-world boundary geometry (or the void) below `underworld`,
                // which the server treats as fallen-through-the-world → ZoneToBindPoint, then CLE
                // linkdead. Recover to the last good grounded position instead; if we have none yet,
                // just stop sinking and hold above the underworld. (#150)
                //
                // NO server correction sorts this out (#724, the branch labelled
                // `UnderworldNoRecovery`): the held body goes on streaming its own unchanged
                // position, the server agrees with it, and so nothing generates a further
                // correction. The hold is terminal until a GM `#goto`/`#summon` moves the character
                // or it zones out — which is why the branch below reports a `ControllerHold` instead
                // of relying on a rescue that was never coming.
                let landing_valid = |f: f32| cand <= f && f > self.underworld;
                match floor {
                    Some(f) if landing_valid(f) => {
                        self.pos[2] = f; self.vel_z = 0.0; self.on_ground = true;
                        // §442: a genuine landing. If we tracked an airborne start (i.e. it was not
                        // cleared by a teleport / depenetration / underworld recovery), latch a
                        // one-shot fall height for the nav thread to apply driver-agnostic damage.
                        if let Some(start) = self.airborne_start_z.take() {
                            self.landed_fall_height = Some((start - f).max(0.0));
                        }
                    }
                    _ if cand <= self.underworld => {
                        // NOTE (#724 review B2): the recovering arm re-grounds the body ON the
                        // restored sample. Pre-#724 that is what made the stale-ring window
                        // unbounded — the ring then re-banks copies of the stale point, 4 → 6 → 8 —
                        // and it is the mechanism the retracted "never banks again" claim in
                        // `forget_recovery_history` got wrong.
                        // #661 (issue's "second un-`Recovery`'d writer" note): routed through
                        // `recover` so the fall-through guard shares the net's single
                        // position+support writer instead of re-stating the flags inline. Ring
                        // samples are banked only while grounded and non-embedded — since #661's
                        // review (B3) that is enforced by the explicit `!is_embedded` predicate at
                        // the banking site, not by the control flow's shape (the widened wet door
                        // briefly made the old shape-argument false, measured) — so
                        // `Recovery::Grounded` holds by construction, same as the stuck fallback
                        // in `depenetrate`. Behaviour-identical routing: `recover` additionally
                        // zeroes `stuck_time`, but this arm only runs on frames `depenetrate`
                        // returned false, which already reset it.
                        // ⚠️ NOT AMENDED BY #845, deliberately — see `last_resort_placement`. This
                        // arm looks like `depenetrate`'s dead end and is NOT one: it runs AFTER
                        // collide-and-slide, so the driver's lateral input has already reached the
                        // body this frame and `UnderworldNoRecovery` is not absorbing. A held body
                        // here can be walked out under its own client-API power, which is precisely
                        // the exit #845 is about. Widening the zone search to cover this arm as
                        // well was tried and reverted: it would teleport bodies the #724 guard is
                        // deliberately holding where the SERVER put them, and three tests pin that
                        // intent (`a_body_held_above_the_underworld_with_no_recovery_history_says_so_too`,
                        // `a_large_same_zone_relocation_forgets_the_pre_relocation_recovery_ring`,
                        // and the `fell_through` half of
                        // `no_recovery_ever_restores_a_position_a_relocation_superseded`). Overturning
                        // #724's rationale needs its own evidence, not a side effect of this fix.
                        let recovered = match self.good.back().copied() {
                            Some(g) => { self.recover(g[0], g[1], Recovery::Grounded(g[2])); true }
                            None => false, // hold current pos; don't sink below underworld
                        };
                        self.vel_z = 0.0;
                        self.airborne_start_z = None; // underworld recovery is not a fall landing (§442)
                        // The no-history branch changes NOTHING — it holds `pos`, leaves `on_ground`
                        // false, and so re-runs every frame for as long as the hold lasts. Left
                        // unthrottled it emits at frame rate for ever, which #720 review flagged
                        // once clearing the ring on a zone change made an empty ring routine. The
                        // recovering branch stays unthrottled: it moves the body, so each of its
                        // lines reports a distinct event.
                        if recovered {
                            tracing::info!(
                                "fall-through guard: blocked descent below underworld {:.1} → {:?}",
                                self.underworld, self.pos);
                        } else {
                            self.enter_hold(ControllerHoldReason::UnderworldNoRecovery, dt, prev_hold);
                        }
                    }
                    _ => self.pos[2] = cand,
                }
            }
        }
        // ── #776: the afloat no-progress window ──────────────────────────────────────────────────
        //
        // Folded in at the END of the frame, from the frame's own resolved facts: `in_water` and
        // `on_ground` as every branch above left them, `throttle` and `intent.speed` as the
        // collide-and-slide itself used them (so the classification cannot disagree with the motion
        // about whether a wish was made), and `self.pos` as the frame resolved it. See the block
        // comment above `AfloatStall` for why this is a separate signal from `ControllerHold`, why
        // the WISH half is horizontal, and why the PROGRESS half is not.
        self.afloat.observe(
            AfloatFrame::classify(self.in_water && !self.on_ground, throttle, intent.speed),
            self.pos, dt);
        if let Some(stall) = self.afloat.stall() {
            if self.afloat_log_cooldown <= 0.0 {
                self.afloat_log_cooldown = AFLOAT_STALL_LOG_SECS;
                tracing::info!(
                    "controller AFLOAT STALL: afloat at {:?} with a horizontal wish for {:.1}s and \
                     still within {:.2}u of {:?} IN ANY DIRECTION — the drive is being honoured and \
                     producing no progress (a sealed pocket, or a passage the duck-under refuses). \
                     This is NOT a freeze: a different wish — notably an explicit down-wish dive — \
                     may still cross, and a body that IS descending or rising is not reported here \
                     at all. This line is throttled to one per {:.0}s while it lasts.",
                    self.pos, stall.secs(), AFLOAT_PROGRESS, stall.anchor(), AFLOAT_STALL_LOG_SECS);
            }
        }
        self.pos
    }

    /// Iterative collide-and-slide of a horizontal `delta` from `from`. Returns the resolved
    /// position and whether any surface was hit. (Design §3.1.)
    ///
    /// Uses the centre ray (at foot and chest heights) for the contact, then backs the cylinder
    /// centre off by `radius` measured along the hit normal — a penetration-free "ray + radius"
    /// capsule approximation.
    ///
    /// **#870 — THE RAY MUST LOOK AS FAR AS THE BACK-OFF IT WILL APPLY.** The ray used to be cast
    /// exactly `|remaining|` long while the resolution backed the body off by `radius/ndot + SKIN`
    /// (≥ 1.05 u). Those two lengths disagreeing is the whole defect: when a frame's step ENDED
    /// short of a face, the ray never reached it, `nearest_hit` reported nothing, and the body
    /// advanced its full step — coming to rest anywhere in `(0, radius + SKIN)` of a solid face,
    /// i.e. with its own collision cylinder overlapping that face. The comment that stood here
    /// ("grazing cases the thin centre ray slips past are caught next frame by the depenetration
    /// net") named that as the remedy, and the remedy is a TELEPORT: `is_embedded` tests the
    /// footprint ring at `Body::ring` = 3.0 with radius `PLAYER_RADIUS` = 1.0, so a body resting
    /// 0.75 u from any face at least `Body::ring` tall reads EMBEDDED and the net relocates it to a
    /// ring candidate up to `PUSHOUT_RADII` away. On `ce1d89f` (pre-#870) that teleport fired on
    /// alternate frames and dragged a grounded walker sideways along the wall it was merely leaning
    /// on: +99.7 u at a north half-extent of 100, +342.9 u at a half-extent of 1000, in the same
    /// 15 s (900-frame, dt 1/60) drive. (The "1000" READS #870's "2000 u one" as a full span — the
    /// original fixture was produced off-tree during #870's review and exists at no sha, so this is
    /// an interpretation, not a recovered measurement. It is immaterial: the figure is
    /// drive-bounded, so half-extents 1000, 2000 and 4000 are bit-identical here — see below.)
    ///
    /// **The drag is bounded by whichever is SMALLER, the wall's north half-extent or the drive —
    /// and only the +99.7 figure is extent-bounded.** RE-DERIVED here (#987 review round 2) by
    /// wrapping this fn's own look-ahead back off (`ray_len = len + back_off * 0.0`), which
    /// reproduces both historical figures at the precision they were recorded — 99.717377 against
    /// the reported 99.7174, and 342.867859 against 342.8679 — and then varies each axis separately:
    ///
    /// | north half-extent | 900 frames (15 s) | 1800 frames | 3600 frames |
    /// |---|---|---|---|
    /// | 100 | 99.717377 | 99.717377 | — |
    /// | 400 | — | 399.848816 | — |
    /// | 1000 | 342.867859 | 698.348450 | 999.893250 |
    /// | 2000 | 342.867859 | 698.348450 | — |
    /// | 4000 | 342.867859 | 698.348450 | — |
    ///
    /// So **+342.9 u is DRIVE-bounded**: quadrupling the half-extent at a fixed 900-frame drive
    /// leaves it bit-identical at 342.867859, while doubling the drive at half-extent 1000 moves it
    /// to 698.348450. The extent binds only where it is the smaller of the two — half-extent 100
    /// caps at 99.717377 no matter how long the drive runs, half-extent 400 at 399.848816, and
    /// half-extent 1000 does become the binding constraint once the drive reaches 3600 frames
    /// (999.893250). An earlier draft of this paragraph called BOTH figures wall-length-bounded;
    /// that is false for +342.9, and the pre-#932 text — which attributed +342.9 to "15 s of
    /// simulated time" — had this one right.
    ///
    /// Both figures are historical; neither reproduces on this branch, because the bug they
    /// measured is fixed. What IS pinned here, on THIS branch, at both half-extents: **0.0000 u**
    /// of drift (`a_grounded_walk_never_drifts_on_a_short_or_a_1000u_extent_wall`, #932). That
    /// test does go RED under exactly the wrap above — but the pin that fires FIRST is its
    /// per-frame `is_embedded` assert (measured: half-extent 100, frame 33), not its drift assert.
    ///
    /// The `lip >= 3.00` threshold in #870 is exactly `Body::ring`; below it the ring never sees
    /// the barrier at all.
    ///
    /// So the look-ahead is DERIVED from the back-off rather than being a second hand-tuned number:
    /// `radius + SKIN` is the back-off at `ndot = 1`, the head-on case, and `contact` is now scaled
    /// by the ray's own length instead of by `len`. A face inside the look-ahead but beyond the
    /// step resolves to `advance == len` (no behaviour change, the body simply completes its step)
    /// unless it is close enough that completing the step would penetrate — which is exactly the
    /// case that used to be handed to the net.
    ///
    /// **Residual, stated because it is real — and it obeys NO formula.** The ray is lengthened by
    /// the head-on `radius + SKIN`, not by the oblique `radius/ndot + SKIN` (which reaches 20 u at
    /// the `ndot` floor and is not a sane ray length), so head-on is the only approach with a
    /// clean clearance. MEASURED, driving a grounded body at a 6 u wall at eight approach angles,
    /// 40 phases each, 600 frames, 35 u/s at 60 Hz, **on a wall and floor of north half-extent
    /// 500** — the extent is part of the protocol, not a detail (#938; see below) — recording the
    /// closest perpendicular approach ever reached, where `ndot` is the cosine of the approach
    /// against the face normal:
    ///
    /// | `ndot` | 1.000 | 0.985 | 0.866 | 0.707 | 0.500 | 0.342 | 0.174 | 0.087 |
    /// |---|---|---|---|---|---|---|---|---|
    /// | this branch | 1.050 | 1.039 | 0.920 | 0.743 | 0.708 | 0.801 | 0.899 | 0.949 |
    /// | pre-#870 `ce1d89f` | 0.417 | 0.426 | 0.495 | 0.588 | 0.708 | 0.801 | 0.899 | 0.949 |
    ///
    /// (`ce1d89f` is the pre-#870 baseline for both tables in this file.)
    ///
    /// **#938 — the half-extent 500 is load-bearing, and this file's own `wall`/`floor` helpers do
    /// NOT produce this table.** They hard-code a north half-extent of 100, at which only FOUR of
    /// the eight columns above reproduce within the 0.01 tolerance (`ndot` 1.000, 0.985, 0.500 and
    /// 0.342). The other four read 0.5027, 0.5978, 2.2375 and 11.2202 at `ndot` 0.866, 0.707,
    /// 0.174 and 0.087 — the body sliding around the END of a too-short wall, which is a different
    /// phenomenon from an oblique contact residual, not a worse measurement of the same one. All
    /// of these figures were re-derived in the #987 round-2 review and are pinned, both extents
    /// and all eight columns, by `the_residual_clearance_table_needs_the_walls_lateral_extent_stated`.
    ///
    /// `is_embedded` still fires from `ndot` 0.866 down — those are the rows whose worst clearance
    /// is inside `PLAYER_RADIUS` = 1.0, and they are the rows with nonzero embedded frames on this
    /// branch. It does NOT fire on the two rows above them; read the table rather than this
    /// sentence for where the boundary is. Two things follow, and
    /// both contradict what an earlier draft of this paragraph asserted: there is no useful lower
    /// bound at all below head-on, and the residual is NOT monotonic in the approach angle — it is
    /// worst through the oblique middle and recovers toward parallel, so any claim of the form
    /// "degrades toward X as the approach turns parallel" has the shape wrong as well as the
    /// number. What the fix does buy is measured and one-directional: this branch is strictly
    /// better than `ce1d89f` at `ndot` >= 0.707 and exactly tied at 0.500 and below, where the
    /// lengthened ray changes neither the worst clearance nor the embedded-frame count. Closing
    /// the oblique case needs a swept-cylinder contact, not a centre ray — out of scope here.
    fn slide(&self, from: [f32; 3], delta: [f32; 3], col: &Collision) -> ([f32; 3], bool) {
        // The contact heights AND the radius come from the ONE shared body (#386, #378 Phase 2):
        // the chest ray here and the planner's top edge probe are the same `Body::chest` field, and
        // the back-off radius is `Body::radius` — the planner can never again clear a band this ray
        // collides with, nor plan to a clearance this back-off disagrees with.
        let body = &crate::traversability::PLAYER_BODY;
        let probes = body.contact_probes();
        let radius = body.radius;
        // #870: ONE expression, read by the ray length below and by the back-off in the resolution
        // arm. Editing either alone re-opens the gap the net was papering over.
        let back_off = radius + SKIN;
        let mut pos = from;
        let mut remaining = delta;
        let mut hit_any = false;
        for _ in 0..MAX_SLIDE_ITERS {
            let len = hlen(remaining);
            if len < 1e-5 { break; }
            let d_hat = [remaining[0] / len, remaining[1] / len];
            // #870: the step, PLUS the distance the resolution would back off by. See the doc above.
            let ray_len = len + back_off;
            // Nearest contact among the foot and chest centre rays.
            let mut best: Option<crate::nav::collision::Hit> = None;
            for &hz in &probes {
                let f = [pos[0], pos[1], pos[2] + hz];
                let to = [f[0] + d_hat[0] * ray_len, f[1] + d_hat[1] * ray_len, f[2]];
                if let Some((t, n)) = col.nearest_hit(f, to) {
                    if best.map_or(true, |b| t < b.t) { best = Some(crate::nav::collision::Hit { t, normal: n }); }
                }
            }
            match best {
                None => { pos[0] += remaining[0]; pos[1] += remaining[1]; break; }
                Some(hit) => {
                    hit_any = true;
                    // Distance into the plane along the motion (floored so grazing hits don't blow up).
                    let ndot = (-(d_hat[0] * hit.normal[0] + d_hat[1] * hit.normal[1])).max(0.05);
                    // #870: `hit.t` is a fraction of the RAY, which is now longer than the step.
                    let contact = hit.t * ray_len;
                    let advance = (contact - radius / ndot - SKIN).max(0.0);
                    // #870: the look-ahead lengthens the RAY, and must not lengthen the STEP.
                    // `contact <= ray_len = len + radius + SKIN` and `radius/ndot >= radius`
                    // (`ndot <= 1`), so `advance <= len` — the extra reach is spent entirely on the
                    // back-off. Asserted rather than clamped: a `.min(len)` here would be
                    // unreachable by that derivation, and an unreachable clamp hides the day the
                    // derivation stops holding instead of reporting it. Debug-only, so it is live
                    // under `cargo test` across every caller in the suite and free in release.
                    debug_assert!(advance <= len + 1e-4,
                        "#870: slide advanced {advance} past its own step {len} (contact {contact}, \
                         ray {ray_len}, ndot {ndot}) — the look-ahead has become extra travel");
                    pos[0] += d_hat[0] * advance;
                    pos[1] += d_hat[1] * advance;
                    // Slide the unused budget along the plane (horizontal; z owned by ground/gravity).
                    let budget = (len - advance).max(0.0);
                    let dd = d_hat[0] * hit.normal[0] + d_hat[1] * hit.normal[1];
                    let slide = [d_hat[0] - hit.normal[0] * dd, d_hat[1] - hit.normal[1] * dd];
                    remaining = [slide[0] * budget, slide[1] * budget, 0.0];
                }
            }
        }
        (pos, hit_any)
    }

    /// COLLIDED vertical swim ascent (#359, second mechanism): how much of an upward swim `want`
    /// (> 0) the zone geometry actually allows. Sweeps the BODY TOP (`pos + Body::height`) up
    /// through the rise and stops `SKIN` short of the first solid hit — the same ray discipline
    /// the horizontal `slide` uses — so neither buoyancy nor the nav swim-up drive can ever push
    /// a swimmer's head into a ceiling. Against a flush ceiling the rise settles just below it
    /// and holds (rise → 0), instead of embedding and triggering a depenetration slam-back.
    ///
    /// **#855 applies here too, by construction.** This is the same `nearest_hit` the descent uses,
    /// with the same `1e-3 × ray length` blind band before #855 and the same world-unit
    /// `collision::hit_accepted` after it, and — since round 2 deleted the descent's extra clamp —
    /// with no guard on either side that the other lacks. There is nothing asymmetric left to fix
    /// (round-1 review, finding 7). The direction of the failure differs: a missed hit here stops a
    /// swimmer's head short of nothing and lets it enter a ceiling, where the depenetration net
    /// recovers it; a missed hit going down loses the floor with nothing underneath to recover
    /// against, which is why #855 is a descent issue.
    fn swim_rise(&self, want: f32, col: &Collision) -> f32 {
        let top = self.pos[2] + crate::traversability::PLAYER_BODY.height;
        let from = [self.pos[0], self.pos[1], top];
        let to = [self.pos[0], self.pos[1], top + want];
        match col.nearest_hit(from, to) {
            Some((t, _)) => (t * want - SKIN).max(0.0),
            None => want,
        }
    }

    /// COLLIDED vertical swim descent: the downward mirror of [`Self::swim_rise`] — sweeps the
    /// FEET down through `want` (< 0) and stops `SKIN` short of the floor/geometry below, so an
    /// explicit swim-down can't drive the character through the pool bottom.
    ///
    /// **THE FLOOR FLOOR (#855).** This is the ONLY path by which a swim descent moves the body
    /// down (the down-wish branch of `step` and `try_duck_under` both route through here), so its
    /// postcondition is the whole controller's: *the returned delta never takes the feet below the
    /// nearest surface underneath them.* The sweep alone did not give that. `nearest_hit`'s
    /// acceptance window used to start at `t > 1e-3`, a blind band of `1e-3 × ray length` — here
    /// `1e-3 × |wish_vspeed| · dt`, i.e. `5.8e-4` world units at the 35 u/s swim cap and 60 Hz,
    /// shrinking with `dt` (`1.8e-3` u at 20 Hz) — and a swimmer whose feet sat inside it got
    /// `None` back, read it as open water, and descended THROUGH the floor (#855, reached
    /// naturally). The band being `dt`-dependent is why the bug is knife-edge on frame rate.
    ///
    /// #855 replaces that window with `collision::hit_accepted` — a lower bound expressed in world
    /// units rather than as a fraction of the caller's ray — which is what makes this sweep's answer
    /// independent of both `dt` and the character's world coordinates.
    ///
    /// **This is the whole mechanism; there is no second one.** Round 1 of review shipped a
    /// belt-and-braces clamp here against a facing-blind `column_hits` probe, justified by
    /// `nearest_hit` refusing rays shorter than `3.16e-5`. Review measured that the clamp's answer
    /// was a strict superset of the sweep's for this vertical ray, so it *masked* the sweep entirely
    /// and left the primitive fix unpinned by any test. #855 round 2 removed the disagreement at its
    /// source instead — `collision::MIN_RAY_LEN` is now the one degenerate-ray guard for all three
    /// scans, so the sweep answers everything the column probe would have — and deleted the clamp.
    /// The sweep is therefore load-bearing and measured so: see the MUTATION-CHECK block on
    /// `a_driven_swim_descent_never_passes_the_pool_floor_at_any_dt`.
    ///
    /// It never pushes UP: the swept result is `min(0.0)`-ed, so a body already below the surface
    /// beneath it (a depenetration/teleport artifact) is left alone rather than teleported —
    /// recovering that is the depenetration net's job, not the swim step's.
    fn swim_sink(&self, want: f32, col: &Collision) -> f32 {
        let to = [self.pos[0], self.pos[1], self.pos[2] + want];
        match col.nearest_hit(self.pos, to) {
            Some((t, _)) => (t * want + SKIN).min(0.0),
            None => want,
        }
    }

    /// **#870 — the creep, and why it had to be added at the same time as the slide's look-ahead.**
    /// This probe is a POINT probe down the destination's centre column, but the body is a cylinder
    /// of `Body::radius`, and a riser it is pressed against is `radius + SKIN` ahead of that centre.
    /// Before #870 the arithmetic worked only because `slide` left the body PENETRATING the riser:
    /// resting `d` u short of the face with `d <= |wish|`, the raised sweep landed the centre at
    /// `face + (|wish| - d)` — over the lip — and the centre probe found the tread. Once `slide`
    /// honours its own back-off the body rests a full `radius + SKIN` = 1.05 u out, so at any frame
    /// time with `|wish| < 1.05` (35 u/s at 60 Hz is 0.58) the raised sweep can no longer put the
    /// centre past the face and NOTHING is ever mountable. MEASURED as three RED tests when the
    /// look-ahead landed alone: `a_swimmer_at_a_solid_bank_still_hauls_out_the_duck_does_not_override_191`,
    /// `a_swimmer_hauling_out_at_a_legitimate_bank_never_raises_the_afloat_stall`, and
    /// `a_duck_across_a_divable_far_side_is_a_round_trip`, each ending at `east 2.95 = 4.0 − 1.05`,
    /// i.e. correctly backed off and permanently unable to climb out.
    ///
    /// So the destination search creeps forward, in the travel direction, by AT MOST the same
    /// `radius + SKIN` — one expression, the same one the slide backs off by — and takes the first
    /// standable landing.
    ///
    /// **That is a real WIDENING of what counts as climbable, not a restoration of the pre-fix
    /// reach.** An earlier draft of this paragraph claimed the creep lands the body "at or behind
    /// where the penetrating version put it"; that is false and the measurement is the refutation.
    /// Fixture: a 2.0 riser at east 0 whose tread only BEGINS `g` east of the face — a slot the
    /// centre probe lands in — driven due east at 20 u/s, 60 Hz, 600 frames, recording the first
    /// mounting frame (so a depenetration teleport cannot be mistaken for a creep; `is_embedded`
    /// was false on every frame before every mount, and lateral drift was 0.0000 throughout). Wall
    /// and floor use the file's own `wall`/`floor` helpers, north half-extent 100 — which is NOT
    /// the extent behind #938's residual-clearance table above; that table needs half-extent 500,
    /// and at 100 only four of its eight columns reproduce. The difference does not matter here:
    /// the drive is due EAST only, so the body never approaches the north edge and the extent does
    /// not bound these numbers, whereas that table's oblique drives run the body into the wall's
    /// end (#938).
    ///
    /// | tread gap `g` | 0.00 | 0.20 | 0.34 | 0.80 | 1.20 | 1.38 | 1.39 |
    /// |---|---|---|---|---|---|---|---|
    /// | `ce1d89f`, east at mount | 0.0000 | 0.3333 | none | none | none | none | none |
    /// | this branch, east at mount | 0.0000 | 0.3333 | 0.4646 | 0.8583 | 1.2521 | 1.3833 | none |
    ///
    /// For every gap from 0.34 up, the penetrating version puts the body nowhere on the tread at
    /// all, so "at or behind where it put it" had no true reading there. The landings step in
    /// exact increments of `(radius + SKIN) / STEP_LANDING_CREEP_SAMPLES` = 0.13125 and stop at
    /// east 1.3833 — one whole `radius + SKIN` past a base of 0.3333 (that base is the measured
    /// value on this fixture, not a traced mechanism). The 0.13125 stride and the 1.3833 ceiling
    /// are together the fingerprint of the cap, and are what
    /// `the_step_landing_creep_reaches_one_back_off_past_the_riser_and_no_further` pins.
    ///
    /// **Open, and deliberately not asserted either way: whether the widening is DESIRED.**
    /// Stepping over a crack narrower than the body's own diameter is defensible, but it is a
    /// reachability change, it was not compared against the reference client, and no live client
    /// was run (#930).
    ///
    /// Two soundness gates, both load-bearing:
    ///   * the creep runs ONLY when the raised sweep made full progress. If the raised sweep was
    ///     itself stopped by geometry, creeping past its stopping point would push the body into
    ///     the thing that stopped it.
    ///   * every creep sample re-runs the SAME band test as the centre probe, so the creep can
    ///     only find a landing the centre probe would also have accepted — it changes WHERE the
    ///     landing is looked for, never WHAT counts as one.
    const STEP_LANDING_CREEP_SAMPLES: usize = 8;

    /// Step-offset climb (design §3.2): raise the cylinder by `STEP_UP`, sweep again, and — only if
    /// a floor exists to stand on at the raised destination (the no-geometry-gap guard) — return the
    /// stepped-up `[east, north, floor_z]`. `None` = a taller-than-2u wall, OR a tread gap wider than
    /// `radius + SKIN` (#870's creep, doc'd above, extends the search that far past the raised lip —
    /// see #930, which corrected this comment after review found it still calling any gap terminal),
    /// OR a landing outside the step band — a DROP beyond the lip, with no gap in the floor at all.
    /// That third cause is easy to miss because it is not a hole: MEASURED (#987 review round 2), a
    /// 1.5 u riser with a CONTINUOUS far floor 10 u below returns `None` at `wish` 0.58 from east
    /// −0.30 and from −0.10, while the identical fixture with the far floor at 0.0 returns `Some`.
    /// `in_band` is the gate — a floor is only standable if it is within `GROUND_SNAP_TOL` below
    /// the feet and no more than `max_step` above them.
    fn try_step_up(&self, wish: [f32; 3], max_step: f32, col: &Collision) -> Option<[f32; 3]> {
        let raised = [self.pos[0], self.pos[1], self.pos[2] + max_step];
        let (hi, _) = self.slide(raised, wish, col);
        let origin = self.pos[2] + max_step + GROUND_ORIGIN;
        let depth = max_step + GROUND_ORIGIN + GROUND_SNAP_TOL;
        // Probe for a floor near the raised destination, within the step band. The slide above only
        // makes progress when there is open space over the lip, so we never "climb" into solid wall;
        // and a floor must exist here to stand on, so a taller bare wall still returns None.
        let in_band = |f: f32| {
            f >= self.pos[2] - GROUND_SNAP_TOL && f - self.pos[2] <= max_step + GROUND_SNAP_TOL
        };
        if let Some(f) = col.ground_below(hi[0], hi[1], origin, depth) {
            if in_band(f) { return Some([hi[0], hi[1], f]); }
        }
        // #870: creep the destination forward by up to the slide's own back-off. See the doc above.
        let len = hlen(wish);
        if len < 1e-5 { return None; }
        let travelled = hlen([hi[0] - raised[0], hi[1] - raised[1], 0.0]);
        if travelled + 1e-4 < len { return None; } // the raised sweep was itself blocked — do not creep
        let d_hat = [wish[0] / len, wish[1] / len];
        let back_off = crate::traversability::PLAYER_BODY.radius + SKIN;
        for i in 1..=Self::STEP_LANDING_CREEP_SAMPLES {
            let s = back_off * (i as f32) / (Self::STEP_LANDING_CREEP_SAMPLES as f32);
            let (e, n) = (hi[0] + d_hat[0] * s, hi[1] + d_hat[1] * s);
            if let Some(f) = col.ground_below(e, n, origin, depth) {
                if in_band(f) { return Some([e, n, f]); }
            }
        }
        None
    }

    /// The swimming step-up's downward mirror (#661): can a blocked swimmer pass UNDER the
    /// obstruction by diving? Sink the feet (collided, via [`Self::swim_sink`] — the dive cannot
    /// pass through the pool floor) by up to the same envelope the step-up can climb
    /// (`STEP_UP + GROUND_SNAP_TOL` = 2.5 u, the controller's real step capability), then sweep the
    /// wish again from the lowered position. Every clause is a refusal condition, each pinned by a
    /// RED-when-deleted test (named per clause below):
    ///
    /// * the dive must find real room below (`sink` actually resolved downward);
    /// * the lowered start must keep the feet in water — the duck may not dive out the BOTTOM of
    ///   its own water volume, even when the destination would be wet again
    ///   (`a_duck_never_dives_out_the_bottom_of_its_own_water_volume`);
    /// * the destination must keep the feet in water — the duck may not exit the medium SIDEWAYS
    ///   into dry space, where `want_swim` is inert and gravity owns the body
    ///   (`a_duck_never_exits_the_water_sideways`);
    /// * **the destination column must be re-divable (#661 review, B1): its floor must exist and
    ///   sit at or below the ducked feet.** Without this the duck is itself a one-way transition —
    ///   the defect class this whole fix exists to remove: over a far shelf shallower than the
    ///   duck depth, the outbound duck clears the obstruction from deep water while the return
    ///   duck's sink clamps on the shelf and can never get the chest back under the lip (measured:
    ///   far floor −3.0 vs duck z −4.5 → crossed once, then converged against the far face for
    ///   ever, `on_ground=false, in_water=true, hold()=None` — trapped with every observable
    ///   reading "swimming normally"). Requiring `floor ≤ ducked feet` makes the crossing
    ///   **reversible by a driven dive, by construction**: the return only needs feet to re-occupy
    ///   the passage depth, which the floor now provably admits and the destination water check
    ///   already covers. (Residual, **REASONED from the two clamps, not measured** — the label is
    ///   review N1's own, on this very sentence: *"'stated exactly' is a reasoned figure, not a
    ///   measured one."* The return's sink stops `SKIN` = 0.05 u above the floor, so a passage
    ///   whose outbound clearance was under 0.05 u can still shut behind
    ///   the body; and this is reversibility of the PASSAGE for a driver that dives — a
    ///   horizontal-only wish still needs the return column deep enough for the autonomous duck,
    ///   which the same floor bound gives everywhere the two sides' surfaces match.) The probe is
    ///   the same `ground_below` the step-up's landing check uses, so the two mirrors refuse
    ///   unoccupiable destinations the same way — the reviewer's structural point, adopted.
    ///
    /// The feet-only `in_water` probes here are DELIBERATE, not an oversight of the #649
    /// feet-probe lesson (#661 review, N3): a duck is a descent into the medium's interior, so
    /// "the FEET themselves are in water" is the required condition — a chest-based probe would
    /// accept landings whose lower body is out of the volume. In the one geometry where the feet
    /// probe lies (a `.wtr` volume that stops short of the pool floor), these clauses can only
    /// REFUSE a duck, never mount anything — the failure is a missed shortcut, not a wrong state.
    ///
    /// The caller compares the returned progress against the surface slide's and only takes a duck
    /// that measured strictly better — on an ascending bank face (solid to the bottom) the lowered
    /// slide gains nothing and the haul-out step-up keeps the right of way (#191).
    fn try_duck_under(&self, wish: [f32; 3], col: &Collision) -> Option<[f32; 3]> {
        let sink = self.swim_sink(-(STEP_UP + GROUND_SNAP_TOL), col);
        if sink >= -1e-3 { return None; }
        let lowered = [self.pos[0], self.pos[1], self.pos[2] + sink];
        if !col.in_water(lowered) { return None; }
        let (lo, _) = self.slide(lowered, wish, col);
        if !col.in_water(lo) { return None; }
        // Re-divability, floor axis (#661 review B1): the landing column's floor must admit
        // re-occupying the passage depth. `ground_below`'s probe origin is `feet + GROUND_ORIGIN`,
        // so a floor slightly above the ducked feet would still be FOUND — hence the explicit `<=`
        // bound, not just `is_some()`: a floor above the ducked feet shallows the return sink and
        // shuts the door (far floor −4.0 vs duck depth −4.5 is a measured one-way trap under
        // `is_some()` alone; both values are pinned in
        // `a_duck_never_crosses_into_a_column_it_cannot_dive_back_out_of`).
        let floor = col.ground_below(lo[0], lo[1], lo[2] + GROUND_ORIGIN, GROUND_DEPTH)?;
        if floor > lo[2] { return None; }
        // Re-divability, surface axis (#661 review R2-B1): the landing column's own float plane
        // must be inside the duck envelope of the passage depth, or the return duck — whose sink
        // starts from wherever buoyancy parks the body on the far side — cannot get the chest
        // back under the lip (measured: a 2 u surface mismatch across the author's lintel traps
        // permanently, `hold()=None`, every observable reading "swimming normally"). With BOTH
        // bounds an admitted duck's crossing is autonomously round-trip-capable: the return sink
        // can reach the passage depth (this bound), the floor provably admits it (the floor
        // bound), and the passage is wet at both ends (the water checks) — up to the SKIN-sized
        // clearance residual, which review N1 SWEPT FOR AND COULD NOT CONSTRUCT.
        //
        // ⚠️ CORRECTED (#794). This clause used to read "which review N1 measured unreachable in
        // practice", and that overstated what N1 did. N1 swept the lintel underside across the
        // band — −0.20 / −0.40 / −0.46 / −0.49 all crossed AND returned, −0.52 refused outbound —
        // and concluded it could not build the case; it then labelled its own mechanism
        // **REASONED**, in as many words ("'stated exactly' is a reasoned figure, not a measured
        // one"). A failed attempt to construct a counter-example is evidence, but it is not a
        // measurement of unreachability: the sweep can only report what it visited. So the honest
        // statement is the one above — searched, not found, mechanism reasoned — and NOT the
        // reciprocal overclaim either: nothing here says the residual IS reachable, only that this
        // file does not know that it is not. The distinction is load-bearing in this specific
        // comment because the same PR's history is two universals that survived several rewrites
        // and were then falsified by measurement (see the ordering comment in `step` and the two
        // re-divability bounds below), i.e. exactly the shape "swept and did not find" cannot rule
        // out.
        //
        // Cost, stated plainly: this also refuses the qcat pocket-mouth duck for a HORIZONTAL-only
        // wish (the shaft's surface is ~13 u above the pocket's, so that crossing is genuinely
        // one-way for the autonomous driver — the same shape as the trap, distinguishable only by
        // global knowledge the controller does not have; the planner has it and can always route
        // a dive back). Measured against the real system, the cost is nearly nil: a DRIVEN dive
        // (an explicit down-wish, what a dive-first route sends) crosses with no duck involved
        // (`qcat_pocket_swimmer_escapes_to_the_shaft_under_a_driven_dive`), and the walker's own
        // rise-y steering at this pocket sends an UP-wish, which the `wish_vspeed` gate already
        // excludes from ducking — with or without this bound. What the bound actually forecloses
        // is a horizontal-wish driver (e.g. `/move/manual`) making a one-way crossing it cannot
        // undo; that driver now stalls wet at the mouth instead
        // (`qcat_pocket_horizontal_wish_alone_stalls_wet_at_the_mouth`).
        //
        // ⚠️ **The tolerance is not decoration (#870).** Without it this comparison is decided by
        // float noise for the commonest case there is: a body resting AT its own float plane.
        // Expand the left side — `surf − float_depth − (pos_z − sink)` with an unclamped
        // `sink = −(STEP_UP + GROUND_SNAP_TOL)` — and the clause reduces exactly to
        // `pos_z >= surf − float_depth`, i.e. "the body is at or above its float plane". Buoyancy
        // parks it there asymptotically, so the deciding quantity is the last ULPs of the approach.
        // MEASURED on the `a_duck_across_a_divable_far_side_is_a_round_trip` lintel, same fixture,
        // two resting spots 0.78 u apart: `2.4999952 <= 2.5` (admits) at east 4.267 versus
        // `2.5000072 <= 2.5` (REFUSES) at east 5.05 — a 1.2e-5 u swing across the boundary, which
        // is far below anything the geometry means. `DUCK_ENVELOPE_TOL` is 1e-3 u, and both
        // ratios are re-derived from the constants rather than quoted: 1e-3 / 1.2e-5 = 83× that
        // noise, and 1e-3 / (STEP_UP + GROUND_SNAP_TOL) = 1e-3 / 2.5 = 1/2500 of the envelope it
        // slackens, so it cannot admit a duck whose return is in any physical doubt — the trap
        // this bound exists to refuse is a 2 u mismatch, and 2.0 / 1e-3 = two thousand times
        // wider. #870 found it by moving where the body rests, not by changing this
        // clause; on `main` the same test passes only because its body happens to settle on the
        // admitting side of the knife-edge.
        let surf = col.water_surface(lo)?;
        ((surf - crate::traversability::PLAYER_BODY.float_depth) - lo[2]
            <= STEP_UP + GROUND_SNAP_TOL + DUCK_ENVELOPE_TOL).then_some(lo)
    }

    /// Is the wedged-against barrier a *hoppable* fence — i.e. is there walkable floor `HOP_REACH`
    /// ahead in the move direction, at roughly the current foot height? True → a low rail with flat
    /// floor beyond (hop over it). False → no floor in band ahead, meaning a real wall (far floor
    /// much higher or absent) or a ledge/cliff (far floor far below); don't hop in either case (#41).
    fn can_hop(&self, wish: [f32; 3], col: &Collision) -> bool {
        let len = hlen(wish);
        if len < 1e-4 { return false; }
        let px = self.pos[0] + wish[0] / len * HOP_REACH;
        let py = self.pos[1] + wish[1] / len * HOP_REACH;
        // Use nearest_floor (whole-column) rather than a single down-ray: a cart/fence can be TALLER
        // than the probe origin, which makes a down-ray miss its top and report garbage. nearest_floor
        // returns the surface closest to our CURRENT height — i.e. the low ground/slope to land on,
        // not the cart top — so we only hop toward a near-level landing, never up a wall or off a cliff.
        match col.nearest_floor(px, py, self.pos[2], HOP_PROBE_UP, HOP_PROBE_DOWN) {
            Some(f) => f - self.pos[2] <= HOP_PROBE_UP && self.pos[2] - f <= HOP_PROBE_DOWN,
            None => false,
        }
    }

    /// Depenetration / unstuck net (§3.3). Returns `true` when this frame was embedded and handled
    /// (push-out moved us, or the last-good fallback fired, or we are still searching) — the caller
    /// then freezes the rest of the step. Returns `false` on a normal (clear) frame.
    ///
    /// `prev_hold` is the hold `step` took at the top of this frame — see `enter_hold`. Nothing in
    /// here reads it for physics; it exists so a continuing hold can accumulate its duration.
    fn depenetrate(&mut self, dt: f32, col: &Collision, prev_hold: Option<ControllerHold>) -> bool {
        // No geometry loaded → no constraints; never teleport the free player.
        if !col.has_geometry() {
            self.stuck_time = 0.0;
            return false;
        }
        let p = self.pos;
        // #661: A BODY AFLOAT IN WATER IS NEVER THE NET'S PROBLEM. The net exists for bodies
        // stuck in geometry with gravity pulling them deeper; a floating body's vertical is owned
        // by buoyancy and its lateral motion by the collided slide, and every question the net
        // asks about it is mis-posed:
        //
        //   * `footprint_clear` probes the ring at `feet + Body::ring` = feet + 3, and a swimmer
        //     floats at `surface − float_depth` = surface − 2 — so the probe tests the AIR band
        //     1 u ABOVE the waterline, where every shoreline's dry geometry lives. Measured at
        //     the qcat spawn pocket (#661): a swimmer whose route was physically open (its slide
        //     made full progress on every frame it was allowed to run) read as "embedded" on
        //     alternate frames purely from above-surface rim contact; the ring push-out then ate
        //     its input and ping-ponged it in place — `walker_stalled`, live — until one candidate
        //     fell outside the `.wtr` region's XY extent and the water-blind floor fall-through
        //     beached the still-swimming body onto the dry tile at −55.96875 (`on_ground`, dry,
        //     `want_swim` inert: the one-way strand).
        //   * `ground_below(..).is_none()` reads "fallen out of the world", which is not a state
        //     a floating body can be in — it is how #664's clear-footprint deep-water swimmer
        //     was dragged into the net at all.
        //   * And a MORE water-aware net was measured worse, not better: an intermediate revision
        //     of this fix probed the footprint at the submerged torso band instead, which made
        //     the net fire during ordinary bank approaches and TUNNEL the body through the bank
        //     face to the first clear ring candidate on the far side (walker_sim P1 ended 650 u
        //     out to sea). There is no probe height at which "near geometry" is an emergency for
        //     a body that floats.
        //
        // So the medium decides AT THE DOOR, with the same body probe `step` uses (#649's
        // `body_in_water`, pinned by `the_nets_water_probe_is_the_BODY_not_the_feet`): a floating
        // body takes the ordinary clear path — stuck-clock reset, good-sample banking (waders,
        // standing in shallow water, still bank) — and physics keeps custody. The dry-body net
        // below is byte-identical to what it has always been.
        if body_in_water(col, p) || !is_embedded(col, p) {
            self.stuck_time = 0.0;
            self.good_timer += dt;
            // The ring's invariant — every banked sample is GROUNDED and NON-EMBEDDED — used to
            // hold by position in the control flow (this arm was only reachable when
            // `!is_embedded`). Widening the door for wet bodies broke that silently: a wading
            // body embedded between rocks reached this arm and banked embedded restore points,
            // which both ring readers (the stuck fallback and the underworld fall-through guard)
            // would then restore a body INTO — the #724 stale-ring re-banking shape (#661 review,
            // B3, measured: `banked=4 any_embedded=TRUE` vs main's `banked=0`). So the invariant
            // is now enforced where the sample is taken, for every medium; the predicate runs
            // only on sample frames (every GOOD_SAMPLE_SECS), and for a dry body it is a
            // re-evaluation of the door's own answer.
            if self.on_ground && self.good_timer >= GOOD_SAMPLE_SECS && !is_embedded(col, p) {
                self.good_timer = 0.0;
                if self.good.len() >= GOOD_RING_LEN { self.good.pop_front(); }
                self.good.push_back(self.pos);
            }
            return false;
        }
        // Embedded (and dry, per the door above): ring push-out to the nearest clear column with
        // a floor the body can stand on.
        for &r in &PUSHOUT_RADII {
            for i in 0..PUSHOUT_DIRS {
                let a = (i as f32) / (PUSHOUT_DIRS as f32) * std::f32::consts::TAU;
                let (e, n) = (p[0] + a.cos() * r, p[1] + a.sin() * r);
                if !col.footprint_clear(e, n, p[2], PLAYER_RADIUS, PUSHOUT_DIRS / 2) { continue; }
                if let Some(rec) = Recovery::at_column(col, e, n, p[2]) {
                    self.recover(e, n, rec);
                    tracing::debug!("depenetrate: pushed out from ({:.1},{:.1},{:.1}) to ({:.1},{:.1},{:?})",
                        p[0], p[1], p[2], e, n, rec);
                    return true;
                }
            }
        }
        // Push-out failed: count time stuck, then fall back to the most recent good position.
        self.stuck_time += dt;
        if self.stuck_time >= STUCK_FALLBACK_SECS {
            // Bound the ring borrow before the `None` arm needs `&mut self` (#845).
            let banked = self.good.back().copied();
            match banked {
                Some(g) => {
                    tracing::info!("depenetrate: stuck {:.1}s, falling back to last good pos {:?}", self.stuck_time, g);
                    // The ring buffer only ever samples GROUNDED, NON-EMBEDDED positions — enforced
                    // by the explicit `!is_embedded` predicate at the banking site since #661's
                    // review (B3; the wet-door widening had silently let an embedded wading body
                    // bank) — so this fallback is a `Grounded` recovery by construction, routed
                    // through the same single writer so the net has exactly one place that sets
                    // the support flags.
                    self.recover(g[0], g[1], Recovery::Grounded(g[2]));
                }
                // #724 review B1 — the branch that used not to exist. With an empty ring this arm
                // changes nothing and `depenetrate` returns `true`, so `step` skips the entire rest
                // of the frame: the body cannot move in ANY direction, under any driver, for ever.
                // Before #724 an empty ring here was rare; #724 makes it the normal state after
                // every relocation, so this is now an ordinary outcome of a GM summon into rock. It
                // was completely silent — the `tracing::info!` above is inside the `Some` arm, and
                // no agent-visible field carried a stuck/embedded signal at all. Say so, on both
                // channels.
                //
                // ⚠️ AMENDED (#845). "The body cannot move in ANY direction, under any driver, for
                // ever" was an accurate description of this arm and that is the whole problem: it
                // is an ABSORBING state of this state machine. Nothing the arm executes writes
                // `pos`, `on_ground`, `good` or `stuck_time`, so the next frame is bit-identical
                // and no driver input appears in any variable the arm reads. Two live validation
                // runs died in it; the second was measured — every one of `/v1/move/manual`
                // (east / north / west+jump / up), `/v1/move/jump` and `/v1/move/stop` returned
                // HTTP 200 and left `pos` byte-identical at `[-2190.5, 902.125, 3.5]` while
                // `held_secs` ran from 38 s to 130 s. The hold below is still raised, and is still
                // the honest disclosure; what changed is that it is now raised only after
                // `last_resort_placement` has asked the ZONE whether there is anywhere to stand,
                // rather than only asking this body's own erased history. See
                // `nearest_standing_place`.
                //
                // AMENDED AGAIN (#884): the freeze this comment describes is UNCHANGED — this arm is
                // still absorbing and no driver input still reaches it. What changed is the HTTP
                // answer above it. `/v1/move/{goto,follow,zone_cross,manual,jump}` now read
                // `player_hold` and return `409 {"status":"held"}` instead of `200` while
                // `EmbeddedNoRecovery` is published, because the measured `200`s quoted above were
                // the client asserting an acceptance it could not honour. `/v1/move/stop` is
                // deliberately still `200`: it cancels a goal, which is true of a frozen body.
                // `UnderworldNoRecovery` is NOT gated — that arm runs after collide-and-slide, so
                // lateral wishes do reach the body and walking out is its only client-API exit.
                None if self.last_resort_placement(col, dt) => {}
                None => self.enter_hold(ControllerHoldReason::EmbeddedNoRecovery, dt, prev_hold),
            }
        }
        true
    }

    /// #845 — **the last resort for the one arm that is genuinely absorbing.** Returns `true` if the
    /// body was placed somewhere it can stand, `false` if this zone offered nowhere (or the retry
    /// throttle declined to look this frame).
    ///
    /// Called from exactly ONE site: the stuck fallback in [`Self::depenetrate`], and only on the
    /// arm where the `good` ring is empty. Where the ring HAS a sample nothing changes — the restore
    /// still wins, because a banked position is a place this body actually stood, which is strictly
    /// better evidence than a search result.
    ///
    /// **Deliberately NOT called from `step`'s #150 fall-through guard**, whose no-history arm looks
    /// like the same dead end and is not one. That arm runs after collide-and-slide, so the driver's
    /// lateral input has already been applied to the body by the time it executes: a body holding
    /// `UnderworldNoRecovery` can be walked out through `/v1/move/manual` under its own power, so it
    /// already has the client-API exit #845 asks for. Extending the search there was implemented and
    /// then reverted, because it relocates bodies that #724 is deliberately holding where the SERVER
    /// put them; that rationale deserves its own evidence rather than being overturned as a side
    /// effect. The consequence to be honest about: an `underworld_no_recovery` body whose zone has no
    /// floor within lateral reach still has no exit, and this PR does not change that.
    ///
    /// **This is a relocation, and the log is loud about it.** The body is moved as much as
    /// [`RESCUE_RADII`]'s reach, without the driver asking, which is exactly the kind of silent
    /// client-side write this project treats as a lie. Two things keep it honest: it fires only
    /// from a state the body could not leave under ANY driver (so the alternative is not "walk
    /// there yourself", it is "stay frozen until a GM intervenes"); and it logs at `warn`,
    /// unthrottled, with the origin and the destination in full — because a relocation is an event
    /// rather than a condition. (The `moved` figure on that line is HORIZONTAL only, so read the
    /// destination, not the distance, if the placement changed height.)
    ///
    /// ⚠️ **A third thing does NOT keep it honest, and an earlier version of this comment claimed
    /// it did.** A success here is invisible to `player.hold`: this arm returns before
    /// [`Self::enter_hold`] is reached, and `step` takes the hold at the top of every frame, so the
    /// body is relocated with the field `None` throughout — measured at 0 held frames of 300 in a
    /// zone this search solves. The transition an agent can see is the inverse one: a *published*
    /// hold means this search answered `nowhere`, and it does not clear on its own (measured at
    /// 1800 frames / 60 s in a static zone it cannot solve, raised and never cleared). Nothing on
    /// the HTTP side marks the relocation; the `warn` below is its only record. That gap is #925.
    ///
    /// Not throttled on success — a success moves the body, so it cannot repeat from the same
    /// place. [`RESCUE_RETRY_SECS`] throttles only the failing search.
    fn last_resort_placement(&mut self, col: &Collision, dt: f32) -> bool {
        if self.rescue_cooldown > 0.0 { return false; }
        let from = self.pos;
        match nearest_standing_place(col, from, self.underworld) {
            Some(q) => {
                let moved = ((q[0] - from[0]).powi(2) + (q[1] - from[1]).powi(2)).sqrt();
                tracing::warn!(
                    "controller RELOCATED [#845]: {:?} was unrecoverable (push-out found nowhere, \
                     no recovery history) — moved {:.1} u to the nearest place this zone can stand \
                     a body, {:?}. This is a client-side relocation, not a server correction.",
                    from, moved, q);
                self.recover(q[0], q[1], Recovery::Grounded(q[2]));
                true
            }
            None => {
                self.rescue_cooldown = RESCUE_RETRY_SECS.max(dt);
                false
            }
        }
    }

    /// Apply a [`Recovery`] — the ONLY place the depenetration net writes position and support
    /// state, so the variant's meaning ("feet on floor" vs "floating") cannot be contradicted by a
    /// caller that forgets a flag (#649).
    fn recover(&mut self, east: f32, north: f32, rec: Recovery) {
        self.pos = [east, north, rec.z()];
        self.vel_z = 0.0;
        self.on_ground = rec.on_ground();
        self.stuck_time = 0.0;
        self.airborne_start_z = None; // a push-out / last-good recovery is not a fall landing (§442 hazard 2a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{ZoneAssets, MeshData, RenderMode};
    use crate::nav::collision::Collision;

    #[test]
    fn manual_wish_normalizes_and_faces_the_move_direction() {
        // North (+north) → unit north, heading 0.
        let (w, h) = manual_wish([0.0, 5.0]);
        assert!((w[0]).abs() < 1e-5 && (w[1] - 1.0).abs() < 1e-5);
        assert!((h.unwrap()).abs() < 1e-4);
        // East (+east) → unit east, heading 270 (EQ: 0=north, CCW, so east = 270°).
        let (w, h) = manual_wish([5.0, 0.0]);
        assert!((w[0] - 1.0).abs() < 1e-5 && w[1].abs() < 1e-5);
        assert!((h.unwrap() - 270.0).abs() < 1e-3);
        // Zero request → no movement, no heading change (e.g. jump in place).
        let (w, h) = manual_wish([0.0, 0.0]);
        assert_eq!(w, [0.0, 0.0]);
        assert!(h.is_none());
    }

    fn mesh(positions: Vec<[f32; 3]>) -> MeshData {
        MeshData {
            positions, normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }
    /// Floor at height `z` over east [e0,e1] × north [-100,100]. libeq pos = [north, height, east].
    fn floor(z: f32, e0: f32, e1: f32) -> MeshData {
        mesh(vec![[-100.0, z, e0], [100.0, z, e0], [100.0, z, e1], [-100.0, z, e1]])
    }
    /// Vertical wall at east=`e`, north [-100,100], height [h0,h1].
    fn wall(e: f32, h0: f32, h1: f32) -> MeshData {
        mesh(vec![[-100.0, h0, e], [100.0, h0, e], [100.0, h1, e], [-100.0, h1, e]])
    }
    fn col(meshes: Vec<MeshData>) -> Collision {
        Collision::build(&ZoneAssets { terrain: meshes, objects: vec![], textures: vec![] }, 4.0)
    }
    fn walk(speed: f32, dir: [f32; 2]) -> MoveIntent {
        MoveIntent { wish_dir: dir, wish_vspeed: 0.0, jump: false, want_swim: false, speed,
                     climb: 0.0, hop: false }
    }
    /// Partial vertical wall: east=`e`, north [n0,n1], height [h0,h1] — for bends/obstacles.
    fn wall_seg(e: f32, n0: f32, n1: f32, h0: f32, h1: f32) -> MeshData {
        mesh(vec![[n0, h0, e], [n1, h0, e], [n1, h1, e], [n0, h1, e]])
    }
    /// Min distance from `p=[east,north]` to the path polyline's XY segments (cross-track error).
    fn xte(p: [f32; 2], path: &[[f32; 3]]) -> f32 {
        let mut best = f32::MAX;
        for seg in path.windows(2) {
            let (a, b) = (seg[0], seg[1]);
            let ab = [b[0] - a[0], b[1] - a[1]];
            let l2 = ab[0] * ab[0] + ab[1] * ab[1];
            let t = if l2 < 1e-6 { 0.0 } else { (((p[0] - a[0]) * ab[0] + (p[1] - a[1]) * ab[1]) / l2).clamp(0.0, 1.0) };
            let c = [a[0] + ab[0] * t, a[1] + ab[1] * t];
            best = best.min(((p[0] - c[0]).powi(2) + (p[1] - c[1]).powi(2)).sqrt());
        }
        best
    }

    /// Targeted navigation regression: drive the real controller down a real A* path that BENDS
    /// around an obstacle, using the same fast-steering the nav thread does (carrot look-ahead on the
    /// path from the CURRENT position each frame), and assert the avatar HUGS the line — it reaches
    /// the goal and never strays more than a small margin. This is what "not following the line /
    /// running into things" looks like as a measurement: excessive cross-track error at the bend.
    #[test]
    fn nav_walker_hugs_a_bending_path_without_straying() {
        use crate::nav::steering::carrot_along;
        // Floor east[-50,50] × north[-100,100]; a wall at east=0 blocks north<12, so the route must
        // detour up over the wall top (north≥12) and back down — a bend the walker must track.
        let col = col(vec![
            floor(0.0, -50.0, 50.0),
            wall_seg(0.0, -100.0, 12.0, 0.0, 20.0),
        ]);
        let start = [-40.0, 0.0, 0.0];
        let goal  = [40.0, 0.0, 0.0];
        let path = col.find_path(start, goal, PLAYER_RADIUS, &[], false).expect("route around the wall");
        let line: Vec<[f32; 3]> = std::iter::once(start).chain(path.iter().copied()).collect();

        let mut ctrl = CharacterController::new(start);
        ctrl.on_ground = true;
        let (mut path_i, mut max_xte, mut arrived) = (0usize, 0.0f32, false);
        for _ in 0..4000 {
            // Advance the active segment as we pass it (mirrors the walker's path_i logic).
            while path_i + 2 < line.len() {
                let (a, b) = (line[path_i], line[path_i + 1]);
                let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
                let l2 = ab[0] * ab[0] + ab[1] * ab[1] + ab[2] * ab[2];
                let t = if l2 < 1e-6 { 1.0 } else { ((ctrl.pos[0] - a[0]) * ab[0] + (ctrl.pos[1] - a[1]) * ab[1] + (ctrl.pos[2] - a[2]) * ab[2]) / l2 };
                if t >= 1.0 { path_i += 1; } else { break; }
            }
            let carrot = carrot_along(&line, path_i, [ctrl.pos[0], ctrl.pos[1], ctrl.pos[2]], 5.0).unwrap();
            let (dx, dy) = (carrot[0] - ctrl.pos[0], carrot[1] - ctrl.pos[1]);
            let d = (dx * dx + dy * dy).sqrt().max(1e-3);
            let intent = MoveIntent { wish_dir: [dx / d, dy / d], wish_vspeed: 0.0, jump: false,
                want_swim: false, speed: 44.0, climb: 0.0, hop: false };
            ctrl.step(intent, 0.016, &col);
            // Skip the tail approach to the goal (the carrot shortens there) — measure along the route.
            if ((ctrl.pos[0] - goal[0]).powi(2) + (ctrl.pos[1] - goal[1]).powi(2)).sqrt() > 6.0 {
                max_xte = max_xte.max(xte([ctrl.pos[0], ctrl.pos[1]], &line));
            }
            if ((ctrl.pos[0] - goal[0]).powi(2) + (ctrl.pos[1] - goal[1]).powi(2)).sqrt() < 3.0 { arrived = true; break; }
        }
        assert!(arrived, "walker must reach the goal (ended at {:?})", ctrl.pos);
        assert!(max_xte < 3.0, "walker strayed {max_xte:.1}u off the line at the bend (corner-cutting into walls)");
    }

    #[test]
    fn slides_along_wall_instead_of_stopping() {
        let c = col(vec![floor(0.0, -100.0, 100.0), wall(5.0, 0.0, 10.0)]);
        let mut ctrl = CharacterController::new([3.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        // Drive diagonally into the wall (north-east). East is blocked at 5; the controller should
        // slide north rather than stop dead.
        ctrl.step(walk(35.0, [std::f32::consts::FRAC_1_SQRT_2, std::f32::consts::FRAC_1_SQRT_2]), 0.1, &c);
        assert!(ctrl.pos[0] < 4.1, "should be stopped short of the wall (no penetration, east<4.1): {}", ctrl.pos[0]);
        assert!(ctrl.pos[1] > 0.5, "should have slid north along the wall: {}", ctrl.pos[1]);
    }

    #[test]
    fn buoyancy_floats_toward_surface_instead_of_sinking() {
        // Open deep water: everything below z=10 is water, and there is NO floor at all.
        let mut c = col(vec![]);
        c.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::flat_below(10.0))));
        // Submerged at z=0, not on the ground, and NOT actively swimming (want_swim=false) — the
        // "walked into the river" case. Previously this free-fell forever (#172).
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        ctrl.on_ground = false;
        for _ in 0..180 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(ctrl.pos[2] > 0.0, "should float UP, not sink: {}", ctrl.pos[2]);
        assert!((7.0..=9.0).contains(&ctrl.pos[2]),
            "should settle just below the z=10 surface (~8): {}", ctrl.pos[2]);
        assert!(ctrl.vel_z.abs() < 1e-3, "no accumulating fall velocity: {}", ctrl.vel_z);
    }

    #[test]
    fn nav_swim_floats_off_the_bottom_toward_the_surface() {
        // Deep water (surface z=10). The character starts submerged and grounded on the bottom
        // (z=-20) — the case where a path routed it to the pool floor. A nav-driven swim
        // (want_swim=true, no vertical wish) must float it UP to the surface, not leave it crawling
        // the bottom (#191).
        let mut c = col(vec![]);
        c.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::flat_below(10.0))));
        let mut ctrl = CharacterController::new([0.0, 0.0, -20.0]);
        ctrl.on_ground = true;
        let swim = MoveIntent {
            wish_dir: [1.0, 0.0], wish_vspeed: 0.0, jump: false, want_swim: true,
            speed: 35.0, climb: 0.0, hop: false,
        };
        for _ in 0..240 { ctrl.step(swim, 1.0 / 60.0, &c); }
        assert!(ctrl.pos[2] > 5.0, "swim floats off the bottom toward the surface (~8): {}", ctrl.pos[2]);
    }

    #[test]
    fn buoyancy_floats_off_the_bottom_when_grounded_and_not_swimming() {
        // #197: nav pathed the character DOWN to the pool floor and then STOPPED driving, so it
        // rests on_ground on the bottom, submerged, with want_swim=false. Passive buoyancy must
        // still float it back up — before the fix it sat on the bottom forever (the buoyancy branch
        // required !on_ground).
        let mut c = col(vec![]);
        c.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::flat_below(10.0))));
        let mut ctrl = CharacterController::new([0.0, 0.0, -20.0]);
        ctrl.on_ground = true; // resting on the pool bottom, NOT swimming
        for _ in 0..240 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(ctrl.pos[2] > 5.0, "must float off the bottom to the surface (~8), got {}", ctrl.pos[2]);
        assert!(!ctrl.on_ground, "detaches from the floor while floating up");
    }

    /// #329, the qcat spawn shaft: the water volume's LOWER bound sits slightly ABOVE the floor the
    /// character stands on (floor -69.97, water -69.5…-43.0). Probing water at the character's
    /// origin — its FEET — then reports "dry" for a character standing under 26 units of water: it
    /// never swims, buoyancy never fires, and it is pinned to the shaft floor for ever. That is what
    /// made the qcat spawn pocket an inescapable trap. Water must be probed against the BODY.
    #[test]
    fn submerged_character_whose_feet_are_below_the_water_volume_still_floats() {
        // Water from z=-69.5 up to z=-43 — a box that does NOT reach the floor at -69.97.
        let mut c = col(vec![]);
        c.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::water_slab(-69.5, -43.0),
        )));
        // Sanity: the feet really are outside the water volume, the chest really is inside it.
        assert!(!c.in_water([0.0, 0.0, -69.97]), "feet sit below the water region's lower bound");

        let mut ctrl = CharacterController::new([0.0, 0.0, -69.97]);
        ctrl.on_ground = true; // standing on the shaft floor, fully submerged
        for _ in 0..240 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(ctrl.pos[2] > -50.0,
            "a submerged character must float up toward the surface (~-45), got {}", ctrl.pos[2]);
    }

    /// P3 — ceiling-flush water (#359, second mechanism; water design §9 gate). A swimmer under a
    /// low ceiling, driven UP by the nav swim-up wish, must NOT get embedded in the ceiling: the
    /// vertical swim is now COLLIDED (`swim_rise` sweeps the body top and stops short of the first
    /// solid hit). Pre-fix this was a raw `pos[2] += wish_vspeed*dt` write — the rise drove the
    /// body top straight through the ceiling; the depenetration net then read the frame as embedded
    /// and slammed the character back to the last good GROUNDED position, the shaft floor. That is
    /// the qcat spawn corridor (water line flush with the ceiling): rising CAUSED the strand it was
    /// meant to fix.
    ///
    /// The fixture: a solid ceiling floor at z=6, water up to z=5 (its surface a hair under the
    /// ceiling), a swimmer starting mid-column. Assert the body top (`pos.z + Body::height`, 6.0)
    /// never crosses the ceiling — i.e. it is never embedded — and it does not get slammed back
    /// down below its start. MUTATION-CHECK: reverting `swim_rise` to the raw `pos[2] += want`
    /// write turns this RED (the head embeds and/or the depenetration slam-back fires).
    #[test]
    fn p3_collided_swim_does_not_embed_under_a_flush_ceiling() {
        // Ceiling slab at z=6 (a floor the body top would hit), plus a deep pool floor at z=-20 so
        // the depenetration net (which reads "no floor anywhere below" as embedded) doesn't freeze
        // the swimmer — the qcat shaft has a floor under the water too.
        let mut c = col(vec![floor(6.0, -100.0, 100.0), floor(-20.0, -100.0, 100.0)]);
        // Water in the SLAB -19.5..5 — surface one unit under the ceiling, bounded below like a real
        // `.wtr` volume (and not touching the pool floor, the qcat shape).
        c.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::water_slab(-19.5, 5.0))));

        let body_h = crate::traversability::PLAYER_BODY.height; // 6.0
        // Start with the body fully under the ceiling: feet at z=-2 → head at z=4, under the z=6 slab.
        let start_z = -2.0;
        let mut ctrl = CharacterController::new([0.0, 0.0, start_z]);
        // Drive a persistent upward swim wish (the nav swim-up toward a high waypoint), like the walker.
        let swim_up = MoveIntent {
            wish_dir: [0.0, 0.0], wish_vspeed: 20.0, jump: false, want_swim: true,
            speed: 0.0, climb: 0.0, hop: false,
        };
        let mut worst_head = f32::MIN;
        for _ in 0..600 {
            ctrl.step(swim_up, 1.0 / 60.0, &c);
            worst_head = worst_head.max(ctrl.pos[2] + body_h);
        }
        // The head must never cross the ceiling (never embedded): a hair of skin-width tolerance.
        assert!(worst_head <= 6.0 + 0.1,
            "collided swim must keep the head below the z=6 ceiling (no embed): worst head z={worst_head}");
        // And it must NOT have been slammed back below its start by a depenetration recovery.
        assert!(ctrl.pos[2] >= start_z - 0.1,
            "swimmer must not be slammed back down (no depenetration recovery to the floor): z={}", ctrl.pos[2]);
        // Positive control: it DID rise toward the surface (feet up near surface − float_depth = 3).
        assert!(ctrl.pos[2] > start_z + 1.0,
            "swimmer should still have risen toward the surface, got z={}", ctrl.pos[2]);
    }

    // ── #855: a driven swim descent is floor-bounded ─────────────────────────────────────────────

    /// Pool floor at `FLOOR_Z` with water from just under it up to z = 5 — a swimmer near the
    /// bottom is wet at the feet, so `want_swim` really engages the swim branch.
    const POOL_FLOOR_Z: f32 = -20.0;
    fn pool() -> Collision {
        let mut c = col(vec![floor(POOL_FLOOR_Z, -100.0, 100.0)]);
        c.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::water_slab(POOL_FLOOR_Z - 0.5, 5.0))));
        c
    }
    fn dive(vspeed: f32) -> MoveIntent {
        MoveIntent { wish_dir: [0.0, 0.0], wish_vspeed: vspeed, jump: false, want_swim: true,
                     speed: 0.0, climb: 0.0, hop: false }
    }

    /// **#855 — THE UNIVERSAL: a driven swim descent never puts the feet below the floor, at ANY
    /// `dt`.** Written as a `dt` sweep on purpose: the reported defect was knife-edge on the frame
    /// time, and "whether the pool bottom is solid" is not allowed to be a property of the frame
    /// rate. The starting `eps` sweep covers the band the issue measured as fatal (0 … 0.0005 at
    /// `dt = 0.016`) and the old `1e-3 × ray length` cliff at every swept speed.
    ///
    /// **Scope, stated so the sweep is not mistaken for a reproduction.** The issue's knife-edge
    /// (`dt = 0.016` fell through, `dt = 1/60` parked) was measured on real zone geometry, which
    /// this test does not have and which was NOT re-run here. What this test asserts is the
    /// stronger, `dt`-free bound; the reproduction lives in
    /// `a_driven_swim_descent_never_passes_a_real_zone_floor`.
    ///
    /// One number, with its provenance: under the reach mutation below the assertion reports
    /// `z=-55 (floor -20) at dt=1 eps=0 vspeed=-35`. That is the WORST cell and it is all the
    /// assertion reports — it is deliberately not a per-`dt` table, because a previous version of
    /// this block carried one whose per-row provenance could not be reconstructed and which the
    /// panic message cannot support. Whether the mutation falls through at *every* swept `dt` was
    /// therefore NOT measured and is not claimed here.
    ///
    /// MUTATION-CHECK — every row below was RUN against the code that SHIPS, in both directions,
    /// not predicted. Round 1 of review caught that the previous version of this block drew its
    /// reach conclusion from a configuration that did not ship (epsilon restored *plus* the clamp
    /// deleted); the clamp is gone now and every row is the shipped shape.
    ///   * epsilon `t > 1e-3` restored in `Collision::nearest_hit` — the scan `swim_sink` actually
    ///     calls — and NOTHING else → **RED**. This is the REACH control (#799): it proves this
    ///     test exercises `nearest_hit`'s acceptance test rather than merely coexisting with it.
    ///   * `hit_accepted` WRAPPED to `t > 0.0 && t <= 1.0`, i.e. the call site still reached and the
    ///     upper bound still enforced but the world-unit lower slack never applied → GREEN **here**,
    ///     **RED** on the real-geometry corpus (`a_driven_swim_descent_never_passes_a_real_zone_floor`)
    ///     and RED on three `collision.rs` unit pins. Recorded as a measured limit of THIS test, and
    ///     the reason for it is NOT the one an earlier draft of this block gave. That draft said a
    ///     pool "at the world origin cannot see the coordinate-scaled cancellation"; the discriminator
    ///     was measured and it is not the coordinate magnitude. Moving a flat quad out to
    ///     `(2081, 2320, −87)` does not reopen the band (it measures ~1e-28 with the slack removed),
    ///     while a TILTED quad at the origin does:
    ///     `a_floor_z_the_module_just_reported_always_has_a_floor_under_it`
    ///     goes RED at 958/1369 in its origin regime under exactly this wrap. What
    ///     matters is whether the floor z handed back was RECONSTRUCTED off-plane: on an axis-aligned
    ///     quad the interpolated z is the plane's z exactly, so a ray from it never starts inside the
    ///     solid. `pool()` is one axis-aligned quad, so it structurally cannot exhibit the defect this
    ///     PR fixes — which is why the corpus test and the tilted fixtures exist and why this test is
    ///     the `dt`-universal, not the reproduction.
    ///   * epsilon restored in `nearest_hit_t` ONLY → **GREEN**. Correct and worth recording: the
    ///     controller never routes through the `_t` scan, so a fix applied to only that one would
    ///     have shipped with this test still passing.
    #[test]
    fn a_driven_swim_descent_never_passes_the_pool_floor_at_any_dt() {
        let c = pool();
        let mut worst = (f32::MAX, 0.0f32, 0.0f32, 0.0f32); // (z, dt, eps, vspeed)
        for &dt in &[1.0 / 60.0, 0.016, 0.0161, 0.017, 0.02, 0.033, 0.05, 0.1, 0.25, 0.5, 1.0] {
            for &eps in &[0.0f32, 1e-6, 1e-4, 3e-4, 5e-4, 1e-3, 2.5e-3, 9e-3, 0.05, 0.5] {
                for &vspeed in &[-35.0f32, -10.0, -1.0, -0.1] {
                    let mut ctrl = CharacterController::new([0.0, 0.0, POOL_FLOOR_Z + eps]);
                    for _ in 0..300 {
                        ctrl.step(dive(vspeed), dt, &c);
                        if ctrl.pos[2] < worst.0 { worst = (ctrl.pos[2], dt, eps, vspeed); }
                    }
                }
            }
        }
        assert!(worst.0 >= POOL_FLOOR_Z - 1e-3,
            "a driven swim descent reached z={} (floor {POOL_FLOOR_Z}) at dt={} eps={} vspeed={} — \
             the pool bottom must not be permeable, and must not depend on the frame time",
            worst.0, worst.1, worst.2, worst.3);
    }

    /// **The short-ray hole, closed at its source (#855 round 2).** `Collision::nearest_hit` used
    /// to return `None` unconditionally for a ray whose squared length was under `1e-9` —
    /// `|want| < ~3.16e-5` — so a small driven `wish_vspeed` produced a descent the sweep did not
    /// test *at all*, whatever its acceptance window was. That is reachable: `want = wish_vspeed · dt`, and
    /// 0.001 u/s at 60 Hz is `1.67e-5`.
    ///
    /// Round 1 shipped a second, column-based clamp to cover it. Round 2 removed the cause instead:
    /// all three scans now share `collision::MIN_RAY_LEN` (`1e-6`, linear, world units), so the
    /// sweep answers here too and the clamp was deleted. The first assertion below is the pin on
    /// that — it is the exact case that used to return `None`.
    ///
    /// MUTATION-CHECK, RUN: restoring `nearest_hit`'s `|dir|² < 1e-9` guard → **RED** on the first
    /// assertion. Restoring `nearest_hit_t`'s `|dir|² < 1e-6` guard → GREEN here (wrong scan) but
    /// **RED** on `the_three_scans_agree_on_short_segments` in `collision.rs`.
    #[test]
    fn a_swim_descent_shorter_than_the_old_sweep_would_answer_is_still_floor_bounded() {
        let c = pool();
        // 0.001 u/s at 60 Hz ⇒ want ≈ 1.67e-5: under the OLD 1e-9 squared-length guard, above
        // MIN_RAY_LEN. This ray is the whole reason the guards had to be reconciled.
        let want = 0.001 * (1.0 / 60.0);
        assert!(want * want < 1e-9 && want > eqoxide_nav::collision::MIN_RAY_LEN,
            "positive control: this ray is shorter than the OLD sweep would answer, longer than MIN_RAY_LEN");
        assert!(c.nearest_hit([0.0, 0.0, POOL_FLOOR_Z], [0.0, 0.0, POOL_FLOOR_Z - want]).is_some(),
            "the sweep must answer a ray this short with the floor flush at its origin — it is what \
             the deleted clamp used to cover");

        let mut ctrl = CharacterController::new([0.0, 0.0, POOL_FLOOR_Z + 1e-6]);
        let mut lowest = f32::MAX;
        for _ in 0..2000 { ctrl.step(dive(-0.001), 1.0 / 60.0, &c); lowest = lowest.min(ctrl.pos[2]); }
        assert!(lowest >= POOL_FLOOR_Z - 1e-3,
            "a descent below the OLD sweep's answerable length must still be floor-bounded: \
             reached z={lowest} under floor {POOL_FLOOR_Z}");
    }

    /// Negative control for the two tests above: the floor bound must not have welded the swimmer to
    /// the bottom. Started well ABOVE it, the same driven dive still descends — so a GREEN result
    /// there means "stopped at the floor", not "never moved".
    #[test]
    fn the_floor_bound_does_not_stop_a_swim_descent_that_has_water_below_it() {
        let c = pool();
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        for _ in 0..60 { ctrl.step(dive(-10.0), 1.0 / 60.0, &c); }
        assert!(ctrl.pos[2] < -8.0, "the dive must actually descend in open water, got z={}", ctrl.pos[2]);
        assert!(ctrl.pos[2] >= POOL_FLOOR_Z - 1e-3, "…and still not pass the floor, got z={}", ctrl.pos[2]);
    }

    /// **#855 — THE REAL-GEOMETRY CONTROL.** Round-1 review measured that the synthetic pool above,
    /// green throughout, sat on top of 698 fall-throughs in 4116 driven descents on real baked
    /// geometry (PR comment 5200310297, "this PR" row). This is that corpus, kept as a test so the
    /// number can be re-taken rather than quoted.
    ///
    /// It is a control for TWO things the synthetic pool cannot supply, and only the second is
    /// about coordinates. First, real floors are tilted, adjacent and re-triangulated, so a floor-z
    /// query returns a reconstructed `f32` that lands either side of the plane — the mechanism
    /// `collision::contact_tol` documents, which fires at the origin too. Second, real coordinates
    /// are in the thousands, which sets how coarse that reconstruction is. Round-1 review proposed
    /// the coordinate as the whole story; measurement says it is the multiplier, not the cause (see
    /// `collision::contact_tol`, and the three-regime table on
    /// `collision::tests::a_floor_z_the_module_just_reported_always_has_a_floor_under_it`).
    ///
    /// This drives the same dive against baked zone geometry with the zone's real `.wtr` region
    /// map: sample columns whose floor is genuinely submerged, place the feet at a sweep of small
    /// offsets above the floor `Collision` itself reports for that column, dive at the 35 u/s cap,
    /// and count the runs that end more than 0.5 u **below their own starting floor** without
    /// drifting more than 0.25 u horizontally — i.e. went THROUGH, rather than off an edge.
    ///
    /// `#[ignore]`d and env-gated because the baked assets are a local cache, not repo content.
    /// Set `EQOXIDE_ZONE_ASSETS` to a directory holding `<zone>.glb` and `maps/water/<zone>.wtr`;
    /// optionally `EQOXIDE_CORPUS_ZONES` (comma-separated) and `EQOXIDE_CORPUS_COLUMNS`.
    /// Measured results for this corpus are recorded on PR #866; re-take them, do not quote them
    /// from here, since the corpus depends on which zones the runner has baked.
    #[test]
    #[ignore = "real-geometry corpus: set $EQOXIDE_ZONE_ASSETS to a local baked-asset models dir"]
    fn a_driven_swim_descent_never_passes_a_real_zone_floor() {
        let root = std::path::PathBuf::from(std::env::var("EQOXIDE_ZONE_ASSETS").expect(
            "set EQOXIDE_ZONE_ASSETS to a dir holding <zone>.glb and maps/water/<zone>.wtr"));
        let zones = std::env::var("EQOXIDE_CORPUS_ZONES").unwrap_or_else(|_| {
            "tox,qeynos,qeynos2,erudnext,oasis,lakerathe,everfrost,blackburrow,\
             butcher,ecommons,freportn,gfaydark,innothule,misty".into()
        });
        let per_zone: usize = std::env::var("EQOXIDE_CORPUS_COLUMNS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(40);

        let offsets = [0.0f32, 1e-5, 1e-4, 5e-4, 0.01, SKIN];
        let dts = [1.0f32 / 60.0, 0.05];
        let mut runs = 0usize;
        let mut through = 0usize;
        let mut by_offset = [0usize; 6];
        let mut cols_used = 0usize;
        let mut zones_used = 0usize;
        let mut worst = (0.0f32, String::new(), 0.0f32, 0.0f32, [0.0f32; 3]);

        for zone in zones.split(',').map(str::trim).filter(|z| !z.is_empty()) {
            let glb = root.join(format!("{zone}.glb"));
            let Ok(za) = ZoneAssets::from_glb(&glb) else { continue };
            let Ok(w) = crate::region_map::RegionMap::try_load(&root.join("maps").join("water"), zone)
                else { continue };
            let mut c = Collision::build(&za, 32.0);
            c.set_water(Some(std::sync::Arc::new(w)));

            // Candidate columns from the terrain vertices themselves (libeq pos = [north, height,
            // east]), deduped onto an 8 u grid so we sample places rather than triangles.
            zones_used += 1;
            let mut seen = std::collections::HashSet::new();
            let mut picked = 0usize;
            'cols: for m in za.terrain.iter() {
                for p in m.positions.iter().step_by(17) {
                    let (e, n) = (p[2], p[0]);
                    if !seen.insert(((e / 8.0) as i32, (n / 8.0) as i32)) { continue; }
                    let Some(fz) = c.ground_below(e, n, p[1] + 2.0, 400.0) else { continue };
                    if !c.in_water([e, n, fz + 0.5]) { continue }
                    let Some(surf) = c.water_surface([e, n, fz + 0.5]) else { continue };
                    if surf < fz + 4.0 { continue } // need real depth, not a puddle
                    cols_used += 1;
                    picked += 1;
                    for (oi, &off) in offsets.iter().enumerate() {
                        for &dt in dts.iter() {
                            runs += 1;
                            let mut ctrl = CharacterController::new([e, n, fz + off]);
                            for _ in 0..200 {
                                ctrl.step(dive(-35.0), dt, &c);
                                if ctrl.pos[2] < fz - 100.0 { break; } // runaway: depth is the story
                            }
                            let drift = ((ctrl.pos[0] - e).powi(2) + (ctrl.pos[1] - n).powi(2)).sqrt();
                            let depth = fz - ctrl.pos[2];
                            if drift <= 0.25 && depth > 0.5 {
                                through += 1;
                                by_offset[oi] += 1;
                                if depth > worst.0 {
                                    worst = (depth, zone.to_string(), dt, off, [e, n, fz]);
                                }
                            }
                        }
                    }
                    if picked >= per_zone { break 'cols; }
                }
            }
        }

        // Printed on SUCCESS too, so the corpus this run actually covered is re-takeable with
        // `-- --ignored --nocapture` rather than quoted from a doc (the assets are a local cache,
        // so the size is a property of the runner, not of this branch).
        println!("#855 corpus: {zones_used} zones, {cols_used} submerged columns, {runs} runs, \
                  {through} through the floor");
        assert!(runs > 0, "no corpus: EQOXIDE_ZONE_ASSETS found no zone with a submerged floor \
                           (checked {zones}) — this test cannot pass vacuously");
        assert_eq!(through, 0,
            "{through}/{runs} driven swim descents over {cols_used} real submerged columns ended \
             below their own floor; by start-offset {offsets:?} = {by_offset:?}; worst {:.3} u \
             below floor in {} at dt={} offset={} col={:?}",
            worst.0, worst.1, worst.2, worst.3, worst.4);
    }

    #[test]
    fn falls_normally_in_air_without_water() {
        // Regression guard: with no water map, an airborne controller still falls under gravity.
        let c = col(vec![floor(0.0, -100.0, 100.0)]);
        let mut ctrl = CharacterController::new([0.0, 0.0, 50.0]);
        ctrl.on_ground = false;
        ctrl.step(walk(0.0, [0.0, 0.0]), 0.1, &c);
        assert!(ctrl.pos[2] < 50.0 && ctrl.vel_z < 0.0, "should fall under gravity: z={} vz={}", ctrl.pos[2], ctrl.vel_z);
    }

    /// #529 Levitate: a levitating self-player has gravity OFF — it HOVERS at altitude and
    /// free-floats over land instead of falling, while a normal (non-levitate) character falls
    /// exactly as before. MUTATION-CHECK: delete the `else if self.levitating` hover branch in
    /// `step` (so gravity applies unconditionally) and the levitating assertion goes RED — the
    /// hovering character falls all the way to the floor.
    #[test]
    fn levitating_player_hovers_instead_of_falling() {
        // Floor at z=0; character suspended 50u above it with no horizontal input.
        let c = col(vec![floor(0.0, -100.0, 100.0)]);
        let start = [0.0, 0.0, 50.0];

        // Levitating: gravity off → holds altitude (hovers), never sinks to the floor 50u below.
        let mut lev = CharacterController::new(start);
        lev.set_levitating(true);
        for _ in 0..600 { lev.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &c); }
        assert!((lev.pos[2] - 50.0).abs() < 0.5,
            "levitating player must hover at altitude (~50), got {}", lev.pos[2]);
        assert!(!lev.on_ground, "hovering over ground 50u below is not grounded");
        assert_eq!(lev.vel_z, 0.0, "no fall velocity accumulates while levitating");

        // Control: NOT levitating (default) → normal gravity pulls it down onto the floor. This is
        // the byte-identical baseline the fix must not disturb.
        let mut fall = CharacterController::new(start);
        for _ in 0..600 { fall.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &c); }
        assert!((fall.pos[2] - 0.0).abs() < 0.5,
            "non-levitating player must fall to the floor (~0), got {}", fall.pos[2]);
        assert!(fall.on_ground, "landed on the floor");
    }

    /// #529 nav-awareness: a levitating player driven horizontally off a ledge GLIDES over the gap
    /// at hover height instead of dropping onto the lower floor below — the controller no longer
    /// floor-snaps/falls it, so it traverses small gaps/water at altitude. (Having the `/goto`
    /// PLANNER deliberately ROUTE a levitator across otherwise-impassable water is a larger
    /// nav-model change, filed as a Slice-2 follow-up.)
    #[test]
    fn levitating_player_glides_over_a_lower_gap() {
        // High ledge at z=0 over east[-50,0]; a lower floor at z=-40 over east[0,300] (the gap/water
        // bed — a real gap has geometry below, unlike a bottomless void the unstuck net would fight).
        // The lower floor spans the whole ~190u eastward run so neither character ever leaves the
        // floored region (past the floor edge the depenetration net grabs a hoverer to the nearest
        // floor — a separate bottomless-void edge case, not what this test measures).
        let c = col(vec![floor(0.0, -50.0, 0.0), floor(-40.0, 0.0, 300.0)]);

        // Levitating: glides east out over the gap, holding ~ledge height, not dropping to -40.
        let mut lev = CharacterController::new([-10.0, 0.0, 0.0]);
        lev.on_ground = true;
        lev.set_levitating(true);
        for _ in 0..400 { lev.step(walk(30.0, [1.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(lev.pos[0] > 5.0, "levitator should glide east out over the gap, got {}", lev.pos[0]);
        assert!(lev.pos[2] > -5.0,
            "must hover near ledge height (~0) over the gap, not drop to the -40 floor, got {}", lev.pos[2]);

        // Control: a non-levitating walker falls onto the -40 gap floor.
        let mut fall = CharacterController::new([-10.0, 0.0, 0.0]);
        fall.on_ground = true;
        for _ in 0..400 { fall.step(walk(30.0, [1.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(fall.pos[2] < -35.0, "non-levitating walker falls to the -40 gap floor, got {}", fall.pos[2]);
    }

    /// #587 shallow-slope regression — the defect the flat/cliff hover tests missed. A levitator
    /// walked DOWN a GENTLE ramp must HOLD altitude, not track the ground down. The original hover
    /// branch reused the 0.5u DOWNWARD ground-snap tolerance (`f >= foot - GROUND_SNAP_TOL`), so on
    /// any slope whose per-frame descent was < 0.5u — i.e. essentially ALL walkable terrain at run
    /// speed — it snapped the feet to the floor every frame and the levitator followed the hill down,
    /// indistinguishable from walking (live-caught on a Qeynos-Hills ridge: z −1.9 → −7.5). Only a
    /// >0.5u/frame cliff or a true no-floor gap ever held altitude — which is exactly why the earlier
    /// flat-ground + single sharp-ledge tests passed while ordinary sloped terrain was broken.
    /// MUTATION-CHECK: restore `Some(f) if f >= foot - GROUND_SNAP_TOL` (the buggy down-snap) and this
    /// goes RED — the levitator's Z tracks the ramp down to ~−7.5 instead of holding ~−2.5.
    #[test]
    fn levitating_player_holds_altitude_down_a_shallow_slope() {
        // A single planar ramp descending in +east: height 0 at east=-100 down to −10 at east=+100
        // (a 5% grade → ~0.025u/frame vertical at the run speed below, FAR under GROUND_SNAP_TOL's
        // 0.5u). Vertex = [north, height, east] (libeq axes, as in the `floor` helper).
        let ramp = mesh(vec![
            [-100.0, 0.0, -100.0], [100.0, 0.0, -100.0],
            [100.0, -10.0, 100.0], [-100.0, -10.0, 100.0],
        ]);
        let c = col(vec![ramp]);
        // Ramp height at east=-50 is −2.5; start resting there and walk EAST (downhill).
        let start = [-50.0, 0.0, -2.5];

        let mut lev = CharacterController::new(start);
        lev.on_ground = true;
        lev.set_levitating(true);
        for _ in 0..200 { lev.step(walk(30.0, [1.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(lev.pos[0] > 30.0, "levitator should travel well east downhill, got {}", lev.pos[0]);
        assert!(lev.pos[2] > -3.5,
            "levitator must HOLD altitude (~-2.5) down the shallow slope, not track it down; got {}", lev.pos[2]);

        // Control: a non-levitating walker DOES track the ramp down (ground-snapped every frame).
        let mut walker = CharacterController::new(start);
        walker.on_ground = true;
        for _ in 0..200 { walker.step(walk(30.0, [1.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(walker.pos[2] < -6.0,
            "non-levitating walker follows the slope down toward ~-7.5, got {}", walker.pos[2]);
    }

    #[test]
    fn steps_up_a_2u_ledge() {
        // Floor z=0 for east<5, a 2u riser face at east=5, floor z=2 beyond.
        let c = col(vec![floor(0.0, -100.0, 5.0), wall(5.0, 0.0, 2.0), floor(2.0, 5.0, 100.0)]);
        let mut ctrl = CharacterController::new([3.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        ctrl.step(walk(35.0, [1.0, 0.0]), 0.2, &c);
        assert!(ctrl.pos[0] > 5.0, "should have climbed past the ledge edge: {}", ctrl.pos[0]);
        assert!((ctrl.pos[2] - 2.0).abs() < 0.3, "should be standing on the 2u ledge: {}", ctrl.pos[2]);
    }

    // ── #870: a grounded walker must never be handed to the depenetration net by the slide ──────
    //
    // #870 reports a grounded walker ending up beyond a thin barrier it never crossed, at lips
    // >= 3.00, with the mechanism explicitly UNDETERMINED. The mechanism established here is the
    // COMPOSITION of two things, and it is NOT #854:
    //
    //   * #854 is `slide()`'s probe HEIGHTS — `contact_probes()` starts at `Body::foot` = 0.5, so a
    //     face whose top lands in `(feet, feet + 0.5)` is never sampled horizontally. That band is
    //     0.5 u tall and these barriers are 3.0-6.0 u tall, so it cannot reach this case; #870 says
    //     so and this agrees.
    //   * #870 is `slide()`'s ray LENGTH. The ray was cast `|delta|` long while the resolution backs
    //     the body off by `radius/ndot + SKIN` >= 1.05 u. A step that ENDS short of a face never
    //     reaches it, so the body advances in full and comes to rest with its own cylinder
    //     overlapping the face. `is_embedded` then reads the footprint ring — cast at
    //     `Body::ring` = 3.0 above the feet, at radius `PLAYER_RADIUS` = 1.0 — as pierced, and the
    //     depenetration net TELEPORTS the body to a `PUSHOUT_RADII` ring candidate chosen on two
    //     conditions only: the candidate's own footprint is clear, and its column has a floor. The
    //     SEGMENT between the body and the candidate is never tested, which is the structural
    //     reason a landing on the far side of the barrier is representable at all.
    //
    // `Body::ring` = 3.0 is where #870's reported `lip >= 3.00` threshold comes from, and
    // `the_footprint_ring_band_is_what_makes_lips_at_body_ring_different` re-derives it from the
    // field rather than inheriting the number from the issue.
    //
    // What is NOT claimed here: this file does not reproduce #870's specific reported endpoint
    // (east 99.4, z -10). Driving the fixture as the issue states it — 240 runs over 6 lips x 2 far
    // floors x 40 approach phases — produced the net's teleport every time and a far-side landing
    // NONE of the times; on a planar barrier every ring candidate east of the face is correctly
    // refused by `footprint_clear`, and the body is instead dragged sideways along the wall
    // (measured: +99.7 u at a north half-extent of 100, +342.9 u at a half-extent of 1000, both in
    // 900 frames at dt 1/60 — see `slide()`'s doc for which of those two is extent-bounded and
    // which is drive-bounded; this line said "100 u wall" / "2000 u one" until #987 round 2, mixing
    // half-extent and full-span naming in one sentence).
    // That lateral drag is the same falsehood as the crossing — a reported position the body never
    // walked to — and it is what these tests pin. See the PR body for the full disclosure.

    /// The REACH CONTROL for the property test below, and the re-derivation of #870's threshold.
    ///
    /// Two things a green property test cannot tell you on its own: that the bad state exists at
    /// all, and that `is_embedded` is the predicate that sees it. Both are asserted here directly,
    /// against a hand-placed body rather than a driven one — so the property test cannot be passing
    /// because the harness is blind. The threshold is read off `Body::ring`, NOT off the issue.
    #[test]
    fn the_footprint_ring_band_is_what_makes_lips_at_body_ring_different() {
        let ring = crate::traversability::PLAYER_BODY.ring;
        let radius = crate::traversability::PLAYER_BODY.radius;
        // A body 0.75 u from the face — inside its own collision radius of it, which is the state
        // the un-extended ray used to leave behind.
        let inside = -0.75_f32;
        assert!(inside.abs() < radius, "the control must place the body inside the ring radius");
        for &(lip, want_embedded) in &[
            (ring - 0.5, false), (ring - 0.05, false), (ring + 0.05, true), (ring + 3.0, true),
        ] {
            let c = col(vec![floor(0.0, -100.0, 0.0), wall(0.0, 0.0, lip), floor(-10.0, 0.0, 100.0)]);
            assert_eq!(is_embedded(&c, [inside, 0.0, 0.0]), want_embedded,
                "a body {inside} from the face of a {lip}-tall barrier: is_embedded should be \
                 {want_embedded} (Body::ring = {ring})");
        }
        // And the same body one back-off out is NOT embedded at any of those lips — which is the
        // clearance the fix makes `slide` guarantee.
        for lip in [ring - 0.5, ring + 0.05, ring + 3.0] {
            let c = col(vec![floor(0.0, -100.0, 0.0), wall(0.0, 0.0, lip), floor(-10.0, 0.0, 100.0)]);
            assert!(!is_embedded(&c, [-(radius + SKIN), 0.0, 0.0]),
                "a body backed off by radius+SKIN from a {lip}-tall barrier must not read embedded");
        }
    }

    /// **THE UNIVERSAL (#870).** Over a grid of barrier heights, far-floor depths, speeds, frame
    /// times and approach phases, a grounded walker driven straight at a barrier taller than
    /// `STEP_UP`:
    ///
    ///   * never ends up on the far side of it (the reported failure),
    ///   * never moves sideways (the net's teleport is lateral, and the drive has no lateral
    ///     component — any north displacement at all is motion the body did not make),
    ///   * and, the invariant that makes both of those true rather than merely observed, is never
    ///     classified `is_embedded` on ANY frame. That is the real claim: the depenetration net is
    ///     a recovery for bodies stuck IN geometry, and a body walking into a wall is not one.
    ///
    /// A driven run cannot discharge a universal, and this does not pretend to: it is a grid, and
    /// the parameters it sweeps are named above. What makes the frame-by-frame `is_embedded` check
    /// worth more than the endpoint checks is that it fails at the FIRST frame the body enters the
    /// band, before any teleport has had to be lucky enough to land somewhere visible.
    #[test]
    fn a_grounded_walk_at_a_barrier_never_enters_the_depenetration_net() {
        let radius = crate::traversability::PLAYER_BODY.radius;
        let mut runs = 0_usize;
        let mut band_frames = 0_usize;
        // #933 — per-run reach control, not a global sum. Re-measured on this branch in the #987
        // round-2 review, on the 2.51-lip grid below: all 1134 runs settle for between 343 and 392
        // of their 400 frames. `MIN_RUN_BAND_FRAMES` = 300 sits under that observed floor with
        // headroom but rejects the failure mode `band_frames > runs` could not — TWO settled
        // frames per run (an average of 2/400) passes the old control, 2268 > 1134, and fails this
        // one on the FIRST run. It has to be two, not one: 1 × 1134 = 1134 and the old control is
        // a STRICT `>`, so one frame per run already fails it.
        const MIN_RUN_BAND_FRAMES: usize = 300;
        // ⚠️ **The lips start at 2.51, and the gap below it is a DIFFERENT, STILL-OPEN bug —
        // tracked by #917, which is OPEN.** Its FAMILY is #854 (probe heights), but #854 is CLOSED
        // as completed, so it is not where this band's fix is owed; cite it for the mechanism and
        // #917 for the work.
        // A lip in `(STEP_UP, STEP_UP + Body::foot]` = (2.00, 2.50] is passable, and #870's fix
        // does not touch it: `try_step_up` raises the body by `STEP_UP` and sweeps again, and that
        // raised sweep's own foot ray sits `Body::foot` = 0.5 ABOVE the raised feet, so a lip
        // topping out inside that half-unit is invisible to it — #854's blind band, one storey up.
        // MEASURED at lip 2.40, 20 u/s, 60 Hz: east −1.00 → −0.67 → −0.33 → 0.00 → +0.33 …, a
        // smooth 0.333 u/frame walk THROUGH the barrier, `is_embedded` false the whole way — and
        // byte-identical on unmodified `main` and on this branch, which is what makes it a
        // separate bug and not a regression. It is excluded here rather than fixed because the fix
        // is a probe-HEIGHT one — #854's family, owed on #917 — not this one's (ray length), and
        // asserting a barrier that low holds would be asserting something no code in this PR makes
        // true. It is also a LATERAL
        // teleport, not merely a pass-through: at lip 2.5000 under a due-EAST drive the body ends
        // at north 99.72–99.95 across the four speed/dt pairs
        // `the_blind_step_up_band_is_closed_at_its_upper_bound` uses — ~99.7 u of displacement the
        // body never made, which is the same wrong-position class #870 is about, in the band this
        // block hands off to #917.
        //
        // #931: the true boundary is 2.50 — `the_blind_step_up_band_is_closed_at_its_upper_bound`
        // below pins 2.5000 as still passable and 2.5001 as already blocking. So 2.51 is NOT the
        // lowest lip that blocks; it is the lowest TWO-DECIMAL lip above the boundary, and the
        // grid still carries margin above the true edge — 0.01 u, down from the previous 0.10, not
        // gone. MEASURED (#987 round 2, 600 frames, the same four speed/dt pairs): 2.5001, 2.51,
        // 2.55, 2.59 and 2.60 all rest at east −1.0500 with `is_embedded` never firing, so every
        // one of those would have served as the grid's first lip.
        for lip in [2.51_f32, 2.80, 2.95, 3.00, 3.05, 3.50, 4.00, 6.00, 12.00] {
            for far in [0.0_f32, -10.0] {
                for speed in [20.0_f32, 35.0, 44.0] {
                    for dt in [1.0_f32 / 60.0, 1.0 / 30.0, 1.0 / 20.0] {
                        for k in 0..7 {
                            // Phase: where in the step the body first comes within a step of the
                            // face. This is the parameter the un-extended ray was sensitive to.
                            let start = -20.0 - (k as f32) * 0.017;
                            let c = col(vec![floor(0.0, -100.0, 0.0), wall(0.0, 0.0, lip),
                                             floor(far, 0.0, 100.0)]);
                            let mut ctrl = CharacterController::new([start, 0.0, 0.0]);
                            ctrl.on_ground = true;
                            runs += 1;
                            let mut run_band_frames = 0_usize;
                            for f in 0..400 {
                                assert!(!is_embedded(&c, ctrl.pos),
                                    "lip {lip} far {far} speed {speed} dt {dt} k {k} frame {f}: \
                                     a walker at a barrier was handed to the depenetration net at \
                                     {:?} (#870)", ctrl.pos);
                                ctrl.step(walk(speed, [1.0, 0.0]), dt, &c);
                                // `-radius` is where the cylinder's east face touches the barrier;
                                // 1e-4 is float slack on a ~20 u accumulated walk, not tolerance
                                // for penetration (the failure this pins overshoots by ~1 u).
                                assert!(ctrl.pos[0] <= -radius + 1e-4,
                                    "lip {lip} far {far} speed {speed} dt {dt} k {k} frame {f}: \
                                     east {} is inside/through a barrier at east 0 (#870)",
                                    ctrl.pos[0]);
                                assert!(ctrl.pos[1].abs() < 1e-3,
                                    "lip {lip} far {far} speed {speed} dt {dt} k {k} frame {f}: \
                                     north {} — the drive is due east, so this is displacement the \
                                     body never made (#870)", ctrl.pos[1]);
                                if ctrl.pos[0] > -(radius + SKIN) - 1e-3 { band_frames += 1; run_band_frames += 1; }
                            }
                            // #933: classify THIS run, not just the corpus total — a run that
                            // barely touches the band cannot hide behind others that settle solidly.
                            assert!(run_band_frames >= MIN_RUN_BAND_FRAMES,
                                "lip {lip} far {far} speed {speed} dt {dt} k {k}: only \
                                 {run_band_frames}/400 settled frames (want >= {MIN_RUN_BAND_FRAMES}) \
                                 — this run did not reach and rest against the barrier (#870/#933)");
                        }
                    }
                }
            }
        }
        // Reach control for the LOOP itself (the corpus is an item too): a grid that silently
        // visited nothing, or fewer combinations than intended, would also be green otherwise.
        assert_eq!(runs, 9 * 2 * 3 * 3 * 7, "the grid must actually run every combination");
        assert!(band_frames > runs, "every run must reach the barrier and rest against it; \
                                     only {band_frames} settled frames over {runs} runs");
    }

    /// #932 — the fixture for `slide()`'s doc-comment figures. `ce1d89f` (pre-#870) drove this exact
    /// shape — a grounded walker into a 3.0-tall barrier, due east, 35 u/s, 900 frames (15 s) at
    /// dt 1/60 — and measured +99.7 u of lateral drift at a north half-extent of 100 and +342.9 u
    /// at a half-extent of 1000.
    ///
    /// **Those two figures do not have the same bound, and `slide()`'s doc carries the full
    /// derivation.** The drag is bounded by whichever is smaller, the half-extent or the drive:
    /// +99.7 is extent-bounded (half-extent 100 caps at 99.717377 at 900 frames AND at 1800),
    /// while +342.9 is DRIVE-bounded (342.867859 at 900 frames at half-extents 1000, 2000 and 4000
    /// alike; 698.348450 when the drive doubles to 1800 frames). Re-derived in the #987 round-2
    /// review by wrapping `slide()`'s look-ahead back off; neither figure is reproducible here,
    /// because the bug they measured is gone.
    ///
    /// What IS reproducible on THIS branch, pinned below at both half-extents: zero drift. The
    /// property `a_grounded_walk_at_a_barrier_never_enters_the_depenetration_net` already pins the
    /// half-extent 100 case; this extends the same drive to half-extent 1000 so the doc comment has
    /// a real, current-branch number for the long wall too, instead of restating `ce1d89f`'s.
    ///
    /// Both legs are named by their HALF-extent — the convention `wall()`, `wall_ext` and
    /// `residual_clearance` use throughout this file. A reader who set `half_extent = 2000` would
    /// be building a different fixture from the one that produced +342.9.
    #[test]
    fn a_grounded_walk_never_drifts_on_a_short_or_a_1000u_extent_wall() {
        for half_extent in [100.0_f32, 1000.0] {
            let c = col(vec![
                floor_ext(0.0, -100.0, 0.0, half_extent),
                wall_ext(0.0, 0.0, 3.0, half_extent),
                floor_ext(0.0, 0.0, 100.0, half_extent),
            ]);
            let mut ctrl = CharacterController::new([-20.0, 0.0, 0.0]);
            ctrl.on_ground = true;
            let mut max_drift = 0.0_f32;
            for f in 0..900 {
                assert!(!is_embedded(&c, ctrl.pos),
                    "half_extent {half_extent} frame {f}: a walker at a barrier was handed to the \
                     depenetration net at {:?} (#870/#932)", ctrl.pos);
                ctrl.step(walk(35.0, [1.0, 0.0]), 1.0 / 60.0, &c);
                max_drift = max_drift.max(ctrl.pos[1].abs());
            }
            assert!(max_drift < 1e-3,
                "half_extent {half_extent}: {max_drift:.4} u of north drift under a due-east drive \
                 — ce1d89f measured +99.7 u at half-extent 100 (extent-bounded) / +342.9 u at \
                 half-extent 1000 (drive-bounded) here; #870 must measure ~0 at either (#932)");
        }
    }

    /// #870's example, pinned: the drop shape it names (3.0 lip, floor beyond at -10). The walker
    /// stops dead one back-off short of the face, stays on the line it was driven along, keeps its
    /// z, stays grounded — and reports no hold, because it is not held: it is standing at a wall.
    #[test]
    fn a_grounded_walker_stops_at_a_3u_barrier_with_a_10u_drop_beyond() {
        let radius = crate::traversability::PLAYER_BODY.radius;
        let c = col(vec![floor(0.0, -100.0, 0.0), wall(0.0, 0.0, 3.0), floor(-10.0, 0.0, 100.0)]);
        let mut ctrl = CharacterController::new([-20.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        for _ in 0..900 { ctrl.step(walk(35.0, [1.0, 0.0]), 1.0 / 60.0, &c); }
        assert!((ctrl.pos[0] - -(radius + SKIN)).abs() < 1e-3,
            "should rest exactly one back-off short of the face: east={}", ctrl.pos[0]);
        assert!(ctrl.pos[1].abs() < 1e-3, "no lateral drift: north={}", ctrl.pos[1]);
        assert!(ctrl.pos[2].abs() < 1e-3, "still on the near floor: z={}", ctrl.pos[2]);
        assert!(ctrl.on_ground, "still grounded");
        assert!(ctrl.hold().is_none(), "standing at a wall is not a hold: {:?}", ctrl.hold());
    }

    /// **The step-landing creep's `radius + SKIN` cap, pinned in BOTH directions (#870, review
    /// round 2).** Round 2's independent review ran a mutant that multiplied the creep's `back_off`
    /// by 10 — reach 10.5 u instead of 1.05 u — through the whole `--lib` suite and it SURVIVED,
    /// green. That mutation lets a body step across a chasm ten times its own diameter and then
    /// report standing on ground it never crossed: a silent wrong-position generator behind a green
    /// suite, which this repo ranks above crashes. Nothing pinned the cap because every existing
    /// #870 test asks whether a body climbs, never how far past the riser it is willing to look.
    ///
    /// This asks exactly that, and it is a bracket, not a one-sided assertion — which is what makes
    /// it fail under a WIDENED cap and under a NARROWED one:
    ///
    ///   * a gap of 1.38 u past the riser face must still mount, AND must land at east 1.3833.
    ///     Halve the cap and the reachable landings stop at 0.8583, so this row stops mounting.
    ///   * a gap of 1.39 u and everything above it must NOT mount at all. Multiply the cap by 10
    ///     and the first creep sample lands at 1.6458, so every one of these rows mounts.
    ///
    /// A third mutant was run and it is the reason the east column exists at all: changing
    /// `STEP_LANDING_CREEP_SAMPLES` from 8 to 4 leaves the CAP intact and so SURVIVED the
    /// mount/refuse rows alone, green — a draft of this comment asserted, by reasoning rather than
    /// measurement, that any change to either factor would be caught. It is not; the coarser
    /// stride happens to reach the same 1.3833 ceiling. Pinning the measured east of each landing
    /// closes it (8 samples put the 0.34 row at 0.4646, 4 samples at 0.5958).
    ///
    /// A refusal is checked POSITIVELY too — the body must be left on the near floor, grounded,
    /// never `is_embedded` on any frame and never laterally displaced — so a mutant that turns a
    /// refusal into a depenetration teleport cannot pass by merely failing to reach z.
    #[test]
    fn the_step_landing_creep_reaches_one_back_off_past_the_riser_and_no_further() {
        let radius = crate::traversability::PLAYER_BODY.radius;
        // A 2.0 riser at east 0 whose tread only BEGINS `gap` east of the face: the centre probe
        // lands in the slot, so only the creep can find the tread.
        let run = |gap: f32| -> Option<[f32; 3]> {
            let c = col(vec![floor(0.0, -100.0, 0.0), wall(0.0, 0.0, 2.0), floor(2.0, gap, 100.0)]);
            let mut ctrl = CharacterController::new([-20.0, 0.0, 0.0]);
            ctrl.on_ground = true;
            for _ in 0..600 {
                ctrl.step(walk(20.0, [1.0, 0.0]), 1.0 / 60.0, &c);
                assert!(!is_embedded(&c, ctrl.pos),
                    "gap {gap}: the depenetration net must never be involved here — {:?}", ctrl.pos);
                assert!(ctrl.pos[1].abs() < 1e-3,
                    "gap {gap}: north {} — the drive is due east", ctrl.pos[1]);
                if ctrl.pos[2] > 1.5 { return Some(ctrl.pos); }
            }
            None
        };
        // Reach control: the fixture must be climbable at all, or every "refused" row below is
        // vacuous. Gap 0 is a plain 2.0 step and must mount.
        assert!(run(0.0).is_some(), "the fixture is not climbable even with no tread gap");
        // MEASURED landings. The east column is what pins the STRIDE as well as the cap: the
        // samples sit at `base + i*(radius + SKIN)/STEP_LANDING_CREEP_SAMPLES` for `i` in 1..=8, so
        // halving or doubling the sample count moves the 0.34 row (measured: 8 samples → 0.4646,
        // 4 samples → 0.5958) even where it leaves the coarser rows alone.
        for &(gap, want_east) in &[(0.20_f32, 0.3333_f32), (0.34, 0.4646), (0.80, 0.8583),
                                   (1.20, 1.2521), (1.38, 1.3833)] {
            let p = run(gap).unwrap_or_else(|| panic!(
                "gap {gap} must still mount — the creep reaches radius+SKIN = {} past the face; \
                 a NARROWED cap fails here", radius + SKIN));
            assert!((p[2] - 2.0).abs() < 1e-3, "gap {gap}: mounted to z {} not the 2.0 tread", p[2]);
            assert!((p[0] - want_east).abs() < 2e-3,
                "gap {gap}: landed at east {} not the measured {want_east} — the creep's stride \
                 (radius+SKIN)/STEP_LANDING_CREEP_SAMPLES has moved (#870)", p[0]);
        }
        // THE CAP. The deepest landing the creep can ever reach on this fixture is one whole
        // `radius + SKIN` past the base it starts from, in `STEP_LANDING_CREEP_SAMPLES` steps of
        // (radius+SKIN)/8 = 0.13125. Measured base on this fixture: east 0.3333.
        let deepest = run(1.38).expect("gap 1.38 must mount");
        assert!((deepest[0] - 1.3833).abs() < 2e-3,
            "the creep's deepest landing must be east 1.3833 = 0.3333 + (radius+SKIN) = 0.3333 + \
             {}; got {} — the cap has moved (#870)", radius + SKIN, deepest[0]);
        // And one stride further out is unreachable, at every gap above it.
        for gap in [1.39_f32, 1.50, 2.00, 4.00, 10.00] {
            assert!(run(gap).is_none(),
                "gap {gap} must NOT mount — it is more than radius+SKIN = {} past the face; \
                 a WIDENED cap fails here, and a body that mounts it reports standing on ground \
                 it never crossed (#870)", radius + SKIN);
        }
        // A refusal must leave the body on the NEAR floor and grounded — not somewhere odd, and in
        // particular not on the far tread by some other route. Deliberately NOT asserted here: the
        // east it rests at. This riser is exactly `STEP_UP` tall, which is BELOW `Body::ring` = 3.0,
        // so `is_embedded` structurally cannot see it and the body ends flush against the face
        // (measured: east 5.4e-7). Whether that is right is a probe-height question — #854's family,
        // open on #917 — not #870's ray length, and asserting a back-off here would be asserting
        // something no code in this PR makes true.
        let c = col(vec![floor(0.0, -100.0, 0.0), wall(0.0, 0.0, 2.0), floor(2.0, 4.0, 100.0)]);
        let mut ctrl = CharacterController::new([-20.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        for _ in 0..600 { ctrl.step(walk(20.0, [1.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(ctrl.pos[2].abs() < 1e-3, "a refused creep stays on the near floor: z={}", ctrl.pos[2]);
        assert!(ctrl.on_ground, "still grounded");
        assert!(ctrl.pos[1].abs() < 1e-3, "no lateral drift: north={}", ctrl.pos[1]);
    }

    #[test]
    fn blocked_by_a_3u_wall() {
        let c = col(vec![floor(0.0, -100.0, 100.0), wall(5.0, 0.0, 3.0)]);
        let mut ctrl = CharacterController::new([3.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        ctrl.step(walk(35.0, [1.0, 0.0]), 0.2, &c);
        assert!(ctrl.pos[0] < 4.1, "a 3u wall must block (no step-up): east={}", ctrl.pos[0]);
        assert!((ctrl.pos[2] - 0.0).abs() < 0.3, "should stay at floor z=0: {}", ctrl.pos[2]);
    }

    #[test]
    fn nav_does_not_scale_a_lip_taller_than_the_native_step() {
        // A 6u lip: floor z=0, a 6u riser at east=5, floor z=6 beyond. #239: nav must move like a
        // WASD player — the native 2u step-up can't mount a 6u riser, and the old NAV_CLIMB=20
        // super-step is gone — so nav is blocked at the lip exactly like WASD. (find_path now routes
        // AROUND such lips: its feet-level path_clear rejects the >2.5u riser; a THIN fence with flat
        // floor on both sides is crossed by `hop`, not climb — see the hop test below.)
        let geo = || col(vec![floor(0.0, -100.0, 5.0), wall(5.0, 0.0, 6.0), floor(6.0, 5.0, 100.0)]);

        // Free WASD (climb=0 → native 2u step): blocked at the lip, stays at z=0.
        let mut wasd = CharacterController::new([3.0, 0.0, 0.0]);
        wasd.on_ground = true;
        for _ in 0..5 { wasd.step(walk(35.0, [1.0, 0.0]), 0.1, &geo()); }
        assert!(wasd.pos[0] < 5.1, "WASD must NOT scale a 6u lip: east={}", wasd.pos[0]);
        assert!(wasd.pos[2] < 1.0, "WASD should stay at floor z=0: {}", wasd.pos[2]);

        // Nav is now capped at the same native step-up (no NAV_CLIMB): also blocked, also at z=0.
        // climb is set high (not 0) deliberately: intent.climb is now ignored entirely (see
        // `let _ = intent.climb;` in step()), but the WASD and nav intents used to be byte-identical
        // here (both climb: 0.0), so re-introducing the old NAV_CLIMB super-step (`if intent.climb >
        // 0 { climb up to intent.climb }`) would NOT have been caught by this test. Setting climb
        // to a value that WOULD scale the lip if honored makes the test an actual regression guard.
        let nav_intent = MoveIntent { wish_dir: [1.0, 0.0], wish_vspeed: 0.0, jump: false,
            want_swim: false, speed: 35.0, climb: 20.0, hop: false };
        let mut nav = CharacterController::new([3.0, 0.0, 0.0]);
        nav.on_ground = true;
        for _ in 0..5 { nav.step(nav_intent, 0.1, &geo()); }
        assert!(nav.pos[0] < 5.1, "nav must NOT scale a 6u lip either (#239): east={}", nav.pos[0]);
        assert!(nav.pos[2] < 1.0, "nav should stay at floor z=0: {}", nav.pos[2]);
    }

    #[test]
    fn nav_hops_a_thin_fence_with_flat_floor_both_sides() {
        // The Halas sled-pen case (#41): a thin upright fence (z=0..5) with FLAT floor z=0 on both
        // sides — step-up can't cross it (no higher floor to step onto), only a jump-over works.
        let geo = || col(vec![floor(0.0, -100.0, 100.0), wall(5.0, 0.0, 5.0)]);

        // Free WASD (allow_hop=false): blocked at the fence, never crosses.
        let mut wasd = CharacterController::new([2.0, 0.0, 0.0]);
        wasd.on_ground = true;
        for _ in 0..40 { wasd.step(walk(35.0, [1.0, 0.0]), 0.05, &geo()); }
        assert!(wasd.pos[0] < 5.0, "WASD must NOT cross the fence: east={}", wasd.pos[0]);

        // Nav with hop commanded: hops the fence and lands on the flat floor beyond (z≈0, east>5).
        let nav_intent = MoveIntent { wish_dir: [1.0, 0.0], wish_vspeed: 0.0, jump: false,
            want_swim: false, speed: 35.0, climb: 0.0, hop: true };
        let mut nav = CharacterController::new([2.0, 0.0, 0.0]);
        nav.on_ground = true;
        for _ in 0..40 { nav.step(nav_intent, 0.05, &geo()); }
        assert!(nav.pos[0] > 6.0, "nav should hop past the fence: east={}", nav.pos[0]);
        assert!(nav.pos[2].abs() < 0.5, "nav should land back on the flat floor z=0: {}", nav.pos[2]);
    }

    #[test]
    fn jump_reaches_a_usable_height() {
        // eqoxide#92: a Space jump must clear/mount low ledges (peak well above the 2u step-up),
        // not the old ~0.7u placeholder that "barely leaves the ground".
        let c = col(vec![floor(0.0, -100.0, 100.0)]); // flat ground at z=0
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        let dt = 1.0 / 60.0;
        // Launch (jump only on the first frame — holding it must not re-launch mid-air).
        ctrl.step(MoveIntent { jump: true, ..Default::default() }, dt, &c);
        let mut peak = ctrl.pos[2];
        for _ in 0..180 {
            ctrl.step(MoveIntent::default(), dt, &c);
            peak = peak.max(ctrl.pos[2]);
            if ctrl.on_ground { break; }
        }
        assert!(peak > 3.0, "jump should clear a small ledge (peak > 3u), got {peak}");
        assert!(peak < 6.0, "jump should be a hop, not a launch (peak < 6u), got {peak}");
        assert!(ctrl.pos[2].abs() < 0.6, "should land back on the ground, got z={}", ctrl.pos[2]);
    }

    #[test]
    fn ground_snap_uses_plus_one_origin() {
        // Floor at z=0; feet start 0.5 BELOW it. A foot-origin downward probe could not see the
        // floor above; the +1.0 origin can, so the controller snaps UP onto it.
        let c = col(vec![floor(0.0, -100.0, 100.0)]);
        let mut ctrl = CharacterController::new([0.0, 0.0, -0.5]);
        ctrl.on_ground = true;
        ctrl.step(walk(0.0, [0.0, 0.0]), 0.05, &c);
        assert!((ctrl.pos[2] - 0.0).abs() < 1e-2, "should snap up to floor z=0: {}", ctrl.pos[2]);
    }

    #[test]
    fn depenetrates_embedded_point_to_clear_floor() {
        // Floor everywhere, plus two close walls boxing the origin (footprint pierced).
        let c = col(vec![floor(0.0, -100.0, 100.0), wall(0.8, 0.0, 10.0), wall(-0.8, 0.0, 10.0)]);
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        let handled = ctrl.step(walk(0.0, [0.0, 0.0]), 0.05, &c);
        let _ = handled;
        assert!(c.footprint_clear(ctrl.pos[0], ctrl.pos[1], ctrl.pos[2], PLAYER_RADIUS, 8),
            "after depenetration the footprint must be clear: pos={:?}", ctrl.pos);
        assert!(ctrl.on_ground, "should be grounded on the pushed-out floor");
    }

    // ── #649/#661: the depenetration net must not touch a SWIMMER at all ────────────────────────
    //
    // A body in water that fails `footprint_clear` is NOT "embedded in rock" in the sense the net
    // assumes — a swimmer in a narrow flooded pocket has geometry within a body radius as a matter
    // of course. The net used to recover it with `nearest_floor(up = STEP_UP + GROUND_ORIGIN = 3,
    // down = GROUND_DEPTH = 200)`, which takes whichever floor is NEARER rather than one the body
    // can occupy, and then declared `on_ground = true`. One mechanism, two symptoms, both pinned
    // below: it MOUNTS a swimmer on a slab above it, and it DROPS one onto the pool floor below.
    //
    // #649/#658 kept the net running for swimmers and made the recovery depth-preserving; #661
    // then measured, at the same qcat coordinate, that the remainder was STILL two defects (the
    // dry-candidate beach and the input-eating ping-pong — see `depenetrate`'s door comment), and
    // the fix became: a body afloat in water never enters the net. These tests' assertions are
    // unchanged in what they FORBID (a swimmer teleported vertically / grounded / dried); how the
    // controller satisfies them changed from "the net recovers at own depth" to "the net stays
    // out and physics keeps custody".
    //
    // The scene mirrors the qcat spawn pocket at 1/10 scale: a flooded corridor too narrow for a
    // clear footprint (walls 0.8 u either side vs `PLAYER_RADIUS` 1.0), water to z = 0.5, and — in
    // the first test — a dry slab 2 u overhead, just inside the 3 u upward search.

    /// Water everywhere below `top`; the corridor walls that make the footprint fail.
    fn flooded_corridor(meshes: Vec<MeshData>, bottom: f32, top: f32) -> Collision {
        let mut c = col(meshes);
        c.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::water_slab(bottom, top))));
        c
    }
    fn swim_still() -> MoveIntent {
        MoveIntent { wish_dir: [0.0, 0.0], wish_vspeed: 0.0, jump: false, want_swim: true,
                     speed: 0.0, climb: 0.0, hop: false }
    }

    #[test]
    fn depenetration_never_mounts_a_swimmer_onto_the_slab_above_it() {
        // Pool floor 12 u down, a dry slab 2 u UP (nearer, so the old search preferred it), water to
        // z = 0.5 so the feet at z = 0 are wet and the slab at z = 2 is DRY.
        let c = flooded_corridor(
            vec![floor(-12.0, -100.0, 100.0), floor(2.0, -100.0, 100.0),
                 wall(0.8, -12.0, 10.0), wall(-0.8, -12.0, 10.0)],
            -12.0, 0.5);
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        assert!(!c.footprint_clear(0.0, 0.0, 0.0, PLAYER_RADIUS, 8),
            "fixture: the swimmer's footprint must FAIL here, else the net never runs and this test \
             proves nothing");
        assert!(c.in_water([0.0, 0.0, 0.0]) && !c.in_water([0.0, 0.0, 2.0]),
            "fixture: feet wet at z=0, slab at z=2 dry — that asymmetry is the whole bug");

        ctrl.step(swim_still(), 1.0 / 60.0, &c);

        assert!(ctrl.pos[2].abs() < 1e-3,
            "#649: the push-out may move a swimmer sideways, never VERTICALLY — it was mounted onto \
             the slab at z={:.4} (main lifts it to 2.0, the qcat −55.9687 wedge at 1/10 scale)",
            ctrl.pos[2]);
        assert!(!ctrl.on_ground,
            "#649: a floating body is not standing on anything — `on_ground` must stay false, or the \
             next frame's swim/buoyancy branch never runs again");
        assert!(c.in_water(ctrl.pos),
            "#649: and it must still be IN THE WATER it was swimming in; got {:?}", ctrl.pos);
        // ⚠️ #661 REWROTE THE FOURTH ASSERTION. It used to demand the push-out "resolve the
        // horizontal overlap" (`footprint_clear` at the end position) — i.e. it required the net
        // to ACT on this swimmer. Acting on swimmers is exactly what #661 removed: the ring nudge
        // was cosmetic here (a body between two long parallel walls is in a narrow CANAL, not a
        // trap) and the same machinery, pointed at qcat, was the strand. The replacement pins what
        // actually matters about this "wedged" swimmer: it is not stuck at all — a lateral swim
        // wish moves it freely along the corridor, in water, at its own depth, with no net rescue.
        let mut swim_north = swim_still();
        swim_north.wish_dir = [0.0, 1.0];
        swim_north.speed = 35.0;
        for _ in 0..60 { ctrl.step(swim_north, 1.0 / 60.0, &c); }
        assert!(ctrl.pos[1] > 20.0,
            "a swimmer between close parallel walls is in a canal, not a trap: one second of swim \
             input must carry it well along the corridor (the net must not eat its input); got {:?}",
            ctrl.pos);
        assert!(ctrl.pos[2].abs() < 1e-3 && c.in_water(ctrl.pos) && !ctrl.on_ground,
            "…still at its own depth, wet, unsupported: {:?}", ctrl.pos);
    }

    #[test]
    fn depenetration_never_drops_a_swimmer_to_the_pool_floor() {
        // The same defect pointing DOWN: no slab overhead, so the only floor the search can find is
        // the pool bottom 12 u below — and the old code sank the swimmer onto it and grounded it.
        let c = flooded_corridor(
            vec![floor(-12.0, -100.0, 100.0), wall(0.8, -12.0, 10.0), wall(-0.8, -12.0, 10.0)],
            -12.0, 0.5);
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        assert!(!c.footprint_clear(0.0, 0.0, 0.0, PLAYER_RADIUS, 8), "fixture: footprint must fail");

        ctrl.step(swim_still(), 1.0 / 60.0, &c);

        assert!(ctrl.pos[2].abs() < 1e-3,
            "#649 (the other direction): the swimmer must hold its own depth, not be dropped to the \
             pool floor — got z={:.4} (main sinks it to −12)", ctrl.pos[2]);
        assert!(!ctrl.on_ground, "#649: and it is not standing on the pool floor");
    }

    #[test]
    // #730: capitalised on purpose — DRY vs wet is the load-bearing distinction this test pins
    // (see the "BLAST-RADIUS PIN" comment below); lower-casing it would erase that signal.
    #[allow(non_snake_case)]
    fn depenetration_still_grounds_a_DRY_body_exactly_as_before() {
        // THE BLAST-RADIUS PIN. The net exists because characters genuinely DO get embedded in
        // geometry; over-narrowing it strands them on land — a new bug in the same family. The same
        // scene with NO water must behave exactly as `depenetrates_embedded_point_to_clear_floor`:
        // pushed out AND mounted on the nearest floor AND grounded.
        let c = col(vec![floor(-12.0, -100.0, 100.0), floor(2.0, -100.0, 100.0),
                         wall(0.8, -12.0, 10.0), wall(-0.8, -12.0, 10.0)]);
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        ctrl.step(swim_still(), 1.0 / 60.0, &c);
        assert!((ctrl.pos[2] - 2.0).abs() < 1e-3,
            "a DRY embedded body must still be recovered onto the nearest floor (z=2): {:?}", ctrl.pos);
        assert!(ctrl.on_ground, "…and still be grounded there");
    }

    #[test]
    fn a_swimmer_whose_only_clear_neighbours_are_dry_is_never_beached_by_the_net() {
        // ⚠️ #661 INVERTED THIS TEST. Under the name `depenetration_grounds_a_swimmer_pushed_out_
        // of_the_water_entirely` it PINNED the dry-candidate fall-through: "a body that IS afloat
        // but whose only clear neighbour is OUTSIDE the water takes the ordinary floor recovery,
        // unchanged", asserting the swimmer ends at z=2.0, grounded, dry. That behaviour is the
        // MEASURED writer of the #661 strand: at the qcat spawn pocket the ring push-out's
        // candidate fell a fraction of a unit outside the `.wtr` region's XY extent while the BODY
        // was still afloat in water, the fall-through beached it onto the tile floor 0.009 u above
        // the waterline, and — dry, `on_ground`, `want_swim` inert, nothing solid to sink through —
        // the transition was one-way: the live soft-lock. "The candidate column is dry" never
        // meant "the body left the water"; it usually means the water region's edge is nearby.
        //
        // The same fixture now pins the opposite: the net does not touch a floating body at all
        // (see `depenetrate`'s door), so the swimmer is never beached, stays wet at its own depth,
        // and — the part the old behaviour destroyed — remains fully FUNCTIONAL: swim input still
        // moves it, because no recovery is eating its frames.
        let mut c = col(vec![floor(-12.0, -100.0, 100.0), floor(2.0, -100.0, 100.0),
                             wall(0.8, -12.0, 10.0), wall(-0.8, -12.0, 10.0)]);
        c.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::box_below(-100.0, 100.0, -1.0, 1.0, 0.5))));
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        assert!(c.in_water([0.0, 0.0, 0.0]) && !c.in_water([2.0, 0.0, 0.0]),
            "fixture: afloat at the centre, dry two units east — the exact shape the old \
             fall-through beached");
        assert!(!c.footprint_clear(0.0, 0.0, 0.0, PLAYER_RADIUS, 8),
            "fixture: the dry predicate must call this body embedded, or the door is never tested");

        // Two seconds idle: the old code beached it on frame 1 (z=2.0, grounded, dry).
        for _ in 0..120 { ctrl.step(swim_still(), 1.0 / 60.0, &c); }
        assert!(ctrl.pos[2].abs() < 1e-3 && !ctrl.on_ground && c.in_water(ctrl.pos),
            "#661: a floating body must NEVER be recovered onto dry land — the old code put this \
             one at z=2.0, on_ground, dry, where `want_swim` does nothing and the state is a \
             one-way soft-lock; got {:?} on_ground={}", ctrl.pos, ctrl.on_ground);
        assert!(ctrl.hold().is_none(),
            "…and it is not an emergency either: a swimmer in a narrow water strip is just \
             swimming, not held; got {:?}", ctrl.hold());

        // And it still answers the driver: swim along the strip.
        let mut swim_north = swim_still();
        swim_north.wish_dir = [0.0, 1.0];
        swim_north.speed = 35.0;
        for _ in 0..60 { ctrl.step(swim_north, 1.0 / 60.0, &c); }
        assert!(ctrl.pos[1] > 20.0,
            "swim input must still move the body along the water strip; got {:?}", ctrl.pos);
    }

    #[test]
    fn an_afloat_body_with_no_floor_below_is_never_pushed_out_into_a_drift() {
        // #649 REVIEW FINDING 1 — a REGRESSION PIN, not a fails-on-main pin: `main` passes this too.
        // `is_embedded` counts `floor.is_none()` as embedded, so a swimmer in deep water with a
        // perfectly CLEAR footprint and no floor within GROUND_DEPTH below used to enter the net.
        // The first cut of the #649 fix answered that with an `Afloat` recovery at the first ring
        // candidate — which is *equally* embedded, so the next frame re-entered the net from there
        // and the body walked east one ring radius per frame (60 u/s), ignoring the wish input,
        // reporting a stale `in_water`. A recovery that is itself embedded is not a recovery.
        //
        // Since #661 the protection is structural: a floating body never enters the net at all
        // ("no floor below" is not a state a body that FLOATS can be in an emergency about), so
        // there is no recovery to get wrong. This pin stays: it is the test that catches any
        // future change re-admitting floaters to a net whose recoveries can drift.
        //
        // Geometry: floor only far to the east (so `has_geometry` is true and `ground_below` at the
        // origin is None), water everywhere. Nothing must move.
        let mut c = col(vec![floor(-50.0, 100.0, 200.0)]);
        c.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::water_slab(-1000.0, 10.0))));
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        assert!(c.footprint_clear(0.0, 0.0, 0.0, PLAYER_RADIUS, 8)
                && c.ground_below(0.0, 0.0, 1.0, GROUND_DEPTH).is_none(),
            "fixture: clear footprint but NO floor below — that combination is the whole case");
        assert!(c.in_water([0.0, 0.0, 0.0]), "fixture: and the body is afloat");

        for _ in 0..60 { ctrl.step(swim_still(), 1.0 / 60.0, &c); }

        assert!(hlen([ctrl.pos[0], ctrl.pos[1], 0.0]) < 1e-3,
            "the net must not walk an afloat body across the zone one ring radius at a time — it \
             drifted to {:?} in 60 frames with ZERO wish input (the first cut reached [60,0,0])",
            ctrl.pos);
    }

    // ── #661: the swimming duck-under — the step-up's downward mirror ───────────────────────────

    /// **A swimmer blocked by a hanging face with open water beneath passes UNDER it.**
    ///
    /// The 1/10-scale qcat pocket mouth: a face whose bottom edge sits a hair below the waterline
    /// (z −0.2, surface 0), open water beneath it down to −40. A swimmer at the float plane (−2)
    /// is blocked — its chest ray (feet + 4 = +2) hits the face — and before #661 it had no
    /// downward answer at all: the step-up could not mount (the face runs 20 u up), so it pressed
    /// into the lip for ever. That asymmetry is the issue title: the controller could climb 2.5 u
    /// OUT of water but never pass 2.5 u UNDER an obstruction, so every mount was one-way.
    /// `try_duck_under` dives the feet up to the same 2.5 u envelope and re-slides; here that
    /// clears the face bottom and the swimmer swims through, buoyancy returning it to the plane
    /// on the far side.
    ///
    /// (On the unfixed controller this scene fails twice over: no duck exists, AND the
    /// depenetration net — whose footprint ring probes the AIR band a swimmer's plane puts 1 u
    /// above the waterline — reads the approach as "embedded" and eats the input.)
    #[test]
    fn a_swimmer_ducks_under_a_hanging_face_instead_of_stranding_at_it() {
        let c = flooded_corridor(
            vec![floor(-40.0, -100.0, 100.0), wall(4.0, -0.2, 20.0)],
            -40.0, 0.0);
        let plane = -2.0; // surface 0 − float_depth 2
        let mut ctrl = CharacterController::new([-5.0, 0.0, plane]);
        assert!(c.in_water(ctrl.pos), "fixture: starts afloat at the plane");

        let mut swim_east = swim_still();
        swim_east.wish_dir = [1.0, 0.0];
        swim_east.speed = 35.0;
        for _ in 0..150 { ctrl.step(swim_east, 1.0 / 60.0, &c); }

        assert!(ctrl.pos[0] > 8.0,
            "#661: the swimmer must pass UNDER the hanging face at east=4 (open water below its \
             −0.2 bottom edge) — without the duck it presses into the lip for ever; got {:?}",
            ctrl.pos);
        assert!((ctrl.pos[2] - plane).abs() < 0.1 && c.in_water(ctrl.pos) && !ctrl.on_ground,
            "…and be back on its swim plane on the far side, wet, unsupported: {:?}", ctrl.pos);
    }

    /// **THE #191 CONTROL: the duck must not override a genuine bank haul-out.**
    ///
    /// Same shape, but the face is SOLID to the pool floor — a real bank whose lip (surface
    /// + 0.1) is inside the swimming step-up's 2.5 u reach. `try_duck_under` measures the water
    /// route shut (diving gains no lateral progress against a face that runs to the bottom), so
    /// the haul-out keeps the right of way and the swimmer climbs out exactly as before #661.
    /// This is the asset-free companion to `walker_sim`'s
    /// `p1_haul_out_admission_matches_controller_execution`, which sweeps the full lip-height
    /// contract; disabling the swimming step-up turns BOTH red.
    #[test]
    fn a_swimmer_at_a_solid_bank_still_hauls_out_the_duck_does_not_override_191() {
        let c = flooded_corridor(
            vec![floor(-40.0, -100.0, 4.0), floor(0.1, 4.0, 100.0), wall(4.0, -40.0, 0.1)],
            -40.0, 0.0);
        let mut ctrl = CharacterController::new([0.0, 0.0, -2.0]);
        assert!(c.in_water(ctrl.pos) && !c.in_water([8.0, 0.0, 0.1]),
            "fixture: afloat at the plane; the bank top is dry");

        let mut swim_east = swim_still();
        swim_east.wish_dir = [1.0, 0.0];
        swim_east.speed = 35.0;
        let mut out = false;
        for _ in 0..300 {
            ctrl.step(swim_east, 1.0 / 60.0, &c);
            if ctrl.on_ground && ctrl.pos[0] > 4.0 && (ctrl.pos[2] - 0.1).abs() < 0.6 {
                out = true;
                break;
            }
        }
        assert!(out,
            "#191: a lip 2.1 u above the swim plane (0.1 above the surface) is inside the swimming \
             step-up's reach and must still be mounted — the duck may only win where diving \
             actually gains progress; ended at {:?}", ctrl.pos);
    }

    // ── #661 review: every duck guard is pinned by a RED-when-deleted test ──────────────────────
    //
    // The review measured that three of the duck's four refusal clauses were pinned by nothing
    // (deleting each left the whole tree green), and that the duck itself was a NEW one-way
    // transition over a shallow far shelf — the defect class this fix exists to remove. The tests
    // below each pin one clause; each was verified RED with only its clause deleted and GREEN on
    // the fixed controller. The shared scene is the hanging-face corridor from
    // `a_swimmer_ducks_under_a_hanging_face_instead_of_stranding_at_it`, varied one element at a
    // time.

    /// Drive the controller east with a constant intent for `frames` frames.
    fn drive_east(ctrl: &mut CharacterController, col: &Collision, frames: usize, vspeed: f32) {
        let intent = MoveIntent { wish_dir: [1.0, 0.0], wish_vspeed: vspeed, jump: false,
                                  want_swim: true, speed: 44.0, climb: 0.0, hop: false };
        for _ in 0..frames { ctrl.step(intent, 1.0 / 60.0, col); }
    }

    /// **B1 — the duck must not be a one-way transition: a far side too shallow to dive back out
    /// of is REFUSED.** The reviewer's falsifying scene, adopted as the pin: same lintel, but the
    /// far column's floor sits above the −4.5 duck depth, so a body that crossed could never
    /// re-sink far enough to get its chest back under the lip (measured pre-guard: crossed once,
    /// then converged against the far face for ever, every observable reading "swimming
    /// normally"). `try_duck_under`'s floor bound (`floor ≤ ducked feet`) refuses the crossing;
    /// the body presses at the lip — wet, at its plane, still answering input — which nav sees as
    /// a stall and can replan around.
    ///
    /// Two far-floor values, each killing a different half of the bound (#661 review R2-B2 —
    /// round 2 shipped only −3.0, which sits above `ground_below`'s probe origin (−3.5) and so
    /// pins the `?` while leaving the `<=` refinement pinned by NOTHING; the reviewer measured
    /// the whole lib green with `<=` deleted, and −4.0 crossing into the same permanent trap):
    ///
    ///   * −3.0 — above the probe origin: `ground_below` finds nothing → the `?` refuses;
    ///   * −4.0 — inside the probe band but ABOVE the ducked feet: found, and only
    ///     `floor <= lo[2]` refuses it. Its return sink would clamp ~0.5 u short of the passage
    ///     depth and the chest never gets back under the lip.
    #[test]
    fn a_duck_never_crosses_into_a_column_it_cannot_dive_back_out_of() {
        for far_floor in [-3.0f32, -4.0] {
            let c = flooded_corridor(
                vec![floor(-40.0, -100.0, 4.0), floor(far_floor, 4.0, 100.0), wall(4.0, -0.2, 20.0)],
                -40.0, 0.0);
            // Fixture: the far column must NOT be divable to the passage depth (−4.5) — either no
            // floor within the probe, or a floor above the ducked feet.
            assert!(c.ground_below(8.0, 0.0, -4.5 + 1.0, 200.0).map_or(true, |f| f > -4.5),
                "fixture: the far shelf ({far_floor}) must be shallower than the duck depth, or \
                 this is the round-trip scene");
            let mut ctrl = CharacterController::new([-2.0, 0.0, -2.0]);
            drive_east(&mut ctrl, &c, 240, 0.0);
            assert!(ctrl.pos[0] < 4.0,
                "#661 review B1/R2-B2: the duck must REFUSE a crossing whose far side (floor \
                 {far_floor}) cannot be dived back out of — crossing is a one-way trap (measured \
                 for both values); got {:?}", ctrl.pos);
            assert!(c.in_water(ctrl.pos) && (ctrl.pos[2] - (-2.0)).abs() < 0.3 && !ctrl.on_ground,
                "…and the refused swimmer stays wet at its plane, still a swimmer: {:?}", ctrl.pos);
        }
    }

    /// **R2-B1 — the SURFACE axis of re-divability: a far column whose float plane is beyond the
    /// duck envelope above the passage depth is REFUSED.** The round-2 floor bound closed the
    /// shelf half of the one-way trap; the reviewer then measured the same trap on the surface
    /// axis — floor −40 on BOTH sides (so the floor bound admits) and adjacent water volumes
    /// whose surfaces differ by 2 u: the body crosses under the lintel, buoyancy parks it at the
    /// FAR column's float plane, and the return duck's 2.5 u sink from there can no longer reach
    /// the passage depth. Permanently trapped, `hold()=None`, every observable reading "swimming
    /// normally". Adjacent surfaces at different heights are a first-class modelled case
    /// (`region_map::tests::water_boxes_gives_each_box_its_own_bounded_surface`), not exotica.
    /// `try_duck_under`'s surface bound refuses the crossing; deleting it re-opens the trap and
    /// turns this red.
    #[test]
    fn a_duck_never_crosses_into_a_column_whose_float_plane_is_beyond_the_return_envelope() {
        let mut c = col(vec![floor(-40.0, -100.0, 100.0), wall(4.0, -0.2, 20.0)]);
        c.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::water_boxes(&[
            [-100.0, 100.0, -100.0, 4.0, -40.0, 0.0], // near column: surface 0
            [-100.0, 100.0, 4.0, 100.0, -40.0, 2.0],  // far column: surface +2 — the reviewer's trap
        ]))));
        assert!((c.water_surface([2.0, 0.0, -2.0]).unwrap() - 0.0).abs() < 1e-3
                && (c.water_surface([6.0, 0.0, -2.0]).unwrap() - 2.0).abs() < 1e-3,
            "fixture: a 2 u surface mismatch across the lintel — the smallest measured trapping \
             mismatch");
        let mut ctrl = CharacterController::new([-2.0, 0.0, -2.0]);
        drive_east(&mut ctrl, &c, 240, 0.0);
        assert!(ctrl.pos[0] < 4.0,
            "#661 review R2-B1: the duck must REFUSE a crossing whose far float plane is beyond \
             the return duck's envelope — measured, crossing at a 2 u mismatch traps permanently \
             and silently; got {:?}", ctrl.pos);
        assert!(c.in_water(ctrl.pos) && (ctrl.pos[2] - (-2.0)).abs() < 0.3 && !ctrl.on_ground,
            "…and the refused swimmer stays wet at its plane: {:?}", ctrl.pos);
    }

    /// **B1's other half — a divable far side is a ROUND TRIP.** Identical scene with the far
    /// floor at −4.6, just below the −4.5 duck depth: the crossing is allowed, and driving back
    /// west re-ducks under the same lintel and returns. This is the reversibility the ordering
    /// comment claims, held as a measurement — and the over-tightening guard on the B1 fix (a
    /// refusal keyed on anything stronger than re-divability would turn this red).
    #[test]
    fn a_duck_across_a_divable_far_side_is_a_round_trip() {
        let c = flooded_corridor(
            vec![floor(-40.0, -100.0, 4.0), floor(-4.6, 4.0, 100.0), wall(4.0, -0.2, 20.0)],
            -40.0, 0.0);
        let mut ctrl = CharacterController::new([-2.0, 0.0, -2.0]);
        drive_east(&mut ctrl, &c, 240, 0.0);
        assert!(ctrl.pos[0] > 8.0,
            "fixture/capability: with the far floor below the duck depth the crossing must be \
             allowed; got {:?}", ctrl.pos);
        let west = MoveIntent { wish_dir: [-1.0, 0.0], wish_vspeed: 0.0, jump: false,
                                want_swim: true, speed: 44.0, climb: 0.0, hop: false };
        for _ in 0..360 { ctrl.step(west, 1.0 / 60.0, &c); }
        assert!(ctrl.pos[0] < 0.0,
            "#661 review B1: the crossing must be a round trip — driving back west must re-duck \
             under the lintel and return; got {:?}", ctrl.pos);
        assert!(c.in_water(ctrl.pos) && !ctrl.on_ground,
            "…still a swimmer after the round trip: {:?}", ctrl.pos);
    }

    /// **The `wish_vspeed <= 0` gate (review M4): an explicit upward drive is never countermanded
    /// by an autonomous dive.** The walker's haul-out approach (water design §4c) holds an
    /// up-wish; if the duck could fire during it, a bank with any open water under its face would
    /// see the controller dive under instead of climbing out. Here the up-wishing swimmer starts
    /// at the lintel already within duck range: with the gate it presses and RISES (the §4c
    /// shape); with the gate deleted it ducks under on the first blocked frame and crosses —
    /// which is this test's failure.
    #[test]
    fn an_upward_haul_out_drive_is_never_countermanded_by_the_duck() {
        let c = flooded_corridor(
            vec![floor(-40.0, -100.0, 100.0), wall(4.0, -0.2, 20.0)],
            -40.0, 0.0);
        let mut ctrl = CharacterController::new([3.0, 0.0, -2.4]);
        drive_east(&mut ctrl, &c, 120, 5.0); // an explicit, sustained upward swim wish
        assert!(ctrl.pos[0] < 4.0,
            "#661 review M4: an up-wishing swimmer must NEVER be taken under the obstruction by \
             the autonomous duck — the up-wish is the walker's haul-out drive; got {:?}", ctrl.pos);
        assert!(ctrl.pos[2] > -2.4 + 0.05 && c.in_water(ctrl.pos),
            "…and the up-wish must actually be rising it toward the surface: {:?}", ctrl.pos);
        // The surface CLAMP's own pin, under its own name (#661 review N4 — previously this test
        // pinned the clamp only by coupling): a fully-risen up-wishing swimmer parks `SKIN` UNDER
        // the waterline, still wet by the strict probe. Reverting the clamp to exactly `surf`
        // parks the feet ON the boundary, the body reads dry for the frame, and the DRY
        // depenetration net owns a swimming body (in THIS scene, pre-clamp-fix, it ring-recovered
        // the body to the pool floor 40 u down — `nearest_floor` at the first clear candidate,
        // `dz = -40.0`, `Grounded` — because every floor nearer than the pool bottom is outside
        // the candidate columns here; in shallower scenes it shoves laterally instead, the
        // reviewer's measured 2 u variant. Same mechanism, geometry-dependent magnitude).
        assert!(ctrl.pos[2] <= -SKIN + 1e-4,
            "the up-wish clamp must park the feet SKIN under the surface, never ON it — feet at \
             the exact waterline read DRY and hand a swimming body to the dry net; got z={}",
            ctrl.pos[2]);
    }

    /// **The destination water check (review M5): a duck never exits the water SIDEWAYS.** The
    /// water region ends exactly at the lintel's plane, so the surface swimmer always stays wet —
    /// but a duck's landing would be dry. With the check the duck is refused and the body keeps
    /// pressing, wet, at its plane; with the check deleted the duck lands the body dry at depth
    /// with `want_swim` inert and gravity in charge — it plummets to the pool floor 38 u below,
    /// which is this test's failure.
    #[test]
    fn a_duck_never_exits_the_water_sideways() {
        let mut c = col(vec![floor(-40.0, -100.0, 100.0), wall(4.0, -0.2, 20.0)]);
        c.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::box_below(-100.0, 100.0, -100.0, 4.0, 0.0))));
        assert!(c.in_water([3.5, 0.0, -2.0]) && !c.in_water([4.5, 0.0, -4.5]),
            "fixture: wet up to the lintel plane, dry beyond it at every depth");
        let mut ctrl = CharacterController::new([-2.0, 0.0, -2.0]);
        drive_east(&mut ctrl, &c, 180, 0.0);
        assert!(ctrl.pos[0] < 4.05 && (ctrl.pos[2] - (-2.0)).abs() < 0.3,
            "#661 review M5: a duck whose landing is dry must be refused — accepting it exits the \
             medium sideways at depth and gravity takes the body to the pool floor; got {:?}",
            ctrl.pos);
        assert!(c.in_water(ctrl.pos) && !ctrl.on_ground,
            "…the refused swimmer is still a swimmer: {:?}", ctrl.pos);
    }

    /// **The lowered-start water check (review M6): a duck never dives out the BOTTOM of its own
    /// water volume.** The near column's water is only 3.5 u deep over a 40 u-deep passage: the
    /// 2.5 u sink from the float plane would put the feet below the volume's floor, transiting
    /// dry space even though the far side would be wet again. With the check the duck is refused;
    /// with it deleted the body crosses — this test's failure.
    #[test]
    fn a_duck_never_dives_out_the_bottom_of_its_own_water_volume() {
        let mut c = col(vec![floor(-40.0, -100.0, 100.0), wall(4.0, -0.2, 20.0)]);
        c.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::water_boxes(&[
            [-100.0, 100.0, -100.0, 4.0, -3.5, 0.0], // shallow near volume: bottom −3.5
            [-100.0, 100.0, 4.0, 100.0, -40.0, 0.0], // deep far volume
        ]))));
        assert!(c.in_water([2.0, 0.0, -2.0]) && !c.in_water([2.0, 0.0, -4.5])
                && c.in_water([6.0, 0.0, -4.5]),
            "fixture: the ducked depth is BELOW the near volume's bottom but wet on the far side — \
             exactly the case only the lowered-start check refuses");
        let mut ctrl = CharacterController::new([-2.0, 0.0, -2.0]);
        drive_east(&mut ctrl, &c, 180, 0.0);
        assert!(ctrl.pos[0] < 4.05 && ctrl.pos[2] > -3.6,
            "#661 review M6: the duck must not dive through the floor of the water it is in, even \
             to a wet landing; got {:?}", ctrl.pos);
    }

    /// **The 2.5 u envelope (review M7), pinned where CI can see it.** The lintel's underside is
    /// at −1.0: passing it needs the chest (feet + 4) below −1.0, i.e. a 3.0 u dive from the
    /// float plane — just past the `STEP_UP + GROUND_SNAP_TOL` = 2.5 envelope. The duck must
    /// refuse; a widened envelope (the review's 12.0 mutation, previously RED only in
    /// `#[ignore]`d asset-gated tests) crosses and turns this red. The envelope is the step
    /// capability's mirror, not a free dive — anything deeper is the planner's business
    /// (a dive-first route with an explicit down-wish), not the controller's autonomy.
    #[test]
    fn the_duck_envelope_is_the_step_envelope_not_a_free_dive() {
        let c = flooded_corridor(
            vec![floor(-40.0, -100.0, 100.0), wall(4.0, -1.0, 20.0)],
            -40.0, 0.0);
        let mut ctrl = CharacterController::new([-2.0, 0.0, -2.0]);
        drive_east(&mut ctrl, &c, 240, 0.0);
        assert!(ctrl.pos[0] < 4.0 && ctrl.pos[2] > -4.6,
            "#661 review M7: a passage needing a 3.0 u dive is outside the 2.5 u duck envelope and \
             must be refused — a free-dive duck is a new capability nobody approved; got {:?}",
            ctrl.pos);
        assert!(c.in_water(ctrl.pos) && (ctrl.pos[2] - (-2.0)).abs() < 0.3,
            "…the refused swimmer holds its plane: {:?}", ctrl.pos);
    }

    /// **The recovery ring never contains an EMBEDDED sample (review B3).** Widening the net's
    /// door for wet bodies let a wading body embedded between close rocks reach the banking arm —
    /// silently breaking the ring's "grounded and non-embedded by construction" property that
    /// both ring readers (the stuck fallback and the underworld guard) rely on for their restore
    /// points. The banking site now enforces the predicate explicitly. Here: a body wades
    /// embedded in a flooded 1.6 u slot for two full seconds (four banking windows), is then
    /// relocated over a floorless dry column, and the stuck fallback must find NOTHING to restore
    /// — it must hold and disclose, not rubber-band the body back into the embedded slot.
    #[test]
    fn an_embedded_wading_sample_never_enters_the_recovery_ring() {
        let mut c = col(vec![floor(0.0, -100.0, 100.0), wall(0.8, 0.0, 10.0), wall(-0.8, 0.0, 10.0)]);
        // Water only over the slot's east band, so the relocation target is DRY.
        c.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::box_below(-100.0, 100.0, -1.5, 1.5, 1.0))));
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        assert!(c.in_water([0.0, 0.0, 0.0]) && !c.footprint_clear(0.0, 0.0, 0.0, PLAYER_RADIUS, 8),
            "fixture: a WET, GROUNDED, EMBEDDED body — the exact combination the widened door let \
             into the banking arm");
        for _ in 0..120 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(ctrl.on_ground && ctrl.pos[0].abs() < 0.1,
            "fixture: it waded in place, grounded, for the whole banking period: {:?}", ctrl.pos);

        // #845: assert B3's property WHERE IT LIVES, on the ring itself, instead of only inferring
        // it from a downstream restore. The original test could only observe the ring through the
        // stuck fallback's behaviour, which made it hostage to what that fallback does next; this
        // states the invariant directly and holds whatever the fallback is changed to.
        assert!(ctrl.good.is_empty(),
            "#661 review B3: an embedded wade must bank NOTHING; the ring holds {:?}", ctrl.good);

        // Relocate over a column with NO floor below — dry, clear footprint, `ground_below` none →
        // the net's no-floor arm, whose only recovery is the ring. East 1000 rather than east 50
        // since #845: the zone's floor spans east ±100, so at east 50 the last-resort search finds
        // ground ~54 u away and rescues the body, which would silently turn the assertions below
        // into a test of the rescue instead of a test of the ring. At east 1000 the nearest floor
        // is ~900 u away, past `RESCUE_RADII`'s reach, and the arm under test is reached again.
        ctrl.pos = [1000.0, 0.0, -49.0];
        assert!(!c.in_water(ctrl.pos)
                && c.ground_below(1000.0, 0.0, -48.0, GROUND_DEPTH).is_none()
                && nearest_standing_place(&c, ctrl.pos, f32::NEG_INFINITY).is_none(),
            "fixture: the relocation target is dry, with nothing below in probe range and nothing \
             the last-resort search can reach");
        for _ in 0..90 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &c); }
        assert!((ctrl.pos[0] - 1000.0).abs() < 1.5,
            "#661 review B3: the ring must hold NO sample from the embedded wade — restoring one \
             rubber-bands the body back into the slot it was standing embedded in; got {:?}",
            ctrl.pos);
        assert!(matches!(ctrl.hold(), Some(h) if h.reason == ControllerHoldReason::EmbeddedNoRecovery),
            "…and with an empty ring the stuck fallback must HOLD and disclose, not invent a \
             restore point; got {:?}", ctrl.hold());
    }

    #[test]
    // #730: capitalised on purpose — BODY-not-feet is the exact #649 regression this test pins
    // (a feet-only probe calls a submerged body dry, see the comment below); renaming would blur it.
    #[allow(non_snake_case)]
    fn the_nets_water_probe_is_the_BODY_not_the_feet() {
        // #649 REVIEW FINDING 5 — the reviewer's own mutation: `body_in_water(col, p)` →
        // `col.in_water(p)` (feet-only) inside the net left the whole suite green. This pins the
        // reason the hoisted chest probe exists, which is the case `water_probe`'s doc describes: a
        // body can be submerged while its FEET are a hair outside the baked water region, because
        // the `.wtr` volume does not have to meet the floor. A feet-only probe calls that body dry
        // and mounts it on the slab above — the #649 defect, reachable again by "simplification".
        //
        // #661 moved the probe from the recovery choice to the net's DOOR (a floating body never
        // enters at all); the mutation this pins is unchanged and this test still catches it: with
        // a feet-only door probe, this feet-dry body walks straight into the dry net and is
        // beached on the slab at z=2.
        let mut c = col(vec![floor(-12.0, -100.0, 100.0), floor(2.0, -100.0, 100.0),
                             wall(0.8, -12.0, 10.0), wall(-0.8, -12.0, 10.0)]);
        // Water from 0.5 up: the feet at z=0 are OUTSIDE it, the chest at z=3 is inside.
        c.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::water_slab(0.5, 10.0))));
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        assert!(!c.in_water([0.0, 0.0, 0.0]) && c.in_water([0.0, 0.0, 3.0]),
            "fixture: feet dry, chest wet — else this is not the case that distinguishes the probes");

        ctrl.step(swim_still(), 1.0 / 60.0, &c);

        // One frame. Fixed: the door sees a wet BODY, the net stays out, and buoyancy's ordinary
        // rise begins (the pre-#661 assertion demanded z exactly 0, but that was the net FREEZING
        // the frame — "not teleported" and "frozen" are different claims, and the frozen half is
        // gone with the net). The bound is buoyancy's own per-frame maximum, `BUOY_RATE * dt`
        // = 0.5 u — the PHYSICAL claim, not a midpoint to the mutation (#661 review, flip-4 note:
        // `< 1.0` would let a doubled buoyancy rate ship green under this test's name). Mutated
        // (feet-only door probe): the body walks into the dry net and is beached on the slab —
        // z = 2.0, `on_ground` — which both halves below reject.
        assert!(ctrl.pos[2] <= BUOY_RATE * (1.0 / 60.0) + 1e-3 && !ctrl.on_ground,
            "a submerged body whose FEET are outside the water volume is still afloat and rises at \
             most one buoyancy step (0.5 u): a feet-only probe in the net calls it dry and mounts \
             it on the slab at z=2, grounded — got {:?} on_ground={}", ctrl.pos, ctrl.on_ground);
    }

    /// Does the asset tree carry any evidence that `<name>.glb` names a ZONE, as opposed to a
    /// character/creature/prop model sitting in the same directory?
    ///
    /// **This deliberately does not ask whether the zone's water map is USABLE** (#850, #879
    /// round-2 BLOCKING 1). The predicate that stood here was
    /// `maps/water/<name>.wtr` `is_file()`, and a name that failed it was deleted from the corpus
    /// before the loop — so it was never opened, never entered the rollup, and could not make
    /// the rollup incomplete. Measured on four scratch corpora against that code, varying only one
    /// zone's `.wtr`: a CORRUPT file failed the run RED, while DELETING the same file passed
    /// `ok … — COMPLETE` over a corpus of one, and a DIRECTORY in its place passed too. Deleting a
    /// broken file made the build greener than fixing nothing, and the directory case collapsed
    /// `RegionLoadError::Unreadable` into "no water map" — the substitution
    /// `every_wtr_load_failure_is_a_distinct_named_value_762` in `region_map.rs` exists to forbid.
    ///
    /// So membership of the corpus is decided WITHOUT consulting the water map's contents. Three
    /// signals, and **any one** of them puts the name in the population, where the loader — not a
    /// filesystem predicate — classifies it as measured or as `unmeasured` with a named reason:
    ///
    /// 1. `<name>_doors.glb` — the doors companion a baked zone ships beside it.
    /// 2. `maps/<name>.txt` — the EQ map pack `ZoneMap::try_load` reads.
    /// 3. **Any filesystem entry at all** at `maps/water/<name>.wtr`. This uses
    ///    `symlink_metadata`, not `is_file()`, so a directory, a dangling symlink and an
    ///    unreadable file all count as evidence and send the name INTO the corpus to fail loudly.
    ///
    /// Measured on the default `$EQZONES` at the time of writing, over the 94 non-furniture `.glb`
    /// names: each of the three signals splits them the same way, 42 with and 52 without, and the
    /// 52 are exactly the character/creature/prop models. Three signals rather than one because
    /// each is a separate asset that a partial sync can drop on its own; only a name with **none**
    /// of them leaves the population.
    ///
    /// **The residual, stated rather than hidden.** A `<name>.glb` with none of the three is
    /// indistinguishable from a creature model by anything on disk, and is excluded — printed by
    /// name on the discovery line. That is the boundary of what this can know, not an oversight.
    /// Widening it needs a source of truth outside the asset directory: a baked-zone manifest, so
    /// the corpus can ASSERT its population rather than infer it. Filed as #928 with the exact
    /// corpus and both outputs; not closable by a change to this predicate.
    fn zone_evidence(dir: &std::path::Path, name: &str) -> bool {
        let entry_at = |p: std::path::PathBuf| p.symlink_metadata().is_ok();
        entry_at(dir.join(format!("{name}_doors.glb")))
            || entry_at(dir.join("maps").join(format!("{name}.txt")))
            || entry_at(dir.join("maps/water").join(format!("{name}.wtr")))
    }

    /// **The corpus population may never shrink because a water map got WORSE (#850, #879 round-2
    /// BLOCKING 1).** The universal, in one sentence: *no state of a zone's water asset may produce
    /// a verdict greener than a strictly better state of the same asset.*
    ///
    /// The way that gets violated is a pre-filter. A name removed from the population before the
    /// loop is a name the rollup cannot see, so it cannot be counted, named, or reddened — and the
    /// filter's own signal was the very asset whose failure the run is supposed to announce. The
    /// four-state table below is the shape of the round-2 defect: `is_file()` admitted only the
    /// first row, dropped rows 2 and 4 silently, and reddened only row 3 — so the two states that
    /// are no better than row 3 were the two that passed.
    ///
    /// This is a unit test over the population predicate, with no baked assets, so CI runs it. The
    /// other half — that a name IN the population with a bad water map goes RED — belongs to
    /// `open_corpus_zone`'s DROP 3 and is pinned by its own tests in `water_grid.rs`.
    #[test]
    fn no_state_of_a_water_map_can_shrink_the_corpus_population_850() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("maps/water")).unwrap();

        // ── the universal, as a table over the water asset ───────────────────────────────────
        // Each row is a state of `maps/water/<zone>.wtr` for a name the tree ALREADY says is a
        // zone (it has a map pack). Every row must stay in the population; what the water map
        // says is the LOADER's verdict to give, not a filter's.
        let states: &[(&str, fn(&std::path::Path))] = &[
            ("a valid-looking .wtr", |p| std::fs::write(p, b"EQEMUWATER\x02\0\0\0\0\0\0\0").unwrap()),
            ("absent",               |_| ()),
            ("corrupt",              |p| std::fs::write(p, b"not region data at all").unwrap()),
            ("a directory",          |p| std::fs::create_dir(p).unwrap()),
        ];
        for (what, make) in states {
            let zone = format!("zone{}", what.replace(|c: char| !c.is_ascii_alphanumeric(), ""));
            std::fs::write(dir.join(format!("{zone}.glb")), b"").unwrap();
            std::fs::write(dir.join(format!("maps/{zone}.txt")), b"").unwrap();
            make(&dir.join(format!("maps/water/{zone}.wtr")));
            assert!(zone_evidence(dir, &zone),
                "#850: a zone whose water map is {what} was dropped from the corpus population. \
                 Every state of that file must leave the zone IN the corpus so the loader can name \
                 it — filtering here is how DELETING a corrupt .wtr came to make the run greener \
                 than leaving it in place");
        }

        // ── each signal alone is enough ──────────────────────────────────────────────────────
        // A real zone that lost two of the three assets is still a zone, and still has to be
        // measured or named. Written as three separate names so one signal cannot cover another.
        let alone: &[(&str, fn(&std::path::Path, &str))] = &[
            ("only the doors companion", |d, z| { std::fs::write(d.join(format!("{z}_doors.glb")), b"").unwrap(); }),
            ("only the map pack",        |d, z| { std::fs::write(d.join(format!("maps/{z}.txt")), b"").unwrap(); }),
            ("only a .wtr directory",    |d, z| { std::fs::create_dir(d.join(format!("maps/water/{z}.wtr"))).unwrap(); }),
        ];
        for (i, (what, make)) in alone.iter().enumerate() {
            let zone = format!("lone{i}");
            std::fs::write(dir.join(format!("{zone}.glb")), b"").unwrap();
            make(dir, &zone);
            assert!(zone_evidence(dir, &zone),
                "#850: a zone with {what} must still be in the corpus population — the three \
                 signals are OR'd precisely so a partial asset sync cannot silently shrink it");
        }

        // ── and the documented residual, pinned so it stays deliberate ───────────────────────
        // A `.glb` with none of the three is what `bat` and `weapons` are, and is excluded. This
        // half of the pin is what the `if false` wrap mutation reddens.
        std::fs::write(dir.join("bat.glb"), b"").unwrap();
        assert!(!zone_evidence(dir, "bat"),
            "a .glb with no doors companion, no map pack and no .wtr entry is indistinguishable \
             from a creature model, and must be excluded and named — admitting it here would red \
             the real corpus on all 52 character/creature/prop models");
    }

    /// **THE DEPENETRATION CORPUS — the blast-radius harness, committed so its numbers are
    /// reproducible (#649 review, finding 6).**
    ///
    /// Two things at once, over every baked zone found at `$EQZONES` — where **a zone is a
    /// `<name>.glb` the asset tree carries zone evidence for** (see `zone_evidence` and the
    /// discovery block below), not any `.glb` that is not furniture: that older predicate admitted
    /// 52 character/creature/prop models as "zones". Discovery and the per-zone accounting are both
    /// asserted, so the counts this prints are the corpus, not the survivors:
    ///
    /// 1. **An ITERATION invariant, driven through the real controller.** The first cut of the #649
    ///    fix shipped a recovery that was itself embedded, and no one-shot harness could see it —
    ///    the failure only appears when the net is re-entered from its own answer (review finding 1).
    ///    So every embedded sample is driven for two input-free seconds and must have COME TO REST
    ///    by the end unless it is no longer embedded. "Still moving AND still embedded" is the drift
    ///    signature and fails the test; a body that merely came to rest somewhere embedded is the
    ///    pre-existing "the push-out gave up" state `main` produces too, and is not flagged.
    /// 2. **The recovery diff vs the pre-#649 rule**, reproduced inline as `legacy_recovery`, split
    ///    by medium so the partition is explicit: a body is bucketed by whether its FEET are dry and
    ///    whether its CHEST is wet. The dry/dry bucket must show ZERO changes — that is the
    ///    "dry depenetration is untouched" claim, and it is drawn with feet-dry-and-chest-dry, not
    ///    with the new predicate (which would make it partly definitional).
    ///
    /// ```text
    /// EQZONES=~/eqzones cargo test --release --lib depenetration_corpus -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "asset-gated: needs baked zone glbs + maps/water at $EQZONES (#357)"]
    fn depenetration_corpus_over_baked_zones() {
        let dir = std::path::PathBuf::from(std::env::var("EQZONES").unwrap_or_else(|_| {
            format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap())
        }));
        /// The pre-#649 rule, verbatim: nearest floor in EITHER direction, always grounded.
        fn legacy_recovery(col: &Collision, p: [f32; 3]) -> Option<([f32; 3], bool)> {
            for &r in &PUSHOUT_RADII {
                for i in 0..PUSHOUT_DIRS {
                    let a = (i as f32) / (PUSHOUT_DIRS as f32) * std::f32::consts::TAU;
                    let (e, n) = (p[0] + a.cos() * r, p[1] + a.sin() * r);
                    if !col.footprint_clear(e, n, p[2], PLAYER_RADIUS, PUSHOUT_DIRS / 2) { continue; }
                    if let Some(f) = col.nearest_floor(e, n, p[2], STEP_UP + GROUND_ORIGIN, GROUND_DEPTH) {
                        return Some(([e, n, f], true));
                    }
                }
            }
            None
        }
        /// This branch's rule, expressed through the production `Recovery` (no second copy of it).
        /// #661: a body afloat in water never enters the net at all, so its "recovery" is None —
        /// physics keeps custody (mirrors `depenetrate`'s door, which uses the same body probe).
        fn new_recovery(col: &Collision, p: [f32; 3]) -> Option<([f32; 3], bool)> {
            if body_in_water(col, p) { return None; }
            for &r in &PUSHOUT_RADII {
                for i in 0..PUSHOUT_DIRS {
                    let a = (i as f32) / (PUSHOUT_DIRS as f32) * std::f32::consts::TAU;
                    let (e, n) = (p[0] + a.cos() * r, p[1] + a.sin() * r);
                    if !col.footprint_clear(e, n, p[2], PLAYER_RADIUS, PUSHOUT_DIRS / 2) { continue; }
                    if let Some(rec) = Recovery::at_column(col, e, n, p[2]) {
                        return Some(([e, n, rec.z()], rec.on_ground()));
                    }
                }
            }
            None
        }

        // ── #850 / #879 review B3: DISCOVERY, with every directory entry accounted ──────────────
        //
        // What used to stand here was a `filter_map` whose predicate was "any `.glb` not ending in
        // `_doors`/`_obj`", and whose count was then printed as the corpus size. Measured on the
        // default `$EQZONES` at the time of writing: 185 directory entries, 136 `.glb`, 42 of them
        // `_doors`/`_obj`, leaving 94 — and **52 of those 94 are character/creature/prop models**
        // (`bat`, `bear`, `race_*`, `weapons`, …), which the loop below happily built a collision
        // grid for and sampled 500 random columns inside. A line reading `zones=94` over a corpus
        // that is 55% not-zones is the same confident falsehood #850 is about, one level up from
        // the drop paths.
        //
        // The predicate is `zone_evidence` — see its doc comment for what it reads and why it does
        // NOT read whether the water map loads. Round 2 of this fix used
        // `maps/water/<name>.wtr` `is_file()` here, which measurably made a DELETED water map
        // greener than a corrupt one and collapsed `Unreadable` into "missing"; that is #879's
        // round-2 blocking finding and `no_state_of_a_water_map_can_shrink_the_corpus_population_850`
        // is the pin. The narrowing this bucket exists for is still needed: the whole sample
        // partition is `in_water(feet)` x `in_water(chest)`, so a creature model contributes
        // vacuous dry counts and an empty water ladder.
        //
        // Every entry `read_dir` yields lands in EXACTLY ONE bucket, and the buckets are asserted
        // against the raw entry count below — so the `e.ok()?` / `to_str()?` swallows the old
        // `filter_map` performed (review N2) are counted and named instead of vanishing.
        let mut entries = 0usize;
        let mut unreadable: Vec<String> = Vec::new();
        let mut non_glb = 0usize;
        let mut furniture: Vec<String> = Vec::new();
        let mut not_a_zone: Vec<String> = Vec::new();
        let mut zones: Vec<String> = Vec::new();
        for ent in std::fs::read_dir(&dir).expect("$EQZONES") {
            entries += 1;
            let path = match ent {
                Ok(e) => e.path(),
                Err(e) => { unreadable.push(format!("<unreadable dir entry: {e}>")); continue }
            };
            let Some(os_name) = path.file_name() else {
                unreadable.push(format!("<no file name: {}>", path.display()));
                continue;
            };
            let Some(file) = os_name.to_str() else {
                unreadable.push(format!("<non-UTF-8 name: {}>", path.to_string_lossy()));
                continue;
            };
            let Some(name) = file.strip_suffix(".glb") else { non_glb += 1; continue };
            if name.ends_with("_doors") || name.ends_with("_obj") {
                furniture.push(name.to_string());
            } else if !zone_evidence(&dir, name) {
                not_a_zone.push(name.to_string());
            } else {
                zones.push(name.to_string());
            }
        }
        zones.sort();
        furniture.sort();
        not_a_zone.sort();
        unreadable.sort();
        // A FUTURE-EDIT guard, not a check on the filesystem (#879 review N4). As the loop above is
        // written every path increments `entries` and lands in exactly one bucket, so this cannot
        // fail on any input — a reader must not take a green run here as evidence that the scan saw
        // what the directory holds. What it catches is the next `continue` added to that loop
        // without a bucket, which is the shape #850 is about one level up.
        assert_eq!(entries,
            unreadable.len() + non_glb + furniture.len() + not_a_zone.len() + zones.len(),
            "#850: every entry $EQZONES yielded must land in exactly one discovery bucket — \
             {entries} entries vs {} unreadable + {non_glb} non-glb + {} doors/obj + {} non-zone \
             glb + {} zone glb", unreadable.len(), furniture.len(), not_a_zone.len(), zones.len());
        assert!(!zones.is_empty(),
            "no baked zones at {dir:?} — a zone here is a `<name>.glb` the tree carries zone \
             evidence for: a `<name>_doors.glb`, a `maps/<name>.txt`, or any entry at \
             `maps/water/<name>.wtr` ({entries} entries scanned, {non_glb} non-glb, {} doors/obj, \
             {} glb with no zone evidence)", furniture.len(), not_a_zone.len());
        let discovered = zones.len();

        // ── #850 / #879 review B1: the ACCOUNTING, owned by a type instead of by call sites ─────
        //
        // Round 1 of this fix pushed `(zone, reason)` onto a local `dropped: Vec<_>` at each
        // `continue` and asserted `covered + dropped.len() == discovered`. That is the round-2
        // shape `water_grid.rs`'s own round-3 lesson rejects, and it failed the same way: the
        // covered counter was incremented near the TOP of the loop body, so it counted ENTRY, not
        // completion, and a reviewer's one-line `continue` added after it — no `dropped.push` —
        // produced `discovered=2 covered=2 dropped=0` over a corpus with one zone silently
        // abandoned, green. Bit-for-bit the pre-fix defect, now with an assertion advertising
        // completeness.
        //
        // So the state "an iteration ended without being classified" is no longer representable
        // here: `open_corpus_zone` calls `WaterRollup::begin_zone` as its first statement and every
        // `Err` return is preceded by a `skip`/`add` that closes the zone; the only way to close an
        // OPEN zone is the `cover.add` at the very bottom of the body. Anything that leaves the
        // body in between — an existing `continue`, a `continue` added next year, a `break`, a `?`,
        // an early `return` — lands the zone in `unaccounted`, which makes `is_complete()` false
        // and names the zone in `Display`. Nothing has to be wired per call site, and nothing has
        // to be re-verified by enumeration when this loop changes.
        //
        // `is_complete()` also carries the clean-over-nothing guard (`attempted_zones() > 0`) that
        // round 1's arithmetic control had no analogue for: an all-bad corpus satisfies
        // `covered + dropped == discovered` and passed green having measured zero zones, with
        // `drifters.is_empty()` and `ch_dry == 0` both vacuously true.
        let mut cover = crate::nav::water_grid::WaterRollup::new();
        let mut t_emb = 0u64;
        let (mut ch_dry, mut ch_chest, mut ch_wet) = (0u64, 0u64, 0u64);
        let (mut same_dry, mut same_chest, mut same_wet) = (0u64, 0u64, 0u64);
        let (mut none_legacy, mut none_new) = (0u64, 0u64);
        let (mut t_cols, mut no_floor) = (0u64, 0u64);
        let mut drifters: Vec<(String, [f32; 3], [f32; 3])> = Vec::new();
        for name in &zones {
            let (col, zw) = match crate::nav::water_grid::open_corpus_zone(&mut cover, &dir, name, 32.0) {
                Ok(v) => v,
                // Already recorded in `cover` by the time this value exists — printing it is
                // diagnostics, not bookkeeping.
                Err(why) => { println!("{name:>12}: DROPPED — {why}"); continue }
            };
            let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut rnd = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
                               (seed >> 11) as f64 / (1u64 << 53) as f64 };
            let mut zone_emb = 0u32;
            for _ in 0..500 {
                t_cols += 1;
                let e = col.origin[0] + rnd() as f32 * (col.cols as f32 * col.cell_size);
                let n = col.origin[1] + rnd() as f32 * (col.rows as f32 * col.cell_size);
                // A SAMPLE drop, not a zone drop: 500 random columns over a zone's bounding box are
                // expected to miss the floor, and #850 says to decide about this one deliberately
                // rather than fold it into the zone accounting. Decided: it stays a `continue`, and
                // it is COUNTED and printed (`no floor: N of M columns`) so the reader can see how
                // much of the sample budget never became a probe.
                let Some(fz) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { no_floor += 1; continue };
                // A ladder of z around the column's floor (below it = embedded in rock, above =
                // open air), plus, when the column holds water, a ladder of depths inside it.
                let mut zs: Vec<f32> = [-16.0f32, -8.0, -4.0, -2.0, -1.0, -0.25, 0.0, 0.5, 2.0, 6.0, 15.0]
                    .iter().map(|o| fz + o).collect();
                if let Some(surf) = col.water_surface([e, n, fz + 1.0]) {
                    for d in [0.5f32, 1.0, 2.0, 3.0, 5.0, 9.0, 15.0] { zs.push(surf - d); }
                }
                for z in zs {
                    let p = [e, n, z];
                    if !is_embedded(&col, p) { continue; }
                    t_emb += 1; zone_emb += 1;
                    // (2) recovery diff, bucketed by an EXPLICIT medium partition
                    let (a, b) = (legacy_recovery(&col, p), new_recovery(&col, p));
                    if a.is_none() { none_legacy += 1; }
                    if b.is_none() { none_new += 1; }
                    let feet_wet = col.in_water(p);
                    let chest_wet = col.in_water([p[0], p[1], p[2] + WATER_BODY]);
                    let bucket = if feet_wet { 2 } else if chest_wet { 1 } else { 0 };
                    match (a == b, bucket) {
                        (true, 0) => same_dry += 1, (true, 1) => same_chest += 1, (true, _) => same_wet += 1,
                        (false, 0) => ch_dry += 1, (false, 1) => ch_chest += 1, (false, _) => ch_wet += 1,
                    }
                    // (1) the ITERATION invariant — only for a bounded sample per zone, since it
                    // steps the real controller 120 times per point.
                    if zone_emb <= 400 {
                        let mut ctrl = CharacterController::new(p);
                        let idle = MoveIntent { wish_dir: [0.0, 0.0], wish_vspeed: 0.0, jump: false,
                                                want_swim: true, speed: 0.0, climb: 0.0, hop: false };
                        for _ in 0..110 { ctrl.step(idle, 1.0 / 60.0, &col); }
                        let settle_from = ctrl.pos;
                        for _ in 0..10 { ctrl.step(idle, 1.0 / 60.0, &col); }
                        // The drift signature is STILL MOVING while STILL EMBEDDED after two input-free
                        // seconds: the net answering itself, frame after frame. A body that came to
                        // rest is fine even if the rest position is technically embedded — that is the
                        // pre-existing "push-out gave up" state `main` also produces.
                        let still_moving = hlen([ctrl.pos[0] - settle_from[0], ctrl.pos[1] - settle_from[1], 0.0])
                            > 1e-3 || (ctrl.pos[2] - settle_from[2]).abs() > 1e-3;
                        if still_moving && is_embedded(&col, ctrl.pos) && drifters.len() < 20 {
                            drifters.push((name.clone(), p, ctrl.pos));
                        }
                    }
                }
            }
            println!("{name:>12}: embedded={zone_emb}");
            // The ONLY way to close the zone `open_corpus_zone` opened. Reached only by an
            // iteration that ran to the bottom; anything else leaves the zone `unaccounted`.
            cover.add(name, &zw.tally());
        }
        // `accounting:` is ZONE accounting only. The rollup's own water total is structurally 0
        // here (#879 review N6) because this corpus keeps its water numbers in the plain counters
        // below — the `changed:`/`unchanged:` buckets — and folds a zero-valued tally per zone. The
        // "0" on that line is not a measurement of anything; the "over N/M zones" and the
        // COMPLETE/INCOMPLETE verdict are.
        println!("\nzones: discovered={discovered} covered={} embedded={t_emb}\n  \
                  accounting (zones only; the leading 0 is not a water measurement): {cover}\n  \
                  discovery: {entries} $EQZONES entries = {discovered} zone glb (with zone \
                  evidence) + {} doors/obj glb + {} glb with no zone evidence (NOT sampled) + \
                  {non_glb} non-glb + {} unreadable\n  \
                  no zone evidence, excluded: {not_a_zone:?}\n  \
                  unreadable: {unreadable:?}\n  \
                  no floor: {no_floor} of {t_cols} sampled columns\n  \
                  changed: dry-body={ch_dry} wet-chest-dry-feet={ch_chest} \
                  submerged={ch_wet}\n  unchanged: dry-body={same_dry} wet-chest-dry-feet={same_chest} \
                  submerged={same_wet}\n  no recovery: legacy={none_legacy} new={none_new}",
                  cover.measured_zones(), furniture.len(), not_a_zone.len(), unreadable.len());
        // #850 reach control. `is_complete()` is false unless EVERY zone this loop opened was
        // closed by the `cover.add` at the bottom of the body — no `skipped` (dropped before the
        // water check ran), no `unmeasured` (its `.wtr` did not load), no `unaccounted` (opened and
        // abandoned by ANY control flow), and at least one zone folded in at all. That last term is
        // the clean-over-nothing guard: a host whose asset cache is broken now goes RED here
        // instead of reporting `ok` over zero zones with `drifters`/`ch_dry` vacuously satisfied —
        // the case a same-shaped scanner has silently passed before (#778).
        assert!(cover.is_complete(),
            "#850: every discovered zone must be measured, or named as the reason it was not — \
             {cover}");
        // …and the rollup's own denominator must equal what the filesystem scan found, which is
        // the one thing the rollup cannot check for itself: a zone skipped by a filter added to
        // this loop's HEAD would never be opened at all, so it would be invisible to `cover`.
        assert_eq!(cover.attempted_zones(), discovered,
            "#850: the rollup saw {} zones but discovery found {discovered} — a zone was never even \
             opened, so the corpus is smaller than its own rollup line says",
            cover.attempted_zones());
        // CORPUS-level clean-over-nothing, and only that (#879 review N5): `t_emb` is the total
        // across every zone, so this catches a corpus that probed nothing at all and NOT a corpus
        // where all but one zone contributed zero probes. It exists because `drifters.is_empty()`
        // and `ch_dry == 0` below are both vacuously true on an empty sample. A per-zone version
        // would be the stronger claim and is not what this is.
        assert!(t_emb > 0,
            "#850: {discovered} zone(s) measured but ZERO embedded samples were found across the \
             WHOLE corpus — `drifters.is_empty()` and `ch_dry == 0` below are vacuous at this \
             coverage ({no_floor} of {t_cols} sampled columns found no floor)");
        assert!(drifters.is_empty(),
            "a recovery must never itself be embedded — {} sample(s) were STILL MOVING and STILL \
             EMBEDDED after two input-free seconds (the review's finding-1 drift signature): {:?}",
            drifters.len(), drifters);
        assert_eq!(ch_dry, 0,
            "DRY depenetration must be untouched: {ch_dry} of {} recoveries for bodies whose feet AND \
             chest are dry changed", same_dry + ch_dry);
    }

    #[test]
    fn last_good_fallback_after_being_stuck() {
        let good = col(vec![floor(0.0, -100.0, 100.0)]);
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        // Accumulate a good grounded sample at the origin.
        for _ in 0..40 { ctrl.step(walk(0.0, [0.0, 0.0]), 0.05, &good); }
        assert!((ctrl.pos[0]).abs() < 1e-3 && (ctrl.pos[1]).abs() < 1e-3, "stayed at origin on good floor");
        // Now jam it: move into an embedded void (walls box the player, no floor anywhere → push-out
        // can never find a landing) and run long enough to trip the last-good fallback.
        ctrl.pos = [40.0, 40.0, 0.0];
        let bad = col(vec![wall(39.2, 0.0, 10.0), wall(40.8, 0.0, 10.0)]);
        for _ in 0..20 { ctrl.step(walk(0.0, [0.0, 0.0]), 0.05, &bad); }
        assert!((ctrl.pos[0]).abs() < 1e-2 && (ctrl.pos[1]).abs() < 1e-2,
            "should have rubber-banded to the last good grounded position (origin): {:?}", ctrl.pos);
        // The ring buffer only ever samples GROUNDED, NON-EMBEDDED positions (the explicit
        // `!is_embedded` gate at the banking site — #661 review B3), so the fallback recovers a
        // body that IS standing — pinned here because #649 routed this write through the shared
        // `recover` (`Recovery::Grounded`) and an unpinned refactor is an unnoticed behaviour change.
        assert!(ctrl.on_ground, "the last-good position is a grounded one: {:?}", ctrl.pos);
    }

    // ── #845: the two "no recovery" arms are no longer absorbing states ──────────────────────────
    //
    // What #845 is, in state-machine terms: `depenetrate`'s stuck fallback with an empty ring, and
    // `step`'s underworld arm with an empty ring, both used to write NOTHING — not `pos`, not
    // `on_ground`, not `good`, not `stuck_time` — and `depenetrate` returning `true` makes `step`
    // skip the rest of the frame. So the successor state equalled the current state for every
    // possible input: an absorbing state, reachable in ordinary play. Live measurement on the
    // reported casualty (issue #845, and an independent live run recorded there) agrees exactly:
    // thirteen client-API calls — manual moves in four directions, jump, swim-up, stop, two
    // `/goto`s, two `/zone_cross`es, sit, stand, respawn — all returned their documented success
    // shape and moved the body ZERO units, while the state survived a full client restart. The
    // exits that DID work were all external position writes (GM `#summon`, GM `#goto`, `#zone`),
    // i.e. exactly the `teleport` edge the source analysis predicts, and all three need GM status,
    // which an ordinary character does not have.
    //
    // The fix is not a new escape hatch on the API. It removes the absorbing property at the arm
    // itself, by changing the question asked when the ring is empty from "where was this body"
    // (erased by #724 on precisely the events that create the predicament) to "where in this zone
    // could a body stand" (available whenever collision is). The hold is still raised when the zone
    // genuinely answers nowhere — that disclosure is load-bearing and is pinned below.
    //
    // Scope: this covers `EmbeddedNoRecovery` ONLY. `step`'s #150 fall-through guard has an
    // empty-ring arm of the same SHAPE that is not the same THING — it runs after collide-and-slide,
    // so lateral driver input still reaches the body and `UnderworldNoRecovery` is not absorbing.
    // Extending the search there was implemented and reverted; see `last_resort_placement`'s doc.
    //
    // Fixtures here are stated in the measured geometry of the live case rather than round numbers:
    // an empty column, and the nearest floor 133 u away and ~81 u ABOVE the feet.

    /// The live #845 column, reduced: nothing whatever over the body, and the only ground in the
    /// zone is a slab 133 u east and 80.5 u up. `PUSHOUT_RADII` reaches 32 u, so the push-out
    /// cannot see it; `Recovery::at_column`'s `STEP_UP + GROUND_ORIGIN` up-band would reject it
    /// even if it could. Both numbers are from the offline scan of the reported zone's baked GLB.
    fn void_column_with_distant_ground() -> Collision {
        col(vec![floor(84.0, 133.0, 400.0)])
    }
    const VOID_START: [f32; 3] = [0.0, 0.0, 3.5];

    /// #845 — the absorbing state itself. A body in an empty column with no banked history used to
    /// stay at its exact start coordinate for ever, under any driver. It must now be placed on the
    /// zone's real ground, and must be able to walk once it is there.
    ///
    /// MUTATION-CHECK (both directions): wrap `depenetrate`'s rescue call so the source is present
    /// but unreachable — `None if false && self.last_resort_placement(col, dt) => {}` — and this
    /// test fails on the "never left" assertion. Restore it and it passes. Separately, truncating
    /// `RESCUE_RADII` below 133 fails the same assertion, which is what pins the REACH rather than
    /// merely the call.
    #[test]
    fn a_body_in_an_empty_column_with_no_history_is_no_longer_frozen_845() {
        let c = void_column_with_distant_ground();
        let mut ctrl = CharacterController::new(VOID_START);
        ctrl.set_underworld(Some(-222.0));

        // Fixture, stated rather than assumed: this really is the #845 entry state — embedded by
        // the void half of the predicate, with an empty ring and no floor the push-out can reach.
        assert!(is_embedded(&c, VOID_START), "fixture: the start column must be `is_embedded`");
        assert!(c.ground_below(VOID_START[0], VOID_START[1], VOID_START[2] + GROUND_ORIGIN,
                               GROUND_DEPTH).is_none(),
            "fixture: the body must be embedded by the VOID half of the predicate (nothing below), \
             not by being pierced — that is the shape the live casualty was in");
        assert!(ctrl.good.is_empty(), "fixture: no recovery history, as after #724's forget");

        // Before `STUCK_FALLBACK_SECS` nothing should happen: the rescue is the last resort, not
        // the first, and a test that passed instantly would not be watching the arm it claims to.
        for _ in 0..10 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); } // 0.33 s < 0.5 s
        assert_eq!(ctrl.pos, VOID_START,
            "the push-out and the ring must be tried first — nothing may move before \
             STUCK_FALLBACK_SECS: {:?}", ctrl.pos);

        for _ in 0..50 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); } // to 2.0 s total

        assert_ne!(ctrl.pos, VOID_START,
            "#845: the body never left the void column — this is the absorbing state the issue \
             reports, in which every driver input produces zero motion for ever");
        assert!(ctrl.hold().is_none(),
            "the hold must clear by the body MOVING, not by being suppressed: {:?}", ctrl.hold());
        assert!(ctrl.on_ground, "the placement is a grounded one: {:?}", ctrl.pos);
        assert!((ctrl.pos[2] - 84.0).abs() < GROUND_SNAP_TOL,
            "the body must be standing on the zone's only floor (z=84), got {:?}", ctrl.pos);
        let moved = (ctrl.pos[0].powi(2) + ctrl.pos[1].powi(2)).sqrt();
        assert!(moved >= 133.0 && moved <= *RESCUE_RADII.last().unwrap(),
            "the placement must be at least as far as the nearest ground (133 u) and within the \
             search's own reach, got {moved:.1} u to {:?}", ctrl.pos);

        // The point of the exercise: it can be DRIVEN now. A body placed somewhere it cannot move
        // from would satisfy every assertion above and still be the bug.
        let before = ctrl.pos;
        for _ in 0..30 { ctrl.step(walk(20.0, [1.0, 0.0]), 1.0 / 30.0, &c); }
        assert!((ctrl.pos[0] - before[0]).abs() > 1.0,
            "after the placement the body must respond to a driver, moved {:?} → {:?}",
            before, ctrl.pos);
    }

    /// #845 — the disclosure is NOT removed. A zone that genuinely offers nowhere to stand must
    /// still raise `EmbeddedNoRecovery`: the fix narrows when the hold fires, it does not silence
    /// it, and a "fix" that stopped reporting the state would be strictly worse than the bug.
    ///
    /// MUTATION-CHECK: make `nearest_standing_place` return `Some(from)` unconditionally and this
    /// test fails — which is what stops the rescue from being written as "move it anywhere".
    #[test]
    fn a_zone_with_nowhere_to_stand_still_raises_the_hold_845() {
        // Two walls and no floor anywhere in the zone: nothing is standable at any radius.
        let c = col(vec![wall(39.2, 0.0, 10.0), wall(40.8, 0.0, 10.0)]);
        let start = [40.0, 40.0, 0.0];
        let mut ctrl = CharacterController::new(start);
        ctrl.set_underworld(Some(-222.0));
        assert!(nearest_standing_place(&c, start, -222.0).is_none(),
            "fixture: this zone must genuinely have nowhere to stand");

        for _ in 0..60 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); } // 2 s

        let hold = ctrl.hold().expect(
            "#845 must not remove the disclosure: with nowhere in the zone to stand, the body IS \
             frozen and `player.hold` is the only thing that says so");
        assert_eq!(hold.reason, ControllerHoldReason::EmbeddedNoRecovery);
        assert_eq!(ctrl.pos, start, "nothing to move to, so nothing may move: {:?}", ctrl.pos);
    }

    /// #845 — **the acceptance predicate, pinned.** `nearest_standing_place` rejects a candidate
    /// column if it is `is_embedded` or `body_in_water`, and the doc on that function claims those
    /// two rejections are what stop the search from handing the body straight back to the net
    /// (#649: "a recovery that is itself embedded is not a recovery").
    ///
    /// ⚠️ This test exists because that claim was **measured unpinned**. Wrapping the predicate —
    /// `if false && (is_embedded(col, q) || body_in_water(col, q)) { continue; }` — left the whole
    /// suite GREEN, with nothing failing anywhere. Every other #845 test looks only at where the
    /// body ENDS UP, and the search recovers on the following frame from a bad placement, so the bad
    /// placement is invisible to a final-position assertion. The fix is to assert on EVERY frame
    /// instead.
    ///
    /// The zone is built so the two rejected kinds are strictly nearer than the good one:
    /// an embedded column (a floor wedged between two walls) at radius 16, a submerged column at
    /// radius 48, and honest dry ground only from radius 160 out.
    ///
    /// MUTATION-CHECK (both directions, RUN): wrap the predicate as above and this test fails on
    /// `frame 14: the body was moved to [16.0, 0.0, 0.0], which the net reads as embedded`.
    /// Restore it and the suite is green again. (No suite totals are quoted here on purpose — they
    /// move with every merge, and a stale one reads as a live measurement.)
    ///
    /// The wrap above disables BOTH halves at once and the embedded decoy is nearer, so that RED is
    /// the `is_embedded` half. A stated limit here used to say the `body_in_water` half would need
    /// the water decoy moved inside the embedded one to isolate. **It does not** — struck after the
    /// #920 review constructed the isolating mutation and I re-ran it. Leaving `is_embedded` live
    /// already rejects the r≈16 decoy, which promotes the submerged r≈48 one to nearest, so wrapping
    /// only the water half — `if is_embedded(col, q) || (false && body_in_water(col, q))` — reds
    /// this same test, alone, on the water-specific message
    /// (`#649/#845 frame 14: the body was moved into water at [48.0, 0.0, 0.0]`).
    /// Both halves are independently mutation-killed.
    #[test]
    fn the_last_resort_never_places_a_body_somewhere_the_net_would_take_back_845() {
        let mut c = col(vec![
            // r≈16: a floor between two walls — a column with ground that is `is_embedded`.
            floor(0.0, 14.0, 18.0), wall(15.2, 0.0, 10.0), wall(16.8, 0.0, 10.0),
            // r≈48: a floor that is under water.
            floor(0.0, 46.0, 50.0),
            // r≈160: the only honest standing place in the zone.
            floor(84.0, 133.0, 400.0),
        ]);
        c.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::box_below(-100.0, 100.0, 44.0, 52.0, 5.0))));

        // Fixture, stated: the two decoys really are decoys, and the good place really is good.
        assert!(is_embedded(&c, [16.0, 0.0, 0.0]),
            "fixture: the near column must be embedded, else the `is_embedded` half is untested");
        assert!(body_in_water(&c, [48.0, 0.0, 0.0]) && !is_embedded(&c, [48.0, 0.0, 0.0]),
            "fixture: the middle column must be WET and otherwise fine, else the `body_in_water` \
             half is untested");
        assert!(!is_embedded(&c, [160.0, 0.0, 84.0]) && !body_in_water(&c, [160.0, 0.0, 84.0]),
            "fixture: the far column must be acceptable");

        let mut ctrl = CharacterController::new(VOID_START);
        ctrl.set_underworld(Some(-222.0));
        assert!(is_embedded(&c, VOID_START), "fixture: the start is the #845 entry state");

        let mut relocations = 0usize;
        let mut last = ctrl.pos;
        for f in 0..90 {
            ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c);
            if ctrl.pos == last { continue; }
            let d = ((ctrl.pos[0] - last[0]).powi(2) + (ctrl.pos[1] - last[1]).powi(2)).sqrt();
            if d > 32.0 { relocations += 1; } // beyond any push-out radius: a last-resort placement
            last = ctrl.pos;
            assert!(!is_embedded(&c, ctrl.pos),
                "#649/#845 frame {f}: the body was moved to {:?}, which the net reads as embedded \
                 — a recovery that is itself embedded is not a recovery", ctrl.pos);
            assert!(!body_in_water(&c, ctrl.pos),
                "#649/#845 frame {f}: the body was moved into water at {:?}; `Recovery::Grounded` \
                 promises feet on dry floor and this would make that promise false", ctrl.pos);
        }
        assert!(ctrl.hold().is_none(), "the zone HAS a standing place: {:?}", ctrl.hold());
        assert!((ctrl.pos[2] - 84.0).abs() < GROUND_SNAP_TOL && ctrl.pos[0] >= 133.0,
            "the body must end on the far honest ground, got {:?}", ctrl.pos);
        assert_eq!(relocations, 1,
            "one placement, not a walk across the zone via the decoys (#649); got {relocations}");
    }

    /// #845 — a banked position still wins. The ring holds somewhere this body actually STOOD,
    /// which is strictly better evidence than a search result, and the search must not start
    /// pre-empting it (that would silently change every existing rubber-band into a relocation).
    #[test]
    fn a_banked_recovery_still_beats_the_last_resort_search_845() {
        // A platform to bank on in the west, and the #845 far slab in the east.
        let c = col(vec![floor(0.0, -100.0, -50.0), floor(84.0, 133.0, 400.0)]);
        let mut ctrl = CharacterController::new([-80.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        ctrl.set_underworld(Some(-222.0));
        for _ in 0..60 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); }
        let banked = *ctrl.good.back().expect("fixture: the platform must bank a good sample");

        // Jam it into the void column WITHOUT `teleport`, so the ring survives (a `teleport` would
        // clear it — #724 — which is the very thing that makes the empty-ring arm ordinary).
        ctrl.pos = VOID_START;
        for _ in 0..40 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); }

        assert!((ctrl.pos[0] - banked[0]).abs() < 1e-2 && (ctrl.pos[1] - banked[1]).abs() < 1e-2,
            "the banked sample {banked:?} must still win over the search; got {:?}", ctrl.pos);
        assert!(ctrl.pos[2] < 10.0,
            "and specifically NOT the far slab at z=84 the search would have picked: {:?}", ctrl.pos);
    }

    // ── #845 property test: every reachable state has an exit ────────────────────────────────────

    /// xorshift64* — a seeded PRNG in `[0,1)`. The workspace has no `proptest`/`quickcheck`, and a
    /// hand-rolled seeded generator is the house style for property tests here.
    fn a845_rand(state: &mut u64) -> f32 {
        let mut x = *state;
        x ^= x >> 12; x ^= x << 25; x ^= x >> 27;
        *state = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32) / ((1u32 << 24) as f32)
    }
    fn a845_range(state: &mut u64, lo: f32, hi: f32) -> f32 { lo + a845_rand(state) * (hi - lo) }
    /// Axis-aligned horizontal slab: height `z`, east [e0,e1] × north [n0,n1]. `floor` fixes north
    /// to ±100, which cannot express a zone with holes in BOTH axes.
    fn slab(z: f32, e0: f32, e1: f32, n0: f32, n1: f32) -> MeshData {
        mesh(vec![[n0, z, e0], [n1, z, e0], [n1, z, e1], [n0, z, e1]])
    }

    /// The INDEPENDENT oracle for the property below: a dense Cartesian grid scan, deliberately a
    /// different search strategy from the production polar-ring search, sharing only the collision
    /// primitives and the acceptance predicate (which is the definition of "standable" and would be
    /// meaningless to vary). Answers: "does this zone offer this body anywhere to stand?"
    ///
    /// ⚠️ **This oracle is deliberately CONSERVATIVE, and the property is correspondingly narrow.**
    /// The production search is a SAMPLED one — 15 rings × 32 spokes — and is therefore not
    /// complete: ground can exist in a zone and fall between its samples. A test that asserted
    /// "the oracle found a point, so the search must have" would be asserting completeness, which
    /// is false, and would fail for the right reason at the wrong time. So this oracle reports a
    /// place only when it is (a) within 256 u — well inside `RESCUE_RADII`'s 512 u, with ten rings
    /// crossing that band — and (b) part of a standable REGION, confirmed by its four neighbours
    /// one grid step out, so the region is ~96 u across against a worst-case angular gap of ~50 u
    /// at 256 u. Everything the oracle reports is thus something the ring search cannot miss.
    ///
    /// The property below is therefore not "the search is complete". It is "a zone that plainly
    /// offers ground never leaves the body frozen", and the conservatism is on the side that makes
    /// the test quieter, not louder.
    fn a845_oracle(c: &Collision, from: [f32; 3], underworld: f32) -> Option<[f32; 3]> {
        const STEP: f32 = 48.0;
        const REACH: f32 = 256.0;
        let ref_z = from[2].max(underworld);
        let down = (ref_z - underworld).max(0.0).min(RESCUE_BAND);
        let standable = |e: f32, n: f32| -> Option<f32> {
            let f = c.nearest_floor(e, n, ref_z, RESCUE_BAND, down)?;
            if f <= underworld { return None; }
            let q = [e, n, f];
            (!is_embedded(c, q) && !body_in_water(c, q)).then_some(f)
        };
        let k = (REACH / STEP) as i32;
        for i in -k..=k {
            for j in -k..=k {
                let (e, n) = (from[0] + i as f32 * STEP, from[1] + j as f32 * STEP);
                if ((e - from[0]).powi(2) + (n - from[1]).powi(2)).sqrt() > REACH { continue; }
                let Some(f) = standable(e, n) else { continue };
                // Region check: a lone standable grid point could be a sliver the ring search is
                // entitled to miss. Four neighbours make it a place.
                if [(STEP, 0.0), (-STEP, 0.0), (0.0, STEP), (0.0, -STEP)]
                    .iter().any(|(de, dn)| standable(e + de, n + dn).is_none()) { continue; }
                return Some([e, n, f]);
            }
        }
        None
    }

    /// #845 — **the universal**. "Every reachable state has an exit" is a claim no live run can
    /// discharge: a race that usually wins is indistinguishable from one that cannot lose, and the
    /// live evidence on this issue is an existence proof over exactly one trajectory. This is the
    /// half that a transcript structurally cannot supply.
    ///
    /// Over 200 seeded zones × start positions, spanning voids, below-underworld decks, walls the
    /// body starts inside, and ordinary ground, it asserts three things at once:
    ///
    /// * **P_A (the universal):** whenever an `EmbeddedNoRecovery` hold is in force at the end of a
    ///   run, the independent oracle agrees there was nowhere to stand. Contrapositive: a zone with
    ///   somewhere to stand never ends in the frozen state. This is the property #845 is about.
    ///   Scoped to that variant on purpose — this PR deliberately does not change the underworld
    ///   arm, so claiming the universal over both would be claiming something the code does not do.
    /// * **P_B (no ping-pong):** at most two rescue-sized relocations per run. #649 measured the
    ///   failure mode where a net that keeps finding "somewhere better" walks a body across a zone
    ///   one ring-radius at a time; a rescue that fired every frame would satisfy P_A and be a new
    ///   bug.
    /// * **P_C (mobility after):** a body left un-held and grounded responds to a driver in at least
    ///   one of the four cardinal directions. Applied only to the wall-free zones, because a body
    ///   legitimately wedged in a corner would fail it for the right reason.
    /// * **P_D (the untouched arm is not absorbing):** a body holding `UnderworldNoRecovery` still
    ///   responds to a lateral drive. This is the premise the scope decision rests on — the reason
    ///   #845's rescue is NOT wired into `step`'s #150 guard — so it is measured here rather than
    ///   argued from reading the control flow.
    ///
    /// The counters are printed so a reader can see the family was not vacuous — a run in which
    /// nothing ever got stuck would pass P_A trivially. Both P_A and P_D carry an explicit vacuity
    /// assertion rather than relying on a human reading the printed line; P_D's first version was
    /// measured at **0 of 200** while passing, which is exactly the failure those guards exist for.
    #[test]
    fn every_reachable_controller_state_has_an_exit_845() {
        const CASES: usize = 200;
        const FRAMES: usize = 200; // 6.7 s at 30 Hz — ~13× STUCK_FALLBACK_SECS
        const DT: f32 = 1.0 / 30.0;
        let mut seed: u64 = 0x845_845_845_845;

        let (mut stuck_ever, mut rescued, mut held_end, mut mobile, mut mobility_cases) =
            (0usize, 0usize, 0usize, 0usize, 0usize);
        let (mut underworld_cases, mut underworld_drivable, mut oracle_checked) =
            (0usize, 0usize, 0usize);

        for case in 0..CASES {
            // ── zone ────────────────────────────────────────────────────────────────────────────
            let n_slabs = 1 + (a845_rand(&mut seed) * 3.0) as usize; // 1..=3
            let n_walls = (a845_rand(&mut seed) * 3.0) as usize;     // 0..=2
            let mut meshes = Vec::new();
            let mut lowest = f32::MAX;
            for _ in 0..n_slabs {
                let z = a845_range(&mut seed, -180.0, 180.0);
                let e0 = a845_range(&mut seed, -500.0, 300.0);
                let n0 = a845_range(&mut seed, -500.0, 300.0);
                let (w, d) = (a845_range(&mut seed, 256.0, 512.0), a845_range(&mut seed, 256.0, 512.0));
                lowest = lowest.min(z);
                meshes.push(slab(z, e0, e0 + w, n0, n0 + d));
            }
            for _ in 0..n_walls {
                let e = a845_range(&mut seed, -300.0, 300.0);
                let n0 = a845_range(&mut seed, -300.0, 200.0);
                let h0 = a845_range(&mut seed, -200.0, 150.0);
                meshes.push(wall_seg(e, n0, n0 + a845_range(&mut seed, 50.0, 300.0),
                                     h0, h0 + a845_range(&mut seed, 10.0, 80.0)));
            }
            let underworld = lowest - 20.0;
            // Every other zone gets a wide deck BELOW the underworld — the #712 shape, and the only
            // way this family can reach `step`'s #150 guard at all. Without it a body that falls
            // past every slab has nothing within `GROUND_DEPTH` beneath it, so `is_embedded`'s void
            // disjunct is true, `depenetrate` early-returns, and the gravity path is never taken:
            // the first version of P_D was measured VACUOUS at 0/0 for exactly this reason. With
            // the deck the body lands in the guard's band instead and the guard has to refuse it.
            let deck = case % 2 == 0;
            if deck { meshes.push(slab(underworld - 40.0, -400.0, 400.0, -400.0, 400.0)); }
            let c = col(meshes);

            // ── body ────────────────────────────────────────────────────────────────────────────
            let start = [a845_range(&mut seed, -300.0, 300.0),
                         a845_range(&mut seed, -300.0, 300.0),
                         a845_range(&mut seed, -200.0, 200.0)];
            let mut ctrl = CharacterController::new(start);
            ctrl.set_underworld(Some(underworld));

            let mut jumps = 0usize;
            let mut ever_stuck = false;
            for _ in 0..FRAMES {
                let before = ctrl.pos;
                ctrl.step(walk(0.0, [0.0, 0.0]), DT, &c);
                let dxy = ((ctrl.pos[0] - before[0]).powi(2) + (ctrl.pos[1] - before[1]).powi(2)).sqrt();
                // Larger than the push-out can ever move a body (`PUSHOUT_RADII` tops out at 32),
                // so this counts last-resort relocations and nothing else.
                if dxy > 32.0 { jumps += 1; }
                if ctrl.stuck_time >= STUCK_FALLBACK_SECS || ctrl.hold().is_some() { ever_stuck = true; }
            }
            // ⚠️ The obvious counter — "did I ever observe `stuck_time >= STUCK_FALLBACK_SECS`
            // after a step" — UNDERCOUNTS by an order of magnitude, and the first version of this
            // test failed its own vacuity guard because of it (5 of 200, measured). `recover()`
            // zeroes `stuck_time`, and the last resort runs through `recover()`, so every case the
            // rescue SUCCEEDS on has already had the evidence erased by the time the loop looks.
            // A rescue-sized jump is that evidence: the arm is reachable only from the stuck
            // fallback. With it: 123 of 200.
            if ever_stuck || jumps > 0 || ctrl.hold().is_some() { stuck_ever += 1; }
            if jumps > 0 { rescued += 1; }

            // P_A — the universal, for the arm this PR changes.
            if let Some(h) = ctrl.hold() {
                held_end += 1;
                if h.reason == ControllerHoldReason::EmbeddedNoRecovery {
                    oracle_checked += 1;
                    let oracle = a845_oracle(&c, ctrl.pos, underworld);
                    assert!(oracle.is_none(),
                        "#845 case {case}: the controller is held ({:?}) at {:?} while an \
                         independent dense-grid scan of the SAME zone found a standable place at \
                         {:?} — a reachable state with an exit the controller did not take \
                         (underworld {underworld:.1})",
                        h.reason, ctrl.pos, oracle);
                }
            }

            // P_B — no ping-pong.
            assert!(jumps <= 2,
                "#845 case {case}: {jumps} rescue-sized relocations in {FRAMES} frames — the last \
                 resort must fire once, not walk the body across the zone (#649)");

            // P_C — a placed body can be driven.
            if n_walls == 0 && ctrl.hold().is_none() && ctrl.on_ground {
                mobility_cases += 1;
                let base = ctrl.pos;
                let mut moved = false;
                for dir in [[1.0, 0.0], [-1.0, 0.0], [0.0, 1.0], [0.0, -1.0]] {
                    // `CharacterController` is not `Clone`, so each direction is probed on a fresh
                    // controller placed at the same coordinate. A fresh ring is harmless here: this
                    // asks only whether the position is drivable, not how it got there.
                    let mut probe = CharacterController::new(base);
                    probe.on_ground = true;
                    probe.set_underworld(Some(underworld));
                    for _ in 0..30 { probe.step(walk(20.0, dir), DT, &c); }
                    if ((probe.pos[0] - base[0]).powi(2) + (probe.pos[1] - base[1]).powi(2)).sqrt() > 1.0 {
                        moved = true;
                        break;
                    }
                }
                if moved { mobile += 1; }
                assert!(moved,
                    "#845 case {case}: body left un-held and grounded at {base:?} but no cardinal \
                     drive moved it — un-held is supposed to mean drivable");
            }

            // P_D — the claim the SCOPE rests on, measured instead of read. This PR does not touch
            // `step`'s #150 fall-through guard, and the stated reason is that its no-history arm is
            // NOT absorbing: it runs after collide-and-slide, so lateral driver input has already
            // reached the body. That is a universal about a state I am deliberately leaving in
            // place, so it gets tested rather than asserted in prose. Restricted to wall-free zones
            // for the same reason P_C is — a body wedged in a corner would fail it for the right
            // reason. NOTE this drives the real `ctrl`, so it must stay last in the case body.
            if n_walls == 0
                && matches!(ctrl.hold(), Some(h) if h.reason == ControllerHoldReason::UnderworldNoRecovery)
            {
                underworld_cases += 1;
                let base = ctrl.pos;
                for _ in 0..30 { ctrl.step(walk(20.0, [1.0, 0.0]), DT, &c); }
                let d = ((ctrl.pos[0] - base[0]).powi(2) + (ctrl.pos[1] - base[1]).powi(2)).sqrt();
                assert!(d > 1.0,
                    "#845 case {case}: a body holding `underworld_no_recovery` at {base:?} did not \
                     respond to a lateral drive (moved {d:.2} u) — if this is RED then that arm IS \
                     absorbing after all, and the scope decision in this PR is wrong");
                underworld_drivable += 1;
            }
        }
        // P_D's own vacuity guard. It is an assertion about a state the generator has to REACH, and
        // the first version reached it zero times out of 200 while passing.
        assert!(underworld_cases >= 10,
            "P_D never exercised the arm it is about: only {underworld_cases} of {CASES} cases \
             ended in an `underworld_no_recovery` hold in a wall-free zone");

        println!("#845 property: {CASES} cases, {stuck_ever} reached the stuck/held branch, \
                  {rescued} were relocated by the last resort, {held_end} ended held \
                  (of which {oracle_checked} were `embedded_no_recovery` and checked against the \
                  oracle), {mobile}/{mobility_cases} drivable-after checks passed, \
                  {underworld_drivable}/{underworld_cases} underworld holds still drivable");
        // Not an assertion about the FIX — an assertion about the FAMILY. If the generator drifts
        // to zones where nothing ever gets stuck, P_A passes vacuously and this test stops being
        // evidence. Re-tune the generator rather than lowering this.
        assert!(stuck_ever >= CASES / 10,
            "the generated family must actually exercise the stuck branch; only {stuck_ever} of \
             {CASES} cases did");
        // P_A's own family guard, and the one to read carefully. `oracle_checked` is SMALL (single
        // digits, measured) and that is not a defect: P_A's antecedent is "ended `embedded_no_recovery`",
        // and making that antecedent rare is the entire point of the fix. The evidence that P_A is
        // not vacuous is therefore the OTHER side of it — the cases that entered the arm and were
        // let out again, counted by `rescued`. If this ever goes RED, the generator has stopped
        // reaching the arm and P_A has stopped meaning anything, whatever `oracle_checked` says.
        assert!(rescued >= CASES / 4,
            "the family must actually exercise the last resort: only {rescued} of {CASES} cases \
             were relocated, so P_A's antecedent is untested rather than rare");
    }

    #[test]
    fn fall_through_guard_never_descends_below_underworld() {
        // A gap that would drop the character onto deep below-world boundary geometry at z=-300
        // (below the zone's underworld floor -189), plus a normal floor at z=0 above. The guard must
        // refuse to sink below underworld and recover to the last good grounded position. (#150)
        let c = col(vec![floor(0.0, -100.0, 100.0), floor(-300.0, -100.0, 100.0)]);
        let mut ctrl = CharacterController::new([0.0, 0.0, -188.0]); // already dropped just above underworld
        ctrl.set_underworld(Some(-189.0));
        ctrl.vel_z = -50.0;                 // falling fast toward the boundary
        ctrl.good.push_back([1.0, 2.0, 3.0]); // a known-good grounded position (on the z=0 floor)

        ctrl.step(walk(0.0, [0.0, 0.0]), 0.1, &c);

        assert!(ctrl.pos[2] >= -189.0, "must not sink to/below underworld: z={}", ctrl.pos[2]);
        assert_eq!(ctrl.pos, [1.0, 2.0, 3.0], "should recover to the last good grounded position");
        assert!(ctrl.on_ground, "recovered position is treated as grounded");
    }

    #[test]
    fn fall_through_guard_disabled_when_underworld_unknown() {
        // With no underworld set (default), the guard must not fire — a normal fall onto real floor
        // below still lands there, unchanged from prior behavior.
        let c = col(vec![floor(-50.0, -100.0, 100.0)]);
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        // underworld left at its NEG_INFINITY default (set_underworld never called).
        for _ in 0..40 { ctrl.step(walk(0.0, [0.0, 0.0]), 0.1, &c); }
        assert!((ctrl.pos[2] - (-50.0)).abs() < 0.5, "falls to and lands on the real floor at -50: {}", ctrl.pos[2]);
        assert!(ctrl.on_ground);
    }

    /// §442 (#442) — the unified controlled fall is COLLIDED. A fall onto a floor with an intervening
    /// SOLID floor in between must STOP on the solid, not pass through to the floor below: the descent
    /// consults `ground_below` every frame (the ONE collided `step`), it is not a raw `gs.player_z`
    /// write like the retired nav big-drop path. Mirrors `p3_collided_swim_does_not_embed_under_a_flush_ceiling`.
    ///
    /// MUTATION-CHECK: temporarily replacing the landing arm in `step`'s gravity block with a raw,
    /// un-collided descent (always `self.pos[2] = cand`, ignoring `floor`) drops the character
    /// straight through z=20 to the bottom (and, with no underworld set, keeps sinking) — this test
    /// then goes RED. Restore the collided landing arm to make it green again.
    #[test]
    fn controlled_fall_collides_with_intervening_geometry() {
        // A high start, a SOLID intervening floor at z=20, and the real floor at z=0 below it.
        let c = col(vec![floor(0.0, -100.0, 100.0), floor(20.0, -100.0, 100.0)]);
        let mut ctrl = CharacterController::new([0.0, 0.0, 50.0]);
        ctrl.on_ground = false; // airborne, about to fall
        for _ in 0..300 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &c); }
        assert!((ctrl.pos[2] - 20.0).abs() < 0.5,
            "collided fall must land ON the intervening solid at z=20, not pass through it, got {}", ctrl.pos[2]);
        assert!(ctrl.pos[2] > 15.0, "must NOT have fallen through to the bottom floor at z=0: {}", ctrl.pos[2]);
        assert!(ctrl.on_ground, "must be grounded after landing on the solid");
    }

    /// §442 (#442) — the fall-damage signal is driver-agnostic, edge-triggered, and consumed once.
    /// A genuine airborne stretch (floor drops away → fall → land) latches ONE `landed_fall_height`
    /// equal to the drop the controller ITSELF tracked (airborne start − landing z), not a waypoint z.
    #[test]
    fn fall_damage_signal_fires_once_from_airborne_height() {
        // Only a floor at z=0; the controller claims grounded 30u above it, so frame 1 sees the floor
        // drop away (a genuine true→false transition) and records the airborne start at z=30.
        let c = col(vec![floor(0.0, -100.0, 100.0)]);
        let mut ctrl = CharacterController::new([0.0, 0.0, 30.0]);
        ctrl.on_ground = true;
        for _ in 0..300 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(ctrl.on_ground && ctrl.pos[2].abs() < 0.5, "should have landed on the z=0 floor: {:?}", ctrl.pos);
        let h = ctrl.take_landed_fall_height();
        assert!(h.is_some_and(|h| (h - 30.0).abs() < 1.0),
            "landed-fall-height must equal the tracked airborne drop (~30), got {h:?}");
        // Edge-triggered + consumed once: a second take yields nothing.
        assert!(ctrl.take_landed_fall_height().is_none(), "the fall signal must fire exactly once");
    }

    /// §442 (#442) hazard 2b — a teleport / server correction MID-FALL must NOT be misread as a fall
    /// landing: `teleport` clears the airborne tracking, so the settle onto the floor emits nothing.
    #[test]
    fn teleport_mid_fall_emits_no_fall_damage() {
        let c = col(vec![floor(0.0, -100.0, 100.0)]);
        let mut ctrl = CharacterController::new([0.0, 0.0, 30.0]);
        ctrl.on_ground = true;
        for _ in 0..15 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &c); } // begin falling
        assert!(!ctrl.on_ground, "should be airborne after the floor dropped away");
        ctrl.teleport([0.0, 0.0, 0.0]); // server correction snaps us onto the floor
        for _ in 0..120 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(ctrl.on_ground, "settled on the floor after the teleport");
        assert!(ctrl.take_landed_fall_height().is_none(),
            "a teleport/server correction must suppress the fall-damage signal");
    }

    /// §442 (#442) hazard 2a — a mid-fall depenetration / ground-snap grounding (the anti-embed net
    /// flicking `on_ground` true) is a RECOVERY, not a genuine landing: it must emit no (spurious,
    /// partial-height) fall damage. The net clears the airborne tracking and never latches a height.
    #[test]
    fn depenetration_recovery_mid_fall_emits_no_fall_damage() {
        // Floor everywhere plus two close walls boxing the origin (footprint pierced → embedded),
        // as in `depenetrates_embedded_point_to_clear_floor`.
        let c = col(vec![floor(0.0, -100.0, 100.0), wall(0.8, 0.0, 10.0), wall(-0.8, 0.0, 10.0)]);
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        ctrl.on_ground = false;
        ctrl.airborne_start_z = Some(50.0); // pretend we are mid-fall from z=50
        ctrl.step(walk(0.0, [0.0, 0.0]), 0.05, &c);
        assert!(ctrl.on_ground, "the depenetration net should have grounded us");
        assert!(ctrl.take_landed_fall_height().is_none(),
            "a depenetration/ground-snap grounding mid-fall must NOT emit fall damage");
        assert!(ctrl.airborne_start_z.is_none(), "the net must clear the stale airborne start");
    }

    /// §442 (#442) DEFECT-1 — water BREAKS a fall: no phantom fall damage on stepping out onto shore.
    /// Fall off a cliff (airborne start recorded) INTO a lake, float, then walk onto dry ground — the
    /// dry-land step-out must NOT latch the stale pre-water airborne height (which would be lethal).
    /// This is the agent-honesty defect: a calm swim-out must never register a phantom HP drop/death.
    ///
    /// MUTATION-CHECK: removing the `self.airborne_start_z = None` clears from the water branches
    /// (swim + submerged/buoyancy) makes this RED — the shore landing then latches
    /// `Some(pre_water_start − shore_z)` and `take_landed_fall_height()` returns `Some(..)`.
    #[test]
    fn water_breaks_a_fall_no_phantom_damage_on_shore() {
        // Phase 1 — deep water z∈[0,30] over a pool floor at z=0. Start "grounded" 60u up so frame 1
        // sees the floor drop away (airborne start = 60), then fall INTO the lake and float.
        let water_c = {
            let mut c = col(vec![floor(0.0, -100.0, 100.0)]);
            c.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::water_slab(0.0, 30.0))));
            c
        };
        let mut ctrl = CharacterController::new([0.0, 0.0, 60.0]);
        ctrl.on_ground = true;
        for _ in 0..240 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &water_c); }
        assert!(ctrl.in_water, "phase 1 must have fallen into the water: z={}", ctrl.pos[2]);
        assert!(ctrl.airborne_start_z.is_none(), "water must end the airborne episode (clear the start)");

        // Phase 2 — the character walks out onto a DRY shore (no water, dry floor at z=0 below the
        // float line). Landing here must NOT latch the stale z=60 airborne start.
        let shore = col(vec![floor(0.0, -100.0, 100.0)]);
        for _ in 0..240 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &shore); }
        assert!(ctrl.on_ground, "should have come to rest on the dry shore floor: z={}", ctrl.pos[2]);
        assert!(ctrl.take_landed_fall_height().is_none(),
            "water broke the fall — stepping onto shore must apply NO (phantom) fall damage");
    }

    /// #444 — exiting the BOTTOM of a SUSPENDED water volume (a floating slab with open air below it,
    /// e.g. a pathological `.wtr` shape over a pit) must resume normal fall tracking. Pre-fix: the
    /// swim branch unconditionally clears `airborne_start_z` (§442 DEFECT-1, water breaks a fall) and
    /// leaves `on_ground = false`; the next frame reads `in_water = false` and falls straight into the
    /// dry-gravity branch, but that branch only re-arms `airborne_start_z` from GROUNDED transitions
    /// (jump, floor-drops-away-while-on_ground) — neither of which fires here because we are already
    /// airborne. The fall through the pit below then lands with no tracked start: no landing height,
    /// no fall-damage signal, and (agent-honesty) the character would still read as having just been
    /// swimming rather than plummeting.
    ///
    /// MUTATION-CHECK: disabling the
    /// `was_in_water && was_swim_sinking && !self.in_water && !self.on_ground && ...` re-arm added
    /// for #444 (verified by short-circuiting it to `false`) makes this RED —
    /// `take_landed_fall_height()` returns `None` instead of `Some(height)`. The
    /// `was_swim_sinking` gate itself is also load-bearing: swapping it for a bare `was_in_water`
    /// check turns `water_breaks_a_fall_no_phantom_damage_on_shore` (below) RED instead — a
    /// passively-floating character whose water vanishes must NOT get a phantom fall height.
    #[test]
    fn exiting_the_bottom_of_a_suspended_water_volume_resumes_the_fall() {
        // A suspended water slab z∈[0,30] floating over an OPEN PIT (no geometry from -50..0) with the
        // real floor down at z=-50 — the "floating slab over a drop" shape from #444.
        let c = {
            let mut c = col(vec![floor(-50.0, -100.0, 100.0)]);
            c.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::water_slab(0.0, 30.0))));
            c
        };
        // Start submerged near the bottom of the slab, deliberately swimming DOWN and out of it.
        let mut ctrl = CharacterController::new([0.0, 0.0, 5.0]);
        ctrl.on_ground = false;
        let swim_down = MoveIntent { wish_dir: [0.0, 0.0], wish_vspeed: -15.0, jump: false,
            want_swim: true, speed: 0.0, climb: 0.0, hop: false };

        // Drive it down through the water bottom into the pit; stop as soon as we're clearly out of
        // the water and still above the pit floor, so we can inspect the mid-fall state.
        let mut exited = false;
        for _ in 0..600 {
            ctrl.step(swim_down, 1.0 / 60.0, &c);
            if !ctrl.in_water && ctrl.pos[2] < -1.0 && ctrl.pos[2] > -45.0 { exited = true; break; }
        }
        assert!(exited, "should have exited the water bottom into the open pit: z={}, in_water={}",
            ctrl.pos[2], ctrl.in_water);
        assert!(!ctrl.on_ground, "must be airborne in the pit, not grounded: {:?}", ctrl.pos);
        assert!(ctrl.airborne_start_z.is_some(),
            "water-bottom exit must re-arm a fresh airborne start, not leave it cleared");

        // Let gravity carry it the rest of the way down to the real floor.
        for _ in 0..300 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(ctrl.on_ground, "should have landed on the pit floor at z=-50: {:?}", ctrl.pos);
        assert!((ctrl.pos[2] + 50.0).abs() < 0.5, "should be resting at z=-50: {}", ctrl.pos[2]);
        let h = ctrl.take_landed_fall_height();
        assert!(h.is_some_and(|h| h > 5.0),
            "the post-exit air-fall must be tracked and reported (nonzero landed height), got {h:?}");
    }

    /// #444 (PR #511 review) — the water-exit re-arm must key off GENUINE descent, not down-INTENT.
    /// The `.wtr`-gap shape family: a pond whose lateral edge sits on a floor that continues FLUSH
    /// outside the water region. A character RESTING on that submerged floor while holding a down-wish
    /// (extremely common in the real WASD path, where `wish_vspeed = dz·speed` couples the camera
    /// pitch into a downward vertical component whenever you swim forward looking even slightly down)
    /// drifts LATERALLY out of the water — never through the bottom. `swim_sink` clamps to ~0 against
    /// the floor, so there is NO real fall: the character only moved sideways and stayed on the floor.
    /// It must latch NO fall height. Pre-tightening this false-positived (the gate was
    /// `wish_vspeed < 0` alone), re-arming `airborne_start_z` and latching a spurious `Some(~0)` —
    /// a phantom "fall" for a purely lateral exit, violating the §442 DEFECT-1 invariant (inert
    /// today only because `SAFE_FALL_HEIGHT` discards it, but wrong).
    ///
    /// MUTATION-CHECK: reverting the gate to `if want < 0.0 { self.swim_sinking = true; }` (the
    /// down-intent sign, ignoring the resolved `swim_sink` delta) turns this RED —
    /// `take_landed_fall_height()` returns `Some(_)` instead of `None`. Keep
    /// `exiting_the_bottom_of_a_suspended_water_volume_resumes_the_fall` (a REAL bottom-exit) and
    /// `water_breaks_a_fall_no_phantom_damage_on_shore` green.
    #[test]
    fn lateral_swim_exit_over_a_flush_floor_latches_no_fall() {
        // Flush floor at z=0 spanning the whole scene; a water pond ONLY over east∈[-100,0] up to
        // z=10, so the SAME floor continues dry outside the pond (the .wtr-gap shape). The floor is
        // submerged under the pond, so a character on it is fully in water while resting.
        let c = {
            let mut c = col(vec![floor(0.0, -100.0, 100.0)]);
            c.set_water(Some(std::sync::Arc::new(
                crate::region_map::RegionMap::box_below(-100.0, 100.0, -100.0, 0.0, 10.0),
            )));
            c
        };
        // Rest a hair above the submerged floor, INSIDE the pond (east=-20), so `swim_sink` clamps
        // against the floor (~0) — the character never actually descends.
        let mut ctrl = CharacterController::new([-20.0, 0.0, 0.05]);
        ctrl.on_ground = false;
        assert!(c.in_water([ctrl.pos[0], ctrl.pos[1], ctrl.pos[2]]), "must start submerged in the pond");
        // Swim EAST (lateral) while holding a persistent DOWN wish — the look-slightly-down case.
        let swim_out = MoveIntent { wish_dir: [1.0, 0.0], wish_vspeed: -15.0, jump: false,
            want_swim: true, speed: 40.0, climb: 0.0, hop: false };

        // Drive east until we've drifted out of the pond AND settled back onto the (flush, dry)
        // floor, then stop — running further would walk the character off the finite test floor's
        // far edge, a genuine (unrelated) fall. Assert on every frame that no fall height is latched.
        let mut settled = false;
        for _ in 0..160 {
            ctrl.step(swim_out, 1.0 / 60.0, &c);
            // At no point may this purely lateral traversal latch a fall height.
            assert!(ctrl.take_landed_fall_height().is_none(),
                "lateral exit over a flush floor must never latch a fall height (e={} z={})",
                ctrl.pos[0], ctrl.pos[2]);
            if !ctrl.in_water && ctrl.pos[0] > 1.0 {
                // Out of the pond: the exit must NOT have re-armed a fall (§442 DEFECT-1 holds).
                assert!(ctrl.airborne_start_z.is_none(),
                    "a purely LATERAL water-exit (resting on a flush floor) must NOT re-arm a fall: start={:?}",
                    ctrl.airborne_start_z);
                if ctrl.on_ground { settled = true; break; }
            }
        }
        assert!(settled, "the character should have drifted east out of the pond and settled on the dry flush floor: {:?}", ctrl.pos);
        assert!(ctrl.pos[2].abs() < 0.5, "should be at the flush floor z=0, not sunk/fallen: {}", ctrl.pos[2]);
    }

    /// §442 (#442) DEFECT-2 — a nav auto-hop begins an airborne stretch too: the hop-launch path must
    /// record the airborne start so a hop that lands lower still reports its fall height (else the hop
    /// off a fence at an edge would deal no damage). Hop a thin fence onto a floor 3u lower.
    #[test]
    fn hop_off_a_fence_latches_a_fall_height() {
        // Floor z=0 for east<5, a thin fence (z=0..5) at east=5, and floor z=-3 beyond (a drop within
        // the hop probe band so `can_hop` fires). The hop launches from z=0 and lands on z=-3.
        let geo = col(vec![floor(0.0, -100.0, 5.0), wall(5.0, 0.0, 5.0), floor(-3.0, 5.0, 100.0)]);
        let nav_intent = MoveIntent { wish_dir: [1.0, 0.0], wish_vspeed: 0.0, jump: false,
            want_swim: false, speed: 35.0, climb: 0.0, hop: true };
        let mut ctrl = CharacterController::new([2.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        for _ in 0..80 { ctrl.step(nav_intent, 0.05, &geo); }
        assert!(ctrl.pos[0] > 6.0, "nav should have hopped past the fence: east={}", ctrl.pos[0]);
        assert!((ctrl.pos[2] + 3.0).abs() < 0.5, "should land on the z=-3 floor beyond: {}", ctrl.pos[2]);
        let h = ctrl.take_landed_fall_height();
        assert!(h.is_some_and(|h| (h - 3.0).abs() < 1.0),
            "the hop-launch must record the airborne start so the 3u drop is reported, got {h:?}");
    }

    // ── #712: a walk-in arrival must not end wedged in the PREVIOUS zone's coordinates ──────────
    //
    // Measured live (lfaydark → steamfont), and the numbers below are those measurements, not a
    // sketch: the server corrected the character to (2205, 579, −114.4); steamfont's baked floor in
    // that column is −113.25 — 1.15 u OVERHEAD, i.e. just past the ground clamp's `foot + 1.0`
    // probe origin and therefore invisible to it — with the next floor down at −232.0 and the zone
    // underworld at −222.0. The body free-fell past the underworld, the #150 guard recovered it to
    // the newest `good` sample, and that sample was still the LFAYDARK position
    // (−2190.08, 911.27, −4.78). Steamfont has no geometry at all in that column (nearest standable
    // floor 133 u away), so `is_embedded` was permanently true, no ring candidate yielded a
    // `Recovery`, and the 0.5 s stuck fallback restored the same point every 0.5 s for ever.
    //
    // Two independent defects, fixed separately and pinned separately: the reground stops the fall,
    // the ring clear stops any fall from being able to land in another zone's coordinate space.

    /// The measured steamfont arrival column: arrival floor at −113.25, next surface 117 u down at
    /// −232.0 — which is BELOW the zone's measured underworld of −222.0, and that, not the 1.15 u
    /// gap above, is what makes this position unrecoverable and so what the reground keys on.
    fn steamfont_arrival_column() -> Collision {
        col(vec![floor(-113.25, -100.0, 100.0), floor(-232.0, -100.0, 100.0)])
    }

    #[test]
    fn zone_in_reground_lifts_a_body_that_arrived_just_under_the_arrival_floor() {
        let c = steamfont_arrival_column();
        let p = [0.0, 0.0, -114.4];
        assert!(c.ground_below(p[0], p[1], p[2] + GROUND_ORIGIN, GROUND_DEPTH).is_some(),
            "fixture: there IS a floor below (the −232 deck) — that is exactly why the old \
             'no floor at all below' condition declared this settled and let the body fall");
        match zone_in_reground(&c, p, Some(-222.0)) {
            Reground::Lift(f) => assert!((f + 113.25).abs() < 1e-2,
                "#712: must lift onto the arrival floor −113.25, got {f}"),
            other => panic!("#712: a body 1.15u under its arrival floor must be lifted, got {other:?}"),
        }
    }

    // The next two are the #720 reviewer's constructions, reproduced verbatim as acceptance
    // criteria. Both were RED when first added — the reviewer's findings are confirmed, not taken
    // on trust.

    #[test]
    fn zone_in_reground_does_not_mount_an_airborne_arrival_onto_an_overhead_slab() {
        // Ground at −3, a walkable slab top 2.4 u OVERHEAD, body arriving airborne at z=0 with a
        // perfectly ordinary 3 u drop ahead of it. `nearest_floor` anchors on distance to the
        // body, so its own floor is outside the [z−1, z+3] band and the slab wins by default —
        // which would teleport the character ON TOP of the bridge it was arriving under.
        let c = col(vec![floor(-3.0, -100.0, 100.0), floor(2.4, -100.0, 100.0)]);
        let p = [0.0, 0.0, 0.0];
        assert!(c.ground_below(p[0], p[1], p[2] + GROUND_ORIGIN, GROUND_DEPTH)
                 .is_some_and(|f| (f + 3.0).abs() < 1e-2),
            "fixture: the body must have landable ground 3 u below it");
        assert!(c.nearest_floor(p[0], p[1], p[2], 3.0, 1.0).is_some_and(|f| (f - 2.4).abs() < 1e-2),
            "fixture: and a standable slab inside the upward band, or this proves nothing");
        assert_eq!(zone_in_reground(&c, p, Some(-222.0)), Reground::Retire,
            "#720 B1: a body with landable ground beneath it must be left to FALL onto it, not \
             lifted onto whatever happens to be overhead");
    }

    #[test]
    fn zone_in_reground_never_hauls_a_swimmer_onto_a_submerged_shelf() {
        // #649 made "in water AND on_ground" unrepresentable through `Recovery`; the reground must
        // not reach that state by another route.
        //
        // The reviewer's construction — pool bottom −20, shelf +2, water to +10, body at 0 — is
        // asserted first, but note WHY it passes: the −20 bottom is above the underworld, so the
        // landability gate retires it before the water check is ever consulted. Mutation-checked and
        // confirmed: deleting the water branch leaves that case green, so on its own it does not
        // pin the swimmer rule at all.
        let shallow = flooded_corridor(
            vec![floor(-20.0, -100.0, 100.0), floor(2.0, -100.0, 100.0)], -20.0, 10.0);
        assert_eq!(zone_in_reground(&shallow, [0.0, 0.0, 0.0], Some(-222.0)), Reground::Retire,
            "#720 B2 (reviewer's construction; retired by the landability gate)");

        // So here is the same shape with the gate taken away: deep water whose bottom is 230 u down
        // and BELOW the underworld, i.e. exactly the "nowhere legal to land" state the reground
        // exists to act on — with a submerged shelf 2 u overhead. Only the water short-circuit
        // stands between this swimmer and being planted on that shelf.
        let deep = flooded_corridor(
            vec![floor(-230.0, -100.0, 100.0), floor(2.0, -100.0, 100.0)], -230.0, 10.0);
        let p = [0.0, 0.0, 0.0];
        assert!(body_in_water(&deep, p), "fixture: the body must be in water");
        assert!(deep.ground_below(p[0], p[1], p[2] + GROUND_ORIGIN, GROUND_DEPTH).is_none(),
            "fixture: and NOTHING landable below, or the gate retires before the water check and \
             this test pins nothing");
        assert!(deep.nearest_floor(p[0], p[1], p[2], GROUND_DEPTH, GROUND_ORIGIN)
                    .is_some_and(|f| (f - 2.0).abs() < 1e-2),
            "fixture: with a shelf the lift branch WOULD take");
        assert_eq!(zone_in_reground(&deep, p, Some(-222.0)), Reground::Retire,
            "#720 B2: the reground must not act on a swimmer at all — the swim/buoyancy branch \
             owns that body, and `app.rs` marks every Lift `on_ground`");
    }

    #[test]
    fn zone_in_reground_leaves_a_body_that_is_standing_on_its_floor() {
        let c = col(vec![floor(0.0, -100.0, 100.0)]);
        assert_eq!(zone_in_reground(&c, [0.0, 0.0, 0.0], Some(-222.0)), Reground::Retire);
    }

    #[test]
    fn zone_in_reground_does_not_yank_a_body_up_through_the_terrain_above_it() {
        // Surface at z=0, a cellar/cave floor at z=−12, body arriving in between with 2 u to fall.
        // It is somewhere else entirely, and it has landable ground of its own — so it falls onto
        // that, and is not teleported 10 u up through solid rock.
        let c = col(vec![floor(0.0, -100.0, 100.0), floor(-12.0, -100.0, 100.0)]);
        assert!(c.ground_below(0.0, 0.0, -10.0 + GROUND_ORIGIN, GROUND_DEPTH)
                 .is_some_and(|f| f > -222.0),
            "fixture: the body must have LANDABLE ground beneath it (above the underworld)");
        assert_eq!(zone_in_reground(&c, [0.0, 0.0, -10.0], Some(-222.0)), Reground::Retire,
            "#712: a body with a floor the fall-through guard would accept must be left to fall");
    }

    #[test]
    fn zone_in_reground_lifts_only_when_the_floor_below_is_under_the_underworld() {
        // The same column twice, differing ONLY in where the underworld sits — which is the whole
        // gate. Floor 10 u below the body: with the underworld beneath it that floor is a legal
        // landing and we do nothing; with the underworld above it the guard would refuse that
        // landing, so the body has nowhere to go but the surface overhead.
        let c = col(vec![floor(2.0, -100.0, 100.0), floor(-10.0, -100.0, 100.0)]);
        let p = [0.0, 0.0, 0.0];
        assert_eq!(zone_in_reground(&c, p, Some(-222.0)), Reground::Retire,
            "floor at −10 is above the underworld −222 → a legal landing, leave it alone");
        match zone_in_reground(&c, p, Some(-5.0)) {
            Reground::Lift(f) => assert!((f - 2.0).abs() < 1e-2, "expected Lift(2.0), got {f}"),
            other => panic!("floor at −10 is BELOW the underworld −5 → the guard would refuse it, \
                             so the body must be lifted onto the surface above; got {other:?}"),
        }
    }

    #[test]
    fn zone_in_reground_does_nothing_when_the_underworld_is_unknown() {
        // Same column as #712's, but with no underworld from OP_NewZone yet. The fall-through guard
        // is disabled in that state, so there is no wedge to pre-empt and we must not act.
        let c = steamfont_arrival_column();
        assert_eq!(zone_in_reground(&c, [0.0, 0.0, -114.4], None), Reground::Retire);
    }

    #[test]
    fn zone_in_reground_still_lifts_a_body_spawned_far_below_the_terrain() {
        // The case the block was originally written for, preserved: arrival z deep under the new
        // zone's floor with nothing at all beneath it — lifted from any distance.
        let c = col(vec![floor(0.0, -100.0, 100.0)]);
        match zone_in_reground(&c, [0.0, 0.0, -150.0], Some(-222.0)) {
            Reground::Lift(f) => assert!(f.abs() < 1e-2, "must lift onto the z=0 terrain, got {f}"),
            other => panic!("a body 150u below the terrain with nothing beneath it must be \
                             lifted, got {other:?}"),
        }
    }

    #[test]
    fn zone_in_reground_will_not_lift_onto_a_floor_that_is_itself_below_the_underworld() {
        // Body at −300 with a floor 10 u overhead at −290 — but the underworld is −222, so that
        // floor is not a place the fall-through guard would let anything rest either. Lifting onto
        // it would move the body 10 u and change nothing. `Wait` leaves the one-shot armed instead.
        let c = col(vec![floor(-290.0, -100.0, 100.0)]);
        let p = [0.0, 0.0, -300.0];
        assert!(c.ground_below(p[0], p[1], p[2] + GROUND_ORIGIN, GROUND_DEPTH).is_none(),
            "fixture: nothing below, so we reach the lift branch");
        assert!(c.nearest_floor(p[0], p[1], p[2], GROUND_DEPTH, GROUND_ORIGIN)
                 .is_some_and(|f| (f + 290.0).abs() < 1e-2),
            "fixture: with a floor above that the lift branch would otherwise take");
        assert_eq!(zone_in_reground(&c, p, Some(-222.0)), Reground::Wait,
            "the lift target has to be somewhere the guard would accept, not merely higher up");
    }

    #[test]
    fn zone_in_reground_waits_when_the_column_is_empty() {
        // Geometry exists, but not in this column — the one-shot must stay armed rather than
        // retire on a position with nothing to stand on.
        let c = col(vec![floor(0.0, 500.0, 600.0)]);
        assert_eq!(zone_in_reground(&c, [0.0, 0.0, 0.0], Some(-222.0)), Reground::Wait);
    }

    #[test]
    fn a_zone_change_forgets_the_previous_zones_recovery_ring() {
        // Zone A: stand on a floor long enough to bank a good sample (GOOD_SAMPLE_SECS = 0.5).
        let a = col(vec![floor(0.0, -100.0, 100.0)]);
        let mut ctrl = CharacterController::new([10.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        ctrl.set_underworld(Some(-222.0));
        for _ in 0..60 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &a); }
        let stale = *ctrl.good.back()
            .expect("fixture: zone A must bank a good sample, else this test proves nothing");

        // The zone change. `app.rs` drops the old collision here; it must drop this with it.
        ctrl.forget_recovery_history();
        assert!(ctrl.good.is_empty(),
            "#712: the previous zone's coordinates must not survive a zone change, got {:?}",
            ctrl.good);

        // Zone B: the arrival column has no floor within reach and the only deck is BELOW the
        // underworld, so the fall-through guard is guaranteed to fire — and zone A's coordinates
        // are still perfectly plausible floats, which is why the stale restore looked like success.
        let b = col(vec![floor(-232.0, -100.0, 100.0)]);
        ctrl.teleport([10.0, 0.0, -114.4]);
        for _ in 0..120 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &b); }

        assert!(ctrl.pos != stale,
            "#712: the guard recovered onto a PREVIOUS-zone coordinate {stale:?}");
        assert!(ctrl.pos[2] > -222.0,
            "the underworld guard must still hold the body above −222, got {:?}", ctrl.pos);
    }

    /// One zone, one hole: a platform the character banks good samples on (east −100…−50, z=0) and,
    /// far to the east, a deck at −232 that is BELOW the −222 underworld so nothing can land on it.
    /// Everything between is void. This is the same-zone shape of the #712 geometry.
    fn zone_with_a_hole() -> Collision {
        col(vec![floor(0.0, -100.0, -50.0), floor(-232.0, 40.0, 100.0)])
    }

    /// #724 — reachability of the stale-ring recovery after a LARGE SAME-ZONE relocation
    /// (GM summon / `#movechar` within a zone / any server correction over `CORRECTION_SQ` = 12 u).
    ///
    /// The relocation goes through [`CharacterController::teleport`], which — before this fix — left
    /// the last-good ring intact. The ring only banks while `on_ground`, so a body teleported into a
    /// column it can only fall out of NEVER banks a fresh sample: the stale window is not
    /// `GOOD_SAMPLE_SECS` wide as #724 supposed, it lasts until the body is grounded again, which
    /// for a fall-through is never. The #150 guard then restores the pre-summon coordinate and
    /// silently undoes the server's relocation.
    ///
    /// MUTATION-CHECK: delete the `self.good.clear()` line from `teleport` and this test FAILS on
    /// the `pos[0]` assertion with the body back at the pre-summon east.
    #[test]
    fn a_large_same_zone_relocation_forgets_the_pre_relocation_recovery_ring() {
        let c = zone_with_a_hole();
        let mut ctrl = CharacterController::new([-80.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        ctrl.set_underworld(Some(-222.0));
        // Stand on the platform long enough to bank good samples (GOOD_SAMPLE_SECS = 0.5).
        for _ in 0..60 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); }
        let stale = *ctrl.good.back()
            .expect("fixture: the platform must bank a good sample, else this test proves nothing");
        assert!((stale[0] - (-80.0)).abs() < 1e-3,
            "fixture: the banked sample is the pre-summon position, got {stale:?}");

        // The summon: 160 u east, far over the 12 u correction threshold, SAME zone — so the #712
        // zone-change clear in `app.rs` never runs and cannot help here. The arrival z is #712's
        // own measured one, which puts the deck below within `GROUND_DEPTH` so this is a genuine
        // FALL (the fixture assert below pins that) and not the embedded/depenetration vector the
        // next test covers.
        let target = [80.0, 0.0, -114.4];
        assert!(!is_embedded(&c, target),
            "fixture: the relocation target must take the gravity path, not the depenetration net");
        ctrl.teleport(target);

        for _ in 0..150 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); }

        // Bounded on BOTH sides: the body must still be where the server put it horizontally (not
        // merely "not exactly the stale point"), and held in the narrow band the guard leaves —
        // just above the underworld, having actually fallen. A one-sided `> -222` would pass for a
        // body that never moved at all, and `!= stale` would pass for any wrong answer but one.
        assert!(ctrl.pos != stale, "#724: recovered onto the superseded position {stale:?}");
        assert!((ctrl.pos[0] - 80.0).abs() < 1e-3 && ctrl.pos[1].abs() < 1e-3,
            "#724: the guard moved the body away from where the server relocated it: {:?} \
             (pre-relocation sample was {stale:?})", ctrl.pos);
        assert!(ctrl.pos[2] > -222.0 && ctrl.pos[2] < -217.0,
            "the body must have fallen and been HELD in the one-frame band just above the \
             underworld, got z={}", ctrl.pos[2]);
        assert!(ctrl.good.is_empty(),
            "#724: a position discontinuity must supersede the recovery ring, got {:?}", ctrl.good);
    }

    /// #724, second vector — the same stale ring is read by the DEPENETRATION stuck fallback, which
    /// #724 does not mention. A summon into geometry the push-out net cannot escape rubber-bands the
    /// body to `good.back()` after `STUCK_FALLBACK_SECS`, i.e. straight back out of the summon.
    ///
    /// MUTATION-CHECK: delete the `self.good.clear()` line from `teleport` and this test FAILS with
    /// the body back at the pre-summon platform.
    #[test]
    fn a_large_same_zone_relocation_forgets_the_ring_for_the_stuck_fallback_too() {
        // Platform to bank on, plus a walled slot with no floor anywhere near it: every push-out
        // radius finds no column that yields a `Recovery`, so the stuck fallback is the only exit.
        let c = col(vec![floor(0.0, -100.0, -50.0), wall(999.2, 0.0, 10.0), wall(1000.8, 0.0, 10.0)]);
        let mut ctrl = CharacterController::new([-80.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        ctrl.set_underworld(Some(-222.0));
        for _ in 0..60 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); }
        let stale = *ctrl.good.back().expect("fixture: must bank a good sample");

        // Fixture, checked against the pure predicate so it holds under the mutation too: the slot
        // is a place the body reads as embedded, with nothing in push-out range to recover onto.
        let target = [1000.0, 40.0, 0.0]; // summoned into the slot, 1050 u from the platform
        assert!(is_embedded(&c, target), "fixture: the slot must read as embedded");
        assert!(nearest_standing_place(&c, target, -222.0).is_none(),
            "fixture (#845): the last-resort search must find nowhere, else this test measures a \
             rescued body instead of a held one");

        ctrl.teleport(target);
        for _ in 0..40 { ctrl.step(walk(0.0, [0.0, 0.0]), 0.05, &c); } // 2 s ≫ STUCK_FALLBACK_SECS

        assert!(ctrl.pos != stale, "#724: stuck fallback restored the superseded position {stale:?}");
        assert!((ctrl.pos[0] - 1000.0).abs() < 1e-3 && (ctrl.pos[1] - 40.0).abs() < 1e-3,
            "#724: the body must be held where the server put it, got {:?}", ctrl.pos);
        // …and it is genuinely still STUCK there, i.e. the fallback branch really was reached and
        // declined for want of history — not a body that quietly walked out of the fixture.
        assert!(ctrl.stuck_time >= STUCK_FALLBACK_SECS,
            "the stuck fallback branch must have been reached (stuck_time={})", ctrl.stuck_time);
    }

    /// #724 — the UNIVERSAL. "A recovery never restores a position the server has superseded" is a
    /// claim about all relocations, not about the two shapes pinned above, so it gets a sweep rather
    /// than an example: 240 seeded combinations of pre-relocation stance, relocation target and
    /// post-relocation predicament (free fall through a hole vs. embedded in a walled slot), each
    /// asserting that no frame after the relocation ever puts the body on a pre-relocation sample.
    ///
    /// Deterministic — a hand-rolled xorshift, not a `proptest` dependency, so the sweep is exactly
    /// reproducible and adds nothing to `Cargo.lock`.
    ///
    /// MUTATION-CHECK: delete the `self.good.clear()` line from `teleport` and this test FAILS.
    #[test]
    fn no_recovery_ever_restores_a_position_a_relocation_superseded() {
        struct Xs(u32);
        impl Xs {
            fn next(&mut self) -> u32 {
                self.0 ^= self.0 << 13; self.0 ^= self.0 >> 17; self.0 ^= self.0 << 5; self.0
            }
            fn frac(&mut self) -> f32 { (self.next() % 10_000) as f32 / 10_000.0 }
        }
        let mut rng = Xs(0x5eed_0724);

        let mut fell_through = 0usize;
        let mut got_stuck = 0usize;
        for case in 0..240 {
            let embedded_case = case % 2 == 0;
            let c = if embedded_case {
                col(vec![floor(0.0, -100.0, -50.0), wall(999.2, 0.0, 10.0), wall(1000.8, 0.0, 10.0)])
            } else {
                zone_with_a_hole()
            };
            // Vary where the body stands before the relocation, and for how long.
            let start_e = -95.0 + rng.frac() * 40.0;
            let mut ctrl = CharacterController::new([start_e, -40.0 + rng.frac() * 80.0, 0.0]);
            ctrl.on_ground = true;
            ctrl.set_underworld(Some(-222.0));
            let settle = 20 + (rng.next() % 100) as usize;
            for _ in 0..settle { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); }
            let superseded: Vec<[f32; 3]> = ctrl.good.iter().copied().collect();
            if superseded.is_empty() { continue; } // too short a settle to bank; nothing to prove

            // Vary the relocation target. Both branches are ≫ 12 u from the platform, i.e. exactly
            // the corrections that reach `teleport` at all.
            let target = if embedded_case {
                [1000.0, 40.0, 0.0]
            } else {
                // z chosen so the sub-underworld deck is inside `GROUND_DEPTH` of the arrival:
                // the body then takes the gravity path and meets the #150 guard, rather than
                // reading as embedded (which is the other half of the sweep).
                [45.0 + rng.frac() * 50.0, -40.0 + rng.frac() * 80.0, -120.0 + rng.frac() * 60.0]
            };
            assert_eq!(is_embedded(&c, target), embedded_case,
                "case {case}: fixture must exercise the intended recovery path at {target:?}");
            if embedded_case {
                assert!(nearest_standing_place(&c, target, -222.0).is_none(),
                    "case {case} fixture (#845): the embedded half must have nowhere to be \
                     rescued to, else it stops exercising the stuck fallback");
            }
            ctrl.teleport(target);

            for f in 0..200 {
                ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c);
                for s in &superseded {
                    assert!(ctrl.pos != *s,
                        "case {case} frame {f}: recovery restored superseded position {s:?} \
                         (relocated to {target:?})");
                }
            }
            // Confirm the sweep actually exercised a recovery path rather than a body that simply
            // stood still: an embedded case must be stuck, a hole case must be held above the
            // underworld having fallen well below its relocation z.
            if embedded_case {
                if ctrl.stuck_time >= STUCK_FALLBACK_SECS { got_stuck += 1; }
            } else if ctrl.pos[2] > -222.0 && ctrl.pos[2] < target[2] - 20.0 {
                fell_through += 1;
            }
            assert!(ctrl.good.is_empty(),
                "case {case}: a superseded sample survived the relocation: {:?}", ctrl.good);
        }
        assert!(got_stuck >= 100, "sweep did not exercise the stuck fallback enough: {got_stuck}");
        assert!(fell_through >= 100, "sweep did not exercise the fall-through guard enough: {fell_through}");
    }

    /// Fixture shared by the hold tests: a platform to bank good samples on, plus a walled slot far
    /// away with no floor anywhere near it, so every push-out radius fails and the stuck fallback is
    /// the only exit. Identical to the fixture in
    /// `a_large_same_zone_relocation_forgets_the_ring_for_the_stuck_fallback_too`.
    ///
    /// ⚠️ AMENDED (#845): the slot moved from east 40 to east **1000**, and every user's relocation
    /// target moved with it. Nothing about what these tests assert changed — but the state they are
    /// about ("embedded with no recovery available") now requires that the WHOLE ZONE offer nowhere
    /// to stand, not merely that nothing is within push-out range. At east 40 the platform is ~120 u
    /// away, which the new last-resort search reaches, so the body would be rescued and these tests
    /// would be measuring a different state than the one their names claim. At east 1000 the
    /// platform is ~1050 u away, beyond `RESCUE_RADII`'s 512 u reach, and the premise holds again.
    /// Each user asserts that premise directly against `nearest_standing_place` so it cannot rot
    /// silently if the reach is ever raised.
    fn platform_and_inescapable_slot() -> Collision {
        col(vec![floor(0.0, -100.0, -50.0), wall(999.2, 0.0, 10.0), wall(1000.8, 0.0, 10.0)])
    }

    /// #724 round-2 review, **B1 — the mutation that catches the silent freeze.**
    ///
    /// #724 clears the recovery ring on every position discontinuity, which makes "embedded with an
    /// empty ring" the NORMAL post-relocation state rather than a rarity. In that state
    /// `depenetrate` changes nothing and returns `true`, so `step` skips the whole frame and the
    /// body is frozen for ever — and before this fix it was frozen SILENTLY: `depenetrate`'s only
    /// `tracing::info!` is inside `if let Some(&g) = self.good.back()`, and no agent-visible field
    /// carried any stuck/embedded signal. That is the "wedged but reporting normal" shape (#343/
    /// #679), which on this project outranks the wrong answer #724 removes.
    ///
    /// This test pins BOTH halves — the freeze is real, and it is now disclosed.
    ///
    /// MUTATION-CHECK: delete the `enter_hold` call from `depenetrate`'s `None` arm (i.e. restore
    /// the old `if let Some(&g)` shape) and this test FAILS on the `hold` assertion while every
    /// other test in this file stays green — which is exactly how the defect shipped.
    #[test]
    fn an_embedded_body_with_no_recovery_history_freezes_and_says_so() {
        let c = platform_and_inescapable_slot();
        let mut ctrl = CharacterController::new([-80.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        ctrl.set_underworld(Some(-222.0));
        for _ in 0..60 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); }
        assert!(ctrl.hold().is_none(),
            "fixture: a body standing on ordinary ground must NOT report a hold, else this test \
             would pass on a field that is always set");

        let target = [1000.0, 40.0, 0.0];
        assert!(is_embedded(&c, target), "fixture: the slot must read as embedded");
        assert!(nearest_standing_place(&c, target, -222.0).is_none(),
            "fixture (#845): the last-resort search must find nowhere, else this test measures a \
             rescued body instead of a held one");
        ctrl.teleport(target); // the relocation — clears the ring, per this PR

        // The freeze itself, measured rather than assumed: 2 s of frames, none of which move the
        // body by any amount.
        let mut moved_frames = 0usize;
        let mut last = ctrl.pos;
        for _ in 0..40 {
            ctrl.step(walk(0.0, [0.0, 0.0]), 0.05, &c);
            if ctrl.pos != last { moved_frames += 1; last = ctrl.pos; }
        }
        assert_eq!(moved_frames, 0,
            "fixture: the body must be genuinely frozen at {target:?}, got {:?}", ctrl.pos);
        assert!(ctrl.good.is_empty(), "fixture: the ring must be empty (that is the point)");

        // …and the freeze is DISCLOSED. This is the assertion the pre-review code fails.
        let h = ctrl.hold().expect(
            "#724 review B1: a body frozen with nothing to recover onto must report a hold — \
             otherwise an agent reads a perfectly plausible position and a perfectly normal state \
             while every movement command it issues does nothing");
        assert_eq!(h.reason, ControllerHoldReason::EmbeddedNoRecovery);
        // Duration is the controller's own accumulated frame time for the UNBROKEN hold. It starts
        // at STUCK_FALLBACK_SECS into the episode (the fallback branch is what discovers the hold),
        // so it is bounded below by "we really have been here a while" and above by the episode.
        assert!(h.secs > 1.0 && h.secs <= 2.0 + 1e-3,
            "the hold must report how long it has lasted, got {}", h.secs);
    }

    /// #724 round-2 review, B1 — **the clear path.** An observable that latches on and never clears
    /// is its own honesty bug (#343/#679), so the hold gets a test for going away, by both routes:
    /// the recovery becoming possible again, and a relocation out of the predicament.
    ///
    /// The clear is structural — `step` does `self.hold.take()` before anything can re-set it — so
    /// there is no "clear" code path to forget; this test pins that property rather than a branch.
    ///
    /// MUTATION-CHECK: change the `take()` at the top of `step` to a plain read and this test FAILS
    /// on the first `is_none` assertion.
    #[test]
    fn a_hold_clears_as_soon_as_the_body_is_free_again() {
        let c = platform_and_inescapable_slot();
        let mut ctrl = CharacterController::new([-80.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        ctrl.set_underworld(Some(-222.0));
        for _ in 0..60 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); }
        assert!(nearest_standing_place(&c, [1000.0, 40.0, 0.0], -222.0).is_none(),
            "fixture (#845): the last-resort search must find nowhere for the slot");
        ctrl.teleport([1000.0, 40.0, 0.0]);
        for _ in 0..40 { ctrl.step(walk(0.0, [0.0, 0.0]), 0.05, &c); }
        assert!(ctrl.hold().is_some(), "fixture: the body must be held before we test the clear");

        // Route 1 — the recovery becomes available. Hand the controller a good sample; the very next
        // stuck-fallback frame takes the branch that was previously empty, moves the body, and the
        // hold must be gone on that same frame.
        ctrl.good.push_back([-80.0, 0.0, 0.0]);
        ctrl.step(walk(0.0, [0.0, 0.0]), 0.05, &c);
        assert!(ctrl.hold().is_none(),
            "the hold must clear the frame the body is recovered, got {:?}", ctrl.hold());
        assert!((ctrl.pos[0] - (-80.0)).abs() < 1e-3, "fixture: the fallback really did fire");

        // Route 2 — a relocation out of the predicament. Get held again, then teleport somewhere
        // standable; the hold must not survive either the `teleport` itself or the next step.
        ctrl.teleport([1000.0, 40.0, 0.0]);
        for _ in 0..40 { ctrl.step(walk(0.0, [0.0, 0.0]), 0.05, &c); }
        assert!(ctrl.hold().is_some(), "fixture: held again");
        ctrl.teleport([-80.0, 0.0, 0.0]);
        assert!(ctrl.hold().is_none(), "a position discontinuity must supersede the hold too");
        for _ in 0..10 { ctrl.step(walk(0.0, [0.0, 0.0]), 0.05, &c); }
        assert!(ctrl.hold().is_none(),
            "…and it must not come back once the body is standing normally, got {:?}", ctrl.hold());
    }

    /// #724 round-2 review, B1 — the SECOND hold path, so the disclosure is symmetric with #720's.
    /// #720's review added the throttled hold log to the fall-through guard; #724 extends the empty
    /// ring to the depenetration path, and both now report the same way.
    ///
    /// MUTATION-CHECK: delete the `enter_hold` call from `step`'s underworld `else` arm and this
    /// test FAILS on the `hold` assertion.
    #[test]
    fn a_body_held_above_the_underworld_with_no_recovery_history_says_so_too() {
        let c = zone_with_a_hole();
        let mut ctrl = CharacterController::new([-80.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        ctrl.set_underworld(Some(-222.0));
        for _ in 0..60 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); }
        ctrl.teleport([80.0, 0.0, -114.4]);
        for _ in 0..150 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); }

        assert!(ctrl.pos[2] > -222.0 && ctrl.pos[2] < -217.0,
            "fixture: the guard must be holding the body just above the underworld, got {:?}", ctrl.pos);
        let h = ctrl.hold().expect(
            "#724 review B1: a body the fall-through guard is holding with nothing to restore must \
             report a hold");
        assert_eq!(h.reason, ControllerHoldReason::UnderworldNoRecovery);
        assert!(h.secs > 0.0, "the hold must report how long it has lasted, got {}", h.secs);
    }

    /// #724 round-2 review, **N4 — pin the `app.rs` zone-change call site**, which had nothing
    /// holding it in place.
    ///
    /// Review measured that deleting `self.controller.forget_recovery_history()` from the
    /// `zone_needs_reload` block in `app.rs`, together with the #712 test's own direct call and its
    /// `is_empty` assert, left the suite green (154 passed) — because that test's *behavioural*
    /// assertions are now satisfied by the `teleport` two lines below them rather than by the
    /// zone-change clear. The method was pinned; the call was not.
    ///
    /// It is not redundant, for one reason that needs no measurement and one that is only reasoned.
    /// The solid one: `app.rs` runs this at the moment the old zone's collision is dropped, which is
    /// earlier than and independent of any arrival, so no arrival-time `teleport` can be doing this
    /// job. The reasoned one, labelled as such: the #593 note in `eqoxide-net`'s `action_loop.rs`
    /// (`stream_position`) describes a cross-zone arrival landing within `CORRECTION_SQ` of the
    /// last-streamed OLD-zone position, where the correction branch is skipped and **no `teleport`
    /// fires at all** — that is read off the branch structure, not captured on the wire.
    ///
    /// A source scan rather than a behavioural test because `App` owns a window, a GPU and an event
    /// loop; the same `include_str!` technique is used in `eqoxide-net`'s `transport.rs` for exactly
    /// this kind of "one call site must not disappear" pin.
    ///
    /// # If this test reds and you did not touch the recovery ring — READ THIS BEFORE DELETING IT
    ///
    /// It scans another file's source, so **any** edit to `app.rs` that moves, renames, re-indents
    /// or re-wraps the `zone_needs_reload` block can red it — including an edit git merges cleanly,
    /// which makes it a cross-PR tripwire of the shape that has turned `main` red here before
    /// (#724 round-2 review, N7). It reds by design in that case; it is not a false positive to be
    /// silenced. Triage in this order:
    ///
    /// 1. **`expect` on `find` fired** (`app.rs no longer has the zone-change reload block…`): the
    ///    `if zone_needs_reload(…)` line was respelled or the block moved. Re-anchor the search
    ///    string on the new spelling. Do not delete the test.
    /// 2. **`expect` on the closing-brace search fired**: the block's indentation changed. Update
    ///    the `"\n        }"` sentinel to the new depth, and re-check that a nested block still
    ///    cannot close first.
    /// 3. **The `assert!` fired** with the block printed: the call really is gone. Put it back —
    ///    that is a #712 regression, not a test problem. Read `forget_recovery_history`'s doc for
    ///    why the `teleport` fold does not cover this path.
    ///
    /// Deleting this test is only correct if `app.rs` no longer drops the old zone's collision at a
    /// point distinct from the arrival, i.e. if the premise above stopped being true. Say so in the
    /// commit if you conclude that.
    #[test]
    fn the_zone_change_reload_block_still_forgets_the_recovery_ring() {
        const APP_RS: &str = include_str!("app.rs");
        let start = APP_RS.find("if zone_needs_reload(&self.scene.zone, &self.current_zone) {")
            .expect("app.rs no longer has the zone-change reload block this pin is about — if it \
                     moved, move this pin with it; do not delete it");
        // The block body is indented 12 spaces; its closing brace is the first newline followed by
        // exactly 8 spaces and `}`. Nested blocks close deeper, so this cannot match early.
        let end = APP_RS[start..].find("\n        }")
            .expect("could not find the end of the zone-change reload block in app.rs");
        let block = &APP_RS[start..start + end];
        assert!(block.contains("self.controller.forget_recovery_history();"),
            "#712/#724: the zone-change reload block in app.rs MUST drop the controller's recovery \
             ring — the previous zone's untagged coordinates are what wedged #712, and #724's fold \
             into `teleport` does NOT cover this path: this clear runs when the OLD zone's collision \
             is dropped, earlier than and independent of any arrival. Block was:\n{block}");
    }

    /// #724 round-3 review (B1) — the frames that do NOT step must still clear the hold.
    ///
    /// The published claim is "nothing here latches". Round 2 established that for frames that
    /// **step**: `step` does `self.hold.take()` before any branch can re-arm it (pinned by
    /// `a_hold_clears_as_soon_as_the_body_is_free_again`). That argument covers only stepping
    /// frames, and `app.rs` has a run of frames — the whole ~10 s zone-asset load, when
    /// `self.collision` is `None` — where the controller is deliberately not stepped. On those
    /// frames the property is not structural at all: it is supplied by one imperative call,
    /// [`CharacterController::clear_hold`], in the `else` arm beside the step.
    ///
    /// **Round-3 review MEASURED that call unpinned**: deleting it (together with
    /// `GameState::begin_zone_in`'s `player_hold = None`) left the whole workspace green,
    /// 158 passed / 0 failed. Nothing noticed. What is *measured* is the survivor; the consequence
    /// — `ControllerView::hold` keeping the OLD zone's `Some(...)`, `ActionLoop::stream_position`
    /// re-mirroring it into `gs.player_hold`, and `/v1/observe/debug` reporting *"the character is
    /// EMBEDDED in world geometry … ask a GM to move the character"* about a zone already left — is
    /// READ OFF THE BRANCH STRUCTURE, not captured in a run. It is a short trace over code in this
    /// repository (one `step` call site, one `clear_hold` call site, two `player_hold` writers), but
    /// no client was run to watch the stale value appear.
    ///
    /// **What this test does NOT establish** (it is a source scan, so be precise about its reach):
    ///
    /// * It does not prove the `else` arm is ever *reached* — only that, if it is, it clears.
    ///   Reachability is `self.collision == None`, which is the ordinary pre-load state.
    /// * It does not prove [`CharacterController::clear_hold`] clears anything; that is
    ///   `clear_hold_drops_a_hold_without_stepping` below, which is the behavioural half.
    /// * It cannot see a rewrite that keeps the call but stops the `else` arm being the
    ///   not-stepped one. It is anchored on the `if let Some(c) = self.collision` / `step` pair so
    ///   that such a rewrite moves the anchor and reds the `expect` rather than passing silently.
    ///
    /// Triage if this reds — same order as the sibling pin above: fired `expect` on the anchor →
    /// the step block was respelled or restructured, re-anchor and keep the test; fired `expect` on
    /// the `} else {` search → the not-stepped arm is gone, which needs a fresh look at whether the
    /// property still holds; fired `assert!` → the clear really was deleted, put it back.
    #[test]
    fn the_frames_that_do_not_step_still_clear_the_hold() {
        const APP_RS: &str = include_str!("app.rs");
        // Anchored on the camera-init + collision pair together: `if let Some(c) =
        // self.collision.as_deref() {` alone occurs twice in app.rs, and only this one guards the
        // controller step.
        let start = APP_RS.find(
            "if self.camera_initialized {\n                if let Some(c) = self.collision.as_deref() {")
            .expect("app.rs no longer has the camera-init/collision-guarded controller step this \
                     pin is about — if it moved, move this pin with it; do not delete it");
        // The `if self.camera_initialized` block is indented 12 spaces; its closing brace is the
        // first newline followed by exactly 12 spaces and `}`. Everything inside closes at 16 or
        // deeper, so this cannot match early.
        let end = APP_RS[start..].find("\n            }")
            .expect("could not find the end of the controller-step block in app.rs");
        let block = &APP_RS[start..start + end];
        // Narrow to the NOT-stepped arm specifically: a `clear_hold` that had drifted into the
        // stepping arm would satisfy a whole-block scan while leaving the load window unguarded.
        let else_at = block.find("} else {")
            .expect("the controller-step block in app.rs no longer has a not-stepped arm — the \
                     no-collision frames this pin is about may have moved; do not delete this test \
                     without establishing where they went");
        let not_stepped = &block[else_at..];
        assert!(not_stepped.contains("self.controller.clear_hold();"),
            "#724 B1: the NOT-stepped arm of app.rs's controller block (no collision loaded, i.e. \
             the whole zone-asset load) MUST clear the hold. Nothing recomputes it on those frames, \
             so without this call the last hold — computed against geometry that has since been \
             dropped — is published as a confident \"you are wedged\" about a zone we have already \
             left, and re-mirrored into gs.player_hold on every net tick for the whole ~10 s load. \
             The `step` take does NOT cover this path: these frames do not step. Arm was:\n{not_stepped}");
    }

    /// The behavioural half of the pin above: [`CharacterController::clear_hold`] actually drops
    /// the hold.
    ///
    /// Trivial by construction, and that is the point — the source pin can only see that the call
    /// is written, so something has to see that the call does anything. Deleting the body of
    /// `clear_hold` reds here and nowhere else.
    #[test]
    fn clear_hold_drops_a_hold_without_stepping() {
        let mut c = CharacterController::new([0.0, 0.0, 0.0]);
        c.enter_hold(ControllerHoldReason::EmbeddedNoRecovery, 0.1, None);
        assert!(c.hold().is_some(), "fixture: the controller should be holding before we clear it");

        c.clear_hold();

        assert!(c.hold().is_none(),
            "#724 B1: clear_hold must drop the hold WITHOUT a step — it is the only thing making \
             \"nothing here latches\" true on the frames app.rs does not step the controller");
    }

    // ── #794: the SKIN-residual claim in `try_duck_under` ────────────────────────────────────────

    /// **The retired overclaim must stay retired.**
    ///
    /// `try_duck_under`'s doc used to say review N1 had *measured* the SKIN-sized clearance
    /// residual to be unreachable in practice. N1 did no such measurement: it swept the lintel band
    /// (−0.20 / −0.40 / −0.46 / −0.49 crossed and returned, −0.52 refused outbound), reported that
    /// it could not CONSTRUCT the case, and labelled its own mechanism REASONED in as many words.
    ///
    /// A comment cannot be pinned by behaviour, so it is pinned by text. This is not
    /// ceremony — [[eq-docs-are-the-honesty-surface]]: in the five-round review this file came
    /// through, every blocking finding was a false claim in a tracked file and none was in the code.
    /// The banned strings below are the exact ones the issue quotes; if a future edit reintroduces
    /// either, this fails by name.
    ///
    /// (The scan deliberately starts at the top of the file and stops at this function, so the
    /// assertions cannot match their own text. The doc block you are reading is inside that window,
    /// which is why it paraphrases the retired claim instead of quoting it.)
    ///
    /// **The claim is also bound to the PLACE it is about** — see `duck_span` below. Round-2 review
    /// N2 defeated the earlier file-wide version by RELOCATION: it deleted the duck's corrected
    /// clause and its whole retraction block, leaving that function saying nothing about the
    /// residual at all, and satisfied every assertion from three lines of unrelated housekeeping
    /// comment at the top of the file — GREEN. A source-text pin proves a claim is WRITTEN, never
    /// that it is where the issue requires it ([[eq-source-text-pins-prove-written-not-reached]]).
    #[test]
    // Capitalised on purpose: MEASURED is the word this test exists to keep out of the file.
    #[allow(non_snake_case)]
    fn the_duck_never_again_claims_the_skin_residual_was_MEASURED_unreachable() {
        const THIS_FILE: &str = include_str!("movement.rs");
        // Skip this test's own body, or the quoted phrase below would match itself.
        let cut = THIS_FILE
            .find("fn the_duck_never_again_claims_the_skin_residual_was_MEASURED_unreachable")
            .expect("this test's own name must appear in this file");
        let src = &THIS_FILE[..cut];

        // ── The LOCALITY window: the duck's own doc block and body ──────────────────────────────
        //
        // Found structurally, not by line number: back up from the signature over its contiguous
        // `///` / attribute lines, and stop at the next sibling method. Nothing here names a
        // neighbouring function, so reordering the impl cannot silently empty the window — and the
        // REACH CONTROL below fails loudly if it ever does.
        let sig = src.find("fn try_duck_under")
            .expect("#794: try_duck_under is gone from movement.rs — this guard is about a claim \
                     inside it, so it cannot be silently satisfied elsewhere. Re-point it or \
                     retire it deliberately.");
        let doc_start = src[..sig].rfind("\n\n").map(|i| i + 2).unwrap_or(0);
        let span_end = src[sig..].find("\n    fn ").map(|i| sig + i).unwrap_or(src.len());
        let duck_span = &src[doc_start..span_end];
        // REACH CONTROL ([[eq-guard-reach-control]]): prove the window is the one intended before
        // trusting anything it says. A scanner that silently collapsed to a few bytes would make
        // every assertion below vacuously satisfiable from the wrong place — the exact failure this
        // rewrite exists to close.
        // The anchors are the function's SIGNATURE and its first statement — code, never any of the
        // prose this test is about. An anchor drawn from the guarded text would make the reach
        // control fire on the very edits the locality assertions exist to catch, and report a reach
        // failure where the real answer is "the claim moved". (Measured: an earlier draft anchored
        // on a word inside the retraction and did exactly that.)
        assert!(duck_span.len() > 1500 && duck_span.contains("fn try_duck_under")
                && duck_span.contains("let sink = self.swim_sink("),
            "#794 REACH: the try_duck_under window did not resolve to that function (len {}). Every \
             locality assertion below would be meaningless.", duck_span.len());

        assert!(!src.contains("measured it unreachable"),
            "#794: the overclaim is back in movement.rs, reworded. Review N1 SWEPT the lintel band \
             and could not construct the case, and labelled its mechanism REASONED — a failed \
             construction is evidence, not a measurement of unreachability.");
        // The retired wording is allowed to survive EXACTLY ONCE, inside the retraction that
        // quotes it — this repo retracts in place rather than deleting, so the quote is the record.
        // Anywhere else it is an assertion again.
        let hits: Vec<usize> = src.match_indices("measured unreachable").map(|(i, _)| i).collect();
        assert_eq!(hits.len(), 1,
            "#794: expected the retired claim to appear exactly once (quoted inside its own \
             retraction); found {} occurrence(s). A second one is the overclaim restated.",
            hits.len());
        let at = hits[0];
        assert!(at >= doc_start && at < span_end,
            "#794 LOCALITY: the one surviving quote of the retired claim is no longer inside \
             try_duck_under. The retraction has to live where the claim lived, or the duck's doc \
             is free to say nothing at all while this guard is satisfied from elsewhere in the \
             file — measured GREEN in round-2 review N2 before this assertion existed.");
        assert!(src[..at].rfind("⚠️ CORRECTED (#794)").is_some_and(|j| at - j < 400 && j >= doc_start),
            "#794: the surviving `measured unreachable` is no longer inside a `⚠️ CORRECTED \
             (#794)` retraction block within try_duck_under — it has drifted back into an \
             assertion. Say \"swept for and could not construct\" (and do not overclaim the other \
             way either: nothing has shown the residual IS reachable).");
        assert!(duck_span.contains("SWEPT FOR AND COULD NOT CONSTRUCT"),
            "#794: the corrected clause is gone from try_duck_under. If the residual is ever \
             genuinely measured, replace this assertion with the measurement — do not just delete \
             the label, and do not move it somewhere the reader of this function will never see \
             it.");
    }

    // ── #776: the trapped-swimmer disclosure ────────────────────────────────────────────────────
    //
    // Two directions, and the SECOND is the one that matters more. A genuinely trapped swimmer must
    // raise the signal — and every ordinary floating character must NOT, because a false alarm in
    // an honesty observable is the same defect as a silence (the argument #661 recorded at the
    // neutral-buoyancy branch, and the reason #776 could not simply be bolted onto `ControllerHold`).
    // The false-alarm pins below outnumber the positive one deliberately.

    /// Deep water to z = 0 over a pool floor 40 u down, with a SOLID wall at east = 4 running from
    /// the floor to 20 u above the surface. A swimmer pressing east into it can neither duck under
    /// it (solid to the bottom, so the lowered slide gains nothing) nor step up it (20 u tall, and
    /// no floor anywhere in the step band): the qcat pocket mouth at bench scale, and the exact
    /// #776 shape — `on_ground = false`, `in_water = true`, `hold() = None`, `stuck_time` never
    /// accruing because the net's door hands every wet body back to physics.
    fn sealed_east_face() -> Collision {
        flooded_corridor(vec![floor(-40.0, -100.0, 100.0), wall(4.0, -40.0, 20.0)], -40.0, 0.0)
    }
    /// Open deep water — no wall at all, otherwise the same scene.
    fn open_water() -> Collision {
        flooded_corridor(vec![floor(-40.0, -100.0, 100.0)], -40.0, 0.0)
    }
    fn swim_toward(dir: [f32; 2], speed: f32) -> MoveIntent {
        MoveIntent { wish_dir: dir, wish_vspeed: 0.0, jump: false, want_swim: true, speed,
                     climb: 0.0, hop: false }
    }
    /// The swim plane of the scenes above: `surface (0) − float_depth`.
    fn plane() -> f32 { -crate::traversability::PLAYER_BODY.float_depth }

    #[test]
    fn a_swimmer_pressing_at_a_face_it_cannot_pass_raises_the_afloat_stall() {
        let c = sealed_east_face();
        let mut ctrl = CharacterController::new([0.0, 0.0, plane()]);
        // Fixture: the state really is the "reads as swimming normally" one this issue is about.
        for _ in 0..30 { ctrl.step(swim_toward([1.0, 0.0], 44.0), 1.0 / 60.0, &c); }
        assert!(!ctrl.on_ground && ctrl.in_water && ctrl.hold().is_none(),
            "fixture: the trapped swimmer must be afloat, wet and NOT held — if any of those is \
             false this test is measuring some other bug; got pos={:?} on_ground={} in_water={} \
             hold={:?}", ctrl.pos, ctrl.on_ground, ctrl.in_water, ctrl.hold());
        assert!(ctrl.afloat_stall().is_none(),
            "half a second of pressing is not a stall — the window must not have fired yet");

        let pinned_at = ctrl.pos;
        for _ in 0..(5 * 60) { ctrl.step(swim_toward([1.0, 0.0], 44.0), 1.0 / 60.0, &c); }

        let s = ctrl.afloat_stall().expect(
            "#776: a swimmer given five seconds of honoured horizontal drive that produces NO \
             progress against a face it can neither duck under nor climb must SAY SO. Before this \
             fix the state was completely silent: on_ground=false, in_water=true, hold()=None, \
             stuck_time never accruing (the depenetration net stopped running for floaters in \
             #661), i.e. every observable read \"swimming normally\" for ever.");
        assert!(s.secs() >= AFLOAT_STALL_SECS,
            "the disclosed duration must have actually reached the threshold; got {:.2}s", s.secs());
        assert!(hlen([pinned_at[0] - s.anchor()[0], pinned_at[1] - s.anchor()[1], 0.0])
                    <= AFLOAT_PROGRESS,
            "the anchor must be the point the body failed to leave; body pinned near {pinned_at:?}, \
             anchor {:?}", s.anchor());
        assert!(ctrl.hold().is_none(),
            "#776: and this is NOT a ControllerHold — the body is not frozen, a driven dive may \
             still cross. Conflating the two would be a different lie; got {:?}", ctrl.hold());
    }

    /// #801 — `disclosures()` reports what `hold()` and `afloat_stall()` report, not a constant.
    ///
    /// `app.rs` publishes both halves through this one call, so a `disclosures()` that quietly
    /// hard-coded one half (`(self.hold(), None)`) would compile, pass every existing test in this
    /// file — they all read `afloat_stall()` directly — and silence the entire HTTP observable #801
    /// exists to add. That is the shape this test exists for.
    ///
    /// The tuple's ORDER is not tested because it is not testable: the two halves have different
    /// types, so a swapped return does not compile. Making a hazard unrepresentable is preferable to
    /// pinning it, and this records which of the two treatments each half got.
    ///
    /// **Axes deliberately varied:** all three reachable combinations of the two disclosures that
    /// this fixture can produce — neither, then stall-without-hold. **Axis NOT varied, and stated
    /// rather than hidden:** hold-without-stall and both-at-once are not exercised here; a
    /// `ControllerHold` needs an embedded/underworld body, which this flooded-corridor fixture
    /// cannot make. `hold()`'s own publication is pinned separately by
    /// `the_frames_that_do_not_step_still_clear_the_hold`.
    ///
    /// MUTATION CHECK (#801): change `disclosures` to return `(self.hold(), None)` → RED here.
    #[test]
    fn disclosures_reports_both_halves_and_not_a_hardcoded_one_801() {
        let c = sealed_east_face();
        let mut ctrl = CharacterController::new([0.0, 0.0, plane()]);

        // State 1: nothing wrong yet — both halves None, and both must AGREE with the singles.
        for _ in 0..30 { ctrl.step(swim_toward([1.0, 0.0], 44.0), 1.0 / 60.0, &c); }
        let (h, s) = ctrl.disclosures();
        assert_eq!(h, ctrl.hold(), "the hold half must be `hold()`, whatever it says");
        assert_eq!(s, ctrl.afloat_stall(), "the stall half must be `afloat_stall()`");
        assert!(h.is_none() && s.is_none(), "fixture: half a second of pressing discloses nothing");

        // State 2: a matured stall with NO hold — the case a hard-coded `None` would erase.
        for _ in 0..(5 * 60) { ctrl.step(swim_toward([1.0, 0.0], 44.0), 1.0 / 60.0, &c); }
        let (h, s) = ctrl.disclosures();
        assert_eq!(h, ctrl.hold());
        assert_eq!(s, ctrl.afloat_stall());
        assert!(h.is_none(),
            "fixture: a trapped swimmer is not HELD — if this fires the fixture drifted into the \
             depenetration net's territory and the assertion below proves nothing");
        let s = s.expect(
            "#801: the publisher reads BOTH halves through this one call. A `disclosures()` that \
             returned a constant `None` here would compile, keep every test in this file green — \
             they all read `afloat_stall()` directly — and publish \"swimming normally\" over HTTP \
             about a swimmer that has gone nowhere for five seconds.");
        assert!(s.secs() >= AFLOAT_STALL_SECS, "disclosed {:.2}s", s.secs());
    }

    #[test]
    fn an_ordinary_floater_never_raises_the_afloat_stall_however_long_it_floats() {
        // THE FALSE-ALARM PIN, and the reason #776 needed its own signal rather than a naive report
        // of the shape. This body is stationary, unsupported and wet — bit-for-bit the trapped
        // swimmer's observable state, right up against the very same face. The ONLY difference is
        // that nobody is asking it to go anywhere. Raising here would light `hold`-class alarms on
        // every idle swimmer in the world.
        let c = sealed_east_face();
        let mut ctrl = CharacterController::new([3.0, 0.0, plane()]);
        for i in 0..(60 * 60) {
            ctrl.step(swim_still(), 1.0 / 60.0, &c);
            assert!(ctrl.afloat_stall().is_none(),
                "#776 FALSE ALARM at frame {i}: an idle floater beside a wall is a swimmer doing \
                 what swimmers do, not a trapped one. A false alarm in an honesty observable is the \
                 same defect as a silence. pos={:?}", ctrl.pos);
        }
        assert!(!ctrl.on_ground && ctrl.in_water,
            "fixture: …and it really was afloat the whole minute, i.e. this test was not vacuous; \
             got pos={:?} on_ground={} in_water={}", ctrl.pos, ctrl.on_ground, ctrl.in_water);
    }

    #[test]
    fn a_sustained_up_wish_at_the_surface_never_raises_the_afloat_stall() {
        // The single most common wish in the whole water system: the walker's haul-out drive sends
        // an UP-wish, and `step`'s surface clamp pins the feet SKIN under the surface, so the body
        // asks to rise and does not rise, indefinitely. That is correct behaviour at a surface, and
        // it is why both halves of the stall predicate are HORIZONTAL.
        let c = open_water();
        let mut ctrl = CharacterController::new([0.0, 0.0, plane()]);
        let up = MoveIntent { wish_dir: [0.0, 0.0], wish_vspeed: 20.0, jump: false, want_swim: true,
                              speed: 0.0, climb: 0.0, hop: false };
        for i in 0..(30 * 60) {
            ctrl.step(up, 1.0 / 60.0, &c);
            assert!(ctrl.afloat_stall().is_none(),
                "#776 FALSE ALARM at frame {i}: a swimmer holding an up-wish at the surface is not \
                 trapped — it is at the top of the water. pos={:?}", ctrl.pos);
        }
        assert!(ctrl.in_water && !ctrl.on_ground && ctrl.pos[2] > plane(),
            "fixture: the up-wish must really have carried it to the clamped surface and held it \
             there (else the frames above were not the case this pins); got {:?}", ctrl.pos);
    }

    #[test]
    fn a_swimmer_crossing_open_water_never_raises_the_afloat_stall() {
        for speed in [44.0f32, 10.0, 1.0] {
            // 1.0 u/s is the tightest legitimate case in the sweep: it needs half a second to clear
            // AFLOAT_PROGRESS, so the window opens and re-arms repeatedly without ever maturing.
            let c = open_water();
            let mut ctrl = CharacterController::new([-60.0, 0.0, plane()]);
            for i in 0..(30 * 60) {
                ctrl.step(swim_toward([1.0, 0.0], speed), 1.0 / 60.0, &c);
                assert!(ctrl.afloat_stall().is_none(),
                    "#776 FALSE ALARM at frame {i} (speed {speed}): a body that is actually getting \
                     somewhere must never accumulate a stall. pos={:?}", ctrl.pos);
            }
            assert!(ctrl.pos[0] > -60.0 + AFLOAT_PROGRESS,
                "fixture: speed {speed} must actually have moved the body, else this is vacuous; \
                 got {:?}", ctrl.pos);
        }
    }

    /// A deep column — 300 u of water over a floor far below — with the same impassable east face.
    /// The horizontal wish is blocked exactly as in [`sealed_east_face`]; the only difference is
    /// that there is room to actually GO somewhere vertically.
    fn deep_sealed_east_face() -> Collision {
        flooded_corridor(vec![floor(-300.0, -100.0, 100.0), wall(4.0, -300.0, 20.0)], -300.0, 0.0)
    }

    #[test]
    fn a_driven_dive_or_rise_along_a_blocked_face_never_raises_the_afloat_stall() {
        // ROUND-2 REVIEW B1 — the false alarm the horizontal-only PROGRESS term produced, and the
        // pin that keeps the progress term three-dimensional.
        //
        // This is the PRODUCTION intent shape, not a contrived one: the nav walker sets a
        // normalised unit `wish_dir` and a `swim_vspeed` in the SAME `MoveIntent` (Slice-3 depth
        // control), and so does the manual/WASD path. So a body descending a shaft while its
        // lateral wish is pressed against the shaft wall is ordinary, and it is exactly what the
        // qcat escape does.
        //
        // Measured on the horizontal-only progress term, before the fix (6 s, 1/60 dt, ±20 u/s):
        //   dive: z −2.00 → −122.00 (120 u travelled)   afloat_stall() = Some(secs 5.98)
        //   rise: z −250.00 → −130.00 (120 u travelled) afloat_stall() = Some(secs 5.98)
        // A body that had moved 120 units was being disclosed as "producing no progress", and the
        // log line then offered "a down-wish dive may still cross" to a body already performing
        // one. A false alarm in an honesty observable is the same defect as a silence.
        //
        // If the progress term ever reverts to horizontal-only, this goes RED in both directions.
        for (label, vspeed, start_z) in [("driven dive", -20.0f32, plane()), ("driven rise", 20.0, -250.0)] {
            let c = deep_sealed_east_face();
            let mut ctrl = CharacterController::new([0.0, 0.0, start_z]);
            let intent = MoveIntent { wish_dir: [1.0, 0.0], wish_vspeed: vspeed, jump: false,
                                      want_swim: true, speed: 44.0, climb: 0.0, hop: false };
            for _ in 0..(6 * 60) { ctrl.step(intent, 1.0 / 60.0, &c); }
            let travelled = (ctrl.pos[2] - start_z).abs();
            assert!(ctrl.in_water && !ctrl.on_ground,
                "fixture ({label}): the body must still be afloat, else this test is about some \
                 other state; got pos={:?} in_water={} on_ground={}",
                ctrl.pos, ctrl.in_water, ctrl.on_ground);
            assert!(travelled > 100.0,
                "fixture ({label}): the drive must genuinely have moved the body a long way, else \
                 this test is vacuous; travelled {travelled:.2} u to {:?}", ctrl.pos);
            assert!(ctrl.afloat_stall().is_none(),
                "#776 FALSE ALARM ({label}): the body travelled {travelled:.2} u vertically in 6 s \
                 and was disclosed as going nowhere: {:?}. PROGRESS is net displacement in THREE \
                 dimensions — a swimmer crossing 120 u of water column is not trapped, whatever \
                 its blocked lateral wish is doing. (The WISH half stays horizontal; that is a \
                 different question and a different justification — see AfloatFrame.)",
                ctrl.afloat_stall());
        }
    }

    #[test]
    fn a_unit_wish_at_zero_speed_never_raises_the_afloat_stall() {
        // ROUND-2 REVIEW N5. `throttle` is `|wish_dir|` alone, so `wish_dir=[1,0], speed=0.0` used
        // to classify as `Wished` and stall in EMPTY OPEN WATER with no geometry at all (measured:
        // Some(secs 5.98) after 6 s at the origin of an open pool). A frame whose requested
        // displacement is identically zero is not a drive that is failing — nobody asked for
        // anything. Latent rather than live at the time (no production intent site sets speed 0
        // with a unit direction), but it is a false alarm in the open, which is the direction that
        // matters most here.
        let c = open_water();
        let mut ctrl = CharacterController::new([0.0, 0.0, plane()]);
        for i in 0..(10 * 60) {
            ctrl.step(swim_toward([1.0, 0.0], 0.0), 1.0 / 60.0, &c);
            assert!(ctrl.afloat_stall().is_none(),
                "#776 FALSE ALARM at frame {i}: a wish direction with NO speed behind it requests \
                 no displacement — in open water with no geometry, reporting it as a stall is a \
                 confident falsehood. pos={:?}", ctrl.pos);
        }
        assert!(ctrl.in_water && !ctrl.on_ground,
            "fixture: the body must have been afloat throughout; got {:?}", ctrl.pos);
    }

    #[test]
    fn a_swimmer_hauling_out_at_a_legitimate_bank_never_raises_the_afloat_stall() {
        // The #191 bank from `a_swimmer_at_a_solid_bank_still_hauls_out_the_duck_does_not_override_191`:
        // a lip 2.1 u above the swim plane, inside the swimming step-up's reach. The body presses,
        // mounts, and walks out — never stalling, because the approach makes progress and the
        // haul-out ends the afloat state entirely.
        //
        // ⚠️ **The bank floor runs to east 3000, not east 100 (#870).** The drive is 30 s at
        // 35 u/s — 1050 u of travel — and the body starts at east −20, so a floor ending at east
        // 100 is walked OFF at frame ~240, four fifths of the way through the run still to go.
        // MEASURED on `main`, unmodified, with the old 100 u floor: from frame 240 the body was
        // dragged 99.7 u NORTH under a due-east drive (`is_embedded` = true → ring push-out, ~0.4 u
        // of teleport per frame, the #870 drift), and then oscillated across the floor edge for the
        // remaining 1500 frames — grounded, ungrounded, grounded — so the closing
        // `assert!(ctrl.on_ground)` was decided by which side of that oscillation frame 1799
        // happened to land on. It landed grounded on `main` and ungrounded once #870's back-off
        // moved the haul-out 0.78 u east: a passing test whose pass was a coin toss, not a claim
        // about hauling out. Widening the floor puts the whole 1050 u drive on solid ground, which
        // is the premise the test's own doc comment asserts.
        let c = flooded_corridor(
            vec![floor(-40.0, -100.0, 4.0), floor(0.1, 4.0, 3000.0), wall(4.0, -40.0, 0.1)],
            -40.0, 0.0);
        let mut ctrl = CharacterController::new([-20.0, 0.0, plane()]);
        for i in 0..(30 * 60) {
            ctrl.step(swim_toward([1.0, 0.0], 35.0), 1.0 / 60.0, &c);
            assert!(ctrl.afloat_stall().is_none(),
                "#776 FALSE ALARM at frame {i}: swimming to a bank and hauling out of it is the \
                 water system working. pos={:?} on_ground={}", ctrl.pos, ctrl.on_ground);
        }
        assert!(ctrl.on_ground && ctrl.pos[0] > 4.0,
            "fixture: the body must genuinely have hauled out (#191), else this test never visited \
             the transition it claims to cover; got {:?} on_ground={}", ctrl.pos, ctrl.on_ground);
    }

    #[test]
    fn a_wading_body_blocked_by_a_wall_never_raises_the_afloat_stall() {
        // Wet but SUPPORTED. Scope pin: the afloat signal must not annex the grounded vocabulary
        // (`stuck_time` / `EmbeddedNoRecovery`), which is what owns bodies standing on something.
        // The water starts at east 0 and is 1 u deep over the floor, so the body grounds on DRY
        // land first and wades in — the only way a wet grounded body actually arises (a body that
        // is already unsupported when it meets water is taken by the buoyancy branch and stays
        // afloat).
        let mut c = col(vec![floor(0.0, -100.0, 100.0), wall(4.0, 0.0, 20.0)]);
        c.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::box_below(-100.0, 100.0, 0.0, 100.0, 1.0))));
        let mut ctrl = CharacterController::new([-20.0, 0.0, 0.0]);
        for _ in 0..(20 * 60) { ctrl.step(walk(44.0, [1.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(ctrl.on_ground && ctrl.in_water,
            "fixture: the body must be wading — wet AND grounded; got {:?} on_ground={} in_water={}",
            ctrl.pos, ctrl.on_ground, ctrl.in_water);
        assert!(ctrl.afloat_stall().is_none(),
            "#776: a body standing on the bottom is not AFLOAT, whatever else is true of it — the \
             afloat window must stay shut; got {:?}", ctrl.afloat_stall());
    }

    #[test]
    fn a_dry_body_pressed_against_a_wall_never_raises_the_afloat_stall() {
        // The other scope pin, and an explicitly UNCOVERED case: a dry body walking into a wall for
        // ever is equally silent, and this fix does not change that. It is pre-existing, it is not
        // what #661 altered, and reporting it here would put an "afloat" word on a body that is not.
        let c = col(vec![floor(0.0, -100.0, 100.0), wall(4.0, 0.0, 20.0)]);
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        for _ in 0..(20 * 60) { ctrl.step(walk(44.0, [1.0, 0.0]), 1.0 / 60.0, &c); }
        assert!(!ctrl.in_water && ctrl.on_ground, "fixture: dry and grounded; got {:?}", ctrl.pos);
        assert!(ctrl.afloat_stall().is_none(),
            "#776 scope: the dry wall-press is NOT covered by this signal; got {:?}",
            ctrl.afloat_stall());
    }

    #[test]
    fn the_afloat_stall_clears_the_frame_the_body_makes_progress_again() {
        // Level-triggered, like `hold`. An observable that latches on and never clears is its own
        // honesty bug (#343/#679), not a fix for one.
        let c = sealed_east_face();
        let mut ctrl = CharacterController::new([0.0, 0.0, plane()]);
        for _ in 0..(6 * 60) { ctrl.step(swim_toward([1.0, 0.0], 44.0), 1.0 / 60.0, &c); }
        assert!(ctrl.afloat_stall().is_some(), "fixture: the body must be stalled before we clear it");

        // One frame of swimming AWAY covers 0.73 u > AFLOAT_PROGRESS, so the window re-anchors.
        ctrl.step(swim_toward([-1.0, 0.0], 44.0), 1.0 / 60.0, &c);
        assert!(ctrl.afloat_stall().is_none(),
            "#776: the stall must clear the FIRST frame the body gets somewhere, not decay; got {:?}",
            ctrl.afloat_stall());
    }

    #[test]
    fn the_afloat_stall_clears_the_frame_the_driver_stops_asking() {
        let c = sealed_east_face();
        let mut ctrl = CharacterController::new([0.0, 0.0, plane()]);
        for _ in 0..(6 * 60) { ctrl.step(swim_toward([1.0, 0.0], 44.0), 1.0 / 60.0, &c); }
        assert!(ctrl.afloat_stall().is_some(), "fixture: stalled before we drop the wish");

        ctrl.step(swim_still(), 1.0 / 60.0, &c);
        assert!(ctrl.afloat_stall().is_none(),
            "#776: with the wish withdrawn the body is a resting floater again — the same physical \
             state, and no longer a report. The stall says \"this drive is not working\", never \
             \"this body is trapped\"; got {:?}", ctrl.afloat_stall());
    }

    #[test]
    fn a_position_discontinuity_supersedes_the_afloat_stall() {
        let c = sealed_east_face();
        let mut ctrl = CharacterController::new([0.0, 0.0, plane()]);
        for _ in 0..(6 * 60) { ctrl.step(swim_toward([1.0, 0.0], 44.0), 1.0 / 60.0, &c); }
        assert!(ctrl.afloat_stall().is_some(), "fixture: stalled before the teleport");

        ctrl.teleport([-50.0, 0.0, plane()]);
        assert!(ctrl.afloat_stall().is_none(),
            "#776: a summon / large server correction relocates the body, so the anchor describes a \
             point it is no longer at. Carrying the seconds across would hand the new position an \
             alarm it did not earn — the same shape #724 removed from the recovery ring");
    }

    #[test]
    fn a_frame_the_depenetration_net_handles_closes_the_afloat_window() {
        // `step` early-returns on any frame the net handled, BEFORE the fold at the bottom — so
        // without an explicit close on that path a mature stall would freeze in place and be
        // published, unchanged, about a body that is now dry and embedded (whose real disclosure is
        // `EmbeddedNoRecovery`). The net's door only ever hands it DRY bodies, so `NotAfloat` there
        // is the true classification, not a convenient default.
        //
        // Constructed by stepping the same body against a second, DRY collision in which its
        // position is embedded — the shape a water region vanishing under a swimmer produces.
        let wet = sealed_east_face();
        let mut ctrl = CharacterController::new([0.0, 0.0, plane()]);
        for _ in 0..(6 * 60) { ctrl.step(swim_toward([1.0, 0.0], 44.0), 1.0 / 60.0, &wet); }
        assert!(ctrl.afloat_stall().is_some(), "fixture: stalled before the medium changes");
        let pinned = ctrl.pos;

        let dry = col(vec![floor(-12.0, -100.0, 100.0),
                           wall(pinned[0] + 0.8, -12.0, 10.0), wall(pinned[0] - 0.8, -12.0, 10.0)]);
        assert!(!dry.footprint_clear(pinned[0], pinned[1], pinned[2], PLAYER_RADIUS, 8),
            "fixture: the body must be EMBEDDED in the dry scene, or the net never runs and this \
             test proves nothing");
        ctrl.step(swim_toward([1.0, 0.0], 44.0), 1.0 / 60.0, &dry);

        assert!(ctrl.afloat_stall().is_none(),
            "#776: a frame the depenetration net handled is a frame with a DRY body — the afloat \
             window must close there, not freeze at its last value and go on reporting a swimmer \
             that no longer exists; got {:?}", ctrl.afloat_stall());
    }

    #[test]
    fn clear_hold_drops_the_afloat_window_too() {
        // `app.rs` calls `clear_hold` on frames it renders but does not step (no collision loaded,
        // i.e. the whole ~10 s zone-asset load). Nothing recomputes the window on those frames, so
        // without this the last stall — computed against geometry that has since been dropped —
        // would survive the load. Folding it into `clear_hold` is what makes the eventual
        // publication correct with no change to `app.rs`.
        let c = sealed_east_face();
        let mut ctrl = CharacterController::new([0.0, 0.0, plane()]);
        for _ in 0..(6 * 60) { ctrl.step(swim_toward([1.0, 0.0], 44.0), 1.0 / 60.0, &c); }
        assert!(ctrl.afloat_stall().is_some(), "fixture: stalled before the clear");

        ctrl.clear_hold();
        assert!(ctrl.afloat_stall().is_none(),
            "#776: clear_hold must drop the afloat window as well as the hold — the frames it \
             covers have no geometry for either claim to be about");
    }

    // ── #776: the universals, on the pure clock ─────────────────────────────────────────────────
    //
    // The example tests above are existence proofs over particular trajectories. "A resting floater
    // can NEVER stall" is a universal, and no finite number of scenes discharges one
    // ([[eq-verification-hierarchy]]). These drive `AfloatStallClock` directly, which is pure, and
    // sweep it far outside anything a scene reaches.

    /// A tiny deterministic LCG — no dev-dependency, and a fixed seed so a failure is reproducible.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 { self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); self.0 >> 33 }
        fn f32(&mut self, lo: f32, hi: f32) -> f32 { lo + (self.next() % 100_000) as f32 / 100_000.0 * (hi - lo) }
        /// A displacement of magnitude `mag` in a UNIFORMLY RANDOM 3-D direction.
        ///
        /// Round-2 review B1 measured why this exists: the sweep below used to build every position
        /// as `[base[0] + step, base[1], 0.0]`, so **`y` and `z` were identically constant in all
        /// 500,000 iterations**. A universal test blind to two of three axes cannot discriminate a
        /// horizontal-only progress term from a 3-D one — and it did not: the whole suite stayed
        /// green when the term was changed under it. Sampling a direction rather than an axis is
        /// what makes the sweep able to fail.
        fn dir3(&mut self, mag: f32) -> [f32; 3] {
            let (az, el) = (self.f32(0.0, std::f32::consts::TAU), self.f32(-1.5, 1.5));
            [mag * el.cos() * az.cos(), mag * el.cos() * az.sin(), mag * el.sin()]
        }
    }
    /// Euclidean 3-D length — the sweep's INDEPENDENT copy of the progress measure. Deliberately
    /// written out here rather than reaching into `afloat`'s private `len3`, so the shadow model
    /// below is not checking the implementation against itself.
    fn len3_shadow(d: [f32; 3]) -> f32 { (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() }

    #[test]
    fn no_run_of_frames_without_a_wish_can_ever_stall() {
        // THE false-alarm universal. Any sequence at all of NotAfloat / Resting frames — any
        // durations, any positions, any length — must leave the clock silent.
        let mut rng = Lcg(0x776_0001);
        let mut clock = AfloatStallClock::default();
        for i in 0..500_000u32 {
            let frame = if rng.next() % 2 == 0 { AfloatFrame::Resting } else { AfloatFrame::NotAfloat };
            let pos = [rng.f32(-1000.0, 1000.0), rng.f32(-1000.0, 1000.0), rng.f32(-200.0, 200.0)];
            clock.observe(frame, pos, rng.f32(0.0, 0.5));
            assert!(clock.stall().is_none(),
                "#776: a stall was assembled out of frames that contained NO horizontal wish \
                 (iteration {i}, {frame:?}). The wish is the ENTIRE resting-vs-trapped distinction; \
                 without it every idle swimmer in the world reports trapped.");
        }
    }

    #[test]
    fn one_wishless_frame_always_ends_the_window_immediately() {
        // Stronger than "a resting run never stalls": a single non-`Wished` frame must close a
        // MATURE window at once, so an alarm can never be reassembled out of scattered frames with
        // ordinary swimming in between.
        let mut rng = Lcg(0x776_0002);
        for i in 0..20_000u32 {
            let mut clock = AfloatStallClock::default();
            // Mature a stall in place.
            for _ in 0..200 { clock.observe(AfloatFrame::Wished, [0.0, 0.0, 0.0], 0.1); }
            assert!(clock.stall().is_some(), "fixture (iteration {i}): the window must be mature");
            let frame = if rng.next() % 2 == 0 { AfloatFrame::Resting } else { AfloatFrame::NotAfloat };
            clock.observe(frame, [rng.f32(-9.0, 9.0), rng.f32(-9.0, 9.0), rng.f32(-9.0, 9.0)],
                          rng.f32(0.0, 0.5));
            assert!(clock.stall().is_none(),
                "#776: one {frame:?} frame must clear a mature stall outright (iteration {i})");
        }
    }

    #[test]
    fn a_stall_always_implies_an_unbroken_wished_run_that_went_nowhere() {
        // The positive universal, checked against an independent shadow model: whenever the clock
        // reports a stall, (a) every frame since the anchor was `Wished`, (b) the body never got
        // more than AFLOAT_PROGRESS from that anchor, and (c) the reported duration is the real
        // elapsed time of that run. Any one of those failing is a stall that overstates.
        let mut rng = Lcg(0x776_0003);
        let mut clock = AfloatStallClock::default();
        // Shadow: (anchor, secs since anchor) for the current unbroken Wished run.
        let mut shadow: Option<([f32; 3], f32)> = None;
        let mut stalls = 0u32;
        for i in 0..500_000u32 {
            // Biased toward Wished, toward SMALL steps and toward long frames, so mature windows
            // actually occur — an unbiased sweep reaches a stall essentially never (measured: 0
            // stalls in 500k iterations), and a universal test that never visits the state it is
            // about is the "passes both ways" failure this project keeps catching. The `stalls`
            // floor at the bottom is what makes that non-vacuity checkable rather than assumed.
            let roll = rng.next() % 20;
            let frame = match roll {
                0 => AfloatFrame::Resting,
                1 => AfloatFrame::NotAfloat,
                _ => AfloatFrame::Wished,
            };
            let step = if rng.next() % 8 == 0 { rng.f32(0.0, 1.2) } else { rng.f32(0.0, 0.45) };
            let dt = rng.f32(0.05, 0.25);
            let prev = clock;
            let base = match shadow { Some((a, _)) => a, None => [0.0, 0.0, 0.0] };
            // A step of that magnitude in a random 3-D direction — NOT along +x with y and z pinned
            // (round-2 review B1; see `Lcg::dir3`). The magnitude distribution is unchanged, so the
            // stall rate and the non-vacuity floor below still mean what they meant.
            let off = rng.dir3(step);
            let pos = [base[0] + off[0], base[1] + off[1], base[2] + off[2]];
            clock.observe(frame, pos, dt);
            match frame {
                AfloatFrame::Resting | AfloatFrame::NotAfloat => shadow = None,
                AfloatFrame::Wished => shadow = Some(match shadow {
                    None => (pos, 0.0),
                    Some((a, s)) => {
                        if len3_shadow([pos[0] - a[0], pos[1] - a[1], pos[2] - a[2]]) > AFLOAT_PROGRESS
                            { (pos, 0.0) } else { (a, s + dt) }
                    }
                }),
            }
            if let Some(st) = clock.stall() {
                stalls += 1;
                let (a, s) = shadow.expect(
                    "#776: the clock reported a stall on a frame the shadow model closed the window \
                     on — i.e. a stall survived a wishless frame");
                assert_eq!(frame, AfloatFrame::Wished,
                    "#776: a stall was in force on a {frame:?} frame (iteration {i}, prev {prev:?})");
                assert!((st.secs() - s).abs() < 1e-3,
                    "#776: reported {:.4}s, real run {s:.4}s (iteration {i})", st.secs());
                assert!(st.secs() >= AFLOAT_STALL_SECS,
                    "#776: an AfloatStall below the threshold is not constructible by contract; got \
                     {:.4}s (iteration {i})", st.secs());
                assert!(len3_shadow([pos[0] - a[0], pos[1] - a[1], pos[2] - a[2]]) <= AFLOAT_PROGRESS,
                    "#776: stalled while more than {AFLOAT_PROGRESS}u from the anchor IN 3-D \
                     (iteration {i}); pos {pos:?} anchor {a:?}");
                assert_eq!(st.anchor(), a, "#776: anchor drifted from the run's start (iteration {i})");
            }
        }
        assert!(stalls > 1000,
            "the sweep must actually REACH the stalled state often enough to mean something — a \
             green run that never stalled would prove nothing; got {stalls}");
    }

    /// **Rename guard for this file's doc-comment citations (#874).** Growing `citation_corpus` in
    /// `crates/eqoxide-nav/src/steering.rs` to include this file turned the nav crate's
    /// citation-resolution scan
    /// (`every_test_citation_in_the_five_citation_files_resolves_and_is_listed_in_a_guard`) onto
    /// every `#[test]` this file's own doc comments cite by name — mostly `MUTATION-CHECK` notes
    /// pointing at the fixture that proves a given behavior true.
    ///
    /// Measured, not assumed: #874 explicitly left "how many existing `movement.rs` citations would
    /// fail the guard today" as unmeasured. Running the scan against the widened corpus answered it.
    /// **Measured on `main` at `d63776d`, before this array existed:** fourteen distinct test names
    /// over nineteen citation sites — ten cited once and four cited more than once (×3, ×2, ×2, ×2;
    /// 10 + 9 = 19) — all fourteen resolving to real `#[test] fn`s in this same file and none of
    /// them named in any guard, because this file had none. This is that guard, mirroring the one
    /// already in `steering.rs`/`walker.rs`/`collision.rs`/`tests/walker_sim.rs`. The fourteen names
    /// are the array below; that is the live list, and it is what a rename breaks the build on.
    ///
    /// ⚠️ **Corrections (#882 round 2).** Two, both in the sentence above. It said **five** names
    /// were cited more than once and then listed four; the arithmetic only closes with four
    /// (10 singletons + 9 repeat sites = 19), and re-deriving the scan's own charset rules against
    /// `main` gives four. And the nineteen is a **historical** figure with the predicate now stated:
    /// this doc comment is itself in the file the scan reads, so every test name written into it
    /// counts as one more citation site — the first version of this paragraph named the four
    /// repeat-cited tests and raised the live count to twenty-three the moment it landed, while
    /// still saying nineteen in the present tense. The names are gone from the prose (the array
    /// below is the list that matters), the pin is `main` before this guard, which cannot move, and
    /// nothing here asserts a live number.
    #[test]
    fn doc_comment_citations_in_this_file_are_rename_guarded() {
        let _cited: &[fn()] = &[
            fall_through_guard_disabled_when_underworld_unknown,
            a_large_same_zone_relocation_forgets_the_pre_relocation_recovery_ring,
            the_zone_change_reload_block_still_forgets_the_recovery_ring,
            the_frames_that_do_not_step_still_clear_the_hold,
            clear_hold_drops_a_hold_without_stepping,
            a_driven_swim_descent_never_passes_the_pool_floor_at_any_dt,
            a_duck_never_dives_out_the_bottom_of_its_own_water_volume,
            a_duck_never_exits_the_water_sideways,
            a_driven_swim_descent_never_passes_a_real_zone_floor,
            p3_collided_swim_does_not_embed_under_a_flush_ceiling,
            water_breaks_a_fall_no_phantom_damage_on_shore,
            exiting_the_bottom_of_a_suspended_water_volume_resumes_the_fall,
            a_large_same_zone_relocation_forgets_the_ring_for_the_stuck_fallback_too,
            a_hold_clears_as_soon_as_the_body_is_free_again,
            // #870 (review round 2). These four are cited by the `STEP_LANDING_CREEP_SAMPLES` doc
            // comment. They are NOT pre-existing offenders newly pulled into scope by #882's
            // widened guard — the citations are new lines added by this PR — so they belong here
            // and NOT in the guard's `KNOWN_VIOLATIONS` list, which is labelled "pre-existing".
            a_swimmer_at_a_solid_bank_still_hauls_out_the_duck_does_not_override_191,
            a_swimmer_hauling_out_at_a_legitimate_bank_never_raises_the_afloat_stall,
            a_duck_across_a_divable_far_side_is_a_round_trip,
            the_step_landing_creep_reaches_one_back_off_past_the_riser_and_no_further,
            // #932: cited by `slide()`'s doc comment (the new wall-length-independent zero-drift
            // pin) and by the new test's own doc comment (which names the pre-existing 100 u pin).
            a_grounded_walk_never_drifts_on_a_short_or_a_1000u_extent_wall,
            a_grounded_walk_at_a_barrier_never_enters_the_depenetration_net,
            // #938 (#987 round 2): cited by `slide()`'s residual-clearance table, which now states
            // the half-extent its protocol needs and points here for the pinned proof.
            the_residual_clearance_table_needs_the_walls_lateral_extent_stated,
        ];
    }

    /// #931 — the passable band's upper boundary is CLOSED (`STEP_UP + Body::foot`, 2.50, is
    /// still inside it), not open as the ⚠️ block above used to state. MEASURED, flat far floor,
    /// straight-on drive at 20/35/44 u/s and dt in {1/60, 1/30, 1/20}, 600 frames: a lip of exactly
    /// 2.5000 is fully passable (`is_embedded` fires while crossing and the body ends past the wall
    /// on every speed/dt combination tried), while 2.5001 already rests correctly at
    /// `-(radius + SKIN)` on all of them — a 1e-4 u swing across the boundary.
    ///
    /// The passable leg asserts `east > 0` and not merely "not resting at `-(radius + SKIN)`":
    /// #987 round 2 pointed out that the weaker predicate is satisfied by a body stuck ANYWHERE
    /// else, which is not what "passable" means. Measured, the 2.5000 leg ends at east
    /// 99.39–100.00 across the four pairs, so `east > 0` has ~99 u of headroom and is not a
    /// tautology. This test deliberately does NOT pin the ~99.7 u of north drift that accompanies
    /// the crossing — that figure is recorded in the ⚠️ block above, and it belongs to the blind
    /// band tracked by #917 (#854's family, one storey up), not to the boundary this test is about.
    #[test]
    fn the_blind_step_up_band_is_closed_at_its_upper_bound() {
        let radius = crate::traversability::PLAYER_BODY.radius;
        for &(lip, want_blocked) in &[(2.5000_f32, false), (2.5001_f32, true)] {
            for &(speed, dt) in &[(20.0_f32, 1.0_f32 / 60.0), (35.0, 1.0 / 60.0),
                                   (35.0, 1.0 / 30.0), (44.0, 1.0 / 20.0)] {
                let c = col(vec![floor(0.0, -100.0, 0.0), wall(0.0, 0.0, lip), floor(0.0, 0.0, 100.0)]);
                let mut ctrl = CharacterController::new([-20.0, 0.0, 0.0]);
                ctrl.on_ground = true;
                for _ in 0..600 { ctrl.step(walk(speed, [1.0, 0.0]), dt, &c); }
                let blocked = (ctrl.pos[0] - -(radius + SKIN)).abs() < 1e-2;
                assert_eq!(blocked, want_blocked,
                    "lip {lip} speed {speed} dt {dt}: end east {} — blocked={blocked}, want {want_blocked}",
                    ctrl.pos[0]);
                if !want_blocked {
                    // "Passable" means the body ENDED PAST THE WALL, not just "somewhere other
                    // than the rest position" — a body wedged anywhere else passes that (#987 r2).
                    assert!(ctrl.pos[0] > 0.0,
                        "lip {lip} speed {speed} dt {dt}: end east {} — a passable lip must leave \
                         the body past the barrier at east 0, not merely off the rest position",
                        ctrl.pos[0]);
                }
            }
        }
    }

    /// Extent-parameterized wall/floor for #938 (the file's `wall`/`floor` hard-code a north
    /// extent of 100, which is too short for the oblique columns below — see the doc above).
    fn wall_ext(e: f32, h0: f32, h1: f32, half_extent: f32) -> MeshData {
        mesh(vec![[-half_extent, h0, e], [half_extent, h0, e], [half_extent, h1, e], [-half_extent, h1, e]])
    }
    fn floor_ext(z: f32, e0: f32, e1: f32, half_extent: f32) -> MeshData {
        mesh(vec![[-half_extent, z, e0], [half_extent, z, e0], [half_extent, z, e1], [-half_extent, z, e1]])
    }

    /// The residual-clearance table's own harness: drive a grounded body at a 6u wall at the
    /// given `ndot` (cosine of approach vs. face normal), 40 sub-frame phases, 600 frames,
    /// 35 u/s at 60 Hz, and return the closest perpendicular approach ever reached. `half_extent`
    /// is the wall/floor's north half-extent — the parameter #938 found missing from the doc.
    fn residual_clearance(ndot: f32, half_extent: f32) -> f32 {
        let theta = ndot.acos();
        let dir = [theta.cos(), theta.sin()];
        let c = col(vec![
            floor_ext(0.0, -100.0, 0.0, half_extent),
            wall_ext(0.0, 0.0, 6.0, half_extent),
            floor_ext(0.0, 0.0, 100.0, half_extent),
        ]);
        let mut worst = f32::MAX;
        for k in 0..40 {
            let phase = (k as f32) * (35.0 / 60.0) / 40.0;
            let mut ctrl = CharacterController::new([-20.0 - phase, 0.0, 0.0]);
            ctrl.on_ground = true;
            for _ in 0..600 {
                ctrl.step(walk(35.0, dir), 1.0 / 60.0, &c);
                if ctrl.pos[0] < 0.0 { worst = worst.min(-ctrl.pos[0]); }
            }
        }
        worst
    }

    /// #938 — the residual-clearance table in `slide()`'s doc is reproducible ONLY past the wall's
    /// own lateral extent, so the extent is part of its protocol. MEASURED (this fn): at
    /// `half_extent` = 500 (max north drift observed across all eight columns: 399.8 u, so 500
    /// clears it), every column matches the doc table within 0.01.
    ///
    /// At the file's own `wall`/`floor` half-extent of 100, **four** of the eight columns still
    /// reproduce and four do not — pinned below, all eight, so neither half of that claim rests on
    /// prose. The four that diverge are `ndot` 0.866, 0.707, 0.174 and 0.087, reading 0.5027,
    /// 0.5978, 2.2375 and 11.2202 against table cells of 0.920, 0.743, 0.899 and 0.949. #938 and
    /// the first draft of this test named only the first two; the 0.174 and 0.087 columns are off
    /// by 1.34 and 10.27 — over 100× this test's own tolerance — and are the reason the extent
    /// belongs in the protocol line rather than in a footnote.
    #[test]
    fn the_residual_clearance_table_needs_the_walls_lateral_extent_stated() {
        // (ndot, doc-table cell, value at the file's own half-extent of 100)
        let cases = [
            (1.000_f32, 1.0500_f32, 1.0500_f32), (0.985, 1.0388, 1.0388),
            (0.866, 0.9203, 0.5027), (0.707, 0.7430, 0.5978),
            (0.500, 0.7084, 0.7086), (0.342, 0.8005, 0.8005),
            (0.174, 0.8985, 2.2375), (0.087, 0.9493, 11.2202),
        ];
        // Reach control for the CORPUS, the same treatment the #933 grid gets ~3,500 lines above
        // ("the corpus is an item too"). `reproduce_at_100` below pins the NUMERATOR; without this
        // the DENOMINATOR is free, and three pieces of tracked prose say eight. MEASURED (#987
        // round 3): deleting the most divergent row, `(0.087, 0.9493, 11.2202)`, left this test
        // GREEN at seven columns with four still reproducing — so the doc's "four of eight" could
        // silently become "four of seven" behind a passing suite. It cannot now.
        assert_eq!(cases.len(), 8, "the doc's denominator is eight columns");
        let mut reproduce_at_100 = 0_usize;
        for &(ndot, want, want_at_100) in &cases {
            let got = residual_clearance(ndot, 500.0);
            assert!((got - want).abs() < 0.01,
                "ndot {ndot}: worst clearance {got:.4} at half_extent=500, want {want:.4}");
            // The documented failure mode at the file's own (too-short) extent, so a future reader
            // cannot mistake "the table is wrong" for "the extent was never the issue". Pinned for
            // every column, not just the two #938 happened to name.
            let got_100 = residual_clearance(ndot, 100.0);
            assert!((got_100 - want_at_100).abs() < 0.01,
                "ndot {ndot}: worst clearance {got_100:.4} at half_extent=100, want {want_at_100:.4}");
            if (got_100 - want).abs() < 0.01 { reproduce_at_100 += 1; }
        }
        // The "four of eight" figure in this test's doc and in `slide()`'s, asserted rather than
        // asserted-in-prose. A change that made the extent stop mattering would land here.
        assert_eq!(reproduce_at_100, 4,
            "exactly four of the eight doc-table columns must still reproduce at half_extent=100");
    }
}
