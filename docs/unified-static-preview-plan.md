# Unified static visual interactive preview implementation plan

> **For agentic workers:** Use the agent-fleet development and independent acceptance process. Steps use checkboxes for tracking.

**Goal:** Inspect the unified static visual artifact in a controllable window without entering gameplay startup or implying world readiness.

**Architecture:** A strict startup-mode parser dispatches preview before config/account and game initialization. The dedicated preview runner owns its winit/wgpu state and one camera, using the bounded decoder/common GPU inspector. A small loopback router communicates through bounded event-loop requests and reports exact submitted/captured identities; local exit tears down on the event thread.

**Tech stack:** Existing Rust, winit 0.30, wgpu 22, axum/tokio, serde, image and common static visual modules.

**Spec:** `docs/unified-baked-assets-design.md`, `docs/unified-static-reader.md`, `docs/unified-static-gpu.md`. Tracks #1150 and #1126; depends on PR #1149/#1146.

## Global constraints

- Preview is a mutually exclusive startup mode. `--preview-glb PATH` and `--preview-glb=PATH` accept one nonempty value; reject duplicates, missing values and unknown flags. Preview conflicts with testzone, config, profile, nav-debug and agent-socket. Preserve existing non-preview flag behavior.
- Dispatch before login config resolution, packet telemetry, asset sync, gameplay slots/routers, account/network threads or the ordinary App loop. Existing gameplay/testzone behavior and reader advertisements stay unchanged.
- Bound file input with metadata plus take(limit+1), then prepare the actual common static profile before window/server creation. Unsupported or malformed input returns nonzero; no fallback scene or successful empty response.
- Reuse GpuStaticVisual, direct server axes and its unlit material/sampling policy. Add RGBA/BGRA8 unorm/sRGB attachment support with actual color readback tests; interactive surfaces require sRGB. Check surface format/usages before requesting COPY_SRC. Refuse unsupported capture rather than invent a frame.
- Add checked `scene_triangle_instances` accounting and default `UploadLimits.max_triangle_instances = 20_000_000`, preflighted before float conversion/GPU resources. This bounds primitive-index/instance amplification independent of command count. Expose immutable prepared header for source identity; preserve private construction.
- Default API binds 127.0.0.1:0 and prints API_PORT. Explicit port binds exactly or fails startup. No account/config is needed. Author port 8771 and independent reviewer port 8772 are reserved only during their own launch; no shared character exists.
- API contains only inspect/debug, camera, PNG capture and local exit. Connected/world_responsive/world_ready/collision/navigation remain false/unavailable from startup. Unsupported/gameplay operations refuse without a gameplay queue.
- Camera JSON is strict, with finite eye/target and a finite supported fov_degrees; validate nondegenerate view/projection using the RH negative-view-Z convention and the actual clipping/viewport context before accepting a request. Invalid user camera input preserves the existing usable scene. One event-thread camera is authoritative for mouse/API changes; snapshots report requested/submitted state explicitly. No stale local/shared overwrite.
- A camera response may acknowledge queued work explicitly or wait for a named submission; never report an applied/drawn success before the relevant submission. Publish monotonic camera revision and submitted frame identity. The ten-second camera deadline bounds queue admission to active CPU state, which has its own explicit active outcome; it is not a submission deadline. Superseded/expired/failed requests have explicit outcomes; request bounds prevent overwriting another caller.
- Capture is single-flight, with explicit busy response and a 10-second deadline. Bound width/height to 2048, row-pitch and total bytes with checked arithmetic. Readback uses nonblocking polling/checked callback result with a deadline; PNG encoding returns Result. No unbounded Maintain::Wait followed by a misleading timeout.
- Captured PNG responses name the exact frame and camera revision in headers. Captures must be actual completed readbacks; default image responses never claim gameplay readiness. Minimized/zero-sized windows return unavailable capture, not stale pixels.
- Exit sets shutdown and wakes the event loop. No camp, background process::exit/watchdog or logout sleep. Stop HTTP runtime and drop GPU/window state normally before main records clean exit.
- One low-priority local build job at a time, no remote builds/uploads, debug/incremental disabled. Work only in this managed worktree, never stash or touch shared main. No merge/deployment without human review. Public docs/comments/bodies omit local/proprietary provenance.

## Review focus

- Invalid login/asset-server environment cannot affect preview; valid GLB draws with no config.
- API/mouse camera state and capture metadata refer to the actual submission/readback, including concurrent, superseded and timed-out requests.
- BGRA bytes must become correctly colored PNGs; zero size, oversized capture, missing map completion or encoder failure cannot return image success.
- Local exit promptly stops only its own process without camping or watchdog tasks; game writes remain refused.
- Small shared geometry with many repeated indices/instances must trip GPU workload budget before submission despite small payload/command counts.

## Task 1: Dedicated runner, renderer surface support and observable control

**Owner:** worker. New `src/preview/mod.rs` and focused supporting files/tests; renderer `src/static_visual.rs`/GPU tests for BGRA/work budget/header accessor only. ROOT owns CLI/main/lib export, plan and usage docs.

Public interface used by root:

```rust
pub struct PreviewOptions { pub api_port: Option<u16> }
pub fn run(path: std::path::PathBuf, options: PreviewOptions) -> anyhow::Result<()>;
```

Runner binds API, creates window/surface, uploads private prepared scene, uses finite auto-framing bounds, and drives orbit/zoom plus event-loop control. Handle init/render/resize/map failures explicitly. Capture must refer to an actually submitted image and checked map completion. Use stable source filename plus prepared header/bake identity in inspect data, without credentials or guessed readiness.

HTTP interface:

- `GET /v1/observe/debug`: preview mode, source/header/stats/bounds, phase, terminal error, submitted camera/frame and explicit unavailable network/world/collision/navigation.
- `GET /v1/camera`: authoritative camera/submission snapshot.
- `POST /v1/camera`: strict `{eye:[x,y,z],target:[x,y,z],fov_degrees:number}`. Return explicit queued identity/outcome or submission acknowledgement; document exact response once implemented.
- `GET /v1/observe/frame`: single-flight actual PNG with `X-Preview-Frame-ID` and `X-Preview-Camera-Revision`, `Cache-Control:no-store`. Busy/unavailable/timeout/map/encoding failures are non-200 structured errors.
- `POST /v1/lifecycle/exit`: local shutdown acknowledgement and main-thread wakeup.
- Unsupported routes: explicit unsupported-in-preview failure, no side effects.

- [ ] Add failing pure camera/state/router and upload-work-budget tests. Test no unsupported/gameplay side effects, immediate disconnected state, queue competition, deadlines, frame identity and error propagation.
- [ ] Implement runner and lower-level BGRA/work-accounting changes with private preparation/header access. Preserve existing normal renderer/tests; extend unsupported output test to use a still-unsupported format and add explicit BGRA GPU pixels.
- [ ] Build/tests on this worktree; explicitly run actual GPU tests. Launch its own preview with no config/usable asset server, verify status, camera change plus distinct captured PNGs, rejected gameplay writes and prompt API exit. Test missing/malformed input and occupied exact port before window startup.
- [ ] Demonstrate meaningful camera/capture/shutdown/budget mutations failing, restore sources/binaries and rerun. Commit owned code/tests only; give root exact endpoint/state schema and observed results.

## Task 2: Strict startup parser and user-facing documentation

**Owner:** ROOT. New `src/cli.rs`; modify `src/main.rs` parser/dispatch and `src/lib.rs` preview export only; new `docs/unified-static-preview.md`.

```rust
pub enum StartupMode { Gameplay, TestZone, Preview(std::path::PathBuf) }
pub struct CliArgs { pub mode: StartupMode, pub profile: bool, pub nav_debug: bool,
    pub config: Option<String>, pub api_port: Option<u16>, pub agent_socket: Option<std::path::PathBuf> }
pub enum ParsedCli { Help, Run(CliArgs) }
pub fn parse_args(args: &[String]) -> Result<ParsedCli, String>;
```

- [x] Write strict parsing tests for legacy flags and both preview forms, empty/missing/duplicate path, all conflicting game flags, api-port bounds, unknown flags and help. Run RED before implementation, then GREEN.
- [x] Preserve usage/error exit2 semantics. Main dispatches preview immediately after parsed args and records clean shutdown only after run returns; preview errors print context and exit1. Other startup modes keep their existing path.
- [x] Document actual command, camera schema/acknowledgement, ephemeral versus exact port, frame proof/error behavior, orbit/zoom and unavailable world semantics. Include workload/payload/capture limits and unlit/texture/sorting limitations; no readiness claims.
- [x] Self-validate live through the actual binary/GUI/API on the assigned port, inspect captured PNG and record honest metadata and shutdown timing. Scan public additions and commit owned files.

## Task 3: Independent acceptance and human review

**Owner:** separate acceptance reviewer and ROOT.

- [ ] Refute source/API contracts and public references. Independently run relevant crate/root tests and actual GPU tests; demonstrate targeted regressions are mutation-discriminating.
- [ ] Launch the actual built preview independently on port8772 with no account/config or reachable asset server. Check real submitted/captured identities, camera updates, competing capture requests, explicit unavailable gameplay and prompt clean local exit; inspect PNGs. Verify failure launches do not claim success.
- [ ] Fix confirmed findings and return them for reacceptance. Publish a PR stacked on #1149, ready for human review; no merges.

## Subsequent units

Normalize both source importers into the common writer, resolving material approximations explicitly. Then define collision/query semantics, volumes and component association and independently verify server landmarks/gameplay scope before enabling publication or online capabilities. Interactive visual preview does not satisfy those gates.
