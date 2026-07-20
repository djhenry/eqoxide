//! Universal Chat Service (UCS) link — the separate EQStream connection RoF2 uses for cross-zone
//! tells, OOC, and chat channels. The zone sends `OP_SetChatServer` at zone-in with the UCS
//! address + mail key; the client opens a second EQStream to it, logs in with `OP_MailLogin`, then
//! exchanges `OP_ChannelMessage`s. This module starts with parsing that bootstrap packet; the
//! connection + login + message routing build on top.
//!
//! See bugs/cross-zone-chat-needs-ucs.md and EQEmu ucs/clientlist.cpp for the server side.

// The `UcsInfo` POD moved DOWN into `eqoxide-core` (#544 Step 2b) so `game_state` (now also in
// core) no longer up-references this `eq_net` layer. Re-export it here so every existing
// `crate::eq_net::ucs::UcsInfo` path — and this module's own parser/tests — keep resolving.
pub use eqoxide_core::ucs::UcsInfo;

/// Parse the `OP_SetChatServer` payload, a NUL-terminated comma string:
/// `"<host>,<port>,<shortname>.<charname>,<connTypeChar><key>"`. The mailbox contains a '.', not a
/// ',', so a 4-way comma split is unambiguous. Returns `None` if malformed.
pub fn parse_set_chat_server(payload: &[u8]) -> Option<UcsInfo> {
    let s = String::from_utf8_lossy(payload);
    let s = s.trim_end_matches('\0');
    let parts: Vec<&str> = s.splitn(4, ',').collect();
    if parts.len() < 4 { return None; }
    let host    = parts[0].to_string();
    let port    = parts[1].parse::<u16>().ok()?;
    let mailbox = parts[2].to_string();
    let mut rest = parts[3].chars();
    let conn_type = rest.next()?;
    let key = rest.as_str().to_string();
    if host.is_empty() || mailbox.is_empty() || key.is_empty() { return None; }
    Some(UcsInfo { host, port, mailbox, conn_type, key })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_set_chat_server_payload() {
        // host,port,shortname.charname,<typeChar><8charKey>  (NUL-terminated)
        let raw = b"127.0.0.1,7778,peq.Claude,C12345678\0";
        let info = parse_set_chat_server(raw).expect("parse");
        assert_eq!(info.host, "127.0.0.1");
        assert_eq!(info.port, 7778);
        assert_eq!(info.mailbox, "peq.Claude");
        assert_eq!(info.conn_type, 'C');
        assert_eq!(info.key, "12345678");
    }

    #[test]
    fn rejects_malformed() {
        assert!(parse_set_chat_server(b"127.0.0.1,7778\0").is_none());      // too few fields
        assert!(parse_set_chat_server(b"h,notaport,m.c,Ckey\0").is_none()); // bad port
        assert!(parse_set_chat_server(b"").is_none());
    }
}
