# Zone rendering: terrain + placeable objects

How a zone's geometry gets on screen, and how placeable objects (buildings, etc.) are placed.

## Terrain

`ZoneAssets::load(<zone>.s3d)` reads the main zone `.wld`(s) via `libeq_wld` and extracts
terrain meshes (positions/normals/uvs + per-mesh `center`) in **libeq space**. The two
horizontal libeq axes map to the **server/world** frame as:

```
render.X (= server x) = libeq p[2] + center[2]
render.Y (= server y) = libeq p[0] + center[0]
render.Z (up)         = libeq p[1] + center[1]
```

i.e. `position = [p2 + cz, p0 + cx, p1 + cy]` in `upload_zone_assets`. The two horizontal
axes are **swapped** relative to the naive `[p0, p2, p1]` — this was confirmed by aligning
each zone's server safe-point/spawn coords with the geometry across qeynos/qeynos2/qcat/
freportw (qcat is decisive: its safe `y=860` only fits the geometry's p0 extent). The same
mapping is applied in `bounds_xy` (minimap) and `Collision::build` (grounding/walls) so
terrain, placed objects, NPCs, and the player all share one frame.

### History / the bug this fixed
Earlier the mapping was `[p0, p2, p1]` (server x→p0, y→p2). That happened to look right in
zones with small/symmetric coords (qeynos, gfaydark) but put NPCs and the player off the
terrain in zones with large authored origins — most visibly **qeynos2**, where the player
spawned well north of / outside the city. Swapping the two horizontal axes fixed it everywhere.

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
