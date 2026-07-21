//! `Model` — the COLD backend seam of the client's MVC (epic #445, increment B1 / #449).
//!
//! ## What "the Model" is
//!
//! In this client the **Model is the `eq-net` thread** — the SOLE writer of [`GameState`]. It owns
//! the world: it connects, drains the view's COMMANDS (the [`CommandState`] write-path slots), applies
//! inbound world packets to its private `GameState`, and PUBLISHES an immutable snapshot each tick
//! (`eq_net::gameplay::publish_snapshot` → the `ArcSwap` in [`ModelContext::game_state_snapshot`]).
//! Render/HTTP read that snapshot lock-free via `load_full`.
//!
//! Until B1 that owner was a bare free function (`eq_net::run_login_flow`) hard-wired into `main.rs`.
//! B1 puts it behind a trait so the backend becomes SWAPPABLE — B2 (#450) adds a `MockModel` that
//! implements this same trait with NO server, which is what breaks the circular nav-QA loop (a
//! headless test can drive nav/actions against a deterministic world).
//!
//! ## The seam (consume COMMANDS, produce WORLD SNAPSHOTS)
//!
//! [`ModelContext`] IS the seam, and it is backend-AGNOSTIC:
//!   * **in**  — [`CommandState`] (the typed view→model command slots) + the raw camp/respawn/shutdown
//!     lifecycle signals + the per-domain published-roster bundles the owner also fills;
//!   * **out** — [`ModelContext::game_state_snapshot`] (the `ArcSwap<GameState>` the render hot path
//!     reads) and [`ModelContext::net_health`] (the liveness clocks HTTP turns into `connected` etc).
//!
//! A concrete [`Model`] holds only its backend-SPECIFIC configuration ([`ServerModel`] holds the
//! [`LoginConfig`] + retry count; a future `MockModel` would hold its scripted world). Both receive
//! the SAME `ModelContext`, so a test can point either backend at the same snapshot/command plumbing.
//!
//! ## SCOPE / what is deliberately NOT touched (behavior-preserving, per #449)
//!
//! * The `ArcSwap` snapshot publish path is UNCHANGED — `ServerModel::run` delegates verbatim to the
//!   existing `eq_net::run_login_flow`, so the frozen-`connected` class of bug (#343) cannot be
//!   reintroduced by this increment. This module adds a trait + a struct + a context bundle and
//!   re-homes `main.rs`'s call through them; it moves no world logic.
//! * The trait abstracts the COLD backend only. It has NO per-frame read method — the hot read stays
//!   the concrete `GameState` snapshot (`ArcSwap::load_full`), never a trait object. There is no
//!   per-frame dynamic dispatch anywhere in this module by construction.
//! * `run` is a concrete (non-`dyn`) `async fn` in the trait ON PURPOSE: the client only ever holds
//!   ONE backend at a time and drives it inside the `eq-net` thread's `block_on`, so no boxing / no
//!   `Send`-on-`Future` bound / no `Pin<Box<dyn Future>>` ceremony is needed. B2's headless test
//!   holds a concrete `MockModel` (or is generic over `M: Model`); neither needs `dyn`.
//!
//! ## Shared-Arc identity (load-bearing — see epic #445)
//!
//! `ModelContext` holds `.clone()`s of the SAME `ipc` bundle Arcs `main.rs` also clones into
//! `HttpState`/`CommandState`. Those are shallow Arc-handle clones — identity is preserved — so a
//! `request_*` write on the view side and its `take_*` drain inside the owner still hit the identical
//! cell. Re-homing the construction from a flat `run_login_flow(a, b, …)` arg list into a
//! `ModelContext { … }` literal moves only WHERE the clone lands, not the Arc it points at.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::command_state::CommandState;
use crate::config::LoginConfig;

/// The backend-agnostic Model seam: the command slots a backend DRAINS and the snapshot/health it
/// PUBLISHES, plus the lifecycle signals it obeys. Constructed once in `main.rs` (the Controller /
/// wiring role) from `.clone()`s of the bundles it also hands to `HttpState`/`CommandState`, then
/// moved into whichever [`Model`] drives the `eq-net` thread. Every field is a shallow Arc-handle
/// clone — see the module doc's shared-Arc note.
///
/// This mirrors, one-for-one, the parameters `eq_net::run_login_flow` already takes (minus the
/// server-specific `config`/`max_retries`, which live on [`ServerModel`]); B1 introduces no new
/// channels, it only groups the existing ones under the seam they collectively form.
pub struct ModelContext {
    pub nav:             crate::ipc::NavSlots,
    pub world:           crate::ipc::WorldSlots,
    pub quest:           crate::ipc::QuestSlots,
    pub group_slots:     crate::ipc::GroupSlots,
    /// The typed view→model command write-path facade (the "consume COMMANDS" half of the seam).
    pub command:         CommandState,
    pub social:          crate::ipc::SocialSlots,
    pub merchant_slots:  crate::ipc::MerchantSlots,
    pub inventory_slots: crate::ipc::InventorySlots,
    pub interact:        crate::ipc::InteractSlots,
    pub chat:            crate::ipc::ChatSlots,
    pub controller:      crate::ipc::ControllerSlots,
    pub guild_slots:     crate::ipc::GuildSlots,
    pub collision:       crate::nav::collision::SharedCollision,
    pub maps_dir:        PathBuf,
    pub shutdown:        Arc<AtomicBool>,
    pub camp:            crate::ipc::CampReq,
    pub camp_until:      crate::ipc::CampUntil,
    pub respawn:         crate::ipc::RespawnReq,
    /// The `ArcSwap<GameState>` render/HTTP read lock-free (the "produce WORLD SNAPSHOTS" half).
    pub game_state_snapshot: crate::ipc::GameStateSnapshot,
    /// The liveness clocks the owner stamps; HTTP derives `connected`/`world_responsive` from them.
    pub net_health:          crate::ipc::NetHealthShared,
}

/// The cold backend seam. A `Model` OWNS the world: given a [`ModelContext`], it drives the world
/// state to completion on the `eq-net` thread — connecting (or not, for a mock), draining the
/// context's commands, and publishing `GameState` snapshots each tick — until shutdown.
///
/// `run` takes `self` by value: driving the backend consumes it (it runs for the process lifetime).
/// It is intentionally a concrete `async fn` (no `dyn`, no `Send` bound) — see the module doc.
// `async fn` in a public trait warns because a caller can't add its own `Send` bound; that is
// exactly fine here (the single caller drives it on one thread inside `block_on`), so silence it.
#[allow(async_fn_in_trait)]
pub trait Model {
    /// Drive the backend world to completion. `Ok(())` on a clean end (e.g. login retries exhausted
    /// cleanly / shutdown); `Err` on a fatal backend failure the caller logs.
    async fn run(self, ctx: ModelContext) -> Result<(), String>;
}

/// The production [`Model`]: the real EQ server connection. Holds only the server-specific config;
/// its [`Model::run`] delegates VERBATIM to the existing `eq_net::run_login_flow` (login handshake →
/// `run_gameplay_phase`, the sole `GameState` writer / snapshot publisher). This is the concrete
/// backend the client has always run — B1 only names it and routes construction through the trait.
pub struct ServerModel {
    config:      LoginConfig,
    max_retries: u32,
}

impl ServerModel {
    /// `config` is the per-character login profile; `max_retries` is the login-flow retry budget
    /// (`main.rs` has always passed `10`).
    pub fn new(config: LoginConfig, max_retries: u32) -> Self {
        ServerModel { config, max_retries }
    }
}

impl Model for ServerModel {
    async fn run(self, ctx: ModelContext) -> Result<(), String> {
        // Delegate verbatim to the pre-existing owner. Destructure the context back into the exact
        // arg list `run_login_flow` expects, in order — this is pure re-plumbing, no behavior change.
        crate::eq_net::run_login_flow(
            self.config,
            self.max_retries,
            ctx.nav,
            ctx.world,
            ctx.quest,
            ctx.group_slots,
            ctx.command,
            ctx.social,
            ctx.merchant_slots,
            ctx.inventory_slots,
            ctx.interact,
            ctx.chat,
            ctx.controller,
            ctx.guild_slots,
            ctx.collision,
            ctx.maps_dir,
            ctx.shutdown,
            ctx.camp,
            ctx.camp_until,
            ctx.respawn,
            ctx.game_state_snapshot,
            ctx.net_health,
        )
        .await
    }
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// B2 (#450): MockModel — a deterministic, server-free [`Model`].
// ─────────────────────────────────────────────────────────────────────────────────────────────
//
// This is the testability unlock the B1 module doc promised: a backend that satisfies the SAME
// `Model` trait as `ServerModel` but drives a HAND-AUTHORED world with no sockets, no server, and no
// wall-clock. It exists to break the circular nav-QA loop (see the
// eq-nav-qa-reachability-vs-walkability memory): today the nav merge gate samples start/goal from the
// same floor model it is grading, so it grades itself. A `MockModel` lets a headless test drive REAL
// client logic (the actual `Collision::find_path_ex` planner, the actual `CommandState` drain, the
// actual `ArcSwap` snapshot publish) against a world whose right answers are KNOWN because the test
// wrote the geometry by hand.
//
// It is gated `#[cfg(test)]`: it is test infrastructure, compiled only into the test binary, so it
// adds nothing to the shipped client and cannot drift the production build. Because all of a crate's
// `#[cfg(test)]` code is compiled together, any other in-crate test module — the nav-QA tests in
// `nav/collision.rs`, a future action-resolution test — can drive it via `crate::model::MockModel`.
//
// ## The seam it exercises (identical to `ServerModel`'s)
//   * IN  — it DRAINS `ctx.command` (the typed view→model command slots) deterministically: a queued
//     `/goto` is resolved by the real planner against the scripted map; queued chat is recorded.
//   * OUT — it PUBLISHES the scripted world into `ctx.game_state_snapshot` (the `ArcSwap` render/HTTP
//     read) and the scripted map into `ctx.collision` (the `SharedCollision` the nav thread reads),
//     exactly the two channels `ServerModel` fills from the wire.
//
// ## Determinism contract
// `MockModel::run` performs a fixed, finite sequence with no timing, no I/O, and no RNG. The same
// script therefore yields byte-identical published state and an identical `PlanOutcome` on every run
// and every machine — the property `mock_run_is_deterministic` pins. That is the whole point: a
// nondeterministic mock could not be a trustworthy oracle for a nav regression.
//
// ## What C (#451/#452) builds on this
// C1 formalizes `WorldState` as the Model-only half of `GameState`. `MockModel` already treats the
// published snapshot as the sole Model output, so when `WorldState` is split out, the mock's
// `publish_snapshot` body is the natural place that constructs it — the scripting surface
// (`self_at`/`zone`/`spawn`/`map`) becomes a `WorldState` builder unchanged.
#[cfg(test)]
pub(crate) use mock::{MockModel, MockProbe};

#[cfg(test)]
mod mock {
    use super::*;
    use crate::game_state::{Entity, GameState};
    use crate::ipc::ChatSend;
    use crate::nav::collision::{Collision, PlanCtx, PlanOutcome};
    use std::sync::Mutex;

    /// Observation handles into a running [`MockModel`], cloned out via [`MockModel::probe`] BEFORE
    /// `run` so a test can read what the mock resolved after `run` returns. Every field is a shared
    /// Arc handle (like the `ipc` bundles), so the clone the test holds and the clone inside the mock
    /// are the same cell.
    ///
    /// These surface the two things the published `GameState` snapshot does NOT already carry: the
    /// honest [`PlanOutcome`] of the last resolved `/goto` (so a nav test can assert the REAL planner
    /// said `Route`/`Unreachable`, not merely that the avatar moved), and the drained outgoing chat
    /// (so an action test can prove a command was consumed server-free). Snapshot-carried facts
    /// (self position, zone, spawns) are asserted off `game_state_snapshot.load_full()` directly.
    #[derive(Clone, Default)]
    pub(crate) struct MockProbe {
        last_plan: std::sync::Arc<Mutex<Option<PlanOutcome>>>,
        sent_chat: std::sync::Arc<Mutex<Vec<ChatSend>>>,
    }

    impl MockProbe {
        /// The honest outcome of the most recent `/goto` the mock resolved via the real planner, or
        /// `None` if no `/goto` was queued / the mock had no map to plan against.
        pub(crate) fn last_plan(&self) -> Option<PlanOutcome> {
            self.last_plan.lock().unwrap().clone()
        }
        /// Every chat message the mock drained from `CommandState` this run, FIFO.
        pub(crate) fn sent_chat(&self) -> Vec<ChatSend> {
            self.sent_chat.lock().unwrap().clone()
        }
    }

    /// A deterministic, no-server [`Model`]. Build a world with the chained setters, hand it to the
    /// `eq-net`-style driver via [`Model::run`], then read the published snapshot / [`MockProbe`].
    ///
    /// Minimal but not a toy: `map` accepts a real [`Collision`] grid, and a queued `/goto` is
    /// resolved by the ACTUAL `find_path_ex` planner — so this genuinely drives a nav plan against a
    /// known map, which is the proof it unlocks server-free nav testing.
    pub(crate) struct MockModel {
        zone_id:      u16,
        zone_name:    String,
        /// Avatar position as the planner's world triple `[east, north, up]`, mirrored into the
        /// snapshot's `player_x/y/z`. A resolved `/goto` `Route` advances this to the goal.
        self_pos:     [f32; 3],
        self_heading: f32,
        entities:     Vec<Entity>,
        map:          Option<std::sync::Arc<Collision>>,
        /// Planning radius handed to `find_path_ex` (avatar half-width). Default 1.0.
        plan_radius:  f32,
        probe:        MockProbe,
    }

    impl MockModel {
        /// An empty world: zone 0, avatar at the origin, no spawns, no map.
        pub(crate) fn new() -> Self {
            MockModel {
                zone_id:      0,
                zone_name:    String::new(),
                self_pos:     [0.0, 0.0, 0.0],
                self_heading: 0.0,
                entities:     Vec::new(),
                map:          None,
                plan_radius:  1.0,
                probe:        MockProbe::default(),
            }
        }

        /// Set the scripted zone id + name (published into the snapshot).
        pub(crate) fn zone(mut self, id: u16, name: &str) -> Self {
            self.zone_id = id;
            self.zone_name = name.to_string();
            self
        }

        /// Place the avatar. `pos` is the planner's `[east, north, up]` world triple.
        pub(crate) fn self_at(mut self, pos: [f32; 3]) -> Self {
            self.self_pos = pos;
            self
        }

        /// Set the avatar heading (EQ units), published as `player_heading`.
        pub(crate) fn heading(mut self, h: f32) -> Self {
            self.self_heading = h;
            self
        }

        /// Add a fully-specified spawn to the scripted zone.
        pub(crate) fn spawn(mut self, e: Entity) -> Self {
            self.entities.push(e);
            self
        }

        /// Convenience: add a minimal living NPC at `pos` (`[east, north, up]`). All the cosmetic /
        /// combat fields default to sane values so a nav or action test can place a target in one
        /// line without hand-filling `Entity`'s 20-odd fields.
        pub(crate) fn npc(self, spawn_id: u32, name: &str, pos: [f32; 3]) -> Self {
            self.spawn(Entity {
                spawn_id,
                name: name.to_string(),
                level: 1,
                is_npc: true,
                x: pos[0], y: pos[1], z: pos[2],
                hp_pct: 100.0, cur_hp: 100, max_hp: 100,
                race: "Human".to_string(),
                heading: 0.0,
                dead: false,
                equipment: [0; 9],
                equipment_tint: [[0; 3]; 9],
                gender: 0, helm: 0, showhelm: 0, face: 0, hairstyle: 0, haircolor: 0,
                animation: 100, // Standing
                floating: false,
            })
        }

        /// Publish this collision grid as the zone's known map — the surface a `/goto` is planned
        /// against by the real `find_path_ex`. This is the hand-authored geometry whose right answers
        /// the test knows.
        pub(crate) fn map(mut self, col: Collision) -> Self {
            self.map = Some(std::sync::Arc::new(col));
            self
        }

        /// Override the planning radius (avatar half-width) `find_path_ex` uses. Default 1.0.
        pub(crate) fn plan_radius(mut self, r: f32) -> Self {
            self.plan_radius = r;
            self
        }

        /// Clone out the observation handles BEFORE `run` (which consumes `self`).
        pub(crate) fn probe(&self) -> MockProbe {
            self.probe.clone()
        }

        /// Build the scripted `GameState` and publish it into the snapshot `ArcSwap`. The SOLE
        /// snapshot writer, mirroring `ServerModel`'s wire-driven publish.
        fn publish_snapshot(&self, ctx: &ModelContext) {
            let mut gs = GameState::new();
            gs.player_x = self.self_pos[0];
            gs.player_y = self.self_pos[1];
            gs.player_z = self.self_pos[2];
            // Offline/testzone: this Model IS the position authority (no server), so the position
            // it publishes is by definition known — distances derived from it are real (#513).
            gs.player_pos_known = true;
            gs.player_heading = self.self_heading;
            gs.world.zone_id = self.zone_id;
            gs.world.zone_name = self.zone_name.clone();
            for e in &self.entities {
                gs.world.entities.insert(e.spawn_id, e.clone());
            }
            ctx.game_state_snapshot.store(std::sync::Arc::new(gs));
        }
    }

    impl Model for MockModel {
        async fn run(mut self, ctx: ModelContext) -> Result<(), String> {
            // 1. Publish the scripted map into the SharedCollision the nav thread would read.
            if let Some(map) = &self.map {
                *ctx.collision.write().unwrap() = Some(map.clone());
            }

            // 2. Publish the initial scripted world.
            self.publish_snapshot(&ctx);

            // 3. Drain the view→model commands deterministically (the "consume COMMANDS" half).
            //    Chat: record the drained queue so an action test can prove it was consumed.
            {
                let sent = ctx.command.take_chat_send();
                if !sent.is_empty() {
                    self.probe.sent_chat.lock().unwrap().extend(sent);
                }
            }

            //    Goto: resolve any queued destination with the REAL planner against the known map.
            //    On a complete Route, advance the avatar to the goal (a deterministic "arrival") so
            //    the republished snapshot observably reflects the navigation. This is the line that
            //    makes the mock drive real nav logic, not a stub.
            if let (Some(target), Some(map)) = (ctx.command.goto_target(), &self.map) {
                let goal = [target.0, target.1, target.2];
                let outcome = map.find_path_ex(
                    self.self_pos, goal, self.plan_radius, &[], 8.0, None, 0.0, PlanCtx::default(),
                );
                if let PlanOutcome::Route(path) = &outcome {
                    if let Some(last) = path.last() {
                        self.self_pos = *last;
                    }
                }
                *self.probe.last_plan.lock().unwrap() = Some(outcome);
            }

            // 4. Republish so the final snapshot reflects any resolved movement.
            self.publish_snapshot(&ctx);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// Compile-time proof the production backend implements the seam. If a future edit changed the
    /// trait so `ServerModel` no longer satisfied it, THIS stops compiling — the bad state is
    /// unrepresentable (verification hierarchy tier 1), not merely untested.
    const _: fn() = || {
        fn asserts_impl<M: Model>() {}
        asserts_impl::<ServerModel>();
    };

    /// A server-free [`ModelContext`] for tests: every command/roster bundle is `Default`
    /// (empty slots), the snapshot is seeded with a fresh `GameState`. This is exactly the plumbing
    /// B2's `MockModel` headless test will reuse, which is why the seam was made backend-agnostic.
    fn test_ctx() -> ModelContext {
        ModelContext {
            nav:             Default::default(),
            world:           Default::default(),
            quest:           Default::default(),
            group_slots:     Default::default(),
            command:         CommandState::default(),
            social:          Default::default(),
            merchant_slots:  Default::default(),
            inventory_slots: Default::default(),
            interact:        Default::default(),
            chat:            Default::default(),
            controller:      Default::default(),
            guild_slots:     Default::default(),
            collision:       Default::default(),
            maps_dir:        PathBuf::new(),
            shutdown:        Arc::new(AtomicBool::new(false)),
            camp:            Default::default(),
            camp_until:      Default::default(),
            respawn:         Default::default(),
            game_state_snapshot: Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::game_state::GameState::new(),
            )),
            net_health:      Arc::new(std::sync::Mutex::new(crate::ipc::NetHealth::default())),
        }
    }

    /// A no-server backend that flips a flag and returns — proving the [`Model`] trait is drivable
    /// with NO network at all. This is the B2 (#450) premise ("MockModel breaks the circular nav-QA
    /// loop") reduced to its essence: if the trait had leaked something server-only into its
    /// signature, this mock could not exist and this test would not compile/run.
    struct FlagModel(Arc<AtomicBool>);
    impl Model for FlagModel {
        async fn run(self, ctx: ModelContext) -> Result<(), String> {
            // Touch the seam the way a real backend does: observe a command handle and publish a
            // snapshot — with no server anywhere in sight.
            let _ = ctx.command; // the command write-path is consumable by a backend
            ctx.game_state_snapshot
                .store(Arc::new(crate::game_state::GameState::new()));
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn model_trait_is_drivable_without_a_server() {
        let ran = Arc::new(AtomicBool::new(false));
        let model = FlagModel(ran.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let out = rt.block_on(model.run(test_ctx()));
        assert_eq!(out, Ok(()));
        assert!(ran.load(Ordering::SeqCst), "the backend's run() body executed");
    }

    // ── B2 (#450): MockModel drives real client logic against a KNOWN, hand-authored world ─────────

    use crate::assets::{MeshData, RenderMode, ZoneAssets};
    use crate::nav::collision::{Collision, PlanOutcome};
    use crate::ipc::ChatSend;

    /// A hand-authored known map: one flat floor quad spanning east/north ∈ [-64, 64] at height 0.
    /// The nav answers on this geometry are KNOWN by construction — that is the whole point of a
    /// server-free oracle. Any point inside the square is walkable and mutually reachable; any point
    /// far outside has no floor and is unreachable. This breaks the circular nav-QA loop: the test,
    /// not the model, authored the ground truth.
    fn known_floor_map() -> Collision {
        // MeshData positions are EQ WLD order [north, up, east]; the floor lies flat at up = 0.
        let floor = MeshData {
            positions: vec![
                [-64.0, 0.0, -64.0],
                [ 64.0, 0.0, -64.0],
                [ 64.0, 0.0,  64.0],
                [-64.0, 0.0,  64.0],
            ],
            normals: vec![], uvs: vec![],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let assets = ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] };
        Collision::build(&assets, 8.0)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().build().expect("test runtime")
    }

    /// THE headline unlock: a headless test drives a REAL nav plan (the actual `find_path_ex`) against
    /// a KNOWN map with NO server, and the result is observable in the published snapshot.
    ///
    /// The test queues a `/goto` on the shared `CommandState` (exactly as an HTTP handler would),
    /// runs `MockModel`, then asserts (a) the honest planner outcome the mock recorded is a complete
    /// `Route` reaching the goal, and (b) — the #535 reviewer's weakness fix — the PUBLISHED snapshot
    /// (`load_full`) shows the avatar moved to the goal. Proving the store executed is not enough; we
    /// prove the publish took effect and is observable to a reader.
    #[test]
    fn mock_drives_a_real_nav_plan_to_a_reachable_goal_and_publishes_the_move() {
        let ctx = test_ctx();
        // `run` consumes `ctx`; capture the shared snapshot Arc so a reader can observe it afterward.
        let snap_handle = ctx.game_state_snapshot.clone();
        let goal = (30.0, 20.0, 0.0); // world [east, north, up], well inside the known floor

        // A view queues the command on the SAME command slots the model will drain (shared Arc).
        ctx.command.request_goto(goal);

        let model = MockModel::new()
            .zone(9001, "mockzone")
            .self_at([-40.0, -30.0, 0.0])
            .npc(42, "Scripted Rat", [10.0, 10.0, 0.0])
            .map(known_floor_map());
        let probe = model.probe();

        rt().block_on(model.run(ctx)).expect("mock run");

        // (a) The mock resolved the goto with the REAL planner and it found a complete route.
        match probe.last_plan().expect("a goto was queued, so a plan must have been resolved") {
            PlanOutcome::Route(path) => {
                let last = *path.last().expect("a route has waypoints");
                assert!((last[0] - goal.0).abs() < 8.0 && (last[1] - goal.1).abs() < 8.0,
                    "the real planner's route must reach the known-reachable goal, ended at {last:?}");
            }
            other => panic!("a goal inside the known floor must be a complete Route, got {other:?}"),
        }

        // (b) The publish is observable: a lock-free reader sees the scripted world AND the resolved move.
        let snap = snap_handle.load_full();
        assert_eq!(snap.world.zone_id, 9001, "published zone id must match the script");
        assert_eq!(snap.world.zone_name, "mockzone", "published zone name must match the script");
        assert!((snap.player_x - goal.0).abs() < 8.0 && (snap.player_y - goal.1).abs() < 8.0,
            "the published snapshot must show the avatar navigated to the goal, at ({}, {})",
            snap.player_x, snap.player_y);
        assert_eq!(snap.world.entities.get(&42).map(|e| e.name.as_str()), Some("Scripted Rat"),
            "the scripted spawn must be observable in the published snapshot");
    }

    /// The planner's HONESTY channel, exercised server-free: a goal with no floor under it (far
    /// outside the known map) must come back `Unreachable`, NOT a fabricated route and NOT a silent
    /// stall. The mock reports exactly what the real planner concluded. Mutation-check: if the mock
    /// ever teleported the avatar on a non-`Route` outcome, the position assertion below goes RED.
    #[test]
    fn mock_reports_a_known_unreachable_goal_as_unreachable() {
        let ctx = test_ctx();
        let snap_handle = ctx.game_state_snapshot.clone();
        let start = [0.0, 0.0, 0.0];
        ctx.command.request_goto((500.0, 500.0, 0.0)); // no geometry there

        let model = MockModel::new().self_at(start).map(known_floor_map());
        let probe = model.probe();
        rt().block_on(model.run(ctx)).expect("mock run");

        assert!(matches!(probe.last_plan(), Some(PlanOutcome::Unreachable { .. })),
            "a goal with no floor must be honestly Unreachable, got {:?}", probe.last_plan());
        // The avatar must NOT have moved — an Unreachable goal is not an arrival.
        let snap = snap_handle.load_full();
        assert_eq!([snap.player_x, snap.player_y, snap.player_z], start,
            "the avatar must not teleport toward an unreachable goal");
    }

    /// An ACTION resolves deterministically with no server: a queued chat command is drained and
    /// observably consumed. Also re-asserts the #535 weakness fix on the action path — the published
    /// snapshot reflects the script, not just that a store ran.
    #[test]
    fn mock_resolves_a_queued_action_without_a_server() {
        let ctx = test_ctx();
        let snap_handle = ctx.game_state_snapshot.clone();
        let cmd = ctx.command.clone(); // to observe the drained slot after `run` consumes `ctx`
        ctx.command.request_chat_send(ChatSend { chan: 5, to: String::new(), text: "hello mock".into() });

        let model = MockModel::new().zone(7, "actionzone").self_at([1.0, 2.0, 3.0]).heading(128.0);
        let probe = model.probe();
        rt().block_on(model.run(ctx)).expect("mock run");

        let sent = probe.sent_chat();
        assert_eq!(sent.len(), 1, "the queued chat command must have been drained by the model");
        assert_eq!(sent[0].text, "hello mock");
        // The command slot is now empty — the model consumed it, no server involved.
        assert!(cmd.take_chat_send().is_empty(), "the command must not remain queued");

        let snap = snap_handle.load_full();
        assert_eq!((snap.world.zone_id, snap.world.zone_name.as_str()), (7, "actionzone"));
        assert_eq!([snap.player_x, snap.player_y, snap.player_z], [1.0, 2.0, 3.0]);
        assert_eq!(snap.player_heading, 128.0);
    }

    /// The mock's OWN determinism guarantee (verification hierarchy: the test infra's contract is
    /// itself tested). The SAME script driven twice must yield the SAME published `GameState` and the
    /// SAME `PlanOutcome`. A nondeterministic oracle could not be trusted to catch a nav regression.
    #[test]
    fn mock_run_is_deterministic() {
        let run_once = || {
            let ctx = test_ctx();
            let snap_handle = ctx.game_state_snapshot.clone();
            ctx.command.request_goto((25.0, -15.0, 0.0));
            let model = MockModel::new()
                .zone(9001, "mockzone")
                .self_at([-40.0, -30.0, 0.0])
                .npc(42, "Scripted Rat", [10.0, 10.0, 0.0])
                .map(known_floor_map());
            let probe = model.probe();
            rt().block_on(model.run(ctx)).expect("mock run");
            let snap = snap_handle.load_full();
            ((*snap).clone(), probe.last_plan())
        };
        let (gs_a, plan_a) = run_once();
        let (gs_b, plan_b) = run_once();
        assert_eq!(gs_a, gs_b, "same script must publish an identical GameState");
        assert_eq!(plan_a, plan_b, "same script must resolve an identical PlanOutcome");
        assert!(matches!(plan_a, Some(PlanOutcome::Route(_))), "sanity: the scripted goal is reachable");
    }
}
