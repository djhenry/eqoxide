use eqoxide_assets::static_collision::{
    decode_static_collision, CollisionAvailability, CollisionDecodeLimits, InspectionQueryPolicy,
    QueryUnitFrames,
};
use serde_json::{json, Value};
fn glb(j: Value, bin: Option<Vec<u8>>) -> Vec<u8> {
    let mut j = serde_json::to_vec(&j).unwrap();
    while j.len() % 4 != 0 {
        j.push(b' ');
    }
    let len = 20 + j.len() + bin.as_ref().map_or(0, |b| 8 + b.len());
    let mut out = b"glTF".to_vec();
    out.extend(2u32.to_le_bytes());
    out.extend((len as u32).to_le_bytes());
    out.extend((j.len() as u32).to_le_bytes());
    out.extend(b"JSON");
    out.extend(j);
    if let Some(b) = bin {
        out.extend((b.len() as u32).to_le_bytes());
        out.extend(b"BIN\0");
        out.extend(b);
    }
    out
}
fn empty_json() -> Value {
    json!({"asset":{"version":"2.0"},"scene":0,"scenes":[{}],"extras":{
        "eqoxideAsset":{"schemaVersion":1,"role":"collision","coordinateProfile":"eqoxide-static-y-up-v1","unitScale":1,
          "bakeRevision":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          "requirements":{"reader_version":2,"capabilities":["static-collision-triangles-v1"]}},
        "eqoxideCollision":{"version":1,"scope":"declared-static-triangle-candidates-v1","availability":"available-empty",
            "selectedNodes":[],"queryPolicy":"inspection-segment-front-v1"}}})
}
#[test]
fn explicit_known_empty_has_no_dummy_geometry_or_bounds() {
    let data = decode_static_collision(&glb(empty_json(), None), &CollisionDecodeLimits::default())
        .unwrap();
    assert_eq!(data.availability, CollisionAvailability::AvailableEmpty);
    assert_eq!(data.query_policy, InspectionQueryPolicy::FrontFaces);
    assert!(data.meshes.is_empty() && data.instances.is_empty() && data.bounds.is_none());
}

const FIXTURE: &[u8] = include_bytes!("fixtures/static-collision-v1.glb");
const EMPTY: &[u8] = include_bytes!("fixtures/static-collision-empty-v1.glb");
fn parts(bytes: &[u8]) -> (Value, Option<Vec<u8>>) {
    let n = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    (
        serde_json::from_slice(&bytes[20..20 + n]).unwrap(),
        (20 + n < bytes.len()).then(|| bytes[28 + n..].to_vec()),
    )
}
fn edited(bytes: &[u8], f: impl FnOnce(&mut Value, &mut Option<Vec<u8>>)) -> Vec<u8> {
    let (mut j, mut b) = parts(bytes);
    f(&mut j, &mut b);
    glb(j, b)
}
fn rejected(bytes: &[u8]) {
    assert!(decode_static_collision(bytes, &CollisionDecodeLimits::default()).is_err());
}
#[test]
fn producer_nonempty_and_empty_are_distinct_declared_datasets() {
    let s = decode_static_collision(FIXTURE, &CollisionDecodeLimits::default()).unwrap();
    assert_eq!(s.availability, CollisionAvailability::Available);
    assert_eq!(s.meshes.len(), 1);
    assert_eq!(s.instances.len(), 2);
    assert!(s.bounds.is_some());
    let e = decode_static_collision(EMPTY, &CollisionDecodeLimits::default()).unwrap();
    assert_eq!(e.availability, CollisionAvailability::AvailableEmpty);
    assert_eq!(e.query_policy, InspectionQueryPolicy::BothFaces);
    assert!(e.meshes.is_empty() && e.instances.is_empty() && e.bounds.is_none());
}
#[test]
fn known_empty_requires_omitted_core_resources_and_absent_bin() {
    for key in ["nodes", "meshes", "accessors", "bufferViews", "buffers"] {
        rejected(&edited(EMPTY, |j, _| j[key] = json!([])));
    }
    rejected(&edited(EMPTY, |j, _| j["scenes"][0]["nodes"] = json!([])));
    rejected(&edited(EMPTY, |_, b| *b = Some(vec![])));
    rejected(&edited(EMPTY, |j, _| {
        j["extras"]["eqoxideCollision"]["selectedNodes"] = json!([0])
    }));
    rejected(&edited(FIXTURE, |_, b| *b = None));
}
#[test]
fn source_and_visual_features_have_no_collision_authority() {
    for key in [
        "materials",
        "images",
        "textures",
        "animations",
        "skins",
        "extensions",
        "samplers",
    ] {
        rejected(&edited(FIXTURE, |j, _| j[key] = json!([])));
    }
    rejected(&edited(FIXTURE, |j, _| {
        j["meshes"][0]["primitives"][0]["attributes"]["NORMAL"] = json!(0)
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["nodes"][0]["name"] = json!("__collision__")
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["buffers"][0]["uri"] = json!("file:///private")
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["extras"]["eqoxideAsset"]["role"] = json!("visual")
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["extras"]["eqoxideAsset"]["requirements"]["capabilities"] =
            json!(["static-collision-triangles-v1", "static-segment-query-v1"])
    }));
}
#[test]
fn owner_identity_is_required_unique_opaque_and_schema_strict() {
    rejected(&edited(FIXTURE, |j, _| {
        j["nodes"][0].as_object_mut().unwrap().remove("extras");
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["nodes"][1]["extras"] = j["nodes"][0]["extras"].clone()
    }));
    for owner in ["", "UPPER", "a/b", "-prefix", "a with space"] {
        rejected(&edited(FIXTURE, |j, _| {
            j["nodes"][0]["extras"]["eqoxideCollisionInstance"]["ownerId"] = json!(owner)
        }));
    }
    rejected(&edited(FIXTURE, |j, _| {
        j["nodes"][0]["extras"]["eqoxideCollisionInstance"]["solid"] = json!(true)
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["extras"]["eqoxideCollision"]["queryPolicy"] = json!("native-movement")
    }));
}
#[test]
fn scene_and_application_selection_cover_every_flat_node_once() {
    for nodes in [json!([0]), json!([0, 0]), json!([0, 2]), json!([0.0, 1])] {
        rejected(&edited(FIXTURE, |j, _| {
            j["scenes"][0]["nodes"] = nodes.clone()
        }));
        rejected(&edited(FIXTURE, |j, _| {
            j["extras"]["eqoxideCollision"]["selectedNodes"] = nodes
        }));
    }
    rejected(&edited(FIXTURE, |j, _| {
        j["nodes"][0]["children"] = json!([1])
    }));
}
#[test]
fn collision_descriptor_types_and_bindings_are_strict_even_when_unused() {
    rejected(&edited(FIXTURE, |j, _| {
        j["accessors"][1]["componentType"] = json!(5123)
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["bufferViews"][0]["byteStride"] = json!(12)
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["accessors"][0]["min"] = json!([0, 0, -2])
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["accessors"][0].as_object_mut().unwrap().remove("max");
    }));
    rejected(&edited(FIXTURE, |j, _| {
        let mut a = j["accessors"][0].clone();
        a["type"] = json!("VEC2");
        j["accessors"].as_array_mut().unwrap().push(a);
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["meshes"][0]["primitives"][0]["mode"] = json!(5)
    }));
    rejected(&edited(FIXTURE, |j, b| {
        j["accessors"][1]["count"] = json!(3);
        b.as_mut().unwrap()[48..52].copy_from_slice(&4u32.to_le_bytes());
    }));
}
#[test]
fn every_declared_mesh_triangle_is_validated_even_if_unused() {
    rejected(&edited(FIXTURE, |j, b| {
        let mut m = j["meshes"][0].clone();
        m["primitives"][0]["indices"] = json!(2);
        j["meshes"].as_array_mut().unwrap().push(m);
        let mut a = j["accessors"][1].clone();
        a["count"] = json!(3);
        a["byteOffset"] = json!(24);
        a["bufferView"] = json!(2);
        j["accessors"].as_array_mut().unwrap().push(a);
        j["bufferViews"]
            .as_array_mut()
            .unwrap()
            .push(json!({"buffer":0,"byteOffset":48,"byteLength":36,"target":34963}));
        let b = b.as_mut().unwrap();
        b.extend([0u32, 0, 0].into_iter().flat_map(u32::to_le_bytes));
        j["buffers"][0]["byteLength"] = json!(b.len());
    }));
}
#[test]
fn local_and_instanced_rounding_collapse_are_refused() {
    rejected(&edited(FIXTURE, |_, b| {
        b.as_mut().unwrap()[52..56].copy_from_slice(&0u32.to_le_bytes())
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["nodes"][0]["matrix"][12] = json!(1e20)
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["nodes"][0]["matrix"][0] = json!(-1)
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["nodes"][0]["matrix"][0] = json!(2)
    }));
    rejected(&edited(FIXTURE, |j, _| {
        j["nodes"][0]["matrix"][3] = json!(1)
    }));
}
#[test]
fn every_truncated_prefix_is_refused_without_mutating_input() {
    for source in [FIXTURE, EMPTY] {
        for n in 0..source.len() {
            rejected(&source[..n]);
        }
    }
    let before = FIXTURE.to_vec();
    let _ = decode_static_collision(FIXTURE, &CollisionDecodeLimits::default());
    assert_eq!(before, FIXTURE);
}
fn positions_fixture(points: &[[f32; 3]]) -> Vec<u8> {
    edited(FIXTURE, |j, b| {
        let old = b.take().unwrap();
        let mut bytes = Vec::new();
        for p in points {
            for f in p {
                bytes.extend(f.to_le_bytes());
            }
        }
        let offset = bytes.len();
        bytes.extend(&old[48..]);
        j["accessors"][0]["count"] = json!(points.len());
        let min = std::array::from_fn::<_, 3, _>(|k| {
            points.iter().map(|p| p[k]).fold(f32::INFINITY, f32::min)
        });
        let max = std::array::from_fn::<_, 3, _>(|k| {
            points
                .iter()
                .map(|p| p[k])
                .fold(f32::NEG_INFINITY, f32::max)
        });
        j["accessors"][0]["min"] = json!(min);
        j["accessors"][0]["max"] = json!(max);
        j["bufferViews"][0]["byteLength"] = json!(offset);
        j["bufferViews"][1]["byteOffset"] = json!(offset);
        j["buffers"][0]["byteLength"] = json!(bytes.len());
        *b = Some(bytes);
    })
}
#[test]
fn regenerated_fixture_preserves_axes_winding_shared_pool_owners_and_transform() {
    let s = decode_static_collision(FIXTURE, &CollisionDecodeLimits::default()).unwrap();
    assert_eq!(s.header.role, "collision");
    assert_eq!(s.header.collision_version, 1);
    assert_eq!(
        s.header.requirements.capabilities,
        vec!["static-collision-triangles-v1"]
    );
    assert_eq!(
        s.meshes[0].positions,
        vec![[0., 0., 0.], [2., 0., 0.], [0., 3., 0.], [2., 3., 0.]]
    );
    assert_eq!(s.meshes[0].primitives[0].indices, vec![0, 1, 2, 1, 3, 2]);
    assert_eq!(
        s.instances.iter().map(|i| i.mesh_index).collect::<Vec<_>>(),
        vec![0, 0]
    );
    assert_eq!(s.instances[0].owner_id, "authored-floor");
    assert_eq!(s.instances[1].owner_id, "authored-platform");
    let matrix = glam::Mat4::from_cols_array_2d(&s.instances[1].matrix);
    let p = matrix
        .transform_point3(glam::Vec3::new(2., 3., 0.))
        .to_array();
    for (actual, expected) in p.into_iter().zip([4., 24., 5.]) {
        assert!((actual - expected).abs() < 1e-5);
    }
    let bounds = s.bounds.unwrap();
    assert_eq!(bounds.min, [0., 0., 0.]);
    for (actual, expected) in bounds.max.into_iter().zip([10., 24., 5.]) {
        assert!((actual - expected).abs() < 1e-5);
    }
}
#[test]
fn finite_unused_vertices_never_expand_surface_bounds_or_overflow_world_checks() {
    let b = positions_fixture(&[
        [0., 0., 0.],
        [2., 0., 0.],
        [0., 0., -3.],
        [2., 0., -3.],
        [f32::MAX, 0., 0.],
    ]);
    let s = decode_static_collision(&b, &CollisionDecodeLimits::default()).unwrap();
    assert_eq!(s.meshes[0].positions.len(), 5);
    assert_eq!(s.bounds.as_ref().unwrap().min, [0., 0., 0.]);
    assert!(s.bounds.unwrap().max[0] < 11.);
    rejected(&edited(&b, |j, b| {
        let offset = j["bufferViews"][1]["byteOffset"].as_u64().unwrap() as usize;
        b.as_mut().unwrap()[offset + 4..offset + 8].copy_from_slice(&4u32.to_le_bytes());
    }));
}
#[test]
fn near_degenerate_nonzero_triangles_use_widened_cross_products() {
    let b = positions_fixture(&[
        [0., 0., 0.],
        [1e-25, 0., 0.],
        [0., 0., -1e-25],
        [1e-25, 0., -1e-25],
    ]);
    let b = edited(&b, |j, _| {
        j["nodes"][1]["matrix"] = j["nodes"][0]["matrix"].clone()
    });
    let s = decode_static_collision(&b, &CollisionDecodeLimits::default()).unwrap();
    assert!(s.bounds.unwrap().max[0] > 0.);
}
fn raw_json_glb(bytes: &[u8], from: &str, to: &str) -> Vec<u8> {
    let n = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let raw = std::str::from_utf8(&bytes[20..20 + n])
        .unwrap()
        .replace(from, to);
    let mut json = raw.trim_end().as_bytes().to_vec();
    while json.len() % 4 != 0 {
        json.push(b' ');
    }
    let (_, bin) = parts(bytes);
    let mut out = b"glTF".to_vec();
    out.extend(2u32.to_le_bytes());
    out.extend(((20 + json.len() + bin.as_ref().map_or(0, |b| 8 + b.len())) as u32).to_le_bytes());
    out.extend((json.len() as u32).to_le_bytes());
    out.extend(b"JSON");
    out.extend(json);
    if let Some(b) = bin {
        out.extend((b.len() as u32).to_le_bytes());
        out.extend(b"BIN\0");
        out.extend(b);
    }
    out
}
#[test]
fn fixed_header_tokens_accept_exact_decimal_aliases_and_reject_binary64_rounding() {
    for (from, to) in [
        ("\"version\":1", "\"version\":1.000e0"),
        ("\"schemaVersion\":1", "\"schemaVersion\":100e-2"),
        ("\"unitScale\":1", "\"unitScale\":0.01e2"),
        ("\"reader_version\":2", "\"reader_version\":20e-1"),
    ] {
        assert!(decode_static_collision(
            &raw_json_glb(FIXTURE, from, to),
            &CollisionDecodeLimits::default()
        )
        .is_ok());
    }
    for (from, to) in [
        ("\"version\":1", "\"version\":1.00000000000000000001"),
        (
            "\"schemaVersion\":1",
            "\"schemaVersion\":0.99999999999999999999",
        ),
        (
            "\"reader_version\":2",
            "\"reader_version\":2.00000000000000000001",
        ),
    ] {
        rejected(&raw_json_glb(FIXTURE, from, to));
    }
}
#[test]
fn resource_tokens_never_round_fractional_or_negative_spellings_into_indices() {
    for (from, to) in [
        ("\"mesh\":0", "\"mesh\":0.0"),
        ("\"mesh\":0", "\"mesh\":-0"),
        ("\"count\":4", "\"count\":4.00000000000000000001"),
        ("\"selectedNodes\":[0,1]", "\"selectedNodes\":[0.0,1]"),
    ] {
        rejected(&raw_json_glb(FIXTURE, from, to));
    }
}
#[test]
fn collision_payload_and_instance_work_budgets_are_independent() {
    let mut checks: Vec<(&str, CollisionDecodeLimits)> = vec![];
    let mut l = CollisionDecodeLimits::default();
    l.max_glb_bytes = FIXTURE.len() - 1;
    checks.push(("GLB", l));
    let mut l = CollisionDecodeLimits::default();
    l.max_json_bytes = 1;
    checks.push(("JSON", l));
    let mut l = CollisionDecodeLimits::default();
    l.max_geometry_bytes = 71;
    checks.push(("decoded collision geometry", l));
    let mut l = CollisionDecodeLimits::default();
    l.max_vertices = 3;
    checks.push(("collision vertices", l));
    let mut l = CollisionDecodeLimits::default();
    l.max_indices = 5;
    checks.push(("collision indices", l));
    let mut l = CollisionDecodeLimits::default();
    l.max_resources = 1;
    checks.push(("resource count", l));
    let mut l = CollisionDecodeLimits::default();
    l.max_accessor_validation_bytes = 71;
    checks.push(("accessor validation bytes", l));
    let mut l = CollisionDecodeLimits::default();
    l.max_expanded_triangles = 3;
    checks.push(("expanded collision triangles", l));
    let mut l = CollisionDecodeLimits::default();
    l.max_world_vertices = 11;
    checks.push(("collision world vertex evaluations", l));
    for (label, limits) in checks {
        let e = decode_static_collision(FIXTURE, &limits)
            .unwrap_err()
            .to_string();
        assert!(e.contains(label), "{label}: {e}");
    }
}
#[test]
fn alias_work_preflight_wins_before_first_nonfinite_payload() {
    let b = edited(FIXTURE, |j, b| {
        let a = j["accessors"][0].clone();
        j["accessors"].as_array_mut().unwrap().push(a);
        b.as_mut().unwrap()[0..4].copy_from_slice(&f32::NAN.to_le_bytes());
    });
    let mut limits = CollisionDecodeLimits::default();
    limits.max_accessor_validation_bytes = 72;
    let error = decode_static_collision(&b, &limits)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("accessor validation bytes exceeds limit"),
        "{error}"
    );
    let error = decode_static_collision(&b, &CollisionDecodeLimits::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("nonfinite"), "{error}");
}
#[test]
fn all_primitive_budgets_precede_bad_index_and_all_world_budgets_precede_transform_scans() {
    let b = edited(FIXTURE, |_, b| {
        b.as_mut().unwrap()[48..52].copy_from_slice(&99u32.to_le_bytes())
    });
    let mut limits = CollisionDecodeLimits::default();
    limits.max_indices = 5;
    let e = decode_static_collision(&b, &limits)
        .unwrap_err()
        .to_string();
    assert!(e.contains("collision indices exceeds limit"), "{e}");
    let b = edited(FIXTURE, |j, _| j["nodes"][0]["matrix"][12] = json!(1e20));
    let mut limits = CollisionDecodeLimits::default();
    limits.max_expanded_triangles = 3;
    let e = decode_static_collision(&b, &limits)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("expanded collision triangles exceeds limit"),
        "{e}"
    );
}
#[test]
fn visual_profile_still_requires_bin_and_rejects_collision_roles() {
    use eqoxide_assets::static_visual::{decode_static_visual, DecodeLimits};
    for b in [FIXTURE, EMPTY] {
        assert!(decode_static_visual(b, &DecodeLimits::default()).is_err());
    }
    let (j, _) = parts(include_bytes!("fixtures/static-visual-v1.glb"));
    assert!(decode_static_visual(&glb(j, None), &DecodeLimits::default()).is_err());
}
#[test]
fn unused_scalar_descriptors_cannot_hide_a_vertex_target() {
    let bytes = edited(FIXTURE, |j, _| {
        let mut view = j["bufferViews"][1].clone();
        view["target"] = json!(34962);
        j["bufferViews"].as_array_mut().unwrap().push(view);
        let mut a = j["accessors"][1].clone();
        a["bufferView"] = json!(2);
        j["accessors"].as_array_mut().unwrap().push(a);
    });
    rejected(&bytes);
}
#[test]
fn collision_buffer_names_are_not_part_of_the_strict_profile() {
    rejected(&edited(FIXTURE, |j, _| {
        j["buffers"][0]["name"] = json!("unexpected")
    }));
}

#[test]
fn duplicate_collision_tokens_cannot_hide_invalid_header_or_identity_metadata() {
    for (from, to) in [
        ("\"schemaVersion\":1", "\"schemaVersion\":1.00000000000000000001,\"schemaVersion\":1"),
        ("\"version\":1", "\"version\":2,\"version\":1"),
        ("\"role\":\"collision\"", "\"role\":\"visual\",\"role\":\"collision\""),
        ("\"reader_version\":2", "\"reader_version\":2.00000000000000000001,\"reader_version\":2"),
        ("\"queryPolicy\":\"inspection-segment-front-v1\"", "\"queryPolicy\":\"inspection-segment-both-v1\",\"queryPolicy\":\"inspection-segment-front-v1\""),
        ("\"availability\":\"available\"", "\"availability\":\"unavailable\",\"availability\":\"available\""),
        ("\"ownerId\":\"authored-floor\"", "\"ownerId\":\"authored-platform\",\"ownerId\":\"authored-floor\""),
        ("\"selectedNodes\":[0,1]", "\"selectedNodes\":[0,0],\"selectedNodes\":[0,1]"),
    ] {
        let b=raw_json_glb(FIXTURE,from,to);
        let error=decode_static_collision(&b,&CollisionDecodeLimits::default()).unwrap_err().to_string();
        assert!(error.contains("duplicate object key"),"{from}: {error}");
    }
}
#[test]
fn duplicate_collision_keys_are_refused_recursively_in_resources_and_at_root() {
    for (from, to) in [
        ("\"count\":4", "\"count\":1,\"count\":4"),
        (
            "\"componentType\":5126",
            "\"componentType\":5126,\"componentType\":5126",
        ),
        ("\"mode\":4", "\"mode\":5,\"mode\":4"),
        (
            "\"extras\":{\"eqoxideAsset\"",
            "\"extras\":null,\"extras\":{\"eqoxideAsset\"",
        ),
        (
            "\"schemaVersion\":1",
            "\"schemaVersion\":1,\"schema\\u0056ersion\":1",
        ),
    ] {
        let b = raw_json_glb(FIXTURE, from, to);
        let error = decode_static_collision(&b, &CollisionDecodeLimits::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("duplicate object key"), "{from}: {error}");
    }
    let b = raw_json_glb(
        EMPTY,
        "\"availability\":\"available-empty\"",
        "\"availability\":\"available\",\"availability\":\"available-empty\"",
    );
    assert!(
        decode_static_collision(&b, &CollisionDecodeLimits::default())
            .unwrap_err()
            .to_string()
            .contains("duplicate object key")
    );
}

#[test]
fn duplicate_validation_keeps_json_byte_preflight_and_normal_recursion_limit() {
    let duplicate = raw_json_glb(
        FIXTURE,
        "\"schemaVersion\":1",
        "\"schemaVersion\":1,\"schemaVersion\":1",
    );
    let mut limits = CollisionDecodeLimits::default();
    limits.max_json_bytes = 1;
    let error = decode_static_collision(&duplicate, &limits)
        .unwrap_err()
        .to_string();
    assert!(error.contains("invalid or excessive JSON chunk"), "{error}");
    let nested = format!("\"schemaVersion\":{}1{}", "[".repeat(256), "]".repeat(256));
    let deep = raw_json_glb(FIXTURE, "\"schemaVersion\":1", &nested);
    let error = decode_static_collision(&deep, &CollisionDecodeLimits::default())
        .unwrap_err()
        .to_string();
    assert!(error.contains("recursion limit exceeded"), "{error}");
}

#[test]
fn canonical_query_units_accept_explicit_declaration_without_rebaking_geometry() {
    let bytes = edited(FIXTURE, |j, _| {
        j["extras"]["eqoxideAsset"]["requirements"]["capabilities"] = json!([
            "static-collision-query-unit-frames-v1",
            "static-collision-triangles-v1"
        ]);
    });
    let declared = decode_static_collision(&bytes, &CollisionDecodeLimits::default()).unwrap();
    let original = decode_static_collision(FIXTURE, &CollisionDecodeLimits::default()).unwrap();
    assert_eq!(
        declared.header.requirements.capabilities,
        vec![
            "static-collision-query-unit-frames-v1",
            "static-collision-triangles-v1"
        ]
    );
    assert_eq!(
        declared.header.query_unit_frames,
        QueryUnitFrames::DeclaredCanonical
    );
    assert_eq!(
        declared.header.query_unit_frames.identifier(),
        "declared-canonical"
    );
    assert_eq!(
        original.header.query_unit_frames,
        QueryUnitFrames::Unspecified
    );
    assert_eq!(original.header.query_unit_frames.identifier(), "unspecified");
    assert_eq!(declared.meshes[0].positions, original.meshes[0].positions);
    assert_eq!(declared.instances[1].matrix, original.instances[1].matrix);
    assert_eq!(declared.query_policy, original.query_policy);
}

#[test]
fn canonical_query_units_capabilities_are_unique_closed_sets() {
    for source in [FIXTURE, EMPTY] {
        for caps in [
            json!(["static-collision-query-unit-frames-v1", "static-collision-triangles-v1"]),
            json!(["static-collision-triangles-v1", "static-collision-query-unit-frames-v1"]),
        ] {
            let bytes = edited(source, |j, _| {
                j["extras"]["eqoxideAsset"]["requirements"]["capabilities"] = caps;
            });
            let decoded =
                decode_static_collision(&bytes, &CollisionDecodeLimits::default()).unwrap();
            assert_eq!(decoded.header.requirements.capabilities.len(), 2);
            assert_eq!(
                decoded.header.query_unit_frames,
                QueryUnitFrames::DeclaredCanonical
            );
        }
        for caps in [
            json!([]),
            json!(["static-collision-query-unit-frames-v1"]),
            json!(["static-collision-triangles-v1", "static-collision-triangles-v1"]),
            json!([
                "static-collision-triangles-v1",
                "static-collision-query-unit-frames-v1",
                "static-collision-query-unit-frames-v1"
            ]),
            json!(["static-collision-triangles-v1", "unknown-capability-v1"]),
            json!(["static-collision-triangles-v1", 1]),
        ] {
            rejected(&edited(source, |j, _| {
                j["extras"]["eqoxideAsset"]["requirements"]["capabilities"] = caps;
            }));
        }
    }
}
#[test]
fn canonical_query_units_known_empty_still_obeys_capability_budget() {
    let bytes = edited(EMPTY, |j, _| {
        j["extras"]["eqoxideAsset"]["requirements"]["capabilities"] = json!([
            "static-collision-query-unit-frames-v1",
            "static-collision-triangles-v1"
        ]);
    });
    let mut limits = CollisionDecodeLimits::default();
    limits.max_resources = 1;
    assert!(decode_static_collision(EMPTY, &limits).is_ok());
    let error = decode_static_collision(&bytes, &limits).unwrap_err().to_string();
    assert!(error.contains("resource count"), "{error}");
    limits.max_resources = 2;
    let decoded = decode_static_collision(&bytes, &limits).unwrap();
    assert_eq!(decoded.availability, CollisionAvailability::AvailableEmpty);
    assert_eq!(
        decoded.header.query_unit_frames,
        QueryUnitFrames::DeclaredCanonical
    );
    assert!(decoded.meshes.is_empty() && decoded.instances.is_empty() && decoded.bounds.is_none());
}
