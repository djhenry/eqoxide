//! Probe: are the collision-mesh triangle normals consistently wound in the shipped zone GLBs?
//! Decides whether the navmesh baker can trust `nz > 0` to mean "floor" and `nz < 0` to mean
//! "ceiling" — the only thing that can distinguish them in EQ's thin-shell geometry.
use eqoxide::assets::ZoneAssets;
use eqoxide::navmesh::collision_tris;

fn main() -> anyhow::Result<()> {
    let models = dirs::data_dir().unwrap().join("eqoxide/assets/models");
    for zone in std::env::args().skip(1) {
        let assets = ZoneAssets::from_glb(&models.join(format!("{zone}.glb")))?;
        let tris = collision_tris(&assets);
        let (mut up, mut down, mut vert) = (0usize, 0usize, 0usize);
        // Sample a known-flat region: count how many near-horizontal faces point up vs down.
        let mut zs_up: Vec<f32> = Vec::new();
        let mut zs_down: Vec<f32> = Vec::new();
        for t in &tris {
            let e1 = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
            let e2 = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
            let n = [e1[1] * e2[2] - e1[2] * e2[1],
                     e1[2] * e2[0] - e1[0] * e2[2],
                     e1[0] * e2[1] - e1[1] * e2[0]];
            let nl = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if nl < 1e-9 { continue; }
            let nz = n[2] / nl;
            let cz = (t[0][2] + t[1][2] + t[2][2]) / 3.0;
            if nz > 0.7 { up += 1; zs_up.push(cz); }
            else if nz < -0.7 { down += 1; zs_down.push(cz); }
            else { vert += 1; }
        }
        let med = |v: &mut Vec<f32>| {
            if v.is_empty() { return f32::NAN; }
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        println!("{zone:12} tris={:7}  up(nz>.7)={:6} (median z {:8.1})  down(nz<-.7)={:6} (median z {:8.1})  vertical={:6}",
            tris.len(), up, med(&mut zs_up), down, med(&mut zs_down), vert);
    }
    Ok(())
}
