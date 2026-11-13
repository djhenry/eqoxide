//! Per-connection handshake, tick loop, and action dispatch (spec §5, §7, §8).

use crate::observation_builder::build_observation;
use eqoxide_agent_protocol::call::{Call, CallReply, CallStatus};
use eqoxide_agent_protocol::framing::{decode_line, encode_line};
use eqoxide_agent_protocol::handshake::{HandshakeReply, Hello, PROTOCOL_VERSION};
use eqoxide_agent_protocol::movement::AgentMovement;
use eqoxide_agent_protocol::step::Step;
use eqoxide_agent_protocol::verb::{
    AgentVerb, ChatVerb, CombatVerb, InteractVerb, InventoryVerb, LifecycleVerb, PetCommand, PetVerb,
};
use eqoxide_agent_protocol::verb_result::{VerbOutcome, VerbResult};
use eqoxide_command::CommandState;
use eqoxide_core::game_state::GameState;
use eqoxide_core::spells::SpellDb;
use eqoxide_ipc::{CameraSlots, GameStateSnapshot, ManualMove, NetThreadDeadShared};
use eqoxide_zone_geometry::collision::SharedCollision;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

/// Re-issued every tick (~2x the 150ms tick, `MOVE_LATCH`), so movement naturally stops within
/// ~300ms of the agent going quiet — the same fail-safe deadline mechanism `ManualMove` already
/// gives the op's `/v1/move/manual`, applied here every tick instead of once per request.
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

/// Most verbs that may wait for a tick on one connection.
const MAX_QUEUED_VERBS: usize = 16;

/// Most undelivered results kept for one connection. A client that never reads its Observations is
/// disconnected by the write timeout long before this matters; the cap only bounds memory.
const MAX_QUEUED_RESULTS: usize = 64;

/// Most calls that may be running at once on one connection. A call past this is answered
/// `Unavailable` at once rather than queued, so a flood cannot pile up unbounded work.
const MAX_CALLS_IN_FLIGHT: usize = 32;

/// Runs one `Call` to completion and says how it went. Supplied by whoever embeds the host
/// (eqoxide's binary), which owns the handlers; the host only schedules and answers.
pub type CallHandler = Arc<
    dyn Fn(
            Call,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = (CallStatus, serde_json::Value)> + Send>,
        > + Send
        + Sync,
>;

/// Calls running now and the replies of those that finished, awaiting the next `Observation`.
#[derive(Default)]
struct CallBoard {
    in_flight: usize,
    replies: Vec<CallReply>,
}

/// Start `call` on its own task, or answer it `Unavailable` if too many are already running. Each
/// call gets exactly one reply.
fn submit_call(board: &Arc<Mutex<CallBoard>>, handler: &CallHandler, call: Call) {
    {
        let mut b = board.lock().unwrap();
        if b.in_flight >= MAX_CALLS_IN_FLIGHT {
            b.replies.push(CallReply {
                id: call.id,
                status: CallStatus::Unavailable,
                body: "too many calls are running; this one was not started".into(),
            });
            return;
        }
        b.in_flight += 1;
    }
    let board = board.clone();
    let id = call.id;
    let fut = handler(call);
    tokio::spawn(async move {
        let (status, body) = fut.await;
        let mut b = board.lock().unwrap();
        b.in_flight -= 1;
        b.replies.push(CallReply { id, status, body });
    });
}

/// Verbs read off the socket and not yet dispatched, plus results already decided at intake (a
/// verb refused before it could queue). The tick loop dispatches one verb per tick, oldest first,
/// so a verb is never silently displaced by the next one.
#[derive(Default)]
struct VerbIntake {
    queue: std::collections::VecDeque<(AgentVerb, Option<u64>)>,
    results: Vec<VerbResult>,
}

impl VerbIntake {
    fn refuse(&mut self, seq: Option<u64>, why: &str) {
        tracing::warn!("agent-plugin-host: refused a verb at intake: {why}");
        if let Some(seq) = seq {
            if self.results.len() >= MAX_QUEUED_RESULTS {
                self.results.remove(0);
            }
            self.results.push(VerbResult {
                seq,
                outcome: VerbOutcome::Refused(why.into()),
            });
        }
    }

    /// Takes in the verb of one `Step`.
    fn submit(&mut self, verb: Option<AgentVerb>, seq: Option<u64>) {
        match verb {
            None => {
                if seq.is_some() {
                    self.refuse(seq, "the Step carried a seq but no verb");
                }
            }
            Some(_) if self.queue.len() >= MAX_QUEUED_VERBS => {
                self.refuse(seq, "too many verbs are waiting; this one was dropped");
            }
            Some(v) => self.queue.push_back((v, seq)),
        }
    }
}

/// Gate `verb` on the session and the character, then dispatch it.
///
/// Mirrors `eqoxide-agent-ops`: a dead network thread refuses every write (`require_live_session`),
/// while a dead character is refused movement only, which is not a verb. Respawning a character
/// that is not confirmed dead is refused rather than answered with a no-op "ok" (the route's old
/// 200 for that case is the #952/#1002 falsehood class). The window where HP has halted but no
/// `OP_Death` has arrived is not "dead" here either, matching #1000: respawn is not its remedy.
pub fn answer_verb(
    verb: &AgentVerb,
    command: &CommandState,
    gs: &GameState,
    net_thread_dead: Option<&str>,
) -> VerbOutcome {
    // Tearing a dead session down is `Exit`'s whole job, so it is the one verb a dead network thread
    // does not refuse.
    if matches!(verb, AgentVerb::Lifecycle(LifecycleVerb::Exit)) {
        return dispatch_verb(verb, command, gs);
    }
    if let Some(reason) = net_thread_dead {
        return VerbOutcome::Refused(format!(
            "the network thread is not running: {reason} The verb was NOT sent and will not take effect."
        ));
    }
    if matches!(verb, AgentVerb::Lifecycle(LifecycleVerb::Respawn)) && !gs.player_dead {
        return VerbOutcome::Refused(
            "the character is not confirmed dead; there is nothing to respawn from".into(),
        );
    }
    dispatch_verb(verb, command, gs)
}

/// Translate one `AgentVerb` into the real `CommandState` call it mirrors, and say whether it was
/// taken.
pub fn dispatch_verb(verb: &AgentVerb, command: &CommandState, gs: &GameState) -> VerbOutcome {
    match verb {
        AgentVerb::Combat(CombatVerb::Target { spawn_id }) => {
            command.request_target(*spawn_id);
            VerbOutcome::Accepted
        }
        AgentVerb::Combat(CombatVerb::Attack { on }) => {
            command.request_attack(*on);
            VerbOutcome::Accepted
        }
        AgentVerb::Combat(CombatVerb::Consider { spawn_id }) => {
            command.request_consider(*spawn_id);
            VerbOutcome::Accepted
        }
        AgentVerb::Combat(CombatVerb::Cast(c)) => {
            // Mirrors the agent API's own gem bound (`eqoxide-agent-ops/src/combat.rs`'s `post_cast`,
            // `gem > 8` -> 400 Bad Request) — the gem slots are a fixed 9-wide array
            // (`GameState::mem_spells`), so this keeps both entry points to casting agreeing on
            // what a valid gem is instead of one validating and the other silently accepting
            // anything.
            if c.gem > 8 {
                tracing::warn!(
                    "agent-plugin-host: rejected a Cast with out-of-range gem {}",
                    c.gem
                );
                return VerbOutcome::Refused(format!(
                    "gem {} is out of range; gems are 0-8",
                    c.gem
                ));
            }
            command.request_cast(eqoxide_ipc::CastRequest {
                gem: c.gem,
                target_id: c.target_id,
                item_slot: c.item_slot,
            });
            VerbOutcome::Accepted
        }
        AgentVerb::Interact(InteractVerb::Sit) => {
            command.request_sit(true);
            VerbOutcome::Accepted
        }
        AgentVerb::Interact(InteractVerb::Stand) => {
            command.request_sit(false);
            VerbOutcome::Accepted
        }
        AgentVerb::Lifecycle(LifecycleVerb::Respawn) => {
            command.request_respawn();
            VerbOutcome::Accepted
        }
        AgentVerb::Lifecycle(LifecycleVerb::Camp) => {
            command.request_camp(eqoxide_ipc::CampCmd::Toggle);
            VerbOutcome::Accepted
        }
        AgentVerb::Lifecycle(LifecycleVerb::Exit) => {
            command.request_camp(eqoxide_ipc::CampCmd::Start);
            VerbOutcome::Accepted
        }
        AgentVerb::Inventory(InventoryVerb::Move { from, to }) => {
            if !gs.inventory.iter().any(|i| i.slot == *from as i32) {
                return VerbOutcome::Refused(format!("no item in slot {from} to move"));
            }
            if !command.request_inventory_move(*from, *to) {
                return VerbOutcome::Refused(
                    "an inventory move is already queued and undrained — retry in a moment (it was NOT queued)".into(),
                );
            }
            VerbOutcome::Accepted
        }
        AgentVerb::Inventory(InventoryVerb::Destroy { slot }) => dispatch_destroy(*slot, command, gs),
        AgentVerb::Inventory(InventoryVerb::Consume { slot }) => dispatch_consume(*slot, command, gs),
        AgentVerb::Pet(PetVerb::Command { command: c }) => {
            if !command.request_pet_command(pet_command_byte(*c)) {
                return VerbOutcome::Refused(
                    "a pet command is already queued and undrained — retry in a moment (it was NOT queued)".into(),
                );
            }
            VerbOutcome::Accepted
        }
        AgentVerb::Chat(v) => dispatch_chat(v, command),
    }
}

/// Cursor wire slot (RoF2).
const CURSOR_SLOT: i32 = eqoxide_core::game_state::SLOT_CURSOR;

/// Queue a destroy, refusing every case the net thread would otherwise drop without a word: a dead
/// character, an empty slot, a busy cursor (the item is picked up to the cursor first, which would
/// swap a held item into the slot), a bag that still holds items, or a destroy already queued.
fn dispatch_destroy(slot: u32, command: &CommandState, gs: &GameState) -> VerbOutcome {
    if gs.is_player_dead() {
        return VerbOutcome::Refused("the character is dead; it cannot destroy items".into());
    }
    let Some(item) = gs.inventory.iter().find(|i| i.slot == slot as i32) else {
        return VerbOutcome::Refused(format!("no item in slot {slot} to destroy"));
    };
    if slot as i32 != CURSOR_SLOT && gs.inventory.iter().any(|i| i.slot == CURSOR_SLOT) {
        return VerbOutcome::Refused(
            "the cursor holds an item; destroy picks the item up to the cursor, so the cursor must be empty".into(),
        );
    }
    if (23..=32).contains(&(slot as i32)) {
        let first = eqoxide_core::game_state::BAG_SLOTS_BEGIN + (slot as i32 - 23) * 10;
        if gs.inventory.iter().any(|i| (first..first + 10).contains(&i.slot)) {
            return VerbOutcome::Refused(format!(
                "{} in slot {slot} is a bag that still holds items; empty it first",
                item.name
            ));
        }
    }
    if !command.request_inventory_destroy(slot) {
        return VerbOutcome::Refused(
            "an inventory destroy is already queued and undrained — retry in a moment (it was NOT queued)".into(),
        );
    }
    VerbOutcome::Accepted
}

/// Queue a consume, refusing a dead character, an empty slot, an item that is not food or drink,
/// or a consume already queued.
fn dispatch_consume(slot: u32, command: &CommandState, gs: &GameState) -> VerbOutcome {
    if gs.is_player_dead() {
        return VerbOutcome::Refused("the character is dead; it cannot eat or drink".into());
    }
    let Some(item) = gs.inventory.iter().find(|i| i.slot == slot as i32) else {
        return VerbOutcome::Refused(format!("no item in slot {slot} to consume"));
    };
    if !item.is_consumable() {
        return VerbOutcome::Refused(format!(
            "{} in slot {slot} is not food or drink (item type {})",
            item.name, item.item_type
        ));
    }
    if !command.request_inventory_consume(slot) {
        return VerbOutcome::Refused(
            "an inventory consume is already queued and undrained — retry in a moment (it was NOT queued)".into(),
        );
    }
    VerbOutcome::Accepted
}

/// How long the exit watchdog waits before force-exiting the process. It must outlast the camp
/// (about 30 s) so it never kills mid-camp, which would leave the character linkdead.
const EXIT_WATCHDOG: Duration = Duration::from_secs(45);

/// Why the watchdog killed the process, keyed on how the network thread ended — a dead thread
/// cannot drain the camp, so claiming a generic timeout would misreport it.
fn watchdog_reason(end: Option<eqoxide_ipc::NetThreadEnd>) -> &'static str {
    use eqoxide_ipc::NetThreadEnd;
    match end {
        Some(NetThreadEnd::NeverStarted) => "net-thread-never-started",
        Some(NetThreadEnd::Panicked)
        | Some(NetThreadEnd::Fatal)
        | Some(NetThreadEnd::ReturnedUnexpectedly) => "net-thread-dead",
        Some(NetThreadEnd::ReturnedDuringShutdown) => "shutdown-not-completed",
        None => "watchdog-shutdown-timeout",
    }
}

/// Last resort for `Lifecycle::Exit`: if the camp-out has not ended the process after
/// [`EXIT_WATCHDOG`], force the exit with a labelled reason (an unlabelled exit would be read as an
/// OOM kill in the crash post-mortem).
fn spawn_exit_watchdog(net_thread_dead: NetThreadDeadShared) {
    std::thread::spawn(move || {
        std::thread::sleep(EXIT_WATCHDOG);
        let end = net_thread_dead
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|d| d.end());
        let reason = watchdog_reason(end);
        tracing::warn!("exit: watchdog timeout — forcing process exit (reason={reason})");
        eqoxide_crash::exit(reason, 0)
    });
}

/// Queue one chat message, refusing a blank recipient or text the way `POST /v1/chat/*` did.
/// The OP_PetCommands byte for a [`PetCommand`] (EQEmu `zone/common.h`, via `eqoxide_core::pet`).
fn pet_command_byte(c: PetCommand) -> u8 {
    use eqoxide_core::pet::{PET_ATTACK, PET_BACKOFF, PET_FOLLOWME, PET_GUARDHERE, PET_SIT};
    (match c {
        PetCommand::Attack => PET_ATTACK,
        PetCommand::Follow => PET_FOLLOWME,
        PetCommand::Guard => PET_GUARDHERE,
        PetCommand::SitToggle => PET_SIT,
        PetCommand::BackOff => PET_BACKOFF,
    }) as u8
}

fn dispatch_chat(verb: &ChatVerb, command: &CommandState) -> VerbOutcome {
    let (chan, to, text) = match verb {
        ChatVerb::Tell { to, text } => (7, to.as_str(), text),
        ChatVerb::Ooc { text } => (5, "", text),
        ChatVerb::Shout { text } => (3, "", text),
        ChatVerb::Group { text } => (2, "", text),
        ChatVerb::Guild { text } => (0, "", text),
    };
    if text.trim().is_empty() || (chan == 7 && to.trim().is_empty()) {
        return VerbOutcome::Refused(if chan == 7 {
            "tell requires non-empty 'to' and 'text'".into()
        } else {
            "a chat message requires non-empty 'text'".into()
        });
    }
    command.request_chat_send(eqoxide_ipc::ChatSend { chan, to: to.to_string(), text: text.clone() });
    VerbOutcome::Accepted
}

/// Read the client's `Hello`, reply `Accepted`/`Rejected`, and report which happened. The caller
/// closes the connection immediately on `false` (spec §5, §11).
pub async fn handshake<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    asset_cache_dir: &str,
) -> bool {
    let mut line = String::new();
    let read = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        (&mut *reader)
            .take(MAX_LINE_BYTES as u64)
            .read_line(&mut line),
    )
    .await;
    match read {
        Ok(Ok(0)) | Err(_) => return false, // EOF, or handshake timed out
        Ok(Err(_)) => return false,         // read error
        Ok(Ok(n)) if n >= MAX_LINE_BYTES && !line.ends_with('\n') => return false, // oversized, no newline
        Ok(Ok(_)) => {} // got a line, proceed as before
    }
    let hello: Hello = match decode_line(&line) {
        Ok(h) => h,
        Err(_) => {
            let reply = HandshakeReply::Rejected {
                server_protocol_version: PROTOCOL_VERSION,
                message: "malformed Hello".into(),
            };
            let _ = writer
                .write_all(encode_line(&reply).unwrap().as_bytes())
                .await;
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
        let _ = writer
            .write_all(encode_line(&reply).unwrap().as_bytes())
            .await;
        return false;
    }
    let _ = writer
        .write_all(
            encode_line(&HandshakeReply::Accepted {
                asset_cache_dir: asset_cache_dir.to_string(),
            })
            .unwrap()
            .as_bytes(),
        )
        .await;
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
    asset_cache_dir: String,
    calls: CallHandler,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);
    if !handshake(&mut reader, &mut write_half, &asset_cache_dir).await {
        return;
    }

    // Movement and verb are latched independently, not as a shared `Step` snapshot: a `Step`
    // carrying only a verb (spec §7 — "a verb with no movement change") must not clear movement an
    // earlier `Step` latched, so each field updates only when its own `Step` actually sets it, not
    // whenever any `Step` arrives.
    let latched_movement: Arc<Mutex<Option<AgentMovement>>> = Arc::new(Mutex::new(None));
    let intake: Arc<Mutex<VerbIntake>> = Arc::new(Mutex::new(VerbIntake::default()));
    let reader_movement = latched_movement.clone();
    let reader_verb = intake.clone();
    let call_board: Arc<Mutex<CallBoard>> = Arc::new(Mutex::new(CallBoard::default()));
    let reader_board = call_board.clone();
    let reader_task = tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match (&mut reader)
                .take(MAX_LINE_BYTES as u64)
                .read_line(&mut line)
                .await
            {
                Ok(0) | Err(_) => break, // EOF or read error — connection is done
                // An unbounded line with no newline is a client bad enough to disconnect, not
                // just skip one Step — drop the connection rather than silently truncate/misparse.
                Ok(n) if n >= MAX_LINE_BYTES && !line.ends_with('\n') => break,
                Ok(_) => {
                    if let Ok(step) = decode_line::<Step>(&line) {
                        match step.movement {
                            // A non-finite dir/up/wish_heading (overflowed from the wire's f64, or
                            // sent as NaN directly) would otherwise flow straight into the
                            // character controller's position integration every tick, permanently
                            // corrupting GameState's position from that tick onward — reject just
                            // the movement, matching the existing "a malformed Step rejects only
                            // that Step" policy below, rather than latching a poisoned value.
                            Some(m) if m.is_finite() => {
                                *reader_movement.lock().unwrap() = Some(m);
                            }
                            Some(_) => {
                                tracing::warn!(
                                    "agent-plugin-host: rejected a Step with a non-finite movement field"
                                );
                            }
                            None => {}
                        }
                        reader_verb.lock().unwrap().submit(step.verb, step.seq);
                        for call in step.calls {
                            submit_call(&reader_board, &calls, call);
                        }
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
        // separate, one-shot discrete action: popped off the intake queue so it fires exactly
        // once per `Step` sent, not once per tick until the next `Step` arrives (a single
        // `Combat::Cast` must not re-fire every 150ms forever).
        let movement = *latched_movement.lock().unwrap();
        let (verb, mut verb_results) = {
            let mut intake = intake.lock().unwrap();
            (
                intake.queue.pop_front(),
                std::mem::take(&mut intake.results),
            )
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
        let gs: arc_swap::Guard<Arc<GameState>> = game_state.load();
        if let Some((v, seq)) = verb {
            let dead_reason = net_thread_dead
                .lock()
                .unwrap()
                .as_ref()
                .map(|d| d.reason().to_string());
            let outcome = answer_verb(&v, &command, &gs, dead_reason.as_deref());
            if matches!(v, AgentVerb::Lifecycle(LifecycleVerb::Exit))
                && outcome == VerbOutcome::Accepted
            {
                spawn_exit_watchdog(net_thread_dead.clone());
            }
            if let VerbOutcome::Refused(why) = &outcome {
                tracing::warn!("agent-plugin-host: refused {v:?}: {why}");
            }
            if let Some(seq) = seq {
                verb_results.push(VerbResult { seq, outcome });
            }
        }
        let mut obs = build_observation(&gs, &shared_collision, &spells, &net_thread_dead, tick);
        obs.verb_results = verb_results;
        obs.call_replies = std::mem::take(&mut call_board.lock().unwrap().replies);
        tick += 1;
        let line = encode_line(&obs).expect("Observation always serializes");
        // Bounded to a few ticks' width: spec §5 promises eqoxide "never waits" to push an
        // Observation, so a consumer that stops draining its socket buffer (previously ~80s to
        // fill at this cadence, then write_all blocking forever) must be detected within a tick
        // or two, not left to wedge the tick loop (and, since run() would never return, the
        // accept loop too) indefinitely.
        let write_result = tokio::time::timeout(
            Duration::from_millis(TICK_MS * 3),
            write_half.write_all(line.as_bytes()),
        )
        .await;
        if !matches!(write_result, Ok(Ok(()))) {
            // `write_all` is not cancel-safe: a timeout can fire mid-write, leaving a truncated
            // line already flushed. `break` (not `continue`) is load-bearing here — it ends the
            // connection immediately so `write_half`'s drop closes the write side, and the client
            // sees EOF right after the fragment instead of a second write interleaving with it.
            break;
        }
    }
    reader_task.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CACHE_DIR: &str = "/cache/models";
    use eqoxide_agent_protocol::movement::AgentMovement;
    use eqoxide_agent_protocol::verb::CastRequest;
    use std::sync::{Arc, Mutex};

    #[test]
    fn combat_target_writes_to_the_target_slot() {
        let command = CommandState::default();
        dispatch_verb(
            &AgentVerb::Combat(CombatVerb::Target { spawn_id: 42 }),
            &command,
            &GameState::default(),
        );
        assert_eq!(command.take_target(), Some(42));
    }

    #[test]
    fn combat_attack_writes_to_the_attack_slot() {
        let command = CommandState::default();
        dispatch_verb(
            &AgentVerb::Combat(CombatVerb::Attack { on: true }),
            &command,
            &GameState::default(),
        );
        assert_eq!(command.take_attack(), Some(true));
    }

    #[test]
    fn combat_consider_writes_to_the_consider_slot() {
        let command = CommandState::default();
        dispatch_verb(
            &AgentVerb::Combat(CombatVerb::Consider { spawn_id: 7 }),
            &command,
            &GameState::default(),
        );
        assert_eq!(command.take_consider(), Some(7));
    }

    #[test]
    fn combat_cast_writes_to_the_cast_slot() {
        let command = CommandState::default();
        dispatch_verb(
            &AgentVerb::Combat(CombatVerb::Cast(CastRequest {
                gem: 0,
                target_id: Some(9),
                item_slot: None,
            })),
            &command,
            &GameState::default(),
        );
        let cast = command.take_cast().expect("cast queued");
        assert_eq!(cast.gem, 0);
        assert_eq!(cast.target_id, Some(9));
    }

    /// Mirrors the agent API's own `gem > 8` rejection (`eqoxide-agent-ops/src/combat.rs`'s
    /// `post_cast`) — `GameState::mem_spells` is a fixed 9-wide array, so an out-of-range gem is
    /// rejected the same way on both entry points instead of only on one.
    #[test]
    fn combat_cast_with_an_out_of_range_gem_is_rejected() {
        let command = CommandState::default();
        dispatch_verb(
            &AgentVerb::Combat(CombatVerb::Cast(CastRequest {
                gem: 9,
                target_id: Some(9),
                item_slot: None,
            })),
            &command,
            &GameState::default(),
        );
        assert!(
            command.take_cast().is_none(),
            "an out-of-range gem must never reach the cast slot"
        );
    }

    #[test]
    fn a_dead_character_can_still_cancel_attack_and_respawn() {
        let command = CommandState::default();
        let dead = GameState {
            player_dead: true,
            ..Default::default()
        };
        let off = AgentVerb::Combat(CombatVerb::Attack { on: false });
        assert_eq!(
            answer_verb(&off, &command, &dead, None),
            VerbOutcome::Accepted
        );
        assert_eq!(command.take_attack(), Some(false));
        let respawn = AgentVerb::Lifecycle(LifecycleVerb::Respawn);
        assert_eq!(
            answer_verb(&respawn, &command, &dead, None),
            VerbOutcome::Accepted
        );
    }

    #[test]
    fn chat_verbs_queue_on_their_channel_and_refuse_blank_text() {
        let command = CommandState::default();
        let gs = GameState::default();
        let sends = [
            (ChatVerb::Tell { to: "Alpha".into(), text: "hi".into() }, 7, "Alpha"),
            (ChatVerb::Ooc { text: "hi".into() }, 5, ""),
            (ChatVerb::Shout { text: "hi".into() }, 3, ""),
            (ChatVerb::Group { text: "hi".into() }, 2, ""),
            (ChatVerb::Guild { text: "hi".into() }, 0, ""),
        ];
        for (verb, _, _) in &sends {
            assert_eq!(
                answer_verb(&AgentVerb::Chat(verb.clone()), &command, &gs, None),
                VerbOutcome::Accepted
            );
        }
        let queued = command.take_chat_send();
        assert_eq!(queued.len(), 5);
        for (q, (_, chan, to)) in queued.iter().zip(&sends) {
            assert_eq!((q.chan, q.to.as_str(), q.text.as_str()), (*chan, *to, "hi"));
        }
        for verb in [
            ChatVerb::Tell { to: " ".into(), text: "hi".into() },
            ChatVerb::Tell { to: "Alpha".into(), text: "".into() },
            ChatVerb::Ooc { text: "  ".into() },
            ChatVerb::Guild { text: "".into() },
        ] {
            assert!(matches!(
                answer_verb(&AgentVerb::Chat(verb), &command, &gs, None),
                VerbOutcome::Refused(_)
            ));
        }
        assert!(command.take_chat_send().is_empty(), "a refused chat verb must not queue");
    }

    #[test]
    fn pet_command_queues_once_and_refuses_a_second_before_the_drain() {
        let command = CommandState::default();
        let gs = GameState::default();
        let attack = AgentVerb::Pet(PetVerb::Command { command: PetCommand::Attack });
        let follow = AgentVerb::Pet(PetVerb::Command { command: PetCommand::Follow });
        assert_eq!(answer_verb(&attack, &command, &gs, None), VerbOutcome::Accepted);
        assert!(matches!(answer_verb(&follow, &command, &gs, None), VerbOutcome::Refused(_)));
        assert_eq!(command.take_pet_command(), Some(2), "the first command must survive");
        assert_eq!(command.take_pet_command(), None);
    }

    #[test]
    fn every_pet_command_maps_to_its_wire_byte() {
        for (c, byte) in [
            (PetCommand::Attack, 2),
            (PetCommand::Follow, 4),
            (PetCommand::Guard, 5),
            (PetCommand::SitToggle, 6),
            (PetCommand::BackOff, 28),
        ] {
            assert_eq!(pet_command_byte(c), byte, "{c:?}");
        }
    }

    #[test]
    fn inventory_move_is_refused_from_an_empty_slot_and_when_the_slot_is_busy() {
        let command = CommandState::default();
        let mut gs = GameState::default();
        let mv = AgentVerb::Inventory(InventoryVerb::Move { from: 23, to: 30 });
        assert!(matches!(answer_verb(&mv, &command, &gs, None), VerbOutcome::Refused(_)));
        assert_eq!(command.take_inventory_move(), None, "a refused move must not queue");
        gs.inventory.push(eqoxide_core::game_state::InvItem {
            slot: 23,
            item_id: 13073,
            name: "Bone Chips".into(),
            charges: 1,
            ..Default::default()
        });
        assert_eq!(answer_verb(&mv, &command, &gs, None), VerbOutcome::Accepted);
        assert!(matches!(answer_verb(&mv, &command, &gs, None), VerbOutcome::Refused(_)));
        assert_eq!(command.take_inventory_move(), Some((23, 30)));
    }

    fn gs_with(slot: i32, name: &str, item_type: u8) -> GameState {
        let mut gs = GameState::default();
        gs.inventory.push(eqoxide_core::game_state::InvItem {
            slot,
            item_id: 1,
            name: name.into(),
            charges: 1,
            item_type,
            ..Default::default()
        });
        gs
    }

    fn refusal(o: VerbOutcome) -> String {
        match o {
            VerbOutcome::Refused(r) => r,
            VerbOutcome::Accepted => panic!("expected a refusal"),
        }
    }

    #[test]
    fn inventory_destroy_dispatches_and_refuses_every_unsendable_case() {
        let destroy = AgentVerb::Inventory(InventoryVerb::Destroy { slot: 24 });

        // empty slot
        let command = CommandState::default();
        assert!(refusal(dispatch_verb(&destroy, &command, &GameState::default())).contains("no item in slot 24"));
        assert_eq!(command.take_inventory_destroy(), None);

        // dead
        let mut gs = gs_with(24, "Rusty Dagger", 0);
        gs.player_dead = true;
        assert!(refusal(dispatch_verb(&destroy, &command, &gs)).contains("dead"));
        assert_eq!(command.take_inventory_destroy(), None);

        // busy cursor
        let mut gs = gs_with(24, "Rusty Dagger", 0);
        gs.inventory.push(eqoxide_core::game_state::InvItem { slot: 33, item_id: 2, ..Default::default() });
        assert!(refusal(dispatch_verb(&destroy, &command, &gs)).contains("cursor"));
        assert_eq!(command.take_inventory_destroy(), None);

        // bag with contents (bag in 23 holds an item at flat slot 251)
        let mut gs = gs_with(23, "Small Bag", 0);
        gs.inventory.push(eqoxide_core::game_state::InvItem { slot: 251, item_id: 3, ..Default::default() });
        let bag = AgentVerb::Inventory(InventoryVerb::Destroy { slot: 23 });
        assert!(refusal(dispatch_verb(&bag, &command, &gs)).contains("still holds items"));
        assert_eq!(command.take_inventory_destroy(), None);

        // accepted, then a second is refused as already queued
        let gs = gs_with(24, "Rusty Dagger", 0);
        assert_eq!(dispatch_verb(&destroy, &command, &gs), VerbOutcome::Accepted);
        assert!(refusal(dispatch_verb(&destroy, &command, &gs)).contains("already queued"));
        assert_eq!(command.take_inventory_destroy(), Some(24));

        // an item on the cursor itself is destroyable
        let gs = gs_with(33, "Rusty Dagger", 0);
        let cursor = AgentVerb::Inventory(InventoryVerb::Destroy { slot: 33 });
        assert_eq!(dispatch_verb(&cursor, &command, &gs), VerbOutcome::Accepted);
    }

    #[test]
    fn inventory_consume_dispatches_and_refuses_every_unsendable_case() {
        let consume = AgentVerb::Inventory(InventoryVerb::Consume { slot: 25 });

        let command = CommandState::default();
        assert!(refusal(dispatch_verb(&consume, &command, &GameState::default())).contains("no item in slot 25"));

        let gs = gs_with(25, "Rusty Dagger", 0);
        let r = refusal(dispatch_verb(&consume, &command, &gs));
        assert!(r.contains("not food or drink") && r.contains("Rusty Dagger"), "{r}");

        let mut gs = gs_with(25, "Bread", eqoxide_core::game_state::ITEM_TYPE_FOOD);
        gs.player_dead = true;
        assert!(refusal(dispatch_verb(&consume, &command, &gs)).contains("dead"));
        assert_eq!(command.take_inventory_consume(), None, "no refusal may queue");

        let gs = gs_with(25, "Bread", eqoxide_core::game_state::ITEM_TYPE_FOOD);
        assert_eq!(dispatch_verb(&consume, &command, &gs), VerbOutcome::Accepted);
        assert!(refusal(dispatch_verb(&consume, &command, &gs)).contains("already queued"));
        assert_eq!(command.take_inventory_consume(), Some(25));

        let gs = gs_with(25, "Water Flask", eqoxide_core::game_state::ITEM_TYPE_DRINK);
        assert_eq!(dispatch_verb(&consume, &command, &gs), VerbOutcome::Accepted);
    }

    #[test]
    fn camp_is_refused_on_a_dead_session_but_exit_is_not() {
        let command = CommandState::default();
        let gs = GameState::default();
        let camp = AgentVerb::Lifecycle(LifecycleVerb::Camp);
        let exit = AgentVerb::Lifecycle(LifecycleVerb::Exit);
        assert!(matches!(
            answer_verb(&camp, &command, &gs, Some("panicked")),
            VerbOutcome::Refused(_)
        ));
        assert_eq!(answer_verb(&exit, &command, &gs, Some("panicked")), VerbOutcome::Accepted);
    }

    #[test]
    fn respawning_a_character_not_confirmed_dead_is_refused_not_answered_ok() {
        let command = CommandState::default();
        let respawn = AgentVerb::Lifecycle(LifecycleVerb::Respawn);
        assert!(matches!(
            answer_verb(&respawn, &command, &GameState::default(), None),
            VerbOutcome::Refused(_)
        ));
        // HP halted but no OP_Death: not dead (#1000), so respawn is refused and attack is not.
        let halted = GameState {
            cur_hp: 0,
            max_hp: 100,
            ..Default::default()
        };
        assert!(
            halted.is_player_dead() && !halted.player_dead,
            "precondition: the unconfirmed window"
        );
        assert!(matches!(
            answer_verb(&respawn, &command, &halted, None),
            VerbOutcome::Refused(_)
        ));
        let attack = AgentVerb::Combat(CombatVerb::Attack { on: true });
        assert_eq!(
            answer_verb(&attack, &command, &halted, None),
            VerbOutcome::Accepted
        );
    }

    #[test]
    fn a_dead_net_thread_refuses_every_verb_and_sends_nothing() {
        let command = CommandState::default();
        let attack = AgentVerb::Combat(CombatVerb::Attack { on: true });
        let r = answer_verb(
            &attack,
            &command,
            &GameState::default(),
            Some("it panicked."),
        );
        assert!(
            matches!(&r, VerbOutcome::Refused(m) if m.contains("it panicked.")),
            "{r:?}"
        );
        assert_eq!(command.take_attack(), None);
    }

    #[test]
    fn intake_keeps_every_verb_in_order_and_refuses_overflow_and_a_seq_with_no_verb() {
        let mut i = VerbIntake::default();
        let t = AgentVerb::Combat(CombatVerb::Target { spawn_id: 1 });
        let a = AgentVerb::Combat(CombatVerb::Attack { on: true });
        i.submit(Some(t.clone()), Some(1));
        i.submit(Some(a.clone()), None);
        assert_eq!(
            i.queue
                .iter()
                .map(|(v, s)| (v.clone(), *s))
                .collect::<Vec<_>>(),
            vec![(t.clone(), Some(1)), (a, None)]
        );
        i.submit(None, Some(9));
        assert!(matches!(
            &i.results[..],
            [VerbResult {
                seq: 9,
                outcome: VerbOutcome::Refused(_)
            }]
        ));
        i.submit(None, None);
        assert_eq!(i.results.len(), 1, "no verb and no seq: nothing to answer");
        for n in 0..MAX_QUEUED_VERBS {
            i.submit(Some(t.clone()), Some(100 + n as u64));
        }
        assert_eq!(i.queue.len(), MAX_QUEUED_VERBS);
        assert!(
            i.results
                .iter()
                .any(|r| r.seq == 100 + (MAX_QUEUED_VERBS as u64 - 1)),
            "overflow answered"
        );
    }

    #[test]
    fn outcomes_say_what_dispatch_did() {
        let command = CommandState::default();
        let bad = AgentVerb::Combat(CombatVerb::Cast(CastRequest {
            gem: 9,
            target_id: None,
            item_slot: None,
        }));
        assert!(matches!(
            dispatch_verb(&bad, &command, &GameState::default(),),
            VerbOutcome::Refused(_)
        ));
        assert_eq!(
            dispatch_verb(&AgentVerb::Interact(InteractVerb::Sit), &command, &GameState::default(),),
            VerbOutcome::Accepted
        );
    }

    #[test]
    fn interact_sit_requests_sit_true() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Interact(InteractVerb::Sit), &command, &GameState::default(),);
        assert_eq!(command.take_sit(), Some(true));
    }

    #[test]
    fn interact_stand_requests_sit_false() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Interact(InteractVerb::Stand), &command, &GameState::default(),);
        assert_eq!(command.take_sit(), Some(false));
    }

    #[test]
    fn lifecycle_respawn_does_not_panic() {
        let command = CommandState::default();
        dispatch_verb(&AgentVerb::Lifecycle(LifecycleVerb::Respawn), &command, &GameState::default(),);
    }

    #[tokio::test]
    async fn handshake_accepts_matching_protocol_version() {
        let hello = encode_line(&Hello {
            protocol_version: PROTOCOL_VERSION,
        })
        .unwrap();
        let mut reader = tokio::io::BufReader::new(hello.as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let accepted = handshake(&mut reader, &mut writer, TEST_CACHE_DIR).await;
        assert!(accepted);
        let reply: HandshakeReply = decode_line(std::str::from_utf8(&writer).unwrap()).unwrap();
        assert_eq!(
            reply,
            HandshakeReply::Accepted {
                asset_cache_dir: TEST_CACHE_DIR.into()
            }
        );
    }

    #[tokio::test]
    async fn handshake_rejects_mismatched_protocol_version() {
        let hello = encode_line(&Hello {
            protocol_version: PROTOCOL_VERSION + 1,
        })
        .unwrap();
        let mut reader = tokio::io::BufReader::new(hello.as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let accepted = handshake(&mut reader, &mut writer, TEST_CACHE_DIR).await;
        assert!(!accepted);
        let reply: HandshakeReply = decode_line(std::str::from_utf8(&writer).unwrap()).unwrap();
        match reply {
            HandshakeReply::Rejected {
                server_protocol_version,
                ..
            } => {
                assert_eq!(server_protocol_version, PROTOCOL_VERSION);
            }
            HandshakeReply::Accepted { .. } => panic!("a version mismatch must not be accepted"),
        }
    }

    #[tokio::test]
    async fn handshake_rejects_malformed_first_line() {
        let mut reader = tokio::io::BufReader::new("not json\n".as_bytes());
        let mut writer: Vec<u8> = Vec::new();
        let accepted = handshake(&mut reader, &mut writer, TEST_CACHE_DIR).await;
        assert!(!accepted);
    }

    #[tokio::test]
    async fn handshake_returns_false_on_immediate_eof() {
        let mut reader = tokio::io::BufReader::new(&b""[..]);
        let mut writer: Vec<u8> = Vec::new();
        let accepted = handshake(&mut reader, &mut writer, TEST_CACHE_DIR).await;
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

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            handshake(&mut reader, &mut writer, TEST_CACHE_DIR),
        )
        .await;
        assert_eq!(
            result,
            Ok(false),
            "handshake must time out and return false, not hang forever"
        );

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
        let accepted = tokio::time::timeout(
            Duration::from_secs(5),
            handshake(&mut reader, &mut writer, TEST_CACHE_DIR),
        )
        .await
        .unwrap();
        assert!(
            !accepted,
            "an oversized line with no newline must be rejected, not hang"
        );
    }

    #[tokio::test]
    async fn run_pushes_an_observation_every_tick_after_handshake() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let camera = CameraSlots::for_test();
        let command = CommandState::default();
        let game_state: GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        let shared_collision: SharedCollision = Arc::new(std::sync::RwLock::new(None));
        let spells = Arc::new(SpellDb::default());
        let net_thread_dead: NetThreadDeadShared = Arc::new(Mutex::new(None));

        let server_task = tokio::spawn(run(
            server,
            camera,
            command,
            game_state,
            shared_collision,
            spells,
            net_thread_dead,
            TEST_CACHE_DIR.into(),
            no_calls(),
        ));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(
                encode_line(&Hello {
                    protocol_version: PROTOCOL_VERSION,
                })
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let reply: HandshakeReply = decode_line(&line).unwrap();
        assert_eq!(
            reply,
            HandshakeReply::Accepted {
                asset_cache_dir: TEST_CACHE_DIR.into()
            }
        );

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
        let game_state: GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        let shared_collision: SharedCollision = Arc::new(std::sync::RwLock::new(None));
        let spells = Arc::new(SpellDb::default());
        let net_thread_dead: NetThreadDeadShared = Arc::new(Mutex::new(None));

        let server_task = tokio::spawn(run(
            server,
            camera,
            command,
            game_state,
            shared_collision,
            spells,
            net_thread_dead,
            TEST_CACHE_DIR.into(),
            no_calls(),
        ));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(
                encode_line(&Hello {
                    protocol_version: PROTOCOL_VERSION + 1,
                })
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let reply: HandshakeReply = decode_line(&line).unwrap();
        assert!(matches!(reply, HandshakeReply::Rejected { .. }));

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), server_task).await;
        assert!(
            result.is_ok(),
            "run() must return promptly after a rejected handshake"
        );
    }

    /// Every verb is dispatched, in order; each one sent with a `seq` is answered exactly once,
    /// echoing the `seq`; a verb sent without one is dispatched but not answered.
    #[tokio::test]
    async fn queued_verbs_are_each_dispatched_and_answered_once_in_order() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let command = CommandState::default();
        let command_check = command.clone();
        let game_state: GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        let server_task = tokio::spawn(run(
            server,
            CameraSlots::for_test(),
            command,
            game_state,
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(SpellDb::default()),
            Arc::new(Mutex::new(None)),
            TEST_CACHE_DIR.into(),
            no_calls(),
        ));
        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(
                encode_line(&Hello {
                    protocol_version: PROTOCOL_VERSION,
                })
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        // Four Steps back to back, well inside one 150ms tick: none may displace another.
        let steps = [
            Step {
                movement: None,
                verb: Some(AgentVerb::Combat(CombatVerb::Target { spawn_id: 1 })),
                seq: Some(1),
                calls: vec![],
            },
            Step {
                movement: None,
                verb: Some(AgentVerb::Combat(CombatVerb::Attack { on: true })),
                seq: Some(2),
                calls: vec![],
            },
            Step {
                movement: None,
                verb: Some(AgentVerb::Interact(InteractVerb::Sit)),
                seq: None,
                calls: vec![],
            },
            Step {
                movement: None,
                verb: Some(AgentVerb::Lifecycle(LifecycleVerb::Respawn)),
                seq: Some(4),
                calls: vec![],
            },
        ];
        for s in &steps {
            write_half
                .write_all(encode_line(s).unwrap().as_bytes())
                .await
                .unwrap();
        }

        let mut answers = Vec::new();
        for _ in 0..8 {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let obs: eqoxide_agent_protocol::observation::Observation = decode_line(&line).unwrap();
            answers.extend(obs.verb_results);
        }
        let seqs: Vec<u64> = answers.iter().map(|a| a.seq).collect();
        assert_eq!(
            seqs,
            vec![1, 2, 4],
            "each seq answered once, in order, none for the seq-less verb"
        );
        assert_eq!(answers[0].outcome, VerbOutcome::Accepted);
        assert_eq!(answers[1].outcome, VerbOutcome::Accepted);
        assert!(
            matches!(answers[2].outcome, VerbOutcome::Refused(_)),
            "a living character cannot respawn"
        );
        assert_eq!(
            command_check.take_sit(),
            Some(true),
            "the seq-less verb was still dispatched"
        );

        drop(write_half);
        drop(reader);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), server_task).await;
    }

    /// Regression test for the Critical bug: a latched `Step`'s verb must dispatch exactly once,
    /// not every tick until a new `Step` arrives (spec §7's movement-latches/verb-is-one-shot
    /// split). Drives the fix through the real socket path (`run()`) — also covers spec §13's "a
    /// Step reaching action dispatch over the socket" gap (10b).
    #[tokio::test]
    async fn a_steps_verb_dispatches_exactly_once_despite_multiple_ticks() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let camera = CameraSlots::for_test();
        let command = CommandState::default();
        let command_check = command.clone();
        let game_state: GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        let shared_collision: SharedCollision = Arc::new(std::sync::RwLock::new(None));
        let spells = Arc::new(SpellDb::default());
        let net_thread_dead: NetThreadDeadShared = Arc::new(Mutex::new(None));

        let server_task = tokio::spawn(run(
            server,
            camera,
            command,
            game_state,
            shared_collision,
            spells,
            net_thread_dead,
            TEST_CACHE_DIR.into(),
            no_calls(),
        ));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(
                encode_line(&Hello {
                    protocol_version: PROTOCOL_VERSION,
                })
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let reply: HandshakeReply = decode_line(&line).unwrap();
        assert_eq!(
            reply,
            HandshakeReply::Accepted {
                asset_cache_dir: TEST_CACHE_DIR.into()
            }
        );

        let step = Step {
            movement: None,
            verb: Some(AgentVerb::Combat(CombatVerb::Target { spawn_id: 42 })),
            seq: None,
            calls: vec![],
        };
        write_half
            .write_all(encode_line(&step).unwrap().as_bytes())
            .await
            .unwrap();

        // Read several observations in a row, proving several ticks elapsed and giving the
        // latched Step every opportunity to (incorrectly) re-dispatch if the bug were still there.
        for _ in 0..3 {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let _obs: eqoxide_agent_protocol::observation::Observation =
                decode_line(&line).unwrap();
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

    /// Fix 10c: a `Step`'s `movement` must reach `CameraSlots::manual_move` through the real
    /// socket path (`run()`).
    #[tokio::test]
    async fn a_steps_movement_reaches_camera_manual_move_over_the_socket() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let camera = CameraSlots::for_test();
        let camera_check = camera.clone();
        let command = CommandState::default();
        let game_state: GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        let shared_collision: SharedCollision = Arc::new(std::sync::RwLock::new(None));
        let spells = Arc::new(SpellDb::default());
        let net_thread_dead: NetThreadDeadShared = Arc::new(Mutex::new(None));

        let server_task = tokio::spawn(run(
            server,
            camera,
            command,
            game_state,
            shared_collision,
            spells,
            net_thread_dead,
            TEST_CACHE_DIR.into(),
            no_calls(),
        ));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(
                encode_line(&Hello {
                    protocol_version: PROTOCOL_VERSION,
                })
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let reply: HandshakeReply = decode_line(&line).unwrap();
        assert_eq!(
            reply,
            HandshakeReply::Accepted {
                asset_cache_dir: TEST_CACHE_DIR.into()
            }
        );

        let step = Step {
            movement: Some(AgentMovement {
                dir: [1.0, 0.0],
                up: 0.0,
                jump: false,
                wish_heading: Some(90.0),
            }),
            verb: None,
            seq: None,
            calls: vec![],
        };
        write_half
            .write_all(encode_line(&step).unwrap().as_bytes())
            .await
            .unwrap();

        // At least one tick must elapse for the tick loop to drain the Step and apply it.
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let _obs: eqoxide_agent_protocol::observation::Observation = decode_line(&line).unwrap();

        let m = camera_check
            .manual_move
            .lock()
            .unwrap()
            .expect("manual move must have been queued");
        assert_eq!(m.dir, [1.0, 0.0]);
        assert_eq!(m.wish_heading, Some(90.0));

        drop(write_half);
        drop(reader);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), server_task).await;
    }

    /// A `Step` with a non-finite movement field (e.g. `dir` overflowed from the wire's f64 into
    /// `f32::INFINITY`) must never reach `CameraSlots::manual_move` — that value would otherwise
    /// flow straight into the character controller's position integration and permanently corrupt
    /// `GameState`'s position from that tick onward.
    #[tokio::test]
    async fn a_step_with_a_non_finite_movement_field_never_reaches_camera_manual_move() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let camera = CameraSlots::for_test();
        let camera_check = camera.clone();
        let command = CommandState::default();
        let game_state: GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        let shared_collision: SharedCollision = Arc::new(std::sync::RwLock::new(None));
        let spells = Arc::new(SpellDb::default());
        let net_thread_dead: NetThreadDeadShared = Arc::new(Mutex::new(None));

        let server_task = tokio::spawn(run(
            server,
            camera,
            command,
            game_state,
            shared_collision,
            spells,
            net_thread_dead,
            TEST_CACHE_DIR.into(),
            no_calls(),
        ));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(
                encode_line(&Hello {
                    protocol_version: PROTOCOL_VERSION,
                })
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let reply: HandshakeReply = decode_line(&line).unwrap();
        assert_eq!(
            reply,
            HandshakeReply::Accepted {
                asset_cache_dir: TEST_CACHE_DIR.into()
            }
        );

        let step = Step {
            movement: Some(AgentMovement {
                dir: [f32::INFINITY, 0.0],
                up: 0.0,
                jump: false,
                wish_heading: None,
            }),
            verb: None,
            seq: None,
            calls: vec![],
        };
        write_half
            .write_all(encode_line(&step).unwrap().as_bytes())
            .await
            .unwrap();

        // At least one tick must elapse for the tick loop to drain the Step, had it been latched.
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let _obs: eqoxide_agent_protocol::observation::Observation = decode_line(&line).unwrap();

        assert!(
            camera_check.manual_move.lock().unwrap().is_none(),
            "a non-finite movement field must never reach CameraSlots::manual_move"
        );

        drop(write_half);
        drop(reader);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), server_task).await;
    }

    /// Regression test for the Critical bug: a `Step` that carries a verb but no movement
    /// (`movement: None`, spec §7's "a verb with no movement change") must NOT clear movement an
    /// earlier `Step` latched. A `latest_step: Option<Step>` snapshot got this wrong by construction
    /// — any later `Step`, including a verb-only one, replaced the whole snapshot and silently
    /// stopped an agent mid-kite the instant it also fired a verb.
    #[tokio::test]
    async fn a_verb_only_step_does_not_clear_previously_latched_movement() {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let camera = CameraSlots::for_test();
        let camera_check = camera.clone();
        let command = CommandState::default();
        let game_state: GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        let shared_collision: SharedCollision = Arc::new(std::sync::RwLock::new(None));
        let spells = Arc::new(SpellDb::default());
        let net_thread_dead: NetThreadDeadShared = Arc::new(Mutex::new(None));

        let server_task = tokio::spawn(run(
            server,
            camera,
            command,
            game_state,
            shared_collision,
            spells,
            net_thread_dead,
            TEST_CACHE_DIR.into(),
            no_calls(),
        ));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(
                encode_line(&Hello {
                    protocol_version: PROTOCOL_VERSION,
                })
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let reply: HandshakeReply = decode_line(&line).unwrap();
        assert_eq!(
            reply,
            HandshakeReply::Accepted {
                asset_cache_dir: TEST_CACHE_DIR.into()
            }
        );

        // First Step: movement only.
        let movement_step = Step {
            movement: Some(AgentMovement {
                dir: [1.0, 0.0],
                up: 0.0,
                jump: false,
                wish_heading: None,
            }),
            verb: None,
            seq: None,
            calls: vec![],
        };
        write_half
            .write_all(encode_line(&movement_step).unwrap().as_bytes())
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let _obs: eqoxide_agent_protocol::observation::Observation = decode_line(&line).unwrap();
        assert!(
            camera_check.manual_move.lock().unwrap().is_some(),
            "movement must have latched"
        );

        // Second Step: a verb only, no movement field at all — must not cancel the latch.
        let verb_only_step = Step {
            movement: None,
            verb: Some(AgentVerb::Combat(CombatVerb::Consider { spawn_id: 7 })),
            seq: None,
            calls: vec![],
        };
        write_half
            .write_all(encode_line(&verb_only_step).unwrap().as_bytes())
            .await
            .unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let _obs: eqoxide_agent_protocol::observation::Observation = decode_line(&line).unwrap();

        let m = camera_check
            .manual_move
            .lock()
            .unwrap()
            .expect("movement must still be latched after a verb-only Step");
        assert_eq!(
            m.dir,
            [1.0, 0.0],
            "a verb-only Step must not have cleared the latched movement"
        );

        drop(write_half);
        drop(reader);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), server_task).await;
    }

    /// A handler that answers every call invalid, for tests that send no calls.
    fn no_calls() -> CallHandler {
        Arc::new(|_| Box::pin(async { (CallStatus::Invalid, serde_json::Value::Null) }))
    }

    /// Echoes the call args back, after a delay if args.delay_ms says so; an unknown op is
    /// answered invalid.
    fn echo_calls() -> CallHandler {
        Arc::new(|call: Call| {
            Box::pin(async move {
                if call.op != "echo" {
                    return (CallStatus::Invalid, serde_json::json!("unknown op"));
                }
                if let Some(ms) = call.args.get("delay_ms").and_then(|v| v.as_u64()) {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                (CallStatus::Ok, call.args)
            })
        })
    }

    async fn connect_with(
        calls: CallHandler,
    ) -> (
        tokio::net::unix::OwnedWriteHalf,
        tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    ) {
        let (client, server) = UnixStream::pair().expect("socket pair");
        let game_state: GameStateSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(GameState::default()));
        tokio::spawn(run(
            server,
            CameraSlots::for_test(),
            CommandState::default(),
            game_state,
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(SpellDb::default()),
            Arc::new(Mutex::new(None)),
            TEST_CACHE_DIR.into(),
            calls,
        ));
        let (read_half, mut write_half) = client.into_split();
        let mut reader = tokio::io::BufReader::new(read_half);
        write_half
            .write_all(
                encode_line(&Hello {
                    protocol_version: PROTOCOL_VERSION,
                })
                .unwrap()
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        (write_half, reader)
    }

    /// Reads Observations until the wanted number of replies have arrived, returning them.
    async fn collect_replies(
        reader: &mut tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
        want: usize,
    ) -> Vec<CallReply> {
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.len() < want && Instant::now() < deadline {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let obs: eqoxide_agent_protocol::observation::Observation = decode_line(&line).unwrap();
            got.extend(obs.call_replies);
        }
        got
    }

    fn call_step(calls: Vec<Call>) -> Step {
        Step {
            movement: None,
            verb: None,
            seq: None,
            calls,
        }
    }

    #[tokio::test]
    async fn each_call_in_a_burst_is_answered_once_by_id() {
        let (mut w, mut r) = connect_with(echo_calls()).await;
        let calls = (1..=5)
            .map(|id| Call {
                id,
                op: "echo".into(),
                args: serde_json::json!({ "n": id }),
            })
            .collect();
        w.write_all(encode_line(&call_step(calls)).unwrap().as_bytes())
            .await
            .unwrap();
        let mut replies = collect_replies(&mut r, 5).await;
        replies.sort_by_key(|c| c.id);
        assert_eq!(replies.len(), 5);
        for (i, c) in replies.iter().enumerate() {
            assert_eq!(c.id, i as u64 + 1);
            assert_eq!(c.status, CallStatus::Ok);
            assert_eq!(c.body, serde_json::json!({ "n": c.id }));
        }
    }

    #[tokio::test]
    async fn an_unknown_op_is_answered_invalid_not_dropped() {
        let (mut w, mut r) = connect_with(echo_calls()).await;
        let step = call_step(vec![Call {
            id: 9,
            op: "nope".into(),
            args: serde_json::Value::Null,
        }]);
        w.write_all(encode_line(&step).unwrap().as_bytes())
            .await
            .unwrap();
        let replies = collect_replies(&mut r, 1).await;
        assert_eq!(replies.len(), 1);
        assert_eq!((replies[0].id, replies[0].status), (9, CallStatus::Invalid));
    }

    #[tokio::test]
    async fn a_slow_call_does_not_hold_back_a_fast_one() {
        let (mut w, mut r) = connect_with(echo_calls()).await;
        let step = call_step(vec![
            Call {
                id: 1,
                op: "echo".into(),
                args: serde_json::json!({ "delay_ms": 1500 }),
            },
            Call {
                id: 2,
                op: "echo".into(),
                args: serde_json::json!({}),
            },
        ]);
        w.write_all(encode_line(&step).unwrap().as_bytes())
            .await
            .unwrap();
        let replies = collect_replies(&mut r, 1).await;
        assert_eq!(replies[0].id, 2, "the fast call must finish first");
        let rest = collect_replies(&mut r, 1).await;
        assert_eq!(rest[0].id, 1);
    }

    #[tokio::test]
    async fn calls_past_the_in_flight_bound_are_answered_unavailable() {
        let (mut w, mut r) = connect_with(echo_calls()).await;
        let total = MAX_CALLS_IN_FLIGHT as u64 + 3;
        let calls = (1..=total)
            .map(|id| Call {
                id,
                op: "echo".into(),
                args: serde_json::json!({ "delay_ms": 800 }),
            })
            .collect();
        w.write_all(encode_line(&call_step(calls)).unwrap().as_bytes())
            .await
            .unwrap();
        let replies = collect_replies(&mut r, total as usize).await;
        assert_eq!(
            replies.len() as u64,
            total,
            "every call is answered exactly once"
        );
        let refused: Vec<_> = replies
            .iter()
            .filter(|c| c.status == CallStatus::Unavailable)
            .collect();
        assert_eq!(refused.len(), 3);
    }
}
