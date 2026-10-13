use anyhow::Result;
use eqoxide_assets::static_visual::Bounds;
use glam::{Mat4, Vec3};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct Camera {
    pub eye: [f32; 3],
    pub target: [f32; 3],
    pub fov_degrees: f32,
}
impl Camera {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.eye.iter().chain(&self.target).all(|v| v.is_finite()),
            "camera coordinates must be finite"
        );
        anyhow::ensure!(
            self.fov_degrees.is_finite() && (5.0..=120.0).contains(&self.fov_degrees),
            "fov_degrees must be finite and between 5 and 120"
        );
        let delta = glam::DVec3::from_array(self.target.map(f64::from))
            - glam::DVec3::from_array(self.eye.map(f64::from));
        anyhow::ensure!(
            delta.length().is_finite() && delta.length() >= 0.001,
            "camera eye and target must be distinct"
        );
        anyhow::ensure!(
            delta.normalize().cross(glam::DVec3::Z).length() > 0.0001,
            "camera direction cannot be parallel to server up Z"
        );
        Ok(())
    }
    pub fn frame(bounds: &Bounds) -> Result<Self> {
        let min = glam::DVec3::from_array(bounds.min.map(f64::from));
        let max = glam::DVec3::from_array(bounds.max.map(f64::from));
        let center = (min + max) * 0.5;
        let radius = ((max - min).length() * 0.5).max(0.5);
        let eye = center + glam::DVec3::new(1., -1., 0.75).normalize() * radius * 2.8;
        let camera = Self {
            eye: eye.to_array().map(|v| v as f32),
            target: center.to_array().map(|v| v as f32),
            fov_degrees: 50.,
        };
        camera.validate()?;
        Ok(camera)
    }
    pub fn matrices(&self, aspect: f32, radius: f32) -> Result<(Mat4, Mat4)> {
        self.validate()?;
        anyhow::ensure!(
            aspect.is_finite() && aspect > 0. && radius.is_finite() && radius > 0.,
            "invalid preview camera aspect/bounds radius"
        );
        let eye = glam::DVec3::from_array(self.eye.map(f64::from));
        let direction = glam::DVec3::from_array(self.target.map(f64::from)) - eye;
        let near = (radius as f64 * 0.001).max(0.01) as f32;
        let far = (direction.length() + radius as f64 * 4.).max(near as f64 * 2.) as f32;
        anyhow::ensure!(
            near.is_finite() && far.is_finite() && far > near,
            "camera clipping range overflow"
        );
        let view = Mat4::look_to_rh(
            Vec3::from(self.eye),
            direction.normalize().as_vec3(),
            Vec3::Z,
        );
        let projection = Mat4::perspective_rh(self.fov_degrees.to_radians(), aspect, near, far);
        anyhow::ensure!(
            view.is_finite() && projection.is_finite() && (projection * view).is_finite(),
            "camera matrix overflow"
        );
        Ok((view, projection))
    }
    pub fn orbit(&self, horizontal: f32, vertical: f32) -> Result<Self> {
        self.validate()?;
        let target = glam::DVec3::from_array(self.target.map(f64::from));
        let offset = glam::DVec3::from_array(self.eye.map(f64::from)) - target;
        let distance = offset.length();
        let yaw = offset.y.atan2(offset.x) + horizontal as f64;
        let pitch = ((offset.z / distance).asin() + vertical as f64).clamp(-1.45, 1.45);
        let eye = target
            + glam::DVec3::new(
                yaw.cos() * pitch.cos(),
                yaw.sin() * pitch.cos(),
                pitch.sin(),
            ) * distance;
        let camera = Self {
            eye: eye.to_array().map(|v| v as f32),
            ..self.clone()
        };
        camera.validate()?;
        Ok(camera)
    }
    pub fn zoom(&self, scroll: f32) -> Result<Self> {
        self.validate()?;
        let target = glam::DVec3::from_array(self.target.map(f64::from));
        let offset = glam::DVec3::from_array(self.eye.map(f64::from)) - target;
        let eye = target
            + offset.normalize()
                * (offset.length() * (-scroll as f64 * 0.12).clamp(-5., 5.).exp()).max(0.01);
        let camera = Self {
            eye: eye.to_array().map(|v| v as f32),
            ..self.clone()
        };
        camera.validate()?;
        Ok(camera)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finite_camera_rejects_degenerate_views_and_unsupported_fov() {
        let mut c = Camera {
            eye: [0., -4., 2.],
            target: [0.; 3],
            fov_degrees: 60.,
        };
        assert!(c.validate().is_ok());
        c.eye = c.target;
        assert!(c.validate().is_err());
        c.eye = [0., 0., 3.];
        assert!(c.validate().is_err());
        c.eye = [f32::NAN, 2., 3.];
        assert!(c.validate().is_err());
        c.eye = [0., -4., 2.];
        c.fov_degrees = 180.;
        assert!(c.validate().is_err());
    }
    #[test]
    fn framing_and_mouse_changes_preserve_valid_server_camera() {
        let bounds = Bounds {
            min: [-3., -7., 2.],
            max: [16., 13., 6.5],
        };
        let c = Camera::frame(&bounds).unwrap();
        assert_eq!(c.target, [6.5, 3., 4.25]);
        assert_ne!(c.orbit(0.3, 0.2).unwrap(), c);
        assert_ne!(c.zoom(1.).unwrap(), c);
        let (v, p) = c.matrices(4. / 3., 50.).unwrap();
        assert!(v.is_finite() && p.is_finite());
        assert!((v * Vec3::from(c.target).extend(1.)).z < 0.);
    }
    #[test]
    fn camera_json_rejects_unknown_and_missing_fields() {
        assert!(serde_json::from_str::<Camera>(
            r#"{"eye":[1,2,3],"target":[0,0,0],"fov_degrees":60,"mode":"game"}"#
        )
        .is_err());
        assert!(serde_json::from_str::<Camera>(r#"{"eye":[1,2,3],"target":[0,0,0]}"#).is_err());
    }
}
