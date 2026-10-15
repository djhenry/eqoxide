//! Verified owned CPU inspection data; no runtime activation or query operation.
use crate::asset_sync::Manifest;
use anyhow::Result;
use eqoxide_assets::{CollisionDecodeLimits, DecodeLimits, StaticCollision, StaticVisual};
#[derive(Debug, Clone)]
pub struct PackageDecodeLimits {
    pub max_descriptor_bytes: usize,
    pub max_component_bytes: usize,
    pub max_assembled_bytes: usize,
    pub max_supplied_files: usize,
    pub max_manifest_files: usize,
    pub max_chunk_references: usize,
    pub visual: DecodeLimits,
    pub collision: CollisionDecodeLimits,
}
impl Default for PackageDecodeLimits {
    fn default() -> Self {
        Self {
            max_descriptor_bytes: 65536,
            max_component_bytes: 128 << 20,
            max_assembled_bytes: (128usize << 20)
                .checked_mul(2)
                .and_then(|n| n.checked_add(65536))
                .expect("fixed package byte bound"),
            max_supplied_files: 3,
            max_manifest_files: 1024,
            max_chunk_references: 65536,
            visual: DecodeLimits::default(),
            collision: CollisionDecodeLimits::default(),
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub struct PackageFile<'a> {
    pub path: &'a str,
    pub bytes: &'a [u8],
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentAvailability {
    NotExtracted,
    Unsupported,
    Available,
    AvailableEmpty,
}
/// Owned CPU inspection data with immutable verified association.
/// External code cannot rewrite its verified identity:
/// ```compile_fail
/// use eqoxide::static_scene_package::StaticScenePackage;
/// fn rebind(package: &mut StaticScenePackage) {
///     package.zone = "other".to_string();
/// }
/// ```
#[derive(Debug)]
pub struct StaticScenePackage {
    zone: String,
    set: String,
    manifest_revision: String,
    bake_revision: String,
    descriptor_blake3: String,
    visual_blake3: Option<String>,
    collision_blake3: Option<String>,
    visual_availability: ComponentAvailability,
    collision_availability: ComponentAvailability,
    visual: Option<StaticVisual>,
    collision: Option<StaticCollision>,
}
impl StaticScenePackage {
    pub fn zone(&self) -> &str {
        &self.zone
    }
    pub fn set(&self) -> &str {
        &self.set
    }
    pub fn manifest_revision(&self) -> &str {
        &self.manifest_revision
    }
    pub fn bake_revision(&self) -> &str {
        &self.bake_revision
    }
    pub fn descriptor_blake3(&self) -> &str {
        &self.descriptor_blake3
    }
    pub fn visual_blake3(&self) -> Option<&str> {
        self.visual_blake3.as_deref()
    }
    pub fn collision_blake3(&self) -> Option<&str> {
        self.collision_blake3.as_deref()
    }
    pub fn visual_availability(&self) -> ComponentAvailability {
        self.visual_availability
    }
    pub fn collision_availability(&self) -> ComponentAvailability {
        self.collision_availability
    }
    pub fn visual(&self) -> Option<&StaticVisual> {
        self.visual.as_ref()
    }
    pub fn collision(&self) -> Option<&StaticCollision> {
        self.collision.as_ref()
    }
}
mod descriptor;
use crate::{
    asset_compatibility::{valid_hash, valid_path, valid_token, validate_integrity},
    asset_sync::blake3_hex,
};
use anyhow::{ensure, Context};
use descriptor::{CollisionComponent, SceneDescriptor, VisualComponent};
use std::collections::BTreeSet;
fn charge(total: &mut usize, count: usize, limit: usize, label: &str) -> Result<()> {
    *total = total
        .checked_add(count)
        .context("package work counter overflow")?;
    ensure!(*total <= limit, "{label} limit exceeded");
    Ok(())
}
fn capabilities_equal(a: &[String], b: &[String]) -> bool {
    let a: BTreeSet<_> = a.iter().collect();
    let b: BTreeSet<_> = b.iter().collect();
    a == b
}
fn visual_state(v: &VisualComponent) -> ComponentAvailability {
    match v {
        VisualComponent::NotExtracted {} => ComponentAvailability::NotExtracted,
        VisualComponent::Unsupported {} => ComponentAvailability::Unsupported,
        VisualComponent::Available { .. } => ComponentAvailability::Available,
    }
}
fn collision_state(c: &CollisionComponent) -> ComponentAvailability {
    match c {
        CollisionComponent::NotExtracted {} => ComponentAvailability::NotExtracted,
        CollisionComponent::Unsupported {} => ComponentAvailability::Unsupported,
        CollisionComponent::Available { .. } => ComponentAvailability::Available,
        CollisionComponent::AvailableEmpty { .. } => ComponentAvailability::AvailableEmpty,
    }
}
/// Decode a complete owned inspection package from explicitly supplied assembled bytes.
/// This verifies whole files, not individual CAS chunks or active-world generation.
pub fn decode_static_scene_package(
    expected_zone: &str,
    expected_revision: &str,
    manifest: &Manifest,
    files: &[PackageFile<'_>],
    limits: &PackageDecodeLimits,
) -> Result<StaticScenePackage> {
    ensure!(
        manifest.files.len() <= limits.max_manifest_files,
        "manifest file work limit exceeded"
    );
    ensure!(
        files.len() <= limits.max_supplied_files,
        "supplied file limit exceeded"
    );
    let mut chunks = 0;
    for file in &manifest.files {
        charge(
            &mut chunks,
            file.chunks.len(),
            limits.max_chunk_references,
            "chunk reference",
        )?;
    }
    let mut supplied_paths = BTreeSet::new();
    let mut bytes = 0;
    for file in files {
        ensure!(
            valid_path(file.path) && supplied_paths.insert(file.path),
            "invalid or duplicate supplied path"
        );
        charge(
            &mut bytes,
            file.bytes.len(),
            limits.max_assembled_bytes,
            "assembled byte",
        )?;
        let limit = if file.path == "scene.json" {
            limits.max_descriptor_bytes
        } else {
            limits.max_component_bytes
        };
        ensure!(
            file.bytes.len() <= limit,
            "supplied file byte limit exceeded"
        );
    }
    ensure!(valid_token(expected_zone), "invalid expected zone");
    ensure!(
        valid_hash(expected_revision),
        "invalid expected manifest revision"
    );
    let set = format!("zone/{expected_zone}");
    validate_integrity(manifest, &set)?;
    ensure!(
        manifest.revision == expected_revision,
        "expected manifest revision mismatch"
    );
    let scene = files
        .iter()
        .find(|f| f.path == "scene.json")
        .context("scene.json bytes missing")?;
    let entry = manifest
        .files
        .iter()
        .find(|f| f.path == "scene.json")
        .context("scene.json entry missing")?;
    let descriptor_blake3 = blake3_hex(scene.bytes);
    ensure!(
        entry.size == u64::try_from(scene.bytes.len())? && entry.blake3 == descriptor_blake3,
        "descriptor content identity mismatch"
    );
    let d: SceneDescriptor = descriptor::decode(scene.bytes, limits)?;
    ensure!(
        d.zone == expected_zone && d.set == set,
        "descriptor requested identity mismatch"
    );
    let mut declared_paths = BTreeSet::from(["scene.json"]);
    for (file, _) in d.components.references() {
        declared_paths.insert(file.path.as_str());
    }
    ensure!(
        manifest.files.len() == declared_paths.len()
            && manifest
                .files
                .iter()
                .all(|f| declared_paths.contains(f.path.as_str()))
            && supplied_paths == declared_paths,
        "closed package file membership mismatch"
    );
    ensure!(
        manifest.requirements.reader_version == 2
            && capabilities_equal(
                &manifest.requirements.capabilities,
                &d.requirements.capabilities
            ),
        "manifest requirement union mismatch"
    );
    for (reference, _) in d.components.references() {
        let entry = manifest
            .files
            .iter()
            .find(|f| f.path == reference.path)
            .context("component entry missing")?;
        ensure!(
            entry.size == reference.size && entry.blake3 == reference.blake3,
            "component reference identity mismatch"
        );
    }
    if let VisualComponent::Available { file, .. } = &d.components.visual {
        ensure!(
            file.size <= u64::try_from(limits.visual.max_glb_bytes)?,
            "visual declared size limit exceeded"
        );
    }
    if let CollisionComponent::Available { file, .. }
    | CollisionComponent::AvailableEmpty { file, .. } = &d.components.collision
    {
        ensure!(
            file.size <= u64::try_from(limits.collision.max_glb_bytes)?,
            "collision declared size limit exceeded"
        );
    }
    // Every supplied whole file passes identity before the first GLB dependency is called.
    for file in files {
        let entry = manifest
            .files
            .iter()
            .find(|f| f.path == file.path)
            .context("file entry missing")?;
        ensure!(
            entry.size == u64::try_from(file.bytes.len())?
                && entry.blake3 == blake3_hex(file.bytes),
            "assembled file content identity mismatch"
        );
    }
    let mut visual_blake3 = None;
    let mut collision_blake3 = None;
    let visual = if let VisualComponent::Available {
        file, requirements, ..
    } = &d.components.visual
    {
        let bytes = files
            .iter()
            .find(|f| f.path == file.path)
            .context("visual bytes missing")?
            .bytes;
        let v = eqoxide_assets::decode_static_visual(bytes, &limits.visual)
            .context("decode package visual")?;
        ensure!(
            v.header.schema_version == d.schema_version
                && v.header.role == "visual"
                && v.header.coordinate_profile == d.coordinate_profile
                && v.header.unit_scale == d.unit_scale as f32,
            "visual header profile mismatch"
        );
        ensure!(
            v.header.bake_revision == d.bake_revision,
            "visual bake identity mismatch"
        );
        ensure!(
            v.header.requirements.reader_version == requirements.reader_version
                && capabilities_equal(
                    &v.header.requirements.capabilities,
                    &requirements.capabilities
                ),
            "visual header requirements mismatch"
        );
        visual_blake3 = Some(file.blake3.clone());
        Some(v)
    } else {
        None
    };
    let collision = if let CollisionComponent::Available {
        file,
        requirements,
        scope,
        query_policy,
    }
    | CollisionComponent::AvailableEmpty {
        file,
        requirements,
        scope,
        query_policy,
    } = &d.components.collision
    {
        let bytes = files
            .iter()
            .find(|f| f.path == file.path)
            .context("collision bytes missing")?
            .bytes;
        let c = eqoxide_assets::decode_static_collision(bytes, &limits.collision)
            .context("decode package collision")?;
        ensure!(
            c.header.schema_version == d.schema_version
                && c.header.role == "collision"
                && c.header.coordinate_profile == d.coordinate_profile
                && c.header.unit_scale == d.unit_scale as f32,
            "collision header profile mismatch"
        );
        ensure!(
            c.header.bake_revision == d.bake_revision,
            "collision bake identity mismatch"
        );
        ensure!(
            c.header.requirements.reader_version == requirements.reader_version
                && capabilities_equal(
                    &c.header.requirements.capabilities,
                    &requirements.capabilities
                ),
            "collision header requirements mismatch"
        );
        ensure!(
            c.header.collision_version == 1 && c.header.scope == *scope,
            "collision scope mismatch"
        );
        let empty = matches!(
            d.components.collision,
            CollisionComponent::AvailableEmpty { .. }
        );
        ensure!(
            (c.availability
                == eqoxide_assets::static_collision::CollisionAvailability::AvailableEmpty)
                == empty,
            "collision availability mismatch"
        );
        ensure!(
            c.query_policy == query_policy.decoded(),
            "collision policy mismatch"
        );
        collision_blake3 = Some(file.blake3.clone());
        Some(c)
    } else {
        None
    };
    Ok(StaticScenePackage {
        visual_availability: visual_state(&d.components.visual),
        collision_availability: collision_state(&d.components.collision),
        zone: d.zone,
        set: d.set,
        bake_revision: d.bake_revision,
        manifest_revision: manifest.revision.clone(),
        descriptor_blake3,
        visual_blake3,
        collision_blake3,
        visual,
        collision,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checked_work_counter_cannot_wrap() {
        let mut n = usize::MAX;
        assert!(charge(&mut n, 1, usize::MAX, "counter").is_err());
        let mut n = 0;
        charge(&mut n, 2, 2, "counter").unwrap();
        assert!(charge(&mut n, 1, 2, "counter").is_err());
    }
}
