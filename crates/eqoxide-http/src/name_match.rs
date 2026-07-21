//! Name→entity resolution for the name-based lookup endpoints (`/v1/move/goto {name}`,
//! `/v1/combat/target/name`) — #513, agent-honesty.
//!
//! The bug these fixed: those endpoints returned the resolved COORDINATES / a bare success but
//! never told the caller WHICH entity the fuzzy name-resolution actually matched. So the driving
//! agent — which has no independent channel to reality — could not confirm the resolution picked
//! the intended spawn, and a transient fuzzy fallback could silently route to a *different* entity
//! than the one named (the live near-miss: `goto {name:"a_rodent020"}` resolving to a distant
//! `Astaed_Wemor`). A confident-but-wrong resolution the caller can't catch is exactly the
//! falsehood the honesty invariant forbids.
//!
//! The fix, honesty-first:
//!   1. **Exact beats fuzzy.** An exact (case-insensitive) name match is ALWAYS preferred over any
//!      partial/substring one — an exact match is never passed over for a nearer fuzzy candidate.
//!   2. **The match carries the entity.** Resolution yields a typed [`NameMatch`] — id, canonical
//!      name, match quality, and distance — not bare coordinates. The endpoint derives BOTH the
//!      routed goal AND the disclosed `matched` object from the SAME value, so the disclosure can
//!      never disagree with the entity the caller is actually routed to / targeting.
//!   3. **Honest failure.** No plausible match ⇒ `None` ⇒ the endpoint 404s (the pre-existing
//!      honest-404-on-nonexistent-name path), rather than silently accepting a distant wrong match.

use std::collections::HashMap;
use eqoxide_core::game_state::clean_entity_name;

/// Whether a name resolved by an EXACT (case-insensitive) name match or only a fuzzy/partial
/// (substring) one. The agent gates on this: a `fuzzy` result means "I could not find an entity
/// with this exact name; here is the closest partial match" — the caller should verify (or reject)
/// it, never assume it's the intended spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum MatchQuality {
    Exact,
    Fuzzy,
}

impl MatchQuality {
    fn as_str(self) -> &'static str {
        match self {
            MatchQuality::Exact => "exact",
            MatchQuality::Fuzzy => "fuzzy",
        }
    }
}

/// A resolved name→entity match. Carries everything the agent needs to VERIFY the resolution picked
/// the intended spawn (#513): the spawn id, the canonical (cleaned) name, whether the match was
/// exact or fuzzy, and — when both the entity and the player have a known position — the distance.
///
/// The endpoint MUST derive its routed goal from THIS value's [`pos`](Self::pos) (not a second,
/// independent lookup) so the disclosed `matched` object can never describe a different entity than
/// the one actually routed to. That agreement is the whole point of the type: id, name, and coords
/// travel together and cannot drift apart.
#[derive(Debug, Clone)]
pub(crate) struct NameMatch {
    /// The matched spawn id.
    pub id: u32,
    /// The raw entity table key (e.g. `"a_rat003"`) — the key the nav walker re-resolves each tick
    /// to follow a moving entity's live position.
    pub key: String,
    /// The canonical, human-facing name (`clean_entity_name` of the key) — what the agent sees.
    pub name: String,
    pub quality: MatchQuality,
    /// The matched entity's world position, when it is in the position table.
    pub pos: Option<(f32, f32, f32)>,
    /// Distance from the player to the matched entity, when both positions are known. `None` is an
    /// honest "unknown" (a missing position), never a fabricated `0`.
    pub distance: Option<f32>,
}

impl NameMatch {
    /// The `matched` JSON object every name-resolving endpoint embeds so the agent can confirm the
    /// resolution. `distance` is included only when known (rounded to 0.1u); omitted otherwise so a
    /// missing position never masquerades as "0 units away".
    pub fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::json!({
            "id": self.id,
            "name": self.name,
            "quality": self.quality.as_str(),
        });
        if let Some(d) = self.distance {
            // Round in f64 so the JSON reads cleanly ("42.3", not an f32 "42.29999…" artifact).
            m["distance"] = serde_json::json!((d as f64 * 10.0).round() / 10.0);
        }
        m
    }
}

/// Straight-line distance between two world points, or `None` if either is absent.
fn distance_between(a: Option<(f32, f32, f32)>, b: Option<(f32, f32, f32)>) -> Option<f32> {
    match (a, b) {
        (Some(a), Some(b)) => {
            Some(((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt())
        }
        _ => None,
    }
}

/// Resolve a (possibly fuzzy) name to a single entity, PREFERRING an exact (case-insensitive) name
/// match over any partial/substring one (#513, goal 1). Both `ids` and `positions` are keyed by the
/// same raw entity name (they are written together, in lockstep, by `sync_entities`), so the id and
/// the position for a matched key always describe the same spawn.
///
/// Match order — the first three are `Exact`, the last is `Fuzzy`:
///   1. raw key equality (the exact table key, e.g. an agent echoing a name from `/observe/entities`)
///   2. case-insensitive equality on the cleaned name (`"a rat"` == `clean("a_rat003")`)
///   3. case-insensitive equality on the raw key
///   4. substring (the cleaned name or the raw key CONTAINS the query) — fuzzy/partial
///
/// Returns `None` when nothing even fuzzy-matches, so the caller can 404 honestly instead of
/// routing to a distant wrong entity.
pub(crate) fn resolve_entity(
    name: &str,
    ids: &HashMap<String, u32>,
    positions: &HashMap<String, (f32, f32, f32)>,
    player_pos: Option<(f32, f32, f32)>,
) -> Option<NameMatch> {
    let nl = name.to_lowercase();

    // EXACT first — never passed over for a nearer fuzzy candidate (#513 goal 1).
    let exact = ids
        .get_key_value(name)
        .or_else(|| ids.iter().find(|(k, _)| clean_entity_name(k).to_lowercase() == nl))
        .or_else(|| ids.iter().find(|(k, _)| k.to_lowercase() == nl));

    let (key, id, quality) = if let Some((k, &id)) = exact {
        (k.clone(), id, MatchQuality::Exact)
    } else {
        // FUZZY — only reached when no exact match exists anywhere.
        let (k, &id) = ids.iter().find(|(k, _)| {
            clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl)
        })?;
        (k.clone(), id, MatchQuality::Fuzzy)
    };

    let pos = positions.get(&key).copied();
    let distance = distance_between(player_pos, pos);
    Some(NameMatch { id, key: key.clone(), name: clean_entity_name(&key), quality, pos, distance })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the two lockstep tables (`ids` + `positions`) from `(key, id, pos)` triples, exactly as
    /// `sync_entities` does — same key in both maps.
    fn tables(
        rows: &[(&str, u32, (f32, f32, f32))],
    ) -> (HashMap<String, u32>, HashMap<String, (f32, f32, f32)>) {
        let mut ids = HashMap::new();
        let mut pos = HashMap::new();
        for (k, id, p) in rows {
            ids.insert((*k).to_string(), *id);
            pos.insert((*k).to_string(), *p);
        }
        (ids, pos)
    }

    // ── #513 PROPERTY: an exact match is ALWAYS chosen over any fuzzy candidate ──────────────────
    // The live near-miss shape: the queried name matches ONE entity exactly and (as a substring)
    // several others. Resolution must return the exact one, whatever iteration order the HashMap
    // happens to use — and the returned id/name must be that entity's, never a fuzzy neighbour's.

    #[test]
    fn exact_match_is_always_preferred_over_fuzzy_substrings() {
        // "a_rodent" fuzzy-contains the query "rodent"; "Astaed_Wemor" does NOT — but to model the
        // real near-miss we give the EXACT target a name that is ALSO a substring of decoys, so a
        // naive "first substring hit" resolver could pick a decoy. The exact match must still win.
        let rows = [
            ("a_rat000", 10, (0.0, 0.0, 0.0)),
            ("a_rat_hunter001", 11, (1.0, 0.0, 0.0)), // fuzzy: contains "a rat"
            ("dire_a_rat002", 12, (2.0, 0.0, 0.0)),   // fuzzy: contains "a rat"
        ];
        // Run many times: HashMap iteration order is randomized per-process, so a single pass could
        // pass by luck. If exact-preference were broken (fuzzy taken first), some iteration would
        // surface a decoy.
        for _ in 0..256 {
            let (ids, pos) = tables(&rows);
            let m = resolve_entity("a rat", &ids, &pos, Some((0.0, 0.0, 0.0)))
                .expect("an exact clean-name match exists");
            assert_eq!(m.quality, MatchQuality::Exact);
            assert_eq!(m.id, 10, "must resolve the EXACT 'a rat', never a fuzzy decoy");
            assert_eq!(m.name, "a rat");
        }
    }

    /// PROPERTY: the disclosed id/name/pos always describe the SAME entity — the id maps to the key,
    /// the name is that key's canonical form, and the position is that key's position. The disclosure
    /// can never point at a different spawn than the coordinates.
    #[test]
    fn disclosure_matches_the_routed_entity_for_every_row() {
        let rows = [
            ("a_rat000", 10, (5.0, 6.0, 7.0)),
            ("Guard_Phaeton001", 20, (100.0, 200.0, 3.0)),
            ("Astaed_Wemor002", 30, (-50.0, -60.0, 1.0)),
        ];
        let (ids, pos) = tables(&rows);
        for (key, id, p) in rows {
            let m = resolve_entity(key, &ids, &pos, None).expect("exact key match");
            assert_eq!(m.id, id, "id must be the queried key's id");
            assert_eq!(m.name, clean_entity_name(key));
            assert_eq!(m.pos, Some(p), "pos must be the queried key's position");
        }
    }

    #[test]
    fn case_insensitive_exact_beats_fuzzy() {
        let rows = [
            ("Fippy_Darkpaw000", 1, (0.0, 0.0, 0.0)),
            ("Fippy_the_Bold001", 2, (1.0, 1.0, 1.0)), // fuzzy: contains "fippy"
        ];
        let (ids, pos) = tables(&rows);
        let m = resolve_entity("fIpPy dArKpAw", &ids, &pos, None).expect("ci exact");
        assert_eq!(m.quality, MatchQuality::Exact);
        assert_eq!(m.id, 1);
    }

    #[test]
    fn fuzzy_is_signalled_when_only_a_partial_match_exists() {
        let rows = [("Astaed_Wemor000", 30, (10.0, 10.0, 0.0))];
        let (ids, pos) = tables(&rows);
        // "Wemor" is only a SUBSTRING of the canonical name — no exact match anywhere.
        let m = resolve_entity("Wemor", &ids, &pos, None).expect("fuzzy substring");
        assert_eq!(m.quality, MatchQuality::Fuzzy,
            "a partial-only match must be flagged fuzzy so the agent can gate on it");
        assert_eq!(m.id, 30);
    }

    #[test]
    fn nonexistent_name_resolves_to_none() {
        let rows = [("a_rat000", 10, (0.0, 0.0, 0.0))];
        let (ids, pos) = tables(&rows);
        assert!(resolve_entity("a dragon", &ids, &pos, None).is_none(),
            "a name that doesn't even fuzzy-match must be None (→ honest 404), not a wrong match");
    }

    #[test]
    fn distance_is_computed_when_both_positions_known() {
        let rows = [("a_rat000", 10, (3.0, 4.0, 0.0))];
        let (ids, pos) = tables(&rows);
        let m = resolve_entity("a rat", &ids, &pos, Some((0.0, 0.0, 0.0))).unwrap();
        assert_eq!(m.distance, Some(5.0), "3-4-5 triangle");
    }

    #[test]
    fn distance_is_none_when_player_position_unknown() {
        let rows = [("a_rat000", 10, (3.0, 4.0, 0.0))];
        let (ids, pos) = tables(&rows);
        let m = resolve_entity("a rat", &ids, &pos, None).unwrap();
        assert_eq!(m.distance, None, "unknown player pos → honest None, never a fake 0");
    }

    #[test]
    fn to_json_omits_distance_when_unknown_and_rounds_when_known() {
        let m = NameMatch {
            id: 7, key: "a_rat000".into(), name: "a rat".into(),
            quality: MatchQuality::Exact, pos: None, distance: None,
        };
        let j = m.to_json();
        assert_eq!(j["id"], 7);
        assert_eq!(j["name"], "a rat");
        assert_eq!(j["quality"], "exact");
        assert!(j.get("distance").is_none(), "unknown distance must be omitted, not 0");

        let m2 = NameMatch { distance: Some(42.347), ..m };
        assert_eq!(m2.to_json()["distance"], 42.3, "distance rounds to 0.1u");
    }
}
