//! Exercises every wired verb and a few `Call` ops, correlating each answer by id. Run:
//!
//! ```text
//! cargo run -p eqoxide-agent-protocol --example verbs_and_calls_client -- /path/to/agent.sock [spawn_id [destroy_slot [consume_slot]]]
//! ```
//!
//! against a live instance started with `--agent-socket /path/to/agent.sock`. Each verb is sent in
//! a `Step` carrying a `seq`; its answer arrives in `Observation.verb_results` with the same `seq`.
//! Each `Call` carries an `id`; its answer arrives in `Observation.call_replies` with the same `id`.
//! The script is deliberately harmless (target, attack on then off, sit then stand, a chat on
//! /ooc, a pet follow, an inventory move that may be refused) and prints every outcome, refusals
//! included. `Inventory::Destroy` is irreversible, so it and `Inventory::Consume` are sent only
//! when `destroy_slot` / `consume_slot` are given (wire slots, e.g. 24); a slot the host cannot act
//! on is answered `Refused` with the reason. See docs/agent-api.md.

use eqoxide_agent_protocol::call::{Call, CallReply};
use eqoxide_agent_protocol::framing::{decode_line, encode_line};
use eqoxide_agent_protocol::handshake::{HandshakeReply, Hello, PROTOCOL_VERSION};
use eqoxide_agent_protocol::observation::Observation;
use eqoxide_agent_protocol::step::Step;
use eqoxide_agent_protocol::verb::{AgentVerb, ChatVerb, CombatVerb, InteractVerb, InventoryVerb, PetVerb};
use eqoxide_agent_protocol::verb_result::VerbResult;
use serde_json::json;
use std::io::{BufRead, Write};

/// An answer is expected within a few ticks (150 ms each); a verb waits at most one tick per queued
/// verb ahead of it, a call as long as its op takes (merchant ops up to ~4 s, give up to ~8 s).
const MAX_OBSERVATIONS_PER_WAIT: usize = 100;

/// A `Step` carrying only a verb. `movement: None` does not clear movement latched earlier.
fn verb_step(seq: u64, verb: AgentVerb) -> Step {
    Step { movement: None, verb: Some(verb), seq: Some(seq), calls: vec![] }
}

/// A `Step` carrying only calls.
fn call_step(calls: Vec<Call>) -> Step {
    Step { movement: None, verb: None, seq: None, calls }
}

/// Slots for the opt-in inventory verbs; `None` leaves that verb out of the script.
#[derive(Clone, Copy, Default)]
struct InventoryTargets {
    destroy_slot: Option<u32>,
    consume_slot: Option<u32>,
}

/// The scripted verbs, each with its own `seq` (1-based, in order).
fn verb_script(target_spawn_id: u32, inv: InventoryTargets) -> Vec<(&'static str, Step)> {
    let mut verbs = vec![
        ("target", AgentVerb::Combat(CombatVerb::Target { spawn_id: target_spawn_id })),
        ("consider", AgentVerb::Combat(CombatVerb::Consider { spawn_id: target_spawn_id })),
        ("attack on", AgentVerb::Combat(CombatVerb::Attack { on: true })),
        ("attack off", AgentVerb::Combat(CombatVerb::Attack { on: false })),
        ("sit", AgentVerb::Interact(InteractVerb::Sit)),
        ("stand", AgentVerb::Interact(InteractVerb::Stand)),
        ("inventory move 23 -> 24", AgentVerb::Inventory(InventoryVerb::Move { from: 23, to: 24 })),
        ("chat ooc", AgentVerb::Chat(ChatVerb::Ooc { text: "hello from the example client".into() })),
        ("pet follow", AgentVerb::Pet(PetVerb::Command { command: 4 })),
    ];
    if let Some(slot) = inv.consume_slot {
        verbs.push(("inventory consume", AgentVerb::Inventory(InventoryVerb::Consume { slot })));
    }
    if let Some(slot) = inv.destroy_slot {
        verbs.push(("inventory destroy", AgentVerb::Inventory(InventoryVerb::Destroy { slot })));
    }
    verbs
        .into_iter()
        .enumerate()
        .map(|(i, (label, v))| (label, verb_step(i as u64 + 1, v)))
        .collect()
}

/// The scripted calls, each with its own `id`. `op` is `"<METHOD> <path>"`; `args` may carry
/// `query` (an object) and `body` (the JSON request body).
fn call_script() -> Vec<(&'static str, Call)> {
    vec![
        ("read inventory", Call { id: 100, op: "GET /v1/observe/inventory".into(), args: json!(null) }),
        ("read npc dialogue lines", Call {
            id: 101,
            op: "GET /v1/observe/messages".into(),
            args: json!({"query": {"kind": "npc"}}),
        }),
        ("list the open merchant's wares", Call { id: 102, op: "GET /v1/merchant/list".into(), args: json!(null) }),
        ("an op that does not exist", Call { id: 103, op: "GET /v1/no/such/op".into(), args: json!(null) }),
    ]
}

struct Session<R: BufRead, W: Write> {
    reader: R,
    writer: W,
}

impl<R: BufRead, W: Write> Session<R, W> {
    fn connect(mut reader: R, mut writer: W) -> Result<Self, String> {
        let hello = encode_line(&Hello { protocol_version: PROTOCOL_VERSION }).map_err(|e| e.to_string())?;
        writer.write_all(hello.as_bytes()).map_err(|e| e.to_string())?;
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|e| e.to_string())?;
        match decode_line::<HandshakeReply>(&line).map_err(|e| e.to_string())? {
            HandshakeReply::Accepted { asset_cache_dir } => {
                println!("handshake accepted (asset cache: {asset_cache_dir})");
                Ok(Self { reader, writer })
            }
            HandshakeReply::Rejected { server_protocol_version, message } => {
                Err(format!("handshake rejected: server={server_protocol_version} message={message}"))
            }
        }
    }

    fn send(&mut self, step: &Step) -> Result<(), String> {
        let line = encode_line(step).map_err(|e| e.to_string())?;
        self.writer.write_all(line.as_bytes()).map_err(|e| e.to_string())
    }

    fn next_observation(&mut self) -> Result<Observation, String> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed".into());
        }
        decode_line(&line).map_err(|e| e.to_string())
    }

    /// Reads observations until one carries the `VerbResult` whose `seq` matches.
    fn wait_for_verb_result(&mut self, seq: u64) -> Result<VerbResult, String> {
        for _ in 0..MAX_OBSERVATIONS_PER_WAIT {
            if let Some(r) = self.next_observation()?.verb_results.into_iter().find(|r| r.seq == seq) {
                return Ok(r);
            }
        }
        Err(format!("no verb_result for seq {seq}"))
    }

    /// Reads observations until the replies to every id in `ids` have arrived (any order).
    fn wait_for_call_replies(&mut self, ids: &[u64]) -> Result<Vec<CallReply>, String> {
        let mut got: Vec<CallReply> = Vec::new();
        for _ in 0..MAX_OBSERVATIONS_PER_WAIT {
            got.extend(self.next_observation()?.call_replies);
            if ids.iter().all(|id| got.iter().any(|r| r.id == *id)) {
                return Ok(got);
            }
        }
        Err(format!("missing call_replies; wanted {ids:?}, got {:?}", got.iter().map(|r| r.id).collect::<Vec<_>>()))
    }
}

/// Everything the script learned, in the order it was asked.
struct Report {
    verbs: Vec<(&'static str, VerbResult)>,
    calls: Vec<(&'static str, CallReply)>,
}

fn run<R: BufRead, W: Write>(reader: R, writer: W, target_spawn_id: u32, inv: InventoryTargets) -> Result<Report, String> {
    let mut session = Session::connect(reader, writer)?;
    let mut report = Report { verbs: vec![], calls: vec![] };

    // Verbs go one at a time: the host dispatches one verb per tick, and waiting for each answer
    // keeps the output readable. A client may instead queue up to 16 and match results by seq.
    for (label, step) in verb_script(target_spawn_id, inv) {
        let seq = step.seq.expect("verb_script always sets seq");
        session.send(&step)?;
        let result = session.wait_for_verb_result(seq)?;
        println!("verb {label:<26} seq={seq} -> {:?}", result.outcome);
        report.verbs.push((label, result));
    }

    // Calls run concurrently on the host and answer in any order: send them all in one Step, then
    // collect by id.
    let calls = call_script();
    let ids: Vec<u64> = calls.iter().map(|(_, c)| c.id).collect();
    session.send(&call_step(calls.iter().map(|(_, c)| c.clone()).collect()))?;
    let replies = session.wait_for_call_replies(&ids)?;
    for (label, call) in &calls {
        let reply = replies.iter().find(|r| r.id == call.id).expect("wait_for_call_replies checked").clone();
        println!("call {label:<32} id={} -> {:?} {}", call.id, reply.status, reply.body);
        report.calls.push((label, reply));
    }
    Ok(report)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let socket_path = args
        .next()
        .expect("usage: verbs_and_calls_client <socket path> [spawn_id [destroy_slot [consume_slot]]]");
    let mut number = |what: &str| args.next().map(|s| s.parse::<u32>().unwrap_or_else(|_| panic!("{what} must be a number")));
    let spawn_id = number("spawn_id").unwrap_or(0);
    let inv = InventoryTargets { destroy_slot: number("destroy_slot"), consume_slot: number("consume_slot") };
    let stream = std::os::unix::net::UnixStream::connect(&socket_path).expect("connect to agent socket");
    let writer = stream.try_clone().expect("clone stream for writing");
    match run(std::io::BufReader::new(stream), writer, spawn_id, inv) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_agent_protocol::call::CallStatus;
    use eqoxide_agent_protocol::observation::{LegalActionMask, OwnState};
    use eqoxide_agent_protocol::verb_result::VerbOutcome;
    use std::io::BufReader;
    use std::os::unix::net::UnixStream;

    fn observation(tick: u64, verb_results: Vec<VerbResult>, call_replies: Vec<CallReply>) -> Observation {
        Observation {
            own: OwnState {
                pos: [0.0; 3], heading: 0.0, hp: 1, hp_max: 1, hp_verified: true, mana: 0, mana_max: 0,
                endurance: 0, endurance_max: 0, endurance_confirmed: true, casting: None, buffs: vec![],
                zone_name: "test".into(), target_id: None, target_name: None, auto_attack: false,
                sitting: false, held: false, player_class: "Warrior".into(), player_level: 1,
            },
            visible: vec![],
            legal_actions: LegalActionMask { gems: [false; 9], abilities: vec![] },
            dead: false, terminated: false, truncated: false, visibility_available: false,
            tick, verb_results, call_replies,
        }
    }

    /// A stand-in host. For every `Step` it sends one empty tick, then a tick carrying the answers:
    /// verbs are Accepted except an inventory move from slot 23 and a consume of slot 25, which are
    /// Refused; `GET
    /// /v1/merchant/list` is answered Conflict, an unknown op NotFound, everything else Ok. Replies
    /// are reversed to show the client does not depend on their order.
    fn fake_host(stream: UnixStream) -> Vec<Step> {
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let hello: Hello = decode_line(&line).unwrap();
        assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
        let accepted = HandshakeReply::Accepted { asset_cache_dir: "/cache".into() };
        writer.write_all(encode_line(&accepted).unwrap().as_bytes()).unwrap();
        let mut seen = vec![];
        let mut tick = 0;
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 {
                return seen;
            }
            let step: Step = decode_line(&line).unwrap();
            let verb_results: Vec<VerbResult> = match (&step.verb, step.seq) {
                (Some(AgentVerb::Inventory(InventoryVerb::Move { from: 23, .. })), Some(seq)) => vec![VerbResult {
                    seq,
                    outcome: VerbOutcome::Refused("no item in slot 23 to move".into()),
                }],
                (Some(AgentVerb::Inventory(InventoryVerb::Consume { slot: 25 })), Some(seq)) => vec![VerbResult {
                    seq,
                    outcome: VerbOutcome::Refused("Rusty Dagger in slot 25 is not food or drink (item type 0)".into()),
                }],
                (Some(_), Some(seq)) => vec![VerbResult { seq, outcome: VerbOutcome::Accepted }],
                _ => vec![],
            };
            let mut call_replies: Vec<CallReply> = step
                .calls
                .iter()
                .map(|c| {
                    let (status, body) = match c.op.as_str() {
                        "GET /v1/merchant/list" => (CallStatus::Conflict, json!("no merchant is open")),
                        "GET /v1/observe/inventory" => (CallStatus::Ok, json!({"count": 0, "items": []})),
                        "GET /v1/observe/messages" => {
                            assert_eq!(c.args, json!({"query": {"kind": "npc"}}));
                            (CallStatus::Ok, json!({"count": 0, "messages": []}))
                        }
                        other => (CallStatus::NotFound, json!(format!("no such op: {other:?}"))),
                    };
                    CallReply { id: c.id, status, body }
                })
                .collect();
            call_replies.reverse();
            seen.push(step);
            for o in [observation(tick, vec![], vec![]), observation(tick + 1, verb_results, call_replies)] {
                writer.write_all(encode_line(&o).unwrap().as_bytes()).unwrap();
            }
            tick += 2;
        }
    }

    #[test]
    fn script_matches_every_answer_to_its_request_by_seq_and_id() {
        let (client, host) = UnixStream::pair().unwrap();
        let host_thread = std::thread::spawn(move || fake_host(host));
        let writer = client.try_clone().unwrap();
        let report = run(
            BufReader::new(client),
            writer,
            42,
            InventoryTargets { destroy_slot: Some(24), consume_slot: Some(25) },
        )
        .unwrap();
        // Both client halves dropped when `run` returned, which ends the host's read loop.
        let seen = host_thread.join().unwrap();

        // Every scripted verb went out once with its own seq, in order.
        let seqs: Vec<u64> = seen.iter().filter_map(|s| s.seq).collect();
        assert_eq!(seqs, (1..=report.verbs.len() as u64).collect::<Vec<_>>());
        assert_eq!(report.verbs.len(), 11);
        assert!(matches!(
            seen[0].verb,
            Some(AgentVerb::Combat(CombatVerb::Target { spawn_id: 42 }))
        ));

        // The refused move and consume are reported as such, everything else was accepted.
        for (label, r) in &report.verbs {
            let want = match *label {
                "inventory move 23 -> 24" => VerbOutcome::Refused("no item in slot 23 to move".into()),
                "inventory consume" => VerbOutcome::Refused(
                    "Rusty Dagger in slot 25 is not food or drink (item type 0)".into(),
                ),
                _ => VerbOutcome::Accepted,
            };
            assert_eq!(r.outcome, want, "{label}");
        }
        assert!(seen.iter().any(|s| matches!(
            s.verb,
            Some(AgentVerb::Inventory(InventoryVerb::Destroy { slot: 24 }))
        )));

        // The four calls went out in one Step and each reply was matched by id despite reordering.
        let call_steps: Vec<&Step> = seen.iter().filter(|s| !s.calls.is_empty()).collect();
        assert_eq!(call_steps.len(), 1);
        assert_eq!(call_steps[0].calls.len(), 4);
        let by_id = |id| report.calls.iter().find(|(_, r)| r.id == id).unwrap().1.status;
        assert_eq!(by_id(100), CallStatus::Ok);
        assert_eq!(by_id(102), CallStatus::Conflict);
        assert_eq!(by_id(103), CallStatus::NotFound);
    }

    #[test]
    fn a_rejected_handshake_is_an_error() {
        let (client, host) = UnixStream::pair().unwrap();
        let t = std::thread::spawn(move || {
            let mut w = host.try_clone().unwrap();
            let mut line = String::new();
            BufReader::new(host).read_line(&mut line).unwrap();
            let r = HandshakeReply::Rejected { server_protocol_version: 99, message: "nope".into() };
            w.write_all(encode_line(&r).unwrap().as_bytes()).unwrap();
        });
        let writer = client.try_clone().unwrap();
        let err = run(BufReader::new(client), writer, 0, InventoryTargets::default()).err().unwrap();
        t.join().unwrap();
        assert!(err.contains("rejected") && err.contains("99"), "{err}");
    }
}
