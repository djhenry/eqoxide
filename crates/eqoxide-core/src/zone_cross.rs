//! Zone-line crossing: the decisions BOTH the net thread and the HTTP surface need.
//!
//! # Why this lives in `eqoxide-core` and not in `eqoxide-net` (#713)
//!
//! [`classify_unresolved_cross`] is the #679/#683 gate: it decides whether the client may fire the
//! retail `OP_ZoneChange(zoneID = 0)` "let the server resolve it" crossing at a zone-line region
//! whose baked index matches no advertised zone point. It used to be a `pub(crate)` item of
//! `eqoxide_net::action_loop`, reachable only by the code that ACTS on it.
//!
//! `/v1/observe/zone_exits` now has to REPORT that same verdict (#713 item 3: a `zone_id: null`
//! exit in a gated zone cannot be auto-crossed, and an agent deserves to know that BEFORE walking
//! there rather than from a message-log line after it arrives). A reporter that re-derives the
//! verdict from its own copy of the rule is two rules that can drift — and a drifted `gated` flag
//! is precisely the confident falsehood the agent-honesty invariant forbids, because the agent has
//! no way to check it. So the function moved DOWN to the crate both sides already depend on, and
//! both sides call the same one. It is unchanged: same body, same gates, same property test.
//!
//! # What is NOT here
//!
//! Nothing in this module decides *where* a crossing goes, and nothing in it changes what the gate
//! decides. #713 is a reporting change: it exposes the existing verdict, it does not re-open the
//! same-zone-pad question (#543/#266/#582), which is owner-decision-pending.

use crate::game_state::ZonePoint;

/// Disposition for a standing auto-cross on a zone-line region whose index matches NO advertised
/// zone point (#683): `SendServerResolved` = send OP_ZoneChange with zoneID=0 and let the server
/// resolve the destination from our position; `Ignore` = keep the old no-op.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum UnresolvedCross {
    SendServerResolved,
    Ignore,
}

/// Classify an unresolved-index zone-line hit (#683) into its ONE disposition — a pure function.
///
/// The retail client sends OP_ZoneChange(zoneID=0) on ANY zone-line region hit and the server
/// resolves the destination from position (`Handle_OP_ZoneChange` →
/// `GetClosestZonePointWithoutZone`, an XY-nearest match over the zone's DB zone points) — no
/// embedded index required. So "the region's index resolves to no advertised point" must not be a
/// silent no-op: qrg's ONLY exit region is baked with index 0, and ignoring it was a total
/// zone-exit lockout.
///
/// Two gates keep this from re-opening the #679 same-zone drift (a zoneID=0 packet fired while
/// standing on an INTRA-zone translocator pad makes the server resolve nearest-XY to a real
/// cross-zone trigger and zone us away — the "right location, then zoned away" bug):
/// 1. **At least one SERVER-ORIGINATED advert must be present** — an entry with
///    `iterator != u32::MAX`. Before OP_SendZonepoints arrives EVERY region index is unresolved —
///    including the indices of known same-zone pads — so firing in that window could zone a
///    character standing on a pad. Plain non-emptiness does NOT prove the adverts arrived: the
///    shared list also carries CLIENT-SYNTHESIZED map-label fallback points, which
///    `sync_zone_points` marks with `iterator: u32::MAX` (#683 review F2, observed live in
///    qeynos) — those prove nothing about the server, so they don't count.
/// 2. **No advertised point may target the CURRENT zone.** A same-zone pad is a DB zone point
///    targeting its own zone, and the server advertises the zone's DB points — so "some advertised
///    point targets this zone" ⇒ intra-zone pads exist here, and an unresolved region cannot be
///    distinguished from a misbaked pad. Refuse in such zones. (A properly-advertised pad never
///    even gets here — its index resolves, and `perform_cross`'s same-zone branch sends NO packet.)
///    This gate counts EVERY entry, synthetic ones included — the conservative direction.
///
/// **Scope of the guarantee (function level, property-tested):** `SendServerResolved` is returned
/// ONLY when the list holds a server-originated advert AND no entry targets the current zone —
/// see `classify_unresolved_cross_property_683`. The remaining SYSTEM premise, which the property
/// cannot see, is that the list's server-originated entries belong to the CURRENT zone; its two
/// known violators are closed at the source (`GameState::begin_zone_in` clears the previous
/// zone's points; `sync_zone_points` rebuilds the shared list on every zone change) but that is a
/// maintained invariant of those writers, not a structural impossibility here.
///
/// qrg advertises only cross-zone points, so its locked exit passes both gates.
///
/// **Input contract (#713).** `zone_points` must be the SHARED list — the one
/// `ActionLoop::sync_zone_points` publishes, which carries both the server adverts and the
/// client-synthesized `iterator == u32::MAX` map labels. Gate 1 is defined in terms of that
/// distinction, so passing `GameState::world.zone_points` (server entries only) instead would make
/// gate 1 trivially true and is NOT the same question. Both callers — the net thread's
/// `cross_unresolved` and the HTTP `/v1/observe/zone_exits` reporter — pass the shared list.
pub fn classify_unresolved_cross(zone_points: &[ZonePoint], current_zone_id: u16) -> UnresolvedCross {
    let have_server_adverts = zone_points.iter().any(|zp| zp.iterator != u32::MAX);
    let same_zone_pad_advertised = zone_points.iter().any(|zp| zp.zone_id == current_zone_id);
    if have_server_adverts && !same_zone_pad_advertised {
        UnresolvedCross::SendServerResolved
    } else {
        UnresolvedCross::Ignore
    }
}

/// How many auto-cross attempts the client will make during ONE continuous stand on zone-line
/// geometry before it stops and publishes the terminal state (#713 item 1, from the #683 review's
/// F3). See [`CrossAttempts`] for why the tally is per stand rather than per region index.
///
/// **Why 3 — a judgement, not a measurement.** I did not measure the distribution of server
/// denials, so this is reasoned:
/// * `1` is too few. A server refusal is not proof of a permanent condition — EQEmu's
///   `Handle_OP_ZoneChange` refuses transiently too (already-zoning state, a rule/instance check
///   that can change), and stopping on the first one would strand a character on a line that would
///   have worked on the next try.
/// * The refire interval is the auto-cross cooldown (10 s), so `3` caps the server-side anomaly
///   events at three per continuous stand — roughly a 20 s window for a transient to clear —
///   instead of the unbounded stream F3 reported.
/// * A too-low bound is CHEAP to recover from, which argues for the small end: the tally is cleared
///   on the FIRST tick the standing probe finds the character off every zone-line region, so
///   stepping off the line and back on re-arms it. (That probe deliberately runs OUTSIDE the
///   auto-cross cooldown. While it ran inside it, this whole argument was unsound — the recovery
///   the small bound is justified by did not actually work; #713 review round 2, B2.) A too-HIGH
///   bound is not recoverable — it is just more anomaly events.
pub const MAX_CROSS_ATTEMPTS: u32 = 3;

/// Auto-cross attempts made during ONE continuous stand on zone-line geometry that did not result
/// in a crossing (#713 item 1).
///
/// # Why this counts ATTEMPTS and not server DENIALS
///
/// The issue asked for "N consecutive denials". Counting attempts is strictly stronger and is what
/// this implements. An `OP_ZoneChange` request carries no correlation id, so a denial echo can only
/// be attributed to a request by recency — and, worse, a denial-counter cannot see the case where
/// the server answers **nothing at all**, which refires forever exactly like the denial case it was
/// written to bound.
///
/// The attempt counter needs no echo: the auto-cross fires only while the character is physically
/// standing inside a zone-line region, and a crossing that WORKED removes them from it (a
/// cross-zone change tears down the zone session; a same-zone translocator repositions them off the
/// pad). So "we fired, the cooldown elapsed, and we are STILL standing in zone-line geometry" is
/// direct evidence the previous attempt did not take effect, whatever the server did or did not say.
///
/// # Why the tally is NOT per region index
///
/// It was, in the first cut of #713 — `record` started a fresh count whenever the index changed, on
/// the reasoning that "a character touching several lines should not be stopped by another line's
/// history". **The property test falsified that**: the sequence `[OnA, OnA, OnA, OnB, OnA]` — never
/// once off zone-line geometry — attempted region A four times, because the single intervening tick
/// on B reset the tally. Two adjacent or overlapping DRNTP boxes are enough to make an
/// alternating stand refire without limit, which is exactly the unbounded storm the bound exists to
/// stop. A per-index tally cannot close that without unbounded per-index state.
///
/// So the count is TOTAL attempts since the character was last off every zone-line region, and
/// [`Self::blocks`] takes no index. `last_index` is retained for reporting only — it names the most
/// recent region attempted, and is not part of the predicate.
///
/// **The accepted cost, stated as a rule rather than as its one example:** when a stand spans more
/// than one region, the allowance is shared, so a region that would have crossed can be refused
/// because a DIFFERENT region in the same stand used up the budget. That is the safe direction (it
/// errs toward fewer server anomaly events), it is announced rather than silent, and it is
/// recovered by the documented escape hatch — leave all zone-line geometry and return.
///
/// The bound is therefore on *attempts per continuous stand*, and it holds regardless of what comes
/// back on the wire. It is NOT a claim that the client can never send more than
/// [`MAX_CROSS_ATTEMPTS`] zone-change packets in a session — leaving zone-line geometry and
/// returning legitimately re-arms it, and that is the intended escape hatch, not a leak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrossAttempts {
    last_index: i32,
    count: u32,
}

impl CrossAttempts {
    /// The zone-line region index of the MOST RECENT attempt. Reporting only — it is deliberately
    /// not part of [`Self::blocks`] (see the type doc). Region indices are a per-zone namespace.
    pub fn last_index(&self) -> i32 {
        self.last_index
    }

    /// How many attempts have been made during the current stand, across every region it touched.
    /// Never exceeds [`MAX_CROSS_ATTEMPTS`], because [`Self::blocks`] stops the next attempt at
    /// that point.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Fold one attempt at `index` into the running tally. The count is NOT per-region — see the
    /// type doc for the alternating-region leak that forced this.
    pub fn record(prev: Option<Self>, index: i32) -> Self {
        CrossAttempts {
            last_index: index,
            count: prev.map_or(1, |p| p.count.saturating_add(1)),
        }
    }

    /// **The single predicate.** `true` = the client will not attempt another auto-cross until the
    /// tally is cleared (by stepping off every zone-line region, or by zoning).
    ///
    /// The gate in `ActionLoop::drain_zone_cross` and the `zone_cross_stopped` observable on
    /// `GET /v1/observe/debug` both read THIS — so "the client stopped trying" and "the client says
    /// it stopped trying" cannot disagree. That is the honesty-relevant invariant here, not the
    /// particular value of [`MAX_CROSS_ATTEMPTS`]: a bound whose observable could drift from it
    /// would be a silent wrong answer, which outranks the retry storm it replaces.
    pub fn blocks(&self) -> bool {
        self.count >= MAX_CROSS_ATTEMPTS
    }
}

/// How the destination of a drained `POST /v1/move/zone_cross` was resolved (#713 item 2, from the
/// #683 review's F4).
///
/// The #683 fallback walks the character to a zone-line region whose baked index matches no
/// advertised zone point and lets the server pick the destination. Nothing false is asserted when
/// that happens — the arrival zone is echo-derived, and the client never claims the requested zone
/// — but before #713 the ONLY signal was a prose line in the message log ("destination resolved by
/// server"). A caller could not tell "I got the zone I asked for" from "I got whatever the server
/// picked" except by diffing requested-vs-arrived after the fact. That is an *undetectable
/// degradation*, not a lie; this type makes it detectable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneCrossResolution {
    /// The located zone-line region's baked index DOES match an advertised zone point, so the
    /// destination is known before the walk starts. Carries that advertised destination.
    ///
    /// Still not a promise about where the character lands: the server resolves the crossing from
    /// position when the character arrives. It is the client's best pre-walk knowledge, and it is
    /// the non-degraded case.
    Advertised { zone_id: u16 },
    /// The #683 best-effort fallback: the located region's index matches NO advertised zone point,
    /// so the client is walking to a line whose destination it genuinely does not know and the
    /// server will decide. **This is the degradation the caller could not previously detect.**
    ServerResolved,
}

impl ZoneCrossResolution {
    /// The stable machine token for this resolution, for the agent-facing JSON. A closed
    /// vocabulary, like `nav_reason`.
    pub fn token(&self) -> &'static str {
        match self {
            ZoneCrossResolution::Advertised { .. } => "advertised_destination",
            ZoneCrossResolution::ServerResolved => "server_resolved_destination",
        }
    }

    /// `true` for the best-effort case — the one a caller must be able to detect.
    pub fn is_best_effort(&self) -> bool {
        matches!(self, ZoneCrossResolution::ServerResolved)
    }
}

/// What the most recent drained `POST /v1/move/zone_cross` actually decided to do (#713 item 2).
///
/// Published on `GET /v1/observe/debug`. Set when the resolution picks a zone-line region to walk
/// to; cleared at the start of every resolution (so a request that failed to locate a line leaves
/// no stale plan behind) and by `GameState::begin_zone_in` (a plan is about the zone it was made
/// in — region indices and advertised zone ids are per-zone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneCrossPlan {
    /// The `zone_id` the caller asked for, or `None` when it asked for "the nearest line"
    /// (`{}` / `zone_id: 0`). Kept so a caller can see WHICH request this plan answers.
    pub requested_zone_id: Option<u16>,
    /// The zone-line region index being walked to. Per-zone namespace.
    pub index: i32,
    /// Whether the destination is known up front, or is the server's to pick.
    pub resolution: ZoneCrossResolution,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zp_at(iterator: u32, zone_id: u16, at: [f32; 3]) -> ZonePoint {
        ZonePoint {
            iterator,
            server_x: at[0],
            server_y: at[1],
            server_z: at[2],
            heading: 0.0,
            zone_id,
        }
    }

    /// **#683 — the unresolved-cross gates, at the classification boundary.** The fallback may fire
    /// ONLY when zone points have arrived AND none of them targets the current zone; both gates
    /// exist to keep the zoneID=0 packet structurally unreachable from an intra-zone translocator
    /// pad (the #679 drift). Mutation checks: drop the non-empty gate → the first assertion goes
    /// RED; drop the same-zone gate → the third goes RED; invert the whole classify → the second.
    ///
    /// (Moved verbatim from `eqoxide_net::action_loop` by #713, with the function it tests.)
    #[test]
    fn classify_unresolved_cross_fires_only_without_same_zone_pads_683() {
        const HERE: u16 = 54;
        // Zone-in window: OP_SendZonepoints not yet received — EVERY index is unresolved then,
        // including known same-zone pads', so the fallback must stay shut.
        assert_eq!(classify_unresolved_cross(&[], HERE), UnresolvedCross::Ignore,
            "no zone points yet → every index is unresolved → firing could zone a char off a pad");
        // The qrg shape: only CROSS-zone points advertised → the fallback is safe and must fire
        // (this is the exit-lockout fix itself).
        let qrg_like = [zp_at(1, 181, [1790.0, 1315.0, -13.0]), zp_at(2, 4, [196.7, 5100.9, -1.0])];
        assert_eq!(classify_unresolved_cross(&qrg_like, HERE), UnresolvedCross::SendServerResolved,
            "cross-zone-only advertisements → server-resolved crossing allowed (the #683 fix)");
        // A same-zone pad advertised: an unresolved region here is indistinguishable from a
        // misbaked pad, and zoneID=0 on a pad is exactly the #679 wrong-zone drift → refuse.
        let with_pad = [zp_at(1, 181, [0.0; 3]), zp_at(9, HERE, [111.0, 222.0, 33.0])];
        assert_eq!(classify_unresolved_cross(&with_pad, HERE), UnresolvedCross::Ignore,
            "a same-zone pad advertised in this zone → the fallback must refuse (#679 defense)");
        // CLIENT-SYNTHESIZED map-label entries (`sync_zone_points` marks them iterator=u32::MAX)
        // prove nothing about OP_SendZonepoints having arrived — a list of ONLY those must stay
        // shut (#683 review F2: plain non-emptiness was satisfiable with zero server adverts).
        let synthetic_only = [zp_at(u32::MAX, 2, [10.0, 20.0, 0.0]), zp_at(u32::MAX, 1, [-5.0, -6.0, 0.0])];
        assert_eq!(classify_unresolved_cross(&synthetic_only, HERE), UnresolvedCross::Ignore,
            "client-synthesized label points alone must not open the fallback — no server advert has arrived");
    }

    /// **#683 review F2 — the classify guarantee, discharged as a PROPERTY, not examples.**
    ///
    /// Exhaustively enumerates every zone-point list of length 0..=3 over an alphabet of the six
    /// entry shapes that exist in the wild (server cross-zone advert, server same-zone advert,
    /// server zone_id-0 sentinel, and the client-synthesized `iterator=u32::MAX` variants of each)
    /// — 259 lists — and asserts the guarantee as an implication over ALL of them:
    ///
    ///   `SendServerResolved`  ⇒  (∃ server-originated entry) ∧ (∄ entry targeting the current zone)
    ///
    /// plus the two refusal directions (no server entry ⇒ Ignore; any same-zone entry ⇒ Ignore).
    /// This is what lets the doc say the zoneID=0 fallback cannot fire from a list that lacks a
    /// server advert or contains a same-zone pad — for EVERY such list, not the five hand-picked
    /// ones. (The remaining system premise — that server entries belong to the CURRENT zone — is a
    /// writer invariant pinned by `begin_zone_in_clears_the_previous_zones_zone_points_683` in
    /// `game_state.rs` and `sync_zone_points`' clear-on-zone-change; a function-level property
    /// cannot see writers.)
    ///
    /// Mutation checks: revert gate 1 to plain `!zone_points.is_empty()` → the synthetic-only
    /// lists violate the implication, RED; drop the same-zone gate → RED; invert either → RED.
    ///
    /// (Moved verbatim from `eqoxide_net::action_loop` by #713, with the function it tests.)
    #[test]
    fn classify_unresolved_cross_property_683() {
        const HERE: u16 = 54;
        const OTHER: u16 = 181;
        // The six entry shapes. (zone_id 0 = the server's "resolve from position" sentinel.)
        let shapes: [ZonePoint; 6] = [
            zp_at(1, OTHER, [1.0, 2.0, 3.0]),        // server advert, cross-zone
            zp_at(9, HERE, [4.0, 5.0, 6.0]),         // server advert, SAME-zone (a pad)
            zp_at(0, 0, [0.0, 0.0, 0.0]),            // server sentinel advert
            zp_at(u32::MAX, OTHER, [7.0, 8.0, 0.0]), // synthetic map label, cross-zone
            zp_at(u32::MAX, HERE, [9.0, 1.0, 0.0]),  // synthetic map label, same-zone
            zp_at(u32::MAX, 0, [0.0, 0.0, 0.0]),     // synthetic, sentinel-shaped
        ];
        let mut checked = 0u32;
        // All lists of length 0..=3 over the alphabet (6^0 + 6^1 + 6^2 + 6^3 = 259).
        let mut all: Vec<Vec<ZonePoint>> = vec![vec![]];
        let mut frontier: Vec<Vec<ZonePoint>> = vec![vec![]];
        for _ in 0..3 {
            let mut next = Vec::new();
            for l in &frontier {
                for s in &shapes {
                    let mut l2 = l.clone();
                    l2.push(s.clone());
                    next.push(l2);
                }
            }
            all.extend(next.iter().cloned());
            frontier = next;
        }
        assert_eq!(all.len(), 259, "exhaustive enumeration of lengths 0..=3");
        for list in &all {
            let got = classify_unresolved_cross(list, HERE);
            let has_server_entry = list.iter().any(|zp| zp.iterator != u32::MAX);
            let has_same_zone = list.iter().any(|zp| zp.zone_id == HERE);
            if got == UnresolvedCross::SendServerResolved {
                assert!(has_server_entry && !has_same_zone,
                    "fired without its premises: server_entry={has_server_entry} same_zone={has_same_zone}, list={list:?}");
            }
            if !has_server_entry {
                assert_eq!(got, UnresolvedCross::Ignore,
                    "no server-originated advert ⇒ the zone-in window may be open ⇒ must refuse: {list:?}");
            }
            if has_same_zone {
                assert_eq!(got, UnresolvedCross::Ignore,
                    "an entry targets the current zone ⇒ pads may exist here ⇒ must refuse (#679): {list:?}");
            }
            checked += 1;
        }
        assert_eq!(checked, 259);
    }

    /// **#713 item 1 — the bound, as a PROPERTY over every event sequence, not one example.**
    ///
    /// The universal being discharged: *between two ticks off zone-line geometry, the client cannot
    /// make more than [`MAX_CROSS_ATTEMPTS`] auto-cross attempts, at any region or combination of
    /// regions.* A live run can never discharge that (it is a "cannot"), so it is checked here over
    /// every sequence of length 0..=8 drawn from the three events the standing auto-cross can see —
    /// a tick on region A, a tick on region B, and a tick off every region — 3^0+…+3^8 = 9841
    /// sequences.
    ///
    /// The simulation below is the auto-cross's OWN control flow (`blocks` → skip, else `record`),
    /// which is what makes this a test of the type rather than of a copy of the rule.
    ///
    /// **This property FOUND A BUG rather than confirming one.** The first cut kept a per-index
    /// tally that reset when the index changed; `[OnA, OnA, OnA, OnB, OnA]` — never once off
    /// zone-line geometry — then attempted A four times, and an alternating stand between two
    /// adjacent DRNTP boxes was unbounded, i.e. the exact storm the bound exists to stop. The type
    /// was changed (the tally is now per STAND, not per region); this is why `blocks()` takes no
    /// index. Recorded here because the reasoned design was wrong and only the property said so.
    ///
    /// Mutation checks:
    /// * delete the `blocks()` guard from the simulated tick (i.e. always attempt) → RED;
    /// * make `record` restart the count when the index changes (the original bug) → RED at the
    ///   alternating sequences;
    /// * make `record` not increment at all → the non-vacuity assertion goes RED.
    #[test]
    fn auto_cross_attempts_are_bounded_per_stand_713() {
        #[derive(Clone, Copy, Debug)]
        enum Tick { OnA, OnB, Off }
        const A: i32 = 0;
        const B: i32 = 7;

        let alphabet = [Tick::OnA, Tick::OnB, Tick::Off];
        let mut seqs: Vec<Vec<Tick>> = vec![vec![]];
        let mut frontier: Vec<Vec<Tick>> = vec![vec![]];
        for _ in 0..8 {
            let mut next = Vec::new();
            for s in &frontier {
                for t in &alphabet {
                    let mut s2 = s.clone();
                    s2.push(*t);
                    next.push(s2);
                }
            }
            seqs.extend(next.iter().cloned());
            frontier = next;
        }
        assert_eq!(seqs.len(), 9841, "exhaustive enumeration of tick sequences of length 0..=8");

        let mut saw_a_blocked_run = false;
        let mut saw_alternating_stand = false;
        for seq in &seqs {
            let mut tally: Option<CrossAttempts> = None;
            // Attempts made since the last time the character was OFF every zone-line region — the
            // "continuous stand" the bound is stated over.
            let mut attempts_this_stand = 0u32;
            let mut regions_this_stand: std::collections::BTreeSet<i32> = Default::default();
            for t in seq {
                let index = match t {
                    Tick::OnA => Some(A),
                    Tick::OnB => Some(B),
                    Tick::Off => None,
                };
                match index {
                    // This models the SHAPE of `drain_zone_cross`'s gate (consult `blocks`, then
                    // attempt + `record`) but deliberately NOT its cooldown: this arm attempts on
                    // every `On` tick, while the real function attempts at most once per
                    // `ZONE_CROSS_COOLDOWN_MS`. That gap is why this property test, though its model
                    // is correct as a model, did not catch #713 round 2's B2 defect — the reset below
                    // is what needed to track the cooldown-gated code, and this arm does not exercise
                    // that interaction. See the `None` arm's comment for what actually grades the real
                    // function.
                    Some(i) => {
                        if tally.is_some_and(|t| t.blocks()) {
                            saw_a_blocked_run = true;
                            continue;
                        }
                        tally = Some(CrossAttempts::record(tally, i));
                        attempts_this_stand += 1;
                        regions_this_stand.insert(i);
                        if regions_this_stand.len() > 1 { saw_alternating_stand = true; }
                    }
                    // Off every zone-line region: the stand ends and the tally is re-armed. In
                    // `drain_zone_cross` this is a STANDALONE `if index.is_none() { .. }` reset block
                    // (round 2, B2) — there is no `else` branch any more; the cooldown-gated attempt
                    // below it is a separate, later `if`. Whether that real block behaves like this
                    // arm is asserted by this comment, not checked by this test: this property grades
                    // the MODEL above, and the model has no cooldown (see the `Some` arm). The test
                    // that actually drives the real `drain_zone_cross` and grades this correspondence
                    // is `the_escape_hatch_works_on_the_tick_you_step_off_713`
                    // (crates/eqoxide-net/src/action_loop.rs) — trust that one over this comment if
                    // they ever disagree.
                    None => {
                        tally = None;
                        attempts_this_stand = 0;
                        regions_this_stand.clear();
                    }
                }
                // The observable and the gate must agree at EVERY step, not just at the end: the
                // tally reports blocking exactly when the next attempt would be refused.
                assert_eq!(tally.is_some_and(|t| t.blocks()),
                           attempts_this_stand >= MAX_CROSS_ATTEMPTS,
                    "gate/observable disagreement after {seq:?}");
            }
            assert!(attempts_this_stand <= MAX_CROSS_ATTEMPTS,
                "{attempts_this_stand} attempts in one continuous stand (bound is \
                 {MAX_CROSS_ATTEMPTS}) for sequence {seq:?}");
        }
        // The enumeration must actually EXERCISE the bound and the multi-region stand, or the
        // assertions above are vacuous.
        assert!(saw_a_blocked_run, "no sequence ever hit the bound — the property would be vacuous");
        assert!(saw_alternating_stand,
            "no sequence ever attempted TWO different regions within one stand — the case that \
             falsified the original per-index design would be untested");
    }

    /// The count a stopped tally REPORTS is exactly the count that stopped it — the gate and the
    /// observable read one predicate, so "stopped trying" and "says it stopped" cannot disagree.
    #[test]
    fn a_blocking_tally_reports_the_bound_it_reached_713() {
        let mut t = CrossAttempts::record(None, 4);
        assert_eq!(t.count(), 1);
        assert!(!t.blocks(), "one attempt is not the bound");
        while !t.blocks() {
            t = CrossAttempts::record(Some(t), 4);
        }
        assert_eq!(t.count(), MAX_CROSS_ATTEMPTS,
            "the tally blocks at exactly the documented bound, and reports that number");
        assert_eq!(t.last_index(), 4, "and names the region it last tried");
        // The allowance is per STAND, not per region: an attempt at another region while blocked is
        // still blocked. (This is the accepted cost documented on `CrossAttempts` — recorded as a
        // test so nobody "fixes" it back into the unbounded per-index design.)
        assert!(CrossAttempts::record(Some(t), 5).blocks(),
            "the tally is per continuous stand — switching region does not buy a fresh allowance");
    }

    /// The two resolutions are distinguishable by a stable machine token, and only the #683
    /// fallback is flagged as best-effort (#713 item 2). Mutation: make `is_best_effort` true for
    /// `Advertised` → RED.
    #[test]
    fn only_the_server_resolved_arm_is_best_effort_713() {
        let exact = ZoneCrossResolution::Advertised { zone_id: 45 };
        let degraded = ZoneCrossResolution::ServerResolved;
        assert!(!exact.is_best_effort());
        assert!(degraded.is_best_effort());
        assert_ne!(exact.token(), degraded.token(), "the tokens must be distinguishable");
        assert_eq!(degraded.token(), "server_resolved_destination");
    }
}
