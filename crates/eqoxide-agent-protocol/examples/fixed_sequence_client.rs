//! Minimal fixed-action-sequence client — NOT the real agent, just enough to exercise the protocol
//! end-to-end against a live eqoxide instance (spec §13). Run:
//!
//! ```text
//! cargo run -p eqoxide-agent-protocol --example fixed_sequence_client -- /path/to/agent.sock
//! ```
//!
//! against an instance started with `cargo run -- --testzone --agent-socket /path/to/agent.sock`.
//! Sends a short scripted sequence of `Step`s and prints each `Observation` received in between.

use eqoxide_agent_protocol::framing::{decode_line, encode_line};
use eqoxide_agent_protocol::handshake::{HandshakeReply, Hello, PROTOCOL_VERSION};
use eqoxide_agent_protocol::movement::AgentMovement;
use eqoxide_agent_protocol::observation::Observation;
use eqoxide_agent_protocol::step::Step;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

fn read_line(reader: &mut impl BufRead) -> String {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read line from socket");
    line
}

fn main() {
    let socket_path = std::env::args().nth(1).expect("usage: fixed_sequence_client <socket path>");
    let stream = UnixStream::connect(&socket_path).expect("connect to agent socket");
    let mut writer = stream.try_clone().expect("clone stream for writing");
    let mut reader = BufReader::new(stream);

    writer
        .write_all(encode_line(&Hello { protocol_version: PROTOCOL_VERSION }).unwrap().as_bytes())
        .expect("send Hello");

    let reply: HandshakeReply = decode_line(&read_line(&mut reader)).expect("parse HandshakeReply");
    match reply {
        HandshakeReply::Accepted => println!("handshake accepted"),
        HandshakeReply::Rejected { server_protocol_version, message } => {
            eprintln!("handshake rejected: server={server_protocol_version} message={message}");
            std::process::exit(1);
        }
    }

    // A short scripted sequence: stand still (read one Observation), walk east for a beat, stop.
    let sequence: Vec<Step> = vec![
        Step { movement: None, verb: None },
        Step {
            movement: Some(AgentMovement { dir: [1.0, 0.0], up: 0.0, jump: false, wish_heading: None }),
            verb: None,
        },
        Step { movement: Some(AgentMovement { dir: [0.0, 0.0], up: 0.0, jump: false, wish_heading: None }), verb: None },
    ];

    for (i, step) in sequence.iter().enumerate() {
        writer.write_all(encode_line(step).unwrap().as_bytes()).expect("send Step");
        let obs: Observation = decode_line(&read_line(&mut reader)).expect("parse Observation");
        println!(
            "step {i}: pos={:?} hp={}/{} zone={} dead={} terminated={} visible={}",
            obs.own.pos, obs.own.hp, obs.own.hp_max, obs.own.zone_name, obs.dead, obs.terminated, obs.visible.len()
        );
    }
}
