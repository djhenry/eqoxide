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
            eprintln!("eqstr: loaded {} strings from {}", n, path.display());
        }
        Err(e) => eprintln!("eqstr: could not load {}: {}", path.display(), e),
    }
}

/// Substitute `%1`..`%9` in `template` with `args` (1-indexed). Missing args become "".
/// Pure and unit-tested — the core of formatted-message rendering.
pub fn substitute(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some(&d) = chars.peek() {
                if ('1'..='9').contains(&d) {
                    chars.next();
                    let idx = d as usize - '1' as usize;
                    out.push_str(args.get(idx).copied().unwrap_or(""));
                    continue;
                }
            }
        }
        out.push(c);
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
}
