//! `/v1/guild/*` — guild membership: roster (who's in the guild + online status) and the
//! join/leave/invite/remove actions. Mirrors `/v1/group/*`. Guild identity (name/id/rank) is also
//! surfaced on `/v1/observe/debug`. (#295)

use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use super::*;
use crate::refusal::Refusal;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/roster", get(get_roster))
        .route("/invite", post(post_invite))
        .route("/accept", post(post_accept))
        .route("/leave", post(post_leave))
        .route("/remove", post(post_remove))
}

/// GET /v1/guild/roster — the player's guild identity and full member roster. `members` empty (and
/// `guild_id` 0) means not in a guild. Each member carries online status + last-seen zone so an
/// agent can route guild messages to who's actually present.
async fn get_roster(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let g = s.guild_slots.guild.lock().unwrap();
    let members: Vec<serde_json::Value> = g.members.iter().map(|m| serde_json::json!({
        "name":    m.name,
        "rank":    m.rank,
        "level":   m.level,
        "class":   m.class,
        "zone_id": m.zone_id,
        "online":  m.online,
        "public_note": m.public_note,
    })).collect();
    Json(serde_json::json!({
        "guild":          g.guild_name,
        "guild_id":       g.guild_id,
        "guild_rank":     g.guild_rank,
        "pending_invite": g.pending_invite,
        "members":        members,
    }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NameBody { name: String }

fn extract_name(body: Result<Json<NameBody>, axum::extract::rejection::JsonRejection>) -> Result<String, (StatusCode, String)> {
    match body {
        Ok(Json(b)) if !b.name.trim().is_empty() => Ok(b.name),
        _ => Err((StatusCode::BAD_REQUEST, "provide {\"name\":\"X\"}".into())),
    }
}

/// 409 CONFLICT body for an occupied guild-action slot. This module already refused rather than
/// overwrote before #347 (it is the pattern the other domains were converted to); the wording is
/// unchanged so an existing caller's string match still works.
const BUSY_GUILD: &str = "a guild action is already pending";

/// Queue a single guild action (rejecting if one is already pending and undrained).
fn queue(s: &HttpState, action: GuildAction) -> (StatusCode, String) {
    let msg = match &action {
        GuildAction::Invite(n) => format!("inviting {n} to the guild"),
        GuildAction::Accept    => "accepting guild invite".into(),
        GuildAction::Leave     => "leaving guild".into(),
        GuildAction::Remove(n) => format!("removing {n} from the guild"),
    };
    if let Some(busy) = s.command.request_guild_action(action).refused(BUSY_GUILD) { return busy; }
    (StatusCode::OK, msg)
}

/// POST /v1/guild/invite {"name":"X"} — invite player X to our guild (requires invite rights).
async fn post_invite(
    State(s): State<HttpState>,
    body: Result<Json<NameBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let name = match extract_name(body) { Ok(n) => n, Err(e) => return e };
    queue(&s, GuildAction::Invite(name))
}

/// POST /v1/guild/accept — accept a pending guild invite. 400 if none is pending.
async fn post_accept(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if s.guild_slots.guild.lock().unwrap().pending_invite.is_none() {
        return (StatusCode::BAD_REQUEST, "no pending guild invite".into());
    }
    queue(&s, GuildAction::Accept)
}

/// POST /v1/guild/leave — leave the current guild.
async fn post_leave(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if s.guild_slots.guild.lock().unwrap().guild_id == 0 {
        return (StatusCode::BAD_REQUEST, "not in a guild".into());
    }
    queue(&s, GuildAction::Leave)
}

/// POST /v1/guild/remove {"name":"X"} — remove member X (guild leader / GM only, server-enforced).
async fn post_remove(
    State(s): State<HttpState>,
    body: Result<Json<NameBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let name = match extract_name(body) { Ok(n) => n, Err(e) => return e };
    queue(&s, GuildAction::Remove(name))
}

/// #347 step 2, crate-wide guard. Lives here because `guild.rs` is where the 409-on-occupied
/// pattern originated — its `queue()` above was the only handler in the crate that checked whether
/// its command was actually accepted, and #347 generalized it to every other domain.
#[cfg(test)]
mod no_silent_overwrite_guard {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    /// Canonical refusal call sites in this crate, measured on this tree. A ratchet, not a target:
    /// adding a route is free, losing one is RED. See the assertion below for why it is not a loose
    /// floor any more.
    ///
    /// Reconciliation, since the round-2 review counted 39 by grep: the normalised scanner and the
    /// old line-based one enumerate the SAME 38 sites (verified by diffing both against `3c495c1`
    /// — no site in one and not the other), so the rewrite lost nothing. A bare
    /// `grep -c 's\.command\.request_'` runs higher because it also counts prose and, now, the
    /// snippets inside this module's own table test.
    const CANONICAL_SITES: usize = 38;

    fn crate_src() -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src") }
    fn command_src() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../eqoxide-command/src")
    }

    fn rs_files(dir: &PathBuf) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir).expect("readable source dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "rs"))
            .collect();
        v.sort();
        v
    }

    /// Every `CommandState::request_*` that can REFUSE, i.e. whose signature returns `bool`.
    ///
    /// Search key (re-runnable by hand):
    ///   `grep -rn 'pub fn request_' crates/eqoxide-command/src/*.rs`
    /// A signature may span several lines, so the whole thing up to the opening `{` is accumulated
    /// before testing it for `-> bool`.
    fn refusable_requests() -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for path in rs_files(&command_src()) {
            let text = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim_start();
                let Some(rest) = trimmed.strip_prefix("pub fn request_") else { continue };
                let name = format!("request_{}", rest.split(['(', '<']).next().unwrap_or(""));
                let mut sig = String::new();
                for l in &lines[i..] {
                    sig.push_str(l);
                    if l.contains('{') { break; }
                }
                if sig.contains("-> bool") {
                    out.insert(name);
                }
            }
        }
        out
    }

    /// Normalised source, so that this guard cannot be evaded — or silently switched off — by
    /// reformatting.
    ///
    /// **Round-2 review, R2-1:** the previous version enumerated call sites with
    /// `line.find("s.command.request_")`, a single-LINE needle, and Rust lets a receiver wrap. So
    /// `let _busy = s\n.command\n.request_sit(true)\n.refused(BUSY_SIT);` was never enumerated at
    /// all — the shape check below was never reached — and shipped fully green. Worse, the
    /// *correctly* written statement disappeared from the guarded set the moment anyone reflowed
    /// it, with no signal. Coverage must not be a function of source formatting, which nothing
    /// enforces.
    ///
    /// Returns the source with comments removed, string and char literals blanked (so that a `{`,
    /// `;` or `//` inside `format!("… {c} …")` or `split('"')` cannot be read as punctuation),
    /// whitespace runs collapsed to a single space, and spaces adjacent to `.` dropped — that last
    /// step is what makes a wrapped method chain byte-identical to its one-line spelling. The
    /// second return value maps every BYTE offset of the normalised text to its 1-based source
    /// line. Non-ASCII characters are replaced by `_` so that byte offsets stay char boundaries.
    fn normalize(src: &str) -> (String, Vec<usize>) {
        let chars: Vec<char> = src.chars().collect();
        let mut kept: Vec<(char, usize)> = Vec::with_capacity(chars.len());
        let mut line = 1usize;
        let mut i = 0usize;
        while i < chars.len() {
            let c = chars[i];
            if c == '\n' { line += 1; i += 1; kept.push((' ', line)); continue; }
            if c.is_whitespace() { i += 1; kept.push((' ', line)); continue; }
            if c == '/' && chars.get(i + 1) == Some(&'/') {
                while i < chars.len() && chars[i] != '\n' { i += 1; }
                kept.push((' ', line));
                continue;
            }
            if c == '/' && chars.get(i + 1) == Some(&'*') {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    if chars[i] == '\n' { line += 1; }
                    i += 1;
                }
                i = (i + 2).min(chars.len());
                kept.push((' ', line));
                continue;
            }
            // Raw string: r"…", r#"…"#, r##"…"##
            if c == 'r' && matches!(chars.get(i + 1), Some('"') | Some('#')) {
                let mut j = i + 1;
                let mut hashes = 0usize;
                while chars.get(j) == Some(&'#') { hashes += 1; j += 1; }
                if chars.get(j) == Some(&'"') {
                    j += 1;
                    while j < chars.len() {
                        if chars[j] == '\n' { line += 1; }
                        if chars[j] == '"' && (1..=hashes).all(|k| chars.get(j + k) == Some(&'#')) {
                            j += hashes + 1;
                            break;
                        }
                        j += 1;
                    }
                    i = j;
                    kept.push(('"', line));
                    kept.push(('"', line));
                    continue;
                }
            }
            if c == '"' {
                let mut j = i + 1;
                while j < chars.len() {
                    if chars[j] == '\\' { j += 2; continue; }
                    if chars[j] == '\n' { line += 1; }
                    if chars[j] == '"' { j += 1; break; }
                    j += 1;
                }
                i = j;
                kept.push(('"', line));
                kept.push(('"', line));
                continue;
            }
            // `'x'` / `'\n'` are literals; `'a` in `&'a str` is a lifetime and must pass through.
            if c == '\'' {
                let escaped = chars.get(i + 1) == Some(&'\\');
                let closes = if escaped { chars.get(i + 3) == Some(&'\'') }
                             else { chars.get(i + 2) == Some(&'\'') };
                if closes {
                    i += if escaped { 4 } else { 3 };
                    kept.push(('\'', line));
                    kept.push(('\'', line));
                    continue;
                }
            }
            kept.push((if c.is_ascii() { c } else { '_' }, line));
            i += 1;
        }

        // Pass 1: collapse whitespace runs (and drop any leading space).
        let mut collapsed: Vec<(char, usize)> = Vec::with_capacity(kept.len());
        for &(c, l) in &kept {
            if c == ' ' && (collapsed.is_empty() || collapsed.last().unwrap().0 == ' ') { continue; }
            collapsed.push((c, l));
        }
        // Pass 2: a space touching `.` is formatting, not syntax — this is the whole point.
        let mut out = String::new();
        let mut map: Vec<usize> = Vec::new();
        for (idx, &(c, l)) in collapsed.iter().enumerate() {
            if c == ' ' {
                let prev = idx.checked_sub(1).map(|p| collapsed[p].0);
                let next = collapsed.get(idx + 1).map(|n| n.0);
                if prev == Some('.') || next == Some('.') { continue; }
            }
            out.push(c);
            while map.len() < out.len() { map.push(l); }
        }
        (out, map)
    }

    /// The ONE accepted spelling of a refusal at an HTTP call site:
    ///
    /// ```text
    /// if let Some(busy) = s.command.request_x(..).refused(MSG) { return busy; }
    /// ```
    ///
    /// **Round-2 review, R2-3.** This predicate is the sole defence for the majority of refusal
    /// sites (the reviewer counted 15 behavioural 409 tests against 39 sites), and until now
    /// nothing stopped anyone relaxing it: reverting it to `contains("s.command.request_")` — the
    /// literal round-1 B1 hole — left the whole suite green, because every site still landed in
    /// `checked` and the anti-vacuity floors had ~14 sites of slack. A doc comment saying "do not
    /// weaken this" is not an enforcement mechanism.
    ///
    /// So it is extracted here and pinned by [`the_call_site_predicate_accepts_only_the_canonical_shape`],
    /// a table test over literal source snippets. The table includes the wrapped-receiver form, so
    /// it would also have caught R2-1 at design time, without a build.
    ///
    /// **Round-3 review, R3-1.** The round-2 version of that sentence read "relaxing this function
    /// fails that test", and it was FALSE. The predicate is a three-way conjunction, but every
    /// negative row in the table failed on conjunct 1 (`starts_with`) alone, so no row ever reached
    /// conjuncts 2 or 3 — deleting either of them left the round-2 suite green at `240 passed;
    /// 0 failed` (the round-3 reviewer's measurement, mutations M-R3a and M-R3c; not re-run here,
    /// because the code it applied to no longer exists). With conjunct 3 gone, a handler could bind
    /// the refusal and then throw it away — `{ tracing::warn!("busy: {busy:?}"); }` — and answer
    /// `200` to a refused write, which is #347 on the exact route this patch protects (M-R3b).
    /// A conjunction is only as pinned as its least-covered conjunct.
    ///
    /// Hence the fault vector rather than a bare `bool`: each conjunct is numbered, the table
    /// asserts the exact set of conjuncts a snippet violates, and an anti-vacuity assertion
    /// requires every conjunct to be the SOLE fault of at least one row. Deleting any single
    /// conjunct therefore turns that row's expectation from `[n]` into `[]` and fails the test.
    fn refusal_shape_faults(stmt: &str) -> Vec<u8> {
        let mut faults = Vec::new();
        // 1: the statement is the `if let Some(busy) = …` binding, not `if !…`, `let ok = …`,
        //    `if let Some(_) = …`, or a wrapped discard.
        if !stmt.starts_with("if let Some(busy) = s.command.request_") { faults.push(1); }
        // 2: the refusal goes through the one polarity site, `refusal::Refusal`.
        if !(stmt.contains(".refused(") || stmt.contains(".refused_json(")) { faults.push(2); }
        // 3: the bound refusal is actually RETURNED, not logged, counted, or dropped.
        if !stmt.contains("{ return busy;") { faults.push(3); }
        faults
    }

    #[derive(Debug)]
    struct Site {
        line: usize,
        name: String,
        /// Which conjuncts of [`refusal_shape_faults`] this site violates; empty = canonical.
        /// Carried rather than collapsed to a `bool` so the table test can pin each conjunct
        /// separately — see R3-1 on [`refusal_shape_faults`].
        faults: Vec<u8>,
    }

    impl Site {
        fn canonical(&self) -> bool { self.faults.is_empty() }
    }

    /// Every `s.command.request_*` call site in `src`, found on the NORMALISED text so that the
    /// enumeration is whitespace-insensitive, each carrying its [`refusal_shape_faults`] verdict.
    ///
    /// The statement bounds are the previous `;`/`{`/`}` and — because the canonical shape carries
    /// its `return busy;` inside a block — the first `;` after the statement's opening brace, when
    /// a brace comes before the next `;`.
    fn refusal_sites(src: &str) -> Vec<Site> {
        const NEEDLE: &str = "s.command.request_";
        let (text, map) = normalize(src);
        let mut out = Vec::new();
        for (pos, _) in text.match_indices(NEEDLE) {
            let name: String = text[pos + "s.command.".len()..]
                .chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            let raw_start = text[..pos].rfind([';', '{', '}']).map(|b| b + 1).unwrap_or(0);
            let start = raw_start + (text[raw_start..].len() - text[raw_start..].trim_start().len());
            let after = &text[pos..];
            let end = match (after.find('{'), after.find(';')) {
                (Some(b), Some(s)) if b < s => {
                    after[b..].find(';').map(|k| pos + b + k + 1).unwrap_or(text.len())
                }
                (_, Some(s)) => pos + s + 1,
                (_, None) => text.len(),
            };
            let stmt = &text[start..end.max(start)];
            out.push(Site {
                line: map.get(pos).copied().unwrap_or(0),
                name,
                faults: refusal_shape_faults(stmt),
            });
        }
        out
    }

    /// R2-1 + R2-3 + R3-1, pinned. Every row is a literal source snippet; the expectation is the
    /// EXACT set of [`refusal_shape_faults`] conjuncts it violates (`None` = no site enumerated at
    /// all, `Some([])` = canonical). A return to line-oriented detection flips the wrapped rows to
    /// `None`.
    ///
    /// **Why fault sets and not a `bool` (R3-1).** With a `bool` expectation this table was green
    /// under deletion of conjunct 2 *or* conjunct 3, because every rejected row also failed conjunct
    /// 1 and `false` is `false` however you reach it. The exact-set form plus the per-conjunct
    /// anti-vacuity assertion below makes each conjunct independently load-bearing: for each `n`
    /// there is a row whose only fault is `n`, so deleting conjunct `n` turns that row's answer into
    /// `[]` and this test goes RED.
    #[test]
    fn the_call_site_predicate_accepts_only_the_canonical_shape() {
        // (label, snippet, expected: None = no site enumerated, Some(exact fault set))
        let cases: Vec<(&str, &str, Option<Vec<u8>>)> = vec![
            (
                "canonical, one line",
                r#"fn h() { if let Some(busy) = s.command.request_sit(true).refused(BUSY_SIT) { return busy; } }"#,
                Some(vec![]),
            ),
            (
                "canonical, receiver wrapped across lines (R2-1: correct code, reflowed)",
                "fn h() {\n    if let Some(busy) = s\n        .command\n        .request_sit(true)\n        .refused(BUSY_SIT) { return busy; }\n}",
                Some(vec![]),
            ),
            (
                "M-R2a: the reviewer's wrapped discard — must be FOUND and rejected",
                "fn h() {\n    let _busy = s\n        .command\n        .request_sit(true)\n        .refused(BUSY_SIT);\n}",
                Some(vec![1, 3]),
            ),
            (
                "round-1 B1: `if !…`",
                r#"fn h() { if !s.command.request_sit(true) { return (StatusCode::CONFLICT, BUSY_SIT.into()); } }"#,
                Some(vec![1, 2, 3]),
            ),
            (
                "round-1 B1 inverted: `if …`",
                r#"fn h() { if s.command.request_sit(true) { return (StatusCode::CONFLICT, BUSY_SIT.into()); } }"#,
                Some(vec![1, 2, 3]),
            ),
            (
                "bound and ignored",
                r#"fn h() { let ok = s.command.request_sit(true); }"#,
                Some(vec![1, 2, 3]),
            ),
            (
                "M-B1f: empty-bodied `if let Some(_)`",
                r#"fn h() { if let Some(_) = s.command.request_sit(true).refused(BUSY_SIT) { } }"#,
                Some(vec![1, 3]),
            ),
            (
                "canonical via refused_json",
                r#"fn h() { if let Some(busy) = s.command.request_buy(x).refused_json(json!({"e":1})) { return busy; } }"#,
                Some(vec![]),
            ),
            // R3-1, conjunct 1 alone. This form is BEHAVIOURALLY CORRECT — it returns the refusal —
            // and the guard rejects it anyway, because #347's whole point is that there is one
            // spelling to audit. Keeping that as a row makes the cost explicit instead of leaving
            // it as a surprise, and it is the only row whose sole fault is conjunct 1.
            (
                "R3-1: `match` instead of `if let` — correct, but not the canonical spelling",
                r#"fn h() { match s.command.request_sit(true).refused(BUSY_SIT) { Some(busy) => { return busy; } None => {} } }"#,
                Some(vec![1]),
            ),
            // R3-1, conjunct 2 alone: correct binding, correct return, but the refusal is built by
            // some other helper instead of the one polarity site. Nothing else in this table can
            // fail here, so conjunct 2 exists or this row is wrong.
            (
                "R3-1: bypasses `Refusal` — fails conjunct 2 and NOTHING else",
                r#"fn h() { if let Some(busy) = s.command.request_sit(true).busy_response(BUSY_SIT) { return busy; } }"#,
                Some(vec![2]),
            ),
            // R3-1, conjunct 3 alone: this is M-R3b, the composite the reviewer shipped green in
            // round 2 — it answers 200 on a refused write, i.e. #347 on the very route this patch
            // is meant to protect.
            (
                "M-R3b: binds the refusal, logs it, returns 200 — fails conjunct 3 and NOTHING else",
                r#"fn h() { if let Some(busy) = s.command.request_sit(true).refused(BUSY_SIT) { tracing::warn!("busy: {busy:?}"); } }"#,
                Some(vec![3]),
            ),
            (
                "prose in a doc comment is not a call site",
                "//! use `if let Some(busy) = s.command.request_sit(true).refused(BUSY_SIT)`\nfn h() {}",
                None,
            ),
            (
                "prose in a trailing line comment is not a call site",
                "fn h() {} // s.command.request_sit(true) is the shape\n",
                None,
            ),
            (
                "a `{` inside a string literal must not be read as a statement boundary",
                "fn h() {\n    let _ = format!(\"{x} queued; now\");\n    if let Some(busy) = s.command.request_sit(true).refused(BUSY_SIT) { return busy; }\n}",
                Some(vec![]),
            ),
            (
                "a `{` inside a raw string must not be read as a statement boundary",
                "fn h() {\n    let _ = r#\"{\"a\":1};\"#;\n    if let Some(busy) = s.command.request_sit(true).refused(BUSY_SIT) { return busy; }\n}",
                Some(vec![]),
            ),
            (
                "a quote inside a char literal must not open a string",
                "fn h() {\n    let _ = x.split('\"').next();\n    if let Some(busy) = s.command.request_sit(true).refused(BUSY_SIT) { return busy; }\n}",
                Some(vec![]),
            ),
        ];

        let mut wrong: Vec<String> = Vec::new();
        for (label, snippet, expected) in &cases {
            let sites = refusal_sites(snippet);
            let got = match sites.len() {
                0 => None,
                1 => Some(sites[0].faults.clone()),
                n => { wrong.push(format!("{label}: expected at most one site, found {n}")); continue; }
            };
            if got != *expected {
                wrong.push(format!("{label}: expected {expected:?}, got {got:?} ({sites:?})"));
            }
            // `Site::canonical` is the form the scanner's callers read; it must agree with the
            // fault vector on every row, so a future edit cannot fix one and leave the other behind.
            if let (Some(g), Some(site)) = (got.as_ref(), sites.first()) {
                if site.canonical() != g.is_empty() {
                    wrong.push(format!("{label}: canonical()/faults disagree ({site:?})"));
                }
            }
        }
        assert!(wrong.is_empty(), "the call-site predicate has drifted:\n  {}", wrong.join("\n  "));

        // Anti-vacuity: the table must contain every verdict shape and at least one wrapped
        // snippet, otherwise a future edit could satisfy it with a predicate that answers a
        // constant.
        assert!(cases.iter().any(|c| c.2.as_deref() == Some(&[][..])), "no accepted row");
        assert!(cases.iter().any(|c| c.2.as_ref().is_some_and(|f| !f.is_empty())), "no rejected row");
        assert!(cases.iter().any(|c| c.2.is_none()), "no not-a-call-site row");
        assert!(
            cases.iter().filter(|c| c.1.contains("\n        .command")).count() >= 2,
            "the table must keep BOTH wrapped-receiver rows — they are what pins R2-1"
        );

        // R3-1: every conjunct must be the SOLE fault of some row. Without this, a conjunct can be
        // deleted with the whole table still green — which is exactly what M-R3a and M-R3c did to
        // the round-2 version of this test.
        for n in 1u8..=3 {
            assert!(
                cases.iter().any(|c| c.2.as_deref() == Some(&[n][..])),
                "conjunct {n} of refusal_shape_faults is not independently pinned: no row fails it \
                 and only it, so deleting it would leave this test green"
            );
        }
        // …and the conjunct numbering must not have silently grown past what the loop above covers.
        assert!(
            refusal_shape_faults("").iter().copied().eq(1u8..=3),
            "refusal_shape_faults gained or lost a conjunct; extend the anti-vacuity loop to match \
             (an empty statement must fail every conjunct, in order)"
        );
    }

    /// The universal at the HTTP boundary: a handler that calls a refusable `request_*` and IGNORES
    /// the returned `bool` has re-created exactly the #347 defect — the command was dropped on the
    /// floor and the caller was still told `200`. Every such call site must consume the result.
    ///
    /// **Round-1 review, B1:** the first version of this guard accepted `if !s.command.request_x(..)`
    /// and `if s.command.request_x(..)` *identically*, so deleting one `!` restored #347 with the
    /// whole suite green. It only ever checked that the `bool` was read, never how. The polarity now
    /// lives in one place (`crate::refusal::Refusal`) and this guard requires the canonical shape —
    /// `if let Some(busy) = s.command.request_x(..).refused(MSG) { return busy; }`.
    ///
    /// Precisely what that buys, measured rather than reasoned — the compiler rejects the literal
    /// `!`-drop (`if let None = …` binds nothing, so the `return busy;` is `E0425`), but it does
    /// NOT reject every silent drop: `if let Some(_) = …refused(MSG) { }` compiles cleanly, and so
    /// does a wrapped receiver (R2-1). Both are caught HERE, by [`refusal_shape_faults`], and —
    /// for the four modules that gained tests in round 1 — independently by a behavioural
    /// double-fire test that sees the `200` where a `409` belongs. So the inverted form is not
    /// unrepresentable; every form found SO FAR is provably RED (the enumeration has been wrong
    /// three times — see `refusal.rs`), and the predicate that makes them RED is itself pinned by
    /// [`the_call_site_predicate_accepts_only_the_canonical_shape`] rather than by a request in a
    /// comment.
    ///
    /// Search key: `grep -rn 's\.command\.request_' crates/eqoxide-http/src/*.rs` — but note that
    /// grep is exactly the tool this guard stopped using in round 2, so it undercounts wrapped
    /// sites. Handlers reach the facade through the `HttpState` binding `s`, which is what
    /// distinguishes a real call site from the `command.take_*` forms used inside test bodies.
    #[test]
    fn every_refusable_command_request_is_checked_by_its_http_caller() {
        let refusable = refusable_requests();
        let mut checked: Vec<String> = Vec::new();
        let mut offenders: Vec<String> = Vec::new();
        let mut ignored_by_design: Vec<String> = Vec::new();

        for path in rs_files(&crate_src()) {
            let text = std::fs::read_to_string(&path).unwrap();
            let file = path.file_name().unwrap().to_string_lossy().to_string();
            for site in refusal_sites(&text) {
                let canonical = site.canonical();
                let Site { line, name, faults } = site;
                if !refusable.contains(&name) {
                    ignored_by_design.push(format!("{file}:{line}: {name}"));
                } else if canonical {
                    checked.push(format!("{file}:{line}: {name}"));
                } else {
                    offenders.push(format!(
                        "{file}:{line}: {name} — not the canonical refusal (violates conjunct(s) \
                         {faults:?} of refusal_shape_faults)"
                    ));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "these HTTP handlers call a refusable CommandState::request_* and throw away the \
             refusal, so a dropped command is still answered 200 (#347 step 2):\n  {}",
            offenders.join("\n  ")
        );
        // Anti-vacuity: the guard must actually be looking at something. Both numbers are lower
        // bounds measured from this tree, not exact expectations.
        assert!(refusable.len() >= 30,
            "expected the command facade to expose >= 30 refusable request_* methods, found {} — \
             the scanner's parse of `pub fn request_` signatures has probably drifted",
            refusable.len());
        // R2-1: the old floor of 25 against 39 actual sites meant 14 could vanish — by a reflow, or
        // by deletion — before anti-vacuity noticed. The floor is now the measured count, so losing
        // even one site is RED and has to be an explicit edit to this number.
        assert!(checked.len() >= CANONICAL_SITES,
            "expected >= {CANONICAL_SITES} checked refusable call sites in this crate, found {}. \
             If you deliberately removed a route, lower CANONICAL_SITES in the same commit; if you \
             did not, a call site has stopped matching the canonical shape: {:?}",
            checked.len(), checked);
        // `request_goto` / `request_follow` / `request_stop` / `request_zone_cross` / `request_camp`
        // / `request_respawn` / `request_who` / `request_friends_who` / `request_chat_send` are the
        // deliberately last-writer-wins or reply-channel commands (see `eqoxide_command::slot`'s
        // module docs); they do not return `bool` and so land here rather than in `offenders`.
        assert!(!ignored_by_design.is_empty(),
            "no non-refusable call sites found at all — the scanner is probably matching nothing");
    }

    /// The enumeration `docs/http-api.md` makes — "*every* verb under `/v1/combat`, `/v1/interact`,
    /// `/v1/inventory`, `/v1/merchant`, `/v1/trainer`, `/v1/pet`, `/v1/group`, `/v1/quests` and
    /// `/v1/guild` can answer `409`" — is itself a claim, so it is checked here rather than trusted.
    ///
    /// For each of those modules this reads the `router()` table, collects every handler mounted
    /// with `post(..)` or `.delete(..)`, and requires that handler's body to route its queue outcome
    /// through `Refusal::refused` / `refused_json` — directly or via a module-local helper.
    /// `get(..)` routes are reads and are skipped. If a route is added to one of these modules
    /// without a busy-slot refusal, this fails with its name.
    ///
    /// **Round-1 review, B1:** this used to accept the mere presence of the string
    /// `StatusCode::CONFLICT` anywhere in the handler body, which an inverted `if` satisfies just as
    /// well as a correct one (the string is still there, on the wrong branch). Requiring the
    /// `.refused(..)` combinator instead ties the route to the single tested polarity site.
    ///
    /// Search key: `grep -n '\.route(' crates/eqoxide-http/src/{combat,interact,inventory,merchant,trainer,pet,group,quests,guild}.rs`
    #[test]
    fn every_mutating_route_in_the_documented_modules_can_answer_409() {
        const MODULES: [&str; 9] = [
            "combat", "interact", "inventory", "merchant",
            "trainer", "pet", "group", "quests", "guild",
        ];
        let mut checked: Vec<String> = Vec::new();
        let mut offenders: Vec<String> = Vec::new();

        for m in MODULES {
            let path = crate_src().join(format!("{m}.rs"));
            let text = std::fs::read_to_string(&path).expect("module source");

            // 1. Handler fn names mounted as a mutating verb in `router()`.
            let mut handlers: Vec<(String, String)> = Vec::new(); // (route, fn)
            for line in text.lines() {
                let Some(rest) = line.trim_start().strip_prefix(".route(") else {
                    // `pet.rs` mounts its single route inline on `Router::new()`.
                    if !line.contains("Router::new().route(") { continue; }
                    let rest = line.split("Router::new().route(").nth(1).unwrap();
                    collect_route(rest, &mut handlers);
                    continue;
                };
                collect_route(rest, &mut handlers);
            }

            // 2. Module-local helpers that themselves refuse — `guild.rs::queue` and
            //    `interact.rs::queue_loot` hold the refusal for several handlers each, so a
            //    handler that delegates to one of them is just as capable of a 409.
            let refuses = |b: &str| b.contains(".refused(") || b.contains(".refused_json(");
            let conflict_helpers: Vec<String> = text.lines()
                .filter_map(|l| l.trim_start().strip_prefix("fn ").map(|r| {
                    r.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<String>()
                }))
                .filter(|n| !n.is_empty())
                .filter(|n| fn_body_named(&text, &format!("fn {n}("))
                    .is_some_and(|b| refuses(&b)))
                .collect();

            // 3. Each handler's body must run its queue outcome through the refusal combinator,
            //    directly or via one of those helpers.
            for (route, func) in handlers {
                let body = fn_body(&text, &func)
                    .unwrap_or_else(|| panic!("{m}.rs: no body found for handler `{func}`"));
                let via_helper = conflict_helpers.iter().any(|h| body.contains(&format!("{h}(")));
                if refuses(&body) || via_helper {
                    checked.push(format!("/{m}{route} → {func}"));
                } else {
                    offenders.push(format!("/{m}{route} → {func} (in {m}.rs)"));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "docs/http-api.md claims every mutating verb in these modules can answer 409 when its \
             command slot is busy, but these cannot (#347 step 2):\n  {}",
            offenders.join("\n  ")
        );
        // Anti-vacuity: measured at 41 mutating routes across the nine modules when this was
        // written; asserted as a lower bound so adding routes does not fail the guard spuriously.
        assert!(checked.len() >= 41,
            "expected >= 41 mutating routes across {MODULES:?}, found {}: {checked:#?}",
            checked.len());
    }

    /// Pull `("/path", "handler_fn")` pairs out of the remainder of a `.route(..)` line, for the
    /// `post(..)`/`delete(..)` verbs only.
    fn collect_route(rest: &str, out: &mut Vec<(String, String)>) {
        let Some(route) = rest.split('"').nth(1) else { return };
        for verb in ["post(", "delete("] {
            let mut from = 0;
            while let Some(i) = rest[from..].find(verb) {
                let start = from + i + verb.len();
                let name: String = rest[start..]
                    .chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                if !name.is_empty() {
                    out.push((route.to_string(), name));
                }
                from = start;
            }
        }
    }

    /// The source text of `async fn <name>`'s body, from its signature to the closing brace in
    /// column 0. Handlers in this crate are all free functions at module level.
    fn fn_body(text: &str, name: &str) -> Option<String> {
        fn_body_named(text, &format!("async fn {name}("))
    }

    /// As [`fn_body`], but takes the whole leading needle so plain (non-`async`) helpers can be read
    /// too. Handlers and helpers in this crate are all free functions at module level, so the body
    /// ends at the first closing brace in column 0.
    fn fn_body_named(text: &str, needle: &str) -> Option<String> {
        let start = text.find(needle)?;
        let rest = &text[start..];
        let end = rest.find("\n}\n").map(|i| i + 2).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}
