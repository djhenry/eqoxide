//! Inspect explicitly supplied assembled static-package files without activation.
use anyhow::{ensure, Context, Result};
use eqoxide::{asset_sync::Manifest, static_scene_package::*};
use std::{
    io::Read,
    path::{Path, PathBuf},
};
fn bounded_regular_file(path: &Path, maximum: usize) -> Result<Vec<u8>> {
    let maximum = u64::try_from(maximum)?;
    // Preflight before open: caller-selected FIFOs must not block on opening.
    let metadata = std::fs::metadata(path)?;
    ensure!(metadata.is_file(), "input must be a stable regular file");
    ensure!(metadata.len() <= maximum, "input byte limit exceeded");
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= maximum,
        "opened input exceeds regular-file/byte limits"
    );
    let mut bytes = Vec::new();
    file.take(
        maximum
            .checked_add(1)
            .context("input byte allowance overflow")?,
    )
    .read_to_end(&mut bytes)?;
    ensure!(
        u64::try_from(bytes.len())? <= maximum,
        "input byte limit exceeded"
    );
    Ok(bytes)
}
fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let usage="usage: static_scene_package_probe MANIFEST_FILE EXPECTED_ZONE EXPECTED_REVISION [--file LOGICAL_PATH DISK_PATH]";
    let manifest_path = PathBuf::from(args.next().context(usage)?);
    let zone = args
        .next()
        .context(usage)?
        .into_string()
        .map_err(|_| anyhow::anyhow!("zone must be UTF-8"))?;
    let revision = args
        .next()
        .context(usage)?
        .into_string()
        .map_err(|_| anyhow::anyhow!("revision must be UTF-8"))?;
    let limits = PackageDecodeLimits::default();
    let mut paths = Vec::new();
    while let Some(flag) = args.next() {
        ensure!(flag == "--file", "expected explicit --file pair");
        ensure!(
            paths.len() < limits.max_supplied_files,
            "supplied file limit exceeded"
        );
        let wire = args
            .next()
            .context(usage)?
            .into_string()
            .map_err(|_| anyhow::anyhow!("logical path must be UTF-8"))?;
        let disk = PathBuf::from(args.next().context(usage)?);
        paths.push((wire, disk));
    }
    let manifest_bytes = bounded_regular_file(&manifest_path, 8 << 20)?;
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("invalid manifest JSON")?;
    let mut retained = 0usize;
    let mut files = Vec::new();
    for (wire, disk) in paths {
        let limit = if wire == "scene.json" {
            limits.max_descriptor_bytes
        } else {
            limits.max_component_bytes
        };
        let remaining = limits
            .max_assembled_bytes
            .checked_sub(retained)
            .context("assembled input byte limit exceeded")?;
        let bytes = bounded_regular_file(&disk, limit.min(remaining))?;
        retained = retained
            .checked_add(bytes.len())
            .context("assembled input size overflow")?;
        files.push((wire, bytes));
    }
    let borrowed: Vec<_> = files
        .iter()
        .map(|(p, b)| PackageFile { path: p, bytes: b })
        .collect();
    let package = decode_static_scene_package(&zone, &revision, &manifest, &borrowed, &limits)?;
    let visual = package.visual().map(|v| {
        serde_json::json!({"meshes":v.meshes.len(),"instances":v.instances.len(),
        "bounds":{"min":v.bounds.min,"max":v.bounds.max}})
    });
    let collision=package.collision().map(|c|serde_json::json!({"meshes":c.meshes.len(),"instances":c.instances.len(),
        "bounds":c.bounds.as_ref().map(|b|serde_json::json!({"min":b.min,"max":b.max})),
        "declared_policy":match c.query_policy {eqoxide_assets::static_collision::InspectionQueryPolicy::FrontFaces=>"inspection-segment-front-v1",
            eqoxide_assets::static_collision::InspectionQueryPolicy::BothFaces=>"inspection-segment-both-v1"}}));
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "zone":package.zone(),"set":package.set(),"manifest_revision":package.manifest_revision(),
            "bake_revision":package.bake_revision(),"descriptor_blake3":package.descriptor_blake3(),
            "component_digests":{"visual":package.visual_blake3(),"collision":package.collision_blake3()},
            "availability":{"visual":package.visual_availability(),"collision":package.collision_availability()},
            "visual":visual,"collision":collision,"assembled_file_content_verified":true,
            "individual_cas_chunks_verified":false,"runtime_activation":false,"segment_queries":false
        }))?
    );
    Ok(())
}
