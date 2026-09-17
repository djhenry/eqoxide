//! Per-connection handshake, tick loop, and action dispatch (spec §5, §7, §8).

use eqoxide_agent_protocol::framing::{decode_line, encode_line};
use eqoxide_agent_protocol::handshake::{HandshakeReply, Hello, PROTOCOL_VERSION};
use eqoxide_agent_protocol::step::Step;
use eqoxide_agent_protocol::verb::{AgentVerb, CombatVerb, InteractVerb, LifecycleVerb};
use eqoxide_command::CommandState;
use eqoxide_ipc::{CameraSlots, ManualMove};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Re-issued every tick (~2x the 150ms tick, `MOVE_LATCH`), so movement naturally stops within
/// ~300ms of the agent going quiet — the same fail-safe deadline mechanism `ManualMove` already
/// gives HTTP's `/v1/move/manual`, applied here every tick instead of once per request.
const MOVE_LATCH: Duration = Duration::from_millis(300);

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
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return false; // connection closed before sending anything
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

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_agent_protocol::movement::AgentMovement;
    use eqoxide_agent_protocol::verb::{CastRequest, MoveVerb};
    use eqoxide_ipc::{CameraMode, CameraSnapshot};
    use std::sync::{Arc, Mutex};

    fn empty_camera_slots() -> CameraSlots {
        CameraSlots {
            cmd_tx: Arc::new(Mutex::new(None)),
            snapshot: Arc::new(Mutex::new(CameraSnapshot {
                mode: CameraMode::AutoFollow,
                azimuth: 0.0,
                elevation: 0.0,
                radius: 0.0,
                focus: [0.0, 0.0, 0.0],
                eye: [0.0, 0.0, 0.0],
                occluded: false,
                still_blocked: false,
                drawn_frame: None,
                drawn_at: None,
            })),
            frame_req: Arc::new(Mutex::new(None)),
            manual_move: Arc::new(Mutex::new(None)),
        }
    }

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
        let camera = empty_camera_slots();
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
        let camera = empty_camera_slots();
        let command = CommandState::default();
        let step = Step { movement: None, verb: None };
        apply_step(&step, &camera, &command);
        assert!(camera.manual_move.lock().unwrap().is_none());
    }

    #[test]
    fn apply_step_with_both_movement_and_verb_applies_both() {
        let camera = empty_camera_slots();
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
}
