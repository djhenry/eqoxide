# Unified static visual CPU inspection

`eqoxide_assets::static_visual::decode_static_visual` reads the common static visual GLB profile produced by the asset server. It returns validated geometry, instances, normalized material factors and alpha settings, full optional RGBA, indexed textures and exact embedded PNG payloads. PNG validation does not quantize 16-bit input. Images and texture references remain separate resources; names and generator strings do not determine semantics.

The required header is `extras.eqoxideAsset` with schema version 1, visual role, profile `eqoxide-static-y-up-v1`, unit scale 1, a lowercase 64-digit bake revision and reader requirements. Reader 2 plus `static-visual-v1` is required; colored primitives must also declare `vertex-rgba-v1`. Integral numeric spellings such as `1` and `1.0` are equivalent; fractional versions and unsupported requirements fail. Resource counts, offsets and references use unsigned JSON integer tokens; the header's integral spelling equivalence does not repair fractional resource descriptors. Header identities do not replace the future package's digest and component-association validation.

Decoded positions and normals use server geometry axes: `(asset_x, asset_y, asset_z) -> (asset_x, -asset_z, asset_y)`. Instance matrices use the same conjugation; meshes stay shared and winding is preserved. World coordinates must remain finite in both asset and server arithmetic.

Accessors must be dense. Positions and normals use float components; primary UVs and RGBA additionally support normalized unsigned 8/16-bit components. Declared accessor min/max values must match the actual payload. PNG streams require valid complete chunks/CRCs and terminal IEND without trailing data; APNG is outside this static profile.

The baseline supports flat proper-positive-uniform affine instances, shared position/normal/primary-UV pools per mesh, indexed triangles, opaque/mask/blend, RGBA factors, explicit mask cutoff and double-sided choice. Metallic factor must be zero and roughness one. Unsupported hierarchy, skins, animations, morphs, additional attributes, sampler behavior, emissive/normal/occlusion maps or behavioral extras fail explicitly. Their generic capability contracts and runtime implementations are separate additions, usable by either original source family.

Only embedded BIN resources and PNG buffer views are read. External and data URIs cannot trigger additional file or network access. Buffer ranges, attribute counts/references and resource budgets are checked before decoding/collecting payload arrays. Dependency panics become contextual errors; the API returns a whole validated CPU scene or an error.

Default `DecodeLimits` bound GLB input to 128 MiB, JSON to 8 MiB, decoded attribute/index/color payloads to 128 MiB, retained PNG payloads to 128 MiB, aggregate decoded PNG validation bytes to 256 MiB, and aggregate accessor-validation payload work (including unused aliases) to 256 MiB. Instance world-bound evaluation is limited to 50 million mesh-vertex visits, checking both coordinate frames per visit. Individual PNGs have a 64 MiB encoded/decoder allocation limit and 8192-pixel dimension limit. Resource collections are capped at 100,000 entries. These budgets bound specific payloads and parsing input; they are not a strict total-process memory limit. Callers can provide explicit limits. No truncation or partial scene is returned on a budget failure.

This is CPU inspection support. Existing production loaders, shaders and online asset-reader advertisements remain unchanged. Current online clients still advertise reader 1. Reader 2 cannot be advertised until the renderer and runtime can honor its required operations. Successful decoding does not authorize package activation, collision, navigation or gameplay readiness; a visual mesh called `__collision__` remains visual.

Run the actual public-API probe with:

```sh
cargo run -p eqoxide-assets --example static_visual_probe -- INPUT.glb
```

For the native-free fixture in `crates/eqoxide-assets/tests/fixtures/static-visual-v1.glb`, the decoded server bounds are `[-3, -7, 2]` to `[16.44179, 13, 6.5]`, with one mesh shared by two instances. The probe reports inspection data without starting a game session. Source adapters, renderer integration, package association and independent landmark/gameplay acceptance remain subsequent gates.
