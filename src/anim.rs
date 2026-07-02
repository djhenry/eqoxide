//! Skeletal animation data + sampling for character models: joint channels, clips, skin data, and
//! evaluation of a clip at a time `t` into the per-joint skinning matrices the skinned shader uses.
//! Also holds `GroundProbe`s used to center/ground a posed model (see `models.rs`).

pub enum JointProperty { Translation, Rotation, Scale }

pub struct JointChannel {
    pub joint:    usize,
    pub property: JointProperty,
    pub times:    Vec<f32>,
    pub values:   Vec<[f32; 4]>,  // vec3 zero-padded for T/S; xyzw for R
}

pub struct AnimClip {
    pub name:     String,
    pub duration: f32,
    pub channels: Vec<JointChannel>,
}

/// One vertex used to probe the model's lowest point in render (skinned) space.
/// Holds the raw mesh position plus its joint bindings so it can be re-skinned for
/// any pose. We keep only the vertices that sit lowest at bind (the feet), which
/// stay the lowest point through idle/walk poses.
#[derive(Clone)]
pub struct GroundProbe {
    pub pos:     [f32; 3],
    pub joints:  [u32; 4],
    pub weights: [f32; 4],
}

pub struct SkinData {
    pub joint_count: usize,
    pub parents:     Vec<Option<usize>>,
    pub inv_bind:    Vec<[[f32; 4]; 4]>,  // column-major, parallel to joints
    pub clips:       Vec<AnimClip>,
    /// Rest pose per joint (from node local transforms at bind time). Used as
    /// the initial value for joints whose animation clip has no channel — standard
    /// glTF exporters omit channels for stationary joints as an optimization.
    pub rest_translations: Vec<[f32; 3]>,
    pub rest_rotations:    Vec<[f32; 4]>,  // xyzw quaternion
    pub rest_scales:       Vec<[f32; 3]>,
    /// Lowest-at-bind vertices, in render (skinned) space, used to ground the model.
    pub ground_probes:     Vec<GroundProbe>,
    /// glTF node name per joint (uppercased), e.g. "HUFR_POINT". Empty for GLBs baked
    /// before joint names were exported. Used to locate weapon attachment bones.
    pub joint_names:       Vec<String>,
}

impl SkinData {
    /// Build joint global (model-space) transforms from per-joint local TRS arrays.
    fn build_globals(&self, translations: &[glam::Vec3], rotations: &[glam::Quat],
                     scales: &[glam::Vec3]) -> Vec<glam::Mat4> {
        use glam::Mat4;
        let mut global = vec![Mat4::IDENTITY; self.joint_count];
        for j in 0..self.joint_count {
            let local = Mat4::from_scale_rotation_translation(scales[j], rotations[j], translations[j]);
            global[j] = match self.parents[j] {
                Some(p) => global[p] * local,
                None    => local,
            };
        }
        global
    }

    /// Per-joint local TRS initialized from the rest pose (clips that omit a channel
    /// keep the bind-time transform).
    fn rest_trs(&self) -> (Vec<glam::Vec3>, Vec<glam::Quat>, Vec<glam::Vec3>) {
        use glam::{Quat, Vec3};
        (
            self.rest_translations.iter().map(|t| Vec3::from_slice(t)).collect(),
            self.rest_rotations.iter().map(|r| Quat::from_array(*r)).collect(),
            self.rest_scales.iter().map(|s| Vec3::from_slice(s)).collect(),
        )
    }

    /// Joint globals at `clip_idx`/`time`, posing on top of the rest pose.
    /// Shared by `evaluate` (skinning) and `lowest_joint_z` (grounding).
    fn joint_globals(&self, clip_idx: usize, time: f32) -> Vec<glam::Mat4> {
        use glam::{Quat, Vec3};

        let clip = &self.clips[clip_idx];
        let time = if clip.duration > 0.0 && time > clip.duration {
            time % clip.duration
        } else {
            time.max(0.0)
        };

        let (mut translations, mut rotations, mut scales) = self.rest_trs();

        for ch in &clip.channels {
            let (i0, i1, alpha) = find_keyframe(&ch.times, time);
            match ch.property {
                JointProperty::Translation => {
                    let a = Vec3::from_slice(&ch.values[i0][..3]);
                    let b = Vec3::from_slice(&ch.values[i1][..3]);
                    translations[ch.joint] = a.lerp(b, alpha);
                }
                JointProperty::Rotation => {
                    let a = Quat::from_array(ch.values[i0]);
                    let b = Quat::from_array(ch.values[i1]);
                    rotations[ch.joint] = a.slerp(b, alpha);
                }
                JointProperty::Scale => {
                    let a = Vec3::from_slice(&ch.values[i0][..3]);
                    let b = Vec3::from_slice(&ch.values[i1][..3]);
                    scales[ch.joint] = a.lerp(b, alpha);
                }
            }
        }

        self.build_globals(&translations, &rotations, &scales)
    }

    pub fn evaluate(&self, clip_idx: usize, time: f32) -> Vec<[[f32; 4]; 4]> {
        use glam::Mat4;
        let global = self.joint_globals(clip_idx, time);
        (0..self.joint_count)
            .map(|j| {
                let inv = Mat4::from_cols_array_2d(&self.inv_bind[j]);
                (global[j] * inv).to_cols_array_2d()
            })
            .collect()
    }

    /// World (model-space) transform of one joint at (clip, time) — for attaching a held item
    /// (weapon) to a hand bone. Unlike `evaluate`, this is the raw global pose WITHOUT inv_bind,
    /// so a model placed at the bone's transform follows the swing. Identity if `joint` is invalid.
    pub fn joint_world(&self, clip_idx: usize, time: f32, joint: usize) -> [[f32; 4]; 4] {
        self.joint_globals(clip_idx, time)
            .get(joint).copied().unwrap_or(glam::Mat4::IDENTITY)
            .to_cols_array_2d()
    }

    /// Find the attachment joint whose bone name ends with `suffix` (e.g. "R_POINT" matches
    /// "HUFR_POINT"). EQ rigs carry dedicated attachment bones the real client snaps held
    /// items to: R_POINT (primary hand), L_POINT (left hand), SHIELD_POINT (shield).
    /// Several other bones share the suffix (e.g. "HUFGAUNTR_POINT"); the hand point is
    /// always `{race}{suffix}`, so the shortest matching name wins.
    /// Returns None on GLBs baked before joint names were exported.
    pub fn attach_joint(&self, suffix: &str) -> Option<usize> {
        self.joint_names.iter().enumerate()
            .filter(|(_, n)| n.ends_with(suffix))
            .min_by_key(|(_, n)| n.len())
            .map(|(i, _)| i)
    }

    /// Bind-pose world position of each joint (translation of the inverse of inv_bind). Used to
    /// locate attach bones (e.g. the right hand = an arm-chain extremity) when joint names are absent.
    pub fn bind_joint_positions(&self) -> Vec<[f32; 3]> {
        self.inv_bind.iter().map(|ib| {
            let w = glam::Mat4::from_cols_array_2d(ib).inverse();
            let t = w.w_axis;
            [t.x, t.y, t.z]
        }).collect()
    }

    /// Skin a raw vertex with the given skin matrices, returning its full render-space
    /// position. This mirrors exactly what the vertex shader computes.
    pub fn skin_point(pos: [f32; 3], joints: [u32; 4], weights: [f32; 4],
                      skin: &[glam::Mat4]) -> [f32; 3] {
        use glam::Vec4;
        let p = Vec4::new(pos[0], pos[1], pos[2], 1.0);
        let mut acc = Vec4::ZERO;
        for i in 0..4 {
            let w = weights[i];
            if w == 0.0 { continue; }
            if let Some(m) = skin.get(joints[i] as usize) {
                acc += w * (*m * p);
            }
        }
        [acc.x, acc.y, acc.z]
    }

    /// Skin a probe vertex with the given skin matrices and return its render-space Z.
    pub fn probe_z(probe: &GroundProbe, skin: &[glam::Mat4]) -> f32 {
        // Models are Y-up (height = Y), so the "lowest" point used for grounding is
        // the minimum Y of the posed skin, matching the static path's y_bottom.
        Self::skin_point(probe.pos, probe.joints, probe.weights, skin)[1]
    }

    /// Skin matrices (global * inv_bind) at the bind/rest pose. Used at load time to
    /// pick ground probes in render space.
    pub fn bind_skin_matrices(&self) -> Vec<glam::Mat4> {
        let (t, r, s) = self.rest_trs();
        let global = self.build_globals(&t, &r, &s);
        (0..self.joint_count)
            .map(|j| global[j] * glam::Mat4::from_cols_array_2d(&self.inv_bind[j]))
            .collect()
    }

    /// Lowest render-space Z over the ground probes at the given pose. This is the
    /// actual lowest point of the skinned mesh (feet), in the same space the model is
    /// rendered, so lifting by its negation grounds the model correctly regardless of
    /// how the rig reorients the raw mesh.
    pub fn lowest_skinned_z(&self, clip_idx: usize, time: f32) -> f32 {
        if self.ground_probes.is_empty() { return 0.0; }
        let global = self.joint_globals(clip_idx, time);
        let skin: Vec<glam::Mat4> = (0..self.joint_count)
            .map(|j| global[j] * glam::Mat4::from_cols_array_2d(&self.inv_bind[j]))
            .collect();
        self.ground_probes.iter()
            .map(|p| Self::probe_z(p, &skin))
            .fold(f32::MAX, f32::min)
    }

    /// Lowest render-space Z over the ground probes at the bind pose.
    pub fn bind_lowest_skinned_z(&self) -> f32 {
        if self.ground_probes.is_empty() { return 0.0; }
        let skin = self.bind_skin_matrices();
        self.ground_probes.iter()
            .map(|p| Self::probe_z(p, &skin))
            .fold(f32::MAX, f32::min)
    }

    /// Whether the clip chosen for `action` should advance through time.
    /// An idle-family action that resolved to a non-idle clip (a walk fallback,
    /// used for models like the Skeleton that lack a usable idle) should be held
    /// at a static frame so the character stands still rather than walking in place.
    pub fn action_animates(&self, action: &str, clip_idx: usize) -> bool {
        let is_idle = matches!(action, "idle" | "standing" | "wait");
        if !is_idle { return true; }
        match self.clips.get(clip_idx) {
            Some(c) => c.name.to_lowercase().contains("idle"),
            None => true,
        }
    }

    /// Held poses (dead, sitting, crouching/kneeling) play their entry transition ONCE and then
    /// HOLD the final frame — like the native client — instead of looping the stand→pose
    /// transition endlessly (eqoxide#83). The final frame of the transition clip is the resting pose.
    pub fn is_held_pose(action: &str) -> bool {
        matches!(action, "dead" | "sitting" | "crouching")
    }

    pub fn clip_for_action(&self, action: &str) -> Option<usize> {
        match action {
            // Death: find the D05-family death clip (name contains "death" or starts with "d05").
            // Returns None when no such clip exists so the caller can fall back to bind pose.
            "dead" => self.clips.iter().position(|c| {
                let n = c.name.to_lowercase();
                n.contains("death") || n.starts_with("d05")
            }),
            "running" => self.clips.iter().position(|c| {
                let n = c.name.to_lowercase();
                (n.contains("run") || n.contains("running"))
                    && !n.contains("back") && !n.contains("left") && !n.contains("right")
                    && !n.contains("shoot")
            }),
            "sitting" => self.clips.iter().position(|c| {
                let n = c.name.to_lowercase();
                n.contains("sit") && !n.contains("swim")
            }),
            "crouching" => self.clips.iter().position(|c| {
                let n = c.name.to_lowercase();
                n.contains("crouch")
            }),
            // Idle/standing MUST be checked BEFORE the walking fallback — otherwise the
            // walk-first logic in the _ branch hijacks these actions.
            "idle" | "standing" | "wait" => {
                // Prefer a "neutral" idle (relaxed standing) over combat-ready variants.
                let neutral = self.clips.iter().position(|c| {
                    let n = c.name.to_lowercase();
                    n.contains("idle") && n.contains("neutral")
                });
                // A low joint-coverage idle is fine: the unanimated joints simply hold
                // their bind pose. (An earlier coverage filter rejected the Skeleton's
                // real Idle, but the "crouch" it produced was actually the grounding bug.)
                let any_idle = || self.clips.iter().position(|c| {
                    let n = c.name.to_lowercase();
                    n.contains("idle") && !n.contains("gun") && !n.contains("sword")
                        && !n.contains("crouch") && !n.contains("sitting")
                        && !n.contains("swim") && !n.contains("pistol") && !n.contains("torch")
                });
                // Final fallback: use walk when no suitable idle exists.
                let walk_fallback = || self.clips.iter().position(|c| {
                    let n = c.name.to_lowercase();
                    (n.contains("walking") || n.contains("walk"))
                        && !n.contains("fast") && !n.contains("backward")
                        && !n.contains("formal") && !n.contains("crouch")
                });
                neutral.or_else(any_idle).or_else(walk_fallback)
            }
            // Combat swing codes like "C05" (OP_Animation action) → the matching C0N combat clip.
            // Clip names are e.g. "C05B_combat"; prefer the full-body "B" variant.
            a if a.len() == 3 && a.as_bytes()[0] == b'C' && a.as_bytes()[1] == b'0'
                && a.as_bytes()[2].is_ascii_digit() => {
                let pre = a.to_uppercase();
                self.clips.iter().position(|c| {
                    let n = c.name.to_uppercase();
                    n.starts_with(&pre) && n[3..].starts_with('B')
                }).or_else(|| self.clips.iter().position(|c| c.name.to_uppercase().starts_with(&pre)))
            }
            _ => self.clips.iter().position(|c| {
                let n = c.name.to_lowercase();
                (n.contains("walking") || n.contains("walk"))
                    && !n.contains("fast") && !n.contains("backward")
                    && !n.contains("formal") && !n.contains("crouch")
            }),
        }
    }

    /// Lively idle "fidget" animations (look around, shift weight, etc.) that the native client
    /// plays periodically over the near-static neutral stand. Excludes the neutral idle, held
    /// poses, and held-item variants. Prefers the full-body "A" variant over the upper-body "B".
    pub fn idle_fidget_clips(&self) -> Vec<usize> {
        let base = self.clip_for_action("idle"); // the neutral stand — not itself a fidget
        self.clips.iter().enumerate().filter_map(|(i, c)| {
            if Some(i) == base { return None; }
            let n = c.name.to_lowercase();
            let is_idle = n.contains("idle")
                && !n.contains("neutral")  // the near-static base stand handled by clip_for_action
                && !n.contains("swim") && !n.contains("crouch") && !n.contains("sitting")
                && !n.contains("gun") && !n.contains("sword") && !n.contains("pistol")
                && !n.contains("torch");
            // Skip the upper-body-only "B" duplicate (code is 3 chars, then 'A'/'B').
            let is_a = !matches!(n.as_bytes().get(3), Some(&b'b'));
            (is_idle && is_a).then_some(i)
        }).collect()
    }

    /// The idle clip to show for a given cycle `phase` (incremented each time the current idle clip
    /// finishes a loop). Mostly the neutral stand; every 3rd phase a fidget, rotating through the
    /// fidget set — so a character stands quietly with an occasional idle animation, like native.
    /// Falls back to the plain idle clip when the model has no fidgets.
    pub fn idle_clip_for_phase(&self, phase: u32) -> Option<usize> {
        let neutral = self.clip_for_action("idle");
        let fidgets = self.idle_fidget_clips();
        if fidgets.is_empty() {
            return neutral;
        }
        if phase % 3 == 2 {
            Some(fidgets[(phase as usize / 3) % fidgets.len()])
        } else {
            neutral
        }
    }

    pub fn bind_pose(&self) -> Vec<[[f32; 4]; 4]> {
        // Proper rest-pose skinning matrices (global_rest * inv_bind), NOT identity.
        // Identity only reproduces the rest pose for models whose raw mesh is already
        // posed; EQ-converted meshes are authored off-pose, so identity renders the raw
        // un-posed mesh (off-center). Use the same matrices the bounds are measured from.
        self.bind_skin_matrices()
            .iter()
            .map(|m| m.to_cols_array_2d())
            .collect()
    }
}

fn find_keyframe(times: &[f32], t: f32) -> (usize, usize, f32) {
    if times.len() == 1 || t <= times[0] {
        return (0, 0, 0.0);
    }
    let last = times.len() - 1;
    if t >= times[last] {
        return (last, last, 0.0);
    }
    let i = times.partition_point(|&k| k <= t).saturating_sub(1);
    let alpha = ((t - times[i]) / (times[i + 1] - times[i])).clamp(0.0, 1.0);
    (i, i + 1, alpha)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_mat() -> [[f32; 4]; 4] {
        [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ]
    }

    fn default_rest(n: usize) -> (Vec<[f32;3]>, Vec<[f32;4]>, Vec<[f32;3]>) {
        (vec![[0.0;3]; n], vec![[0.0,0.0,0.0,1.0]; n], vec![[1.0;3]; n])
    }

    #[test]
    fn attach_joint_prefers_hand_point_over_longer_suffix_matches() {
        let mut skin = single_translation_skin();
        // Rig order deliberately puts the decoy (gauntlet mount) before the hand point.
        skin.joint_names = vec![
            "HUFPEBIP01".into(),
            "HUFGAUNTR_POINT".into(),
            "HUFR_POINT".into(),
        ];
        skin.joint_count = 3;
        assert_eq!(skin.attach_joint("R_POINT"), Some(2), "hand point, not gauntlet mount");
        assert_eq!(skin.attach_joint("L_POINT"), None);
        // Unnamed joints (pre-name GLBs) find nothing.
        skin.joint_names = vec![String::new(); 3];
        assert_eq!(skin.attach_joint("R_POINT"), None);
    }

    fn single_translation_skin() -> SkinData {
        let (rest_translations, rest_rotations, rest_scales) = default_rest(1);
        SkinData {
            joint_count: 1,
            parents: vec![None],
            inv_bind: vec![identity_mat()],
            clips: vec![AnimClip {
                name: "test".to_string(),
                duration: 1.0,
                channels: vec![JointChannel {
                    joint: 0,
                    property: JointProperty::Translation,
                    times: vec![0.0, 1.0],
                    values: vec![[1.0, 0.0, 0.0, 0.0], [3.0, 0.0, 0.0, 0.0]],
                }],
            }],
            rest_translations, rest_rotations, rest_scales,
            ground_probes: vec![], joint_names: vec![],
        }
    }

    fn make_channel(joint: usize) -> JointChannel {
        JointChannel {
            joint,
            property: JointProperty::Rotation,
            times: vec![0.0, 1.0],
            values: vec![[0.0, 0.0, 0.0, 1.0]; 2],
        }
    }

    fn action_skin() -> SkinData {
        // 3 joints so coverage check (unique >= joint_count/3) is satisfied by 1 channel.
        let (rest_translations, rest_rotations, rest_scales) = default_rest(3);
        SkinData {
            joint_count: 3,
            parents: vec![None, Some(0), Some(1)],
            inv_bind: vec![identity_mat(); 3],
            clips: vec![
                // Each clip animates joint 0 so coverage (1/3) meets the >= 1/3 threshold.
                AnimClip { name: "Spider_Idle".to_string(),             duration: 2.0, channels: vec![make_channel(0)] },
                AnimClip { name: "Spider_Walking".to_string(),          duration: 1.0, channels: vec![make_channel(0)] },
                AnimClip { name: "Spider_Running".to_string(),          duration: 0.5, channels: vec![make_channel(0)] },
                AnimClip { name: "Spider_Walking_Backward".to_string(), duration: 1.0, channels: vec![make_channel(0)] },
                AnimClip { name: "Spider_Walking_Fast".to_string(),     duration: 0.7, channels: vec![make_channel(0)] },
            ],
            rest_translations, rest_rotations, rest_scales,
            ground_probes: vec![], joint_names: vec![],
        }
    }

    #[test]
    fn bind_pose_returns_rest_skinning_not_identity() {
        // Rest pose translates the joint while inv_bind is identity, so the rest skinning
        // matrix is that translation — NOT identity. bind_pose() must reflect this; it used
        // to return identity, which rendered the raw, un-posed (off-center) mesh.
        let skin = SkinData {
            joint_count: 1,
            parents: vec![None],
            inv_bind: vec![identity_mat()],
            clips: vec![],
            rest_translations: vec![[5.0, 0.0, 0.0]],
            rest_rotations: vec![[0.0, 0.0, 0.0, 1.0]],
            rest_scales: vec![[1.0; 3]],
            ground_probes: vec![], joint_names: vec![],
        };
        let bp = skin.bind_pose();
        let bsm = skin.bind_skin_matrices();
        assert_eq!(bp.len(), 1);
        let m = glam::Mat4::from_cols_array_2d(&bp[0]);
        assert!((m.w_axis.x - 5.0).abs() < 1e-4,
            "bind_pose must carry the rest translation (5,0,0), not identity; got {:?}", m.w_axis);
        // bind_pose must equal the real rest skinning matrices
        let (a, b) = (m.to_cols_array(), bsm[0].to_cols_array());
        assert!(a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 1e-5),
            "bind_pose must equal bind_skin_matrices");
    }

    #[test]
    fn evaluate_single_joint_at_keyframe_times() {
        let skin = single_translation_skin();
        let r0 = skin.evaluate(0, 0.0);
        let r1 = skin.evaluate(0, 1.0);
        // Column-major mat4: column 3 is [tx, ty, tz, 1].
        // At t=0: local = T([1,0,0]), inv_bind = identity → result = T([1,0,0])
        assert!((r0[0][3][0] - 1.0).abs() < 1e-5, "t=0 tx should be 1.0, got {}", r0[0][3][0]);
        assert!((r1[0][3][0] - 3.0).abs() < 1e-5, "t=1 tx should be 3.0, got {}", r1[0][3][0]);
    }

    #[test]
    fn evaluate_interpolates_midpoint() {
        let skin = single_translation_skin();
        let r = skin.evaluate(0, 0.5);
        assert!((r[0][3][0] - 2.0).abs() < 1e-5, "t=0.5 tx should be 2.0, got {}", r[0][3][0]);
    }

    #[test]
    fn evaluate_wraps_at_duration() {
        let skin = single_translation_skin();
        let r0   = skin.evaluate(0, 0.0);
        let r2   = skin.evaluate(0, 2.0); // 2.0 % 1.0 = 0.0
        assert!((r0[0][3][0] - r2[0][3][0]).abs() < 1e-5, "t=2.0 should equal t=0.0");
    }

    fn death_skin() -> SkinData {
        // EQ D05-family death clip plus idle and walk, mirroring a typical converted GLB.
        let names = ["D05A_death", "P01A_idle_neutral", "L01A_walk"];
        let (rest_translations, rest_rotations, rest_scales) = default_rest(3);
        SkinData {
            joint_count: 3,
            parents: vec![None, Some(0), Some(1)],
            inv_bind: vec![identity_mat(); 3],
            clips: names.iter().map(|n| AnimClip {
                name: n.to_string(), duration: 2.0, channels: vec![make_channel(0)],
            }).collect(),
            rest_translations, rest_rotations, rest_scales,
            ground_probes: vec![], joint_names: vec![],
        }
    }

    #[test]
    fn clip_for_action_known_actions() {
        let skin = action_skin();
        assert_eq!(skin.clip_for_action("idle"),         Some(0), "idle → Spider_Idle");
        assert_eq!(skin.clip_for_action("standing"),     Some(0), "standing → Spider_Idle");
        assert_eq!(skin.clip_for_action("wait"),         Some(0), "wait → Spider_Idle");
        assert_eq!(skin.clip_for_action("walking"),      Some(1), "walking → Spider_Walking");
        assert_eq!(skin.clip_for_action(""),             Some(1), "'' → Spider_Walking (default)");
        assert_eq!(skin.clip_for_action("running"),      Some(2), "running → Spider_Running");
        // action_skin has no death clip → still returns None (bind-pose fallback).
        assert_eq!(skin.clip_for_action("dead"),         None,    "no death clip → None");
        assert_eq!(skin.clip_for_action("attack"),       Some(1), "unknown → Spider_Walking");
    }

    #[test]
    fn clip_for_action_dead_resolves_to_death_clip() {
        let skin = death_skin();
        // D05A_death is clip 0; "dead" must find it.
        assert_eq!(skin.clip_for_action("dead"), Some(0), "dead → D05A_death clip (index 0)");
    }

    #[test]
    fn held_poses_are_classified_for_play_once_hold() {
        // eqoxide#83: dead/sitting/crouching hold their final frame; motions loop.
        for a in ["dead", "sitting", "crouching"] {
            assert!(SkinData::is_held_pose(a), "{a} should be a held pose");
        }
        for a in ["idle", "standing", "walking", "running", "wait"] {
            assert!(!SkinData::is_held_pose(a), "{a} must NOT be a held pose (it loops/animates)");
        }
    }

    #[test]
    fn clip_for_action_dead_fallback_when_no_death_clip() {
        let skin = action_skin(); // Spider_{Idle,Walking,Running,...} — no death clip
        assert_eq!(skin.clip_for_action("dead"), None,
            "model with no death clip should return None so caller falls back to bind pose");
    }

    #[test]
    fn action_animates_returns_true_for_dead_with_death_clip() {
        let skin = death_skin();
        let ci = skin.clip_for_action("dead").unwrap();
        assert!(skin.action_animates("dead", ci),
            "dead action with a real death clip should animate (play-once)");
    }

    #[test]
    fn low_coverage_idle_is_used_not_rejected() {
        // A low joint-coverage idle (only a few joints animate, rest hold bind pose) is a
        // valid idle — e.g. the Skeleton's real Idle covers ~11/58 joints. It must be
        // selected, not discarded in favour of walk.
        let (rest_translations, rest_rotations, rest_scales) = default_rest(9);
        let skin = SkinData {
            joint_count: 9,
            parents: vec![None; 9],
            inv_bind: vec![identity_mat(); 9],
            clips: vec![
                // Idle: only 1/9 joints animated — still a real idle clip.
                AnimClip { name: "Idle".to_string(), duration: 1.0,
                           channels: vec![make_channel(0)] },
                AnimClip { name: "Walk".to_string(), duration: 1.0,
                           channels: vec![make_channel(0), make_channel(3), make_channel(6)] },
            ],
            rest_translations, rest_rotations, rest_scales,
            ground_probes: vec![], joint_names: vec![],
        };
        assert_eq!(skin.clip_for_action("idle"), Some(0),
            "a real idle clip should be used regardless of joint coverage");
        // And a real idle animates (loops), not frozen.
        assert!(skin.action_animates("idle", 0), "a real idle clip should animate");
    }

    #[test]
    fn idle_falls_back_to_walk_when_no_idle_exists() {
        // A model with no idle clip at all should fall back to walk, held static.
        let (rest_translations, rest_rotations, rest_scales) = default_rest(3);
        let skin = SkinData {
            joint_count: 3,
            parents: vec![None, Some(0), Some(1)],
            inv_bind: vec![identity_mat(); 3],
            clips: vec![
                AnimClip { name: "Walk".to_string(), duration: 1.0, channels: vec![make_channel(0)] },
                AnimClip { name: "Run".to_string(),  duration: 1.0, channels: vec![make_channel(0)] },
            ],
            rest_translations, rest_rotations, rest_scales,
            ground_probes: vec![], joint_names: vec![],
        };
        assert_eq!(skin.clip_for_action("idle"), Some(0),
            "no idle clip → fall back to walk (index 0)");
        assert!(!skin.action_animates("idle", 0),
            "a walk used as idle fallback must be held static");
    }

    #[test]
    fn real_idle_clip_animates() {
        let skin = action_skin(); // clip 0 is "Spider_Idle"
        assert!(skin.action_animates("idle", 0), "a real idle clip should animate");
        assert!(skin.action_animates("walking", 1), "walking should animate");
    }

    /// A humanoid-style skin with the EQ idle clip naming: a neutral stand plus O-series fidgets
    /// (A and B variants) and a walk, mirroring `race_elf.glb`.
    fn humanoid_idle_skin() -> SkinData {
        let names = [
            "P01A_idle_neutral", "O01A_idle", "O01B_idle", "O02A_idle", "O02B_idle",
            "O03A_idle", "O03B_idle", "L01A_walk",
        ];
        let (rest_translations, rest_rotations, rest_scales) = default_rest(3);
        SkinData {
            joint_count: 3,
            parents: vec![None, Some(0), Some(1)],
            inv_bind: vec![identity_mat(); 3],
            clips: names.iter().map(|n| AnimClip {
                name: n.to_string(), duration: 2.0, channels: vec![make_channel(0)],
            }).collect(),
            rest_translations, rest_rotations, rest_scales,
            ground_probes: vec![], joint_names: vec![],
        }
    }

    #[test]
    fn idle_fidget_clips_are_the_a_variant_o_series() {
        let skin = humanoid_idle_skin();
        // O01A_idle=1, O02A_idle=3, O03A_idle=5 — the A-variant fidgets, not neutral, not B, not walk.
        assert_eq!(skin.idle_fidget_clips(), vec![1, 3, 5]);
    }

    #[test]
    fn idle_phase_cycles_neutral_then_fidget() {
        let skin = humanoid_idle_skin();
        let neutral = skin.clip_for_action("idle"); // P01A_idle_neutral = 0
        assert_eq!(neutral, Some(0));
        // Two neutral phases, then a fidget, rotating through O01A/O02A/O03A.
        assert_eq!(skin.idle_clip_for_phase(0), Some(0), "phase 0 → neutral");
        assert_eq!(skin.idle_clip_for_phase(1), Some(0), "phase 1 → neutral");
        assert_eq!(skin.idle_clip_for_phase(2), Some(1), "phase 2 → fidget O01A");
        assert_eq!(skin.idle_clip_for_phase(3), Some(0), "phase 3 → neutral");
        assert_eq!(skin.idle_clip_for_phase(5), Some(3), "phase 5 → fidget O02A");
        assert_eq!(skin.idle_clip_for_phase(8), Some(5), "phase 8 → fidget O03A");
        assert_eq!(skin.idle_clip_for_phase(11), Some(1), "phase 11 → fidget wraps to O01A");
    }

    #[test]
    fn idle_phase_falls_back_to_neutral_without_fidgets() {
        let skin = action_skin(); // only Spider_Idle (no O-series fidgets)
        assert!(skin.idle_fidget_clips().is_empty());
        assert_eq!(skin.idle_clip_for_phase(2), skin.clip_for_action("idle"),
            "no fidgets → always the plain idle clip");
    }
}
