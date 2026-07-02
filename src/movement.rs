//! Unified character controller (design §2-3).
//!
//! The [`CharacterController`] is the SOLE owner of the local player's physical state — position,
//! vertical velocity, on-ground and in-water flags. Whoever drives (WASD on the render thread, or
//! the `/goto` planner on the nav thread) writes a [`MoveIntent`]; `step` integrates it against the
//! zone [`Collision`] using swept-cylinder collide-and-slide, native-parity ground/step handling,
//! and a depenetration / unstuck net, and returns the one authoritative position used for both the
//! render and the server stream. This replaces the old `override_pos` dual-authority artifact.

use crate::assets::Collision;

/// Native RoF2 wall-collision sphere radius (`eqgame.exe.asm:0x00440418`, `fld1`).
pub const PLAYER_RADIUS: f32 = 1.0;
/// Skin width kept between the cylinder and the surface after a swept hit.
const SKIN: f32 = 0.05;
/// Native step-up height (`_DAT_009c58e8 = 2.0f`). Used for free WASD movement.
const STEP_UP: f32 = 2.0;
/// Step-up height the `/goto` planner permits (via [`MoveIntent::climb`]). Matches `find_path`'s
/// edge-climb cap (`Collision::find_path` `STEP_H = 20.0`): whatever A* routes over as a connected
/// walkable ledge, the controller can then actually climb — so it no longer wedges on the small
/// fences/cart lips that gate penned content (#41). The floor-existence guard in `try_step_up`
/// still only mounts a real surface, so this is not a wall-scaling cheat.
pub const NAV_CLIMB: f32 = 20.0;
/// Ground-probe origin above the feet (`_DAT_009c3390 = 1.0f`).
const GROUND_ORIGIN: f32 = 1.0;
/// Ground-probe downward range (`_DAT_009c58e4 = 200.0f`).
const GROUND_DEPTH: f32 = 200.0;
/// Gravity / terminal fall (matches the renderer's prior physics + falling-physics.md).
const GRAVITY: f32 = 120.0;
const MAX_FALL: f32 = 128.0;
/// Jump impulse for the free-WASD Space jump. Peak height = v²/(2·GRAVITY); at 31 that's ~4.0u —
/// enough to clear/mount low ledges, steps and small crates (well above the 2u step-up), matching
/// the reference RoF2 client's usable jump. The old value (13 → only ~0.7u peak, "barely leaves
/// the ground") was a placeholder carried over from the pre-controller WASD block (eqoxide#92).
/// (Exact RoF2 parity of the impulse is worth a decompile/live check; 4u restores a usable jump.)
pub const JUMP_VELOCITY: f32 = 31.0;
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
/// Ring push-out search radii (units).
const PUSHOUT_RADII: [f32; 6] = [1.0, 2.0, 4.0, 8.0, 16.0, 32.0];
/// Directions sampled per push-out ring.
const PUSHOUT_DIRS: usize = 16;

/// What the driver wants this frame. `wish_dir` is a horizontal direction in server axes
/// (east, north); magnitude is treated as a throttle (clamped to 1). `speed` is run speed (u/s).
#[derive(Clone, Copy, Debug, Default)]
pub struct MoveIntent {
    pub wish_dir:    [f32; 2],
    pub wish_vspeed: f32,
    pub jump:        bool,
    pub want_swim:   bool,
    pub speed:       f32,
    /// Max step-up height the controller may climb this move, in EQ units. `0` (default) uses the
    /// native [`STEP_UP`] (2.0) — correct for free WASD, which must NOT be able to scale walls. The
    /// `/goto` planner raises it to [`NAV_CLIMB`] so the controller can surmount the small lips
    /// (fences/cart edges) that `find_path` already routed over (its edge-climb cap is the same).
    /// Without this the path leads over a lip the 2u step can't clear and the player wedges (#41).
    pub climb:       f32,
    /// One-shot request to hop a low barrier (fence/cart) this tick. The `/goto` planner sets it once
    /// its own net-progress stall detection fires (the controller can't see net progress — sliding
    /// ALONG a fence looks like good per-frame motion). The controller hops only if it's grounded,
    /// off cooldown, and a near-level landing exists just beyond ([`can_hop`]). Free WASD leaves it
    /// `false` (a player walking into a wall shouldn't auto-jump). Fixes the Halas sled-pen (#41).
    pub hop:         bool,
}

/// A read-only snapshot of the controller the render thread publishes each frame for the nav
/// thread to stream to the server (design §2 "Threading"). `heading` is EQ-CCW degrees.
#[derive(Clone, Copy, Debug, Default)]
pub struct ControllerView {
    pub pos:     [f32; 3],
    pub heading: f32,
    pub moving:  bool,
    /// False until the render thread has spawned and seeded the controller. The nav streamer must
    /// not mirror/stream a default (origin) position before this is set.
    pub initialized: bool,
}

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
    stuck_time:    f32,
    /// Seconds until another nav auto-hop is allowed (prevents jump-spamming a wall we can't clear).
    hop_cooldown:  f32,
}

#[inline]
fn hlen(d: [f32; 3]) -> f32 { (d[0] * d[0] + d[1] * d[1]).sqrt() }

impl CharacterController {
    pub fn new(pos: [f32; 3]) -> Self {
        Self { pos, vel_z: 0.0, on_ground: false, in_water: false,
               good: std::collections::VecDeque::new(), good_timer: 0.0, stuck_time: 0.0,
               hop_cooldown: 0.0 }
    }

    /// Hard-set the position (zone-in, teleport, large server correction). Clears velocity & stuck.
    pub fn teleport(&mut self, pos: [f32; 3]) {
        self.pos = pos;
        self.vel_z = 0.0;
        self.on_ground = false;
        self.stuck_time = 0.0;
    }

    /// Advance one frame. Returns the new authoritative position.
    pub fn step(&mut self, intent: MoveIntent, dt: f32, col: &Collision) -> [f32; 3] {
        // Depenetration / unstuck net runs first (§3.3). If it handled an embedded frame, freeze
        // the rest of the step so we neither slide deeper nor fall through void.
        if self.depenetrate(dt, col) {
            return self.pos;
        }

        self.in_water = col.in_water(self.pos);
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
            // Free WASD uses the native 2u step; the nav planner raises `climb` to NAV_CLIMB so the
            // controller can follow find_path over fence/cart lips it routed across (#41).
            let max_step = STEP_UP.max(intent.climb);
            if self.on_ground && low_hit && low_prog + 0.01 < hlen(wish) {
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

        // ── Vertical: swim / jump / gravity + ground clamp. ──
        if swimming {
            self.on_ground = false;
            self.vel_z = 0.0;
            self.pos[2] += intent.wish_vspeed * dt;
        } else {
            if intent.jump && self.on_ground {
                self.vel_z = JUMP_VELOCITY;
                self.on_ground = false;
            }
            let foot = self.pos[2];
            let floor = col.ground_below(self.pos[0], self.pos[1], foot + GROUND_ORIGIN, GROUND_DEPTH);
            if self.on_ground {
                match floor {
                    Some(f) if (f - foot).abs() <= GROUND_SNAP_TOL || f > foot => self.pos[2] = f,
                    _ => self.on_ground = false, // floor dropped away / vanished → start falling
                }
            }
            if !self.on_ground {
                self.vel_z = (self.vel_z - GRAVITY * dt).max(-MAX_FALL);
                let cand = self.pos[2] + self.vel_z * dt;
                match floor {
                    Some(f) if cand <= f => { self.pos[2] = f; self.vel_z = 0.0; self.on_ground = true; }
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
        const FOOT: f32 = 0.5;
        const CHEST: f32 = 4.0;
        let mut pos = from;
        let mut remaining = delta;
        let mut hit_any = false;
        for _ in 0..MAX_SLIDE_ITERS {
            let len = hlen(remaining);
            if len < 1e-5 { break; }
            let d_hat = [remaining[0] / len, remaining[1] / len];
            // Nearest contact among the foot and chest centre rays.
            let mut best: Option<crate::assets::Hit> = None;
            for &hz in &[FOOT, CHEST] {
                let f = [pos[0], pos[1], pos[2] + hz];
                let to = [f[0] + remaining[0], f[1] + remaining[1], f[2]];
                if let Some((t, n)) = col.nearest_hit(f, to) {
                    if best.map_or(true, |b| t < b.t) { best = Some(crate::assets::Hit { t, normal: n }); }
                }
            }
            match best {
                None => { pos[0] += remaining[0]; pos[1] += remaining[1]; break; }
                Some(hit) => {
                    hit_any = true;
                    // Distance into the plane along the motion (floored so grazing hits don't blow up).
                    let ndot = (-(d_hat[0] * hit.normal[0] + d_hat[1] * hit.normal[1])).max(0.05);
                    let contact = hit.t * len;
                    let advance = (contact - PLAYER_RADIUS / ndot - SKIN).max(0.0);
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
    fn depenetrate(&mut self, dt: f32, col: &Collision) -> bool {
        // No geometry loaded → no constraints; never teleport the free player.
        if !col.has_geometry() {
            self.stuck_time = 0.0;
            return false;
        }
        let p = self.pos;
        let clear = col.footprint_clear(p[0], p[1], p[2], PLAYER_RADIUS, PUSHOUT_DIRS / 2);
        let floor = col.ground_below(p[0], p[1], p[2] + GROUND_ORIGIN, GROUND_DEPTH);
        let embedded = !clear || floor.is_none();
        if !embedded {
            self.stuck_time = 0.0;
            self.good_timer += dt;
            if self.on_ground && self.good_timer >= GOOD_SAMPLE_SECS {
                self.good_timer = 0.0;
                if self.good.len() >= 8 { self.good.pop_front(); }
                self.good.push_back(self.pos);
            }
            return false;
        }
        // Embedded: try a ring push-out to the nearest clear, floored spot.
        for &r in &PUSHOUT_RADII {
            for i in 0..PUSHOUT_DIRS {
                let a = (i as f32) / (PUSHOUT_DIRS as f32) * std::f32::consts::TAU;
                let (e, n) = (p[0] + a.cos() * r, p[1] + a.sin() * r);
                if !col.footprint_clear(e, n, p[2], PLAYER_RADIUS, PUSHOUT_DIRS / 2) { continue; }
                if let Some(f) = col.nearest_floor(e, n, p[2], STEP_UP + GROUND_ORIGIN, GROUND_DEPTH) {
                    self.pos = [e, n, f];
                    self.vel_z = 0.0;
                    self.on_ground = true;
                    self.stuck_time = 0.0;
                    tracing::debug!("depenetrate: pushed out from ({:.1},{:.1}) to ({:.1},{:.1},{:.1})",
                        p[0], p[1], e, n, f);
                    return true;
                }
            }
        }
        // Push-out failed: count time stuck, then fall back to the most recent good position.
        self.stuck_time += dt;
        if self.stuck_time >= STUCK_FALLBACK_SECS {
            if let Some(&g) = self.good.back() {
                tracing::info!("depenetrate: stuck {:.1}s, falling back to last good pos {:?}", self.stuck_time, g);
                self.pos = g;
                self.vel_z = 0.0;
                self.on_ground = true;
                self.stuck_time = 0.0;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::{Collision, ZoneAssets, MeshData, RenderMode};

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
    fn nav_climb_surmounts_a_fence_lip_that_walk_cannot() {
        // A 6u lip (fence/cart) the native 2u step can't clear but find_path routes over: floor z=0,
        // a 6u riser at east=5, floor z=6 beyond. This is the Halas sled-dog pen case (#41).
        let geo = || col(vec![floor(0.0, -100.0, 5.0), wall(5.0, 0.0, 6.0), floor(6.0, 5.0, 100.0)]);

        // Free WASD (climb=0 → native 2u step): blocked at the lip, stays at z=0.
        let mut wasd = CharacterController::new([3.0, 0.0, 0.0]);
        wasd.on_ground = true;
        for _ in 0..5 { wasd.step(walk(35.0, [1.0, 0.0]), 0.1, &geo()); }
        assert!(wasd.pos[0] < 5.1, "WASD must NOT scale a 6u lip: east={}", wasd.pos[0]);
        assert!(wasd.pos[2] < 1.0, "WASD should stay at floor z=0: {}", wasd.pos[2]);

        // Nav (climb=NAV_CLIMB): climbs the lip and stands on the z=6 surface inside the pen.
        let nav_intent = MoveIntent { wish_dir: [1.0, 0.0], wish_vspeed: 0.0, jump: false,
            want_swim: false, speed: 35.0, climb: NAV_CLIMB, hop: false };
        let mut nav = CharacterController::new([3.0, 0.0, 0.0]);
        nav.on_ground = true;
        for _ in 0..5 { nav.step(nav_intent, 0.1, &geo()); }
        assert!(nav.pos[0] > 5.0, "nav should climb past the lip edge: east={}", nav.pos[0]);
        assert!((nav.pos[2] - 6.0).abs() < 0.3, "nav should stand on the 6u ledge: {}", nav.pos[2]);
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
            want_swim: false, speed: 35.0, climb: NAV_CLIMB, hop: true };
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
    }
}
