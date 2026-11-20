# Visible Terrain in the Observation: Concept Record

**Date:** 2026-11-17
**Status:** Concept. Blocked on moving-surface collision (#194). Nothing here is implemented.

Wire reference: `docs/agent-api.md`. Principle this extends: the agent perceives what a player at
the keyboard would perceive (`docs/specs/2026-11-13-agent-api-verbs-and-calls-design.md` §2).

## 1. Goal

Give an agent the terrain around the character, limited to what the character can currently see,
so a harness can build its own map and plan routes. Today `Observation.visible` lists entities only.
The agent reads terrain from the zone GLBs in `asset_cache_dir`, which is the whole zone and
therefore more than a player sees.

## 2. Decisions

- **Perception is 360 degrees.** A player can swing the third-person camera or look top-down, so a
  forward cone under-reports what a player knows. Distance and line of sight are the only limits.
- **Only terrain visible now is reported.** The agent owns memory. It stores what it has seen and
  keeps its own map, including staleness. The client keeps no seen-before state.
- **Entities sort nearest first.** `Observation.visible` orders by 3D distance, with ascending
  `spawn_id` breaking ties. This shipped in #1211.

## 3. Chosen approach: visible-cell grid

A top-down grid of cells around the character. Each cell reports whether the eye can see it, the
floor height relative to the character, a kind (ground, water, lava, blocked, drop) and whether it
is a zone line. An unseen cell is distinct from an empty cell, which gives the agent its frontier.

- **Cells snap to a world lattice** with index `floor(x / s)`, not to the character. The agent can
  write cells into its map with no resampling, and a cell always names the same ground.
- **Multi-level zones** report the floor surface the eye can see. `Collision::column_surfaces`
  already returns every floor in a column. The selection rule is open (§6).
- **Cost** is roughly one ray per cell, so a 40 by 40 grid is about 1600 rays. The rate or the
  resolution may need to drop with distance.

### Rejected alternatives

| Option | Reason dropped |
|---|---|
| Forward arc sweep of objects | Contradicts the 360 degree decision. |
| Visible surface points (sparse list) | Variable size, and the agent must voxelize it. |
| 360 degree depth panorama | Fixed size but the agent must unproject it, and it reads poorly as text. |
| Extracted features ("ledge at bearing 40") | Puts the player's thinking inside eqoxide. |
| Rendered image | High latency and no names. Possible later as an on-demand look. |

## 4. Dynamic environment

Collision is the static zone mesh. Boats are entities (`is_boat`) outside collision, and
`docs/unified-baked-assets-design.md` lists runtime-controlled shapes as a separate milestone.
A grid built from static collision would show open water under a boat deck.

- **The grid contains static terrain only.** A moving deck is never written into a cell, because
  the agent would store ground that leaves.
- **Dynamic objects are a separate list.** Each entry carries a kind, footprint, position, observed
  velocity and, for doors, open or closed. Velocity is parity, since a player sees motion.
- **A frame hint** such as `standing_on: Option<id>` lets the agent work in a moving surface's local
  frame while riding. It requires the client to track riding (#194 gap 3).
- **Portals** are learned by use. `observe.zone_exits` covers zone lines already. Whether a teleport
  becomes an explicit event is open (§6).
- **Planning is harness work.** Topological graphs, frontier exploration, D* Lite and space-time
  search need the data above and nothing more from eqoxide.

## 5. Order of work

1. **Moving-surface collision first** (#194 gap 3). The client must model a surface whose transform
   changes over time, carry a rider with it, and expose that motion to agents.
2. **Agent detectability is a requirement of step 1.** Platform motion, the rider frame and
   boarding and leaving transitions must be observable, since §4 depends on them.
3. **Then the visible-cell grid** and the dynamic-object list on top of the new collision.

## 6. Open questions

- Delivery as a call or as a reduced-rate field of `Observation`. A call matches the principle
  that reads must not bloat the per-tick line. The other session refactoring the agent API owns
  this decision.
- Cell size and radius, and whether resolution drops with distance.
- The floor selection rule on multi-level columns.
- Whether a closed door is in the collision that line-of-sight rays use today.
- Whether a teleport surfaces as an event or the agent infers it from a position jump.
- Whether water surfaces and transparent materials block the ray.
