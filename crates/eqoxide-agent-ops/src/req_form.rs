//! #952/#956 (agent-honesty): the ONE place a "you sent more than one mutually exclusive form"
//! refusal is decided and worded.
//!
//! **The defect this exists to remove.** All 20 of this crate's request structs carry
//! `#[serde(deny_unknown_fields)]` — `DENY_UNKNOWN_EXEMPT` is now empty and is asserted against the
//! source, so this sentence cannot rot. (It held two `/v1/observe` query structs until #971.)
//! Where the attribute IS present it is what makes a *typo* loud. It is also what makes a
//! *silently ignored* field quiet: a field the struct declares but the handler never reaches is
//! **accepted**, because it is not unknown — 200, no warning, no disclosure. The agent driving this
//! client has no independent channel to reality, so a 200 is the strongest signal available that its
//! instruction was understood; it then attributes behaviour to a parameter that never had any
//! effect. Measured shape (see the tests below, each driven through the real router): a handler
//! resolves several alternative argument forms with a precedence chain — `b.command.or(b.name)`,
//! `if let Some(id) … else if let Some(name) …` — and the *loser* vanishes.
//! `{"command":2,"name":"sit"}` is not an ambiguous request, it is two DIFFERENT pet commands,
//! and the client answered `200 pet command 2 queued`.
//!
//! **Why refusal, not disclosure.** #901 established the split and #947 built the machinery for it:
//! two forms that describe the SAME thing (a Brewall `{map_x,map_y}` pair and a raw `{x,y,z}` triple
//! for one point) are non-contradictory, so precedence applies and the loser is *reported* in
//! `ignored_fields`. Two forms that NAME DIFFERENT THINGS can never sensibly co-occur — there is no
//! reading under which the caller meant both — so they are refused. Every group routed through this
//! module is of the second kind. `resolve_camera_override` refuses `preset` beside `pitch`/`yaw`/
//! `distance` for exactly this reason. This module is the same rule for the rest of the surface,
//! written once so there is one copy of the wording rather than N copies that drift.
//!
//! **Where the check goes in a handler.** After the session/liveness gates (a dead session cannot
//! answer anything honestly), and BEFORE anything that inspects world state or queues work. The
//! request is malformed whatever the world looks like, so answering a state error first —
//! `409 no dialogue choices available`, or `404 no corpse matching …` — would send an agent off
//! to fix the world when the thing to fix is its own request.
//!
//! **The compile-time half.** [`conflicting_forms`] takes the group as data, so on its own it is a
//! runtime check someone must remember to call. What makes the *group* non-forgettable is at the
//! call site: every caller builds the array from an EXHAUSTIVE destructure of its body struct with
//! no `..` rest pattern (`let CommandBody { command, name } = &b;`). A Rust struct pattern must
//! mention every field, so adding a field to the body is a **compile error** in the handler until
//! its author decides whether the new field belongs in the exclusive group. That is the property
//! that was missing when a field was added to a body struct and the handler never looked at it
//! (#952).
//!
//! **How far that reaches, exactly.** It is 5 of the 20 request structs, not all of them: the five
//! handlers listed by
//! `grep -rnE '^\s*let [A-Za-z]+Body \{[^}]*\} = ' crates/eqoxide-agent-ops/src/*.rs`. Only those are
//! protected against a NEW FIELD being added and never read. The source-scan guard below is the
//! other axis and covers all 20, but it guards STRUCT addition — a struct must be classified — not
//! field addition. So the honest summary is: adding a request struct forces the decision; adding a
//! field to one of the other 15 structs does not, and #952's original shape could recur there.

/// Refuse a request that supplied more than one of a set of MUTUALLY EXCLUSIVE form fields.
///
/// `what` names the argument the forms are alternatives for ("pet command", "corpse selection", …).
/// `forms` is every field of the group, in the order the route's own documentation lists them,
/// paired with whether THIS request supplied it — built from an exhaustive destructure of the body
/// struct (see the module doc).
///
/// Returns `Some(message)` when two or more are present; the caller answers `400` with it. `None`
/// for zero or one, which is every well-formed request.
///
/// The message renders the supplied field names verbatim and in the given order. That is load
/// bearing twice over: it is the only way the caller learns WHICH of its fields collided, and it is
/// what the guard test's oracle reads back — a refusal that names no field would pass a
/// `contains("conflicting")` assertion while telling the agent nothing.
///
/// **The prose deliberately avoids every word that is also a field name on this surface, and `what`
/// must too.** The oracle asks "does this refusal name field F?", so an incidental `name`, `command`
/// or `trainer` in the surrounding English is a free hit that makes the assertion weaker than it
/// reads. This was not hypothetical: the first draft said "They *name* different things" and used
/// `what` values "pet command" and "trainer selection", and a mutant that blanked the field list
/// was still credited with naming `name`/`command`/`trainer` on four of the seven rows — measured,
/// then fixed here. `refusal_prose_never_contains_a_field_name_by_accident` keeps it that way.
pub(crate) fn conflicting_forms(what: &str, forms: &[(&'static str, bool)]) -> Option<String> {
    let present: Vec<&'static str> = forms.iter().filter(|&&(_, p)| p).map(|&(f, _)| f).collect();
    if present.len() < 2 {
        return None;
    }
    Some(format!(
        "conflicting {what}: this request supplied {} — mutually exclusive forms of one argument, \
         of which exactly one may be sent. They denote different things, so there is no reading \
         under which both were meant and nothing was chosen for you: this request was REFUSED, \
         nothing was queued, and no state changed. Re-send with a single form.",
        present.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::empty_state;
    use crate::wire::StatusCode;

    #[test]
    fn one_form_or_none_is_not_a_conflict() {
        assert!(conflicting_forms("x", &[("a", false), ("b", false)]).is_none());
        assert!(conflicting_forms("x", &[("a", true), ("b", false)]).is_none());
        assert!(conflicting_forms("x", &[("a", false), ("b", true)]).is_none());
    }

    #[test]
    fn a_conflict_names_every_supplied_field_in_declaration_order() {
        let msg = conflicting_forms("pet command", &[("command", true), ("name", true)]).unwrap();
        assert!(msg.contains("command, name"), "fields must be named, in order: {msg}");
        assert!(msg.contains("REFUSED"),
            "the message must state that nothing happened — a refusal an agent reads as a warning \
             is the same lie in a politer voice: {msg}");
        let reversed = conflicting_forms("pet command", &[("name", true), ("command", true)]).unwrap();
        assert!(reversed.contains("name, command"),
            "order follows the caller's declared group, not a sort: {reversed}");
    }

    /// A field that is present but NOT part of the group can never trigger a refusal — otherwise
    /// composable fields (`target_id` beside `gem`, `allow_pending` beside `preset`) would start
    /// 400ing valid requests, which is a new lie in the opposite direction.
    #[test]
    fn only_group_members_count_toward_a_conflict() {
        assert!(conflicting_forms("x", &[("a", true)]).is_none());
        let msg = conflicting_forms("x", &[("a", true), ("b", false), ("c", true)]).unwrap();
        assert!(msg.contains("a, c") && !msg.contains(", b"), "{msg}");
    }

    // ── The guard ────────────────────────────────────────────────────────────────────────────────
    //
    // Two halves, each with its own reach control, because they fail in different ways:
    //
    //   1. `every_deserialize_request_struct_is_classified` — a SOURCE scan of this crate's whole
    //      `src/` directory. It is the answer to the thing that actually keeps going wrong here: not
    //      the enumeration, but the ENUMERATION METHOD. Three separate hand-reviews of this exact
    //      surface each shipped an incomplete list (each omitted a route, and one table
    //      listed a route path that does not exist). A hand
    //      list cannot be trusted, so the corpus is read off the filesystem and every struct in it
    //      must be classified.
    //   2. `every_declared_mutually_exclusive_form_group_is_refused_not_silently_dropped` — drives
    //      each classified-exclusive group through the REAL `/v1/...` router with every form of the
    //      group present at once, and reads back which fields the refusal names.

    /// One mutually-exclusive form group, probed against the real router.
    struct Case {
        /// `file.rs` + struct name — the key the source scan classifies against. Two
        /// modules each declare a `NameBody`, so the file is part of the identity.
        file: &'static str,
        strukt: &'static str,
        /// The path an agent actually calls, under the same `/v1` nesting production serves.
        route: &'static str,
        /// `Some(json)` → POST with this body. `None` → GET, with the query already in `route`.
        post_body: Option<&'static str>,
        /// Every field of the group, in declaration order. The request supplies ALL of them, and the
        /// refusal must name ALL of them.
        fields: &'static [&'static str],
        /// The `what` this route passes to [`conflicting_forms`], or `None` for the row that
        /// predates this module and words its own refusal (`/frame`). Held here so
        /// `refusal_prose_never_contains_a_field_name_by_accident` can check the wording a caller
        /// actually receives, `what` included.
        what: Option<&'static str>,
    }

    /// Every request struct in this crate with a mutually-exclusive form group.
    ///
    /// The first row is pre-existing, correct behaviour (#422's `resolve_camera_override`)
    /// deliberately brought under the same oracle rather than left unguarded, since a regression in
    /// it would otherwise be invisible here.
    const EXCLUSIVE: &[Case] = &[
        Case { file: "trainer.rs", strukt: "OpenBody", route: "/v1/trainer/open",
               post_body: Some(r#"{"name":"Alpha","trainer":"Beta"}"#), fields: &["name", "trainer"], what: Some("guildmaster selection") },
        Case { file: "social.rs", strukt: "FriendsBody", route: "/v1/social/friends",
               post_body: Some(r#"{"add":"Alpha","remove":"Beta"}"#), fields: &["add", "remove"], what: Some("friends-list edit") },
        Case { file: "interact.rs", strukt: "LootBody", route: "/v1/interact/loot",
               post_body: Some(r#"{"id":7,"name":"a_rat00"}"#), fields: &["id", "name"], what: Some("corpse selection") },
        Case { file: "interact.rs", strukt: "DoorClickBody", route: "/v1/interact/click_door",
               post_body: Some(r#"{"door_id":7,"name":"HHCELL"}"#), fields: &["door_id", "name"], what: Some("door selection") },
        Case { file: "interact.rs", strukt: "DialogueBody", route: "/v1/interact/dialogue",
               post_body: Some(r#"{"index":0,"text":"bind"}"#), fields: &["index", "text"], what: Some("dialogue choice") },
    ];

    /// Every OTHER `Deserialize` request struct in this crate: no mutually-exclusive group, because
    /// every field it declares is read on every path that can return 2xx. One reason each, so a
    /// future reader can check the claim rather than inherit it.
    ///
    /// Adding a struct here is a decision, and it is the decision this whole guard exists to force
    /// someone to make out loud.
    ///
    /// **The reasons are READ, not EXECUTED — do not mistake this table for verification.** The
    /// guard asserts that every struct is *classified*; nothing asserts that a classification is
    /// *right*. Each justification here was written by reading the handler and checking that every
    /// field reaches a use on every 2xx path, but no probe drives these remaining routes with all their
    /// fields set and confirms each one had an effect. A struct wrongly filed here is therefore
    /// invisible to this module — it is exactly the #952 defect, sitting behind a sentence claiming
    /// it is not. The `EXCLUSIVE` rows are the ones that ARE executed, through `probe`.
    const COMPOSABLE: &[(&str, &str, &str)] = &[
        ("combat.rs", "MemorizeBody", "`spell_id` and `gem` are both required and both sent"),
        ("combat.rs", "ScribeBody",
         "`spell_id`, `slot` and `from` are three parts of one scribe, all three passed to \
          request_mem_spell"),
        ("events.rs", "EventsQuery",
         "`since`, `wait` and `directed` are independent filters, all three read by `fetch`"),
        ("group.rs", "NameBody", "single field"),
        ("guild.rs", "NameBody", "single field"),
        ("interact.rs", "ReadBody", "single field"),
        ("interact.rs", "HailBody", "single field"),
        ("interact.rs", "SayBody", "single field"),
        ("interact.rs", "GiveBody", "`npc` and `from` are both required and both used"),
        ("merchant.rs", "TradeOpenBody", "single field"),
        ("merchant.rs", "BuyBody", "`merchant` and `slot` are both required and both used"),
        ("merchant.rs", "SellBody",
         "`merchant`, `slot` and `quantity` are three parts of one sell, all three used"),
        ("observe.rs", "MessagesQuery", "single field"),
        ("quests.rs", "TaskIdBody", "single field"),
        ("trainer.rs", "TrainBody", "single field"),
    ];

    /// Structs the scan below finds that are NOT request inputs at all, so neither list applies.
    /// Empty today; kept as the third door so a future non-request `Deserialize` has somewhere to go
    /// other than a wrong classification.
    const NOT_A_REQUEST_INPUT: &[(&str, &str)] = &[];

    /// **Reach control, half 1.** The number of `Deserialize` structs the scan EXTRACTED — not the
    /// number of files it opened, and not a counter maintained beside the extraction. It is
    /// `structs.len()` of the very vector the classification below iterates, so a scanner that
    /// silently reads a fraction of `src/` (#778: one silently covered ~12% of its corpus and passed
    /// green) cannot leave this figure intact. Re-derive after any change:
    /// `grep -rn '^#\[derive(.*Deserialize' crates/eqoxide-agent-ops/src/ | wc -l`.
    ///
    /// The `^` anchor is not cosmetic and the obvious unanchored form is wrong: it reports 36,
    /// because its one extra hit is in THIS file — the scanner's own `starts_with("#[derive(")`
    /// predicate below, quoted in source. Every real derive on this surface sits at column 0
    /// (`grep -rn '^[[:space:]]\+#\[derive(.*Deserialize' crates/eqoxide-agent-ops/src/` is empty), so
    /// anchoring drops the self-match and nothing else.
    ///
    /// One latent divergence, stated rather than left to be discovered: the scanner below matches on
    /// `line.trim_start()`, so it WOULD count an indented `#[derive(Deserialize)]` — e.g. a struct
    /// declared inside a function — while the `^`-anchored command above would not. The two agree on
    /// this corpus today only because no such derive exists (the indented-form grep is empty, as
    /// above). If one ever appears, the command will read one lower than the scanner and this
    /// constant, and the scanner is the authority: the whole point is to see structs a hand-written
    /// or narrowly-anchored view would miss.
    const DECLARED_REQUEST_STRUCTS: usize = 20;

    /// The request structs that do NOT carry `#[serde(deny_unknown_fields)]`, in scan order.
    ///
    /// **Empty since #971**, which is the whole claim.
    /// Measured over this same corpus: all 20 carry it, 0 do not.
    /// The second figure is written out because "no exceptions" is exactly the sentence that used to
    /// be false here, and a `0` the pin below moves with is harder to leave behind than an absence.
    ///
    /// The list is asserted exactly and in order, so both of the directions the prose can rot in are
    /// covered: dropping the attribute from any struct, or introducing a new struct without it,
    /// turns the corpus guard RED and forces the prose to move with it.
    ///
    /// **What that does NOT cover, stated because a guard's limit is part of its claim.** The scan
    /// reads what is WRITTEN, not what serde APPLIES. It requires the words to sit on an attribute
    /// line and refuses a `cfg_attr` outright (both narrowings came from review, where
    /// `#[cfg_attr(any(), serde(deny_unknown_fields))]` was measured to read as protected while
    /// being inert), but no source-text scan can be sure the attribute it reads is the one the
    /// compiler applied.
    const DENY_UNKNOWN_EXEMPT: &[(&str, &str)] = &[];

    /// **Reach control, half 2.** The total number of field names the probes actually LOCATED in the
    /// routes' own refusal bodies, summed over every case. Not a count of cases iterated: the only
    /// thing it can be derived from is [`named_fields`]'s return value, which is the check. A probe
    /// that stops refusing, a loop that exits early, and an oracle that finds nothing all drive this
    /// number down together. Hand-written on purpose — computing it from `EXCLUSIVE` would let a row
    /// deleted from the table take its own coverage requirement with it.
    const REFUSAL_NAMED_FIELDS: usize = 10;

    /// **Reach control, half 2b — the SUBJECT of the refusal, not just its field list.**
    ///
    /// The number of rows whose refusal body was found to open with `conflicting <what>`, summed the
    /// same way: incremented only when the phrase is actually located in a body the router returned.
    ///
    /// MEASURED, and the reason this exists: the review's RV-M6 changed `/v1/interact/loot`'s handler
    /// to pass `"spell selection"` — so a caller sending two corpse forms was told its *spell*
    /// selection conflicted — and the entire crate stayed green (exit 0, 340 passed, 0 failed),
    /// because the oracle only ever asked which FIELDS were named. Naming the right fields under the
    /// wrong subject is still a false statement about the request, which is the defect class this
    /// whole module is about.
    ///
        /// Hand-written for the same reason as [`REFUSAL_NAMED_FIELDS`] — deriving it from `EXCLUSIVE`
    /// would let a deleted row delete its own coverage requirement.
    const REFUSAL_SUBJECTS: usize = 5;

    /// Those of `group` that appear in `body` as WHOLE TOKENS — bounded by something that is not an
    /// identifier character on both sides.
    ///
    /// Whole tokens, not `contains`, because a bare substring test is vacuous on exactly this
    /// surface — see `substring_matching_would_be_vacuous_on_these_very_messages`, which measures it
    /// rather than asserting it in prose.
    fn named_fields(body: &str, group: &'static [&'static str]) -> Vec<&'static str> {
        fn ident_char(c: char) -> bool { c.is_ascii_alphanumeric() || c == '_' }
        group.iter().copied().filter(|field| {
            body.match_indices(field).any(|(i, m)| {
                let before = body[..i].chars().next_back();
                let after = body[i + m.len()..].chars().next();
                !before.is_some_and(ident_char) && !after.is_some_and(ident_char)
            })
        }).collect()
    }

    /// Why [`named_fields`] matches whole tokens and not substrings, measured on the real strings.
    ///
    /// A single-letter field name makes a `contains` oracle worthless: this module's refusal wording
    /// ("mutually e**x**clusive") contains the letter `x` in an ordinary English word, so
    /// `body.contains("x")` is satisfied by a message that names no field at all. A mutant that blanked the field list
    /// would leave such an assertion green, which is precisely the mutant this guard has to kill.
    #[test]
    fn substring_matching_would_be_vacuous_on_these_very_messages() {
        let ours = conflicting_forms("target", &[("a", true), ("b", true)]).unwrap();
        assert!(ours.contains("x"), "expected an incidental 'x' in: {ours}");
        assert!(named_fields(&ours, &["x"]).is_empty(),
            "whole-token matching must NOT find the field `x` in a message that never names it");

        // And the converse: a whole token IS found, including one containing an underscore.
        assert_eq!(named_fields("supplied door_id, name — mutually", &["door_id", "name"]),
                   vec!["door_id", "name"]);
        // …but not as part of a longer identifier.
        assert!(named_fields("supplied door_id", &["id"]).is_empty(),
            "`id` must not match inside `door_id` — LootBody's `id` and DoorClickBody's `door_id` \
             are different fields and a probe must not credit one for the other");
    }

    /// `docs/http-api.md` quotes one refusal body verbatim as the worked example of this rule. A
    /// quoted response body is a claim about what the code does, and this repo's dominant defect is
    /// exactly that claim going stale — the doc's example was already wrong once, still saying
    /// "conflicting pet command" and "They *name* different things" after both were changed here.
    ///
    /// So the doc is the fixture: this reads the fenced example out of the file, unwraps the line
    /// breaks the Markdown adds, and compares it to what `conflicting_forms` actually renders for
    /// that request. Editing either side alone turns this RED.
    #[test]
    fn the_documented_example_refusal_is_byte_for_byte_what_the_router_renders() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/http-api.md");
        let doc = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {path}: {e}"));

        // The example block is `POST /v1/pet/command …` followed by `400 "<body>"`.
        let anchor = "POST /v1/pet/command {\"command\": 2, \"name\": \"sit\"}";
        let after = doc.split_once(anchor)
            .unwrap_or_else(|| panic!("{path} no longer contains the worked example anchor {anchor:?}")).1;
        let quoted = after.split_once("400 \"")
            .expect("the example must show its status and a quoted body").1
            .split_once('"')
            .expect("the quoted body must be closed").0;

        fn unwrap_ws(s: &str) -> String { s.split_whitespace().collect::<Vec<_>>().join(" ") }

        let rendered = conflicting_forms(
            "pet action", &[("command", true), ("name", true)],
        ).expect("two supplied forms must refuse");

        assert_eq!(unwrap_ws(quoted), unwrap_ws(&rendered),
            "docs/http-api.md quotes a refusal body that this code no longer produces");
    }


    /// The refusal template and every `what` it is given must contain NO word that is also a field
    /// name in the group being refused — otherwise the probe oracle is credited for a field the
    /// message never actually named.
    ///
    /// MEASURED, and the reason this test exists: with the template's original wording ("They *name*
    /// different things") and the original `what` values ("pet command", "trainer selection"), the
    /// M6 mutant — which blanks the rendered field list while still refusing — was still credited
    /// with naming `name` on `/loot` and `/click_door`, `command` on `/pet/command`, and `trainer` on
    /// `/trainer/open`. Four rows would have passed the field-naming assertion on prose alone.
    ///
    /// The probe substitutes sentinel field names, so anything [`named_fields`] finds came from the
    /// fixed prose, not from the rendered list.
    #[test]
    fn refusal_prose_never_contains_a_field_name_by_accident() {
        let mut offenders: Vec<String> = Vec::new();
        for case in EXCLUSIVE {
            let Some(what) = case.what else { continue }; // `/frame` words its own
            let prose = conflicting_forms(
                what, &[("__sentinel_one__", true), ("__sentinel_two__", true)]).unwrap();
            let hits = named_fields(&prose, case.fields);
            if !hits.is_empty() {
                offenders.push(format!(
                    "{}: the refusal wording for {what:?} contains {hits:?} as whole word(s), so the \
                     probe would credit this route with naming {hits:?} even if it named nothing. \
                     Reword the template or `what`. Prose: {prose}", case.route));
            }
        }
        assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    }

    /// Drive one case through the real `/v1` router and return the fields its refusal NAMED.
    ///
    /// The returned vector is the check's product and the reach control's only input — there is no
    /// separate counter that could stay right while this goes wrong.
    async fn probe(case: &Case) -> (StatusCode, String, Vec<&'static str>) {
        let state = empty_state();
        // Seed the world so every request in the table is one that WOULD otherwise be honoured:
        // a corpse for `/loot`, a dialogue choice for `/dialogue`,
        // a door for `/click_door`. A refusal here can then only be the form conflict, never an
        // incidental "nothing to act on" — which would let the whole table pass on an empty world
        // with the conflict check deleted.
        state.world.entity_ids_mut().insert_for_test("a_rat00".into(), 42);
        state.world.entity_positions_mut().insert_for_test("a_rat00".into(), (10.0, 20.0, 3.0));
        // `is_corpse_key` keys on the literal word "corpse", and spawn id 7 is what the `/loot`
        // probe sends as `id` — so with the conflict check removed, `/loot` resolves a real corpse
        // and answers 200. That is the state the assertion has to be able to see.
        state.world.entity_ids_mut().insert_for_test("a_rat00's corpse".into(), 7);
        state.world.entity_positions_mut().insert_for_test("a_rat00's corpse".into(), (10.0, 20.0, 3.0));
        // Both names the `/trainer/open` probe sends resolve, so `b.trainer.or(b.name)` has a real
        // winner AND a real loser — without the conflict check the route answers 200 naming Beta
        // and Alpha vanishes. Seeding only one would make it 404 for an unrelated reason and the
        // row would go green for the wrong reason.
        state.world.entity_ids_mut().insert_for_test("Alpha".into(), 101);
        state.world.entity_ids_mut().insert_for_test("Beta".into(), 102);
        state.interact.dialogue.lock().unwrap().push(
            eqoxide_core::game_state::DialogueChoice { text: "bind".into(), ..Default::default() });
        state.interact.doors_shared.lock().unwrap().push(eqoxide_ipc::DoorView {
            door_id: 7, name: "HHCELL".into(),
            x: 0.0, y: 0.0, z: 0.0, heading: 0.0, opentype: 58, is_open: false,
        });
        let name = format!("{} {}", if case.post_body.is_some() { "POST" } else { "GET" }, case.route);
        assert!(crate::v1_ops().contains(&name), "{name} is not a registered op");
        let req = match case.post_body {
            Some(json) => crate::wire::Req::with_body(serde_json::from_str(json).unwrap()),
            None => crate::wire::Req::default(),
        };
        let reply = crate::testkit::call(&state, &name, req).await;
        let body = match &reply.body {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let named = named_fields(&body, case.fields);
        (reply.status, body, named)
    }

    /// **#952/#956, the universal.** For every mutually-exclusive form group this crate declares, a
    /// request carrying ALL of its forms is REFUSED with a 4xx that NAMES every one of them — never
    /// answered `2xx` with the losers discarded.
    ///
    /// The 4xx assertion alone would be satisfied by a route that refuses for an unrelated reason
    /// ("no corpse matching …"), so the load-bearing assertion is the second one: the refusal has to
    /// name the caller's actual fields.
    ///
    /// MUTATION-CHECK — re-measured on THIS tree after the subject reach control was added, by
    /// running `cargo test -p eqoxide-agent-ops --lib` with each mutant in place and reverting by hand.
    /// WRAP form throughout, never deletion-only (#799):
    /// * `if present.len() < 2 { return None; }` → `if false { … } else { return None; }`, which
    ///   disables every refusal this module owns: this test goes RED, reporting the seven
    ///   guarded rows plus BOTH reach controls, at 4 of 19 fields and 0 of 7 subjects. The residual
    ///   4 fields are `/frame`'s: that refusal is worded in `observe.rs` and is correctly outside
    ///   this mutant's blast radius.
    /// * `present.join(", ")` → `if false { … } else { String::new() }`, i.e. still refuse, still
    ///   with the right subject, but name no field: this test goes RED with field reach 4 of
    ///   19 — and the subject reach control stays at 7. The two controls moving independently is
    ///   the evidence they are separate axes rather than one check counted twice.
    /// * `pet.rs`'s single `conflicting_forms` call wrapped to `None`: RED here only, with
    ///   that row, the field reach control at 17 of 19, and the subject reach control at 6 of 7. One route silently regressing moves both numbers, which is the property a
    ///   counter maintained beside the loop could not give.
    /// * The review's RV-M6, replayed: `interact.rs`'s `"corpse selection"` →
    ///   `if false { "corpse selection" } else { "spell selection" }`, so `/loot` names the right
    ///   fields under `/cast`'s subject. It **SURVIVED** the whole crate before this control existed
    ///   (exit 0, 340 passed) and is now **KILLED**: 2 findings, subject reach 6 of 7.
    /// * INVALID control, so that "every mutant above compiled" carries information:
    ///   `present.join(", ")` → `if false { present.join(", ") } else { present }` fails with
    ///   `error[E0308]: `if` and `else` have incompatible types`. That is UNTESTED, not RED — a
    ///   mutant that does not build says nothing about the guard, and the four results above are
    ///   only meaningful because this one is visibly different from them.
    #[tokio::test]
    async fn every_declared_mutually_exclusive_form_group_is_refused_not_silently_dropped() {
        // Every row is probed and every verdict reported, rather than panicking on the first — a
        // guard over a whole surface that stops at row 1 tells you about row 1 and hides the rest,
        // which is how a partial fix reads as a complete one.
        let mut located: Vec<&'static str> = Vec::new();
        let mut subjects: Vec<&'static str> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        for case in EXCLUSIVE {
            let (status, body, named) = probe(case).await;
            // Counted only out of an actual REFUSAL, which is what the constant is named for: a
            // `200 pet command 2 queued` happens to contain the token `command`, and crediting that
            // toward "fields named in refusal bodies" would be the same category error this whole
            // guard is about.
            if status.is_client_error() {
                located.extend(named.iter().copied());
                if let Some(what) = case.what {
                    if body.starts_with(&format!("conflicting {what}")) {
                        subjects.push(what);
                    } else {
                        failures.push(format!(
                            "{} ({}::{}): refused with {status}, but the refusal does not open with \
                             \"conflicting {what}\" — the subject the route declares in EXCLUSIVE. \
                             Naming the caller's fields under someone else's subject still tells the \
                             agent something untrue about its own request. Body: {body}",
                            case.route, case.file, case.strukt));
                    }
                }
            }
            if !status.is_client_error() {
                failures.push(format!(
                    "{} ({}::{}): sending {:?} together answered {status} — a 2xx (or a 5xx) for a \
                     request carrying two mutually exclusive forms means one of them was discarded \
                     with the caller told nothing. Body: {body}",
                    case.route, case.file, case.strukt, case.fields));
            } else if named != case.fields {
                failures.push(format!(
                    "{} ({}::{}): refused with {status}, but the refusal must NAME every \
                     conflicting field the caller sent. Whole-token search found {named:?}, \
                     expected {:?}. Body: {body}",
                    case.route, case.file, case.strukt, case.fields));
            }
        }
        // The reach control is reported through the SAME failure list, not a later `assert!` — an
        // assertion placed after a passing-required one is an assertion a mutant never reaches, and
        // "the reach control stayed green" would then mean nothing more than "the run stopped first".
        if located.len() != REFUSAL_NAMED_FIELDS {
            failures.push(format!(
                "REACH CONTROL: the probes located {} field names in refusal bodies, expected {}. \
                 This number is summed from `named_fields`'s own output, so it moves when the loop \
                 covers fewer cases, when a route stops refusing, or when a refusal stops naming \
                 fields — it cannot be kept green by a counter running beside the check. \
                 Located: {located:?}",
                located.len(), REFUSAL_NAMED_FIELDS));
        }
        if subjects.len() != REFUSAL_SUBJECTS {
            failures.push(format!(
                "REACH CONTROL: {} refusals opened with their declared `conflicting <what>` subject, \
                 expected {}. This is pushed only where the phrase was found in a body the router \
                 returned, so it drops when a route stops refusing, when the loop covers fewer rows, \
                 or when a refusal's subject drifts from the one declared here. Located: {subjects:?}",
                subjects.len(), REFUSAL_SUBJECTS));
        }
        assert!(failures.is_empty(), "{} finding(s) across {} mutually-exclusive form groups:\n  {}",
            failures.len(), EXCLUSIVE.len(), failures.join("\n  "));
        assert_eq!(
            located.len(), REFUSAL_NAMED_FIELDS,
            "REACH CONTROL: the probes located {} field names in refusal bodies, expected {}. This \
             number is summed from `named_fields`'s own output, so it moves when the loop covers \
             fewer cases, when a route stops refusing, or when a refusal stops naming fields — it \
             cannot be kept green by a counter that runs beside the check. Located: {:?}",
            located.len(), REFUSAL_NAMED_FIELDS, located
        );
        assert_eq!(
            subjects.len(), REFUSAL_SUBJECTS,
            "REACH CONTROL: {} refusals opened with their declared `conflicting <what>` subject, \
             expected {}. Located: {:?}",
            subjects.len(), REFUSAL_SUBJECTS, subjects
        );
    }

    /// **The blank-loser refusals in `docs/http-api.md`'s migration note, EXECUTED.**
    ///
    /// The note tells callers that `/trainer/open` and `/interact/dialogue` refuse a body whose
    /// losing form is present but EMPTY, while `/social/friends` does not. `social.rs` owns the
    /// exemption side; this owns the other two, because the presence rule differing by route is
    /// exactly the kind of thing a later change "simplifies" to one predicate — and the `EXCLUSIVE`
    /// probe would not notice, since it only ever sends two NON-blank forms.
    #[tokio::test]
    async fn a_blank_losing_form_still_refuses_on_the_routes_the_migration_note_names() {
        for (case, subject) in [
            (Case { file: "trainer.rs", strukt: "OpenBody", route: "/v1/trainer/open",
                    post_body: Some(r#"{"name":"","trainer":"Beta"}"#),
                    fields: &["name", "trainer"], what: Some("guildmaster selection") },
             "guildmaster selection"),
            (Case { file: "interact.rs", strukt: "DialogueBody", route: "/v1/interact/dialogue",
                    post_body: Some(r#"{"index":0,"text":""}"#),
                    fields: &["index", "text"], what: Some("dialogue choice") },
             "dialogue choice"),
        ] {
            let (status, body, named) = probe(&case).await;
            assert_eq!(status, StatusCode::BAD_REQUEST,
                "{}: a blank losing form is still a SUPPLIED form on this route — the migration note \
                 says so. If this is now 2xx the note is wrong and callers were told the opposite. \
                 Body: {body}", case.route);
            assert_eq!(named, case.fields, "{}: the refusal must still name both: {body}", case.route);
            assert!(body.starts_with(&format!("conflicting {subject}")),
                "{}: the refusal must OPEN with its declared subject, which is what \
                 `docs/http-api.md` tells callers to key on: {body}", case.route);
        }
    }

    /// Every `.rs` file at or below `dir`, as paths relative to it — `"observe.rs"`,
    /// `"observe/frame.rs"`. Top-level files keep their bare name, so the `file` column of
    /// [`EXCLUSIVE`] and [`COMPOSABLE`] is unaffected by the walk becoming recursive.
    fn rs_files(dir: &std::path::Path, prefix: &str, out: &mut Vec<String>) {
        let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
            .map(|e| e.unwrap())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            if entry.file_type().unwrap().is_dir() {
                rs_files(&entry.path(), &rel, out);
            } else if name.ends_with(".rs") {
                out.push(rel);
            }
        }
    }

    /// [`rs_files`] must descend. This crate has no subdirectory under `src/` today, so running the
    /// walk over the real corpus cannot distinguish a recursive implementation from a flat one —
    /// which is exactly how the flat version survived review as far as it did. The tree here is
    /// synthetic so the assertion has content regardless of the crate's current shape.
    #[test]
    fn rs_files_descends_into_subdirectories() {
        // Keyed by more than the pid: pids are recycled, and a leftover tree from a crashed
        // run under a recycled pid would silently JOIN this assertion (review R2-N4). The nanos
        // make collision require the same pid at the same instant.
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before the epoch").as_nanos();
        let root = std::env::temp_dir()
            .join(format!("eqoxide-req-form-walk-{}-{stamp}", std::process::id()));
        let nested = root.join("subprobe").join("deeper");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("top.rs"), "").unwrap();
        std::fs::write(root.join("notrust.txt"), "").unwrap();
        std::fs::write(root.join("subprobe").join("mod.rs"), "").unwrap();
        std::fs::write(nested.join("inner.rs"), "").unwrap();

        let mut found = Vec::new();
        rs_files(&root, "", &mut found);
        found.sort();
        std::fs::remove_dir_all(&root).unwrap();

        assert_eq!(found, vec!["subprobe/deeper/inner.rs", "subprobe/mod.rs", "top.rs"],
            "the walk must reach every depth and must ignore non-`.rs` files; a flat read would \
             return only [\"top.rs\"] and the corpus guard would silently stop at the top level");
    }

    /// **#952/#956 reach control, half 1 — the corpus, not the list.**
    ///
    /// Every `Deserialize` struct declared anywhere under this crate's `src/` must appear in exactly
    /// one of [`EXCLUSIVE`], [`COMPOSABLE`] or [`NOT_A_REQUEST_INPUT`]. A new request body is then RED
    /// until someone states whether its fields are alternatives — which is precisely the decision
    /// that, made by default, produced #952 and #956.
    ///
    /// The directory is read at test time rather than through a hand-written `include_str!` list,
    /// because a hand-written list of files is the same failure mode one level up: a new module would
    /// simply not be scanned, and the scan would report a clean corpus it never opened.
    ///
    /// The walk is RECURSIVE ([`rs_files`]) and that is load bearing. It was flat in the first
    /// version of this guard, and the review's RV-M8 killed it: a real compiled
    /// `src/subprobe/mod.rs` declaring an unclassified two-form body left all of this module's
    /// tests green with the count still reading 35 — the guard could not see the module, and its
    /// own reach control could not see the blindness. A flat read also disagrees with the
    /// re-derivation command documented on [`DECLARED_REQUEST_STRUCTS`], which is `grep -rn` and
    /// therefore recursive: the doc would have told you to check against a number the scanner
    /// structurally could not produce. `rs_files_descends_into_subdirectories` pins the recursion
    /// on a synthetic tree, because this crate happens to have no subdirectory today and a control
    /// that only passes for that reason proves nothing.
    ///
    /// MUTATION-CHECK on the `deny_unknown_fields` half, measured after review round 2 found this
    /// guard was a naive `contains` over the window and could be walked past:
    /// * `quests.rs`'s real attribute → `#[cfg_attr(any(), serde(deny_unknown_fields))]` (text
    ///   present, attribute inert): **KILLED** — refused by name at `quests.rs:54` with the
    ///   cfg-can't-be-evaluated message. It **SURVIVED** before the narrowing.
    /// * The same attribute replaced by a doc comment *mentioning* the words: **KILLED** —
    ///   `Scanned: [… ("quests.rs", "TaskIdBody")]`, i.e. correctly reported as unprotected.
    /// * Control for that second one, so the narrowing is shown to be what kills it rather than
    ///   something else: with the doc-comment mutant still in place, WRAP the attribute-line test
    ///   off (`if false { lt.starts_with("#[") } else { true }`) and this test goes back to
    ///   **GREEN** — a struct silently stripped of its protection, with every prose claim about
    ///   "2 of 35" still passing. That is the evasion, reproduced and then closed.
    ///
    /// Instance TEN, found in review round 3 and measured here the same way: the narrowing above was
    /// `starts_with("#[")`, and `#[doc = "protected by serde(deny_unknown_fields)"]` IS an attribute
    /// line, so a SENTENCE could still vote the struct protected.
    /// * That attribute in place of `quests.rs`'s real one: **KILLED** — the scan reported
    ///   `TaskIdBody` as unprotected. It survived under the old `#[` test.
    /// * Control, WRAP the narrowing back off — `if false` on the `#[serde(` test, `else` the old
    ///   `#[` one — with the mutant still in place: **GREEN**. The `#[serde(` opener is what kills
    ///   it.
    /// * Inertness re-derived rather than assumed, by driving `POST /v1/quests/accept` with
    ///   `{"task_id":999,"bogus":1}` through the op table: with the mutant the answer is
    ///   `400 no pending task offer with task_id=999` (parsed, `bogus` discarded); with the real
    ///   attribute it is `400 provide {"task_id":N}` (refused at deserialization). Note what that
    ///   also shows: NO behavioural test in this crate covers unknown-field rejection on that route,
    ///   so this scan was the only thing standing between that mutant and a green suite.
    #[test]
    fn every_deserialize_request_struct_is_classified() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut files: Vec<String> = Vec::new();
        rs_files(std::path::Path::new(dir), "", &mut files);
        files.sort();
        assert!(
            files.len() >= 14,
            "REACH CONTROL: only {} .rs files found under {dir} ({:?}). The corpus is read from the \
             filesystem precisely so it cannot go stale; a count this low means the read, not the \
             crate, shrank.",
            files.len(), files
        );

        // (file, struct) pairs, in file order. `strukt` alone is not an identity — `group.rs` and
        // `guild.rs` each declare a `NameBody`, `move_api.rs` and `inventory.rs` each a `MoveBody`.
        let mut structs: Vec<(String, String)> = Vec::new();
        let mut unprotected: Vec<(String, String)> = Vec::new();
        for file in &files {
            let src = std::fs::read_to_string(format!("{dir}/{file}")).unwrap();
            let lines: Vec<&str> = src.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let t = line.trim_start();
                // The derive attribute, not a mention of it: skip comments so this module's own
                // prose above cannot enter the corpus it is classifying.
                if t.starts_with("//") || !t.starts_with("#[derive(") || !line.contains("Deserialize") {
                    continue;
                }
                // Walk from the derive to the `struct` line, noting whether the attribute block
                // between them carries `deny_unknown_fields` — see `DENY_UNKNOWN_EXEMPT`.
                //
                // This is a source-TEXT test, and this repo has been fooled by source-text tests
                // eight times (#799). Instance nine was found in review: a plain
                // `l.contains("deny_unknown_fields")` credits the words wherever they appear, so
                // `#[cfg_attr(any(), serde(deny_unknown_fields))]` — text present, attribute inert —
                // read as protected while the struct genuinely lost its protection, and a doc
                // comment merely MENTIONING the words did the same. Instance TEN, found in the next
                // round, walked past the first narrowing the same way: `#[doc = "…
                // deny_unknown_fields …"]` IS an attribute line, so requiring `#[` still let a
                // sentence vote. So the line must open the attribute serde itself reads —
                // `#[serde(` — and any `cfg_attr` in the window is refused outright rather than
                // guessed at, because deciding whether a conditional attribute is active needs cfg
                // evaluation this scanner does not do. All 29 protected structs in this crate write
                // it as exactly `#[serde(deny_unknown_fields)]`; a struct that splits it across
                // lines or nests it differently will read as UNPROTECTED, which is the safe
                // direction to be wrong in. What remains outside the guard is stated on
                // `DENY_UNKNOWN_EXEMPT`: it reads what is WRITTEN, not what serde APPLIES.
                let mut decl: Option<&str> = None;
                let mut protected = false;
                for (off, l) in lines[i + 1..].iter().take(8).enumerate() {
                    let lt = l.trim_start();
                    assert!(!lt.starts_with("#[cfg_attr") || !l.contains("deny_unknown_fields"),
                        "{file}:{}: `deny_unknown_fields` appears inside a `cfg_attr`. This scanner \
                         reads source text and cannot evaluate cfg predicates, so it would have to \
                         GUESS whether the attribute is active — and guessing 'yes' is how \
                         `#[cfg_attr(any(), serde(deny_unknown_fields))]` reads as protected while \
                         being inert. Write the attribute unconditionally, or teach this scan the \
                         cfg and add the reasoning here.", i + 2 + off);
                    if lt.starts_with("#[serde(") && l.contains("deny_unknown_fields") {
                        protected = true;
                    }
                    if let Some(rest) = l.split("struct ").nth(1) {
                        decl = Some(rest);
                        break;
                    }
                }
                let decl = decl.unwrap_or_else(|| panic!("{file}:{}: derive(Deserialize) with no \
                    struct within 8 lines — the scan cannot classify what it cannot name", i + 1));
                let name: String = decl.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                if !protected {
                    unprotected.push((file.clone(), name.clone()));
                }
                structs.push((file.clone(), name));
            }
        }

        assert_eq!(
            unprotected, DENY_UNKNOWN_EXEMPT.iter().map(|&(f, s)| (f.to_string(), s.to_string()))
                .collect::<Vec<_>>(),
            "the set of Deserialize structs WITHOUT `deny_unknown_fields` changed. This module's \
             opening paragraph, and `docs/http-api.md`'s section on this rule, both describe which \
             structs make a typo loud; that description is only true while this list is. If you \
             ADDED the attribute to one of these, delete its row here and update both texts (the \
             surface got more honest and the docs should say so). If a NEW struct appears here, it \
             silently ignores unknown fields — decide whether that is intended and say so in the \
             docs before adding it. Scanned: {unprotected:?}"
        );

        assert_eq!(
            structs.len(), DECLARED_REQUEST_STRUCTS,
            "REACH CONTROL: the scan extracted {} Deserialize structs, expected {}. Either one was \
             added or removed — classify it below and update this count — or the extraction broke, \
             in which case the classification assertions underneath are vacuous. Extracted: {:?}",
            structs.len(), DECLARED_REQUEST_STRUCTS, structs
        );

        let classified: Vec<(&str, &str)> = EXCLUSIVE.iter().map(|c| (c.file, c.strukt))
            .chain(COMPOSABLE.iter().map(|&(f, s, _)| (f, s)))
            .chain(NOT_A_REQUEST_INPUT.iter().copied())
            .collect();
        for (file, name) in &structs {
            assert!(
                classified.iter().any(|&(f, s)| f == file && s == name),
                "{file}: `{name}` derives Deserialize but is in none of EXCLUSIVE / COMPOSABLE / \
                 NOT_A_REQUEST_INPUT. Say which it is. Left unclassified, a body whose fields are \
                 ALTERNATIVES gets a precedence chain that answers 200 and drops the loser — that is \
                 #952 and #956, and it is what an unstated decision defaults to."
            );
        }
        for (file, name) in &classified {
            assert!(
                structs.iter().any(|(f, s)| f == file && s == name),
                "{file}::{name} is classified here but no longer exists in the source — this guard \
                 is asserting against a struct that is gone, and whatever replaced it is unguarded."
            );
        }
        assert_eq!(
            classified.len(), structs.len(),
            "a struct is classified twice: {classified:?} vs {structs:?}"
        );
    }
}
