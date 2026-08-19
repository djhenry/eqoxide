//! Character-name handling for the OP_ApproveName handshake: the casing normalizer the client
//! applies before sending, and a local model of the subset of the server's name rules that is
//! decidable without contacting the server (#1092).
//!
//! **The rules themselves are documented in exactly one place** —
//! `docs/specs/2026-06-26-pregame-screens-design.md`, "Name rules", which carries the full
//! ordered list with an EQEmu file:line for every clause. This module does not restate that
//! list; each check below cites the line it models and nothing more.
//!
//! # This check is NOT complete
//!
//! Two of the server's rules cannot be evaluated here and never will be:
//!
//! - the `name_filter` table (EQEmu `common/database.cpp:870`) — a case-insensitive substring
//!   match against rows of a server-side table the client has no copy of;
//! - uniqueness (`ReserveName`, `world/client.cpp:619`) — whether the name is already taken.
//!
//! **A name this module accepts can still be rejected by the server.** Passing here means only
//! that the locally-decidable rules hold; it is not an approval. What it does buy is the
//! converse, which is what #1092 asked for: a name that fails here would certainly have been
//! rejected, so it is never put on the wire, and the failure names the rule instead of arriving
//! as an unexplained 1-byte `0x00` at the end of a full login handshake.
//!
//! # Locale
//!
//! The server's checks use C `isalpha`/`islower`/`isupper` on individual `char` bytes. EQEmu's
//! world server never calls `setlocale`, so the C locale is in effect and those classifications
//! are the ASCII ones — bytes >= 0x80 are not alphabetic. The `is_ascii_*` predicates below are
//! that model, not an approximation of it.

use std::fmt;

/// Normalize a character name for OP_ApproveName: first character uppercase, the rest
/// lowercase. Required, not cosmetic — EQEmu's `Client::HandleNameApprovalPacket`
/// (`world/client.cpp`) rejects a lower-case first character, or any upper-case character
/// after it. Casing only: length, spaces, `name_filter` and uniqueness are separate rules
/// (docs/specs/2026-06-26-pregame-screens-design.md, "Name rules"); of those, the ones that
/// are decidable locally are checked by [`check_name`], which runs on this function's OUTPUT.
///
/// `char::to_uppercase`/`to_lowercase` are Unicode-correct and therefore neither length- nor
/// case-shape-preserving: `"ßabc"` normalizes to `"SSabc"` and `"ﬁabc"` to `"FIabc"` — pure
/// ASCII carrying an upper-case character *after the first*, i.e. output that violates one of
/// the two rules this function exists to satisfy. That is exactly why [`check_name`] runs on
/// the normalized bytes rather than the raw ones (#1092).
pub fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, c) in name.chars().enumerate() {
        if i == 0 { out.extend(c.to_uppercase()); }
        else      { out.extend(c.to_lowercase()); }
    }
    out
}

/// The one locally-decidable rule a name broke. Ordered exactly as the server evaluates them,
/// so the variant reported is the rule that would actually have fired first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameRule {
    /// Rule 1 — `world/client.cpp:601`. Payload: the length, in bytes, the server would measure.
    Length(usize),
    /// Rule 2 — `world/client.cpp:603`.
    LowerCaseFirst,
    /// Rule 3 — `world/client.cpp:605`.
    Space,
    /// Rule 4, first clause — `common/database.cpp:843`. Payload: the offending byte's index.
    NonAlpha(usize),
    /// Rule 4, second clause — `common/database.cpp:858` (the test is `num_c > 2`, so it takes
    /// three identical bytes to fail, and the comparison is byte-wise, hence case-sensitive).
    /// Payload: the repeated byte.
    ThreeIdentical(u8),
    /// Rule 5 — `world/client.cpp:611`. Payload: the offending byte's index.
    UpperCaseAfterFirst(usize),
}

impl fmt::Display for NameRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            NameRule::Length(n) => write!(
                f, "rule 1 (world/client.cpp:601): the server requires 4-15 bytes, this is {n}"),
            NameRule::LowerCaseFirst => write!(
                f, "rule 2 (world/client.cpp:603): the first character is lower-case"),
            NameRule::Space => write!(
                f, "rule 3 (world/client.cpp:605): the name contains a space"),
            NameRule::NonAlpha(i) => write!(
                f, "rule 4 (common/database.cpp:843): byte {i} is not an ASCII letter"),
            NameRule::ThreeIdentical(b) => write!(
                f, "rule 4 (common/database.cpp:858): three identical consecutive '{}' characters",
                b as char),
            NameRule::UpperCaseAfterFirst(i) => write!(
                f, "rule 5 (world/client.cpp:611): byte {i} is upper-case, and only the first \
                    character may be"),
        }
    }
}

/// A rejected name: the rule it broke, plus the exact bytes the server would have judged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameRuleViolation {
    pub rule: NameRule,
    /// The normalized name as the server's `strlen`-bounded `char_name` would see it. Kept so a
    /// diagnostic can disclose it when normalization changed what the user typed — an error that
    /// silently judges a string nobody wrote is its own confusion.
    pub wire: String,
}

impl fmt::Display for NameRuleViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.rule)
    }
}

impl std::error::Error for NameRuleViolation {}

/// The bytes EQEmu's `HandleNameApprovalPacket` actually measures and scans, given `normalized`
/// as the name field of the OP_ApproveName packet `build_approve_name` builds.
///
/// Two bounds, in this order: `build_approve_name` copies at most 63 bytes into the 64-byte
/// field, and the server then takes `strlen` of it, which stops at the first NUL. Modelling both
/// is what lets the caller say the rule it names is the rule that would have fired. (The 63-byte
/// bound cannot by itself change an accept/reject verdict — anything long enough to be truncated
/// is already over rule 1's 15-byte limit — so this is fidelity, not a fix; see #1092.)
fn wire_bytes(normalized: &str) -> &[u8] {
    let b   = normalized.as_bytes();
    let cap = b.len().min(63);
    let end = b[..cap].iter().position(|&c| c == 0).unwrap_or(cap);
    &b[..end]
}

/// Evaluate the locally-decidable rules against the wire bytes, in the server's order, first
/// failure wins. `None` = every rule this client can check holds.
///
/// Deliberately byte-oriented rather than `char`-oriented: every server-side test here is over
/// `char` bytes of a C string, so a `char`-oriented model would disagree with it on exactly the
/// inputs that matter (multi-byte UTF-8).
fn first_broken_rule(w: &[u8]) -> Option<NameRule> {
    // Rule 1. `CheckNameFilter` repeats a 4..=15 bound of its own (common/database.cpp:833 and
    // :838), but it is unreachable — this test runs first and is the same bound.
    if !(4..=15).contains(&w.len()) {
        return Some(NameRule::Length(w.len()));
    }
    // Rule 2. `normalize_name` uppercases the first character, so this is not expected to fire on
    // the current send path; it is modelled anyway so the ordering is faithful and so this check
    // does not silently depend on the normalizer's internals.
    if w[0].is_ascii_lowercase() {
        return Some(NameRule::LowerCaseFirst);
    }
    // Rule 3.
    if w.contains(&b' ') {
        return Some(NameRule::Space);
    }
    // Rule 4, first clause.
    if let Some(i) = w.iter().position(|b| !b.is_ascii_alphabetic()) {
        return Some(NameRule::NonAlpha(i));
    }
    // Rule 4, second clause: a run of three identical bytes.
    let mut run  = 0usize;
    let mut prev = 0u8; // the server seeds its comparand with '\0', which no wire byte can be
    for &b in w {
        if b == prev { run += 1; } else { run = 1; prev = b; }
        if run > 2 {
            return Some(NameRule::ThreeIdentical(b));
        }
    }
    // Rule 5.
    if let Some(i) = w.iter().skip(1).position(|b| b.is_ascii_uppercase()) {
        return Some(NameRule::UpperCaseAfterFirst(i + 1));
    }
    None
}

/// Check a raw, as-configured character name against the locally-decidable subset of the
/// server's name rules.
///
/// Takes the **raw** name and normalizes internally, because the string whose acceptability
/// matters is the one that goes on the wire, and the two differ in ways that change the verdict
/// in both directions (see [`normalize_name`]). Callers cannot get that ordering wrong because
/// there is no entry point that skips it.
///
/// `Ok(())` does **not** mean the server will accept the name — see this module's docs. It means
/// no rule that can be decided without the server is broken.
pub fn check_name(raw: &str) -> Result<(), NameRuleViolation> {
    let normalized = normalize_name(raw);
    let w          = wire_bytes(&normalized);
    match first_broken_rule(w) {
        None       => Ok(()),
        Some(rule) => Err(NameRuleViolation {
            rule,
            wire: String::from_utf8_lossy(w).into_owned(),
        }),
    }
}

/// One-line diagnostic for a rejected `raw` name: names the rule, quotes what the user actually
/// typed, discloses the normalized form only when normalization changed it, and states plainly
/// that the local check is partial.
pub fn describe_violation(raw: &str, v: &NameRuleViolation) -> String {
    let sent = if v.wire == raw {
        String::new()
    } else {
        format!(" (sent to the server as '{}' after case normalization)", v.wire)
    };
    format!(
        "character name '{raw}'{sent} breaks {v}. \
         See docs/specs/2026-06-26-pregame-screens-design.md, \"Name rules\". \
         Note this check is partial: the server's name_filter table and the uniqueness check are \
         server-side only, so a name that passes here can still be rejected."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule the fleet actually types every day: a lower-case config name is normalized and
    /// accepted. Guards against a validator that rejects valid names.
    #[test]
    fn ordinary_name_passes() {
        assert_eq!(check_name("mordeth"), Ok(()));
        assert_eq!(check_name("Aiquestbot"), Ok(()));
        assert_eq!(check_name("Fixissuestwo"), Ok(()));
    }

    /// Rule 1, both ends. 15 bytes is the last accepted length (`ValueWithin` is inclusive).
    #[test]
    fn rule_1_length() {
        assert_eq!(check_name("Al").unwrap_err().rule,  NameRule::Length(2));
        assert_eq!(check_name("").unwrap_err().rule,    NameRule::Length(0));
        assert_eq!(check_name("Abc").unwrap_err().rule, NameRule::Length(3));
        assert_eq!(check_name("Abcd"), Ok(()));            // 4  = lower bound, inclusive
        assert_eq!(check_name("Abcdefghijklmno"), Ok(())); // 15 = upper bound, inclusive
        assert_eq!(check_name("Abcdefghijklmnop").unwrap_err().rule, NameRule::Length(16));
    }

    /// Rule 2 is unreachable through `check_name` (the normalizer uppercases byte 0), so it is
    /// exercised on the wire-level model directly. Modelled so the ordering stays faithful.
    #[test]
    fn rule_2_lower_case_first_is_modelled_but_normalizer_prevents_it() {
        assert_eq!(first_broken_rule(b"mordeth"), Some(NameRule::LowerCaseFirst));
        // …and through the public entry point the same input is fine, because it is normalized.
        assert_eq!(check_name("mordeth"), Ok(()));
    }

    #[test]
    fn rule_3_space() {
        assert_eq!(check_name("Bo bby").unwrap_err().rule, NameRule::Space);
    }

    /// Rule 4, first clause: every byte must be an ASCII letter in the C locale.
    #[test]
    fn rule_4_non_alpha() {
        assert_eq!(check_name("Ab1cd").unwrap_err().rule,  NameRule::NonAlpha(2));
        assert_eq!(check_name("Ab-cd").unwrap_err().rule,  NameRule::NonAlpha(2));
    }

    /// Rule 4, second clause. The server's test is `num_c > 2` over BYTES, so it takes three
    /// identical characters, and `A` != `a`.
    ///
    /// #1092's example `"Aaardvark"` is given there as a locally-provable failure; measured
    /// against `common/database.cpp:848-861` it is not one. Its longest run is `a`,`a` — two —
    /// because the leading `A` is a different byte. It is kept here as the boundary case.
    #[test]
    fn rule_4_three_identical() {
        assert_eq!(check_name("Aaardvark"), Ok(()));           // run of 2 — accepted
        assert_eq!(check_name("Aaaardvark").unwrap_err().rule, // run of 3 — rejected
                   NameRule::ThreeIdentical(b'a'));
        assert_eq!(check_name("Abbbcd").unwrap_err().rule, NameRule::ThreeIdentical(b'b'));
    }

    #[test]
    fn rule_5_upper_case_after_first() {
        assert_eq!(first_broken_rule(b"AbCde"), Some(NameRule::UpperCaseAfterFirst(2)));
    }

    /// The three Unicode cases #1092 recorded, on the real normalizer.
    ///
    /// CHOSEN BEHAVIOUR: all three are REJECTED locally, and the rule named is the one the server
    /// would have fired first. That falls out of validating the normalized bytes rather than the
    /// raw ones — normalization laundering `ß`/`ﬁ` into pure ASCII is precisely what makes rule 4
    /// pass them, leaving rule 5 as the only thing that rejects them. A raw-oriented validator
    /// would have to guess, and would guess wrong.
    #[test]
    fn issue_1092_unicode_cases() {
        // "ßabc" -> "SSabc": pure ASCII, so rule 4 accepts it; the second `S` trips rule 5.
        let v = check_name("ßabc").unwrap_err();
        assert_eq!(v.wire, "SSabc");
        assert_eq!(v.rule, NameRule::UpperCaseAfterFirst(1));

        // "ﬁabc" -> "FIabc": same shape — the ligature expands to two ASCII capitals.
        let v = check_name("ﬁabc").unwrap_err();
        assert_eq!(v.wire, "FIabc");
        assert_eq!(v.rule, NameRule::UpperCaseAfterFirst(1));

        // "ŉabc" -> "ʼNabc": NOT pure ASCII (U+02BC is two bytes), so rule 4 fires first — which
        // is the server's order too, since CheckNameFilter is evaluated before the isupper scan.
        let v = check_name("ŉabc").unwrap_err();
        assert_eq!(v.wire, "ʼNabc");
        assert_eq!(v.rule, NameRule::NonAlpha(0));
    }

    /// The decision under test: the validator judges the NORMALIZED bytes, not the raw ones.
    ///
    /// `"aaardvark"` has three identical consecutive bytes and would break rule 4 if it were sent
    /// as typed — but it is not: normalization capitalises the first `a`, leaving a run of two,
    /// and the server accepts it. A validator run on the raw string would reject a name the
    /// server takes. This is the ASCII-only demonstration that the choice is not cosmetic.
    #[test]
    fn validator_judges_the_normalized_bytes_not_the_raw_ones() {
        assert_eq!(first_broken_rule(b"aaardvark"),        // raw bytes: rule 4 (and rule 2)
                   Some(NameRule::LowerCaseFirst));
        assert_eq!(first_broken_rule(b"Aaardvark"), None); // what is actually sent: accepted
        assert_eq!(check_name("aaardvark"), Ok(()));       // so the public check accepts it
    }

    /// A name long enough to hit `build_approve_name`'s 63-byte copy bound still reports rule 1:
    /// truncation cannot rescue a name, because anything truncated is already over 15 bytes.
    #[test]
    fn truncation_cannot_change_the_verdict() {
        let long = "a".repeat(200);
        assert_eq!(check_name(&long).unwrap_err().rule, NameRule::Length(63));
    }

    /// The diagnostic discloses the normalized form only when it differs from what was typed,
    /// and always says the check is partial.
    #[test]
    fn describe_violation_discloses_normalization_and_partiality() {
        let v = check_name("ßabc").unwrap_err();
        let m = describe_violation("ßabc", &v);
        assert!(m.contains("'ßabc'"), "{m}");
        assert!(m.contains("after case normalization"), "{m}");
        assert!(m.contains("SSabc"), "{m}");
        assert!(m.contains("rule 5"), "{m}");
        assert!(m.contains("can still be rejected"), "{m}");

        // Unchanged by normalization -> no "sent as" clause to confuse the reader.
        let v = check_name("Bo bby").unwrap_err();
        let m = describe_violation("Bo bby", &v);
        assert!(!m.contains("after case normalization"), "{m}");
        assert!(m.contains("rule 3"), "{m}");
    }
}
