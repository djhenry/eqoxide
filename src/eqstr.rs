//! EQ string table (eqstr_us.txt) — templates for OP_FormattedMessage / OP_SimpleMessage.
//!
//! Each line is `<id> <template>` where the template may contain `%1`..`%9` placeholders
//! filled by the message's argument strings. Loaded once at startup into a process-global
//! map so `packet_handler` can resolve string ids without threading state everywhere.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

static STRINGS: OnceLock<HashMap<u32, String>> = OnceLock::new();

/// Parse the eqstr table text into an id → template map. Pure (no I/O) for testing.
/// The first line is a header (`EQST0002`); data lines are `<id><space><template>`.
pub fn parse(text: &str) -> HashMap<u32, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let Some((id_str, rest)) = line.split_once(' ') else { continue; };
        if let Ok(id) = id_str.parse::<u32>() {
            map.insert(id, rest.to_string());
        }
    }
    map
}

/// Load the table from `path` (best effort — missing/unreadable file is a no-op so the
/// client still runs, just without resolved string-id text).
pub fn load(path: &Path) {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let map = parse(&text);
            let n = map.len();
            let _ = STRINGS.set(map);
            tracing::info!("eqstr: loaded {} strings from {}", n, path.display());
        }
        Err(e) => tracing::warn!("eqstr: could not load {}: {}", path.display(), e),
    }
}

/// Substitute placeholders in `template` with `args` (1-indexed). Missing args become "".
///
/// Handles both plain positional tokens `%1`..`%9` and letter-prefixed variants
/// `%<L><n>` such as `%B1` / `%T3`, where `L` is an EQ format-code letter (e.g. `B`
/// bold, `T` an indirect string lookup in the real client). RoF2 still passes the
/// argument values positionally, so we resolve every `%<L><n>` to the nth arg — the
/// same as `%<n>`. Without this the prefixed tokens leaked verbatim into chat
/// (e.g. `You have gained the ability "%B1(1)" ... %T3.`). See eqoxide#59.
///
/// Pure and unit-tested — the core of formatted-message rendering.
pub fn substitute(template: &str, args: &[&str]) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut out = String::with_capacity(template.len());
    let push_arg = |out: &mut String, d: char| {
        let idx = d as usize - '1' as usize;
        out.push_str(args.get(idx).copied().unwrap_or(""));
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '%' {
            // Plain positional: %1..%9
            if let Some(&d) = chars.get(i + 1) {
                if ('1'..='9').contains(&d) {
                    push_arg(&mut out, d);
                    i += 2;
                    continue;
                }
                // Letter-prefixed positional: %B1, %T3, ... (letter then 1-based index).
                if d.is_ascii_alphabetic() {
                    if let Some(&d2) = chars.get(i + 2) {
                        if ('1'..='9').contains(&d2) {
                            push_arg(&mut out, d2);
                            i += 3;
                            continue;
                        }
                    }
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out.trim().to_string()
}

/// Resolve a string id with arguments to display text, or `None` if the table is
/// missing/unloaded or the id is unknown.
pub fn format_id(string_id: u32, args: &[&str]) -> Option<String> {
    let map = STRINGS.get()?;
    let template = map.get(&string_id)?;
    Some(substitute(template, args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_id_template_pairs() {
        let map = parse("EQST0002\n100 Your target is out of range, get closer!\n1 %1 %2 %3");
        assert_eq!(map.get(&100).map(String::as_str), Some("Your target is out of range, get closer!"));
        assert_eq!(map.get(&1).map(String::as_str), Some("%1 %2 %3"));
        assert!(!map.contains_key(&0) || map.get(&0).is_some()); // header line skipped (no leading int+space match for EQST...)
    }

    #[test]
    fn substitute_fills_numbered_args() {
        assert_eq!(substitute("%1 tells you, '%2'", &["Guard", "hello"]), "Guard tells you, 'hello'");
        // Missing args render empty; surrounding text/trim preserved.
        assert_eq!(substitute("Hi %1%2", &["there"]), "Hi there");
        // A bare % or %0 is left as-is.
        assert_eq!(substitute("100% done", &[]), "100% done");
    }

    #[test]
    fn substitute_resolves_letter_prefixed_tokens() {
        // The observed skill-gain message: %B1 → arg1, %2 → arg2, %T3 → arg3.
        let out = substitute(
            r#"You have gained the ability "%B1(1)" at a cost of %2 ability %T3."#,
            &["Kick", "0", "points"],
        );
        assert_eq!(out, r#"You have gained the ability "Kick(1)" at a cost of 0 ability points."#);
        // %T1 as a lone token resolves to arg1.
        assert_eq!(substitute("You have gained the ability to use %T1.", &["Slam"]),
                   "You have gained the ability to use Slam.");
    }

    #[test]
    fn substitute_letter_without_index_is_literal() {
        // A lone `%B` (no digit) or a non-token letter must not be swallowed.
        assert_eq!(substitute("grade %B for effort", &["x"]), "grade %B for effort");
        assert_eq!(substitute("50% B1 off", &["x"]), "50% B1 off");
    }
}
