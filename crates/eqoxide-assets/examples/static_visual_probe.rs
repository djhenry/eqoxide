//! Inspect a unified visual artifact through the actual bounded CPU decoder.
use anyhow::{ensure, Context, Result};
use eqoxide_assets::static_visual::{decode_static_visual, DecodeLimits};
use std::io::Read;

fn main() -> Result<()> {
    let path = std::env::args_os().nth(1).context("usage: static_visual_probe INPUT.glb")?;
    let limits = DecodeLimits::default();
    let file = std::fs::File::open(&path).context("open static visual")?;
    ensure!(file.metadata()?.len() <= limits.max_glb_bytes as u64, "GLB exceeds read limit");
    let cap = limits.max_glb_bytes.checked_add(1).context("read limit overflow")?;
    let mut bytes = Vec::new();
    file.take(cap as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limits.max_glb_bytes, "GLB exceeds read limit");
    let visual = decode_static_visual(&bytes, &limits)?;
    let header = &visual.header;
    let colored_primitives = visual.meshes.iter().flat_map(|m| &m.primitives)
        .filter(|p| p.colors.is_some()).count();
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "schema_version": header.schema_version,
        "role": header.role,
        "coordinate_profile": header.coordinate_profile,
        "bake_revision": header.bake_revision,
        "requirements": {
            "reader_version": header.requirements.reader_version,
            "capabilities": header.requirements.capabilities
        },
        "meshes": visual.meshes.len(), "instances": visual.instances.len(),
        "materials": visual.materials.len(), "images": visual.images.len(),
        "textures": visual.textures.len(), "colored_primitives": colored_primitives,
        "bounds_server_geometry": {"min": visual.bounds.min, "max": visual.bounds.max}
    }))?);
    Ok(())
}
