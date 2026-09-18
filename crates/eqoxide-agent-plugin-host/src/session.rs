//! Per-connection handshake, tick loop, and action dispatch (spec §5, §7, §8).

use crate::observation_builder::build_observation;
use eqoxide_agent_protocol::framing::{decode_line, encode_line};
use eqoxide_agent_protocol::handshake::{HandshakeReply, Hello, PROTOCOL_VERSION};
use eqoxide_agent_protocol::step::Step;
use eqoxide_agent_protocol::verb::{AgentVerb, CombatVerb, InteractVerb, LifecycleVerb};
use eqoxide_command::CommandState;
use eqoxide_core::game_state::GameState;
use eqoxide_core::spells::SpellDb;
use eqoxide_ipc::{CameraSlots, GameStateSnapshot, ManualMove, NetThreadDeadShared};
use eqoxide_nav::collision::SharedCollision;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

/// Re-issued every tick (~2x the 150ms tick, `MOVE_LATCH`), so movement naturally stops within
/// ~300ms of the agent going quiet — the same fail-safe deadline mechanism `ManualMove` already
/// gives HTTP's `/v1/move/manual`, applied here every tick instead of once per request.
const MOVE_LATCH: Duration = Duration::from_millis(300);

/// Mirrors `eqoxide_net::action_loop`'s private `NAV_TICK_MS = 150` (`crates/eqoxide-net/src/
/// action_loop.rs:9`) — movement/combat decisions only actually change at that cadence today, so
/// ticking faster would just repeat stale decisions (spec §5). Mirrored, not imported: this crate
/// must not depend on eqoxide-net.
const TICK_MS: u64 = 150;

/// Caps `AsyncBufReadExt::read_line`'s otherwise-unbounded growth at the one untrusted-input
/// boundary this feature has (the handshake's `Hello` line and each `Step` line). Generous: real
/// `Hello`/`Step` lines are well under 1KB. A client that streams bytes with no `\n` past this cap
/// is bad enough to disconnect, not a case worth trying to recover from.
const MAX_LINE_BYTES: usize = 256 * 1024;

/// How long the handshake's first `read_line` waits for a `Hello` before giving up — generous for
/// a real client, but bounded so a connection that never sends anything can't wedge the accept
/// loop (a client that connects and never speaks used to block every subsequent connection
/// forever, since nothing here had a deadline).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

pub fn apply_step(step: &Step, camera: &CameraSlots, command: &CommandState) {
    if let Some(m) = &step.movement {
        camera.request_manual_move(ManualMove {
            dir: m.dir,
            up: m.up,
            jump: m.jump,
            wish_heading: m.wish_heading,
            until: Instant::now() + MOVE_LATCH,
        });
    }
    if let Some(verb) = &step.verb {
        dispatch_verb(verb, command);
    }
}

/// Translate one `AgentVerb` into the real `CommandState` call it mirrors. Reserved-but-unwired
/// arms (`Move(ZoneCross)` and the four uninhabited families) are accepted and deliberately no-op —
/// not malformed, just not wired yet (spec §8, §12).
pub fn dispatch_verb(verb: &AgentVerb, command: &CommandState) {
    match verb {
        AgentVerb::Combat(CombatVerb::Target { spawn_id }) => {
            command.request_target(*spawn_id);
        }
        AgentVerb::Combat(CombatVerb::Attack { on }) => {
            command.request_attack(*on);
        }
        AgentVerb::Combat(CombatVerb::Consider { spawn_id }) => {
            command.request_consider(*spawn_id);
        }
        AgentVerb::Combat(CombatVerb::Cast(c)) => {
            command.request_cast(eqoxide_ipc::CastRequest {
                gem: c.gem,
                target_id: c.target_id,
                item_slot: c.item_slot,
            });
        }
        AgentVerb::Interact(InteractVerb::Sit) => {
            command.request_sit(true);
        }
        AgentVerb::Interact(InteractVerb::Stand) => {
            command.request_sit(false);
        }
        AgentVerb::Lifecycle(LifecycleVerb::Respawn) => {
            command.request_respawn();
        }
        // Reserved, typed, not wired (spec §8, §12) — accepted, deliberately does nothing yet.
        AgentVerb::Move(_) => {}
        // Uninhabited — no value of these types can ever be constructed, so these arms are
        // unreachable in practice; kept for exhaustiveness so a future inhabited variant is a
        // compile error here until dispatched.
        AgentVerb::Merchant(v) => match *v {},
        AgentVerb::Inventory(v) => match *v {},
        AgentVerb::Quests(v) => match *v {},
        AgentVerb::Chat(v) => match *v {},
    }
}

/// Read the client's `Hello`, reply `Accepted`/`Rejected`, and report which happened. The caller
/// closes the connection immediately on `false` (spec §5, §11).
pub async fn handshake<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(reader: &mut R, writer: &mut W) -> bool {
    let mut line = String::new();
    let read = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        (&mut *reader).take(MAX_LINE_BYTES as u64).read_line(&mut line),
    )
    .await;
    match read {
        Ok(Ok(0)) | Err(_) => return false, // EOF, or handshake timed out
        Ok(Err(_)) => return false,          // read error
        Ok(Ok(n)) if n >= MAX_LINE_BYTES && !line.ends_with('\n') => return false, // oversized, no newline
        Ok(Ok(_)) => {}                      // got a line, proceed as before
    }
    let hello: Hello = match decode_line(&line) {
        Ok(h) => h,
        Err(_) => {
            let reply = HandshakeReply::Rejected {
                server_protocol_version: PROTOCOL_VERSION,
                message: "malformed Hello".into(),
            };
            let _ = writer.write_all(encode_line(&reply).unwrap().as_bytes()).await;
            return false;
        }
    };
    if hello.protocol_version != PROTOCOL_VERSION {
        let reply = HandshakeReply::Rejected {
            server_protocol_version: PROTOCOL_VERSION,
            message: format!(
                "client protocol_version {} != server {PROTOCOL_VERSION}",
                hello.protocol_version
            ),
        };
        let _ = writer.write_all(encode_line(&reply).unwrap().as_bytes()).await;
        return false;
    }
    let _ = writer.write_all(encode_line(&HandshakeReply::Accepted).unwrap().as_bytes()).await;
    true
}

/// Serve one connection end-to-end: handshake, then the async-duplex tick loop (spec §5, §10). A
/// disconnect ends this function; the caller's accept loop then serves the next connection —
/// reconnect resumes the same underlying session because nothing here is per-connection state
/// beyond the socket itself (the character/game state is the one shared, persistent thing).
pub async fn run(
    stream: UnixStream,
    camera: CameraSlots,
    command: CommandState,
    game_state: GameStateSnapshot,
    shared_collision: SharedCollision,
    spells: Arc<SpellDb>,
    net_thread_dead: NetThreadDeadShared,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);
    if !handshake(&mut reader, &mut write_half).await {
        return;
    }

    let latest_step: Arc<Mutex<Option<Step>>> = Arc::new(Mutex::new(None));
    let reader_step = latest_step.clone();
    let reader_task = tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match (&mut reader).take(MAX_LINE_BYTES as u64).read_line(&mut line).await {
                Ok(0) | Err(_) => break, // EOF or read error — connection is done
                // An unbounded line with no newline is a client bad enough to disconnect, not
                // just skip one Step — drop the connection rather than silently truncate/misparse.
                Ok(n) if n >= MAX_LINE_BYTES && !line.ends_with('\n') => break,
                Ok(_) => {
                    if let Ok(step) = decode_line::<Step>(&line) {
                        *reader_step.lock().unwrap() = Some(step);
                    }
                    // A malformed line rejects just that one Step, keeping the connection alive
                    // (spec §11) — there is nothing to reply with here since Observation, not an
                    // ack, is the only outbound message shape.
                }
            }
        }
    });

    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(TICK_MS));
    // A stall (slow write, heavy LOS pass) must not misrepresent elapsed game time to the agent
    // by firing every missed tick back-to-back (`Burst`, the default) — `Delay` just resumes on
    // the normal cadence from whenever the stall ended.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Per-connection, starts at 0 on the first Observation after handshake (spec §9). Incremented
    // once per loop iteration that reaches the point of building an Observation, regardless of
    // whether the subsequent write actually succeeds — a failed write ends the connection (`break`
    // below) rather than retrying this same tick, so there is no double-counting to guard against,
    // and this keeps `tick` a plain count of "Observations this loop attempted to build" rather
    // than something that could skip on a transient write hiccup that didn't end the connection.
    let mut tick: u64 = 0;
    loop {
        ticker.tick().await;
        if reader_task.is_finished() {
            break;
        }
        // Movement re-applies every tick (held-key-style latching, spec §7) but a verb is a
        // separate, one-shot discrete action: drained with `Option::take()` so it fires exactly
        // once per `Step` sent, not once per tick until the next `Step` arrives (a single
        // `Combat::Cast` must not re-fire every 150ms forever).
        let (movement, verb) = {
            let mut slot = latest_step.lock().unwrap();
            let mv = slot.as_ref().and_then(|s| s.movement);
            let vb = slot.as_mut().and_then(|s| s.verb.take());
            (mv, vb)
        };
        if let Some(m) = movement {
            camera.request_manual_move(ManualMove {
                dir: m.dir,
                up: m.up,
                jump: m.jump,
                wish_heading: m.wish_heading,
                until: Instant::now() + MOVE_LATCH,
            });
        }
        if let Some(v) = verb {
            dispatch_verb(&v, &command);
        }
        let gs: arc_swap::Guard<Arc<GameState>> = game_state.load();
        let obs = build_observation(&gs, &shared_collision, &spells, &net_thread_dead, tick);
        tick += 1;
        let line = encode_line(&obs).expect("Observation always serializes");
        // Bounded to a few ticks' width: spec §5 promises eqoxide "never waits" to push an
        // Observation, so a consumer that stops draining its socket buffer (previously ~80s to
        // fill at this cadence, then write_all blocking forever) must be detected within a tick
        // or two, not left to wedge the tick loop (and, since run() would never return, the
        // accept loop too) indefinitely.
        let write_result =
            tokio::time::timeout(Duration::from_millis(TICK_MS * 3), write_half.write_all(line.as_bytes())).await;
        if !matches!(write_result, Ok(Ok(()))) {
            break; // write failed or the client isn't draining fast enough
        }
    }
    reader_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_agent_protocol::movement::AgentMovement;
    use eqoxide_agent_protocol::verb::{CastRequest, MoveVerb};
    use std::sync::{Arc, Mutex};

    #[test]
    fn combat_target_writes_to_the_target_slot() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Combat(CombatVerb::Target { spawn_id: 42 }), &command);
        assert_eq!(command.take_target(), Some(42));
    }

    #[test]
    fn combat_attack_writes_to_the_attack_slot() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Combat(CombatVerb::Attack { on: true }), &command);
        assert_eq!(command.take_attack(), Some(true));
    }

    #[test]
    fn combat_consider_writes_to_the_consider_slot() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Combat(CombatVerb::Consider { spawn_id: 7 }), &command);
        assert_eq!(command.take_consider(), Some(7));
    }

    #[test]
    fn combat_cast_writes_to_the_cast_slot() {
        let command = CommandState::default();
        dispatch_verb(
            &AgentVerb::Combat(CombatVerb::Cast(CastRequest { gem: 0, target_id: Some(9), item_slot: None })),
            &command,
        );
        let cast = command.take_cast().expect("cast queued");
        assert_eq!(cast.gem, 0);
        assert_eq!(cast.target_id, Some(9));
    }

    #[test]
    fn interact_sit_requests_sit_true() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Interact(InteractVerb::Sit), &command);
        assert_eq!(command.take_sit(), Some(true));
    }

    #[test]
    fn interact_stand_requests_sit_false() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Interact(InteractVerb::Stand), &command);
        assert_eq!(command.take_sit(), Some(false));
    }

    #[test]
    fn lifecycle_respawn_does_not_panic() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Lifecycle(LifecycleVerb::Respawn), &command);
    }

    #[test]
    fn reserved_move_zone_cross_is_a_deliberate_no_op() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Move(MoveVerb::ZoneCross), &command);
        assert_eq!(command.take_target(), None, "a reserved verb must not touch any real slot");
    }

    #[test]
    fn apply_step_with_movement_writes_manual_move() {
        let camera = CameraSlots::for_test();
        let command = CommandState::default();
        let step = Step {
            movement: Some(AgentMovement { dir: [1.0, 0.0], up: 0.0, jump: false, wish_heading: Some(90.0) }),
            verb: None,
        };
        apply_step(&step, &camera, &command);
        let m = camera.manual_move.lock().unwrap().expect("manual move queued");
        assert_eq!(m.dir, [1.0, 0.0]);
        assert_eq!(m.wish_heading, Some(90.0));
        assert!(m.until > Instant::now(), "the deadline must be in the future");
    }

    #[test]
    fn apply_step_with_no_movement_leaves_manual_move_slot_untouched() {
        let camera = CameraSlots::for_test();
        let command = CommandState::default();
        let step = Step { movement: None, verb: None };
        apply_step(&step, &camera, &command);
        assert!(camera.manual_move.lock().unwrap().is_none());
    }

    #[test]
    fn apply_step_with_both_movement_and_verb_applies_both() {
        let camera = CameraSlots::for_test();
        let command = CommandState::default();
        let step = Step {
            movement: Some(AgentMovement { dir: [0.0, 1.0], up: 0.0, jump: false, wish_heading: None }),
            verb: Some(AgentVerb::Combat(CombatVerb::Attack { on: true })),
        };
        apply_step(&step, &camera, &command);
        assert!(camera.manual_move.lock().unwrap().is_some());
        assert_eq!(command.take_attack(), Some(true));
    }

    #[tokio::test]
    async fn handshake_accepts_matching_protocol_version() {
        let hello = encode_line(&Hello { protocol_version: PROTOCOL_VERSION }).unwrap();
        let mut reader = tokio::io::BufReader::new(hello.as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let accepted = handshake(&mut reader, &mut writer).await;
        assert!(accepted);
        let reply: HandshakeReply = decode_line(std::str::from_utf8(&writer).unwrap()).unwrap();
        assert_eq!(reply, HandshakeReply::Accepted);
    }

    #[tokio::test]
    async fn handshake_rejects_mismatched_protocol_version() {
        let hello = encode_line(&Hello { protocol_version: PROTOCOL_VERSION + 1 }).unwrap();
        let mut reader = tokio::io::BufReader::new(hello.as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let accepted = handshake(&mut reader, &mut writer).await;
        assert!(!accepted);
        let reply: HandshakeReply = decode_line(std::str::from_utf8(&writer).unwrap()).unwrap();
        match reply {
            HandshakeReply::Rejected { server_protocol_version, .. } => {
                assert_eq!(server_protocol_version, PROTOCOL_VERSION);
            }
            HandshakeReply::Accepted => panic!("a version mismatch must not be accepted"),
        }
    }

    #[tokio::test]
    async fn handshake_rejects_malformed_first_line() {
        let mut reader = tokio::io::BufReader::new("not json\n".as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let accepted = handshake(&mut reader, &mut writer).await;
        assert!(!accepted);
    }

    #[tokio::test]
    async fn handshake_returns_false_on_immediate_eof() {
        let mut reader = tokio::io::BufReader::new(&b""[..]);
        let mut writer: Vec<u8> = Vec::new();
        let accepted = handshake(&mut reader, &mut writer).await;
        assert!(!accepted);
    }

    /// Regression test: a client that connects and never sends `Hello` must not wedge the
    /// handshake (and, transitively, the accept loop) forever. Uses a real `UnixStream::pair()`
    /// whose client half is simply never written to and never dropped — the reachable half of the
    /// bug that matters most for "a stuck connection blocks every later one." `start_paused`
    /// makes the internal 5s timeout resolve in virtual time, so this test doesn't really sleep.
    #[tokio::test(start_paused = true)]
    async fn handshake_times_out_on_a_connection_that_never_sends_hello() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let (read_half, _write_half) = server.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        let mut writer: Vec<u8> = Vec::new();

        let result = tokio::time::timeout(Duration::from_secs(10), handshake(&mut reader, &mut writer)).await;
        assert_eq!(result, Ok(false), "handshake must time out and return false, not hang forever");

        drop(client); // keep the client half alive for the whole wait, then let it go
    }

    /// Regression test: `read_line` has no cap on its own, so a client streaming bytes with no
    /// `\n` would otherwise grow the line buffer unboundedly. A line at/over `MAX_LINE_BYTES` with
    /// no trailing newline must be rejected promptly as malformed/oversized, not hang or keep
    /// growing memory waiting for a newline that never comes.
    #[tokio::test]
    async fn handshake_rejects_an_oversized_line_with_no_newline() {
        let oversized = "x".repeat(MAX_LINE_BYTES + 10); // no trailing '\n'
        let mut reader = tokio::io::BufReader::new(oversized.as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let accepted =
            tokio::time::timeout(Duration::from_secs(5), handshake(&mut reader, &mut writer)).await.unwrap();
        assert!(!accepted, "an oversized line with no newline must be rejected, not hang");
    }

    #[tokio::test]
    async fn run_pushes_an_observation_every_tick_after_handshake() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let camera = CameraSlots::for_test();
        let command = CommandState::default();
        let game_state: GameStateSnapshot = Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        let shared_collision: SharedCollision = Arc::new(std::sync::RwLock::new(None));
        let spells = Arc::new(SpellDb::default());
        let net_thread_dead: NetThreadDeadShared = Arc::new(Mutex::new(None));

        let server_task = tokio::spawn(run(
            server, camera, command, game_state, shared_collision, spells, net_thread_dead,
        ));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(encode_line(&Hello { protocol_version: PROTOCOL_VERSION }).unwrap().as_bytes())
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let reply: HandshakeReply = decode_line(&line).unwrap();
        assert_eq!(reply, HandshakeReply::Accepted);

        // Read 3 ticks' worth of Observations (not just 1) and check the monotonic `tick` counter
        // lands on 0, 1, 2 — proving both "every tick" and that the counter actually increments
        // per-tick rather than staying pinned or resetting.
        for expected_tick in 0..3u64 {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let obs: eqoxide_agent_protocol::observation::Observation = decode_line(&line).unwrap();
            assert_eq!(obs.tick, expected_tick);
        }

        drop(write_half);
        drop(reader);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), server_task).await;
    }

    #[tokio::test]
    async fn run_closes_immediately_on_handshake_rejection() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let camera = CameraSlots::for_test();
        let command = CommandState::default();
        let game_state: GameStateSnapshot = Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        let shared_collision: SharedCollision = Arc::new(std::sync::RwLock::new(None));
        let spells = Arc::new(SpellDb::default());
        let net_thread_dead: NetThreadDeadShared = Arc::new(Mutex::new(None));

        let server_task = tokio::spawn(run(
            server, camera, command, game_state, shared_collision, spells, net_thread_dead,
        ));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(encode_line(&Hello { protocol_version: PROTOCOL_VERSION + 1 }).unwrap().as_bytes())
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let reply: HandshakeReply = decode_line(&line).unwrap();
        assert!(matches!(reply, HandshakeReply::Rejected { .. }));

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), server_task).await;
        assert!(result.is_ok(), "run() must return promptly after a rejected handshake");
    }

    /// Regression test for the Critical bug: a latched `Step`'s verb must dispatch exactly once,
    /// not every tick until a new `Step` arrives (spec §7's movement-latches/verb-is-one-shot
    /// split). Drives the fix through the real socket path (`run()`, not `apply_step` directly) —
    /// also covers spec §13's "a Step reaching action dispatch over the socket" gap (10b).
    #[tokio::test]
    async fn a_steps_verb_dispatches_exactly_once_despite_multiple_ticks() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let camera = CameraSlots::for_test();
        let command = CommandState::default();
        let command_check = command.clone();
        let game_state: GameStateSnapshot = Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        let shared_collision: SharedCollision = Arc::new(std::sync::RwLock::new(None));
        let spells = Arc::new(SpellDb::default());
        let net_thread_dead: NetThreadDeadShared = Arc::new(Mutex::new(None));

        let server_task = tokio::spawn(run(
            server, camera, command, game_state, shared_collision, spells, net_thread_dead,
        ));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(encode_line(&Hello { protocol_version: PROTOCOL_VERSION }).unwrap().as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let reply: HandshakeReply = decode_line(&line).unwrap();
        assert_eq!(reply, HandshakeReply::Accepted);

        let step = Step {
            movement: None,
            verb: Some(AgentVerb::Combat(CombatVerb::Target { spawn_id: 42 })),
        };
        write_half.write_all(encode_line(&step).unwrap().as_bytes()).await.unwrap();

        // Read several observations in a row, proving several ticks elapsed and giving the
        // latched Step every opportunity to (incorrectly) re-dispatch if the bug were still there.
        for _ in 0..3 {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let _obs: eqoxide_agent_protocol::observation::Observation = decode_line(&line).unwrap();
        }

        assert_eq!(
            command_check.take_target(),
            Some(42),
            "the verb must have dispatched at least once by now"
        );

        // A buggy re-dispatch-every-tick implementation would refill the slot on the NEXT tick,
        // not synchronously — so the second check must let at least one more tick actually elapse
        // (reading another Observation is the real-time proof of that, not a fixed sleep) before
        // asserting the slot stayed empty. Two back-to-back `take_target()` calls with no tick in
        // between would pass even with the bug present, since nothing would have re-run yet.
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let _obs: eqoxide_agent_protocol::observation::Observation = decode_line(&line).unwrap();

        assert_eq!(
            command_check.take_target(),
            None,
            "a second take_target(), after another tick elapsed, must be empty — the verb must \
             not re-dispatch every tick"
        );

        drop(write_half);
        drop(reader);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), server_task).await;
    }
}
