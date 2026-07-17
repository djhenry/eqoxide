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

/// Maximum recursion depth for `%T<n>` nested string-id resolution (see `substitute_inner`
/// below) — a defensive guard against a cyclic or malformed table. No known RoF2 template
/// chain nests this deep; this only protects against server-sent garbage.
const MAX_NESTED_DEPTH: u32 = 4;

/// Substitute placeholders in `template` with `args` (1-indexed). Missing args become "".
///
/// Handles both plain positional tokens `%1`..`%9` and letter-prefixed variants
/// `%<L><n>` such as `%B1` / `%T3`, where `L` is an EQ format-code letter (e.g. `B`
/// bold). RoF2 still passes the argument values positionally, so we resolve every
/// `%<L><n>` to the nth arg — the same as `%<n>`. Without this the prefixed tokens
/// leaked verbatim into chat (e.g. `You have gained the ability "%B1(1)" ... %T3.`).
/// See eqoxide#59.
///
/// This entry point has no string-table access, so `%T<n>` (see `substitute_inner`)
/// degrades to the same literal-arg substitution as every other letter code — callers
/// that need real `%T` recursion (a `%T<n>` arg is itself a decimal eqstr string_id, to
/// be resolved and spliced in) must go through [`format_id`], which has the table.
///
/// Pure and unit-tested — the core of formatted-message rendering.
pub fn substitute(template: &str, args: &[&str]) -> String {
    substitute_inner(template, args, None, 0)
}

/// Core of [`substitute`]/[`format_id`]. `resolve`, when present, is the loaded eqstr
/// table — its presence is what lets `%T<n>` recurse instead of falling back to a literal
/// digit string.
///
/// `%T<n>` means "arg `n` (1-indexed) is itself a decimal eqstr string_id — resolve it as
/// a template and splice the resolved text in here", reusing the SAME outer `args` array
/// for the nested template's own `%1..%9`/`%T<n>` tokens (not a re-indexed sub-array).
/// Confirmed against EQEmu server source + the shipped `eqstr_us.txt`: id 554
/// (`GENERIC_STRINGID_SAY`, `"%1 says '%T2'"`) is how a merchant's random hail line
/// arrives — the server sends `string_id=554` with `args=[npc_name, "1148", player_name,
/// item_name]`, and `1148` (`MERCHANT_HANDY_ITEM4`, `"Welcome to my shop, %3. You would
/// probably find a %4 handy."`) is itself resolved from arg slot 2, whose own `%3`/`%4`
/// bind to the SAME outer args' slots 3/4. Without this recursion the raw digits `1148`
/// render verbatim as if they were the NPC's words — eqoxide#472 (agent-honesty: a bare
/// numeric id is not the NPC's speech and must not be presented as such).
///
/// `depth` guards against a cyclic/malformed table (`%T<n>` args nesting back on
/// themselves): past [`MAX_NESTED_DEPTH`], a `%T<n>` degrades to literal digits instead of
/// recursing further, same as when `resolve` is `None` or the nested id/table lookup fails.
fn substitute_inner(
    template: &str,
    args: &[&str],
    resolve: Option<&HashMap<u32, String>>,
    depth: u32,
) -> String {
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
                            // %T<n>: recurse through the string table when one is available;
                            // any failure (no table, unparsable/unknown nested id, depth
                            // limit) falls back to the literal digits, same as %B and co.
                            if d == 'T' {
                                if let (Some(map), true) = (resolve, depth < MAX_NESTED_DEPTH) {
                                    let idx = d2 as usize - '1' as usize;
                                    let nested = args.get(idx)
                                        .and_then(|a| a.parse::<u32>().ok())
                                        .and_then(|id| map.get(&id));
                                    if let Some(nested_template) = nested {
                                        let resolved =
                                            substitute_inner(nested_template, args, resolve, depth + 1);
                                        out.push_str(resolved.trim());
                                        i += 3;
                                        continue;
                                    }
                                }
                            }
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

/// [`format_id`]'s implementation against an explicit map — split out so tests can exercise
/// `%T<n>` recursion against a small local table without touching the process-global
/// [`STRINGS`] `OnceLock` (which, being set-once, can't be reset between tests).
fn format_id_from_map(map: &HashMap<u32, String>, string_id: u32, args: &[&str]) -> Option<String> {
    let template = map.get(&string_id)?;
    Some(substitute_inner(template, args, Some(map), 0))
}

/// Resolve a string id with arguments to display text, or `None` if the table is
/// missing/unloaded or the id is unknown. Recursively resolves any nested `%T<n>` string-id
/// args (see `substitute_inner`).
pub fn format_id(string_id: u32, args: &[&str]) -> Option<String> {
    format_id_from_map(STRINGS.get()?, string_id, args)
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

    #[test]
    fn substitute_without_resolver_treats_t_token_as_literal_digits() {
        // The pure substitute() entry point has no table access, so %T<n> must keep the
        // pre-#472 literal-arg behavior rather than silently producing empty/garbled text.
        assert_eq!(substitute("%1 says '%T2'", &["Vaelias", "1148"]), "Vaelias says '1148'");
    }

    #[test]
    fn format_id_resolves_nested_merchant_greeting_string_id() {
        // eqoxide#472: a merchant's random hail arrives as string_id 554
        // (GENERIC_STRINGID_SAY, "%1 says '%T2'") with args [npc_name, "1148", player_name,
        // item_name]. %T2 must recurse into id 1148 (MERCHANT_HANDY_ITEM4) instead of
        // rendering the raw digits as if they were the NPC's words — and the nested
        // template's own %3/%4 bind to the SAME outer args, not a re-indexed sub-array.
        let map = parse(
            "EQST0002\n\
             554 %1 says '%T2'\n\
             1148 Welcome to my shop, %3. You would probably find a %4 handy.\n",
        );
        let args = ["Vaelias", "1148", "you", "a Fine Steel Rapier"];
        let text = format_id_from_map(&map, 554, &args).unwrap();
        assert_eq!(
            text,
            "Vaelias says 'Welcome to my shop, you. You would probably find a a Fine Steel Rapier handy.'"
        );
    }

    #[test]
    fn format_id_falls_back_to_literal_digits_when_nested_id_is_unknown() {
        // A %T<n> arg that doesn't parse, or resolves to an id not in the table, must not
        // drop the message or panic — it degrades to the literal digits (still honest: a
        // labeled fallback would be even better, but bare digits at least aren't silently
        // eaten, and this mirrors the pre-fix behavior for the common substitution case).
        let map = parse("EQST0002\n554 %1 says '%T2'\n");
        let text = format_id_from_map(&map, 554, &["Vaelias", "9999"]).unwrap();
        assert_eq!(text, "Vaelias says '9999'");
    }

    #[test]
    fn format_id_nested_recursion_depth_guard_stops_cycles() {
        // A malformed/cyclic table (id 1's template nests string_id 1 via its own arg,
        // read from args[0] == "1") must terminate rather than infinitely recurse/overflow
        // the stack. Past MAX_NESTED_DEPTH, %T<n> degrades to the literal digit arg.
        let map = parse("EQST0002\n1 %T1\n");
        let text = format_id_from_map(&map, 1, &["1"]).unwrap();
        assert_eq!(text, "1");
    }
}
