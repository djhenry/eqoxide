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
/// Ground-probe origin above the feet.
const GROUND_ORIGIN: f32 = 1.0;
/// Ground-probe downward range.
const GROUND_DEPTH: f32 = 200.0;
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
/// Seconds embedded with no push-out before falling back to the last good grounded position.
const STUCK_FALLBACK_SECS: f32 = 0.5;
/// How often (seconds) a good grounded position is sampled into the ring buffer.
const GOOD_SAMPLE_SECS: f32 = 0.5;
/// Capacity of the last-good ring. Was a bare literal `8` at the one push site; #720's round-2
/// review then cited a `GOOD_RING` constant that did not exist, and #724 asked for the name so the
/// next citation is checkable.
///
/// **This number is DEAD, not merely untested.** The only production reads of the ring are
/// `self.good.back()` at the two recovery sites (the #150 fall-through guard in [`
/// CharacterController::step`] and the stuck fallback in [`CharacterController::depenetrate`]);
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
    /// `ControllerView::hold` → `GameState::player_hold` → `GET /v1/observe` `player.hold`.
    ///
    /// Physics never reads this; it is purely the disclosure that the body is frozen. `secs` is the
    /// controller's own accumulated frame time for the current, unbroken hold.
    hold:          Option<ControllerHold>,
    stuck_time:    f32,
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
fn is_embedded(col: &Collision, p: [f32; 3]) -> bool {
    !col.footprint_clear(p[0], p[1], p[2], PLAYER_RADIUS, PUSHOUT_DIRS / 2)
        || col.ground_below(p[0], p[1], p[2] + GROUND_ORIGIN, GROUND_DEPTH).is_none()
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
/// `on_ground` (#649). A swimmer in a ~12 u flooded pocket fails `footprint_clear` as a matter of
/// course — geometry is within a body radius on every side — which the net read as "embedded in
/// rock" and recovered by hunting the NEAREST floor with
/// `nearest_floor(up = STEP_UP + GROUND_ORIGIN, down = GROUND_DEPTH)`. That search takes whichever
/// floor is closer, not one the character can occupy, so it teleported swimmers in BOTH directions:
/// UP onto the tile floor 2.009 u above the pocket's swim plane (0.009 u above the waterline, hence
/// dry, hence buoyancy never fires again — the live #329 wedge coordinate), and DOWN 10–12 u onto
/// the pool floor from anywhere below it.
///
/// So the state is made unrepresentable instead of guarded: constructing a `Recovery` is the ONLY
/// way the net moves the character, [`Recovery::at_column`] is the ONLY constructor, and it picks the
/// variant from the MEDIUM. A future caller cannot forget a `if in_water` check, because there is no
/// check to forget — `Afloat` carries "feet unsupported" with it and [`CharacterController::recover`]
/// writes the matching flags.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Recovery {
    /// Standing on solid floor at this z. Feet supported: `on_ground = true`.
    Grounded(f32),
    /// Floating in a water column at this z — the body's OWN depth, unchanged. Feet unsupported
    /// (`on_ground = false`), so the next frame's swim/buoyancy branch still owns the body and can
    /// carry it to the surface or across into the next column.
    Afloat(f32),
}

impl Recovery {
    /// The recovery available in the candidate column `(e, n)` for a body whose feet are at `z`.
    /// `afloat` is the medium the body being recovered is IN, measured once at its own position.
    ///
    /// An afloat body is recovered **at its own depth**, never onto a floor, whenever the candidate
    /// column is still water there: a swimmer is not embedded in the sense the net assumes, so the
    /// only thing wrong with its position is the horizontal overlap the ring push-out is already
    /// resolving. Moving it vertically as well is what produced both #649 symptoms.
    ///
    /// Everything else — every dry body, and an afloat body whose candidate column is NOT water
    /// (it left the water laterally) — takes the original floor search, byte-identical.
    ///
    /// An `Afloat` candidate must ALSO be non-[`is_embedded`], which for a clear footprint means "a
    /// floor exists somewhere below". Without that clause the net hands back a spot it would flag
    /// again on the very next frame, and a swimmer over unbounded water drifts one ring-radius per
    /// frame for ever (#649 review, finding 1). When no candidate qualifies the ring simply runs out
    /// and the existing stuck / last-good machinery takes over, exactly as it does today for a dry
    /// body with nowhere to go — an afloat body is never quietly handed the floor search as a
    /// consolation prize, because that IS the water-blind behaviour this change removes.
    fn at_column(col: &Collision, e: f32, n: f32, z: f32, afloat: bool) -> Option<Self> {
        if afloat && body_in_water(col, [e, n, z]) {
            return (!is_embedded(col, [e, n, z])).then_some(Recovery::Afloat(z));
        }
        col.nearest_floor(e, n, z, STEP_UP + GROUND_ORIGIN, GROUND_DEPTH).map(Recovery::Grounded)
    }

    fn z(self) -> f32 { match self { Recovery::Grounded(z) | Recovery::Afloat(z) => z } }
    fn on_ground(self) -> bool { matches!(self, Recovery::Grounded(_)) }
}

impl CharacterController {
    pub fn new(pos: [f32; 3]) -> Self {
        Self { pos, vel_z: 0.0, on_ground: false, in_water: false,
               good: std::collections::VecDeque::new(), good_timer: 0.0, hold_log_cooldown: 0.0,
               hold: None,
               stuck_time: 0.0,
               hop_cooldown: 0.0, underworld: f32::NEG_INFINITY,
               airborne_start_z: None, landed_fall_height: None, levitating: false,
               swim_sinking: false }
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
    /// > ### ⚠️ Correction (#724)
    /// > This paragraph used to say the clear is "deliberately NOT folded into [`Self::teleport`]",
    /// > on the ground that a same-zone correction leaves the ring naming positions in the CURRENT
    /// > zone, so restoring one "is not nonsense the way a cross-zone restore is". The first half is
    /// > still true and the conclusion drawn from it was wrong: not-nonsense is not correct, and
    /// > #724's controller-level tests measure the same-zone failure end to end
    /// > (`a_large_same_zone_relocation_forgets_the_pre_relocation_recovery_ring` and the stuck
    /// > -fallback twin). `teleport` now clears the ring itself. The retracted reasoning is kept
    /// > here rather than deleted, because it is the reasoning that would otherwise be re-derived.
    /// >
    /// > That same paragraph also said the stale window lasts "for a few seconds". That is wrong —
    /// > the window is unbounded, and it is not `GOOD_SAMPLE_SECS`-shaped. The MECHANISM first
    /// > written here was wrong too; see the amendment below.
    ///
    /// > ### ⚠️ Correction to the correction (#724 round-2 review, B2)
    /// > The paragraph above originally read, under a **"Measured:"** label:
    /// >
    /// > > *the ring banks ONLY while `on_ground`, so a body relocated into a column it can only
    /// > > fall out of never banks again and the window is unbounded.*
    /// >
    /// > **That mechanism was measured FALSE**, and the "Measured" label was unearned — it was
    /// > reasoned from the banking site's `on_ground` gate and never instrumented. Round-2 review
    /// > instrumented the pre-fix behaviour (this method's own `zone_with_a_hole` fixture, driven by
    /// > a `teleport` that does not clear, printing the ring every 30 frames) and I re-ran it and
    /// > got the same numbers:
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
    /// > loop **self-reinforces**, not because banking stops. (The old sentence's "the ring still
    /// > holds only pre-relocation *samples*" was true of the values and false of the count, which
    /// > is why it read as evidence for a claim it did not support.)
    /// >
    /// > The conclusion is unchanged and still holds: the stale window is unbounded, so #724's
    /// > framing of a 0.5 s race the descent has to win is wrong. Only the reason is different.
    /// >
    /// > This was not an inert error. It is what hid the hold disclosure now implemented as
    /// > [`ControllerHold`]: had `step`'s recovering arm been read instead of reasoned about, the
    /// > obvious next question was what its `None` arm does *after* the fix — leave `on_ground`
    /// > false for ever — and what the depenetration twin of that arm does, which is nothing at all,
    /// > silently. "Never grounded again" made the hold look self-announcing when it was mute.
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
    /// *Label, because this PR was reviewed for exactly this:* that #593 gap is **read off the
    /// branch structure, not measured on the wire** — the note reasons about what `stream_position`
    /// does when `cd² <= CORRECTION_SQ`, and no run has been captured landing in it. The
    /// independent reason above (the collision-drop clear happens earlier than any arrival, so it
    /// cannot be the arrival's job) needs no measurement and is the one to lean on.
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
    pub fn hold(&self) -> Option<ControllerHold> { self.hold }

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
    pub fn clear_hold(&mut self) { self.hold = None; }

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
                ControllerHoldReason::EmbeddedNoRecovery => tracing::info!(
                    "controller HOLD [embedded_no_recovery]: embedded at {:?} for {:.1}s, push-out \
                     found nowhere to go and there is no recovery history to fall back to — the \
                     body is FROZEN (every step is skipped) until something relocates it. Published \
                     as player.hold; this line is throttled to one per {:.0}s while it lasts.",
                    self.pos, secs, HOLD_LOG_SECS),
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
        // is the better of the two. (An earlier draft of this sentence said #712 *measured* the
        // wrong answer "re-firing every 0.5 s". It did not: #712's measured record, quoted verbatim
        // in `zone_in_reground`'s doc above, is the stale PREVIOUS-zone recovery and the permanent
        // wedge, with no cadence in it. 0.5 s is `GOOD_SAMPLE_SECS`, this file's ring-BANKING
        // interval, which I had silently promoted into a re-fire rate for the server. Retracted
        // here rather than deleted because the audit that caught it — #724 round-2 review's "audit
        // every Measured label" — is the reason it is gone.) The hold is NOT free, and the first
        // draft of this comment said something false about that too:
        //
        //   ⚠️ RETRACTED (#724 round-2 review, B1). This comment used to justify the trade with
        //   "holding is a visible failure a further server correction can fix". Both halves were
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
        // throttle and published to agents as `player.hold` on `GET /v1/observe`. A hold is a
        // reported failure, which is what makes it better than a silent wrong answer.
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
        // A teleport / large server correction is a position discontinuity, NOT a fall: drop any
        // airborne tracking and any not-yet-consumed landing so a correction is never misread as a
        // fall landing (§442 hazard 2b — `app.rs` calls this from the `pos_correction` handler).
        self.airborne_start_z = None;
        self.landed_fall_height = None;
        self.swim_sinking = false; // #444: a teleport isn't a swim-down exit either
    }

    /// Advance one frame. Returns the new authoritative position.
    pub fn step(&mut self, intent: MoveIntent, dt: f32, col: &Collision) -> [f32; 3] {
        self.hold_log_cooldown = (self.hold_log_cooldown - dt).max(0.0);
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
        if self.depenetrate(dt, col, prev_hold) {
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
            if (self.on_ground || swimming) && low_hit && low_prog + 0.01 < hlen(wish) {
                if let Some(step) = self.try_step_up(wish, max_step, col) {
                    if hlen([step[0] - self.pos[0], step[1] - self.pos[1], 0.0]) > low_prog + 0.05 {
                        applied = step;
                        stepped = true;
                    }
                }
                // Step-up couldn't cross it. If nav allows, and we're wedged ~head-on (not sliding
                // along a wall) against a thin barrier with walkable floor just beyond, hop over it
                // (a fence has flat floor both sides, so there's nothing to step UP onto). The
                // airborne collide-and-slide below carries us forward over the rail (#41).
                if !stepped
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
                let want = intent.wish_vspeed * dt;
                if want > 0.0 {
                    let mut rise = self.swim_rise(want, col);
                    if let Some(surf) = col.water_surface(water_at) {
                        rise = rise.min((surf - self.pos[2]).max(0.0));
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
            // ⚠️ CORRECTED (#724 round-2 review B2; the STATED GROUND corrected again in round 3
            // review B2). This used to read "(a server correction or the #150 underworld guard would
            // otherwise have to recover us)". The server-correction half is dropped.
            //
            // Why, precisely — because the first attempt at this correction got the reason wrong and
            // the reason is itself a claim. The retracted parenthetical is a COUNTERFACTUAL about
            // the branch we do not take: *if* we free-fell, one of those two would have to recover
            // us. Round 3 justified dropping it with "a swimmer holding altitude streams a position
            // the server agrees with, so no correction is generated" — which is a fact about the
            // branch we DO take, and so cannot bear on the counterfactual at all. That was a
            // category error, and shipping it inside a correction is the same defect wearing the
            // fix's clothes. It is retracted here rather than silently rewritten.
            //
            // The ground that does reach it: whether a free-falling swimmer sinking past the world
            // would in fact draw a server-side relocation is SERVER behaviour. No run in this PR or
            // in any of its review rounds measured it, and the review that raised this said the same
            // in as many words. It is not established FALSE — it is not established. Naming an
            // unmeasured server rescue as a known consequence, in a comment that a future reader
            // will lean on, is exactly the habit #724 exists to break (see `forget_recovery_history`
            // and `teleport`: the client had been treating "the server will put us back" as a
            // mechanism it could rely on). So the half we cannot check is dropped, and the half we
            // can — the #150 underworld guard, which is our own code — is kept, above, in the same
            // counterfactual form it always had.
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
                // ⚠️ CORRECTED (#724 round-2 review, B2). This line used to end "…and let a server
                // correction sort it". That was WRONG, and it was wrong about the exact branch #724
                // now labels `UnderworldNoRecovery`: the held body goes on streaming its own
                // unchanged position, the server agrees with it, and so nothing generates a further
                // correction. Nothing sorts it. The hold is terminal until a GM `#goto`/`#summon`
                // moves the character or it zones out — which is why the branch below now reports a
                // `ControllerHold` instead of relying on a rescue that was never coming. Retracted
                // in place rather than deleted: `forget_recovery_history` retracts the same claim in
                // this file, `docs/http-api.md` states the opposite in bold, and this comment is the
                // FIRST thing a reader of the underworld hold meets. (Found by the reviewer grepping
                // the CONCEPT, "server correction", not the #724 label — my own audit was scoped to
                // what this PR wrote and could not reach a pre-existing line.)
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
                        let recovered = match self.good.back().copied() {
                            Some(g) => { self.pos = g; self.on_ground = true; true }
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
        self.pos
    }

    /// Iterative collide-and-slide of a horizontal `delta` from `from`. Returns the resolved
    /// position and whether any surface was hit. (Design §3.1.)
    ///
    /// Uses the centre ray (at foot and chest heights) for the contact, then backs the cylinder
    /// centre off by `radius` measured along the hit normal — a penetration-free "ray + radius"
    /// capsule approximation. Grazing cases the thin centre ray slips past are caught next frame by
    /// the depenetration net (§3.3).
    fn slide(&self, from: [f32; 3], delta: [f32; 3], col: &Collision) -> ([f32; 3], bool) {
        // The contact heights AND the radius come from the ONE shared body (#386, #378 Phase 2):
        // the chest ray here and the planner's top edge probe are the same `Body::chest` field, and
        // the back-off radius is `Body::radius` — the planner can never again clear a band this ray
        // collides with, nor plan to a clearance this back-off disagrees with.
        let body = &crate::traversability::PLAYER_BODY;
        let probes = body.contact_probes();
        let radius = body.radius;
        let mut pos = from;
        let mut remaining = delta;
        let mut hit_any = false;
        for _ in 0..MAX_SLIDE_ITERS {
            let len = hlen(remaining);
            if len < 1e-5 { break; }
            let d_hat = [remaining[0] / len, remaining[1] / len];
            // Nearest contact among the foot and chest centre rays.
            let mut best: Option<crate::nav::collision::Hit> = None;
            for &hz in &probes {
                let f = [pos[0], pos[1], pos[2] + hz];
                let to = [f[0] + remaining[0], f[1] + remaining[1], f[2]];
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
                    let contact = hit.t * len;
                    let advance = (contact - radius / ndot - SKIN).max(0.0);
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
    fn swim_sink(&self, want: f32, col: &Collision) -> f32 {
        let to = [self.pos[0], self.pos[1], self.pos[2] + want];
        match col.nearest_hit(self.pos, to) {
            Some((t, _)) => (t * want + SKIN).min(0.0),
            None => want,
        }
    }

    /// Step-offset climb (design §3.2): raise the cylinder by `STEP_UP`, sweep again, and — only if
    /// a floor exists to stand on at the raised destination (the no-geometry-gap guard) — return the
    /// stepped-up `[east, north, floor_z]`. `None` = no valid step (taller-than-2u wall or a gap).
    fn try_step_up(&self, wish: [f32; 3], max_step: f32, col: &Collision) -> Option<[f32; 3]> {
        let raised = [self.pos[0], self.pos[1], self.pos[2] + max_step];
        let (hi, _) = self.slide(raised, wish, col);
        // Probe for a floor near the raised destination, within the step band. The slide above only
        // makes progress when there is open space over the lip, so we never "climb" into solid wall;
        // and a floor must exist here to stand on, so a taller bare wall still returns None.
        let f = col.ground_below(hi[0], hi[1], self.pos[2] + max_step + GROUND_ORIGIN, max_step + GROUND_ORIGIN + GROUND_SNAP_TOL)?;
        if f >= self.pos[2] - GROUND_SNAP_TOL && f - self.pos[2] <= max_step + GROUND_SNAP_TOL {
            Some([hi[0], hi[1], f])
        } else {
            None
        }
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
        if !is_embedded(col, p) {
            self.stuck_time = 0.0;
            self.good_timer += dt;
            if self.on_ground && self.good_timer >= GOOD_SAMPLE_SECS {
                self.good_timer = 0.0;
                if self.good.len() >= GOOD_RING_LEN { self.good.pop_front(); }
                self.good.push_back(self.pos);
            }
            return false;
        }
        // Embedded: try a ring push-out to the nearest clear spot the body can OCCUPY. What
        // "occupy" means depends on the medium, which is measured ONCE here, at the body's own
        // position, and handed to `Recovery::at_column` — see `Recovery` for why a swimmer must not
        // be recovered onto a floor in either direction (#649).
        let afloat = body_in_water(col, p);
        for &r in &PUSHOUT_RADII {
            for i in 0..PUSHOUT_DIRS {
                let a = (i as f32) / (PUSHOUT_DIRS as f32) * std::f32::consts::TAU;
                let (e, n) = (p[0] + a.cos() * r, p[1] + a.sin() * r);
                if !col.footprint_clear(e, n, p[2], PLAYER_RADIUS, PUSHOUT_DIRS / 2) { continue; }
                if let Some(rec) = Recovery::at_column(col, e, n, p[2], afloat) {
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
            match self.good.back().copied() {
                Some(g) => {
                    tracing::info!("depenetrate: stuck {:.1}s, falling back to last good pos {:?}", self.stuck_time, g);
                    // The ring buffer only ever samples GROUNDED positions (see the `!embedded` arm
                    // above), so this fallback is a `Grounded` recovery by construction — routed through
                    // the same single writer so the net has exactly one place that sets the support flags.
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
                None => self.enter_hold(ControllerHoldReason::EmbeddedNoRecovery, dt, prev_hold),
            }
        }
        true
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
        ctrl.step(walk(35.0, [0.7071, 0.7071]), 0.1, &c);
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

    // ── #649: the depenetration net must not teleport a SWIMMER vertically ──────────────────────
    //
    // A body in water that fails `footprint_clear` is NOT "embedded in rock" in the sense the net
    // assumes — a swimmer in a narrow flooded pocket has geometry within a body radius as a matter
    // of course. The net used to recover it with `nearest_floor(up = STEP_UP + GROUND_ORIGIN = 3,
    // down = GROUND_DEPTH = 200)`, which takes whichever floor is NEARER rather than one the body
    // can occupy, and then declared `on_ground = true`. One mechanism, two symptoms, both pinned
    // below: it MOUNTS a swimmer on a slab above it, and it DROPS one onto the pool floor below.
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
        assert!(c.footprint_clear(ctrl.pos[0], ctrl.pos[1], ctrl.pos[2], PLAYER_RADIUS, 8),
            "the push-out must still have resolved the horizontal overlap: {:?}", ctrl.pos);
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
    fn depenetration_grounds_a_swimmer_pushed_out_of_the_water_entirely() {
        // The other arm of the medium test: a body that IS afloat but whose only clear neighbour is
        // OUTSIDE the water takes the ordinary floor recovery, unchanged. Water is a 4 u-wide box
        // around the corridor; the push-out's first clear ring point (east ±2) is outside it.
        let mut c = col(vec![floor(-12.0, -100.0, 100.0), floor(2.0, -100.0, 100.0),
                             wall(0.8, -12.0, 10.0), wall(-0.8, -12.0, 10.0)]);
        c.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::box_below(-100.0, 100.0, -1.0, 1.0, 0.5))));
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        assert!(c.in_water([0.0, 0.0, 0.0]) && !c.in_water([2.0, 0.0, 0.0]),
            "fixture: afloat at the centre, dry two units east — else this arm is never exercised");
        ctrl.step(swim_still(), 1.0 / 60.0, &c);
        assert!((ctrl.pos[2] - 2.0).abs() < 1e-3 && ctrl.on_ground,
            "leaving the water laterally must still recover onto a floor and ground: {:?}", ctrl.pos);
    }

    #[test]
    fn an_afloat_body_with_no_floor_below_is_never_pushed_out_into_a_drift() {
        // #649 REVIEW FINDING 1 — a REGRESSION PIN, not a fails-on-main pin: `main` passes this too.
        // `is_embedded` counts `floor.is_none()` as embedded, so a swimmer in deep water with a
        // perfectly CLEAR footprint and no floor within GROUND_DEPTH below still enters the net. The
        // first cut of this fix answered that with `Recovery::Afloat` at the first ring candidate —
        // which is *equally* embedded, so the next frame re-entered the net from there and the body
        // walked east one ring radius per frame (60 u/s), ignoring the wish input, reporting a stale
        // `in_water`. A recovery that is itself embedded is not a recovery.
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
        let mut c = col(vec![floor(-12.0, -100.0, 100.0), floor(2.0, -100.0, 100.0),
                             wall(0.8, -12.0, 10.0), wall(-0.8, -12.0, 10.0)]);
        // Water from 0.5 up: the feet at z=0 are OUTSIDE it, the chest at z=3 is inside.
        c.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::water_slab(0.5, 10.0))));
        let mut ctrl = CharacterController::new([0.0, 0.0, 0.0]);
        assert!(!c.in_water([0.0, 0.0, 0.0]) && c.in_water([0.0, 0.0, 3.0]),
            "fixture: feet dry, chest wet — else this is not the case that distinguishes the probes");

        ctrl.step(swim_still(), 1.0 / 60.0, &c);

        assert!(ctrl.pos[2].abs() < 1e-3 && !ctrl.on_ground,
            "a submerged body whose FEET are outside the water volume is still afloat: a feet-only \
             probe in the net calls it dry and mounts it on the slab at z=2 — got {:?}", ctrl.pos);
    }

    /// **THE DEPENETRATION CORPUS — the blast-radius harness, committed so its numbers are
    /// reproducible (#649 review, finding 6).**
    ///
    /// Two things at once, over every baked zone found at `$EQZONES`:
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
        fn new_recovery(col: &Collision, p: [f32; 3]) -> Option<([f32; 3], bool)> {
            let afloat = body_in_water(col, p);
            for &r in &PUSHOUT_RADII {
                for i in 0..PUSHOUT_DIRS {
                    let a = (i as f32) / (PUSHOUT_DIRS as f32) * std::f32::consts::TAU;
                    let (e, n) = (p[0] + a.cos() * r, p[1] + a.sin() * r);
                    if !col.footprint_clear(e, n, p[2], PLAYER_RADIUS, PUSHOUT_DIRS / 2) { continue; }
                    if let Some(rec) = Recovery::at_column(col, e, n, p[2], afloat) {
                        return Some(([e, n, rec.z()], rec.on_ground()));
                    }
                }
            }
            None
        }

        let mut zones: Vec<String> = std::fs::read_dir(&dir).expect("$EQZONES").filter_map(|e| {
            let path = e.ok()?.path();
            let n = path.file_name()?.to_str()?.strip_suffix(".glb")?.to_string();
            (!n.ends_with("_doors") && !n.ends_with("_obj")).then_some(n)
        }).collect();
        zones.sort();
        assert!(!zones.is_empty(), "no baked zones at {dir:?}");

        let (mut t_zones, mut t_emb) = (0usize, 0u64);
        let (mut ch_dry, mut ch_chest, mut ch_wet) = (0u64, 0u64, 0u64);
        let (mut same_dry, mut same_chest, mut same_wet) = (0u64, 0u64, 0u64);
        let (mut none_legacy, mut none_new) = (0u64, 0u64);
        let mut drifters: Vec<(String, [f32; 3], [f32; 3])> = Vec::new();
        for name in &zones {
            let Ok(za) = crate::assets::ZoneAssets::from_glb(&dir.join(format!("{name}.glb"))) else { continue };
            let mut col = Collision::build(&za, 32.0);
            if col.cols == 0 { continue; }
            col.set_water(crate::region_map::RegionMap::load(&dir.join("maps/water"), name)
                .map(std::sync::Arc::new));
            t_zones += 1;
            let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut rnd = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
                               (seed >> 11) as f64 / (1u64 << 53) as f64 };
            let mut zone_emb = 0u32;
            for _ in 0..500 {
                let e = col.origin[0] + rnd() as f32 * (col.cols as f32 * col.cell_size);
                let n = col.origin[1] + rnd() as f32 * (col.rows as f32 * col.cell_size);
                let Some(fz) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { continue };
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
        }
        println!("\nzones={t_zones} embedded={t_emb}\n  changed: dry-body={ch_dry} wet-chest-dry-feet={ch_chest} \
                  submerged={ch_wet}\n  unchanged: dry-body={same_dry} wet-chest-dry-feet={same_chest} \
                  submerged={same_wet}\n  no recovery: legacy={none_legacy} new={none_new}");
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
        // The ring buffer only ever samples GROUNDED positions, so the fallback recovers a body that
        // IS standing — pinned here because #649 routed this write through the shared `recover`
        // (`Recovery::Grounded`) and an unpinned refactor is an unnoticed behaviour change.
        assert!(ctrl.on_ground, "the last-good position is a grounded one: {:?}", ctrl.pos);
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
    /// MUTATION-CHECK: disabling the `was_in_water && was_swim_sinking && !self.in_water &&
    /// !self.on_ground && ...` re-arm added for #444 (verified by short-circuiting it to `false`)
    /// makes this RED — `take_landed_fall_height()` returns `None` instead of `Some(height)`. The
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
    /// It must latch NO fall height. Pre-tightening this false-positived (the gate was `wish_vspeed <
    /// 0` alone), re-arming `airborne_start_z` and latching a spurious `Some(~0)` — a phantom "fall"
    /// for a purely lateral exit, violating the §442 DEFECT-1 invariant (inert today only because
    /// `SAFE_FALL_HEIGHT` discards it, but wrong).
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
        let c = col(vec![floor(0.0, -100.0, -50.0), wall(39.2, 0.0, 10.0), wall(40.8, 0.0, 10.0)]);
        let mut ctrl = CharacterController::new([-80.0, 0.0, 0.0]);
        ctrl.on_ground = true;
        ctrl.set_underworld(Some(-222.0));
        for _ in 0..60 { ctrl.step(walk(0.0, [0.0, 0.0]), 1.0 / 30.0, &c); }
        let stale = *ctrl.good.back().expect("fixture: must bank a good sample");

        // Fixture, checked against the pure predicate so it holds under the mutation too: the slot
        // is a place the body reads as embedded, with nothing in push-out range to recover onto.
        let target = [40.0, 40.0, 0.0]; // summoned into the slot, 120 u from the platform
        assert!(is_embedded(&c, target), "fixture: the slot must read as embedded");

        ctrl.teleport(target);
        for _ in 0..40 { ctrl.step(walk(0.0, [0.0, 0.0]), 0.05, &c); } // 2 s ≫ STUCK_FALLBACK_SECS

        assert!(ctrl.pos != stale, "#724: stuck fallback restored the superseded position {stale:?}");
        assert!((ctrl.pos[0] - 40.0).abs() < 1e-3 && (ctrl.pos[1] - 40.0).abs() < 1e-3,
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
                col(vec![floor(0.0, -100.0, -50.0), wall(39.2, 0.0, 10.0), wall(40.8, 0.0, 10.0)])
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
                [40.0, 40.0, 0.0]
            } else {
                // z chosen so the sub-underworld deck is inside `GROUND_DEPTH` of the arrival:
                // the body then takes the gravity path and meets the #150 guard, rather than
                // reading as embedded (which is the other half of the sweep).
                [45.0 + rng.frac() * 50.0, -40.0 + rng.frac() * 80.0, -120.0 + rng.frac() * 60.0]
            };
            assert_eq!(is_embedded(&c, target), embedded_case,
                "case {case}: fixture must exercise the intended recovery path at {target:?}");
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
    fn platform_and_inescapable_slot() -> Collision {
        col(vec![floor(0.0, -100.0, -50.0), wall(39.2, 0.0, 10.0), wall(40.8, 0.0, 10.0)])
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

        let target = [40.0, 40.0, 0.0];
        assert!(is_embedded(&c, target), "fixture: the slot must read as embedded");
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
        ctrl.teleport([40.0, 40.0, 0.0]);
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
        ctrl.teleport([40.0, 40.0, 0.0]);
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
}
