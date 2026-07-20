//! UCS (Universal Chat Service) connection parameters — a POD relocated DOWN into `eqoxide-core`
//! (#544 Step 2b) so `game_state` (also in core) no longer up-references the higher `eq_net` layer.
//!
//! Only the plain data type lives here. The `OP_SetChatServer` wire parser
//! (`parse_set_chat_server`) stays in `eq_net::ucs`, which re-exports this type — so all existing
//! `crate::eq_net::ucs::UcsInfo` paths across `eq_net` keep resolving unchanged (the #557 pattern).

/// Connection parameters parsed from `OP_SetChatServer`, everything needed to reach the UCS.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UcsInfo {
    pub host:      String, // UCS server host
    pub port:      u16,    // UCS server port
    pub mailbox:   String, // "<shortname>.<charname>" — sent verbatim as the OP_MailLogin MailBox
    pub conn_type: char,   // 1-char connection-type indicator (e.g. RoF2 combined)
    pub key:       String, // mail key (8 chars) the UCS verifies via VerifyMailKey
}
