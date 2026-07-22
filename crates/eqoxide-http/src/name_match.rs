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
/// The JSON spelling is produced by [`MatchQuality::as_str`] — this type is deliberately NOT
/// `serde::Serialize`, so there is exactly ONE way `quality` can reach the wire and no second,
/// silently-diverging encoding to keep in sync (#513 review, F6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MatchQuality {
    /// A case-insensitive equality match on the cleaned name or the raw key. Ordered FIRST so
    /// `min()` over candidate qualities yields the best available one.
    Exact,
    Fuzzy,
}

impl MatchQuality {
    pub fn as_str(self) -> &'static str {
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
    /// Distance from the player to the matched entity, when BOTH positions are genuinely known.
    /// `None` is an honest "unknown" — either the entity has no position, or the server has not
    /// told us our own yet (`GameState::player_pos_known`) — never a fabricated `0` and never a
    /// figure secretly measured from the zone origin (#513 review, F4).
    pub distance: Option<f32>,
    /// How many entities matched the query at this SAME quality — i.e. how ambiguous the resolution
    /// was. `1` means the match was unique; `>1` means several spawns were equally good and this is
    /// the NEAREST of them (see [`resolve_entity`]).
    ///
    /// This exists because `quality:"exact"` alone was still dishonest (#513 review, F2): with 17
    /// spawns all exactly named "a large field rat", an arbitrary one was returned labelled
    /// `"exact"`, and the agent reasonably concluded the resolution was unambiguous when it was a
    /// coin flip. The count lets the caller gate on ambiguity instead of being quietly guessed at.
    pub candidates: usize,
}

impl NameMatch {
    /// The `matched` JSON object every name-resolving endpoint embeds so the agent can confirm the
    /// resolution. `distance` is included only when known (rounded to 0.1u); omitted otherwise so a
    /// missing/unknown position never masquerades as "0 units away".
    pub fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::json!({
            "id": self.id,
            "name": self.name,
            "quality": self.quality.as_str(),
            "candidates": self.candidates,
        });
        if let Some(d) = self.distance {
            // Round in f64 so the JSON reads cleanly ("42.3", not an f32 "42.29999…" artifact).
            m["distance"] = serde_json::json!((d as f64 * 10.0).round() / 10.0);
        }
        m
    }
}

/// Straight-line distance between two world points, or `None` if either is absent.
pub(crate) fn distance_between(a: Option<(f32, f32, f32)>, b: Option<(f32, f32, f32)>) -> Option<f32> {
    match (a, b) {
        (Some(a), Some(b)) => {
            Some(((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt())
        }
        _ => None,
    }
}

/// Classify how `key` matches the (already-lowercased) query `nl`, or `None` if it doesn't at all.
///
/// `Exact` = case-insensitive equality on the cleaned name or on the raw key.
/// `Fuzzy` = the cleaned name or the raw key merely CONTAINS the query.
fn classify(key: &str, nl: &str) -> Option<MatchQuality> {
    let clean = clean_entity_name(key).to_lowercase();
    let raw = key.to_lowercase();
    if clean == nl || raw == nl {
        Some(MatchQuality::Exact)
    } else if clean.contains(nl) || raw.contains(nl) {
        Some(MatchQuality::Fuzzy)
    } else {
        None
    }
}

/// Resolve a (possibly fuzzy) name to a single entity. Both `ids` and `positions` are keyed by the
/// same raw entity name (written together, in lockstep, by `sync_entities`), so the id and the
/// position for a matched key always describe the same spawn.
///
/// Selection, in order:
///   1. **Exact beats fuzzy (#513 goal 1).** Every candidate is classified, and the BEST quality
///      present wins outright — an exact match is never passed over for a nearer fuzzy one.
///   2. **Nearest among equals (#513 review, F2).** Among candidates of that same best quality, the
///      one NEAREST the player is returned. Previously this was an arbitrary `HashMap` iteration
///      pick: with 5 spawns all exactly named "a gnoll", `goto` silently chose one 2.7× further away
///      than another equally-exact candidate, while reporting `quality:"exact"` as if unambiguous.
///      When the player's position is unknown the tie-break falls back to the LOWEST SPAWN ID, so
///      the answer is always deterministic rather than randomized by hash order.
///   3. **Ambiguity is disclosed**, not hidden: [`NameMatch::candidates`] carries how many equally-
///      good matches there were, so the caller can gate on `candidates > 1`.
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

    // Classify every entity once, keeping only those that match at all.
    let matches: Vec<(&String, u32, MatchQuality)> = ids
        .iter()
        .filter_map(|(k, &id)| classify(k, &nl).map(|q| (k, id, q)))
        .collect();

    // Best quality present wins outright (Exact < Fuzzy in the enum's ordering).
    let best = matches.iter().map(|(_, _, q)| *q).min()?;
    let equals: Vec<&(&String, u32, MatchQuality)> =
        matches.iter().filter(|(_, _, q)| *q == best).collect();
    let candidates = equals.len();

    // Nearest among equals; deterministic lowest-id tie-break when distance can't decide (unknown
    // player position, missing entity position, or an exact tie).
    let (key, id, quality) = equals
        .iter()
        .min_by(|a, b| {
            let da = distance_between(player_pos, positions.get(a.0).copied());
            let db = distance_between(player_pos, positions.get(b.0).copied());
            match (da, db) {
                (Some(x), Some(y)) => x.total_cmp(&y).then(a.1.cmp(&b.1)),
                (Some(_), None) => std::cmp::Ordering::Less, // a known distance beats an unknown one
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.1.cmp(&b.1),
            }
        })
        .map(|(k, id, q)| ((*k).clone(), *id, *q))?;

    let pos = positions.get(&key).copied();
    let distance = distance_between(player_pos, pos);
    Some(NameMatch {
        id, name: clean_entity_name(&key), key, quality, pos, distance, candidates,
    })
}

/// Resolve `name` against the live world tables, acquiring BOTH mutexes here so no call site has to
/// get the order right.
///
/// ⚠️ **LOCK ORDER: `entity_positions` BEFORE `entity_ids`.** This is the order the network thread
/// uses in `ActionLoop::sync_entities` and `login.rs` (and `interact.rs`), and it is NOT optional:
/// taking them the other way round is an ABBA inversion. The HTTP thread would hold `ids` and wait
/// for `positions` while the net thread holds `positions` and waits for `ids` — a permanent deadlock
/// on `std::sync::Mutex` (no timeout, no recovery). The net thread stalling means no packets, hence
/// linkdead, while the HTTP request never returns. Found by review on the first cut of #513, which
/// had put the inverted order on the two endpoints an agent hits hardest.
///
/// This is not the only place in the HTTP layer that holds both at once — `move_api::
/// current_target_match` also does, in the same canonical order. The invariant that actually
/// matters: **every site that holds both must take `entity_positions` BEFORE `entity_ids`.**
pub(crate) fn resolve_in_world(
    world: &eqoxide_ipc::WorldSlots,
    name: &str,
    player_pos: Option<(f32, f32, f32)>,
) -> Option<NameMatch> {
    let positions = world.entity_positions.lock().unwrap(); // 1st — canonical order
    let ids = world.entity_ids.lock().unwrap();             // 2nd
    resolve_entity(name, &ids, &positions, player_pos)
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

    // ── #513 review F2 PROPERTY: among N EQUALLY-good matches, return the NEAREST and say N ──────
    // The live proof this was needed (qeytoqrg): 5 spawns all cleanly named "a gnoll". `goto
    // {"name":"a gnoll"}` returned id 437 at distance 3183 while an equally-exact candidate sat
    // ~1163 away — 2.7× nearer, silently passed over, and reported as `quality:"exact"` as though
    // the resolution had been unambiguous.

    /// The nearest of N equal-quality matches is ALWAYS returned, and `candidates` reports N.
    /// Repeated so randomized HashMap order can't let an arbitrary pick pass by luck.
    #[test]
    fn nearest_of_equal_matches_is_always_returned_and_counted() {
        // Five identically-named gnolls at increasing distance from the player at the origin.
        let rows = [
            ("a_gnoll000", 100, (5000.0, 0.0, 0.0)),
            ("a_gnoll001", 101, (4000.0, 0.0, 0.0)),
            ("a_gnoll002", 102, (10.0, 0.0, 0.0)),   // ← NEAREST
            ("a_gnoll003", 103, (3000.0, 0.0, 0.0)),
            ("a_gnoll004", 104, (2000.0, 0.0, 0.0)),
        ];
        for _ in 0..256 {
            let (ids, pos) = tables(&rows);
            let m = resolve_entity("a gnoll", &ids, &pos, Some((0.0, 0.0, 0.0))).expect("matches");
            assert_eq!(m.quality, MatchQuality::Exact);
            assert_eq!(m.id, 102, "must return the NEAREST equal match, not an arbitrary one");
            assert_eq!(m.distance, Some(10.0));
            assert_eq!(m.candidates, 5,
                "ambiguity must be DISCLOSED (5 equally-exact spawns), not hidden behind 'exact'");
        }
    }

    /// A unique match reports `candidates: 1` — so `candidates > 1` is a meaningful ambiguity gate.
    #[test]
    fn unique_match_reports_one_candidate() {
        let rows = [("a_rat000", 10, (1.0, 0.0, 0.0)), ("Guard_Cheslin001", 20, (2.0, 0.0, 0.0))];
        let (ids, pos) = tables(&rows);
        let m = resolve_entity("a rat", &ids, &pos, Some((0.0, 0.0, 0.0))).unwrap();
        assert_eq!(m.candidates, 1);
    }

    /// Nearest-among-equals must NOT let a nearer FUZZY candidate beat a far EXACT one — quality is
    /// still the primary key, distance only breaks ties WITHIN a quality tier.
    #[test]
    fn a_nearer_fuzzy_never_beats_a_distant_exact() {
        let rows = [
            ("a_gnoll000", 100, (9000.0, 0.0, 0.0)),       // exact, very far
            ("a_gnoll_pup001", 101, (1.0, 0.0, 0.0)),      // fuzzy, right next to us
        ];
        for _ in 0..256 {
            let (ids, pos) = tables(&rows);
            let m = resolve_entity("a gnoll", &ids, &pos, Some((0.0, 0.0, 0.0))).unwrap();
            assert_eq!(m.quality, MatchQuality::Exact);
            assert_eq!(m.id, 100, "distance must only break ties WITHIN a quality tier");
            assert_eq!(m.candidates, 1, "only one EXACT candidate exists");
        }
    }

    /// With the player position unknown, selection must still be DETERMINISTIC (lowest spawn id),
    /// never a hash-order coin flip — and distance stays an honest `None`.
    #[test]
    fn unknown_player_position_falls_back_to_deterministic_lowest_id() {
        let rows = [
            ("a_gnoll000", 104, (5000.0, 0.0, 0.0)),
            ("a_gnoll001", 101, (4000.0, 0.0, 0.0)), // lowest id
            ("a_gnoll002", 102, (10.0, 0.0, 0.0)),
        ];
        for _ in 0..256 {
            let (ids, pos) = tables(&rows);
            let m = resolve_entity("a gnoll", &ids, &pos, None).unwrap();
            assert_eq!(m.id, 101, "no player position → deterministic lowest-id pick");
            assert_eq!(m.distance, None, "distance must be an honest unknown, never fabricated");
            assert_eq!(m.candidates, 3);
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

    /// #513 review F1 — LOCK-ORDER REGRESSION GUARD (ABBA deadlock).
    ///
    /// The net thread locks `entity_positions` → `entity_ids` → `entity_poses`. If an HTTP handler
    /// takes any two of them the other way round, the two threads can each hold the lock the other
    /// wants: a permanent deadlock on `std::sync::Mutex` (no timeout, no recovery) that wedges the
    /// client and goes linkdead.
    ///
    /// **#643 extended this to the third lock and to the real writer.** The simulated net thread
    /// below no longer imitates `sync_entities` with a hand-written two-lock sequence — it calls
    /// the actual production publisher, `WorldSlots::publish_entities`, which is now the single
    /// writer of all three maps and takes them in the canonical order. So this guard can no longer
    /// drift away from what the net thread really does. A second hammer replays
    /// `observe::get_entities`' `entity_positions` → `entity_poses` read order, which #643 added:
    /// that order was documented with a "do not reverse these" comment and enforced by nothing.
    ///
    /// This hammers `resolve_in_world` against a thread replaying the net thread's exact order,
    /// on a SEPARATE thread from the test's main thread, so a reintroduced inversion turns into a
    /// bounded, diagnostic test failure instead of wedging the whole `cargo test` process (and, in
    /// CI, silently burning GitHub Actions' 6-hour default job timeout while reporting nothing —
    /// see #593). A deadlocked hammer thread can never be joined, so on timeout we deliberately do
    /// NOT try to join or kill it — we panic from the main thread and let the process exit.
    #[test]
    fn resolve_in_world_uses_the_net_threads_lock_order_and_cannot_deadlock() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{mpsc, Arc};
        use std::time::Duration;

        let world = eqoxide_ipc::WorldSlots::default();
        for i in 0..20u32 {
            world.entity_positions.lock().unwrap()
                .insert(format!("a_rat{i:03}"), (i as f32, 0.0, 0.0));
            world.entity_ids.lock().unwrap().insert(format!("a_rat{i:03}"), i);
        }

        let stop = Arc::new(AtomicBool::new(false));
        // The "net thread": the REAL publisher, which locks positions → ids → poses (#643). Using
        // the production function rather than an imitation means this guard cannot go stale if the
        // writer's lock order ever changes.
        let mut roster = std::collections::HashMap::new();
        for i in 0..20u32 {
            roster.insert(i, eqoxide_core::game_state::make_entity(
                i, &format!("a_rat{i:03}"), i as f32, 0.0, 0.0, true));
        }
        let (w2, s2) = (world.clone(), stop.clone());
        let net = std::thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                std::hint::black_box(w2.publish_entities(&roster));
            }
        });

        // A second reader replaying `observe::get_entities`' order: positions → poses (#643). An
        // inversion HERE deadlocks against the publisher above just as surely as one in the
        // resolver, and until now nothing enforced it.
        let (w3, s3) = (world.clone(), stop.clone());
        let observe = std::thread::spawn(move || {
            while !s3.load(Ordering::Relaxed) {
                let positions = w3.entity_positions.lock().unwrap(); // 1st — canonical order
                let poses = w3.entity_poses.lock().unwrap();         // 2nd
                std::hint::black_box((positions.len(), poses.len()));
            }
        });

        // The hammer runs on its OWN thread so the main thread is free to bound it with a timeout
        // instead of blocking on it directly — a reintroduced ABBA inversion deadlocks this thread
        // permanently, and only a thread that ISN'T stuck can detect that and fail loudly.
        let (tx, rx) = mpsc::channel();
        let world2 = world.clone();
        std::thread::spawn(move || {
            for _ in 0..5_000 {
                let m = resolve_in_world(&world2, "a rat", Some((0.0, 0.0, 0.0)));
                assert!(m.is_some(), "the seeded entities must always resolve");
            }
            // Ignore a failed send: if the receiver already gave up (timed out and panicked), there
            // is nothing left to notify — the process is exiting via that panic regardless.
            let _ = tx.send(());
        });

        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(()) => {
                stop.store(true, Ordering::Relaxed);
                net.join().expect("net-thread stand-in must not have panicked");
                observe.join().expect("observe-reader stand-in must not have panicked");
            }
            // Timeout: the hammer thread never reached the `tx.send`, and it's still out there
            // (never join a thread we suspect is deadlocked) — this is the lock-order inversion
            // this test exists to catch.
            Err(mpsc::RecvTimeoutError::Timeout) => panic!(
                "lock-order inversion: resolve_in_world deadlocked against sync_entities' order"
            ),
            // Disconnected: `tx` was dropped WITHOUT sending, which only happens if the hammer
            // thread's own body panicked first (e.g. the `assert!` above) — a real deadlock can
            // never produce this, since a wedged thread still holds `tx` alive. Report the true
            // failure instead of the lock-order message, which would send a maintainer hunting an
            // inversion that doesn't exist.
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                "hammer thread died before completing (see its panic above) — this is NOT a \
                 lock-order inversion, something else broke resolve_in_world"
            ),
        }
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
            quality: MatchQuality::Exact, pos: None, distance: None, candidates: 1,
        };
        let j = m.to_json();
        assert_eq!(j["id"], 7);
        assert_eq!(j["name"], "a rat");
        assert_eq!(j["quality"], "exact");
        assert_eq!(j["candidates"], 1);
        assert!(j.get("distance").is_none(), "unknown distance must be omitted, not 0");

        let m2 = NameMatch { distance: Some(42.347), ..m };
        assert_eq!(m2.to_json()["distance"], 42.3, "distance rounds to 0.1u");
    }
}
