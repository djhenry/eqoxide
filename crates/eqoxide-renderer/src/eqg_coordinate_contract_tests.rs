//! CI contract across the public EQG adapter, CPU upload, and actual instanced shader.
use super::zone_upload_to_world;
use eqoxide_assets::EqgServerPreview;

#[test]
fn eqg_adapter_matches_renderer_cpu_and_instanced_shader() {
    // Export convention C(native x,y,z)=(x,z,-y), using asymmetric points.
    let native = [[2.0_f32, 7., 3.], [6., 7., 3.], [2., 13., 3.]];
    let mut bin = Vec::new();
    for [x,y,z] in native { for v in [x,z,-y] { bin.extend(v.to_le_bytes()); } }
    for _ in 0..3 { for v in [0.0_f32,1.,0.] { bin.extend(v.to_le_bytes()); } }
    for index in [0_u32,1,2] { bin.extend(index.to_le_bytes()); }
    let doc = serde_json::json!({
        "asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0,1]}],
        "nodes":[{"mesh":0},{"mesh":0,"translation":[11,17,-23]}],
        "meshes":[{"primitives":[{"attributes":{"POSITION":0,"NORMAL":1},"indices":2}]}],
        "buffers":[{"byteLength":bin.len()}],
        "bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":36},{"buffer":0,"byteOffset":72,"byteLength":12}],
        "accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[2,3,-13],"max":[6,3,-7]},
            {"bufferView":1,"componentType":5126,"count":3,"type":"VEC3"},
            {"bufferView":2,"componentType":5125,"count":3,"type":"SCALAR"}]
    });
    let mut json = serde_json::to_vec(&doc).unwrap();
    while !json.len().is_multiple_of(4) { json.push(b' '); }
    let mut glb = Vec::new();
    for word in [0x46546c67_u32,2,(28+json.len()+bin.len()) as u32,json.len() as u32,0x4e4f534a] { glb.extend(word.to_le_bytes()); }
    glb.extend(json); glb.extend((bin.len() as u32).to_le_bytes()); glb.extend(0x004e4942_u32.to_le_bytes()); glb.extend(bin);
    struct Fixture(std::path::PathBuf);
    impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }
    let path = Fixture(std::env::temp_dir().join(format!("eqg-renderer-contract-{}.glb", std::process::id())));
    std::fs::write(&path.0, glb).unwrap();
    let adapted = EqgServerPreview::from_glb(&path.0).unwrap().into_assets();
    for (p, [x,y,z]) in adapted.terrain[0].positions.iter().zip(native) {
        assert_eq!(zone_upload_to_world(*p), [y,x,z]);
    }
    assert_eq!(zone_upload_to_world(adapted.terrain[0].normals[0]), [0.,0.,1.]);
    let expanded = eqoxide_assets::expand_objects(&adapted.objects);
    for (p, [x,y,z]) in expanded[0].positions.iter().zip(native) {
        assert_eq!(zone_upload_to_world(*p), [y+23.,x+11.,z+17.]);
    }
    // A source-contract guard, not GPU execution. Strip comments and whitespace so
    // explanatory comments cannot satisfy it. Pin assignments that feed the outputs,
    // including the instance multiplication order; any swizzle change requires review.
    let shader = executable_wgsl(include_str!("shaders/zone_instanced.wgsl"));
    for statement in [
        "letworld=inst*vec4<f32>(in.position,1.0);",
        "letrender=vec3<f32>(world.z,world.x,world.y);",
        "out.clip_pos=camera.view_proj*vec4<f32>(render,1.0);",
        "out.world_pos=render;",
        "letnw=inst3*in.normal;",
        "out.normal=vec3<f32>(nw.z,nw.x,nw.y);",
    ] { assert!(shader.contains(statement), "instanced shader coordinate contract changed: {statement}"); }
}

// WGSL has nested block comments and no string literals. Preserve only executable
// tokens; a commented copy of the old swizzle must never satisfy the contract.
fn executable_wgsl(source: &str) -> String {
    let mut chars = source.chars().peekable();
    let mut depth = 0;
    let mut line_comment = false;
    let mut result = String::new();
    while let Some(ch) = chars.next() {
        if line_comment {
            if ch == '\n' { line_comment = false; }
        } else if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            depth += 1;
        } else if depth > 0 {
            if ch == '*' && chars.peek() == Some(&'/') {
                chars.next();
                depth -= 1;
            }
        } else if ch == '/' && chars.peek() == Some(&'/') {
            chars.next();
            line_comment = true;
        } else if !ch.is_whitespace() {
            result.push(ch);
        }
    }
    assert_eq!(depth, 0, "unterminated WGSL block comment");
    result
}

#[test]
fn shader_contract_ignores_nested_block_and_line_comments() {
    assert_eq!(executable_wgsl("/* old; /* nested; */ old; */ live; // old;\nnext;"), "live;next;");
    assert_eq!(executable_wgsl("// /* not a block\nlive;"), "live;");
}
