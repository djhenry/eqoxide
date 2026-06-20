# Zone rendering: terrain + placeable objects

How a zone's geometry gets on screen, and how placeable objects (buildings, etc.) are placed.

## Terrain

`ZoneAssets::load(<zone>.s3d)` reads the main zone `.wld`(s) via `libeq_wld` and extracts
terrain meshes (positions/normals/uvs + per-mesh `center`) in **libeq space**
`[east, height, north]`. `upload_zone_assets` maps each vertex to render world
`[east, north, up]` via `position = [p0 + cx, p2 + cz, p1 + cy]` — the same frame as NPC/player
server coordinates `[server_x = east, server_y = north, server_z = up]`, so terrain, objects,
and entities all align.

## Placeable objects (buildings, tents, forges, …)

EQ stores object **models** at the origin in `<zone>_obj.s3d`, and the per-instance
**placements** (which model, where, what rotation/scale) as ActorInstance fragments in the
**main** zone `.wld`. `assets.rs::load_placed_objects` ties them together:

1. Load `_obj.s3d` object models, keyed by base name (`NAME_DMSPRITEDEF` → `NAME`), vertices
   already including each mesh's `center`.
2. Read placements from the main zone `.wld` via `WldDoc::objects()` — each `ObjectLocation`
   gives `model_name()` (`NAME_ACTORDEF` → `NAME`), `center()` (libeq `[east, height, north]`),
   `rotation()` (degrees; the `rz` component is the heading about the up axis), and `scale()`.
3. For each placement, transform the matched model's vertices: **scale**, **rotate about the up
   axis** (libeq Y / index 1), then **translate** by the placement center — emitting meshes in
   libeq space so `upload_zone_assets` renders them with terrain.

qeynos: 476/477 placements matched; qeynos2: 478/481.

### History / the bug this fixed
Previously `load_all` merged `_obj.s3d` meshes at their origin and ignored placements, so every
placeable stacked at `0,0,0` (a chaotic pile) and the city was empty — which made NPCs (at
correct city coordinates) appear to stand "in the water" on bare terrain where buildings should
be. Reading and applying the ActorInstance placements fixed all of it.

### Known follow-ups
- A few placements don't match a model (1 qeynos, 3 qeynos2) — likely special/camera/region
  actor types; identify by logging the unmatched `model_name()`.
- Object rotation sign/axis was chosen to make buildings coherent; if any individual object
  faces the wrong way, revisit the rotation in `load_placed_objects` (heading sign / which euler
  component). Verify against the in-game look.
