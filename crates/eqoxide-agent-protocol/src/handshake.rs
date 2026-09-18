//! Connect-time version handshake (spec §5, §11): the client sends `Hello` first, the server
//! replies with `HandshakeReply` and closes the connection on `Rejected` — never a silent
//! misparse downstream.

use serde::{Deserialize, Serialize};

/// Bumped whenever a wire type in this crate changes shape in a way that breaks an old client.
pub const PROTOCOL_VERSION: u32 = 1;

/// The client's first line on every connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
}

/// The server's second line. `Rejected` is followed by the server closing the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum HandshakeReply {
    Accepted,
    Rejected { server_protocol_version: u32, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};

    #[test]
    fn hello_round_trips() {
        let h = Hello { protocol_version: PROTOCOL_VERSION };
        let line = encode_line(&h).unwrap();
        let back: Hello = decode_line(&line).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn handshake_reply_accepted_round_trips() {
        let r = HandshakeReply::Accepted;
        let line = encode_line(&r).unwrap();
        let back: HandshakeReply = decode_line(&line).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn handshake_reply_rejected_round_trips_with_fields() {
        let r = HandshakeReply::Rejected {
            server_protocol_version: PROTOCOL_VERSION,
            message: "client protocol_version 0 != server 1".into(),
        };
        let line = encode_line(&r).unwrap();
        let back: HandshakeReply = decode_line(&line).unwrap();
        assert_eq!(back, r);
    }
}
