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

    pub fn clip_for_action(&self, action: &str) -> Option<usize> {
        match action {
            "dead" => None,
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
            _ => self.clips.iter().position(|c| {
                let n = c.name.to_lowercase();
                (n.contains("walking") || n.contains("walk"))
                    && !n.contains("fast") && !n.contains("backward")
                    && !n.contains("formal") && !n.contains("crouch")
            }),
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
            ground_probes: vec![],
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
            ground_probes: vec![],
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
            ground_probes: vec![],
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

    #[test]
    fn clip_for_action_known_actions() {
        let skin = action_skin();
        assert_eq!(skin.clip_for_action("idle"),         Some(0), "idle → Spider_Idle");
        assert_eq!(skin.clip_for_action("standing"),     Some(0), "standing → Spider_Idle");
        assert_eq!(skin.clip_for_action("wait"),         Some(0), "wait → Spider_Idle");
        assert_eq!(skin.clip_for_action("walking"),      Some(1), "walking → Spider_Walking");
        assert_eq!(skin.clip_for_action(""),             Some(1), "'' → Spider_Walking (default)");
        assert_eq!(skin.clip_for_action("running"),      Some(2), "running → Spider_Running");
        assert_eq!(skin.clip_for_action("dead"),         None,    "dead → None (bind pose)");
        assert_eq!(skin.clip_for_action("attack"),       Some(1), "unknown → Spider_Walking");
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
            ground_probes: vec![],
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
            ground_probes: vec![],
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
}
