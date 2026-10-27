//! Private wire schema pinned to the source-independent descriptor producer.
use super::PackageDecodeLimits;
use crate::asset_compatibility::{valid_hash, valid_path, valid_token, ReaderRequirements};
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use std::collections::BTreeSet;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(super) enum QueryPolicy {
    #[serde(rename = "inspection-segment-front-v1")]
    FrontFaces,
    #[serde(rename = "inspection-segment-both-v1")]
    BothFaces,
}
impl QueryPolicy {
    pub(super) fn decoded(self) -> eqoxide_assets::static_collision::InspectionQueryPolicy {
        match self {
            Self::FrontFaces => eqoxide_assets::static_collision::InspectionQueryPolicy::FrontFaces,
            Self::BothFaces => eqoxide_assets::static_collision::InspectionQueryPolicy::BothFaces,
        }
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SceneFile {
    pub(super) path: String,
    pub(super) blake3: String,
    pub(super) size: u64,
}
impl SceneFile {
    fn validate(&self, limits: &PackageDecodeLimits) -> Result<()> {
        ensure!(
            valid_path(&self.path) && self.path != "scene.json",
            "invalid component path"
        );
        ensure!(valid_hash(&self.blake3), "invalid component BLAKE3");
        ensure!(
            self.size > 0 && self.size <= u64::try_from(limits.max_component_bytes)?,
            "component byte limit exceeded"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "availability",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(super) enum VisualComponent {
    NotExtracted {},
    Unsupported {},
    Available {
        scope: String,
        file: SceneFile,
        requirements: ReaderRequirements,
    },
}
#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "availability",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(super) enum CollisionComponent {
    NotExtracted {},
    Unsupported {},
    Available {
        scope: String,
        file: SceneFile,
        requirements: ReaderRequirements,
        query_policy: QueryPolicy,
    },
    AvailableEmpty {
        scope: String,
        file: SceneFile,
        requirements: ReaderRequirements,
        query_policy: QueryPolicy,
    },
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SceneComponents {
    pub(super) visual: VisualComponent,
    pub(super) collision: CollisionComponent,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct SceneDescriptor {
    pub(super) schema_version: u32,
    pub(super) set: String,
    pub(super) zone: String,
    pub(super) scope: String,
    pub(super) coordinate_profile: String,
    pub(super) unit_scale: u32,
    pub(super) bake_revision: String,
    pub(super) requirements: ReaderRequirements,
    pub(super) required_operations: Vec<String>,
    pub(super) components: SceneComponents,
}
impl SceneDescriptor {
    pub(super) fn validate(&self, limits: &PackageDecodeLimits) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "unsupported descriptor schemaVersion"
        );
        ensure!(
            valid_token(&self.zone) && self.set == format!("zone/{}", self.zone),
            "descriptor set/zone mismatch"
        );
        ensure!(
            self.scope == "declared-static-inspection-v1",
            "unsupported descriptor scope"
        );
        ensure!(
            self.coordinate_profile == "eqoxide-static-y-up-v1" && self.unit_scale == 1,
            "unsupported descriptor coordinate/unit profile"
        );
        ensure!(
            valid_hash(&self.bake_revision),
            "invalid descriptor bakeRevision"
        );
        ensure!(
            self.required_operations.is_empty(),
            "inspection descriptor has no required operations"
        );
        let expected = component_union(&self.components, limits)?;
        self.requirements.validate_encoding()?;
        ensure!(
            self.requirements.reader_version == 2 && self.requirements.capabilities == expected,
            "descriptor requirement union mismatch"
        );
        Ok(())
    }
}
impl SceneComponents {
    pub(super) fn references(&self) -> impl Iterator<Item = (&SceneFile, &ReaderRequirements)> {
        let visual = match &self.visual {
            VisualComponent::Available {
                file, requirements, ..
            } => Some((file, requirements)),
            _ => None,
        };
        let collision = match &self.collision {
            CollisionComponent::Available {
                file, requirements, ..
            }
            | CollisionComponent::AvailableEmpty {
                file, requirements, ..
            } => Some((file, requirements)),
            _ => None,
        };
        visual.into_iter().chain(collision)
    }
}
fn role_requirements(
    requirements: &ReaderRequirements,
    required: &str,
    allowed: &[&str],
) -> Result<()> {
    requirements.validate_encoding()?;
    ensure!(
        requirements.reader_version == 2,
        "unsupported component reader contract"
    );
    let unique: BTreeSet<_> = requirements
        .capabilities
        .iter()
        .map(String::as_str)
        .collect();
    ensure!(
        unique.len() == requirements.capabilities.len(),
        "duplicate component capability"
    );
    ensure!(
        unique.contains(required) && unique.iter().all(|c| allowed.contains(c)),
        "unsupported component requirements"
    );
    Ok(())
}
fn component_union(
    components: &SceneComponents,
    limits: &PackageDecodeLimits,
) -> Result<Vec<String>> {
    if let VisualComponent::Available {
        scope,
        requirements,
        ..
    } = &components.visual
    {
        ensure!(
            scope == "declared-static-visual-scene-v1",
            "unsupported visual declared scope"
        );
        role_requirements(
            requirements,
            "static-visual-v1",
            &["static-visual-v1", "vertex-rgba-v1"],
        )?;
    }
    if let CollisionComponent::Available {
        scope,
        requirements,
        ..
    }
    | CollisionComponent::AvailableEmpty {
        scope,
        requirements,
        ..
    } = &components.collision
    {
        ensure!(
            scope == "declared-static-triangle-candidates-v1",
            "unsupported collision declared scope"
        );
        role_requirements(
            requirements,
            "static-collision-triangles-v1",
            &[
                "static-collision-triangles-v1",
                eqoxide_assets::static_collision::SOURCE_QUERY_FRAMES_PRESERVED_CAPABILITY,
            ],
        )?;
    }
    let mut paths = BTreeSet::new();
    let mut capabilities = BTreeSet::from(["static-scene-package-v1".to_string()]);
    for (file, requirements) in components.references() {
        file.validate(limits)?;
        ensure!(paths.insert(&file.path), "aliased component paths");
        capabilities.extend(requirements.capabilities.iter().cloned());
    }
    Ok(capabilities.into_iter().collect())
}

pub(super) fn decode(bytes: &[u8], limits: &PackageDecodeLimits) -> Result<SceneDescriptor> {
    ensure!(
        bytes.len() <= limits.max_descriptor_bytes,
        "descriptor byte limit exceeded"
    );
    let d: SceneDescriptor =
        serde_json::from_slice(bytes).context("invalid static scene descriptor")?;
    d.validate(limits)?;
    Ok(d)
}
