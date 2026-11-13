//! `/v1/combat/*` — targeting, auto-attack, consider, and spell scribe/memorize/cast.

use crate::wire::{Ops, Req, StatusCode};
use super::*;
use crate::refusal::Refusal;

// ── 409 CONFLICT bodies for an occupied command slot (#347 step 2) ───────────────────────────────
// Each `/v1/combat/*` verb queues into a single-slot mailbox the net thread drains once per tick.
// Before #347 a second request inside that window OVERWROTE the pending one and BOTH callers were
// told `200`, so one of the two actions silently never happened. The slot now refuses the second
// write and keeps the first, and these are what the caller is told instead. A 409 here means the
// request was NOT queued and definitively did not happen — retrying after the drain is safe.
// `/attack/on` vs `/attack/off` carries the caller's intended END STATE (`request_attack(true/false)`),
// so a silently dropped one leaves the agent believing it is swinging when it is not. It refuses,
// unlike the deliberately last-wins `request_camp`; see the criterion written out at `BUSY_SIT` in
// `interact.rs` (round-1 review, N1).
const BUSY_MEM_SPELL: &str = "a memorize/scribe request is already queued and undrained — retry in a moment (it was NOT queued)";

pub(super) fn register(ops: &mut Ops) {
    ops.add("POST /v1/combat/memorize", post_memorize);
    ops.add("POST /v1/combat/scribe", post_scribe);
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MemorizeBody { spell_id: u32, gem: u32 }

/// POST /v1/combat/memorize {"spell_id":N,"gem":0-8} — memorize a known (scribed) spell into a gem.
/// Sends OP_MemorizeSpell with scribing=1.
async fn post_memorize(s: HttpState, req: Req) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let b = match req.json::<MemorizeBody>() { Ok(b) => b, Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"spell_id\":N,\"gem\":0-8}".into()) };
    if b.gem > 8 { return (StatusCode::BAD_REQUEST, "gem must be 0-8".into()); }
    if let Some(busy) = s.command.request_mem_spell(b.gem, b.spell_id, 1, None).refused(BUSY_MEM_SPELL) { return busy; }
    (StatusCode::OK, format!("memorizing spell {} into gem {}", b.spell_id, b.gem))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ScribeBody { spell_id: u32, slot: Option<u32>, from: Option<u32> }

/// POST /v1/combat/scribe {"spell_id":N,"from":S,"slot":B?} — scribe a spell scroll into the
/// spellbook at book slot B (default 0). `from` is the scroll's current inventory wire slot (from
/// GET /v1/observe/inventory): the RoF2 server scribes only the scroll on the CURSOR, so the net
/// thread moves `from` → cursor (OP_MoveItem) before sending OP_MemorizeSpell scribing=0, which
/// consumes the scroll. Omit `from` only if the scroll is already on the cursor. See eqoxide#11.
async fn post_scribe(s: HttpState, req: Req) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let b = match req.json::<ScribeBody>() { Ok(b) => b, Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"spell_id\":N,\"from\":S,\"slot\":B?}".into()) };
    let slot = b.slot.unwrap_or(0);
    // #347 step 1 (reject at the door): the drain's FIRST act is to move `from` → cursor. An empty
    // `from` makes that move a no-op (`GameState::move_item` returns early on an absent source), so
    // the scribe went out against whatever was already on the cursor — or nothing — and the caller
    // was told 200. Checked against the published inventory, exactly as `/v1/interact/read` does.
    if let Some(from) = b.from {
        let occupied = s.inventory_slots.inventory.lock().unwrap().iter().any(|i| i.slot == from as i32);
        if !occupied {
            return (StatusCode::NOT_FOUND, format!("no item in slot {from} to scribe"));
        }
    }
    if let Some(busy) = s.command.request_mem_spell(slot, b.spell_id, 0, b.from).refused(BUSY_MEM_SPELL) { return busy; }
    (StatusCode::OK, match b.from {
        Some(f) => format!("scribing spell {} into book slot {} (scroll from slot {})", b.spell_id, slot, f),
        None    => format!("scribing spell {} into book slot {} (scroll assumed on cursor)", b.spell_id, slot),
    })
}

#[cfg(test)]
mod tests {
    use crate::wire::{Req, StatusCode};
    use crate::testkit::{call, empty_state};

    // --- consider: a malformed body must not silently fall back to "current target" ------------

    // --- cast: preserve the "no gem/spell_id" 400, but a malformed body must say so honestly ----

    // ── #347: reject at the door, and never silently overwrite a queued command ─────────────────

    /// Publish one item at a wire slot, exactly as `OP_CharInventory` decoding does.
    fn seed_item(state: &crate::HttpState, slot: i32) {
        state.inventory_slots.inventory.lock().unwrap().push(eqoxide_core::game_state::InvItem {
            slot, item_id: 13073, name: "Spell: Minor Healing".into(), charges: 1, ..Default::default()
        });
    }

    /// #347 step 1: the scribe drain's FIRST act is to move `from` → cursor, and RoF2 scribes only
    /// what is on the cursor. An empty `from` makes that move a no-op, so the scribe went out
    /// against whatever happened to be on the cursor — and the caller was told 200.
    #[tokio::test]
    async fn scribe_from_an_empty_slot_is_404_and_queues_nothing() {
        let state = empty_state();
        let command = state.command.clone();
        let resp = call(&state, "POST /v1/combat/scribe",
            Req::with_body(serde_json::from_str(r#"{"spell_id":202,"from":23}"#).unwrap())).await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
        assert!(command.take_mem_spell().is_none(), "a rejected scribe must not reach the slot");
    }

    #[tokio::test]
    async fn scribe_from_an_occupied_slot_is_200_and_queues_it() {
        let state = empty_state();
        seed_item(&state, 23);
        let command = state.command.clone();
        let resp = call(&state, "POST /v1/combat/scribe",
            Req::with_body(serde_json::from_str(r#"{"spell_id":202,"from":23}"#).unwrap())).await;
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(command.take_mem_spell(), Some((0, 202, 0, Some(23))));
    }

    // ── #348: /combat/target must not adopt a spawn id the zone doesn't have ────────────────────

    // ── #348: /combat/cast on an EMPTY gem must fail loudly, not 200-then-silence ────────────────

    // ── A3 Migration 3 (#448): POST /v1/combat/cast reports the TRUE outcome, not a queued 200 ──


    // ── #513: /combat/target/name discloses the MATCHED entity (id + name + quality) ────────────

}

/// Reach control for `observe::tests::no_past_dated_net_health_stamp_is_taken_from_a_clock_other_than_the_one_that_reads_it` (#760/C1).
///
/// That guard scans this file's source text. A scanner that silently stops early — as it did, when
/// a `/*` inside a route glob in a doc comment latched its block-comment state — reports
/// a clean scan of a corpus it never read, which is a confident falsehood. The guard asserts it can
/// SEE this constant; because it is the last item in the file, seeing it proves the scan arrived at
/// the end. **Keep it last.**
///
/// Clippy's `items_after_test_module` wants this above `mod tests` — that is precisely the
/// reorder that would defeat it, so the lint is silenced here rather than obeyed (#1110).
#[cfg_attr(test, allow(clippy::items_after_test_module))]
#[cfg(test)]
pub(crate) const GUARD_REACH_SENTINEL_COMBAT: u8 = 0;
