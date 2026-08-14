//! EQ network client — protocol, transport, login flow, and gameplay loop.
//!
//! Extracted into the `eqoxide-net` workspace crate (#544 Step 2m). This crate is the MVC **Model**:
//! `packet_handler` decodes + APPLIES the RoF2 wire into `GameState` (every position, spawn, HP,
//! zone, inventory the agent sees), and `action_loop` drives the net loop + server reconciliation —
//! it is the sole authoritative writer of the shared world state. It depends only on the lower
//! structural crates (`eqoxide-core`/`ipc`/`command`/`nav`/`protocol`/`telemetry`) + externals
//! (tokio/des/cbc/byteorder/miniz_oxide/rand/…) — never on the app crate, renderer, gpu, or UI.
//! The single app-side type it needs, `MoveIntent`, it references from `eqoxide-ipc` directly.
//!
//! The app crate re-exports this crate as `eq_net` (`pub use eqoxide_net as eq_net;`) so every
//! existing `crate::eq_net::…` / `eqoxide::eq_net::…` path (main.rs spawns the net thread; app.rs,
//! ui/*, model.rs read published state) keeps resolving unchanged.

pub mod gameplay;
pub mod item;
pub mod login;
pub mod action_loop;
pub mod packet_handler;
pub mod packet_telemetry;
pub mod transport;
pub mod ucs;

// `wire` (the `WireReader` cursor) and `protocol` (every RoF2 packet decoder/struct/const) were
// extracted into the `eqoxide-protocol` crate (#544 Step 2j) so `packet_telemetry` and
// `http/observe` can reach the decoders without dragging in the whole app crate. Re-exported here
// so every existing `crate::wire::…` / `crate::protocol::…` path keeps resolving
// unchanged.
pub use eqoxide_protocol::{protocol, wire};

pub use login::run_login_flow;
pub use transport::AppPacket;

/// Test-only: mature a real [`eqoxide_core::afloat::AfloatStall`] from outside `eqoxide-core`.
///
/// **Why this exists (#846 round-2 review F1).** The two #846 fixes each clear a PAIR — `player_hold`
/// *and* `player_afloat_stall` — but until this module every fixture in this crate published
/// `(Some(hold), None)`, so every `assert!(…afloat_stall.is_none())` in those tests was already
/// satisfied by `GameState::begin_zone_in`'s own field clear and could not go red. The reviewer
/// demonstrated it with two half-neutering wrap mutations (withdraw/invalidate the hold but leave
/// the stall standing) that were **workspace-GREEN**. That is #778's reach-control lesson applied to
/// an *axis* rather than a branch: the assertion was in the visible window; the fixture never
/// reached it.
///
/// The stall cannot be fabricated — #800/#801 made a premature or invented `AfloatStall`
/// unrepresentable outside its defining module, and `crates/eqoxide-core/tests/afloat_unconstructible.rs`
/// pins that from across a crate boundary. So the only way to get one is the way the render thread
/// gets one: real `Wished` frames at a fixed position until the clock matures. This is that, and it
/// is shared rather than copied into each test module so the `expect` below cannot rot in one place
/// and keep passing in another.
#[cfg(test)]
pub(crate) mod test_afloat {
    use eqoxide_core::afloat::{AfloatFrame, AfloatStall, AfloatStallClock, AFLOAT_STALL_SECS};

    /// Mature a stall at `pos`. `+ 3` frames of slack: the first `Wished` frame opens the window at
    /// `secs = 0.0` and adds no time, and `f32` accumulation can cost another frame or two — the
    /// clock errs toward silence, which is the correct direction for a false-alarm signal.
    pub(crate) fn matured_stall(pos: [f32; 3]) -> AfloatStall {
        const DT: f32 = 0.016;
        let mut clock = AfloatStallClock::default();
        for _ in 0..((AFLOAT_STALL_SECS / DT).ceil() as usize + 3) {
            clock.observe(AfloatFrame::Wished, pos, DT);
        }
        clock.stall().expect(
            "a body pinned at one point under a sustained wish must stall — if this ever returns \
             None the fixtures below go blind on the afloat axis again, which is #846 review F1")
    }
}
