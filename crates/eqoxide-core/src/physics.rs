//! Pure movement/physics constants and calculations shared by the controller (`movement`), the
//! action loop (`eq_net::action_loop`), and the navigation planner/walker (`nav`).
//!
//! These are dependency-free scalars and `f32 -> …` functions — no wgpu/winit/tokio/net/app types —
//! so they live in the leaf `eqoxide-core` crate (#544 Step 2d). The behavior that *operates* on
//! them (the `CharacterController`, the packet builders) stays in the app crate; only the numbers and
//! the pure kinematics moved down. Each original site re-exports the symbol it used to define, so
//! `crate::movement::{PLAYER_RADIUS,STEP_UP,JUMP_VELOCITY,running_jump_reach}`,
//! `crate::eq_net::action_loop::RUN_SPEED`, and `crate::eq_net::protocol::fall_damage` all keep
//! resolving unchanged. Keeping a single source of truth here also prevents the two sides of a shared
//! physics number from silently drifting apart.

/// Wall-collision sphere radius, matched to the reference RoF2 client.
pub const PLAYER_RADIUS: f32 = 1.0;

/// Step-up height, matched to the reference RoF2 client. This is a HARD cap: the
/// native client can auto-step a ledge at most 2.0u tall; anything taller is a wall (jump or go
/// around) — there is no larger climb and no separate slope check. It is the single source of truth
/// for how high nav may climb, so `find_path` derives its edge-climb cap (`STEP_H`) from it. Both
/// free WASD and the nav walker are clamped to this — navigation must never climb what a WASD player
/// can't (#239). (Was decoupled from a super-human `NAV_CLIMB = 20.0`, which teleported the walker up
/// 20u ridges/invisible walls and stranded it on the high side of boundaries.)
pub const STEP_UP: f32 = 2.0;

/// Gravity / terminal fall (matches the renderer's prior physics + falling-physics.md).
pub const GRAVITY: f32 = 120.0;

/// Jump impulse for the free-WASD Space jump. Peak height = v²/(2·GRAVITY); at 31 that's ~4.0u —
/// enough to clear/mount low ledges, steps and small crates (well above the 2u step-up), matching
/// the reference RoF2 client's usable jump. The old value (13 → only ~0.7u peak, "barely leaves
/// the ground") was a placeholder carried over from the pre-controller WASD block (eqoxide#92).
/// (Exact RoF2 parity of the impulse is worth a live check; 4u restores a usable jump.)
pub const JUMP_VELOCITY: f32 = 31.0;

/// Native Titanium base run speed (u/s). The action loop tags outbound move intents with it and the
/// nav walker/steering integrate at it, so it is the single source of truth for both drivers.
pub const RUN_SPEED: f32 = 44.0;

/// Horizontal distance a *running* jump clears to a landing at roughly takeoff height, at
/// `run_speed` (u/s). The character leaves the ground at `JUMP_VELOCITY` and, ignoring the small
/// landing-height difference, is airborne for `2·JUMP_VELOCITY/GRAVITY` seconds (up then back to
/// takeoff height); horizontal reach = airborne_time · run_speed. `find_path` uses this to add
/// jump-edges across genuine floor gaps no wider than a jump can bridge (eqoxide#190). A landing
/// that is LOWER than takeoff gives more airborne time, so this is a conservative (minimum) reach.
pub fn running_jump_reach(run_speed: f32) -> f32 {
    let air_time = 2.0 * JUMP_VELOCITY / GRAVITY;
    air_time * run_speed
}

/// Native Titanium fall damage for a fall of `height` EQ units. Fall damage is CLIENT-computed in
/// EQ (the server only validates OP_EnvDamage). Model: impact velocity = min(terminal,
/// sqrt(2·g·h)) converted to the client's internal per-update z-velocity units (~5-13); then
/// `fall_score = |z_vel| − 4` (char_counter≈0, no safe-fall skill): ≤0 → no damage, ≥9 → lethal
/// (20000), else a roll in `[0, score²·10]`. Returns (rolled_damage, max_damage). See
/// ~/git/eq_kb/falling-physics.md.
pub fn fall_damage(height: f32) -> (u32, u32) {
    const GRAVITY: f32 = 120.0;   // matches the renderer's fall physics
    const TERMINAL: f32 = 128.0;  // native internal z-velocity clamp
    const HZ: f32 = 10.0;         // native position-update rate the formula is calibrated to
    let v = (2.0 * GRAVITY * height.max(0.0)).sqrt().min(TERMINAL);
    let score = v / HZ - 4.0;
    if score <= 0.0 { return (0, 0); }
    if score >= 9.0 { return (20_000, 20_000); }
    let max = (score * score * 10.0) as u32;
    let roll = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos()).unwrap_or(0);
    (if max == 0 { 0 } else { roll % (max + 1) }, max)
}
