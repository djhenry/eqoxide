//! Moving surfaces such as lift platforms, kept beside the static zone triangles of
//! [`crate::collision::Collision`].
//!
//! A [`SurfaceLayer`] stores each surface as world-space triangles for its current pose. The owner
//! of the poses replaces the whole layer every frame with [`crate::collision::Collision::set_surfaces`],
//! so the layer never moves a triangle itself.

/// Identifies a moving surface. A door surface uses the door id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SurfaceId(pub u32);

/// What carries a rider and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    /// Moves only in z. A rider needs no carry because the floor probe reads the new z.
    Platform,
    /// Moves horizontally, so a rider needs a carry on top of the floor probe.
    Boat,
}

/// One moving surface, as world-space triangles for its current pose.
#[derive(Debug, Clone, PartialEq)]
pub struct Surface {
    pub id: SurfaceId,
    pub kind: SurfaceKind,
    pub tris: Vec<[[f32; 3]; 3]>,
    /// Velocity in units per second at this pose, `[0, 0, 0]` for a surface at rest.
    pub velocity: [f32; 3],
}

impl Surface {
    /// Returns the center of the bounding box of the triangles, or the origin for no triangles.
    pub fn center(&self) -> [f32; 3] {
        let mut lo = [f32::MAX; 3];
        let mut hi = [f32::MIN; 3];
        for v in self.tris.iter().flatten() {
            for k in 0..3 { lo[k] = lo[k].min(v[k]); hi[k] = hi[k].max(v[k]); }
        }
        if lo[0] > hi[0] { return [0.0; 3]; }
        [(lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0, (lo[2] + hi[2]) / 2.0]
    }

    /// Returns the largest horizontal distance from [`Self::center`] to a vertex.
    pub fn footprint_radius(&self) -> f32 {
        let c = self.center();
        self.tris.iter().flatten()
            .map(|v| ((v[0] - c[0]).powi(2) + (v[1] - c[1]).powi(2)).sqrt())
            .fold(0.0, f32::max)
    }

    /// Returns whether a rider standing on the surface moves with it. Every kind does: a
    /// [`SurfaceKind::Platform`] through the floor probe and a [`SurfaceKind::Boat`] through the
    /// client's carry.
    pub fn carries_riders(&self) -> bool {
        matches!(self.kind, SurfaceKind::Platform | SurfaceKind::Boat)
    }
}

/// Every moving surface of the current zone for one frame.
#[derive(Debug, Clone, Default)]
pub struct SurfaceLayer {
    pub surfaces: Vec<Surface>,
}

impl SurfaceLayer {
    /// Returns `true` if the layer contains no surface.
    pub fn is_empty(&self) -> bool {
        self.surfaces.is_empty()
    }
}

/// Returns the z component of the unit face normal of `tri`, or 0 for a degenerate triangle.
///
/// The sign is the facing, with the same convention as the per-triangle normals that
/// [`crate::collision::Collision`] stores for static triangles.
pub(crate) fn tri_normal_z(tri: &[[f32; 3]; 3]) -> f32 {
    let e1 = [tri[1][0] - tri[0][0], tri[1][1] - tri[0][1], tri[1][2] - tri[0][2]];
    let e2 = [tri[2][0] - tri[0][0], tri[2][1] - tri[0][1], tri[2][2] - tri[0][2]];
    let n = [e1[1] * e2[2] - e1[2] * e2[1],
             e1[2] * e2[0] - e1[0] * e2[2],
             e1[0] * e2[1] - e1[1] * e2[0]];
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if len > 1e-9 { n[2] / len } else { 0.0 }
}

/// Returns the height at which the vertical line through `(east, north)` crosses `tri`, or `None`
/// if the line misses the triangle or the triangle is vertical.
pub(crate) fn column_cross_z(tri: &[[f32; 3]; 3], east: f32, north: f32) -> Option<f32> {
    let (a, b, c) = (tri[0], tri[1], tri[2]);
    let det = (b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1]);
    if det.abs() < 1e-9 { return None; }
    let u = ((east - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (north - a[1])) / det;
    let v = ((b[0] - a[0]) * (north - a[1]) - (east - a[0]) * (b[1] - a[1])) / det;
    if u < 0.0 || v < 0.0 || u + v > 1.0 { return None; }
    Some(a[2] + u * (b[2] - a[2]) + v * (c[2] - a[2]))
}
