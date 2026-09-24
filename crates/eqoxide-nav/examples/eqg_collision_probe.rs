//! Offline inspection of explicit EQG static collision candidates.
use eqoxide_assets::EqgCollisionCandidates;
use eqoxide_nav::collision::Collision;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let server_axes = args.first().is_some_and(|arg| arg == "--server-axes");
    if server_axes { args.remove(0); }
    if args.len() != 1 && args.len() != 7 {
        return Err("usage: eqg_collision_probe [--server-axes] COLLISION.glb [FROM_X FROM_Y FROM_Z TO_X TO_Y TO_Z]".into());
    }
    let segment = if args.len() == 7 {
        let values: Vec<f32> = args[1..].iter().map(|v| v.parse()).collect::<Result<_, _>>()?;
        if !values.iter().all(|v| v.is_finite() && v.abs() <= 1_000_000.0) {
            return Err("segment coordinates must be finite and within one million coordinate units".into());
        }
        let from = [values[0], values[1], values[2]];
        let to = [values[3], values[4], values[5]];
        if from == to {
            return Err("segment endpoints must differ".into());
        }
        Some((from, to))
    } else {
        None
    };
    let source = EqgCollisionCandidates::from_glb(std::path::Path::new(&args[0]))?;
    let report = if server_axes {
        let source = source.into_server_coordinates();
        let grid = Collision::build_server_eqg_candidates(&source, 32.0)?;
        probe_report(source.triangles(), &grid, "server_geometry_xyz", segment)
    } else {
        let grid = Collision::build_eqg_candidates(&source, 32.0)?;
        probe_report(source.triangles(), &grid, "native_source_xyz", segment)
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn probe_report(
    triangles: &[[[f32; 3]; 3]],
    grid: &Collision,
    coordinates: &str,
    segment: Option<([f32; 3], [f32; 3])>,
) -> serde_json::Value {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for vertex in triangles.iter().flatten() {
        for axis in 0..3 {
            min[axis] = min[axis].min(vertex[axis]);
            max[axis] = max[axis].max(vertex[axis]);
        }
    }
    let mut report = serde_json::json!({
        "scope": "default_static_triangle_candidates",
        "coordinates": coordinates,
        "triangle_count": triangles.len(),
        "bounds": {"min": min, "max": max},
        "grid_cell_size": 32.0,
        "gameplay_ready": false
    });
    if let Some((from, to)) = segment {
        report["segment"] = serde_json::json!({
            "from": from,
            "to": to,
            "hit": grid.nearest_hit(from, to).map(|(fraction, normal)| {
                serde_json::json!({"fraction": fraction, "normal": normal})
            })
        });
    }
    report
}
