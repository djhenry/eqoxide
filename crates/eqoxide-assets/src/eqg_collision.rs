//! Explicit, collision-only EQG staging input. Render assets never enter this type.
use anyhow::{bail, ensure, Context, Result};
use std::path::Path;

/// Validated static triangle candidates in native EQG `(x, y, z)` coordinates.
/// Server alignment and runtime actor filtering are not established by this type.
#[derive(Debug)]
pub struct EqgCollisionCandidates {
    triangles: Vec<[[f32; 3]; 3]>,
}

impl EqgCollisionCandidates {
    pub fn triangles(&self) -> &[[[f32; 3]; 3]] {
        &self.triangles
    }

    pub fn from_glb(path: &Path) -> Result<Self> {
        Self::from_glb_bytes(&std::fs::read(path).context("reading EQG collision GLB")?)
    }

    pub fn from_glb_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.starts_with(b"glTF"), "EQG collision input must be a GLB");
        let gltf = gltf::Gltf::from_slice(bytes).context("invalid collision GLB")?;
        let extras: serde_json::Value = serde_json::from_str(
            gltf.document.as_json().extras.as_ref().context("missing collision metadata")?.get(),
        )?;
        let marker = extras.get("eqCollision").context("missing eqCollision metadata")?;
        ensure!(marker == &serde_json::json!({
            "version": 1, "coordinates": "eqg_gltf_y_up",
            "scope": "default_static_triangle_candidates", "nodes": [0]
        }), "unsupported or malformed eqCollision metadata");
        ensure!(gltf.nodes().len() == 1 && gltf.meshes().len() == 1,
            "collision staging GLB requires exactly one node and mesh");
        ensure!(gltf.scenes().len() == 1, "collision staging GLB requires one scene");
        let scene = gltf.default_scene().context("missing default collision scene")?;
        ensure!(scene.nodes().map(|n| n.index()).collect::<Vec<_>>() == [0], "invalid collision scene roots");
        let node = gltf.nodes().next().unwrap();
        ensure!(node.name() == Some("__collision__") && node.children().len() == 0
            && node.skin().is_none() && node.camera().is_none() && node.weights().is_none(),
            "invalid collision node topology");
        ensure!(node.transform().matrix() == glam::Mat4::IDENTITY.to_cols_array_2d(),
            "collision vertices must already be world-space at identity");
        ensure!(gltf.animations().len() == 0 && gltf.skins().len() == 0,
            "animated collision is unsupported");
        let mesh = node.mesh().context("collision node has no mesh")?;
        ensure!(mesh.name() == Some("__collision__") && mesh.primitives().len() == 1 && mesh.weights().is_none(),
            "invalid collision mesh");
        let primitive = mesh.primitives().next().unwrap();
        ensure!(primitive.mode() == gltf::mesh::Mode::Triangles && primitive.morph_targets().len() == 0,
            "collision requires static triangles");
        ensure!(gltf.buffers().len() == 1, "collision requires one embedded buffer");
        let buffer = gltf.buffers().next().unwrap();
        ensure!(matches!(buffer.source(), gltf::buffer::Source::Bin), "external collision buffers unsupported");
        let blob = gltf.blob.as_deref().context("missing collision binary chunk")?;
        ensure!(blob.len() >= buffer.length(), "truncated collision buffer");
        let positions = primitive.get(&gltf::Semantic::Positions).context("missing collision positions")?;
        ensure!(positions.data_type() == gltf::accessor::DataType::F32
            && positions.dimensions() == gltf::accessor::Dimensions::Vec3 && !positions.normalized(),
            "collision positions must be unnormalized float VEC3");
        validate_span(&positions, buffer.length())?;
        let indices = primitive.indices().context("collision requires indices")?;
        ensure!(indices.dimensions() == gltf::accessor::Dimensions::Scalar && !indices.normalized()
            && matches!(indices.data_type(), gltf::accessor::DataType::U8 | gltf::accessor::DataType::U16 | gltf::accessor::DataType::U32),
            "invalid collision index format");
        validate_span(&indices, buffer.length())?;
        ensure!(indices.count() > 0 && indices.count() % 3 == 0, "empty or incomplete collision triangles");
        let reader = primitive.reader(|_| Some(blob));
        let positions: Vec<_> = reader.read_positions().context("unreadable positions")?
            .map(|p| [p[0], -p[2], p[1]]).collect();
        ensure!(positions.iter().flatten().all(|v| v.is_finite() && v.abs() <= 1_000_000.0), "collision position outside finite staging range");
        let indices: Vec<_> = reader.read_indices().context("unreadable indices")?.into_u32().collect();
        let mut triangles = Vec::with_capacity(indices.len() / 3);
        for ids in indices.chunks_exact(3) {
            ensure!(ids.iter().all(|&i| (i as usize) < positions.len()), "collision index out of bounds");
            let t = [positions[ids[0] as usize], positions[ids[1] as usize], positions[ids[2] as usize]];
            let a = [0, 1, 2].map(|i| t[1][i] as f64 - t[0][i] as f64);
            let b = [0, 1, 2].map(|i| t[2][i] as f64 - t[0][i] as f64);
            ensure!([a[1]*b[2]-a[2]*b[1], a[2]*b[0]-a[0]*b[2], a[0]*b[1]-a[1]*b[0]] != [0.0; 3],
                "degenerate collision triangle");
            triangles.push(t);
        }
        Ok(Self { triangles })
    }
}

fn validate_span(accessor: &gltf::Accessor<'_>, buffer_len: usize) -> Result<()> {
    ensure!(accessor.sparse().is_none(), "sparse collision accessors unsupported");
    let view = accessor.view().context("collision accessor has no buffer view")?;
    let stride = view.stride().unwrap_or(accessor.size());
    ensure!(stride >= accessor.size(), "collision accessor stride too small");
    let Some(end) = accessor.count().checked_sub(1)
        .and_then(|n| n.checked_mul(stride))
        .and_then(|n| n.checked_add(accessor.offset()))
        .and_then(|n| n.checked_add(accessor.size())) else {
        bail!("empty or overflowing collision accessor");
    };
    ensure!(end <= view.length() && view.offset().checked_add(view.length()).is_some_and(|end| end <= buffer_len),
        "collision accessor exceeds embedded buffer");
    Ok(())
}


pub(crate) fn reject_collision_marker(document: &gltf::Document) -> Result<()> {
    if let Some(raw) = document.as_json().extras.as_ref() {
        let extras: serde_json::Value = serde_json::from_str(raw.get())?;
        ensure!(extras.get("eqCollision").is_none(),
            "explicit EQG collision metadata requires EqgCollisionCandidates loader");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(change: impl FnOnce(&mut serde_json::Value, &mut Vec<u8>)) -> Vec<u8> {
        let mut bin = Vec::new();
        for p in [[10f32, 30., -20.], [14., 30., -20.], [10., 30., -24.]] {
            for v in p { bin.extend(v.to_le_bytes()); }
        }
        for i in [0u32, 1, 2] { bin.extend(i.to_le_bytes()); }
        let mut json = serde_json::json!({"asset":{"version":"2.0"}, "scene":0,
            "scenes":[{"nodes":[0]}], "nodes":[{"name":"__collision__","mesh":0}],
            "meshes":[{"name":"__collision__","primitives":[{"attributes":{"POSITION":0},"indices":1}]}],
            "buffers":[{"byteLength":48}], "bufferViews":[{"buffer":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":12}],
            "accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[10,30,-24],"max":[14,30,-20]},
                {"bufferView":1,"componentType":5125,"count":3,"type":"SCALAR"}],
            "extras":{"eqCollision":{"version":1,"coordinates":"eqg_gltf_y_up","scope":"default_static_triangle_candidates","nodes":[0]}}});
        change(&mut json, &mut bin);
        let mut json = serde_json::to_vec(&json).unwrap();
        while json.len() % 4 != 0 { json.push(b' '); }
        let mut out = Vec::new();
        for word in [0x46546c67u32, 2, (28 + json.len() + bin.len()) as u32, json.len() as u32, 0x4e4f534a] { out.extend(word.to_le_bytes()); }
        out.extend(json);
        out.extend((bin.len() as u32).to_le_bytes()); out.extend(0x004e4942u32.to_le_bytes()); out.extend(bin);
        out
    }
    #[test]
    fn eqg_collision_coordinates_and_winding() {
        let source = EqgCollisionCandidates::from_glb_bytes(&fixture(|_,_|{})).unwrap();
        assert_eq!(source.triangles(), &[[[10.,20.,30.], [14.,20.,30.], [10.,24.,30.]]]);
    }
    #[test]
    fn eqg_collision_rejects_metadata_topology_and_geometry() {
        let changes: Vec<Box<dyn FnOnce(&mut serde_json::Value, &mut Vec<u8>)>> = vec![
            Box::new(|j,_| { j.as_object_mut().unwrap().remove("extras"); }),
            Box::new(|j,_| j["extras"]["eqCollision"]["version"] = 2.into()),
            Box::new(|j,_| j["extras"]["eqCollision"]["nodes"] = serde_json::json!([0,0])),
            Box::new(|j,_| j["nodes"][0]["translation"] = serde_json::json!([0,1,0])),
            Box::new(|j,_| j["nodes"][0]["children"] = serde_json::json!([0])),
            Box::new(|j,_| j["meshes"][0]["primitives"][0]["mode"] = 1.into()),
            Box::new(|j,_| j["buffers"][0]["uri"] = "missing.bin".into()),
            Box::new(|j,_| j["accessors"][1]["count"] = 0.into()),
            Box::new(|j,_| j["accessors"][0]["byteOffset"] = 100.into()),
            Box::new(|_,b| b[36..40].copy_from_slice(&9u32.to_le_bytes())),
            Box::new(|_,b| b[0..4].copy_from_slice(&f32::NAN.to_le_bytes())),
            Box::new(|_,b| b[40..44].copy_from_slice(&0u32.to_le_bytes())),
        ];
        for (i, change) in changes.into_iter().enumerate() {
            assert!(EqgCollisionCandidates::from_glb_bytes(&fixture(change)).is_err(), "case {i}");
        }
    }
    #[test]
    fn eqg_collision_marker_rejected_by_legacy_entry_points() {
        let path = std::env::temp_dir().join(format!("eqg-collision-marker-{}.glb", std::process::id()));
        for version in [1,99] {
            std::fs::write(&path, fixture(|j,_| j["extras"]["eqCollision"]["version"] = version.into())).unwrap();
            assert!(crate::ZoneAssets::from_glb(&path).err().unwrap().to_string().contains("requires EqgCollisionCandidates"));
            assert!(crate::ZoneAssets::from_preview_glb(&path).is_err());
            assert!(crate::ZoneAssets::object_models_from_glb(&path).is_err());
        }
        std::fs::write(&path, fixture(|j,_| {j.as_object_mut().unwrap().remove("extras");})).unwrap();
        assert!(crate::ZoneAssets::from_glb(&path).is_ok());
        std::fs::remove_file(path).unwrap();
    }
}
