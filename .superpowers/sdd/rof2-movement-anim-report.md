# Movement & Animation Bug Fix Report

Commit: `d564bdf`
Build: `cargo build --release` — clean, 49s
Tests: `cargo test` — 264 passed, 0 failed, 18 ignored

---

## Bug 1 — NPCs slide without walk animation

**Root cause:** `scene.rs::from_game_state` maps `e.animation` to action strings. The server
always sends `animation=100` (Standing) while an NPC is moving between position updates, so
every entity gets `action="idle"` regardless of motion. There was no position-change detection.

**Fix:** After the per-entity motion-smoothing loop in `app.rs` (~line 850), override
`b.action = "walking"` when `EntityMotion.speed > 0.5` (entity is actively moving) and
`d > 1e-4` (display hasn't yet reached the server target). Only overrides `"idle"` — preserves
`"dead"`, `"C0N"` combat, `"sitting"`, `"crouching"`. `clip_for_action("walking")` already
resolves to the L01 walk clip. The player walking path in `app.rs` was already correct.

**Files:** `src/app.rs` (entity motion loop, ~line 851)

---

## Bug 2 — Dead entities stand upright (bind pose)

**Root cause:**
- `anim.rs::clip_for_action("dead")` returned `None` unconditionally.
- `renderer.rs` skipped animation for `action == "dead"` (`if state.animate && *action != "dead"`).
- `pass.rs` rendered dead entities with `model.skin.bind_pose()` — standing pose.

**Fix:**
- `anim.rs`: `clip_for_action("dead")` now searches for the D05-family death clip
  (`name.contains("death") || name.starts_with("d05")`); returns `None` only when absent.
- `renderer.rs`: when action changes to `"dead"`, use `usize::MAX` as a sentinel when no death
  clip exists (`animate=false`). When a clip is found, set `animate=true` and play once: time
  advances via `next.min(dur)` (hold at last frame when done, `animate=false`).
- `pass.rs` (`encode_skinned_entity_pass`): removed `if b.action == "dead" { bind_pose() }`.
  Now: `if state.clip_idx < model.skin.clips.len() { evaluate(clip_idx, time) } else { bind_pose() }`.
  The sentinel `usize::MAX` is always out-of-range → fallback to bind_pose when no death clip.
- `app.rs`: player death detected via `cur_hp <= 0 && max_hp > 0`; `player_action = "dead"` takes
  priority over combat/walking/idle.

**Files:** `src/anim.rs:214–219`, `src/renderer.rs:885–946`, `src/pass.rs:829–837`, `src/app.rs:~880`

---

## Bug 3 — Player /goto shows stutter (lerp jumping)

**Root cause:** Visual player position used exponential easing
`alpha = 1 - exp(-15 * dt)`. At 60 fps (~0.016s dt) alpha≈0.22; after 150ms (9 frames) only
~88% of a 6.6-unit nav step was covered. Each new step left a residual, causing visible hops.

**Fix:** Added `player_nav_speed: f32` (init 44.0 = RUN_SPEED) and `last_player_nav_update:
Instant` to `App`. In the walking-detection block, when `prev_logical_pos` changes by > 0.01 u,
measure: `player_nav_speed = nav_dist / elapsed.clamp(50ms, 500ms)`. In the visual pos update
block, replaced `alpha = 1 - exp(-15*dt)` with speed-based glide:
`move_d = (player_nav_speed * dt).min(xy_dist)`, clamped (no overshoot). At ~44 u/s the visual
position exactly keeps pace with nav steps — continuous motion, no residual.

**Files:** `src/app.rs` (struct fields ~182, init ~299, walking block ~870, vis-pos block ~1126)

---

## Bug 4 — Player doesn't face walk direction during /goto

**Root cause:** Heading was derived from visual position delta (`de, dn = scene.player_pos -
prev_render_pos`). The motion-vector approach is fragile at nav corners and during the first
frame of each new step (before the visual glide catches up to the logical position).

**Fix:** Added a direct nav heading feed after the motion-vector block in the heading derivation
section (~`app.rs:1242`): when `!manual_move && last_moved_at.elapsed() < 300ms`, set
`heading_target = game_state.player_heading`. The nav thread already sets `gs.player_heading =
eq_heading(mdx, mdy)` (direction of the movement step) at each nav tick. `visual_heading` then
lerps toward `heading_target` at the existing rate, so the character smoothly faces forward.

**Files:** `src/app.rs` (~line 1242)

---

## Tests added (anim.rs)

- `clip_for_action_dead_resolves_to_death_clip` — D05A_death clip found by `clip_for_action("dead")`
- `clip_for_action_dead_fallback_when_no_death_clip` — model without death clip returns None
- `action_animates_returns_true_for_dead_with_death_clip` — animate=true when clip found
- Updated `clip_for_action_known_actions` comment: `action_skin` has no death clip → None still valid

## Uncertain / caveats

- **No death clips in converted GLBs yet**: whether the EQ S3D→GLB converter emits "D05A_death"
  clips depends on the converter. If no death clip exists for a model, fallback to bind pose
  (standing corpse) is preserved — same as before the fix. The death animation code is correct
  and ready; it will activate automatically once D05 clips are exported by `s3d_to_gltf`.
- **player_nav_speed at manual WASD**: speed is only updated when `prev_logical_pos` changes
  (nav-driven). During WASD keyboard movement, `override_pos` is active so visual pos is set
  directly (`self.visual_player_pos = op`) — bypasses the speed-based glide entirely. No issue.
- **nav heading at first /goto step**: `gs.player_heading` is 0 at spawn until the first nav
  tick fires. The 300ms window means heading_target briefly stays at the post-spawn heading,
  then snaps to nav direction. Cosmetically fine (the lerp absorbs it).
