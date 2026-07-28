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

    /// The universal at the HTTP boundary: a handler that calls a refusable `request_*` and IGNORES
    /// the returned `bool` has re-created exactly the #347 defect — the command was dropped on the
    /// floor and the caller was still told `200`. Every such call site must consume the result.
    ///
    /// **Round-1 review, B1:** the first version of this guard accepted `if !s.command.request_x(..)`
    /// and `if s.command.request_x(..)` *identically*, so deleting one `!` restored #347 with the
    /// whole suite green. It only ever checked that the `bool` was read, never how. The polarity now
    /// lives in one place (`crate::refusal::Refusal`) and this guard requires the canonical shape —
    /// `if let Some(busy) = s.command.request_x(..).refused(MSG) { return busy; }` — so a call site
    /// cannot express the inverted form at all. (`if let None = …` does not compile: nothing binds.)
    ///
    /// Search key: `grep -rn 's\.command\.request_' crates/eqoxide-http/src/*.rs`. Handlers reach the
    /// facade through the `HttpState` binding `s`, so this prefix is what distinguishes a real call
    /// site from the `command.take_*` / `command.request_*` forms used inside test bodies.
    #[test]
    fn every_refusable_command_request_is_checked_by_its_http_caller() {
        let refusable = refusable_requests();
        let mut checked: Vec<String> = Vec::new();
        let mut offenders: Vec<String> = Vec::new();
        let mut ignored_by_design: Vec<String> = Vec::new();

        for path in rs_files(&crate_src()) {
            let text = std::fs::read_to_string(&path).unwrap();
            let file = path.file_name().unwrap().to_string_lossy().to_string();
            for (n, line) in text.lines().enumerate() {
                let Some(pos) = line.find("s.command.request_") else { continue };
                // Prose, not code: `refusal.rs`'s module docs quote both the old and the new call
                // shape, and this guard is exactly the thing they are describing.
                if line.trim_start().starts_with("//") { continue; }
                let name_start = pos + "s.command.".len();
                let name: String = line[name_start..]
                    .chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                if !refusable.contains(&name) {
                    ignored_by_design.push(format!("{file}:{}: {name}", n + 1));
                    continue;
                }
                let code = line.trim_start();
                // The ONE accepted shape (B1):
                //   `if let Some(busy) = s.command.request_x(..).refused(MSG) { return busy; }`
                // The `.refused(..)` / `.refused_json(..)` call may wrap onto the next line, so the
                // statement is accumulated up to its opening brace before being matched. Nothing
                // else is accepted — not `if !…`, not `if …`, not `let ok = …` — because those are
                // the forms in which a one-character polarity mutation is expressible.
                let mut stmt = String::new();
                for l in text.lines().skip(n) {
                    stmt.push_str(l.trim());
                    if l.contains('{') { break; }
                    stmt.push(' ');
                }
                let consumed = code.starts_with("if let Some(busy) = s.command.request_")
                    && (stmt.contains(".refused(") || stmt.contains(".refused_json("))
                    && stmt.contains("return busy;");
                if consumed {
                    checked.push(format!("{file}:{}: {name}", n + 1));
                } else {
                    offenders.push(format!("{file}:{}: {name} — result discarded", n + 1));
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
        // bounds measured from this tree at the time of writing, not exact expectations.
        assert!(refusable.len() >= 30,
            "expected the command facade to expose >= 30 refusable request_* methods, found {} — \
             the scanner's parse of `pub fn request_` signatures has probably drifted",
            refusable.len());
        assert!(checked.len() >= 25,
            "expected >= 25 checked refusable call sites in this crate, found {}: {:?}",
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
