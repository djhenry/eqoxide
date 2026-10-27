//! Inspect a unified visual artifact through the actual bounded CPU decoder.
use anyhow::{ensure, Context, Result};
use eqoxide_assets::static_visual::{
    decode_static_visual, DecodeLimits, StaticAlphaMode, StaticSurfaceModel, PBR_FACTORS_CAPABILITY,
};
use std::io::Read;

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: static_visual_probe INPUT.glb")?;
    let limits = DecodeLimits::default();
    let file = std::fs::File::open(&path).context("open static visual")?;
    ensure!(
        file.metadata()?.len() <= limits.max_glb_bytes as u64,
        "GLB exceeds read limit"
    );
    let cap = limits
        .max_glb_bytes
        .checked_add(1)
        .context("read limit overflow")?;
    let mut bytes = Vec::new();
    file.take(cap as u64).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limits.max_glb_bytes,
        "GLB exceeds read limit"
    );
    let visual = decode_static_visual(&bytes, &limits)?;
    let header = &visual.header;
    let modern = header
        .requirements
        .capabilities
        .iter()
        .any(|capability| capability == PBR_FACTORS_CAPABILITY);
    let material_details: Vec<_> = visual
        .materials
        .iter()
        .enumerate()
        .map(|(index, material)| {
            let surface = match material.surface {
                StaticSurfaceModel::Pbr { metallic, roughness } => serde_json::json!({
                    "model": "pbr-metallic-roughness",
                    "metallic_factor": metallic,
                    "roughness_factor": roughness
                }),
                StaticSurfaceModel::Unlit => serde_json::json!({
                    "model": "unlit",
                    "intent": if modern { "KHR_materials_unlit" } else { "legacy-inspection-override" }
                }),
            };
            let (alpha_mode, alpha_cutoff) = match material.alpha_mode {
                StaticAlphaMode::Opaque => ("OPAQUE", None),
                StaticAlphaMode::Mask { cutoff } => ("MASK", Some(cutoff)),
                StaticAlphaMode::Blend => ("BLEND", None),
            };
            serde_json::json!({
                "index": index,
                "name": material.name,
                "surface": surface,
                "base_color_factor": material.base_color,
                "base_color_texture": material.texture_index,
                "alpha_mode": alpha_mode,
                "alpha_cutoff": alpha_cutoff,
                "double_sided": material.double_sided
            })
        })
        .collect();
    let colored_primitives = visual
        .meshes
        .iter()
        .flat_map(|m| &m.primitives)
        .filter(|p| p.colors.is_some())
        .count();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": header.schema_version,
            "role": header.role,
            "coordinate_profile": header.coordinate_profile,
            "bake_revision": header.bake_revision,
            "requirements": {
                "reader_version": header.requirements.reader_version,
                "capabilities": header.requirements.capabilities
            },
            "meshes": visual.meshes.len(), "instances": visual.instances.len(),
            "materials": visual.materials.len(), "material_details": material_details,
            "images": visual.images.len(),
            "textures": visual.textures.len(), "colored_primitives": colored_primitives,
            "bounds_server_geometry": {"min": visual.bounds.min, "max": visual.bounds.max}
        }))?
    );
    Ok(())
}
