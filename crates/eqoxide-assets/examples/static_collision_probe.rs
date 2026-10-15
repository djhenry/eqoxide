//! Inspect a collision artifact through the actual bounded CPU decoder.
use anyhow::{ensure, Context, Result};
use eqoxide_assets::static_collision::{decode_static_collision, CollisionDecodeLimits};
use std::io::Read;

fn main() -> Result<()> {
    let path = std::env::args_os()
        .nth(1)
        .context("usage: static_collision_probe INPUT.glb")?;
    let limits = CollisionDecodeLimits::default();
    let file = std::fs::File::open(&path).context("open static collision")?;
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
    let collision = decode_static_collision(&bytes, &limits)?;
    let first_mesh=collision.meshes.first().map(|m|serde_json::json!({
        "positions_server_geometry":m.positions.iter().take(8).collect::<Vec<_>>(),
        "primitive_indices":m.primitives.iter().take(4).map(|p|p.indices.iter().take(24).collect::<Vec<_>>()).collect::<Vec<_>>()
    }));
    let instances = collision
        .instances
        .iter()
        .take(8)
        .map(|i| {
            serde_json::json!({
                "mesh_index":i.mesh_index,"owner_id":i.owner_id,"matrix_server_geometry":i.matrix
            })
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version":collision.header.schema_version,"role":collision.header.role,
            "coordinate_profile":collision.header.coordinate_profile,"bake_revision":collision.header.bake_revision,
            "scope":collision.header.scope,"collision_version":collision.header.collision_version,
            "requirements":{"reader_version":collision.header.requirements.reader_version,"capabilities":collision.header.requirements.capabilities},
            "availability":format!("{:?}",collision.availability),"declared_query_policy":format!("{:?}",collision.query_policy),
            "query_unit_frames":collision.header.query_unit_frames.identifier(),
            "segment_queries_implemented":false,"meshes":collision.meshes.len(),"instances":collision.instances.len(),
            "first_mesh_sample":first_mesh,"instance_samples":instances,
            "bounds_server_geometry":collision.bounds.map(|b|serde_json::json!({"min":b.min,"max":b.max})),
        }))?
    );
    Ok(())
}
