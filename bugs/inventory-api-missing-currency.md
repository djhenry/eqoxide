# /inventory API does not expose currency amounts

**Summary:** `GET /inventory` returns carried items only; there is no way to read
the character's coin (platinum / gold / silver / copper) through the HTTP API.
An agent therefore can't tell whether it can afford a purchase, blocking any
buy-an-item quest step.

**Severity:** Low–Medium (feature gap, not a malfunction; but it blocks
autonomous play that involves buying — e.g. the Bottle of Red Wine quest).

**Zone / area:** HTTP API (`/inventory`, and the player-state in `/debug`).

## Steps to reproduce
1. Log in any character (e.g. Mordeth on port 8766).
2. `GET /inventory` → `{count, items:[...]}` — items only, no money field.
3. `GET /debug` → player pos/zone/class/etc., no currency either.

## Expected
The API exposes the character's coin so an agent can budget purchases — e.g.
`/inventory` includes a `currency` object `{platinum, gold, silver, copper}` (and
ideally bank/cursor coin), or `/debug` includes the player's coin.

## Actual
No endpoint reports coin. The amounts are tracked client-side (the GUI shows
them) and on the server (the `character_currency` table), but neither is surfaced
over the API.

## Diagnosis notes
- The client already receives coin via the Player Profile and coin-update packets
  (the HUD displays it), so the data is present client-side — it just isn't
  published to a shared slot / API response.

## Suspected root cause / fix
(enhancement) Publish the player's coin into the player-state shared slot and add
it to the `/inventory` (and/or `/debug`) JSON. The values already exist in
`game_state`; this is a plumbing/serialization addition, not new protocol work.

## Status
Fixed — `GET /inventory` and `GET /debug` now include a `currency` object
`{platinum, gold, silver, copper}`, published from `gs.coin` via `PlayerState`.
Verified live: Mordeth's API currency matches the server's `character_currency`
row (0/0/0/0 — a fresh L1, which also explains why he couldn't buy wine).
