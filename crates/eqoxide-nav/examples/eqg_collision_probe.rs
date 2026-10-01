//! Offline inspection of explicit EQG static collision candidates.
use eqoxide_assets::{EqgCollisionCandidates, ServerEqgCollisionCandidates};
use eqoxide_nav::collision::Collision;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (server_axes, args) = parse_args(std::env::args().skip(1).collect())?;
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
    let candidates = if server_axes {
        ProbeCandidates::Server(source.into_server_coordinates())
    } else {
        ProbeCandidates::Source(source)
    };
    let report = probe_report(candidates, segment)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

// The axis tag, triangles, and grid cannot be supplied independently.
enum ProbeCandidates {
    Source(EqgCollisionCandidates),
    Server(ServerEqgCollisionCandidates),
}

fn parse_args(mut args: Vec<String>) -> Result<(bool, Vec<String>), &'static str> {
    let server_axes = args.first().is_some_and(|arg| arg == "--server-axes");
    if server_axes { args.remove(0); }
    if args.first().is_some_and(|arg| arg == "--") { args.remove(0); }
    if args.len() != 1 && args.len() != 7 {
        return Err("usage: eqg_collision_probe [--server-axes] [--] COLLISION.glb [FROM_X FROM_Y FROM_Z TO_X TO_Y TO_Z]");
    }
    Ok((server_axes, args))
}

fn probe_report(
    candidates: ProbeCandidates,
    segment: Option<([f32; 3], [f32; 3])>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let (triangles, grid, coordinates) = match &candidates {
        ProbeCandidates::Source(source) => (source.triangles(),
            Collision::build_eqg_candidates(source, 32.0)?, "native_source_xyz"),
        ProbeCandidates::Server(source) => (source.triangles(),
            Collision::build_server_eqg_candidates(source, 32.0)?, "server_geometry_xyz"),
    };
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
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        let mut bin = Vec::new();
        for p in [[10f32, 30., -20.], [14., 30., -20.], [10., 30., -24.]] {
            for v in p { bin.extend(v.to_le_bytes()); }
        }
        for i in [0u32, 1, 2] { bin.extend(i.to_le_bytes()); }
        let json = serde_json::json!({"asset":{"version":"2.0"}, "scene":0,
            "scenes":[{"nodes":[0]}], "nodes":[{"name":"__collision__","mesh":0}],
            "meshes":[{"name":"__collision__","primitives":[{"attributes":{"POSITION":0},"indices":1}]}],
            "buffers":[{"byteLength":48}], "bufferViews":[{"buffer":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":12}],
            "accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[10,30,-24],"max":[14,30,-20]},
                {"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}],
            "extras":{"eqCollision":{"version":1,"coordinates":"eqg_gltf_y_up","scope":"default_static_triangle_candidates","nodes":[0]}}});
        let mut json = serde_json::to_vec(&json).unwrap();
        while !json.len().is_multiple_of(4) { json.push(b' '); }
        let mut out = Vec::new();
        for word in [0x46546c67u32, 2, (28 + json.len() + bin.len()) as u32, json.len() as u32, 0x4e4f534a] { out.extend(word.to_le_bytes()); }
        out.extend(json);
        out.extend((bin.len() as u32).to_le_bytes()); out.extend(0x004e4942u32.to_le_bytes()); out.extend(bin);
        out
    }
    #[test]
    fn typed_report_keeps_bounds_queries_and_axis_label_together() {
        let load = || EqgCollisionCandidates::from_glb_bytes(&fixture()).unwrap();
        let source = probe_report(ProbeCandidates::Source(load()), Some(([11.,21.,32.],[11.,21.,28.]))).unwrap();
        let server = probe_report(ProbeCandidates::Server(load().into_server_coordinates()), Some(([21.,11.,32.],[21.,11.,28.]))).unwrap();
        assert_eq!(source["coordinates"], "native_source_xyz");
        assert_eq!(server["coordinates"], "server_geometry_xyz");
        assert_eq!(source["bounds"]["min"], serde_json::json!([10.,20.,30.]));
        assert_eq!(server["bounds"]["min"], serde_json::json!([20.,10.,30.]));
        assert_eq!(source["bounds"]["max"], serde_json::json!([14.,24.,30.]));
        assert_eq!(server["bounds"]["max"], serde_json::json!([24.,14.,30.]));
        for report in [&source, &server] {
            assert_eq!(report["segment"]["hit"]["fraction"], 0.5);
            assert_eq!(report["triangle_count"], 1);
            assert_eq!(report["gameplay_ready"], false);
        }
    }

    #[test]
    fn option_terminator_preserves_flag_named_path() {
        let parse = |args: &[&str]| parse_args(args.iter().map(|s| s.to_string()).collect()).unwrap();
        assert_eq!(parse(&["--", "--server-axes"]), (false, vec!["--server-axes".into()]));
        assert_eq!(parse(&["--server-axes", "--", "--server-axes"]), (true, vec!["--server-axes".into()]));
        assert!(parse(&["--server-axes", "zone.glb"]).0);
        assert!(!parse(&["zone.glb"]).0);
    }
}
