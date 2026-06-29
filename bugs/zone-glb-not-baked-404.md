# Zone terrain GLB missing from asset server (404) for some zones

**Summary:** Crossing into certain zones (e.g. `poknowledge`) fails terrain load with a 404 from the asset server because that zone's GLB/manifest was never baked into the shared assets volume.

**Severity:** Medium (those zones render as a void / fallback ground; unrelated to the zone_changed race fixed in 6a337b3)

**Zone / area:** `poknowledge` confirmed; likely any zone not included in the last asset bake.

## Steps to reproduce
1. Launch client (`--config claude`), zone into `arena`.
2. `curl -X POST http://127.0.0.1:$PORT/zone_cross -d '{"zone_id":202}'` (poknowledge).
3. Watch `/tmp/eqoxide-*.log`.

## Expected
Terrain for the new zone loads (`zone_assets::from_glb: loaded N terrain meshes`).

## Actual
```
WARN eqoxide::app: renderer: zone 'poknowledge' load failed:
  manifest zone/poknowledge failed:
  http://localhost:8088/manifest/zone/poknowledge: status code 404
```
The client falls back to `make_fallback_ground()` — no real terrain.

## Diagnosis notes
- The zone_changed→reload trigger now fires reliably (fixed in 6a337b3), so this 404
  is purely an asset-availability gap, not a client trigger bug.
- The asset server only *serves* the shared volume; it never builds. The 404 means
  `zone/poknowledge` has no manifest in the volume.
- Need an inventory of which zones are baked vs. missing.

## Suspected root cause
The last `eqoxide-assets build … --out $VOL` run did not include `poknowledge` (and
possibly other zones). Re-bake the missing zones into the volume and restart the
`eqoxide_assets` container (see the `asset-server-stack` skill). (unconfirmed which
zones are missing beyond poknowledge.)

## Status
Migrated to GitHub issue https://github.com/djhenry/eqoxide_asset_server/issues/1 (in-repo tracker deprecated).
