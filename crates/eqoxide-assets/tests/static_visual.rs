use eqoxide_assets::static_visual::{
    decode_static_visual,
    DecodeLimits,
    StaticAlphaMode
};
use serde_json::{
    json,
    Value
};
const FIXTURE: &[u8] = include_bytes!("fixtures/static-visual-v1.glb");
fn parts(bytes: &[u8]) -> (Value, Vec<u8>) {
    let n = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    let j = serde_json::from_slice(&bytes[20..20+n]).unwrap();
    (j, bytes[28+n..].to_vec())
}
fn glb(j: Value, bin: Vec<u8>) -> Vec<u8> {
    let mut j = serde_json::to_vec(&j).unwrap();
    while j.len()%4 != 0 {
        j.push(b' ');
    }
    let mut b = b"glTF".to_vec();
    b.extend(2u32.to_le_bytes());
    b.extend(((28+j.len()+bin.len()) as u32).to_le_bytes());
    b.extend((j.len() as u32).to_le_bytes());
    b.extend(b"JSON");
    b.extend(j);
    b.extend((bin.len() as u32).to_le_bytes());
    b.extend(b"BIN\0");
    b.extend(bin);
    b
}
fn edited(f: impl FnOnce(&mut Value)) -> Vec<u8> {
    let (mut j, b) = parts(FIXTURE);
    f(&mut j);
    glb(j, b)
}
fn rejected(bytes: &[u8]) {
    assert!(decode_static_visual(bytes, &DecodeLimits::default()).is_err());
}
#[test]
fn producer_fixture_preserves_geometry_materials_and_instances() {
    let s = decode_static_visual(FIXTURE, &DecodeLimits::default()).unwrap();
    assert_eq!(s.header.schema_version, 1);
    assert_eq!(s.header.requirements.reader_version, 2);
    assert_eq!(s.meshes.len(), 1);
    assert_eq!(s.instances.len(), 2);
    for (a, b) in s.bounds.min.into_iter().zip([-3., -7., 2.]) {
        assert!((a-b).abs()<1e-4);
    }
    for (a, b) in s.bounds.max.into_iter().zip([16.44179, 13., 6.5]) {
        assert!((a-b).abs()<1e-4);
    }
    assert_eq!(s.materials[0].alpha_mode, StaticAlphaMode::Mask{
        cutoff: 0.37
    });
    assert_eq!(s.materials[0].base_color, [0.8, 0.6, 0.2, 0.65]);
    assert!(s.meshes[0].primitives[0].colors.is_some());
    assert!(s.meshes[0].primitives[1].colors.is_none());
    assert_eq!(s.images.len(), 1);
    assert_eq!(s.textures[0].image_index, 0);
    assert_eq!((s.images[0].width, s.images[0].height), (1, 1));
}
#[test]
fn header_requires_declared_color_support_and_semantic_integral_versions() {
    let equivalent=edited(|j| {
        j["extras"]["eqoxideAsset"]["schemaVersion"]=json!(1.0);
        j["extras"]["eqoxideAsset"]["requirements"]["reader_version"]=json!(2.0);
    });
    assert!(decode_static_visual(&equivalent, &DecodeLimits::default()).is_ok());
    rejected(&edited(|j|j["extras"]["eqoxideAsset"]["requirements"]["capabilities"]=json!(["static-visual-v1"])));
    for change in [json!(1.5), json!(-1), json!(4294967296u64)] {
        rejected(&edited(|j|j["extras"]["eqoxideAsset"]["schemaVersion"]=change));
    }
}
#[test]
fn external_images_and_unsupported_features_fail() {
    rejected(&edited(|j|j["images"][0]=json!({
        "uri": "data:image/png;base64,AA=="
    })));
    rejected(&edited(|j|j["images"][0]=json!({
        "uri": "file:///secret.png"
    })));
    rejected(&edited(|j|j["nodes"][0]["children"]=json!([1])));
    rejected(&edited(|j|j["materials"][0]["extras"]=json!({
        "eqAdditive": true
    })));
    rejected(&edited(|j|j["animations"]=json!([])));
}
#[test]
fn truncated_accessor_and_budget_fail_without_mutating_source() {
    let source=edited(|j|j["accessors"][0]["count"]=json!(100000000));
    let before=source.clone();
    rejected(&source);
    assert_eq!(source, before);
    let mut limits=DecodeLimits::default();
    limits.max_geometry_bytes=1;
    assert!(decode_static_visual(FIXTURE, &limits).is_err());
    limits=DecodeLimits::default();
    limits.max_png_bytes=1;
    assert!(decode_static_visual(FIXTURE, &limits).is_err());
}
fn replace_accessor(j: &mut Value, bin: &mut Vec<u8>, accessor: usize, data: &[u8], component: u32, normalized: bool) {
    while bin.len()%4 != 0 {
        bin.push(0);
    }
    let offset=bin.len();
    bin.extend(data);
    while bin.len()%4 != 0 {
        bin.push(0);
    }
    let view=j["bufferViews"].as_array().unwrap().len();
    j["bufferViews"].as_array_mut().unwrap().push(json!({
        "buffer": 0,
        "byteOffset": offset,
        "byteLength": data.len(),
        "target": 34962
    }));
    j["buffers"][0]["byteLength"]=json!(bin.len());
    j["accessors"][accessor]["bufferView"]=json!(view);
    j["accessors"][accessor]["componentType"]=json!(component);
    j["accessors"][accessor]["normalized"]=json!(normalized);
}
#[test]
fn normalized_integer_primary_uvs_and_full_rgba_are_retained() {
    let (mut j, mut bin)=parts(FIXTURE);
    replace_accessor(&mut j, &mut bin, 2, &[0, 255, 0, 0, 128, 0, 0, 0, 255, 255, 0, 0], 5121, true);
    let uv_view=j["accessors"][2]["bufferView"].as_u64().unwrap() as usize;
    j["bufferViews"][uv_view]["byteStride"]=json!(4);
    let colors: Vec<u8>=[65535u16, 0, 32768, 16384, 0, 65535, 0, 65535, 0, 0, 65535, 0].into_iter().flat_map(u16::to_le_bytes).collect();
    replace_accessor(&mut j, &mut bin, 4, &colors, 5123, true);
    let s=decode_static_visual(&glb(j, bin), &DecodeLimits::default()).unwrap();
    assert_eq!(s.meshes[0].uvs[0], [0., 1.]);
    assert!((s.meshes[0].uvs[1][0]-128./255.).abs()<1e-7);
    assert_eq!(s.meshes[0].primitives[0].colors.as_ref().unwrap()[0], [1., 0., 32768./65535., 16384./65535.]);
}
#[test]
fn embedded_sixteen_bit_png_is_fully_validated_and_preserved_exactly() {
    use image::ImageEncoder;
    let mut png=vec![];
    let pixels: Vec<u8>=[0x1234u16, 0xabcd, 0x5678, 0x9abc].into_iter().flat_map(u16::to_ne_bytes).collect();
    image::codecs::png::PngEncoder::new(&mut png).write_image(&pixels, 1, 1, image::ExtendedColorType::Rgba16).unwrap();
    let (mut j, mut bin)=parts(FIXTURE);
    let offset=bin.len();
    bin.extend(&png);
    while bin.len()%4!=0{
        bin.push(0);
    }
    j["bufferViews"][0]["byteOffset"]=json!(offset);
    j["bufferViews"][0]["byteLength"]=json!(png.len());
    j["buffers"][0]["byteLength"]=json!(bin.len());
    let s=decode_static_visual(&glb(j.clone(), bin.clone()), &DecodeLimits::default()).unwrap();
    assert_eq!(s.images[0].png_bytes, png);
    let mut limits=DecodeLimits::default();
    limits.max_decoded_png_bytes=7;
    assert!(decode_static_visual(&glb(j.clone(), bin.clone()), &limits).is_err());
    // Corrupt actual IDAT content, not just a declaration.
    let idat=png.windows(4).position(|w|w==b"IDAT").unwrap();
    bin[offset+idat+4]^=0x80;
    rejected(&glb(j, bin));
}
#[test]
fn geometry_axes_normals_winding_and_color_isolation_are_explicit() {
    let (j, bin)=parts(FIXTURE);
    let s=decode_static_visual(FIXTURE, &DecodeLimits::default()).unwrap();
    let pos=j["bufferViews"][1]["byteOffset"].as_u64().unwrap() as usize;
    for i in 0..3 {
        let floats: Vec<f32>=bin[pos+12*i..pos+12*i+12].chunks_exact(4).map(|b|f32::from_le_bytes(b.try_into().unwrap())).collect();
        assert_eq!(s.meshes[0].positions[i], [floats[0], -floats[2], floats[1]]);
    }
    assert_eq!(s.meshes[0].normals, vec![[0., -1./5.0_f32.sqrt(), 2./5.0_f32.sqrt()];3]);
    assert_eq!(s.meshes[0].primitives[0].indices, vec![0, 1, 2]);
    assert_eq!(s.meshes[0].primitives[1].indices, vec![0, 2, 1]);
    assert_eq!(s.meshes[0].primitives[0].colors.as_ref().unwrap()[0], [0.2, 0.4, 0.8, 0.35]);
}
#[test]
fn source_names_and_collision_names_do_not_control_visual_dispatch() {
    let renamed=edited(|j| {
        j["asset"]["generator"]=json!("another importer");
        j["meshes"][0]["name"]=json!("__collision__");
        j["images"][0]["name"]=json!("different source.png");
        j["materials"][0]["name"]=json!("opaque-native-label");
    });
    let s=decode_static_visual(&renamed, &DecodeLimits::default()).unwrap();
    assert_eq!(s.meshes[0].name, "__collision__");
    assert_eq!(s.meshes[0].primitives.len(), 2);
    assert_eq!(s.materials[0].texture_index, Some(0));
    assert_eq!(s.bounds.min, [-3., -7., 2.]);
}
#[test]
fn strict_material_features_reject_loss_and_preserve_blend() {
    for (key, val) in [("extras", json!({
        "eqAnim": {
        }
    })), ("normalTexture", json!({
        "index": 0
    })), ("occlusionTexture", json!({
        "index": 0
    })), ("emissiveFactor", json!([0., 0., 0.])), ("extensions", json!({
    }))] {
        rejected(&edited(|j|j["materials"][0][key]=val));
    }
    for (key, val) in [("metallicFactor", json!(0.5)), ("roughnessFactor", json!(0.8)), ("metallicRoughnessTexture", json!({
        "index": 0
    }))] {
        rejected(&edited(|j|j["materials"][0]["pbrMetallicRoughness"][key]=val));
    }
    rejected(&edited(|j|j["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["texCoord"]=json!(1)));
    rejected(&edited(|j|j["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["extensions"]=json!({
        "KHR_texture_transform": {
        }
    })));
    let blend=edited(|j|{
        j["materials"][0]["alphaMode"]=json!("BLEND");
        j["materials"][0].as_object_mut().unwrap().remove("alphaCutoff");
        j["materials"][0]["doubleSided"]=json!(true);
    });
    let s=decode_static_visual(&blend, &DecodeLimits::default()).unwrap();
    assert_eq!(s.materials[0].alpha_mode, StaticAlphaMode::Blend);
    assert!(s.materials[0].double_sided);
}
#[test]
fn unused_resources_are_validated_and_image_aliases_are_budgeted() {
    rejected(&edited(|j|j["images"].as_array_mut().unwrap().push(json!({
        "uri": "https://example.com/bad.png"
    }))));
    rejected(&edited(|j|j["textures"].as_array_mut().unwrap().push(json!({
        "source": 999
    }))));
    rejected(&edited(|j|j["accessors"].as_array_mut().unwrap().push(json!({
        "bufferView": 99,
        "componentType": 5126,
        "count": 3,
        "type": "VEC3"
    }))));
    rejected(&edited(|j|j["bufferViews"].as_array_mut().unwrap().push(json!({
        "buffer": 0,
        "byteOffset": u64::MAX,
        "byteLength": 5
    }))));
    let aliases=edited(|j|{
        let image=j["images"][0].clone();
        j["images"].as_array_mut().unwrap().push(image);
        j["textures"].as_array_mut().unwrap().push(json!({
            "source": 0
        }));
    });
    let s=decode_static_visual(&aliases, &DecodeLimits::default()).unwrap();
    assert_eq!(s.images.len(), 2);
    assert_eq!(s.textures.len(), 2);
    let mut limits=DecodeLimits::default();
    limits.max_png_bytes=s.images[0].png_bytes.len();
    assert!(decode_static_visual(&aliases, &limits).is_err());
}
#[test]
fn malformed_header_and_unsupported_scene_contracts_are_rejected() {
    for (key, value) in [("schemaVersion", json!(2)), ("role", json!("collision")), ("coordinateProfile", json!("legacy")), ("unitScale", json!(0.01)), ("bakeRevision", json!("AB")), ("unknownFlag", json!(true))] {
        rejected(&edited(|j|j["extras"]["eqoxideAsset"][key]=value));
    }
    rejected(&edited(|j|j.as_object_mut().unwrap().remove("extras").map(|_|()).unwrap()));
    rejected(&edited(|j|j["extras"]["eqoxideAsset"]["requirements"]["capabilities"]=json!(["static-visual-v1", "future-effect"])));
    rejected(&edited(|j|j["meshes"][0]["primitives"][0]["targets"]=json!([])));
    rejected(&edited(|j|j["meshes"][0]["primitives"][0]["attributes"]["TEXCOORD_1"]=json!(2)));
    rejected(&edited(|j|j["accessors"][0]["sparse"]=json!({
    })));
    rejected(&edited(|j|j["samplers"]=json!([])));
    rejected(&edited(|j|j["textures"][0]["sampler"]=json!(0)));
    rejected(&edited(|j|j["scenes"][0]["nodes"]=json!([0, 0])));
    rejected(&edited(|j|j["nodes"][1]["mesh"]=json!(99)));
    rejected(&edited(|j|j["buffers"][0]["uri"]=json!("data:application/octet-stream;base64,AA==")));
}
#[test]
fn matrix_profile_rejects_reflection_shear_nonuniform_and_projective() {
    for (index, value) in [(0, -1.), (0, 2.), (1, 0.1), (3, 0.000001), (15, 0.)] {
        rejected(&edited(|j|{
            j["nodes"][1]["matrix"][index]=json!(value);
        }));
    }
    let huge=edited(|j| {
        for index in [0, 5, 10, 12]{
            j["nodes"][1]["matrix"][index]=json!(f32::MAX);
        }
    });
    rejected(&huge);
}
#[test]
fn all_payload_budgets_and_truncation_are_enforced() {
    for field in 0..7 {
        let mut limits=DecodeLimits::default();
        match field {
            0=>limits.max_glb_bytes=1,
            1=>limits.max_json_bytes=1,
            2=>limits.max_geometry_bytes=1,
            3=>limits.max_png_bytes=1,
            4=>limits.max_decoded_png_bytes=1,
            5=>limits.max_single_png_bytes=1,
            _=>limits.max_resources=1
        };
        assert!(decode_static_visual(FIXTURE, &limits).is_err(), "budget {field}");
    }
    let mut limits=DecodeLimits::default();
    limits.max_png_dimension=0;
    assert!(decode_static_visual(FIXTURE, &limits).is_err());
    for end in 0..FIXTURE.len() {
        rejected(&FIXTURE[..end]);
    }
    rejected(&edited(|j|j["accessors"][0]["byteOffset"]=json!(u64::MAX)));
    rejected(&edited(|j|j["accessors"][0]["count"]=json!(u64::MAX)));
    rejected(&edited(|j|j["accessors"][0]["normalized"]=json!(true)));
}
#[test]
fn declared_accessor_extents_and_vertex_layout_must_be_truthful() {
    rejected(&edited(|j|j["accessors"][0]["min"]=json!([4., 0., -2.])));
    rejected(&edited(|j|j["accessors"][0]["max"]=json!([0., 1., 0.])));
    rejected(&edited(|j|{
        j["accessors"][0].as_object_mut().unwrap().remove("min");
    }));
    rejected(&edited(|j|j["bufferViews"][1]["target"]=json!(34963)));
    rejected(&edited(|j|j["bufferViews"][4]["target"]=json!(34962)));
    rejected(&edited(|j|j["accessors"][0]["max"]=json!([1e100, 1., 0.])));
    let (mut j, mut bin)=parts(FIXTURE);
    replace_accessor(&mut j, &mut bin, 2, &[0, 255, 128, 0, 255, 255], 5121, true);
    rejected(&glb(j, bin));
}
#[test]
fn json_material_normalization_cannot_be_rounded_into_range() {
    rejected(&edited(|j|j["materials"][0]["alphaCutoff"]=json!(1.00000001)));
    rejected(&edited(|j|j["materials"][0]["pbrMetallicRoughness"]["baseColorFactor"][0]=json!(1.00000001)));
    rejected(&edited(|j|j["materials"][0]["pbrMetallicRoughness"]["metallicFactor"]=json!(1e-100)));
}
fn replace_png(j: &mut Value, bin: &mut Vec<u8>, png: &[u8]) {
    let offset=bin.len();
    bin.extend(png);
    while bin.len()%4!=0{
        bin.push(0);
    }
    j["bufferViews"][0]["byteOffset"]=json!(offset);
    j["bufferViews"][0]["byteLength"]=json!(png.len());
    j["buffers"][0]["byteLength"]=json!(bin.len());
}
#[test]
fn png_requires_complete_static_stream_including_end_and_crc() {
    let (j, bin)=parts(FIXTURE);
    let n=j["bufferViews"][0]["byteLength"].as_u64().unwrap() as usize;
    let original=&bin[..n];
    let mut no_end=original.to_vec();
    no_end.truncate(n-12);
    let mut trailing=original.to_vec();
    trailing.extend(b"junk");
    let mut bad_crc=original.to_vec();
    bad_crc[n-1]^=1;
    for png in [no_end, trailing, bad_crc] {
        let(mut j, mut bin)=parts(FIXTURE);
        replace_png(&mut j, &mut bin, &png);
        rejected(&glb(j, bin));
    }
}
#[test]
fn animated_png_is_not_silently_reduced_to_one_frame() {
    let (mut j, mut bin)=parts(FIXTURE);
    let n=j["bufferViews"][0]["byteLength"].as_u64().unwrap() as usize;
    let mut png=bin[..33].to_vec();
    png.extend(8u32.to_be_bytes());
    let mut chunk=b"acTL".to_vec();
    chunk.extend(1u32.to_be_bytes());
    chunk.extend(0u32.to_be_bytes());
    png.extend(&chunk);
    png.extend(crc32fast::hash(&chunk).to_be_bytes());
    png.extend(&bin[33..n]);
    replace_png(&mut j, &mut bin, &png);
    rejected(&glb(j, bin));
}
#[test]
fn header_numbers_are_exact_even_beyond_f64_precision() {
    let (j, bin)=parts(FIXTURE);
    let json=serde_json::to_string(&j).unwrap();
    for (old, new) in [("\"schemaVersion\":1", "\"schemaVersion\":1.00000000000000001"), ("\"reader_version\":2", "\"reader_version\":2.00000000000000001"), ("\"unitScale\":1.0", "\"unitScale\":1.00000000000000001")] {
        assert!(json.contains(old));
        let changed=json.replace(old, new);
        rejected(&raw_glb(changed, bin.clone()));
    }
    for version in ["1.0", "1e0", "10e-1", "0.001e3", "1.00000000000000000000"] {
        let changed=json.replace("\"schemaVersion\":1", &format!("\"schemaVersion\":{version}"));
        assert!(decode_static_visual(&raw_glb(changed, bin.clone()), &DecodeLimits::default()).is_ok(), "{version}");
    }
}
fn raw_glb(json: String, bin: Vec<u8>) -> Vec<u8> {
    let mut json=json.into_bytes();
    while json.len()%4 !=0 {
        json.push(b' ');
    }
    let mut b=b"glTF".to_vec();
    b.extend(2u32.to_le_bytes());
    b.extend(((28+json.len()+bin.len()) as u32).to_le_bytes());
    b.extend((json.len() as u32).to_le_bytes());
    b.extend(b"JSON");
    b.extend(json);
    b.extend((bin.len() as u32).to_le_bytes());
    b.extend(b"BIN\0");
    b.extend(bin);
    b
}
#[test]
fn world_evaluation_budget_counts_shared_mesh_instances_before_bounds_work() {
    let mut limits=DecodeLimits::default();
    limits.max_world_vertices=5;
    let source=FIXTURE.to_vec();
    let before=source.clone();
    assert!(decode_static_visual(&source, &limits).is_err());
    assert_eq!(source, before);
    limits.max_world_vertices=6;
    assert!(decode_static_visual(FIXTURE, &limits).is_ok());
}
