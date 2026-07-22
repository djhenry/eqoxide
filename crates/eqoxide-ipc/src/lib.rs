//! `eqoxide-ipc` — the "inter-thread contracts" crate: the request-slot types shared between the
//! HTTP API thread, the network (login/gameplay/navigation) thread, and the render/app loop.
//!
//! Extracted as the second member of the Cargo workspace (#544 Step 2c). It sits directly above
//! `eqoxide-core` and below everything else — the layering is `core ← ipc ← {net, render, http,
//! command, …}` — and depends ONLY on `eqoxide-core` plus the low-level channel/serde primitives
//! (`tokio::sync::oneshot`, `arc-swap`, `serde`) its slot types are literally made of. It never
//! reaches up into wgpu/winit/egui/eq_net/renderer/app. The app crate re-exports this crate as its
//! `ipc` module (`pub use eqoxide_ipc as ipc`), so existing `crate::ipc::…` paths across the tree
//! keep resolving unchanged.
//!
//! These are `Arc<Mutex<Option<T>>>`-style shared cells an HTTP handler writes a request into and
//! the network action loop (or, for a few render-owned values, the app loop) drains each tick, plus
//! the matching "published snapshot" direction (`Arc<Mutex<T>>` / `Arc<ArcSwap<T>>`) the network
//! thread writes and HTTP/render read. They are neither genuine HTTP types (route state, request/
//! response bodies — those stay in the app crate's `http`) nor genuine network-protocol types — this
//! crate is the neutral third party both sides depend on, so the network loop no longer has to reach
//! into `http` for its own inter-thread plumbing.
//!
//! ## Relocated shared type definitions (#544 Step 2c)
//! Several of this crate's slots wrap type *definitions* that used to live in higher app-crate
//! modules, forcing an up-reference out of `ipc`. Those pure-data definitions moved DOWN here (their
//! BEHAVIOR stayed in the app crate, which now `use`s these types — the correct app → ipc direction):
//! - `MoveIntent`, `ControllerView` — from `movement` (the `CharacterController` stepping logic stays).
//! - `CameraMode`, `CameraCmd`, `CameraSnapshot` — from `camera_state` (the `CameraState` update logic stays).
//! - `FrameProfile`, `FrameSample` — from `profiling` (the `Stopwatch` collection helper stays).
//! - `enabled`/`set_enabled` (the profiling on/off toggle) — from `profiling` (#544 Step 2o), so the
//!   new `eqoxide-ui` crate (which reads it once per window to gate a timing log) does not need an
//!   up-reference into the app crate just for a boolean flag. `Stopwatch` stays in `profiling`.
//!
//! Each origin module re-exports its moved types (`pub use eqoxide_ipc::…`) so every existing
//! `crate::movement::MoveIntent` / `crate::camera_state::CameraCmd` / `crate::profiling::FrameProfile`
//! path is unaffected. Serde derives/attrs/field names were preserved verbatim (several are
//! serialized to the HTTP JSON API — the wire form must not change).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// The A3 Command-with-result outcome types (#557 — `ipc` owns them because its own await-slot
/// aliases below reference them; see `result`'s module doc for why they moved down out of
/// `command_state`, and `command_state`'s re-export for why every existing call site is unaffected).
pub mod result;
pub use result::{BuyOk, CastEnd, CommandResult, GiveOk, OpenOk};

// ── Relocated shared type definitions (#544 Step 2c) ─────────────────────────────────────────────
// Pure-data types the slots above/below wrap, moved down out of the app crate so `ipc` no longer
// up-references `movement`/`camera_state`/`profiling`. Definitions only — the behavior that operates
// on them (controller stepping, camera update, frame-profile collection) stays in those app modules,
// which re-export these. Derives/serde attrs/field visibility are byte-identical to the originals.

/// What the driver wants this frame. `wish_dir` is a horizontal direction in server axes
/// (east, north); magnitude is treated as a throttle (clamped to 1). `speed` is run speed (u/s).
///
/// Relocated from `movement` (#544 Step 2c); `movement::CharacterController::step` consumes it.
#[derive(Clone, Copy, Debug, Default)]
pub struct MoveIntent {
    pub wish_dir:    [f32; 2],
    pub wish_vspeed: f32,
    pub jump:        bool,
    pub want_swim:   bool,
    pub speed:       f32,
    /// Max step-up height the controller may climb this move, in EQ units. `0` (default) uses the
    /// native `movement::STEP_UP` (2.0) — correct for free WASD, which must NOT be able to scale
    /// walls. The `/goto` planner raises it so the controller can surmount the small lips
    /// (fences/cart edges) that `find_path` already routed over (its edge-climb cap is the same).
    /// Without this the path leads over a lip the 2u step can't clear and the player wedges (#41).
    pub climb:       f32,
    /// One-shot request to hop a low barrier (fence/cart) this tick. The `/goto` planner sets it once
    /// its own net-progress stall detection fires (the controller can't see net progress — sliding
    /// ALONG a fence looks like good per-frame motion). The controller hops only if it's grounded,
    /// off cooldown, and a near-level landing exists just beyond (`movement::CharacterController::can_hop`).
    /// Free WASD leaves it `false` (a player walking into a wall shouldn't auto-jump). Fixes the Halas
    /// sled-pen (#41).
    pub hop:         bool,
}

/// A read-only snapshot of the controller the render thread publishes each frame for the nav
/// thread to stream to the server (design §2 "Threading"). `heading` is EQ-CCW degrees.
///
/// Relocated from `movement` (#544 Step 2c); the render thread produces it, the nav thread reads it.
#[derive(Clone, Copy, Debug, Default)]
pub struct ControllerView {
    pub pos:     [f32; 3],
    pub heading: f32,
    pub moving:  bool,
    /// False until the render thread has spawned and seeded the controller. The nav streamer must
    /// not mirror/stream a default (origin) position before this is set.
    pub initialized: bool,
    /// One-shot fall height (feet dropped) latched by the render thread the frame the controller
    /// LANDS from an airborne stretch, for the nav thread to apply driver-agnostic fall damage (§442,
    /// #442). `None` except right after a landing; the nav streamer take-and-clears it exactly once.
    /// Respects the init gate — default `None`, only ever set after `initialized`.
    pub landed_fall_height: Option<f32>,
}

/// Which mode the orbit/follow camera is in. Relocated from `camera_state` (#544 Step 2c).
/// Serialized to the `/v1/camera` JSON — `rename_all = "snake_case"` is part of that wire form.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraMode { AutoFollow, ManualOrbit }

/// An HTTP `/camera` command the render loop applies to the `camera_state::CameraState`. Relocated
/// from `camera_state` (#544 Step 2c).
#[derive(Debug, Clone)]
pub enum CameraCmd {
    Set {
        azimuth:   Option<f32>,
        elevation: Option<f32>,
        radius:    Option<f32>,
        focus:     Option<[f32; 3]>,
    },
    Reset,
}

/// Snapshot of the current camera state for the HTTP GET `/camera` response. Relocated from
/// `camera_state` (#544 Step 2c); `camera_state::CameraState::snapshot` produces it. Serde form
/// preserved verbatim (it is the JSON body).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CameraSnapshot {
    pub mode:      CameraMode,
    pub azimuth:   f32,
    pub elevation: f32,
    pub radius:    f32,
    pub focus:     [f32; 3],
}

/// Smoothed per-phase timings (milliseconds) for the HUD overlay. All zero until the first profiled
/// frame. Each field is an exponential moving average so the on-screen numbers are readable rather
/// than flickering frame-to-frame.
///
/// Relocated from `profiling` (#544 Step 2c). Serialized to `/v1/observe/debug` (`frame_profile`) —
/// the serde form is part of that wire contract. Its `blend` companion + the `FrameSample` it reads
/// moved with it (an inherent impl must be co-located with its type); the `Stopwatch` collection
/// helper stayed in `profiling`.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct FrameProfile {
    pub update_ms: f32,
    /// Update sub-phase: rebuilding `SceneState` from `GameState` (per-frame snapshot clone).
    pub scene_ms:  f32,
    /// Update sub-phase: per-entity motion smoothing + floor snap.
    pub smooth_ms: f32,
    pub render_ms: f32,
    pub egui_ms:   f32,
    pub submit_ms: f32,
    pub total_ms:  f32,
    /// Instantaneous frames-per-second derived from `total` + idle wait (wall-clock between frames).
    pub frame_ms:  f32,
}

impl FrameProfile {
    /// Blend a fresh per-frame sample into the running average.
    pub fn blend(&mut self, s: &FrameSample, frame_ms: f32) {
        const A: f32 = 0.12; // EMA weight — ~0.5s settling at 60fps
        self.update_ms += (s.update_ms() - self.update_ms) * A;
        self.scene_ms  += (s.scene_ms()  - self.scene_ms)  * A;
        self.smooth_ms += (s.smooth_ms() - self.smooth_ms) * A;
        self.render_ms += (s.render_ms() - self.render_ms) * A;
        self.egui_ms   += (s.egui_ms()   - self.egui_ms)   * A;
        self.submit_ms += (s.submit_ms() - self.submit_ms) * A;
        self.total_ms  += (s.total_ms()  - self.total_ms)  * A;
        self.frame_ms  += (frame_ms      - self.frame_ms)  * A;
    }
}

/// Raw per-phase durations captured during one `render_frame`. Built only when profiling is enabled.
/// Relocated from `profiling` (#544 Step 2c) alongside `FrameProfile::blend`, which consumes it.
#[derive(Default)]
pub struct FrameSample {
    pub update: std::time::Duration,
    /// Sub-span of `update`: `SceneState::from_game_state`.
    pub scene:  std::time::Duration,
    /// Sub-span of `update`: entity motion smoothing + floor snap.
    pub smooth: std::time::Duration,
    pub render: std::time::Duration,
    pub egui:   std::time::Duration,
    pub submit: std::time::Duration,
    pub total:  std::time::Duration,
}

impl FrameSample {
    pub fn update_ms(&self) -> f32 { self.update.as_secs_f32() * 1000.0 }
    pub fn scene_ms(&self)  -> f32 { self.scene.as_secs_f32()  * 1000.0 }
    pub fn smooth_ms(&self) -> f32 { self.smooth.as_secs_f32() * 1000.0 }
    pub fn render_ms(&self) -> f32 { self.render.as_secs_f32() * 1000.0 }
    pub fn egui_ms(&self)   -> f32 { self.egui.as_secs_f32()   * 1000.0 }
    pub fn submit_ms(&self) -> f32 { self.submit.as_secs_f32() * 1000.0 }
    pub fn total_ms(&self)  -> f32 { self.total.as_secs_f32()  * 1000.0 }
}

/// The `--profile` / `EQ_PROFILE=1` on/off flag. Relocated from `profiling` (#544 Step 2o) — a
/// process-wide toggle read by both the app crate (`app::render_frame`'s phase timers) and
/// `eqoxide-ui` (gating its per-window timing log), so it lives beside the `FrameProfile`/
/// `FrameSample` data it gates rather than forcing either reader to depend on the other.
static PROFILING_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turn frame profiling on/off (set once at startup from the `--profile` flag / `EQ_PROFILE` env).
pub fn set_enabled(on: bool) {
    PROFILING_ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the `--profile` overlay/timing is active.
#[inline]
pub fn enabled() -> bool {
    PROFILING_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}
// ── end relocated definitions ────────────────────────────────────────────────────────────────────

/// A pending frame capture: the render loop drains this, captures a PNG,
/// and sends the bytes back through the channel.
pub type FrameReq = Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>;

/// A pending `/who all` request: GET /v1/observe/who registers a oneshot sender here; the nav thread
/// drains it, sends OP_WhoAllRequest, and fires it with the parsed roster when OP_WhoAllResponse
/// arrives. (#300)
pub type WhoReq = Arc<Mutex<Option<oneshot::Sender<Vec<eqoxide_core::game_state::WhoEntry>>>>>;

/// The client-local friends list (names). Edited by POST /v1/social/friends {add|remove}; read by the
/// nav thread to build the OP_FriendsWho poll and by GET /v1/social/friends to annotate online. (#301)
pub type FriendsListShared = Arc<Mutex<Vec<String>>>;
/// A pending friends-presence poll: GET /v1/social/friends registers a oneshot here; the nav thread
/// drains it, sends OP_FriendsWho, and fires it with the online-friends roster (the OP_WhoAllResponse
/// the server sends back) — mirrors [`WhoReq`]. (#301)
pub type FriendsReq = Arc<Mutex<Option<oneshot::Sender<Vec<eqoxide_core::game_state::WhoEntry>>>>>;

/// Target position for the navigation system. Set by /goto, cleared on arrival.
pub type GotoTarget = Arc<Mutex<Option<(f32, f32, f32)>>>;

/// When /goto targets a named ENTITY, this holds its `entity_positions` key so the nav walker can
/// re-resolve the entity's CURRENT position each tick and CHASE it — roaming mobs move (and their
/// client position is stale until they come within the server's update range), so pathing to a
/// one-time snapshot lands nowhere near them (eqoxide#88). `None` for coordinate gotos. Cleared
/// on arrival/stop alongside `goto_target`.
pub type GotoEntity = Arc<Mutex<Option<String>>>;

/// Authoritative controller snapshot published by the render thread each frame and read by the nav
/// thread to stream OP_ClientUpdate (design §2). Single source of position truth.
pub type ControllerShared = Arc<Mutex<ControllerView>>;

/// The `/goto` planner's per-frame movement intent. The nav planner writes `Some` while walking a
/// path and `None` when idle/arrived; the render controller consumes it when no WASD key is held.
pub type NavIntent = Arc<Mutex<Option<MoveIntent>>>;

/// A large (>12u) server position correction the nav thread hands to the render controller to apply
/// (teleport). Small deltas are ignored — the controller is authoritative (design §3.4).
pub type PosCorrection = Arc<Mutex<Option<[f32; 3]>>>;

/// Single-owner GameState publication (see
/// docs/superpowers/plans/2026-07-12-gamestate-single-owner-snapshot.md). The network thread is
/// the sole writer of `GameState`; it publishes an immutable clone here after every gameplay tick
/// via `eq_net::gameplay::publish_snapshot`. Render/HTTP consumers read it lock-free via `.load()`
/// (borrowed) or `.load_full()` (owned `Arc<GameState>`).
pub type GameStateSnapshot = std::sync::Arc<arc_swap::ArcSwap<eqoxide_core::game_state::GameState>>;

/// The three clocks that answer "can I trust anything else in this payload?", owned and stamped by
/// the network thread and turned into `Health` **at HTTP read time** (`HttpState::health`), never
/// cached (#8, #343). They are deliberately separate signals, because they fail independently:
///
/// | clock           | bumped when                        | a stale value means                      |
/// |-----------------|------------------------------------|------------------------------------------|
/// | `last_datagram` | ANY inbound UDP datagram           | **the link is dead** → `connected: false` |
/// | `last_packet`   | an inbound APPLICATION packet       | the world is quiet (NOT necessarily dead) |
/// | `last_tick`     | every gameplay tick (~10ms)         | OUR network thread wedged/died            |
///
/// The `last_datagram` / `last_packet` split is load-bearing and was found by live-testing #343: a
/// genuinely idle EQ session (a character sitting alone in an empty zone) goes **40+ seconds**
/// without a single application packet, while the session layer keeps ACKing throughout. Deriving
/// `connected` from application traffic would therefore report a perfectly healthy idle session as
/// disconnected — trading the old false `true` for an equally dishonest false `false`.
///
/// #371 adds a FOURTH failure those three cannot fully see: a zone that is **still ticking but not
/// making application progress for us** — a stuck per-client dispatch, an infinite/blocking quest
/// script, or a tick so slow it never services our packets. Such a zone keeps ACKing
/// (`last_datagram` fresh → `connected: true`) while producing no application output for us
/// (`last_packet` climbing) — which is *pixel-identical* to a healthy-but-idle zone, because the
/// symptom is exactly "the world stopped speaking". No passive clock can separate them. The only
/// sound discriminator is an ACTIVE probe: periodically send a cheap request the zone MAIN LOOP
/// must service to answer, and time the reply. `last_probe_sent` / `last_probe_reply` are that
/// round-trip's clocks; `HttpState::health` turns them into `world_responsive` at read time.
///
/// SCOPE (do not oversell): this EQEmu build runs the zone as a single-threaded libuv loop, so a
/// *total* freeze stops the ACKs too and is ALREADY caught by `connected: false`. `world_responsive`
/// does NOT add total-freeze detection — it adds the still-ticking-but-unresponsive case above,
/// which `connected` cannot see. (The old Titanium `EQStreamFactory` split a hung main loop from a
/// still-ACKing reader thread; this server does not work that way — do not reason from that model.)
#[derive(Debug, Clone, Copy)]
pub struct NetHealth {
    /// Last inbound datagram of ANY kind, session-layer ACKs/keepalives included → link liveness.
    pub last_datagram: std::time::Instant,
    /// Last inbound APPLICATION packet (a decoded opcode that mutated `GameState`) → world activity.
    /// NOTE: the #371 liveness-probe reply is deliberately NOT stamped here — it is a solicited poke,
    /// not spontaneous world output, and counting it would cap `last_packet_age_ms` at the probe
    /// cadence and destroy its "the world has been quiet for 45s" meaning. It stamps `last_probe_reply`.
    pub last_packet:   std::time::Instant,
    /// Last network-thread gameplay tick → client liveness (is our own publisher still running?).
    pub last_tick:     std::time::Instant,
    /// When the network thread MOST RECENTLY (re)sent an active liveness probe (#371). Bumped on
    /// every 30s resend while a probe stays unanswered — this is a scheduling clock only. Do NOT feed
    /// this into `world_responsive`'s timeout check: resending an already-unanswered probe must not
    /// look like a *fresh* one, or a permanently wedged zone would flicker back to "responsive" every
    /// time the resend fires (the exact bug this comment is warning against — see
    /// `first_unanswered_probe_sent` below, which is what `world_responsive` actually reads).
    /// `None` until the first probe fires (e.g. before we are fully in-zone) — in which case there is
    /// simply no probe verdict yet and `world_responsive` defers to the passive signals.
    pub last_probe_sent:  Option<std::time::Instant>,
    /// When we last saw the probe's reply come back from the zone (#371). Compared against
    /// `first_unanswered_probe_sent` to tell an answered probe from an outstanding one.
    pub last_probe_reply: Option<std::time::Instant>,
    /// When the CURRENT unanswered-probe streak began (#371 wedge-flicker fix). Set the first time a
    /// probe is sent while none is already outstanding; deliberately left UNCHANGED by later resends
    /// of that same still-unanswered probe, so a zone that never answers cannot "earn" a fresh 10s
    /// in-flight grace window every time we poke it again. Reset to `None` the moment ANY proof of
    /// life arrives — a genuine probe reply (`record_probe_reply`) OR any spontaneous application
    /// packet (`record_app_packet`) — and on zone-change (`reset_probe_clocks`). Clearing on
    /// spontaneous traffic is load-bearing: it re-arms the clock so a SECOND wedge after a traffic
    /// recovery is timed freshly and still detected (without it, a stale streak-start would make the
    /// answered-clause permanently true → a confident false-alive). This — not `last_probe_sent` — is
    /// what `world_responsive` measures its timeout against, so once a wedge verdict is reached within
    /// one continuous silence it stays `false` until real proof of life, no matter how many resends
    /// happen in between.
    pub first_unanswered_probe_sent: Option<std::time::Instant>,

    // ── Outbound send failures (#612) ──────────────────────────────────────────────────────────
    //
    // Every clock above is about what the SERVER did. These four are about what WE failed to do:
    // a datagram the client built but that never left the machine, because `try_send` returned an
    // error (`WouldBlock`, `ENOBUFS`, `EMSGSIZE`, `ENETUNREACH`, a dead socket…). Before #612 that
    // error was discarded (`let _ = self.socket.try_send(&raw)`), so a packet that never reached
    // the wire was indistinguishable from one that did — the agent-honesty failure the invariant
    // exists to prevent, one layer below #513/#347. `EqStream::transmit` is now the ONLY place in
    // the client that touches the socket's send path, and it stamps these on every failure, so a
    // send cannot fail without being counted.
    /// Cumulative count of outbound datagrams whose `try_send` failed — i.e. that were BUILT but
    /// never put on the wire. Since process start; never reset (a zone change does not un-drop a
    /// packet).
    ///
    /// **`0` IS the expected healthy reading since #641.** History, because the previous text here
    /// said the opposite and an agent reading it would have learned to ignore this counter: the
    /// #612 round-2 review measured **283** on a fresh, healthy login into `qeynos` — all
    /// `WouldBlock`, all 7-byte session-layer control datagrams (ACKs), in a burst during zone-in
    /// and then flat. #641 gave those two recovery paths (an immediate direct `send(2)` retry, and
    /// a deferral queue for control datagrams), and both are counted elsewhere —
    /// `send_wouldblock_rescued` and `send_deferred`. So this counter now means what its name says:
    /// the datagram never reached the wire, and nothing will re-send it.
    ///
    /// The TRIGGER is established — CPU starvation of the client's tokio io driver, reproducible by
    /// pinning the client to one core. The MECHANISM is not: see `send_wouldblock_rescued` for why
    /// neither counter can tell a tokio-synthetic refusal from a kernel one.
    pub send_failures: u64,
    /// Datagrams whose `try_send` returned `WouldBlock` and which an immediate direct `send(2)` on
    /// the same fd then ACCEPTED (#641). They reached the wire, which is why they are counted here
    /// and not in `send_failures`.
    ///
    /// **This is an UPPER BOUND on tokio's synthetic-`WouldBlock` case, not a measurement of it**
    /// (#641 review, finding 3). Two mechanisms produce the same `WouldBlock`:
    ///   1. tokio short-circuits on an empty cached readiness bit and returns `WouldBlock` *without
    ///      issuing the syscall* (the bit is refilled only by its io driver); or
    ///   2. the bit is set, the syscall IS issued, and the kernel returns `EAGAIN`/`ENOBUFS` (which
    ///      also clears the bit).
    /// A direct `send(2)` succeeding microseconds later fits (1) — but fits (2)-then-the-buffer-
    /// drained just as well, and a burst is exactly when the buffer is full and draining hard. So
    /// the error is systematic in one direction. A DOUBLE refusal (the direct `send(2)` fails too)
    /// is hard evidence of (2); that is what refutes "it is all synthetic", and it is all that is
    /// established. Telling them apart properly would need something like `ioctl(SIOCOUTQ)` at the
    /// moment of refusal (≈0 queued bytes ⇒ genuinely synthetic); nobody has done that.
    ///
    /// Read it as a LOAD signal — the socket is refusing sends here — not as a diagnosis. The split
    /// varies RUN TO RUN, not by zone: `gfaydark` measured 0 rescued / 138 deferred on one run and
    /// 175 / 147 on another, same recipe and same binary; `qeynos` measured 141/107, 166/106 and
    /// 119/114. Nothing observable predicts it.
    ///
    /// Before #641 every one of these was a datagram silently dropped on the floor — mostly ACKs,
    /// which the server then had to re-solicit by retransmitting the packets it had not seen
    /// acknowledged.
    pub send_wouldblock_rescued: u64,
    /// How many **datagrams** a transient send refusal (`EAGAIN`/`ENOBUFS`) caused to be QUEUED for
    /// retry on a later net-thread tick instead of being dropped (#641). Only session-layer control
    /// is deferrable — ACK / OutOfOrderAck / keepalive / SessionRequest. (`SessionDisconnect` is
    /// deliberately NOT: there is no "next tick" at shutdown. See `send_session_disconnect`.)
    ///
    /// **Datagrams, not refusal events.** Counted exactly once, in `defer_control`, at the moment
    /// the datagram is queued. It is *not* incremented again when a queued datagram is re-attempted
    /// and refused again, so the number tracks how many datagrams were delayed, not how long the
    /// outage lasted. The first cut of #641 got this wrong in the other direction and its docs and
    /// its code disagreed (#641 review, finding 1).
    ///
    /// **Not a loss counter, and NOT disjoint from `send_failures`** (#641 review, finding 1b). In
    /// the normal case each of these datagrams goes out on a later tick, ~10ms late. But a deferred
    /// datagram can still be lost afterwards — the queue overflows, or the session ends while it is
    /// still queued — and that loss is counted in `send_failures`/`send_failures_unretried` too, so
    /// the same datagram appears in both. `send_failures` stays the honest "was anything lost?"
    /// number; this one answers "how many datagrams did the socket make us delay?".
    ///
    /// That holds on EVERY path that ends a session, including the `OP_GMKick` one that parks
    /// forever without ever unwinding: it calls `abandon_outstanding` explicitly (#641 review R3),
    /// because a `Drop` that never runs cannot account for anything. A counter that is honest
    /// "except on one path" decays into a counter nobody trusts.
    ///
    /// A lower bound on genuine kernel refusals, for the reason given on `send_wouldblock_rescued`.
    /// Before #641 every one of these was a silently dropped ACK, which the server answered by
    /// retransmitting everything it had not seen acknowledged — the road to a `resend_timeout` drop.
    pub send_deferred: u64,
    /// The subset of `send_failures` for datagrams the client does **not** retransmit itself:
    /// unreliable app packets (the `OP_ClientUpdate` position firehose), session-layer control
    /// (ACK / OutOfOrderAck / keepalive / SessionRequest / SessionDisconnect). The complement
    /// (`send_failures - send_failures_unretried`) is the reliable stream, where the failed
    /// datagram is retained verbatim in the resend window and re-sent by `poll_resend` until the
    /// server ACKs it — **for as long as the session lives**.
    ///
    /// That qualifier is load-bearing (#612 review F1) and this counter must NOT be read as a
    /// complete count of lost payload: when a session ends while reliables are still outstanding,
    /// the next stream's window starts EMPTY and those datagrams are genuinely lost while this
    /// counter reads 0 for all of them.
    ///
    /// **Two different endings, and only one of them is counted anywhere:**
    ///   - A zone handoff / world reconnect / clean shutdown — counted by `reliable_abandoned`.
    ///   - **A server-side ~30s `resend_timeout` drop — counted by NOTHING.** The client never
    ///     notices such a drop today (#642), so the stream is never torn down and
    ///     `reliable_abandoned` does not rise either. `connected: false` (15s of link silence,
    ///     which precedes the server's 30s drop) is the ONLY honest signal for it.
    ///
    /// This paragraph has now regenerated the wrong way four times across #612's reviews — most
    /// recently right here, under a field whose name does not contain "abandoned", which is exactly
    /// why greps keyed on `reliable_abandoned` kept missing it. `docs/http-api.md` and
    /// `eqoxide_http::Health` both point readers HERE for the coverage list, so if this doc is wrong
    /// the whole chain is. If you edit it, grep `resend_timeout` across the workspace, not this
    /// field's neighbourhood.
    ///
    /// Do NOT read a nonzero value here as "a command was lost": several of these datagrams have a
    /// recovery path one level up (a fresh position update follows ~50ms later; a lost ACK is
    /// re-solicited by the server's own resend). It means "this exact datagram is gone, and the
    /// client will not re-send THAT datagram" — which is a real, previously invisible fact.
    pub send_failures_unretried: u64,
    /// `ErrorKind` of the most recent send failure (`None` if there has never been one). Kept as an
    /// `ErrorKind` rather than a `String` so `NetHealth` stays `Copy` (it is read by value under the
    /// mutex, like every other field here).
    pub last_send_error_kind: Option<std::io::ErrorKind>,
    /// When the most recent send failure happened. Measured into an age at HTTP READ time, never
    /// stored as a duration — same rule as every other clock in this struct (#343).
    pub last_send_error_at: Option<std::time::Instant>,
    /// Un-ACKed RELIABLE datagrams that were abandoned when a session ended (#612, review F1).
    ///
    /// `send_failures_unretried` deliberately excludes the reliable stream, because `poll_resend`
    /// re-sends a failed reliable datagram verbatim until the server ACKs it. That guarantee holds
    /// only **while the session lives**. EQEmu drops the session at its ~30s `resend_timeout`, and
    /// the reconnect builds a FRESH `EqStream` whose resend window starts EMPTY — every datagram
    /// still outstanding at that moment is genuinely lost, and no amount of "it will be
    /// retransmitted" is true of it any more.
    ///
    /// Without this counter that loss would be exactly the bug #612 fixed, one level up: a
    /// documented contract telling the agent a class of loss cannot have happened when it can.
    /// `EqStream`'s `Drop` impl adds its outstanding window here, so every path that TEARS THE
    /// STREAM DOWN is counted without each one remembering to mirror it. See the COVERAGE note
    /// below for the paths that do not tear it down — one of them is not covered at all.
    ///
    /// Note this counts abandonment, not necessarily *loss of an unsent packet*: a datagram that
    /// reached the wire and whose ACK simply had not arrived yet when we handed off is also counted.
    /// It is an upper bound on "reliable payload this client stopped trying to deliver", which is
    /// the honest direction to err in.
    ///
    /// **MEASURED (#612 round 2): three consecutive clean zone handoffs (qeynos → qeytoqrg → qeynos
    /// → freportw) left this at 0, with zero abandonment WARNs** — the resend window was empty at
    /// every handoff. An earlier version of this doc predicted, from reasoning and explicitly
    /// unmeasured, that a clean handoff "routinely leaves a small number"; that was WRONG and would
    /// have trained an agent to ignore the counter's most likely true positive. **Treat a nonzero
    /// value DURING PLAY as signal, not noise.**
    ///
    /// **Clean shutdown is the one measured exception, and it is expected to be nonzero.** Two live
    /// `/v1/lifecycle/exit` runs measured 4 and 8 (#612 round 3/4). It is invisible to an agent
    /// either way — the process is exiting — so scope the "0 is normal" reading to play, not to exit.
    ///
    /// **The CAUSE of that count is NOT established.** What is known structurally: `OP_Logout` is a
    /// single reliable datagram, so it can account for at most 1; and `OP_SessionDisconnect` cannot
    /// contribute at all, because it is framed by `send_raw` (`SendRetry::None`) and the only
    /// `self.sent.push_back` in the client is in `send_tracked`. What is known empirically: the two
    /// runs INVERT the naive prediction — 4 with reliable traffic injected, 8 on a control run with
    /// none. An earlier version of this doc asserted the "closing OP_Logout/SessionDisconnect are
    /// still un-ACKed" mechanism; it was wrong on both counts and is withdrawn. The remaining count
    /// is most likely reliables left over from earlier in the session, but that is a HYPOTHESIS,
    /// not a traced fact — do not repeat it as one.
    ///
    /// **COVERAGE — read this before relying on a 0.** It is written where the abandonment can be
    /// observed, which is not everywhere a session can end:
    ///   - **Covered:** zone handoff and world reconnect (both `drop` the old stream), zone-in
    ///     failure returns, and clean shutdown (which calls `abandon_outstanding` explicitly,
    ///     because its task parks and is never unwound).
    ///   - **NOT covered: a server-side session drop** — the ~30s `resend_timeout` case. The client
    ///     currently never notices one: inbound `OP_SessionDisconnect` is unhandled, `poll_recv`'s
    ///     "socket closed" return is discarded at every call site, and the gameplay loop has no
    ///     link-staleness exit, so the stream is never dropped and this stays 0 for exactly those
    ///     datagrams. Detecting a server-side drop is #642, deliberately out of scope for #612.
    ///     Until then, `connected: false` (15s of link silence, which precedes the server's 30s
    ///     drop) is the signal for that case — not this counter.
    pub reliable_abandoned: u64,
}

impl Default for NetHealth {
    fn default() -> Self {
        let now = std::time::Instant::now();
        NetHealth {
            last_datagram: now, last_packet: now, last_tick: now,
            last_probe_sent: None, last_probe_reply: None,
            first_unanswered_probe_sent: None,
            send_failures: 0, send_wouldblock_rescued: 0, send_deferred: 0,
            send_failures_unretried: 0,
            last_send_error_kind: None, last_send_error_at: None,
            reliable_abandoned: 0,
        }
    }
}

/// #371: a probe left unanswered longer than this — while no spontaneous application packet has
/// arrived either — means the zone main loop is not processing (a wedged world), even though the
/// link keeps ACKing. Kept below `PROBE_INTERVAL` so a wedge is declared before the next probe is
/// even due; kept well above a normal round-trip so ordinary latency never false-alarms.
pub const PROBE_TIMEOUT_SECS: u64 = 10;

/// #371 resend cadence for an unanswered liveness probe — and, crucially for #470, the interval
/// before the NEXT probe is due AFTER one is answered. `gameplay.rs`'s `PROBE_INTERVAL` is built from
/// this (single source of truth), and `PASSIVE_LIVENESS_STALE_SECS` is derived from it. See the note
/// on the passive bound below for why an ANSWERED probe re-enters the passive branch for a full
/// interval, which is what makes this value — not the first-probe timing — the one that matters.
pub const PROBE_INTERVAL_SECS: u64 = 30;

/// #470: passive-liveness staleness bound for the "no probe outstanding" branch of
/// [`world_responsive`]. It exists to condemn a ZOMBIE session whose active prober is DEAD, WITHOUT
/// ever false-condemning a healthy idle-but-answering session (#343).
///
/// The prober runs inside the gameplay net thread's loop (`gameplay.rs`). A failed world-reconnect
/// can leave that thread exited: no more probes are ever sent, so `first_unanswered_probe_sent`
/// stays `None` forever and the active-probe path can NEVER declare a wedge. Pre-#470 the `None`
/// branch returned `true` unconditionally, so a fully dead session reported `world_responsive: true`
/// indefinitely — the exact agent-honesty lie #470 is about. This bound lets the passive proof-of-life
/// clock ALONE condemn such a session even with no probe outstanding.
///
/// DERIVED FROM THE RESEND CADENCE, not the first-probe timing (the bug the first cut of #470 had).
/// A HEALTHY idle-but-answering session spends most of its life in the `None` branch: the instant a
/// probe is answered, `record_probe_reply` clears the unanswered streak back to `None`, and the NEXT
/// probe is not sent until `PROBE_INTERVAL_SECS` (30s) after the previous SEND. So its freshest
/// proof-of-life (`probe_reply_ago`) climbs to nearly a FULL interval before the next probe refreshes
/// it. The bound must therefore exceed one whole probe cycle plus its reply window, or a perfectly
/// healthy every-probe-answering session would be condemned for the tail of every cycle, forever
/// (`PROBE_INTERVAL` 30s + `PROBE_TIMEOUT` 10s = 40s; the earlier `PROBE_QUIET + PROBE_TIMEOUT` = 22s
/// was < 30 and did exactly that). At the same time a genuinely dead-but-connected session is still
/// condemned: a live prober would have re-probed at 30s and, getting no reply, moved to the Some/
/// timeout branch by ~40s anyway — so nothing alive is still sitting in the `None` branch past 40s.
pub const PASSIVE_LIVENESS_STALE_SECS: u64 = PROBE_INTERVAL_SECS + PROBE_TIMEOUT_SECS;

/// #371/#470: decide, at HTTP read time, whether the WORLD (not just the link) is alive, from the
/// link/probe/app clocks expressed as ages (time since the event; `None` = it never happened). Pure
/// so the state machine can be exhaustively unit-tested without a socket. Returns `world_responsive`.
///
/// **#470 link gate (checked first):** a dead LINK cannot host a responsive world, regardless of any
/// probe verdict. `connected == false` → `false`. This is the branch that actually bites the zombie
/// bug: a failed world-reconnect kills the net thread (and with it the prober), so no probe is ever
/// outstanding; the pre-#470 code then fell straight through to the unconditional `true` below. The
/// caller MUST pass the SAME `connected` it publishes in `Health` (derived from `last_datagram`).
///
/// With the link alive, a probe is only damning once it is BOTH unanswered AND overdue:
/// - **No probe outstanding** → defer to the passive `last_packet` clock. `true` while packets are
///   fresher than `passive_stale`; `false` once staler (#470 — a live prober would have fired and
///   moved us to the `Some` branch by then, so this staleness means the prober itself is gone). A
///   genuinely idle-but-answering session (#343) is in the `Some` branch by now and never reaches
///   here — see [`PASSIVE_LIVENESS_STALE_SECS`].
/// - **Answered** → `true`. "Answered" = proof the zone processed something at or after we sent the
///   probe: its own reply (`probe_reply_ago <= first_unanswered_sent_ago`) OR *any* spontaneous
///   application packet since (`last_packet_ago <= first_unanswered_sent_ago`). The second clause is
///   belt-and-suspenders: a busy zone is obviously alive even if a single probe reply was dropped,
///   and it is exactly what keeps a legitimately-quiet-but-answering idle session from ever
///   false-alarming.
/// - **Outstanding but not yet overdue** (`first_unanswered_sent_ago < timeout`) → `true`. Still in
///   flight; never mistake normal latency for a wedge.
/// - **Outstanding AND overdue** → `false`. The wedged-world signal — the whole point of #371.
///
/// CALLER CONTRACT (#371 wedge-flicker fix): `first_unanswered_sent_ago` MUST be the age of the
/// FIRST send of the current unanswered probe streak, not the most recent resend. `gameplay.rs`
/// resends an unanswered probe every `PROBE_INTERVAL` (30s) purely to keep detecting recovery; if
/// this function were fed the age of that most-recent resend instead, a permanently wedged zone
/// would re-enter the "still in flight" branch every 30s and flicker back to `true` forever even
/// though it never actually answers. `NetHealth::first_unanswered_probe_sent` is the clock that
/// holds still across resends and only clears on a genuine reply or a zone change — feed that one.
pub fn world_responsive(
    connected:                 bool,
    first_unanswered_sent_ago: Option<std::time::Duration>,
    probe_reply_ago:           Option<std::time::Duration>,
    last_packet_ago:           std::time::Duration,
    timeout:                   std::time::Duration,
    passive_stale:             std::time::Duration,
) -> bool {
    // #470 link gate: a dead link is a dead world, no matter what the probe clocks say (and in the
    // zombie case they say nothing — the prober died, leaving `first_unanswered_sent_ago == None`).
    if !connected {
        return false;
    }
    match first_unanswered_sent_ago {
        // No unanswered-probe streak, link alive. This state has TWO causes and they must be told
        // apart: (a) a probe was just ANSWERED — `record_probe_reply` clears the streak, leaving a
        // FRESH `probe_reply_ago` — a legitimately idle-but-answering session (#343) whose last
        // spontaneous packet may be tens of seconds stale yet is provably alive; or (b) the prober is
        // DEAD (#470) — no probe ever replied and no packet has arrived for the whole window. So
        // condemn only when the FRESHEST proof of life (spontaneous packet OR probe reply, exactly the
        // pair `last_world_response_ms` reports) is itself staler than the symmetric bound.
        None => {
            let proof_of_life_ago = probe_reply_ago.map_or(last_packet_ago, |r| r.min(last_packet_ago));
            proof_of_life_ago < passive_stale
        }
        Some(sent_ago) => {
            let answered = probe_reply_ago.is_some_and(|r| r <= sent_ago)
                        || last_packet_ago <= sent_ago;
            answered || sent_ago < timeout
        }
    }
}

pub type NetHealthShared = std::sync::Arc<std::sync::Mutex<NetHealth>>;

/// Smoothed per-frame phase timings, published by the **render** thread (the only agent-visible
/// value the renderer legitimately owns — see `PlayerState`'s note on the network/render split).
pub type FrameProfileShared = std::sync::Arc<std::sync::Mutex<FrameProfile>>;

/// Aggro-avoidance knobs the `/v1/move/*` handlers set and the nav walker reads (#242). `enabled`
/// gates the always-on NPC-camp avoidance (#67) — `false` routes straight through (e.g. to reach a
/// mob). `buffer` widens the soft-avoid radius so the route gives hostile pulls more berth. Default =
/// the historical behavior (avoidance on, no extra buffer). A `/goto`/`/zone_cross` request that omits
/// the fields leaves the current setting unchanged.
#[derive(Clone, Copy)]
pub struct AggroAvoidOpts {
    pub enabled: bool,
    pub buffer:  f32,
}
impl Default for AggroAvoidOpts {
    fn default() -> Self { Self { enabled: true, buffer: 0.0 } }
}
pub type NavAvoidShared = Arc<Mutex<AggroAvoidOpts>>;

// (#608: the old `NavPathView` pair — the walker's committed coarse/fine plan for the 2D overlay —
// is GONE. The walker now publishes the full `eqoxide_nav::diagnostics::NavDebugSnapshot` (which
// carries the committed routes, the plan's per-edge trace, pad knowledge and more) through
// `eqoxide_nav::diagnostics::NavDebugView`. That slot cannot live in this crate: it names nav
// types, and `eqoxide-ipc` sits BELOW `eqoxide-nav` in the crate graph — so it is defined in
// `eqoxide-nav` and wired alongside `ControllerSlots` in `main.rs`. One published source; a second
// copy of the committed route here would be a drift channel.)

/// A name-keyed roster map that **cannot be mutated from outside this crate** (#643 review r3).
///
/// It derefs to `HashMap<String, V>`, so every existing read — `get`, `len`, `iter`, `keys`,
/// `contains_key`, `&*guard` into a `&HashMap` — works exactly as before, with no call-site
/// changes. What it deliberately does NOT implement is `DerefMut`, and it exposes no public
/// mutators. The only way to write one is [`WorldSlots::publish_entities`], which writes all three
/// roster maps together.
///
/// # Why a newtype instead of a plain `HashMap`
///
/// `/v1/observe/entities?labeled=1` promises an agent that `poses` is keyed exactly like
/// `entities`, so `body["poses"][name]` cannot `KeyError`. That promise is only as strong as its
/// weakest writer, and this repo has already broken it once: two publishers existed
/// (`eqoxide_net::action_loop::sync_entities` and `eqoxide_net::login`'s zone-in seed), and when
/// `entity_poses` was added only the first was updated.
///
/// Two weaker fixes were tried first and both were falsified by a reviewer:
///
/// 1. *Add the missing lines to the second loop.* The reviewer deleted them again; the whole
///    workspace suite stayed green. The invariant was a convention duplicated across two
///    hand-written loops.
/// 2. *Add a source scanner asserting there is only one publisher.* The reviewer wrote a third
///    publisher in the most idiomatic Rust form — `world.entity_positions.lock().unwrap()
///    .insert(..)`, mutation through a temporary guard with no binding at all — and the suite
///    stayed green, because the scanner keyed on `let mut` on the same line. It was pinned against
///    the bug that had already happened rather than the one that would happen next, which is the
///    same shape as the original defect. (That scanner had a second, independent hole too: its
///    "skip test modules" logic latched at the first `#[cfg(test)]` and never reset, so most of
///    several large production files went unscanned.)
///
/// So the rule moved into the type system, where a grep does not belong: a third publisher is now
/// a **compile error**, not a test failure and not a review catch. This is the same
/// make-the-bad-state-unrepresentable move `Pose`/`Gait` make one layer down.
///
/// # Test seeding
///
/// [`Roster::insert_for_test`] is gated on `#[cfg(any(test, feature = "test-fixtures"))]` so unit
/// tests can still seed a partial or deliberately-mismatched roster (several existing tests rely on
/// that, e.g. an ids-only fixture). Downstream crates enable `eqoxide-ipc/test-fixtures` as a
/// **dev-dependency** feature, so it is absent from `cargo build --release` entirely. It is named
/// `insert_for_test` rather than `insert` on purpose: if that feature ever did get enabled for a
/// normal dependency, a production call site would still read `insert_for_test` and be obvious in
/// review, instead of silently looking like ordinary map access.
///
/// # The exact strength of this guarantee (measured, not asserted)
///
/// - A production publisher written the idiomatic way — `world.entity_positions.lock().unwrap()
///   .insert(..)` — fails to compile under **both** `cargo test --workspace` and
///   `cargo build --release`, because [`Roster::insert`] is `pub(crate)` to this crate. That is the
///   shape a reviewer used to defeat the previous revision's source scanner.
/// - The same code written with `insert_for_test` fails `cargo build --release` (dev-dependency
///   features are absent there) but *does* compile under `cargo test --workspace`, where Cargo
///   unifies the workspace's dev-dependency features. CI runs `cargo build --release --locked`
///   BEFORE the test job, so it is still caught — but it is caught at build time, not by a test.
///   **That containment rests on CI's shape, and nothing durable pins it:** the build and test steps
///   are steps of the SAME job, steps run sequentially, and a failing step fails the job — so today
///   the release build genuinely gates the tests. Split them into separate jobs, reorder them, or
///   add `continue-on-error`, and this hatch silently reopens with no test noticing. If you touch
///   `.github/workflows/test.yml`, keep `cargo build --release` ahead of `cargo test` in one job.
///
/// That residual is deliberate and bounded: closing it entirely would mean giving up in-crate test
/// fixtures that seed partial rosters on purpose. It is recorded here so nobody has to rediscover
/// it, and so "a third publisher cannot compile" is read with its one qualification attached.
/// `Debug`/`PartialEq` only. **Every other derive or impl on this type is deliberately absent** —
/// see "The sealed surface" in the doc comment above.
#[derive(Debug, PartialEq)]
pub struct Roster<V>(HashMap<String, V>);

impl<V> std::ops::Deref for Roster<V> {
    type Target = HashMap<String, V>;
    fn deref(&self) -> &Self::Target { &self.0 }
}

// ── The seal ─────────────────────────────────────────────────────────────────────────────────
// NOT implemented, each one deliberately, each one a way to write a roster from outside this crate:
//
//   DerefMut     — would re-expose every `HashMap` mutator (`insert`, `remove`, `clear`, `entry`,
//                  `get_mut`, …) through the `MutexGuard`.
//   Default      — `*guard = Roster::default()` wipes a map. Replaced by `pub(crate) fn new()`.
//   FromIterator — `*guard = pairs.into_iter().collect()` REPLACES the whole map. This is the one
//                  that defeated the first version of this seal: it blocked per-entry mutation but
//                  left whole-value assignment open, and a complete third publisher written that
//                  way compiled clean in release. (The `DerefMut` in play there is `MutexGuard`'s,
//                  not `Roster`'s, so the missing impl below was simply routed around.)
//   Clone        — `*guard = kept_earlier.clone()` restores a stale roster into one map and not the
//                  others. Found while enumerating this list rather than by a failing build; with
//                  `Clone` gone, `.clone()` on a `Roster` resolves through `Deref` to
//                  `HashMap::clone`, which yields a `HashMap` that cannot be assigned back.
//   serde        — no `Deserialize`, so no deserialize-into-place either.
//
// What that list does and does not establish. It establishes exactly one thing: an outside crate
// cannot CONSTRUCT a `Roster<V>`. So every write that needs a freshly-built value is closed —
// `*guard = pairs.collect()`, `*guard = Default::default()`, `*guard = kept.clone()`,
// `mem::take(&mut *guard)`, `mem::replace(&mut *guard, ..)` — because each of them has to name a
// producer, and there is none.
//
// It does NOT establish that no `Roster` value can be named or moved, and an earlier revision of
// this comment claimed that it did. That was false. `WorldSlots`' fields are `pub`, `publish_entities`
// is `pub`, and `MutexGuard` supplies `DerefMut` — so an outside crate can legitimately populate a
// SCRATCH `WorldSlots` and then `mem::swap` one of its maps into a live one, MOVING an existing
// `Roster` without ever constructing one. That compiles clean in release today (verified, #665) and
// desyncs `entity_ids` from `entity_positions`, which `combat.rs`'s "is this spawn known?" answers
// from alone. Closing the producer set was necessary and is not sufficient: the remaining leak is
// that the CONTAINER hands out mutable access to what it protects. Tracked in #665; deliberately
// not bolted onto this change.
//
// The reason to close CONSTRUCTORS rather than call-site shapes is that producers are finite and
// enumerable, so "each member is closed" is a claim that can be checked. Two earlier attempts here
// guessed at shapes instead (`let mut` in a source scanner; then per-entry mutation) and each
// survived only the shapes someone happened to try.
// ─────────────────────────────────────────────────────────────────────────────────────────────

impl<V> Roster<V> {
    /// The only constructor. `pub(crate)` — hand-written rather than `Default` so it does not
    /// appear in this type's public API, and so it needs no `V: Default` bound (`EntityPoseView`
    /// has no meaningful default; there is no such thing as a default body pose).
    pub(crate) fn new() -> Self { Roster(HashMap::new()) }

    /// Drop every entry. `pub(crate)` — only the single publisher may write a roster.
    pub(crate) fn clear(&mut self) { self.0.clear(); }
    /// Insert one entry. `pub(crate)` — only the single publisher may write a roster.
    pub(crate) fn insert(&mut self, k: String, v: V) -> Option<V> { self.0.insert(k, v) }

    /// **Test fixtures only.** Seed one entry directly, bypassing the all-three-maps guarantee —
    /// which is exactly what a test wants when it needs a partial or intentionally-mismatched
    /// roster. Never available in a release build; see the type's doc comment.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn insert_for_test(&mut self, k: String, v: V) -> Option<V> { self.0.insert(k, v) }
}

/// Live entity name → (x, y, z) map, published by `WorldSlots::publish_entities`.
pub type EntityPositions = Arc<Mutex<Roster<(f32, f32, f32)>>>;

/// Live entity name → spawn_id map (same keys as EntityPositions).
pub type EntityIds = Arc<Mutex<Roster<u32>>>;

/// One entity's server-published body state, as exposed by `/v1/observe/entities?labeled=1` (#643).
///
/// Both halves are the wire's own signals, kept in their OWN domains — before #643 they shared a
/// single `u32` on `Entity`, so whichever packet arrived last silently decided what the number
/// meant. `pose` is the discrete body state; `gait` is the locomotion speed code.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct EntityPoseView {
    /// `standing` / `freeze` / `looting` / `sitting` / `crouching` / `lying`, or
    /// **`unknown(<raw>)`** when the server sent a code this client does not recognise. An
    /// unrecognised code is reported verbatim rather than guessed at (agent-honesty).
    pub pose: String,
    /// The most recent `OP_ClientUpdate` gait (locomotion speed) code, or `null` when this entity
    /// has not sent a position update yet. `null` means **"not reported"**, NOT "standing still".
    pub gait: Option<i32>,
}

/// Live entity name → pose/gait map (same keys as `EntityPositions`), published each tick by the
/// net thread and read by `GET /v1/observe/entities?labeled=1` (#643).
pub type EntityPoses = Arc<Mutex<Roster<EntityPoseView>>>;

/// Zone exit points received in OP_SEND_ZONE_POINTS, exposed via GET /v1/observe/zone_points.
pub type ZonePoints = Arc<Mutex<Vec<eqoxide_core::game_state::ZonePoint>>>;
/// Native Task-system quest log, published from GameState.tasks each tick (GET /v1/observe/quests/log).
pub type TaskLog = Arc<Mutex<Vec<eqoxide_core::game_state::ActiveTask>>>;

/// Pending offers from an open task-selector window, published each tick (GET /v1/quests/offers).
pub type TaskOffersShared = Arc<Mutex<Vec<eqoxide_core::game_state::TaskOffer>>>;
/// Completed-task history with titles, published each tick (GET /v1/quests/completed).
pub type CompletedTasksShared = Arc<Mutex<Vec<eqoxide_core::game_state::CompletedTaskEntry>>>;
/// Accept/decline a pending task offer, set by POST /v1/quests/accept ({"task_id":N}) or
/// POST /v1/quests/decline (task_id=0). The nav thread reads it once and sends
/// OP_AcceptNewTask (AcceptNewTask_Struct), looking up the offering NPC's id from gs.task_offers.
pub type AcceptTaskReq = Arc<Mutex<Option<u32>>>;
/// Abandon an active task, set by POST /v1/quests/cancel ({"task_id":N}). The nav thread reads it
/// once, looks up the task's sequence_number in gs.tasks, and sends OP_CancelTask
/// (CancelTask_Struct).
pub type CancelTaskReq = Arc<Mutex<Option<u32>>>;

/// Read a book/note item, set by POST /v1/interact/read ({"slot":N}). Carries the inventory wire
/// slot of the item to read. The nav thread takes it, looks up the item's Filename in gs.inventory,
/// and sends OP_ReadBook; the server replies with the text (surfaced via /v1/observe/item_text). (#288)
pub type ReadBookReq = Arc<Mutex<Option<i32>>>;

/// One group member's live view for GET /v1/group/roster (role badges are read-only display
/// flags pushed by the server — not settable via this API in v1).
#[derive(Clone, serde::Serialize)]
pub struct GroupMemberView {
    pub name:     String,
    pub level:    u32,
    pub is_leader: bool,
    pub is_merc:  bool,
    pub tank:     bool,
    pub assist:   bool,
    pub puller:   bool,
    pub offline:  bool,
    pub hp_pct:   f32,
}

/// Published each nav tick from GameState.group_members/group_leader/pending_invite (GET
/// /v1/group/roster, and the UI roster panel). `you_are_leader` is precomputed at publish time
/// (gs.player_name == gs.group_leader) so handlers don't need the player's own name separately.
#[derive(Clone, Default)]
pub struct GroupSnapshot {
    pub members:         Vec<GroupMemberView>,
    pub leader:           String,
    pub pending_invite:   Option<String>,
    pub you_are_leader:   bool,
}
pub type GroupShared = Arc<Mutex<GroupSnapshot>>;

/// Published each nav tick from the player's guild identity + roster: the guild fields of
/// /v1/observe/debug and GET /v1/guild/roster. `guild_id == 0` / empty `guild_name` = not in a
/// guild. Mirrors GroupSnapshot. (#295)
#[derive(Clone, Default)]
pub struct GuildSnapshot {
    pub guild_id:   u32,
    pub guild_name: String,
    pub guild_rank: u32,
    pub members:    Vec<eqoxide_core::game_state::GuildMember>,
    /// Name of whoever has a pending guild invite out to us (for GET /v1/guild/roster), or None.
    pub pending_invite: Option<String>,
}
pub type GuildShared = Arc<Mutex<GuildSnapshot>>;

/// One queued guild action from POST /v1/guild/{invite,accept,leave,remove}, drained by the nav tick
/// which sends the matching RoF2 guild opcode. Bundled into one slot to keep the ActionLoop plumbing
/// small. (#295)
#[derive(Clone, Debug, PartialEq)]
pub enum GuildAction {
    Invite(String),   // POST /v1/guild/invite {"name"} — invite a player to our guild
    Accept,           // POST /v1/guild/accept — accept a pending guild invite
    Leave,            // POST /v1/guild/leave — leave our guild
    Remove(String),   // POST /v1/guild/remove {"name"} — leader/GM removes a member
}
pub type GuildActionReq = Arc<Mutex<Option<GuildAction>>>;

/// POST /v1/group/invite target name. Drained by the nav tick loop, which sends OP_GroupInvite.
pub type GroupInviteReq = Arc<Mutex<Option<String>>>;
/// POST /v1/trainer/open sets this to the trainer NPC's spawn id → nav sends OP_GMTraining (#99).
pub type TrainerOpenReq = Arc<Mutex<Option<u32>>>;
/// POST /v1/trainer/train sets this to a skill id → nav sends OP_GMTrainSkill for the open trainer.
pub type TrainerTrainReq = Arc<Mutex<Option<u32>>>;
/// POST /v1/group/accept trigger — accepts gs.pending_invite. One-shot: `Some(())` then drained.
pub type GroupAcceptReq = Arc<Mutex<Option<()>>>;
/// POST /v1/group/decline trigger — declines gs.pending_invite via a defensive OP_GroupDisband.
pub type GroupDeclineReq = Arc<Mutex<Option<()>>>;
/// POST /v1/group/leave trigger — sends OP_GroupDisband(self, self).
pub type GroupLeaveReq = Arc<Mutex<Option<()>>>;
/// POST /v1/group/kick target member name. Sends OP_GroupDisband(self, target).
pub type GroupKickReq = Arc<Mutex<Option<String>>>;
/// POST /v1/group/makeleader target member name. Sends OP_GroupMakeLeader.
pub type GroupMakeLeaderReq = Arc<Mutex<Option<String>>>;

/// Zone-crossing request set by POST /v1/move/zone_cross; gameplay thread reads it once,
/// teleports to the matching zone line and sends OP_ZONE_CHANGE.
///   Some(0)  → cross the nearest zone line (any destination).
///   Some(id) → cross to a specific destination zone id.
pub type ZoneCrossReq = Arc<Mutex<Option<u16>>>;

/// Manual-movement escape hatch (#188), set by POST /v1/move/manual or /v1/move/jump. The render
/// loop drives the CharacterController with this — exactly like WASD — taking priority over the
/// `/goto` nav planner (but below real keyboard input) until `until`, so an agent can walk/hop out
/// of a spot where A* finds no path. `dir` is a world `(east, north)` direction (zero = stand in
/// place, e.g. a jump with no movement). `up` is the vertical axis for swimming (`-1..1`): while in
/// water it drives the character up/down through the water column (#207); it's ignored on land.
#[derive(Clone, Copy)]
pub struct ManualMove {
    pub dir:   [f32; 2],
    pub up:    f32,
    pub jump:  bool,
    pub until: std::time::Instant,
}
pub type ManualMoveReq = Arc<Mutex<Option<ManualMove>>>;

/// A hail request set by POST /v1/interact/hail: the NPC's display name (for the "Hail, <name>"
/// say text) plus its `spawn_id` when known. The nav thread targets the NPC (`spawn_id`) BEFORE
/// saying, because the server only fires an NPC's `EVENT_SAY` on the player's current target
/// (client.cpp: `Mob* t = GetTarget()`), so a hail without a target is silently ignored (#130).
pub type HailReq = Arc<Mutex<Option<(String, Option<u32>)>>>;

/// Arbitrary Say-channel text, set by POST /v1/interact/say or a HUD button/keyword; the nav thread
/// reads it once and sends it on the Say channel (used for quest keyword follow-ups).
pub type SayReq = Arc<Mutex<Option<String>>>;

/// Spawn id to target, set by POST /v1/combat/target or the HUD "Target nearest" button; the nav
/// thread reads it once, sends OP_TargetCommand + OP_Consider.
pub type TargetReq = Arc<Mutex<Option<u32>>>;

/// Auto-attack toggle — set to true by POST /v1/combat/attack, false by DELETE /v1/combat/attack.
/// Nav thread reads it and sends OP_AUTO_ATTACK(1) or OP_AUTO_ATTACK(0).
pub type AttackReq = Arc<Mutex<Option<bool>>>;

/// Buy request — (merchant spawn id, merchant inventory slot), set by POST /v1/merchant/buy.
/// Nav thread reads it and sends OP_ShopRequest (open) + OP_ShopPlayerBuy (buy that slot).
/// This is the FIRE-AND-FORGET buy the UI merchant-window click uses; the honest awaited variant
/// (POST /v1/merchant/buy over HTTP) rides the sibling [`BuyAwaitReq`] instead. (#448)
pub type BuyReq = Arc<Mutex<Option<(u32, u32)>>>;

/// Command-with-result buy request (A3 Migration 1, #448) — `(merchant spawn id, merchant slot,
/// oneshot Sender)`. POST /v1/merchant/buy writes this and AWAITS the `Sender`; the nav thread
/// drains it, sends the same OP_ShopRequest + OP_ShopPlayerBuy the fire-and-forget [`BuyReq`] path
/// sends, and PARKS the `Sender` in `ActionLoop::pending_buy` until the resolving packet
/// (OP_ShopPlayerBuy echo → `Resolved`, OP_ShopEndConfirm → `Refused`) is applied — or the HTTP
/// timeout / a reaper yields `Unconfirmed`. Sibling of [`BuyReq`], NOT a replacement: the two slots
/// coexist so the UI click path is unchanged. See [`result`] for the flow.
pub type BuyAwaitReq = Arc<Mutex<Option<(u32, u32,
    oneshot::Sender<CommandResult<BuyOk>>)>>>;

/// Sell request — (merchant spawn id, player inventory slot, quantity), set by POST /v1/merchant/sell.
/// Nav thread reads it and sends OP_ShopRequest (open) + OP_ShopPlayerSell (sell that slot).
pub type SellReq = Arc<Mutex<Option<(u32, u32, u32)>>>;

/// Manual pet command — one OP_PetCommands command byte (PET_ATTACK=2, PET_FOLLOWME=4,
/// PET_GUARDHERE=5, PET_SIT=6, PET_BACKOFF=28; EQEmu zone/common.h), set by POST /v1/pet/command
/// or a Pet-window button. The nav thread drains it and sends OP_PetCommands (attack uses the
/// current target as PetCommand_Struct.target; other commands send target 0).
pub type PetCmdReq = Arc<Mutex<Option<u8>>>;

/// Open/close a merchant window. `Open(merchant_id)` from POST /v1/merchant/open; `Close` from
/// POST /v1/merchant/close. The nav thread sends OP_ShopRequest (command 1/0).
/// This is the FIRE-AND-FORGET open/close the UI merchant-window click uses; the honest awaited
/// open (POST /v1/merchant/open over HTTP) rides the sibling [`OpenAwaitReq`] instead. (#479)
#[derive(Clone, Copy)]
pub enum TradeCmd { Open(u32), Close }
pub type TradeReq = Arc<Mutex<Option<TradeCmd>>>;

/// Command-with-result merchant-open request (A3 migration, eqoxide#479) — `(merchant spawn id,
/// oneshot Sender)`. POST /v1/merchant/open writes this and AWAITS the `Sender`; the nav thread's
/// `drain_merchant` drains it, sends the SAME OP_ShopRequest(command=1) the fire-and-forget
/// [`TradeReq`] `Open` path sends, and PARKS the `Sender` in `ActionLoop::pending_open` until the
/// resolving OP_ShopRequest echo lands: `command==1` → `Resolved(OpenOk)` (a real merchant opened
/// the window); `command==0` → `Refused` (a REAL negative ack — RoF2's Handle_OP_ShopRequest
/// collapses faction-KOS/engaged/feigned-invis/charmed/already-busy into this same echo). A target
/// that is not a merchant at all, or out of range, sends NO echo whatsoever (confirmed against the
/// EQEmu RoF2 source — see `~/git/eq_kb/merchant-open-protocol.md`) — that path
/// resolves to `Unconfirmed` via the HTTP timeout / a zone-change reaper, never a fabricated 200.
/// Sibling of [`TradeReq`], NOT a replacement: the UI open/close click path is unchanged. See
/// [`result`] for the flow.
pub type OpenAwaitReq = Arc<Mutex<Option<(u32,
    oneshot::Sender<CommandResult<OpenOk>>)>>>;

/// Camp command, written by POST /v1/lifecycle/exit, POST /v1/lifecycle/camp, the HUD Camp button,
/// and the `/camp` chat keyword. The gameplay loop drains it: `Start` begins a camp if one isn't
/// running (idempotent — used by /exit so a double request doesn't cancel); `Toggle` starts a camp
/// or cancels the one in progress (used by the button / chat command). A completed camp shuts the
/// client down cleanly (no linkdead) once the server's ~29s camp timer has elapsed. See
/// `gameplay::camp_apply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CampCmd { Start, Toggle }
pub type CampReq = Arc<Mutex<Option<CampCmd>>>;
/// Set true by POST /v1/lifecycle/respawn to release a held-dead character back to its bind point
/// (the client no longer auto-respawns — it holds the character slain until asked). (#284)
pub type RespawnReq = Arc<Mutex<bool>>;

/// Published camp state: `Some(deadline)` while a camp is in progress (the instant the client will
/// disconnect), `None` otherwise. Set by the gameplay loop; read by the HUD for the countdown and
/// by handlers to know whether a camp is already running.
pub type CampUntil = Arc<Mutex<Option<std::time::Instant>>>;

/// Live merchant-session snapshot published each nav tick, read by GET /v1/merchant/list and used
/// for the HUD merchant window. `open` mirrors `GameState::merchant_open`.
#[derive(Default, Clone, serde::Serialize)]
pub struct MerchantSnapshot {
    pub open: bool,
    pub merchant_id: Option<u32>,
    pub items: Vec<eqoxide_core::game_state::MerchantItem>,
}
pub type MerchantShared = Arc<Mutex<MerchantSnapshot>>;

/// Move-item request — (from_slot, to_slot), set by POST /v1/inventory/move.
/// Nav thread reads it and sends OP_MoveItem (MoveItem_Struct, number_in_stack=1).
/// Used to equip/unequip/rearrange items (e.g. boots in bag slot 23 -> worn slot 19).
pub type MoveReq = Arc<Mutex<Option<(u32, u32)>>>;

/// Give request — (npc_spawn_id, item_from_slot), set by POST /v1/interact/give.
/// Nav thread runs the trade-window turn-in: puts the item on the cursor, sends OP_TradeRequest,
/// waits for OP_TradeRequestAck, then moves the item into the NPC trade slot + OP_TradeAcceptClick.
/// This is the FIRE-AND-FORGET give the UI turn-in path uses; the honest awaited variant (POST
/// /v1/interact/give over HTTP) rides the sibling [`GiveAwaitReq`] instead. (#448)
pub type GiveReq = Arc<Mutex<Option<(u32, u32)>>>;

/// Command-with-result give request (A3 Migration 2, #448) — `(npc spawn id, item from_slot,
/// oneshot Sender)`. POST /v1/interact/give writes this and AWAITS the `Sender`; the nav thread's
/// `tick_give` state machine drives the SAME trade-window turn-in the fire-and-forget [`GiveReq`]
/// path drives, and PARKS the `Sender` inside its `GiveState` until the resolving packet lands:
/// OP_FinishTrade (the NPC accepted the item) → `Resolved(GiveOk)`; the no-ack / no-finish abort →
/// `Unconfirmed`; a second awaited give while one is in flight → `Refused` (singleton-in-flight).
/// Sibling of [`GiveReq`], NOT a replacement — the two slots coexist so the UI turn-in path is
/// unchanged. See [`result`] for the flow.
pub type GiveAwaitReq = Arc<Mutex<Option<(u32, u32,
    oneshot::Sender<CommandResult<GiveOk>>)>>>;

/// Live snapshot of the player's inventory + equipment, published each tick by the nav thread
/// and read by GET /v1/observe/inventory. Slots are Titanium **wire** ids (the same numbers /give
/// and /inventory/move take — note these are one less than the EQEmu DB `inventory.slot_id` for
/// general slots: DB 23-30 → wire 22-29).
pub type InventoryShared = Arc<Mutex<Vec<eqoxide_core::game_state::InvItem>>>;

/// Loot request — a corpse spawn id, set by POST /v1/interact/loot. The nav thread reads it once and
/// pushes the corpse onto the auto-loot queue (OP_LootRequest → OP_LootItem echoes → OP_EndLootRequest).
pub type LootReq = Arc<Mutex<Option<u32>>>;

/// One machine-readable line from the in-game message log (GET /v1/observe/messages). `kind` is the
/// channel ("npc" = NPC dialogue/emotes, "chat", "combat", "system", "exp", "loot", "trade",
/// "zone", …); `keywords` are the `[bracketed]` quest reply words extracted from the text (say them
/// back via POST /v1/interact/say to advance dialogue quests); `item_links` are any EQ item/say
/// links the text contained — `text` already shows only the clean display name (the raw hex link
/// body is never sent to an agent), and `item_links` gives the resolvable `item_id` behind each one
/// (eqoxide#256). Empty when the line had no links.
#[derive(Clone, serde::Serialize)]
pub struct MessageEntry {
    pub kind:        String,
    pub text:        String,
    pub keywords:    Vec<String>,
    pub item_links:  Vec<eqoxide_core::game_state::ItemLink>,
}

/// Live snapshot of the in-game message log, published each tick by the nav thread and read by
/// GET /v1/observe/messages. Exposes NPC dialogue (kind "npc") as machine-readable text + keywords.
pub type MessagesShared = Arc<Mutex<Vec<MessageEntry>>>;

/// Live snapshot of the current clickable NPC-dialogue choices (saylinks from the most recent NPC
/// message), published each tick by the nav thread and read by GET /v1/observe/dialogue. (#120)
pub type DialogueShared = Arc<Mutex<Vec<eqoxide_core::game_state::DialogueChoice>>>;

/// Live navigation state for the active `/move/goto`, set by the nav thread and read by
/// GET /v1/observe/debug. `state` is the agent-facing contract documented in `docs/http-api.md`:
///
/// `pending` | `idle` | `planning` | `navigating` | `navigating_partial` | `following` | `arrived` |
/// `no_path` | `search_exhausted` | `blocked` | `zone_loading`
///
/// `reason` is the machine-readable WHY behind a terminal state (`goal_not_walkable`,
/// `search_closed`, `search_node_cap`, …). The whole point of the split (#337): a driver must be
/// able to tell "there is no route" (definitive) from "the planner gave up" (I don't know) from
/// "I am wedged" — three answers the old, overloaded `blocked` collapsed into one silent freeze.
#[derive(Clone, Debug, PartialEq)]
pub struct NavStatus {
    pub state:  String,
    pub reason: Option<String>,
    /// GOAL IDENTITY (#349): a monotonically increasing generation stamp, bumped every time a NEW
    /// navigation request (`/move/{goto,follow,zone_cross,stop}`) is accepted. `state` is the status
    /// *of goal `goal_id`* — never of some earlier goal. Without this, a read right after a fresh
    /// `POST /goto` could return the PREVIOUS goto's terminal `arrived`/`no_path`/`blocked` (the
    /// walker only re-labels `state` on its next ~150ms tick), letting an agent conclude the new goto
    /// already finished. Each accept resets `state` to `pending` and bumps this ATOMICALLY (under the
    /// same lock), so goal N's terminal value can never be attributed to goal N+1. `0` = no request
    /// has been issued this session/zone. Surfaced as `nav_goal_id` on GET /v1/observe/debug; echoed
    /// in each accepting POST's response body.
    pub goal_id: u64,
    /// The goal coordinates `[x, y, z]` this `goal_id` is navigating to (server coords), so a caller
    /// can correlate "this state is for the goal I asked for". `None` for `idle`/`stop` (no goal) and
    /// for a `zone_cross` before the walker has resolved the concrete zone-line destination. Surfaced
    /// as `nav_goal` on GET /v1/observe/debug.
    pub goal: Option<[f32; 3]>,
    /// The agent-honesty payload behind a terminal `no_path` (#378 Phase 2): WHAT is blocking the
    /// goal, and WHERE. `blocked_goal` is the definitive "your goal itself cannot be stood at";
    /// `blocked_frontier` is "I got as close as here and this is the obstruction between me and the
    /// goal". Surfaced as `nav_blocked_by.goal` / `nav_blocked_by.frontier` on /v1/observe/debug.
    /// `None` when there is no blockage to report (not a terminal no_path, or the diagnosis could
    /// not be computed) — honest silence, never a fabricated hazard.
    pub blocked_goal: Option<NavBlockage>,
    pub blocked_frontier: Option<NavBlockage>,
    /// Which clearance tier answered the CURRENT route (#378 Phase 2 / design §4c): `preferred`
    /// (roomy) or `minimum` (threaded a tight gap with no margin to spare — a riskier path). `None`
    /// until a route is committed. This is the PER-ROUTE fact the zone-lifetime `nav_tight` counter
    /// could not give (it is `connected:true`'s shape — a field with no per-instance writer, #343).
    pub tier: Option<&'static str>,
    /// The FINE LOCAL (2 u) steering tier's last honest outcome (#382), published as the top-level
    /// `nav_local` on GET /v1/observe/debug. `None` = the tier has not answered for the current route
    /// (idle, or the first fine plan is still in flight).
    ///
    /// It is carried HERE, alongside `state`/`reason`, rather than in a second shared cell, because
    /// the two are read together and must not be able to drift: an agent that sees
    /// `nav_state: navigating` needs to know, in the same snapshot, whether the tier that is actually
    /// steering it can see a way through the next 40 u.
    pub local:  Option<NavLocal>,
}

/// A named obstruction with a position — the agent-facing form of `traversability::Blockage`
/// (#378 Phase 2). `hazard` is `floor` | `wall` | `water`.
#[derive(Clone, Debug, PartialEq)]
pub struct NavBlockage {
    pub hazard: &'static str,
    pub at: [f32; 3],
}

/// What the fine 2 u steering tier last said, verbatim. See `nav::collision::LocalOutcome`.
///
/// **`state` is never `no_path` and structurally cannot be.** The fine search closes only the frontier
/// inside a 40 u window, so it can never prove a goal unreachable; conflating its local dead-end with
/// a definitive "no route" would be #337 with a smaller radius.
#[derive(Clone, Debug, PartialEq)]
pub struct NavLocal {
    /// `threaded` (healthy: a complete fine route to the carrot) | `no_way_through` (the window's
    /// frontier CLOSED — the coarse corridor is not threadable here) | `exhausted` (the search was
    /// cut short: "I don't know") | `planner_dead` (the fine worker died; steering has degraded to
    /// the coarse route only — the walker keeps walking).
    pub state:       String,
    /// `threaded` | `search_closed` | `start_isolated` | `goal_not_walkable` | `no_geometry` |
    /// `search_node_cap` | `local_planner_dead`.
    pub reason:      String,
    /// Consecutive nav ticks the fine tier has failed to thread to its carrot. A nonzero value with
    /// `state: navigating` means the walker is being steered on the coarse route through a stretch the
    /// fine tier says it cannot fit — usually the prelude to a proactive coarse re-plan (#246).
    pub stuck_ticks: u32,
    /// How long the last fine plan took, in microseconds. This is the per-tick cost that used to be
    /// paid **on the network thread** (mean 15.3 ms, worst 358 ms, release/akanon) and is now paid on
    /// the fine worker.
    pub plan_us:     u64,
}

impl Default for NavStatus {
    fn default() -> Self {
        NavStatus { state: "idle".into(), reason: None, local: None,
            blocked_goal: None, blocked_frontier: None, tier: None,
            goal_id: 0, goal: None }
    }
}

impl From<&str> for NavStatus {
    fn from(state: &str) -> Self {
        NavStatus { state: state.to_string(), reason: None, local: None,
            blocked_goal: None, blocked_frontier: None, tier: None,
            goal_id: 0, goal: None }
    }
}

impl PartialEq<&str> for NavStatus {
    fn eq(&self, other: &&str) -> bool { self.state == *other }
}

pub type NavStateShared = Arc<Mutex<NavStatus>>;

/// Pending "click a dialogue choice" request (POST /v1/interact/dialogue or a GUI click): the nav
/// thread drains it and sends an OP_ItemLinkClick for the chosen saylink. (#120)
pub type DialogueClickReq = Arc<Mutex<Option<eqoxide_core::game_state::DialogueChoice>>>;

/// One async game event exposed by the `GET /v1/events/*` feed. `category` is the top-level bucket
/// the events API filters on (chat/combat/navigate/system); `kind` is the sub-type
/// (tell/ooc/shout/group/gmsay/zone/slain/attacked/…). `id` is a 1-based monotonic cursor;
/// `directed` = concerns us specifically (a /tell to our name, a GM message, a zone change, our own
/// death). Agents poll `/v1/events/{all,<category>}?since=<id>` (optionally long-poll with `wait=`).
#[derive(Clone, serde::Serialize)]
pub struct Event {
    pub id:       u64,
    pub category: String,
    pub kind:     String,
    pub from:     String,
    pub directed: bool,
    pub text:     String,
}

/// Live snapshot of async events, published each tick by the nav thread, read by the
/// `GET /v1/events/*` endpoints. Ordered by ascending `id`.
pub type ChatEventsShared = Arc<Mutex<Vec<Event>>>;

/// One queued outgoing chat message, set by POST /v1/chat/{tell,ooc,shout,group} and drained by the
/// nav thread, which builds + sends the `OP_ChannelMessage`. `to` is the recipient for /tell (chan
/// 7), empty for broadcasts. `chan` is the EQ ChatChannel number.
#[derive(Clone)]
pub struct ChatSend {
    pub chan: u32,
    pub to:   String,
    pub text: String,
}

/// Outgoing chat queue (FIFO), written by the /v1/chat/{tell,ooc,shout,group} endpoints.
pub type ChatSendShared = Arc<Mutex<Vec<ChatSend>>>;

#[derive(Clone, Copy)]
pub struct CastRequest {
    pub gem: u8,
    pub target_id: Option<u32>,
    /// When Some, this is an item "clicky" cast: the wire inventory slot of the item to activate.
    /// The gem field is then ignored and the click spell is resolved from the item. (eqoxide#193)
    pub item_slot: Option<u32>,
}
/// Cast a memorized gem (0-8) on an explicit target, else current target, else self.
/// This is the FIRE-AND-FORGET cast the UI spell-gem click uses; the honest awaited variant
/// (POST /v1/combat/cast over HTTP) rides the sibling [`CastAwaitReq`] instead. (#448)
pub type CastReq = Arc<Mutex<Option<CastRequest>>>;

/// Command-with-result cast request (A3 Migration 3, #448) — `(CastRequest, oneshot Sender)`. POST
/// /v1/combat/cast writes this and AWAITS the `Sender`; the nav thread drains it, emits the SAME
/// OP_CastSpell the fire-and-forget [`CastReq`] path sends, and PARKS the `Sender` in
/// `ActionLoop::pending_cast` until the cast's TRUE outcome is known. The cast outcome is already
/// computed by the existing cast machinery into `gs.last_cast` (completed / fizzled / interrupted /
/// failed) — the net thread fulfils by detecting that `last_cast` TRANSITION (NOT a single opcode:
/// the 3-opcode cast-end path is deliberately de-duped, so keying one opcode would double-fire or
/// miss). A cast that never started (empty gem / stale clicky) fires `Refused` immediately from the
/// drain; a truly silent cast resolves to `Unconfirmed` via the HTTP timeout / a zone-change reaper.
/// Sibling of [`CastReq`], NOT a replacement: the UI click path is unchanged. One self-cast at a
/// time → a singleton park suffices. See [`result`] for the flow.
pub type CastAwaitReq = Arc<Mutex<Option<(CastRequest,
    oneshot::Sender<CommandResult<CastEnd>>)>>>;
/// Scribe/memorize request — (slot, spell_id, scribing): scribing 0 = scribe a scroll into the
/// spellbook at book `slot`; 1 = memorize a known spell into gem `slot` (0-8). Set by POST
/// /v1/combat/scribe and POST /v1/combat/memorize; the nav thread sends OP_MemorizeSpell.
/// Tuple = `(slot, spell_id, scribing, from_slot)`. `from_slot` is only used for scribing (0): the
/// RoF2 server scribes only the scroll on the CURSOR, so the nav thread first moves the scroll from
/// `from_slot` → cursor (OP_MoveItem) before the scribe packet. `None` = scroll already on cursor
/// (or memorize/un-mem, which need no move). See eqoxide#11.
pub type MemSpellReq = Arc<Mutex<Option<(u32, u32, u32, Option<u32>)>>>;
/// Posture: Some(true)=sit, Some(false)=stand.
pub type SitReq = Arc<Mutex<Option<bool>>>;
/// Standalone consider of a spawn id.
pub type ConsiderReq = Arc<Mutex<Option<u32>>>;

/// Door-click request — a door_id, set by POST /v1/interact/click_door or a human click in the 3D
/// view. The nav thread reads it once and sends OP_ClickDoor. The door's visual state changes only
/// when the server replies with OP_MoveDoor (server-authoritative).
pub type DoorClickReq = Arc<Mutex<Option<u8>>>;

#[derive(Clone, serde::Serialize)]
pub struct DoorView {
    pub door_id:  u8,
    pub name:     String,
    pub x:        f32,
    pub y:        f32,
    pub z:        f32,
    pub heading:  f32,
    pub opentype: u8,
    pub is_open:  bool,
}
/// Snapshot of the current zone's doors, published each nav tick for GET /v1/observe/doors.
pub type DoorsShared = Arc<Mutex<Vec<DoorView>>>;

/// Current zone name and id, updated on every OP_NEW_ZONE.
#[allow(dead_code)]
pub type ZoneInfo = Arc<Mutex<(String, u16)>>;

// ── Domain slot bundles (M4) ────────────────────────────────────────────────────────────────
//
// Everything above this line is an individual slot alias/type. `ActionLoop` (the network/nav
// thread's per-tick state, `eq_net::action_loop`) and `HttpState` (the HTTP API's per-request
// state, `http::mod`) each used to hold ~50–60 of these as flat, individually-named fields —
// duplicated field lists in two structs, two constructors, and two hand-written test builders,
// with no structure connecting e.g. `attack`/`cast`/`target` as "the combat slots" beyond
// eyeballing the source.
//
// These bundles regroup the same fields BY DOMAIN, one struct per HTTP API group
// (`/v1/combat`, `/v1/merchant`, `/v1/group`, …) — the router nesting in `http::mod::
// spawn_camera_server` is the authoritative domain boundary these mirror, since that's already
// the seam a future shared "controller verb" (one call both a UI click-handler and an agent HTTP
// handler go through, instead of each independently poking a slot) would need to land on. This
// is PURE REGROUPING: every field keeps its original name and `Arc`-sharing semantics unchanged
// — only its home moved from `ActionLoop`/`HttpState` directly to one of these, embedded by
// whichever of the two structs actually reads it. See `ActionLoop::new` and
// `http::mod::spawn_camera_server`/`HttpState` for how a bundle is constructed exactly ONCE and
// then `.clone()`d (a shallow `Arc`-handle clone, not a fresh channel) into each consumer that
// needs it — never `Default`-constructed twice, which would silently sever the channel.
//
// A `TODO(MVC)` marker sits at a handful of representative drain sites in `action_loop.rs` for
// where that future controller-verb unification would land; these bundles are the plumbing for
// it, not the verbs themselves (out of scope here — this is a behavior-preserving refactor).

/// `/v1/combat/*`: targeting, auto-attack, consider, spell scribe/memorize/cast, and the one
/// `/v1/pet/command` slot (small enough on its own that a dedicated `PetSlots` would just be
/// noise — it rides along with the other "act on a target" verbs).
#[derive(Clone, Default)]
pub struct CombatSlots {
    pub attack:   AttackReq,
    pub cast:     CastReq,
    /// The honest awaited-cast slot (A3 Migration 3, #448) — sibling of `cast`. See [`CastAwaitReq`].
    pub cast_await: CastAwaitReq,
    pub mem_spell: MemSpellReq,
    pub consider: ConsiderReq,
    pub target:   TargetReq,
    pub pet_cmd:  PetCmdReq,
}

/// `/v1/merchant/*`: open/close a vendor window, list wares, buy, sell.
#[derive(Clone, Default)]
pub struct MerchantSlots {
    pub merchant: MerchantShared,
    pub buy:      BuyReq,
    /// The honest awaited-buy slot (A3 Migration 1, #448) — sibling of `buy`. See [`BuyAwaitReq`].
    pub buy_await: BuyAwaitReq,
    pub sell:     SellReq,
    pub trade:    TradeReq,
    /// The honest awaited-open slot (eqoxide#479) — sibling of `trade`. See [`OpenAwaitReq`].
    pub open_await: OpenAwaitReq,
}

/// `/v1/inventory/*`: the live snapshot plus the one move/equip/unequip request slot.
#[derive(Clone, Default)]
pub struct InventorySlots {
    pub inventory: InventoryShared,
    pub move_req:  MoveReq,
}

/// `/v1/interact/*`: NPC/world interaction — hail, say, loot, give (turn-in), doors, sit/stand,
/// dialogue clicks, and reading a book/note. Mirrors `http::interact`'s own module doc verbatim
/// ("NPC/world interaction: hail, say, loot, give (turn-in), doors, sit/stand") — that file is
/// the domain boundary this bundle reifies, including `doors_shared` (the read-side twin of
/// `door_click`, published for GET /v1/observe/doors but conceptually the same door verb).
#[derive(Clone, Default)]
pub struct InteractSlots {
    pub hail:           HailReq,
    pub say:            SayReq,
    pub loot:           LootReq,
    pub give:           GiveReq,
    /// The honest awaited-give slot (A3 Migration 2, #448) — sibling of `give`. See [`GiveAwaitReq`].
    pub give_await:     GiveAwaitReq,
    pub door_click:     DoorClickReq,
    pub doors_shared:   DoorsShared,
    pub sit:            SitReq,
    pub dialogue:       DialogueShared,
    pub dialogue_click: DialogueClickReq,
    pub read_book:      ReadBookReq,
}

/// `/v1/quests/*`: the native Task-system log/offers/history plus accept/cancel requests.
#[derive(Clone, Default)]
pub struct QuestSlots {
    pub task_log:               TaskLog,
    pub task_offers_shared:     TaskOffersShared,
    pub completed_tasks_shared: CompletedTasksShared,
    pub accept_task:            AcceptTaskReq,
    pub cancel_task:             CancelTaskReq,
}

/// `/v1/group/*`: roster + invite/accept/decline/leave/kick/transfer-leadership.
#[derive(Clone, Default)]
pub struct GroupSlots {
    pub group:             GroupShared,
    pub group_invite:      GroupInviteReq,
    pub group_accept:      GroupAcceptReq,
    pub group_decline:     GroupDeclineReq,
    pub group_leave:       GroupLeaveReq,
    pub group_kick:        GroupKickReq,
    pub group_make_leader: GroupMakeLeaderReq,
}

/// `/v1/guild/*`: roster + identity snapshot plus the one queued guild action.
#[derive(Clone, Default)]
pub struct GuildSlots {
    pub guild:        GuildShared,
    pub guild_action: GuildActionReq,
}

/// `/v1/trainer/*`: open a trainer window, train a skill.
#[derive(Clone, Default)]
pub struct TrainerSlots {
    pub trainer_open_req:  TrainerOpenReq,
    pub trainer_train_req: TrainerTrainReq,
}

/// `/v1/social/*`: the client-local friends list plus the `/who` and friends-presence polls.
#[derive(Clone, Default)]
pub struct SocialSlots {
    pub who_req:      WhoReq,
    pub friends_list: FriendsListShared,
    pub friends_req:  FriendsReq,
}

/// The outgoing/async text feeds: `/v1/chat/*` (outgoing), `/v1/events/*` (async feed), and the
/// machine-readable NPC/system message log surfaced at `/v1/observe/messages`. Grouped together
/// (rather than splitting `messages` into its own bundle or into `InteractSlots`) because all
/// three are "a queue/log of text the nav thread produces or consumes", read by adjacent handlers
/// in practice (an agent polling `/events` is usually also reading `/observe/messages`).
#[derive(Clone, Default)]
pub struct ChatSlots {
    pub chat_events: ChatEventsShared,
    pub chat_send:   ChatSendShared,
    pub messages:    MessagesShared,
}

/// `/v1/move/*`: the `/goto` target (+ chase-entity), zone-crossing, aggro-avoidance knobs, and live
/// nav status. Does NOT include the manual-move/jump escape hatch (`ManualMoveReq`) — that slot is
/// consumed by the RENDER thread, not the nav thread/`ActionLoop` (see `CameraSlots`), so folding it
/// in here would make `ActionLoop` carry a field it can never read.
///
/// MVC C2 (#452): the walker's draw-only computed path (`nav_path_view`) was moved OUT of here to
/// [`ControllerSlots`] — it is Model→View derived render state, not a view→model command, so it does
/// not belong in a command bundle carried by `command_state::CommandState`.
#[derive(Clone, Default)]
pub struct NavSlots {
    pub goto_target:   GotoTarget,
    pub goto_entity:   GotoEntity,
    pub zone_cross:    ZoneCrossReq,
    pub nav_avoid:     NavAvoidShared,
    pub nav_state:     NavStateShared,
}

/// The live entity registry (`login.rs` writes it as spawn packets arrive): name → position/id,
/// plus the zone's exit points. Read by nearly every domain to resolve a name/target to a spawn
/// id (merchant buy/sell, combat target, trainer open, `/goto` by name, …) — it is genuinely a
/// shared world index, not particular to navigation, even though nav is its biggest reader.
#[derive(Clone)]
pub struct WorldSlots {
    pub entity_positions: EntityPositions,
    pub entity_ids:       EntityIds,
    /// name → pose/gait (#643). Same keys as `entity_positions`; published by the same
    /// `sync_entities` full-replace so it can never go stale independently of the roster.
    pub entity_poses:     EntityPoses,
    pub zone_points:      ZonePoints,
}

// Hand-written rather than `#[derive(Default)]`: `Roster` deliberately has NO public constructor
// (#643 — see its doc comment), so only this crate can build the empty maps. A derive would have
// required `Roster: Default`, which is exactly the public value-producer that let an outside crate
// assign a whole roster through the guard and bypass the single-publisher rule.
impl Default for WorldSlots {
    fn default() -> Self {
        WorldSlots {
            entity_positions: Arc::new(Mutex::new(Roster::new())),
            entity_ids:       Arc::new(Mutex::new(Roster::new())),
            entity_poses:     Arc::new(Mutex::new(Roster::new())),
            zone_points:      Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl WorldSlots {
    /// **The one and only way to publish the entity roster.** Full-replaces `entity_positions`,
    /// `entity_ids` and `entity_poses` from `entities`, holding all three locks for the whole
    /// swap. Returns the number of entities published.
    ///
    /// # Why this exists (#643 review round 2)
    ///
    /// `/v1/observe/entities?labeled=1` promises that `poses` is keyed EXACTLY like `entities`, so
    /// an agent may write `body["poses"][name]` without a `KeyError`. That promise is only as good
    /// as its weakest publisher, and it was already broken once: this crate has **two** roster
    /// publishers — `eqoxide_net::action_loop::sync_entities` (every nav tick) and
    /// `eqoxide_net::login`'s zone-in seed — and when `entity_poses` was added, only the first one
    /// was updated. The seed kept writing positions and ids without poses, so every entity's
    /// `poses` key was missing for the whole window between login and the first nav tick.
    ///
    /// The first fix was to add the missing lines to the second loop. That left the invariant as a
    /// *convention duplicated across two hand-written loops*, which a reviewer falsified by simply
    /// deleting the new lines again: the entire workspace suite stayed green. A third publisher
    /// would reintroduce the bug by omission exactly as the second one did.
    ///
    /// So the invariant moved into a type, next to the fields it constrains: there is now one
    /// function that writes these maps, it cannot write one without the others, and a new publisher
    /// gets the guarantee by construction rather than by remembering. (Same move as `Pose`/`Gait`
    /// in `eqoxide-core`, one level up: make the broken state unrepresentable rather than
    /// documenting a rule and hoping.)
    ///
    /// A source-scanning test was tried here first and a reviewer defeated it in one line — see
    /// [`Roster`], which now makes a second publisher a COMPILE ERROR instead.
    ///
    /// # ⚠️ Lock order
    ///
    /// `entity_positions` → `entity_ids` → `entity_poses`. This is the canonical order every other
    /// site must follow (see `eqoxide_http::name_match`'s `resolve_in_world` and its ABBA regression
    /// guard). Centralising the write path here means the *writer* half of that discipline now
    /// exists in exactly one place and cannot drift.
    ///
    /// # Full replace, deliberately
    ///
    /// Both callers want current-zone truth, so stale entries from a previous zone are cleared
    /// rather than merged. `sync_entities` already did this; the login seed did not, and inherits
    /// the stricter behaviour here.
    ///
    /// For the login seed this is **latent hardening, not a bug that was reachable**: on current
    /// control flow the seed runs exactly once against still-empty maps (it sits in the `Ok(..)`
    /// arm, so a failed attempt never seeds and a successful one never returns to the retry loop),
    /// and `sync_entities` full-replaces from authoritative state on the next tick regardless. An
    /// earlier revision of this PR described it as a second live bug; that was an overclaim.
    pub fn publish_entities<'a, I>(&self, entities: I) -> usize
    where
        I: IntoIterator<Item = (&'a u32, &'a eqoxide_core::game_state::Entity)>,
    {
        let mut positions = self.entity_positions.lock().unwrap(); // 1st — canonical order
        let mut ids       = self.entity_ids.lock().unwrap();       // 2nd
        let mut poses     = self.entity_poses.lock().unwrap();     // 3rd
        positions.clear();
        ids.clear();
        poses.clear();
        for (&id, e) in entities {
            positions.insert(e.name.clone(), (e.x, e.y, e.z));
            ids.insert(e.name.clone(), id);
            poses.insert(e.name.clone(), EntityPoseView {
                pose: e.pose.label(),
                gait: e.gait.map(|g| g.raw()),
            });
        }
        positions.len()
    }
}

/// Single-authority controller integration (design §2): the render thread's authoritative
/// position snapshot streamed to the server, the `/goto` planner's per-frame movement intent, and
/// a server correction handed back to the controller. `ActionLoop`-only — `HttpState` has no
/// controller-facing endpoint today, so there is nothing for it to embed here.
///
/// (#608: the walker's draw-only `nav_path_view` overlay pair that #452 moved here is gone — the
/// walker now publishes the full `eqoxide_nav::diagnostics::NavDebugSnapshot`, whose view slot is
/// defined in `eqoxide-nav` because this crate sits below it. See the note above
/// `EntityPositions`.)
#[derive(Clone, Default)]
pub struct ControllerSlots {
    pub controller_view: ControllerShared,
    pub nav_intent:      NavIntent,
    pub pos_correction:  PosCorrection,
}

/// `/v1/lifecycle/*`: camp (+ its published deadline) and respawn. `HttpState`-only: `ActionLoop`
/// only ever WRITES `camp` (never reads `camp_until`/`respawn`, which the separate gameplay-tick
/// gets directly — see `eq_net::gameplay::run_gameplay_phase`), so it keeps a lone `camp` field
/// rather than embedding this whole bundle for one field it partially uses.
#[derive(Clone, Default)]
pub struct LifecycleSlots {
    pub camp:       CampReq,
    pub camp_until: CampUntil,
    pub respawn:    RespawnReq,
}

/// What HTTP hands straight to the RENDER thread, bypassing the nav thread entirely:
/// `/v1/camera/*` (cmd + published snapshot), `GET /v1/observe/frame` (frame-capture request),
/// and the manual-move/jump escape hatch consumed by the controller alongside WASD. `HttpState`-
/// only; no `Default` (the camera snapshot's initial value is meaningful — see `App::new`/
/// `main.rs` — so callers construct this explicitly rather than risk a silently-wrong default).
///
/// MVC C2 (#452): the manual-move/jump escape hatch (`manual_move`) is a view→RENDER command — the
/// render thread's controller consumes it (see `App`), the Model/nav thread never does — so it lives
/// HERE, on the render-bound camera bundle, and NOT in the view→MODEL `command_state::CommandState`
/// facade. `request_manual_move` is the typed write the HTTP View makes (mirroring
/// `cmd_tx`'s role for the orbit camera); the render View reads `manual_move` directly per frame.
#[derive(Clone)]
pub struct CameraSlots {
    pub cmd_tx:      Arc<Mutex<Option<CameraCmd>>>,
    pub snapshot:    Arc<Mutex<CameraSnapshot>>,
    pub frame_req:   FrameReq,
    pub manual_move: ManualMoveReq,
}

impl CameraSlots {
    /// Queue a manual-move/jump escape-hatch command (POST /v1/move/manual, /v1/move/jump). The
    /// render thread's `CharacterController` picks it up next frame and drives until `m.until`
    /// (#188/#207). This is a view→render command; it never reaches the Model/nav thread.
    pub fn request_manual_move(&self, m: ManualMove) {
        *self.manual_move.lock().unwrap() = Some(m);
    }
}

/// MVC C2 (#452): pin the tidied CommandState boundary at the `ipc` layer.
#[cfg(test)]
mod c2_boundary_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The manual-move/jump escape hatch is a view→RENDER command owned by `CameraSlots` (the
    /// render-bound bundle), not the view→model `CommandState`. Round-trip its typed write against a
    /// direct per-frame read, exactly as the HTTP View writes it and the render View consumes it.
    #[test]
    fn camera_slots_manual_move_round_trips() {
        // Build a plain snapshot directly — the `camera_state::CameraState` that normally produces
        // one lives in the app crate (this crate owns only the `CameraSnapshot` type), and the
        // manual-move round-trip under test does not depend on the snapshot's contents.
        let camera = CameraSlots {
            cmd_tx:      Arc::new(Mutex::new(None)),
            snapshot:    Arc::new(Mutex::new(CameraSnapshot {
                mode:      CameraMode::AutoFollow,
                azimuth:   0.0,
                elevation: 0.0,
                radius:    0.0,
                focus:     [0.0, 0.0, 0.0],
            })),
            frame_req:   Arc::new(Mutex::new(None)),
            manual_move: Arc::new(Mutex::new(None)),
        };
        assert!(camera.manual_move.lock().unwrap().is_none());

        let m = ManualMove { dir: [1.0, 0.0], up: 0.0, jump: false, until: Instant::now() + Duration::from_millis(400) };
        camera.request_manual_move(m);
        // The render thread's per-frame read (see `App`): a non-clearing poll of `Option<ManualMove>`.
        let seen = camera.manual_move.lock().unwrap().expect("manual move queued");
        assert_eq!(seen.dir, [1.0, 0.0]);
    }

    /// #608: the walker's path overlay no longer flows through `ControllerSlots` at all — the
    /// published `NavDebugSnapshot` (in `eqoxide-nav`) is the ONE channel for committed routes.
    /// This pins that the controller bundle stayed a pure movement-integration channel.
    #[test]
    fn controller_slots_carry_only_movement_integration() {
        let controller = ControllerSlots::default();
        assert!(controller.nav_intent.lock().unwrap().is_none());
        assert!(controller.pos_correction.lock().unwrap().is_none());
    }
}

/// #371: the active-liveness-probe state machine, tested as a pure function. These are the exact
/// distinctions the issue turns on — a wedged-but-ACKing world vs a genuinely idle one — proved
/// without a socket. The `secs`/`ms` helpers keep the age arithmetic readable.
#[cfg(test)]
mod world_responsive_tests {
    use super::{world_responsive, PASSIVE_LIVENESS_STALE_SECS, PROBE_TIMEOUT_SECS};
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(PROBE_TIMEOUT_SECS);
    const STALE:   Duration = Duration::from_secs(PASSIVE_LIVENESS_STALE_SECS);
    fn s(secs: u64) -> Duration { Duration::from_secs(secs) }

    /// Shorthand for the #371 probe-path tests, which all assume a LIVE link (`connected == true`)
    /// and the standard bounds — those cases are about the probe verdict, not the link. The #470
    /// tests that vary `connected`/staleness call `world_responsive` in full.
    fn wr(first_unanswered_sent_ago: Option<Duration>, probe_reply_ago: Option<Duration>,
          last_packet_ago: Duration) -> bool {
        world_responsive(true, first_unanswered_sent_ago, probe_reply_ago, last_packet_ago, TIMEOUT, STALE)
    }

    /// THE bug (#371): a probe was sent, no reply has come, and the world has been silent longer than
    /// the bound — while the link is still ACKing. That is a wedged world, and it MUST read as such.
    #[test]
    fn unanswered_probe_past_the_bound_reports_the_world_wedged() {
        // The realistic wedge: the last spontaneous packet PREDATES the probe (world went quiet at
        // 30s ago, we probed 15s ago), the probe was never answered, and 15s > the 10s bound. The
        // probe is only ever sent AFTER a stretch of app-silence, so last_packet_ago > probe_sent_ago
        // always holds here — nothing has arrived since the probe to prove liveness.
        assert!(!wr(Some(s(15)), None, s(30)),
            "an unanswered probe past the timeout, on a still-ACKing link, is a wedged world");
    }

    /// The #343-trap-in-reverse: a legitimately IDLE session that has no spontaneous app traffic for
    /// 45s but whose probe IS answered must STILL read as live. This is the false-alarm we must not
    /// raise — the whole reason a passive `last_packet_age_ms` threshold cannot solve the problem.
    #[test]
    fn idle_but_answered_probe_is_still_live() {
        // last spontaneous packet 45s ago (a normal solo-idle gap), but the probe replied 2s ago.
        assert!(wr(Some(s(30)), Some(s(2)), s(45)),
            "an idle world that ANSWERS the probe is alive — do not false-alarm on app-silence alone");
    }

    /// A probe answered by its own reply is live even with zero spontaneous traffic.
    #[test]
    fn answered_probe_reports_live() {
        assert!(wr(Some(s(30)), Some(s(1)), s(30)));
    }

    /// A probe in flight but not yet overdue must NOT false-alarm — ordinary round-trip latency is
    /// not a wedge. Only crossing the bound flips it.
    #[test]
    fn outstanding_probe_within_the_bound_is_not_yet_a_wedge() {
        // Unanswered (last packet predates the probe → no proof of life since), but 3s < 10s bound.
        assert!(wr(Some(s(3)), None, s(20)),
            "a 3s-old unanswered probe (bound 10s) is still in flight, not a wedge");
        // ...and one whose prior reply predates the newest send is likewise still outstanding.
        assert!(wr(Some(s(3)), Some(s(20)), s(20)),
            "a reply OLDER than the latest probe does not answer it, but 3s < 10s is not yet overdue");
    }

    /// Spontaneous application traffic since the probe was sent proves the world is processing even
    /// if that one probe reply was dropped — a busy zone must never read as wedged. This is the
    /// belt-and-suspenders clause.
    #[test]
    fn spontaneous_traffic_since_the_probe_proves_liveness() {
        // probe sent 15s ago, no probe reply, BUT an app packet arrived 1s ago (world is busy).
        assert!(wr(Some(s(15)), None, s(1)),
            "any app packet since the probe proves liveness — a busy zone is never wedged");
    }

    /// Before the first probe fires (e.g. mid zone-in) AND while packets are still fresh, there is no
    /// probe verdict; defer to the passive clock rather than assert a liveness we have not measured.
    #[test]
    fn no_probe_sent_yet_with_fresh_packets_defers_to_alive() {
        assert!(wr(None, None, s(2)),
            "no probe yet + fresh packets → defer → true (connected/last_packet_age_ms still stand)");
    }

    /// Exactly at the bound counts as overdue (the boundary is closed on the wedge side), so a probe
    /// sitting right at the timeout with no other proof of life reads as wedged.
    #[test]
    fn boundary_at_the_timeout_is_wedged() {
        assert!(!wr(Some(TIMEOUT), None, s(60)),
            "sent_ago == timeout is overdue (not `< timeout`), so it reports wedged");
    }

    // ── #470: the zombie-session honesty fix ────────────────────────────────────────────────────

    /// THE #470 bug, mutation-checked. A failed world-reconnect kills the net thread AND its prober,
    /// so `first_unanswered_probe_sent` is `None` forever while the link goes dead (`connected:false`)
    /// and no packet has arrived for minutes. The pre-#470 `None => true` returned `true` here
    /// UNCONDITIONALLY — a fully dead session that reads alive forever. It MUST now read dead.
    /// Mutation check: revert the fix to `None => true` (or drop the `if !connected` gate) and this
    /// assertion flips to a failure — it cannot pass without the honesty fix.
    #[test]
    fn dead_link_with_no_probe_is_not_responsive() {
        assert!(!world_responsive(false, None, None, s(300), TIMEOUT, STALE),
            "connected:false + stale packets + no outstanding probe is a ZOMBIE, not a live world (#470)");
    }

    /// A dead link is dead even if a probe was once outstanding and even mid-flight — the link gate
    /// precedes every probe branch. (Belt-and-suspenders: the zombie's real state is `None`, but the
    /// gate must not depend on that.)
    #[test]
    fn dead_link_overrides_any_probe_state() {
        assert!(!world_responsive(false, Some(s(1)), Some(s(1)), s(1), TIMEOUT, STALE),
            "connected:false condemns the world regardless of a fresh-looking probe verdict (#470)");
    }

    /// The #343 idle-but-ANSWERED session, in its real no-streak form: `record_probe_reply` clears
    /// `first_unanswered_probe_sent` the instant a genuine reply lands, so an answered idle session has
    /// NO outstanding streak (`None`) even though its last spontaneous packet is 45s stale. A fresh
    /// probe reply is proof of life and must keep it alive — the passive staleness gate must consider
    /// the probe-reply clock, not the spontaneous-packet clock alone, or a healthy idle session reads
    /// as a #470 zombie.
    #[test]
    fn no_streak_but_fresh_probe_reply_is_alive_despite_stale_packets() {
        assert!(world_responsive(true, None, Some(s(2)), s(45), TIMEOUT, STALE),
            "an answered idle session (streak cleared, reply 2s ago) is alive even at 45s app-silence");
    }

    /// The positive companion the fix must NOT regress: a healthy in-session state — link alive,
    /// recent packet, no outstanding probe — still reads alive. This is the ordinary active-play case
    /// (no app-silence → the prober never fires) and it must stay `true`.
    #[test]
    fn healthy_connected_session_with_recent_packet_and_no_probe_is_alive() {
        assert!(world_responsive(true, None, None, s(1), TIMEOUT, STALE),
            "a live link with fresh traffic and no probe outstanding is a healthy session (#470)");
    }

    /// The #343-idle guard for the passive path: a CONNECTED session with no probe outstanding must
    /// stay alive right up to the staleness bound (40s = one full probe cycle + reply window), so an
    /// answered-idle session — whose proof-of-life climbs to nearly a full `PROBE_INTERVAL` between
    /// answered probes — never false-alarms. The bound MUST exceed the resend cadence; below it lies
    /// the regression the reviewer caught. See `gameplay.rs::wedge_timeline_tests` for the end-to-end
    /// cadence proof over a real probe timeline.
    #[test]
    fn connected_no_probe_defers_below_the_passive_bound() {
        assert!(world_responsive(true, None, None, s(30), TIMEOUT, STALE),
            "30s app-silence (one full probe interval) with a live link must still defer to alive (< 40s bound)");
        // ...and a live prober would have re-probed at 30s and, unanswered, moved to the Some/timeout
        // branch by ~40s — so still sitting in the `None` branch past the bound means the prober is
        // gone (#470) → condemn.
        assert!(!world_responsive(true, None, None, STALE, TIMEOUT, STALE),
            "at/after the passive bound with no probe ever, the prober is dead → zombie (#470)");
    }
}

/// #643 review round 2 — the roster-publisher invariant.
#[cfg(test)]
mod world_roster_tests_643 {
    use super::WorldSlots;
    use eqoxide_core::game_state::{make_entity, Gait, Pose};

    /// `publish_entities` writes all three maps or none — the guarantee
    /// `/v1/observe/entities?labeled=1` makes to agents. MUTATION CHECK: delete any one of the
    /// three `insert`s (or any one `clear`) in `publish_entities` and this goes RED.
    #[test]
    fn publish_entities_writes_all_three_maps_with_identical_keys() {
        let world = WorldSlots::default();

        let mut sitter = make_entity(1, "a_sitter", 1.0, 2.0, 3.0, true);
        sitter.pose = Pose::Sitting;
        sitter.gait = Some(Gait::from_wire_10bit(12));
        let mut walker = make_entity(2, "a_walker", 4.0, 5.0, 6.0, true);
        walker.gait = Some(Gait::from_wire_10bit(1012)); // backing up: -12
        let entities: std::collections::HashMap<u32, _> =
            [(1u32, sitter), (2u32, walker)].into_iter().collect();

        assert_eq!(world.publish_entities(&entities), 2);

        let positions = world.entity_positions.lock().unwrap();
        let ids       = world.entity_ids.lock().unwrap();
        let poses     = world.entity_poses.lock().unwrap();

        fn sorted<V>(m: &std::collections::HashMap<String, V>) -> Vec<String> {
            let mut v: Vec<String> = m.keys().cloned().collect();
            v.sort();
            v
        }
        assert_eq!(sorted(&positions), sorted(&ids),
            "positions and ids must have identical key sets");
        assert_eq!(sorted(&positions), sorted(&poses),
            "positions and poses must have identical key sets — an agent indexes `poses` by a name \
             it read from `entities`");

        assert_eq!(positions["a_sitter"], (1.0, 2.0, 3.0));
        assert_eq!(ids["a_sitter"], 1);
        assert_eq!(poses["a_sitter"].pose, "sitting");
        assert_eq!(poses["a_sitter"].gait, Some(12));
        assert_eq!(poses["a_walker"].pose, "standing");
        assert_eq!(poses["a_walker"].gait, Some(-12), "a backing-up mob's gait stays negative");
    }

    /// A second publish must FULL-REPLACE, not merge: an entity from the previous zone (or the
    /// previous login attempt) must not survive in any of the three maps.
    #[test]
    fn publish_entities_full_replaces_so_no_stale_entity_survives() {
        let world = WorldSlots::default();
        let first: std::collections::HashMap<u32, _> =
            [(1u32, make_entity(1, "old_zone_mob", 0.0, 0.0, 0.0, true))].into_iter().collect();
        world.publish_entities(&first);
        let second: std::collections::HashMap<u32, _> =
            [(2u32, make_entity(2, "new_zone_mob", 0.0, 0.0, 0.0, true))].into_iter().collect();
        world.publish_entities(&second);

        assert!(!world.entity_positions.lock().unwrap().contains_key("old_zone_mob"));
        assert!(!world.entity_ids.lock().unwrap().contains_key("old_zone_mob"));
        assert!(!world.entity_poses.lock().unwrap().contains_key("old_zone_mob"),
            "a stale pose is worse than a stale position — it is a confident claim about a body \
             state that no longer exists");
    }

}
