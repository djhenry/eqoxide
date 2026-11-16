//! The one place a "more than one of these mutually exclusive arguments" refusal is worded.
//!
//! An op argument struct can carry several alternative forms of one argument, such as a corpse
//! `id` or a corpse `name`. Both forms are valid alone and name different things together, so the
//! handler refuses the call with [`conflicting_forms`] and nothing is queued. A handler calls it
//! after the session check and before it reads any world state, because the call is malformed
//! whatever the world holds. Each caller builds `forms` from an exhaustive destructure of its
//! argument struct, so a new field is a compile error until its author decides whether the field
//! joins the exclusive group.

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
    use crate::ops::testkit::{call, empty_state};
    use eqoxide_agent_protocol::call::CallStatus;
    use eqoxide_agent_protocol::op::{
        FriendsEdit, InteractClickDoor, InteractDialogue, InteractLoot, Op, TrainerOpen,
    };

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
        assert!(msg.contains("REFUSED"), "the message must state that nothing happened: {msg}");
        let reversed = conflicting_forms("pet command", &[("name", true), ("command", true)]).unwrap();
        assert!(reversed.contains("name, command"), "order follows the caller's group: {reversed}");
    }

    #[test]
    fn only_group_members_count_toward_a_conflict() {
        assert!(conflicting_forms("x", &[("a", true)]).is_none());
        let msg = conflicting_forms("x", &[("a", true), ("b", false), ("c", true)]).unwrap();
        assert!(msg.contains("a, c") && !msg.contains(", b"), "{msg}");
    }

    /// An op with a mutually exclusive form group, sent with every form of the group at once.
    struct Case {
        op: Op,
        fields: &'static [&'static str],
        what: &'static str,
    }

    fn some(s: &str) -> Option<String> {
        Some(s.to_owned())
    }

    fn exclusive() -> Vec<Case> {
        vec![
            Case {
                op: Op::TrainerOpen(TrainerOpen { name: some("Alpha"), trainer: some("Beta") }),
                fields: &["name", "trainer"],
                what: "guildmaster selection",
            },
            Case {
                op: Op::SocialEditFriends(FriendsEdit { add: some("Alpha"), remove: some("Beta") }),
                fields: &["add", "remove"],
                what: "friends-list edit",
            },
            Case {
                op: Op::InteractLoot(InteractLoot { id: Some(7), name: some("a_rat00") }),
                fields: &["id", "name"],
                what: "corpse selection",
            },
            Case {
                op: Op::InteractClickDoor(InteractClickDoor { door_id: Some(7), name: some("HHCELL") }),
                fields: &["door_id", "name"],
                what: "door selection",
            },
            Case {
                op: Op::InteractDialogue(InteractDialogue { index: Some(0), text: some("bind") }),
                fields: &["index", "text"],
                what: "dialogue choice",
            },
        ]
    }

    /// The words of `text` that are not part of a longer identifier.
    fn tokens(text: &str) -> Vec<&str> {
        text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|t| !t.is_empty()).collect()
    }

    #[tokio::test]
    async fn every_exclusive_group_is_refused_and_names_each_field_under_its_subject() {
        for case in exclusive() {
            let name = case.op.name();
            let answer = call(&empty_state(), case.op).await;
            assert_eq!(answer.status, CallStatus::Invalid, "{name}: {}", answer.body);
            let body = answer.body.as_str().unwrap_or_default();
            assert!(body.starts_with(&format!("conflicting {}", case.what)), "{name}: {body}");
            let words = tokens(body);
            for field in case.fields {
                assert!(words.contains(field), "{name}: the refusal must name `{field}`: {body}");
            }
        }
    }

    #[test]
    fn refusal_prose_never_contains_a_field_name_by_accident() {
        for case in exclusive() {
            let msg = conflicting_forms(case.what, &case.fields.iter().map(|f| (*f, true)).collect::<Vec<_>>())
                .unwrap();
            let supplied = case.fields.join(", ");
            let prose = msg.replacen(&supplied, "", 1);
            for field in case.fields {
                assert!(!tokens(&prose).contains(field), "`{field}` appears in the prose of: {msg}");
            }
        }
    }
}
