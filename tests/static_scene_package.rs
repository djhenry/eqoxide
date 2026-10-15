use eqoxide::{
    asset_compatibility,
    asset_sync::{blake3_hex, set_digest, Manifest},
    static_scene_package::*,
};
use eqoxide_assets::static_collision::QueryUnitFrames;
use serde_json::{json, Value};
const MANIFEST: &str = include_str!("fixtures/static-scene-package/manifest.json");
const SCENE: &[u8] = include_bytes!("fixtures/static-scene-package/scene.json");
const VISUAL: &[u8] = include_bytes!("fixtures/static-scene-package/visual.glb");
const COLLISION: &[u8] = include_bytes!("fixtures/static-scene-package/collision.glb");
struct Bundle {
    manifest: Manifest,
    files: Vec<(String, Vec<u8>)>,
}
impl Bundle {
    fn fixture() -> Self {
        Self {
            manifest: serde_json::from_str(MANIFEST).unwrap(),
            files: vec![
                ("scene.json".into(), SCENE.to_vec()),
                ("visual.glb".into(), VISUAL.to_vec()),
                ("collision.glb".into(), COLLISION.to_vec()),
            ],
        }
    }
    fn decode(&self) -> anyhow::Result<StaticScenePackage> {
        self.decode_with(&PackageDecodeLimits::default())
    }
    fn decode_with(&self, limits: &PackageDecodeLimits) -> anyhow::Result<StaticScenePackage> {
        let files: Vec<_> = self
            .files
            .iter()
            .map(|(p, b)| PackageFile { path: p, bytes: b })
            .collect();
        decode_static_scene_package(
            "authored",
            &self.manifest.revision,
            &self.manifest,
            &files,
            limits,
        )
    }
    fn descriptor(&self) -> Value {
        serde_json::from_slice(&self.files[0].1).unwrap()
    }
    fn replace_descriptor(&mut self, d: Value) {
        self.files[0].1 = serde_json::to_vec(&d).unwrap();
        self.rehash();
    }
    fn rehash(&mut self) {
        self.manifest.files = self
            .files
            .iter()
            .map(|(p, b)| eqoxide::asset_sync::FileEntry {
                path: p.clone(),
                size: b.len() as u64,
                blake3: blake3_hex(b),
                chunks: vec![blake3_hex(b)],
            })
            .collect();
        self.manifest.digest = set_digest(&self.manifest.files);
        self.manifest.revision = asset_compatibility::revision(&self.manifest);
    }
}
#[test]
fn actual_producer_pair_returns_owned_identity_and_role_geometry() {
    let package = {
        let b = Bundle::fixture();
        b.decode().unwrap()
    };
    assert_eq!(package.zone(), "authored");
    assert_eq!(package.set(), "zone/authored");
    assert_eq!(
        package.manifest_revision(),
        "1db88f7859e6bd421c9d27093dc38261debbd6e39dc2aeb4bfabaad0af09cd12"
    );
    assert_eq!(package.descriptor_blake3(), blake3_hex(SCENE));
    assert_eq!(
        package.visual_availability(),
        ComponentAvailability::Available
    );
    assert_eq!(
        package.collision_availability(),
        ComponentAvailability::Available
    );
    let v = package.visual().unwrap();
    let c = package.collision().unwrap();
    assert_eq!((v.meshes.len(), v.instances.len()), (1, 2));
    assert_eq!((c.meshes.len(), c.instances.len()), (1, 2));
    assert_eq!(v.header.bake_revision, c.header.bake_revision);
    assert_eq!(c.meshes[0].positions[1], [2., 0., 0.]);
    assert_eq!(c.header.query_unit_frames, QueryUnitFrames::Unspecified);
}
#[test]
fn both_unavailable_returns_honest_metadata_without_fake_decoded_components() {
    let mut b = Bundle::fixture();
    let mut d = b.descriptor();
    d["components"]["visual"] = json!({"availability":"not-extracted"});
    d["components"]["collision"] = json!({"availability":"unsupported"});
    d["requirements"]["capabilities"] = json!(["static-scene-package-v1"]);
    b.files.truncate(1);
    b.manifest.requirements.capabilities = vec!["static-scene-package-v1".into()];
    b.replace_descriptor(d);
    let package = b.decode().unwrap();
    assert_eq!(
        package.visual_availability(),
        ComponentAvailability::NotExtracted
    );
    assert_eq!(
        package.collision_availability(),
        ComponentAvailability::Unsupported
    );
    assert!(package.visual().is_none() && package.collision().is_none());
    assert!(package.visual_blake3().is_none() && package.collision_blake3().is_none());
}
fn parts(bytes: &[u8]) -> (Value, Option<Vec<u8>>) {
    let n = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    (
        serde_json::from_slice(&bytes[20..20 + n]).unwrap(),
        (20 + n < bytes.len()).then(|| bytes[28 + n..].to_vec()),
    )
}
fn glb(j: Value, bin: Option<Vec<u8>>) -> Vec<u8> {
    let mut j = serde_json::to_vec(&j).unwrap();
    while j.len() % 4 != 0 {
        j.push(b' ')
    }
    let n = 20 + j.len() + bin.as_ref().map_or(0, |b| 8 + b.len());
    let mut bytes = b"glTF".to_vec();
    bytes.extend(2u32.to_le_bytes());
    bytes.extend((n as u32).to_le_bytes());
    bytes.extend((j.len() as u32).to_le_bytes());
    bytes.extend(b"JSON");
    bytes.extend(j);
    if let Some(b) = bin {
        bytes.extend((b.len() as u32).to_le_bytes());
        bytes.extend(b"BIN\0");
        bytes.extend(b)
    }
    bytes
}
fn refresh(mut b: Bundle, mut d: Value) -> Bundle {
    let mut caps = std::collections::BTreeSet::from(["static-scene-package-v1".to_owned()]);
    for role in ["visual", "collision"] {
        if let Some(file) = d["components"][role].get_mut("file") {
            let path = file["path"].as_str().unwrap();
            let bytes = &b.files.iter().find(|(p, _)| p == path).unwrap().1;
            file["size"] = json!(bytes.len());
            file["blake3"] = json!(blake3_hex(bytes));
            for cap in d["components"][role]["requirements"]["capabilities"]
                .as_array()
                .unwrap()
            {
                caps.insert(cap.as_str().unwrap().into());
            }
        }
    }
    d["requirements"]["capabilities"] = json!(caps.iter().collect::<Vec<_>>());
    b.manifest.requirements.capabilities = caps.into_iter().collect();
    b.replace_descriptor(d);
    b
}
fn configured(v: &str, c: &str, policy: &str) -> Bundle {
    let mut b = Bundle::fixture();
    let mut d = b.descriptor();
    if v != "available" {
        d["components"]["visual"] = json!({"availability":v});
        b.files.retain(|(p, _)| p != "visual.glb");
    }
    if c == "available" || c == "available-empty" {
        let original = if c == "available-empty" {
            include_bytes!("../crates/eqoxide-assets/tests/fixtures/static-collision-empty-v1.glb")
                .as_slice()
        } else {
            COLLISION
        };
        let (mut j, bin) = parts(original);
        j["extras"]["eqoxideAsset"]["bakeRevision"] = d["bakeRevision"].clone();
        j["extras"]["eqoxideCollision"]["queryPolicy"] = json!(policy);
        b.files
            .iter_mut()
            .find(|(p, _)| p == "collision.glb")
            .unwrap()
            .1 = glb(j, bin);
        d["components"]["collision"]["availability"] = json!(c);
        d["components"]["collision"]["queryPolicy"] = json!(policy);
    } else {
        d["components"]["collision"] = json!({"availability":c});
        b.files.retain(|(p, _)| p != "collision.glb");
    }
    refresh(b, d)
}
#[test]
fn all_twelve_role_combinations_and_both_declared_policies_are_explicit() {
    for v in ["not-extracted", "unsupported", "available"] {
        for c in [
            "not-extracted",
            "unsupported",
            "available",
            "available-empty",
        ] {
            for policy in ["inspection-segment-front-v1", "inspection-segment-both-v1"] {
                let b = configured(v, c, policy);
                let p = b.decode().unwrap();
                assert_eq!(
                    serde_json::to_value(p.visual_availability()).unwrap(),
                    json!(v)
                );
                assert_eq!(
                    serde_json::to_value(p.collision_availability()).unwrap(),
                    json!(c)
                );
                assert_eq!(p.visual().is_some(), v == "available");
                assert_eq!(
                    p.collision().is_some(),
                    c == "available" || c == "available-empty"
                );
                if c == "available-empty" {
                    let collision = p.collision().unwrap();
                    assert!(
                        collision.meshes.is_empty()
                            && collision.instances.is_empty()
                            && collision.bounds.is_none()
                    );
                }
            }
        }
    }
}
#[test]
fn requested_zone_and_immutable_revision_are_mandatory_not_latest_discovery() {
    let b = Bundle::fixture();
    let files: Vec<_> = b
        .files
        .iter()
        .map(|(p, b)| PackageFile { path: p, bytes: b })
        .collect();
    for (zone, revision) in [
        ("other", b.manifest.revision.as_str()),
        ("authored", &"a".repeat(64)),
        ("UPPER", b.manifest.revision.as_str()),
        ("authored", "invalid"),
    ] {
        assert!(decode_static_scene_package(
            zone,
            revision,
            &b.manifest,
            &files,
            &PackageDecodeLimits::default()
        )
        .is_err());
    }
    assert!(b.decode().is_ok()); // The explicitly named immutable revision remains inspectable.
}
#[test]
fn rehashed_envelopes_cannot_mask_metadata_file_identity_or_membership() {
    for which in 0..10 {
        let mut b = Bundle::fixture();
        match which {
            0 => b.manifest.files[0].size += 1,
            1 => b.manifest.files[0].blake3 = "b".repeat(64),
            2 => b.manifest.files[1].size += 1,
            3 => b.manifest.files[1].blake3 = "b".repeat(64),
            4 => b.files[1].1[0] ^= 1,
            5 => {
                b.files[2].1.pop();
            }
            6 => {
                b.files.remove(1);
            }
            7 => b.files.push(("extra.glb".into(), vec![0])),
            8 => b.manifest.files.push(b.manifest.files[1].clone()),
            _ => b
                .manifest
                .requirements
                .capabilities
                .push("legacy-assets-v1".into()),
        }
        b.manifest.digest = set_digest(&b.manifest.files);
        b.manifest.revision = asset_compatibility::revision(&b.manifest);
        assert!(b.decode().is_err(), "corruption {which}");
    }
    let mut b = Bundle::fixture();
    b.manifest.digest = "b".repeat(64);
    b.manifest.revision = asset_compatibility::revision(&b.manifest);
    assert!(b.decode().is_err());
    let mut b = Bundle::fixture();
    b.manifest.revision = "b".repeat(64);
    assert!(b.decode().is_err());
    let mut b = Bundle::fixture();
    b.files[2].0 = "visual.glb".into();
    assert!(b.decode().is_err());
    let mut b = Bundle::fixture();
    let mut d = b.descriptor();
    d["components"]["collision"]["file"]["path"] = json!("visual.glb");
    b.replace_descriptor(d);
    assert!(b.decode().is_err());
    let mut b = configured("unsupported", "unsupported", "inspection-segment-front-v1");
    b.files.push(("visual.glb".into(), VISUAL.to_vec()));
    b.rehash();
    assert!(b.decode().is_err());
}
#[test]
fn descriptor_strict_types_refuse_unknown_missing_and_noncanonical_numbers() {
    for (key, value) in [
        ("schemaVersion", json!(2)),
        ("schemaVersion", json!(1.0)),
        ("unitScale", json!(1.0)),
        ("unitScale", json!(-1)),
        ("zone", json!("UPPER")),
        ("set", json!("zone/other")),
        ("scope", json!("unsupported")),
        ("coordinateProfile", json!("other")),
        ("bakeRevision", json!("A".repeat(64))),
        ("requiredOperations", json!(["static-segment-query-v1"])),
        ("unknown", json!(null)),
    ] {
        let mut b = Bundle::fixture();
        let mut d = b.descriptor();
        d[key] = value;
        b.replace_descriptor(d);
        assert!(b.decode().is_err(), "{key}");
    }
    for key in [
        "schemaVersion",
        "set",
        "zone",
        "scope",
        "coordinateProfile",
        "unitScale",
        "bakeRevision",
        "requirements",
        "requiredOperations",
        "components",
    ] {
        let mut b = Bundle::fixture();
        let mut d = b.descriptor();
        d.as_object_mut().unwrap().remove(key);
        b.replace_descriptor(d);
        assert!(b.decode().is_err(), "{key}");
    }
    for role in ["visual", "collision"] {
        let mut b = Bundle::fixture();
        let mut d = b.descriptor();
        d["components"].as_object_mut().unwrap().remove(role);
        b.replace_descriptor(d);
        assert!(b.decode().is_err());
    }
    for (role, field, value) in [
        ("visual", "availability", json!("available-empty")),
        ("collision", "availability", json!("missing")),
        ("collision", "queryPolicy", json!("unknown")),
        ("visual", "scope", json!("other")),
        ("collision", "scope", json!("other")),
    ] {
        let mut b = Bundle::fixture();
        let mut d = b.descriptor();
        d["components"][role][field] = value;
        b.replace_descriptor(d);
        assert!(b.decode().is_err());
    }
}
#[test]
fn unavailable_payload_refusals_have_valid_controls_and_duplicate_tokens_stay_raw() {
    for role in ["visual", "collision"] {
        for state in ["not-extracted", "unsupported"] {
            for field in ["file", "requirements", "scope", "queryPolicy", "extra"] {
                let mut b = if role == "visual" {
                    configured(state, "available", "inspection-segment-front-v1")
                } else {
                    configured("available", state, "inspection-segment-front-v1")
                };
                assert!(b.decode().is_ok());
                let mut d = b.descriptor();
                d["components"][role][field] = json!(null);
                b.replace_descriptor(d);
                assert!(b.decode().is_err());
            }
        }
    }
    let original = std::str::from_utf8(SCENE).unwrap();
    for (old, new) in [
        (
            "\"schemaVersion\":1",
            "\"schemaVersion\":2,\"schemaVersion\":1",
        ),
        (
            "\"schemaVersion\":1",
            "\"schemaVersion\":1,\"schema\\u0056ersion\":1",
        ),
        (
            "\"availability\":\"available\"",
            "\"availability\":\"unsupported\",\"availability\":\"available\"",
        ),
        (
            "\"path\":\"visual.glb\"",
            "\"path\":\"other\",\"path\":\"visual.glb\"",
        ),
        (
            "\"reader_version\":2",
            "\"reader_version\":3,\"reader_version\":2",
        ),
    ] {
        assert!(original.contains(old));
        let mut b = Bundle::fixture();
        b.files[0].1 = original.replacen(old, new, 1).into_bytes();
        b.rehash();
        assert!(b.decode().is_err());
    }
}
#[test]
fn requirements_reference_paths_and_declared_size_are_closed() {
    for (role, field, value) in [
        ("visual", "reader_version", json!(3)),
        ("collision", "reader_version", json!(1)),
        (
            "visual",
            "capabilities",
            json!(["static-visual-v1", "static-visual-v1"]),
        ),
        (
            "collision",
            "capabilities",
            json!(["static-segment-query-v1"]),
        ),
    ] {
        let mut b = Bundle::fixture();
        let mut d = b.descriptor();
        d["components"][role]["requirements"][field] = value;
        b.replace_descriptor(d);
        assert!(b.decode().is_err());
    }
    for path in ["scene.json", "../x", "a//x", "/x", "a\\x", ""] {
        let mut b = Bundle::fixture();
        let mut d = b.descriptor();
        d["components"]["visual"]["file"]["path"] = json!(path);
        b.replace_descriptor(d);
        assert!(b.decode().is_err());
    }
    for size in [json!(0), json!(134217729u64), json!(2004.0)] {
        let mut b = Bundle::fixture();
        let mut d = b.descriptor();
        d["components"]["visual"]["file"]["size"] = size;
        b.replace_descriptor(d);
        assert!(b.decode().is_err());
    }
    for caps in [
        json!(["static-scene-package-v1"]),
        json!([
            "static-collision-triangles-v1",
            "static-scene-package-v1",
            "static-visual-v1",
            "vertex-rgba-v1",
            "vertex-rgba-v1"
        ]),
    ] {
        let mut b = Bundle::fixture();
        let mut d = b.descriptor();
        d["requirements"]["capabilities"] = caps;
        b.replace_descriptor(d);
        assert!(b.decode().is_err());
    }
}
#[test]
fn valid_role_decoding_cannot_discharge_bake_requirements_availability_or_policy_pairing() {
    for role in ["visual", "collision"] {
        let mut b = Bundle::fixture();
        let index = if role == "visual" { 1 } else { 2 };
        let (mut j, bin) = parts(&b.files[index].1);
        j["extras"]["eqoxideAsset"]["bakeRevision"] = json!("b".repeat(64));
        b.files[index].1 = glb(j, bin);
        if role == "visual" {
            eqoxide_assets::decode_static_visual(&b.files[index].1, &Default::default()).unwrap();
        } else {
            eqoxide_assets::decode_static_collision(&b.files[index].1, &Default::default())
                .unwrap();
        }
        let d = b.descriptor();
        let b = refresh(b, d);
        assert!(b.decode().is_err(), "bake {role}");
    }
    let b = Bundle::fixture();
    let mut d = b.descriptor();
    d["components"]["visual"]["requirements"]["capabilities"] = json!(["static-visual-v1"]);
    let b = refresh(b, d);
    eqoxide_assets::decode_static_visual(&b.files[1].1, &Default::default()).unwrap();
    assert!(b.decode().is_err());
    for (key, value) in [
        ("availability", "available-empty"),
        ("queryPolicy", "inspection-segment-both-v1"),
    ] {
        let mut b = Bundle::fixture();
        let mut d = b.descriptor();
        d["components"]["collision"][key] = json!(value);
        b.replace_descriptor(d);
        eqoxide_assets::decode_static_collision(&b.files[2].1, &Default::default()).unwrap();
        assert!(b.decode().is_err(), "{key}");
    }
}
#[test]
fn malformed_and_role_swapped_hash_matched_files_are_errors_without_fallback() {
    for bytes in [b"not a GLB".as_slice(), COLLISION] {
        let mut b = Bundle::fixture();
        b.files[1].1 = bytes.to_vec();
        let d = b.descriptor();
        let b = refresh(b, d);
        assert!(b.decode().is_err());
    }
    let mut b = Bundle::fixture();
    b.files[2].1 = VISUAL.to_vec();
    let d = b.descriptor();
    let b = refresh(b, d);
    assert!(b.decode().is_err());
    let good = Bundle::fixture().decode().unwrap();
    let original = good.manifest_revision().to_owned();
    let mut bad = Bundle::fixture();
    bad.files[2].1[0] ^= 1;
    assert!(bad.decode().is_err());
    assert_eq!(good.manifest_revision(), original);
    assert!(good.visual().is_some() && good.collision().is_some());
}
#[test]
fn exact_budgets_and_ordering_preflight_before_hashing_or_role_decoding() {
    let b = Bundle::fixture();
    let mut l = PackageDecodeLimits::default();
    l.max_descriptor_bytes = SCENE.len();
    assert!(b.decode_with(&l).is_ok());
    l.max_descriptor_bytes -= 1;
    assert!(b.decode_with(&l).is_err());
    let mut component = PackageDecodeLimits::default();
    component.max_component_bytes = VISUAL.len();
    assert!(b.decode_with(&component).is_ok());
    component.max_component_bytes -= 1;
    assert!(b.decode_with(&component).is_err());
    let sum = b.files.iter().map(|(_, b)| b.len()).sum::<usize>();
    let mut l = PackageDecodeLimits::default();
    l.max_assembled_bytes = sum;
    assert!(b.decode_with(&l).is_ok());
    l.max_assembled_bytes -= 1;
    assert!(b.decode_with(&l).is_err());
    for (kind, l) in [
        ("supplied file", {
            let mut l = PackageDecodeLimits::default();
            l.max_supplied_files = 2;
            l
        }),
        ("manifest file", {
            let mut l = PackageDecodeLimits::default();
            l.max_manifest_files = 2;
            l
        }),
        ("chunk reference", {
            let mut l = PackageDecodeLimits::default();
            l.max_chunk_references = 2;
            l
        }),
        ("assembled byte", {
            let mut l = PackageDecodeLimits::default();
            l.max_assembled_bytes = 0;
            l
        }),
    ] {
        let mut bad = Bundle::fixture();
        bad.manifest.revision = "invalid".into();
        let error = bad.decode_with(&l).unwrap_err().to_string();
        assert!(error.contains(kind), "{error}");
    }
    let mut l = PackageDecodeLimits::default();
    l.visual.max_resources = 0;
    assert!(b.decode_with(&l).is_err());
    let mut l = PackageDecodeLimits::default();
    l.collision.max_resources = 0;
    assert!(b.decode_with(&l).is_err());
    let mut bad = Bundle::fixture();
    bad.files[2].1[0] ^= 1;
    let mut l = PackageDecodeLimits::default();
    l.visual.max_resources = 0;
    let error = bad.decode_with(&l).unwrap_err().to_string();
    assert!(
        error.contains("content identity"),
        "all bytes before first role: {error}"
    );
}
#[test]
fn legacy_gate_stays_closed_and_optional_manifest_metadata_keeps_old_semantics() {
    let b = Bundle::fixture();
    assert!(asset_compatibility::validate(&b.manifest, "zone/authored")
        .unwrap_err()
        .to_string()
        .contains("asset_reader_incompatible"));
    let mut json: Value = serde_json::from_str(MANIFEST).unwrap();
    json["producer_note"] = json!({"ignored":true});
    let m: Manifest = serde_json::from_value(json).unwrap();
    assert_eq!(asset_compatibility::revision(&m), b.manifest.revision);
    let mut b = Bundle::fixture();
    b.manifest.requirements.capabilities.reverse();
    b.manifest
        .requirements
        .capabilities
        .push("static-scene-package-v1".into());
    assert_eq!(
        asset_compatibility::revision(&b.manifest),
        b.manifest.revision
    );
    assert!(b.decode().is_ok());
    let mut b = Bundle::fixture();
    let mut d = b.descriptor();
    let caps = d["components"]["visual"]["requirements"]["capabilities"]
        .as_array_mut()
        .unwrap();
    caps.reverse();
    b.replace_descriptor(d);
    assert!(b.decode().is_ok());
    assert_eq!(asset_compatibility::READERS_HEADER, "1");
    assert_eq!(asset_compatibility::CAPABILITIES_HEADER, "legacy-assets-v1");
}

#[test]
fn valid_changed_component_bytes_and_descriptor_zone_isolate_identity_guards() {
    let mut b = Bundle::fixture();
    let needle = b"authored-floor";
    let offset = b.files[1]
        .1
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap();
    b.files[1].1[offset] = b'b';
    eqoxide_assets::decode_static_visual(&b.files[1].1, &Default::default()).unwrap();
    assert!(b.decode().is_err()); // Valid same-size bytes differ from the recorded whole-file digest.
    b.rehash();
    assert!(b.decode().is_err()); // Actual bytes now match manifest, but descriptor reference is stale.
    let mut b = Bundle::fixture();
    let mut d = b.descriptor();
    d["zone"] = json!("other");
    d["set"] = json!("zone/other");
    b.replace_descriptor(d);
    assert!(b.decode().is_err()); // Each identity is well formed; their association is wrong.
}

#[test]
fn declared_root_union_is_checked_even_when_manifest_agrees_with_wrong_union() {
    let mut b = Bundle::fixture();
    let mut d = b.descriptor();
    d["requirements"]["capabilities"] = json!(["static-scene-package-v1"]);
    b.manifest.requirements.capabilities = vec!["static-scene-package-v1".into()];
    b.replace_descriptor(d);
    assert!(b.decode().is_err());
}
#[test]
fn explicit_role_references_have_no_filename_extension_or_collision_name_authority() {
    let mut b = Bundle::fixture();
    let mut d = b.descriptor();
    b.files[1].0 = "opaque/one.payload".into();
    b.files[2].0 = "unrelated/two.bin".into();
    d["components"]["visual"]["file"]["path"] = json!("opaque/one.payload");
    d["components"]["collision"]["file"]["path"] = json!("unrelated/two.bin");
    let b = refresh(b, d);
    let p = b.decode().unwrap();
    assert!(p.visual().is_some() && p.collision().is_some());
}
#[test]
fn role_geometry_bounds_and_instance_counts_need_not_equal() {
    let mut b = Bundle::fixture();
    let (mut j, bin) = parts(&b.files[1].1);
    j["nodes"].as_array_mut().unwrap().pop();
    j["scenes"][0]["nodes"] = json!([0]);
    b.files[1].1 = glb(j, bin);
    eqoxide_assets::decode_static_visual(&b.files[1].1, &Default::default()).unwrap();
    let d = b.descriptor();
    let b = refresh(b, d);
    let p = b.decode().unwrap();
    let v = p.visual().unwrap();
    let c = p.collision().unwrap();
    assert_eq!(v.instances.len(), 1);
    assert_eq!(c.instances.len(), 2);
    assert_ne!(v.bounds.max, c.bounds.as_ref().unwrap().max);
}
#[test]
fn deep_descriptor_input_and_known_manifest_duplicate_keys_refuse_without_metadata_policy_change() {
    let mut b = Bundle::fixture();
    let original = String::from_utf8(b.files[0].1.clone()).unwrap();
    let nested = format!("\"scope\":{}0{}", "[".repeat(256), "]".repeat(256));
    b.files[0].1 = original
        .replacen("\"scope\":\"declared-static-visual-scene-v1\"", &nested, 1)
        .into_bytes();
    b.rehash();
    assert!(b.decode().is_err());
    let raw = MANIFEST.replacen(
        "\"schema_version\": 1",
        "\"schema_version\": 1, \"schema_version\": 1",
        1,
    );
    assert_ne!(raw, MANIFEST);
    assert!(serde_json::from_str::<Manifest>(&raw).is_err());
    let mut j: Value = serde_json::from_str(MANIFEST).unwrap();
    j["files"][0]["inert_note"] = json!({"ignored":"metadata"});
    let m: Manifest = serde_json::from_value(j).unwrap();
    assert_eq!(
        asset_compatibility::revision(&m),
        Bundle::fixture().manifest.revision
    );
}

fn canonical_query_units_bundle(empty: bool) -> Bundle {
    let mut b = configured(
        "available",
        if empty { "available-empty" } else { "available" },
        "inspection-segment-front-v1",
    );
    let caps = json!(["static-collision-query-unit-frames-v1", "static-collision-triangles-v1"]);
    let (mut j, bin) = parts(&b.files[2].1);
    j["extras"]["eqoxideAsset"]["requirements"]["capabilities"] = caps.clone();
    b.files[2].1 = glb(j, bin);
    let mut d = b.descriptor();
    d["components"]["collision"]["requirements"]["capabilities"] = caps;
    refresh(b, d)
}
#[test]
fn canonical_query_units_available_and_known_empty_bind_all_layers() {
    for empty in [false, true] {
        let b = canonical_query_units_bundle(empty);
        let p = b.decode().unwrap();
        assert_eq!(
            p.collision().unwrap().header.query_unit_frames,
            QueryUnitFrames::DeclaredCanonical
        );
        assert_eq!(
            p.collision().unwrap().header.requirements.capabilities,
            vec![
                "static-collision-query-unit-frames-v1",
                "static-collision-triangles-v1"
            ]
        );
        assert_eq!(
            p.collision_availability(),
            if empty {
                ComponentAvailability::AvailableEmpty
            } else {
                ComponentAvailability::Available
            }
        );
    }
}

#[test]
fn canonical_query_units_fresh_hashes_cannot_mask_layer_disagreement() {
    for empty in [false, true] {
        for canonical in [false, true] {
            for layer in ["glb", "descriptor", "manifest"] {
                let mut b = if canonical {
                    canonical_query_units_bundle(empty)
                } else {
                    configured(
                        "available",
                        if empty { "available-empty" } else { "available" },
                        "inspection-segment-front-v1",
                    )
                };
                assert!(b.decode().is_ok());
                let caps = if canonical {
                    json!(["static-collision-triangles-v1"])
                } else {
                    json!([
                        "static-collision-query-unit-frames-v1",
                        "static-collision-triangles-v1"
                    ])
                };
                if layer == "glb" {
                    let (mut j, bin) = parts(&b.files[2].1);
                    j["extras"]["eqoxideAsset"]["requirements"]["capabilities"] = caps;
                    b.files[2].1 = glb(j, bin);
                    eqoxide_assets::decode_static_collision(&b.files[2].1, &Default::default())
                        .unwrap();
                    let d = b.descriptor();
                    b = refresh(b, d);
                } else if layer == "descriptor" {
                    let mut d = b.descriptor();
                    d["components"]["collision"]["requirements"]["capabilities"] = caps;
                    b = refresh(b, d);
                } else {
                    let caps = &mut b.manifest.requirements.capabilities;
                    if canonical {
                        caps.retain(|c| c != "static-collision-query-unit-frames-v1");
                    } else {
                        caps.push("static-collision-query-unit-frames-v1".into());
                    }
                    b.manifest.revision = asset_compatibility::revision(&b.manifest);
                }
                let error = b.decode().unwrap_err().to_string();
                assert!(
                    error.contains("requirement"),
                    "{empty}/{canonical}/{layer}: {error}"
                );
                assert!(!error.contains("content identity"), "{error}");
            }
        }
    }
}
#[test]
fn canonical_query_units_component_order_and_invalid_declarations() {
    let mut b = canonical_query_units_bundle(false);
    let mut d = b.descriptor();
    d["components"]["collision"]["requirements"]["capabilities"]
        .as_array_mut()
        .unwrap()
        .reverse();
    b.replace_descriptor(d);
    assert!(b.decode().is_ok());
    for caps in [
        json!(["static-collision-query-unit-frames-v1"]),
        json!([
            "static-collision-query-unit-frames-v1",
            "static-collision-triangles-v1",
            "static-collision-triangles-v1"
        ]),
        json!(["static-collision-triangles-v1", "unknown-capability-v1"]),
    ] {
        let b = canonical_query_units_bundle(false);
        let mut d = b.descriptor();
        d["components"]["collision"]["requirements"]["capabilities"] = caps;
        let b = refresh(b, d);
        assert!(b.decode().is_err());
    }
    let mut b = configured("available", "unsupported", "inspection-segment-front-v1");
    assert!(b.decode().unwrap().collision().is_none());
    let mut d = b.descriptor();
    let caps = d["requirements"]["capabilities"].as_array_mut().unwrap();
    caps.push(json!("static-collision-query-unit-frames-v1"));
    caps.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    b.manifest
        .requirements
        .capabilities
        .push("static-collision-query-unit-frames-v1".into());
    b.replace_descriptor(d);
    assert!(b.decode().is_err());
    let b = canonical_query_units_bundle(false);
    let mut d = b.descriptor();
    d["components"]["visual"]["requirements"]["capabilities"]
        .as_array_mut()
        .unwrap()
        .push(json!("static-collision-query-unit-frames-v1"));
    let b = refresh(b, d);
    assert!(b.decode().is_err());
}
