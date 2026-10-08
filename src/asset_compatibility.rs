//! Manifest-envelope identity and supported asset reader contracts.
use crate::asset_sync::{set_digest, Manifest};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;
pub const READERS_HEADER: &str = "1";
pub const CAPABILITIES_HEADER: &str = "legacy-assets-v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderRequirements {
    pub reader_version: u32,
    pub capabilities: Vec<String>,
}

impl ReaderRequirements {
    pub fn legacy() -> Self {
        Self { reader_version: 1, capabilities: vec![CAPABILITIES_HEADER.into()] }
    }

    fn validate_encoding(&self) -> Result<()> {
        ensure!(self.reader_version > 0 && self.capabilities.len() <= 128, "invalid reader requirements");
        for cap in &self.capabilities {
            ensure!(cap.len() <= 128 && cap.bytes().next().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit()), "invalid capability token");
            ensure!(cap.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)), "invalid capability token");
        }
        let unique: std::collections::BTreeSet<_> = self.capabilities.iter().collect();
        let header_bytes = unique.iter().map(|cap| cap.len()).sum::<usize>() + unique.len().saturating_sub(1);
        ensure!(header_bytes <= 4096, "required capabilities exceed advertisement header capacity");
        Ok(())
    }

}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_path(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.contains(['\\', '\0', '\n', '\r', ':'])
        && value.split('/').all(|part| !part.is_empty() && part != "." && part != "..")
}

pub fn revision(manifest: &Manifest) -> String {
    let mut capabilities = manifest.requirements.capabilities.clone();
    capabilities.sort();
    capabilities.dedup();
    let mut files: Vec<_> = manifest.files.iter()
        .map(|f| (&f.path, f.size, &f.blake3, &f.chunks)).collect();
    files.sort_by(|a, b| a.0.cmp(b.0));
    let canonical = serde_json::to_vec(&(manifest.schema_version, &manifest.set, &manifest.digest,
        manifest.requirements.reader_version, capabilities, files)).expect("manifest scalars serialize");
    let mut hash = blake3::Hasher::new();
    hash.update(b"eqoxide-manifest-v1\0");
    hash.update(&canonical);
    hash.finalize().to_hex().to_string()
}

/// Validate before chunk access, assembled-file writes, or conditional cache reuse.
pub fn validate(manifest: &Manifest, requested_set: &str) -> Result<()> {
    ensure!(manifest.schema_version == SCHEMA_VERSION, "unsupported asset manifest schema {}", manifest.schema_version);
    ensure!(manifest.set == requested_set && valid_path(&manifest.set), "manifest requested set mismatch or invalid set");
    ensure!(valid_hash(&manifest.digest) && valid_hash(&manifest.revision), "invalid manifest identity encoding");
    manifest.requirements.validate_encoding()?;
    let mut paths = std::collections::HashSet::new();
    for file in &manifest.files {
        ensure!(valid_path(&file.path) && paths.insert(&file.path), "invalid or duplicate manifest file path");
        ensure!(valid_hash(&file.blake3) && file.chunks.iter().all(|h| valid_hash(h)), "invalid manifest content reference");
    }
    ensure!(set_digest(&manifest.files) == manifest.digest, "manifest digest mismatch for {requested_set}");
    ensure!(revision(manifest) == manifest.revision, "manifest revision mismatch for {requested_set}");
    ensure!(manifest.requirements.reader_version == 1
        && manifest.requirements.capabilities.iter().all(|c| c == CAPABILITIES_HEADER),
        "asset_reader_incompatible: requires reader {} capabilities {:?}; client supports readers [{}] capabilities [{}]",
        manifest.requirements.reader_version, manifest.requirements.capabilities, READERS_HEADER, CAPABILITIES_HEADER);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_sync::{blake3_hex, FileEntry};

    fn fixture() -> Manifest {
        let files = vec![FileEntry { path: "zone/a.glb".into(), size: 1,
            blake3: blake3_hex(b"a"), chunks: vec![blake3_hex(b"a")] }];
        let mut m = Manifest { schema_version: 1, revision: String::new(),
            requirements: ReaderRequirements::legacy(), set: "common".into(), digest: set_digest(&files), files };
        m.revision = revision(&m);
        m
    }

    #[test]
    fn shared_producer_golden_manifests_match_content_and_revision() {
        let legacy: Manifest = serde_json::from_str(include_str!("../tests/fixtures/manifest-v1.json")).unwrap();
        let future: Manifest = serde_json::from_str(include_str!("../tests/fixtures/manifest-reader2.json")).unwrap();
        validate(&legacy, &legacy.set).unwrap();
        assert_eq!(set_digest(&legacy.files), "4e686a7e59c0ec486ca4668f5b75e848f75c1048adbe191a5dbf7c2b1ee7a864");
        assert_eq!(revision(&legacy), "b21a853d6baa30fb182ac81a5cc5099bfa7002fd3344d2b2e592b74ed0116157");
        assert_eq!(legacy.digest, future.digest);
        assert_eq!(revision(&future), "cbc587b05a1010a5c2823ecd0a1cc9fbac4f42c0f849cf74afc052067347f865");
        assert!(validate(&future, &future.set).unwrap_err().to_string().contains("asset_reader_incompatible"));
    }

    #[test]
    fn requirement_header_capacity_uses_canonical_unique_tokens() {
        let mut requirements = ReaderRequirements { reader_version: 1,
            capabilities: (0..31).map(|i| format!("{i:02}{}", "a".repeat(126))).collect() };
        requirements.capabilities.push("z".repeat(97));
        assert_eq!(requirements.capabilities.join(",").len(), 4096);
        requirements.validate_encoding().unwrap();
        requirements.capabilities.push(requirements.capabilities[0].clone());
        requirements.validate_encoding().unwrap(); // duplicates add no canonical header bytes
        requirements.capabilities[31].push('z');
        assert!(requirements.validate_encoding().unwrap_err().to_string().contains("header capacity"));
    }

    #[test]
    fn strict_identity_requirements_and_requested_set() {
        let valid = fixture();
        validate(&valid, "common").unwrap();
        assert!(validate(&valid, "other").is_err());
        let changes: Vec<Box<dyn Fn(&mut Manifest)>> = vec![
            Box::new(|m| m.schema_version = 2),
            Box::new(|m| m.requirements.reader_version = 2),
            Box::new(|m| m.requirements.capabilities.push("unknown-feature".into())),
            Box::new(|m| m.files[0].path = "../escape".into()),
            Box::new(|m| m.files[0].chunks[0] = "../bad".into()),
            Box::new(|m| m.files.push(m.files[0].clone())),
            Box::new(|m| m.digest = "0".repeat(64)),
        ];
        for change in changes { let mut m = valid.clone(); change(&mut m); m.revision = revision(&m); assert!(validate(&m, "common").is_err()); }
        let mut m = valid.clone(); m.revision = "0".repeat(64); assert!(validate(&m, "common").is_err());
        let mut json = serde_json::to_value(valid).unwrap();
        for field in ["schema_version", "requirements", "revision"] {
            let old = json.as_object_mut().unwrap().remove(field).unwrap();
            assert!(serde_json::from_value::<Manifest>(json.clone()).is_err());
            json[field] = old;
        }
    }

    #[test]
    fn requirements_reject_unknown_semantics_but_allow_optional_manifest_metadata() {
        let mut json = serde_json::to_value(fixture()).unwrap();
        json["producer_note"] = serde_json::json!("optional provenance");
        assert!(serde_json::from_value::<Manifest>(json.clone()).is_ok());
        json["requirements"]["required_feature"] = serde_json::json!("future semantics");
        assert!(serde_json::from_value::<Manifest>(json).is_err());
    }

    #[test]
    fn canonical_revision_ignores_file_and_capability_order() {
        let mut a = fixture();
        let mut second = a.files[0].clone(); second.path = "other.glb".into();
        a.files.push(second); a.digest = set_digest(&a.files);
        a.requirements.capabilities = vec!["z-feature".into(), "a-feature".into()];
        let mut b = a.clone(); b.files.reverse(); b.requirements.capabilities.reverse();
        b.requirements.capabilities.push("z-feature".into());
        assert_eq!(revision(&a), revision(&b));
    }

    #[test]
    fn revision_covers_requirements_and_chunk_layout_without_changing_digest() {
        let original = fixture();
        let mut changed = original.clone();
        changed.requirements.capabilities.push("future-material".into());
        assert_ne!(revision(&changed), original.revision);
        assert_eq!(changed.digest, original.digest);
        changed = original.clone(); changed.files[0].size += 1;
        assert_ne!(revision(&changed), original.revision);
        changed = original.clone(); changed.files[0].chunks.push(blake3_hex(b"b"));
        assert_ne!(revision(&changed), original.revision);
        changed = original.clone(); changed.requirements.capabilities.push(CAPABILITIES_HEADER.into());
        assert_eq!(revision(&changed), original.revision);
    }
}
