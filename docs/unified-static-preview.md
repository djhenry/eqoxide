# Interactive unified static visual preview

The preview uses the common bounded GLB decoder and shared material GPU inspector, with a dedicated window and loopback control API. It does not log in, read a character config, synchronize assets or create gameplay state. Ordinary game and testzone startup remain separate. Existing crash diagnostics still run; preview does not start a camp watchdog.

Run:

```sh
cargo run --release --bin eqoxide -- \
  --preview-glb crates/eqoxide-assets/tests/fixtures/static-visual-v1.glb \
  --api-port 8771
```

The input must use the normalized static visual contract documented in `docs/unified-static-reader.md`. Legacy zone GLBs and earlier source-specific staging previews are rejected. Source importers must normalize them through the common writer before this viewer can consume them.

`--preview-glb=INPUT.glb` is also supported. Preview rejects `--testzone`, `--config`, `--profile`, `--nav-debug` and `--agent-socket`. With no `--api-port`, it binds an ephemeral port on `127.0.0.1` and prints `API_PORT=N`. An explicit port must bind exactly or startup fails. A malformed, missing, unsupported or over-limit GLB fails before window/server startup; there is no fallback terrain.

The window frames validated server-world bounds. Left drag orbits around the target; scroll zooms. Geometry and camera coordinates are server X/Y/Z with Z up, using a right-handed view. Texture/factor/full vertex RGBA, actual mask cutoff, alpha mode and sidedness share the same implementation regardless of source family.

Legacy artifacts retain neutral unlit inspection. Artifacts declaring `pbr-metallic-roughness-factors-v1` use standard scalar metallic/roughness materials under the same immutable reference light as the offline GPU example; explicit `KHR_materials_unlit` materials remain unlit. Omitted modern scalars each default to `1`, while producer conversion defaults may explicitly choose rough nonmetal `0/1`. Lit materials support OPAQUE/MASK; lit BLEND is refused. Unlit materials retain the existing blend behavior. This is a directional GGX/Smith/Schlick response with approximate constant ambient fill, a roughness evaluation floor of `0.045`, exposure and per-fragment Reinhard display mapping. It provides neither cubemap image-based lighting/global illumination nor production lighting, fog or shadows. No metallic/roughness, normal, occlusion or emissive maps are supported. See `docs/unified-static-gpu.md` for reference light values, validation and resource bounds.

The material fields and unlit extension follow standard glTF authoring conventions. The viewer accepts the documented bounded artifact profile and header, not arbitrary Blender scenes; author edits still require normalization and package/component validation before gameplay use. There is no preview HTTP lighting control.

The small API provides:

| Operation | Meaning |
| --- | --- |
| `GET /v1/observe/debug` | Mode, source/header, bounds/stats, `rendering.profile` and `rendering.reference_light_applied`, renderer phase/error, camera state and capture proof. Connected, world-responsive, world-ready, collision and navigation remain false/unavailable. |
| `GET /v1/camera` | Active CPU pose, latest requested pose, last submitted pose/frame and bounded request outcomes. |
| `POST /v1/camera` | Queue a strict camera pose; returns HTTP 202 with `status: queued` and a prospective `camera_revision`. |
| `GET /v1/observe/frame` | Request a fresh actual GPU readback as PNG. Headers identify its exact frame and camera revision. |
| `POST /v1/lifecycle/exit` | Request local main-thread shutdown without camping or a background process exit. |

Requests must use `Host: 127.0.0.1:N` or `Host: localhost:N`, where `N` is the actual bound API port. Requests carrying any `Origin` header are refused. POST requests require `Content-Type: application/json`; send `{}` to request exit. These checks apply to the dedicated preview API, including unsupported routes.

Unsupported routes, including gameplay writes, return a structured `unsupported_in_preview` error without queueing a game command.

Camera JSON requires all three fields and rejects unknown keys:

```json
{"eye":[20,-25,15],"target":[6.5,3,4.25],"fov_degrees":50}
```

Coordinates must be finite and representable by the current view/clipping context. Eye and target must differ by at least 0.001 game units; the view direction cannot be parallel to up Z. Field of view ranges from 5 to 120 degrees. Invalid camera input preserves the existing usable scene. PBR GPU camera preparation also requires an affine invertible view and ordinary right-handed WebGPU perspective or orthographic projection; eye coordinates and view-projection coefficients are bounded to magnitude `1e12`. PBR instance-transform intermediate bounds use the same `1e12` domain. Unsupported preparation produces an explicit renderer error rather than a successful stale-camera draw.

HTTP 202 means queued, not applied or visible. The camera queue holds at most eight requests; outcomes retain the latest 32 identities. A queued request must become the active CPU pose within ten seconds or expires. `active` means installed CPU camera data and does not certify a GPU frame. Only a successful submission produces `submitted` and a frame identity; only completed readback/encoding produces capture proof. Newer API or mouse changes can supersede an unsubmitted request. Inspect the returned revision's outcome and submitted/captured identity rather than treating the queue acknowledgement as rendering success.

`phase: ready` means the renderer is initialized with a nonzero viewport and no known OS-reported occlusion; it does not certify visibility. Debug `occluded` is the last OS-observed value, or `null` before any observation. Some platforms never send occlusion events, so unknown state permits rendering and capture.

Capture is single-flight. Competing requests receive HTTP 409; unavailable/minimized/OS-reported occluded windows receive a non-image error. Capture dimensions are bounded to 2048 pixels per axis, and readback plus encoding has a ten-second deadline. Final image publication rechecks the current window availability, viewport and capture request identity. Current unavailability, dimensions that differ from the captured frame, or lost request ownership return a non-image error. Errors are structured JSON with `{"error":"code","message":"explanation"}`; timeout, map or PNG failures never return a successful empty image. A successful image has `Content-Type: image/png`, `Cache-Control: no-store`, `X-Preview-Frame-ID` and `X-Preview-Camera-Revision`. Mouse changes and API updates use one authoritative camera, and older callbacks cannot satisfy a newer capture.

The window uses the adapter's supported texture dimensions for surface/depth allocation, allowing displays larger than the separate 2048-pixel PNG capture limit. A capture exceeding that limit returns an explicit size error while the viewer keeps running.

The window requires an sRGB RGBA/BGRA surface with capture support; unsupported formats/usages fail explicitly. BGRA readback is converted to correctly ordered PNG channels. Input/geometry/PNG limits are those of the CPU decoder. GPU preparation separately caps 512 MiB aggregate payload, 64 MiB per float image, one million encoded draw commands and 20 million triangle instances. The checked triangle-instance count includes solid instanced work, not only transparent command expansion. These payload/work limits do not bound total process memory or driver overhead.

Float textures preserve 16-bit PNG sampling before final eight-bit output, at four times RGBA8 texture payload. Sampling uses base-level repeat/bilinear filtering without mipmaps or anisotropy. Primitive-center transparency sorting retains its intersection/cycle/within-primitive limitations. These are inspection constraints, not a production fidelity declaration.

Successful visual inspection does not enable online reader advertisements, publish an asset package, certify source conversion, or authorize collision/navigation/gameplay. Source adapters, component association and independent landmark/gameplay acceptance remain subsequent gates.
