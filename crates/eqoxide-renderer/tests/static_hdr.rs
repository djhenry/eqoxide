//! Source-independent fixtures for the opt-in linear HDR compositor.
use eqoxide_assets::static_visual::DecodeLimits;
use eqoxide_renderer::static_visual::{
    GpuStaticVisual, PreparedStaticVisual, ReferenceLighting, UploadLimits,
};
use glam::{Mat4, Vec3};
use serde_json::{json, Value};

#[derive(Clone, Copy)]
struct Layer {
    color: [f32; 4],
    z: f32,
    mode: &'static str,
    unlit: bool,
    double_sided: bool,
    reverse: bool,
    cutoff: f32,
    metallic: f32,
    roughness: f32,
}
impl Layer {
    fn new(rgb: [f32; 3], alpha: f32, z: f32, mode: &'static str, unlit: bool) -> Self {
        Self {
            color: [rgb[0], rgb[1], rgb[2], alpha],
            z,
            mode,
            unlit,
            double_sided: false,
            reverse: false,
            cutoff: 0.5,
            metallic: 0.,
            roughness: 1.,
        }
    }
}
fn accessor(
    j: &mut Value,
    bin: &mut Vec<u8>,
    values: &[u8],
    count: usize,
    kind: &str,
    component: u32,
    target: u32,
) -> usize {
    let offset = bin.len();
    bin.extend(values);
    let view = j["bufferViews"].as_array().unwrap().len();
    j["bufferViews"]
        .as_array_mut()
        .unwrap()
        .push(json!({"buffer":0,"byteOffset":offset,"byteLength":values.len(),"target":target}));
    let a = j["accessors"].as_array().unwrap().len();
    j["accessors"]
        .as_array_mut()
        .unwrap()
        .push(json!({"bufferView":view,"componentType":component,"count":count,"type":kind}));
    a
}
fn fixture(layers: &[Layer], hdr: bool) -> Vec<u8> {
    let mut capabilities = vec!["pbr-metallic-roughness-factors-v1", "static-visual-v1"];
    if hdr {
        capabilities.push("pbr-metallic-roughness-blend-v1");
    }
    capabilities.sort_unstable();
    let mut j = json!({"asset":{"version":"2.0"},"extras":{"eqoxideAsset":{"schemaVersion":1,"role":"visual","coordinateProfile":"eqoxide-static-y-up-v1","unitScale":1,"bakeRevision":"a".repeat(64),"requirements":{"reader_version":2,"capabilities":capabilities}}},"scene":0,"scenes":[{"nodes":[]}],"nodes":[],"meshes":[],"materials":[],"buffers":[],"bufferViews":[],"accessors":[]});
    let mut bin = Vec::new();
    for (i, layer) in layers.iter().enumerate() {
        // Writer basis: server (x,y,z) -> glTF (x,z,-y), once.
        let positions = [
            [-0.85, layer.z, 0.85],
            [0.85, layer.z, 0.85],
            [0., layer.z, -0.85],
        ];
        let pb: Vec<_> = positions
            .into_iter()
            .flatten()
            .flat_map(f32::to_le_bytes)
            .collect();
        let nb: Vec<_> = [0.0f32, 1., 0.]
            .repeat(3)
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        let ub: Vec<_> = [0.0f32; 6].into_iter().flat_map(f32::to_le_bytes).collect();
        let indices = if layer.reverse {
            [0u32, 2, 1]
        } else {
            [0u32, 1, 2]
        };
        let ib: Vec<_> = indices.into_iter().flat_map(u32::to_le_bytes).collect();
        let p = accessor(&mut j, &mut bin, &pb, 3, "VEC3", 5126, 34962);
        j["accessors"][p]["min"] = json!([-0.85, layer.z, -0.85]);
        j["accessors"][p]["max"] = json!([0.85, layer.z, 0.85]);
        let n = accessor(&mut j, &mut bin, &nb, 3, "VEC3", 5126, 34962);
        let uv = accessor(&mut j, &mut bin, &ub, 3, "VEC2", 5126, 34962);
        let ix = accessor(&mut j, &mut bin, &ib, 3, "SCALAR", 5125, 34963);
        j["meshes"].as_array_mut().unwrap().push(json!({"name":format!("layer{i}"),"primitives":[{"attributes":{"POSITION":p,"NORMAL":n,"TEXCOORD_0":uv},"indices":ix,"material":i}]}));
        j["nodes"].as_array_mut().unwrap().push(json!({"mesh":i}));
        j["scenes"][0]["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!(i));
        let mut m = json!({"name":format!("material{i}"),"alphaMode":layer.mode,"doubleSided":layer.double_sided,"pbrMetallicRoughness":{"baseColorFactor":layer.color,"metallicFactor":layer.metallic,"roughnessFactor":layer.roughness}});
        if layer.mode == "MASK" {
            m["alphaCutoff"] = json!(layer.cutoff);
        }
        if layer.unlit {
            m["extensions"] = json!({"KHR_materials_unlit":{}});
        }
        j["materials"].as_array_mut().unwrap().push(m);
    }
    if layers.iter().any(|l| l.unlit) {
        j["extensionsUsed"] = json!(["KHR_materials_unlit"]);
        j["extensionsRequired"] = json!(["KHR_materials_unlit"]);
    }
    j["buffers"] = json!([{"byteLength":bin.len()}]);
    let mut js = serde_json::to_vec(&j).unwrap();
    while !js.len().is_multiple_of(4) {
        js.push(b' ');
    }
    let mut bytes = b"glTF".to_vec();
    bytes.extend(2u32.to_le_bytes());
    bytes.extend(((28 + js.len() + bin.len()) as u32).to_le_bytes());
    bytes.extend((js.len() as u32).to_le_bytes());
    bytes.extend(b"JSON");
    bytes.extend(js);
    bytes.extend((bin.len() as u32).to_le_bytes());
    bytes.extend(b"BIN\0");
    bytes.extend(bin);
    bytes
}
fn prepare(bytes: &[u8]) -> PreparedStaticVisual {
    PreparedStaticVisual::decode(bytes, &DecodeLimits::default(), &UploadLimits::default()).unwrap()
}
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
}
impl Gpu {
    fn new() -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .expect("HDR acceptance requires a Vulkan adapter");
        eprintln!("HDR actual adapter: {:?}", adapter.get_info());
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("HDR acceptance"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: Default::default(),
            },
            None,
        ))
        .unwrap();
        Self { device, queue }
    }
}
fn view() -> Mat4 {
    Mat4::look_at_rh(Vec3::new(0., 0., 4.), Vec3::ZERO, Vec3::Y)
}
fn projection() -> Mat4 {
    Mat4::orthographic_rh(-1., 1., -1., 1., 0.1, 10.)
}
fn ambient_light(exposure: f32) -> ReferenceLighting {
    ReferenceLighting {
        direction_to_light: [0., 0., 1.],
        directional_radiance: [0.; 3],
        ambient_radiance: [4.; 3],
        exposure,
    }
}
#[test]
fn hdr_profile_reports_its_distinct_display_semantics() {
    let bytes = fixture(
        &[Layer::new([0.2, 0.3, 0.4], 0.5, 0., "BLEND", false)],
        true,
    );
    assert_eq!(
        prepare(&bytes).render_profile(),
        "pbr-hdr-blend-reference-light-v1"
    );
}
#[test]
fn historical_upload_refuses_hdr_required_artifacts() {
    let gpu = Gpu::new();
    let bytes = fixture(
        &[Layer::new([0.2, 0.3, 0.4], 0.5, 0., "BLEND", false)],
        true,
    );
    let result = GpuStaticVisual::upload(
        &gpu.device,
        &gpu.queue,
        prepare(&bytes),
        wgpu::TextureFormat::Rgba8UnormSrgb,
    );
    assert!(
        result.is_err(),
        "historical borrowed-pass upload must not silently display an HDR-required artifact"
    );
}
#[test]
fn hdr_capability_selects_common_display_even_for_only_unlit_materials() {
    let bytes = fixture(&[Layer::new([0.2, 0.3, 0.4], 0.5, 0., "BLEND", true)], true);
    let scene = prepare(&bytes);
    assert!(!scene.has_pbr_materials());
    assert_eq!(scene.render_profile(), "pbr-hdr-blend-reference-light-v1");
}

// Tests-first appendix, appended after the existing-API routing RED is observed.
use eqoxide_renderer::static_visual::{GpuHdrStaticVisual, HdrFrameLimits};
impl Gpu {
    fn hdr(
        &self,
        bytes: &[u8],
        light: ReferenceLighting,
        limits: UploadLimits,
        frame: HdrFrameLimits,
    ) -> GpuHdrStaticVisual {
        GpuHdrStaticVisual::upload(
            &self.device,
            &self.queue,
            prepare(bytes),
            wgpu::TextureFormat::Rgba8UnormSrgb,
            light,
            &limits,
            &frame,
        )
        .unwrap()
    }
    fn render(
        &self,
        scene: &mut GpuHdrStaticVisual,
        width: u32,
        height: u32,
        clear: [f32; 3],
    ) -> Vec<[u8; 4]> {
        scene.resize(&self.device, width, height).unwrap();
        scene
            .prepare_camera(&self.queue, view(), projection())
            .unwrap();
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("HDR display acceptance"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let pitch = (width * 4).div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("HDR display readback"),
            size: u64::from(pitch) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        scene.encode(&mut encoder, &view, clear).unwrap();
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(pitch),
                    rows_per_image: Some(height),
                },
            },
            size,
        );
        self.queue.submit(Some(encoder.finish()));
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .unwrap()
            .unwrap();
        let pixels = {
            let bytes = slice.get_mapped_range();
            bytes
                .chunks_exact(pitch as usize)
                .flat_map(|row| row[..(width * 4) as usize].as_chunks::<4>().0.to_vec())
                .collect()
        };
        buffer.unmap();
        pixels
    }
    fn encode_ready(&self, scene: &GpuHdrStaticVisual) -> bool {
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: 32,
                height: 32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        scene
            .encode(
                &mut encoder,
                &target.create_view(&Default::default()),
                [0.; 3],
            )
            .is_ok()
    }
}
fn display(rgb: [f64; 3], exposure: f64) -> [u8; 4] {
    let mut out = [0u8; 4];
    for i in 0..3 {
        let x = rgb[i] * exposure;
        let x = x / (1. + x);
        let y = if x <= 0.0031308 {
            12.92 * x
        } else {
            1.055 * x.powf(1. / 2.4) - 0.055
        };
        out[i] = (y * 255.).round() as u8;
    }
    out[3] = 255;
    out
}
fn near(actual: [u8; 4], expected: [u8; 4], tolerance: u8) {
    for i in 0..4 {
        assert!(
            actual[i].abs_diff(expected[i]) <= tolerance,
            "actual {actual:?}, expected {expected:?}, channel {i}"
        );
    }
}
fn ambient_rgb(rgb: [f32; 3]) -> [f64; 3] {
    rgb.map(|v| 4. * (0.96 * f64::from(v) + 0.04))
}
fn over(front: [f64; 3], alpha: f64, back: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| front[i] * alpha + back[i] * (1. - alpha))
}
#[test]
fn hdr_composites_linear_radiance_then_maps_once() {
    let gpu = Gpu::new();
    // Deliberately unsorted nodes: near blend, opaque background, far blend.
    let layers = [
        Layer::new([0.1, 0.2, 0.9], 0.5, 0.4, "BLEND", true),
        Layer::new([0.2, 0.1, 0.05], 1., -0.4, "OPAQUE", false),
        Layer::new([0.8, 0.05, 0.02], 0.25, 0., "BLEND", false),
    ];
    let mut scene = gpu.hdr(
        &fixture(&layers, true),
        ambient_light(2.),
        UploadLimits::default(),
        HdrFrameLimits::default(),
    );
    let pixels = gpu.render(&mut scene, 32, 32, [0.; 3]);
    assert!(pixels.iter().filter(|p| p[0] > 0 || p[1] > 0 || p[2] > 0).count() > 100,
        "the authored overlapping fixture must render visible fragments before evaluating its light oracle");
    let radiance = over(
        [0.1, 0.2, 0.9],
        0.5,
        over(
            ambient_rgb([0.8, 0.05, 0.02]),
            0.25,
            ambient_rgb([0.2, 0.1, 0.05]),
        ),
    );
    near(pixels[16 * 32 + 16], display(radiance, 2.), 2);
    // The whole triangle contains the same radiance under this parallel ambient fixture.
    assert!(pixels.iter().filter(|p| p[0] > 100 && p[2] > 100).count() > 100);
}
#[test]
fn hdr_opaque_depth_mask_equality_and_zero_alpha_are_preserved() {
    let gpu = Gpu::new();
    let mut mask = Layer::new([0.1, 0.7, 0.2], 0.5, 0.3, "MASK", true);
    mask.cutoff = 0.5;
    let layers = [
        Layer::new([1., 0., 0.], 0.9, -0.4, "BLEND", false),
        mask,
        Layer::new([0., 0., 1.], 1., -0.6, "OPAQUE", true),
        Layer::new([1., 1., 1.], 0., 0.6, "BLEND", false),
    ];
    let mut scene = gpu.hdr(
        &fixture(&layers, true),
        ambient_light(1.),
        UploadLimits::default(),
        HdrFrameLimits::default(),
    );
    near(
        gpu.render(&mut scene, 32, 32, [0.; 3])[16 * 32 + 16],
        display([0.1, 0.7, 0.2], 1.),
        2,
    );
    mask.cutoff = 0.51;
    let mut scene = gpu.hdr(
        &fixture(
            &[mask, Layer::new([0., 0., 1.], 1., -0.6, "OPAQUE", true)],
            true,
        ),
        ambient_light(1.),
        UploadLimits::default(),
        HdrFrameLimits::default(),
    );
    near(
        gpu.render(&mut scene, 32, 32, [0.; 3])[16 * 32 + 16],
        display([0., 0., 1.], 1.),
        2,
    );
}
#[test]
fn hdr_sidedness_and_display_background_are_explicit() {
    let gpu = Gpu::new();
    let mut layer = Layer::new([0.7, 0.3, 0.1], 0.5, 0., "BLEND", true);
    layer.reverse = true;
    let mut scene = gpu.hdr(
        &fixture(&[layer], true),
        ambient_light(2.),
        UploadLimits::default(),
        HdrFrameLimits::default(),
    );
    near(
        gpu.render(&mut scene, 32, 32, [0.03, 0.05, 0.1])[16 * 32 + 16],
        display([0.03, 0.05, 0.1], 2.),
        2,
    );
    layer.double_sided = true;
    let mut scene = gpu.hdr(
        &fixture(&[layer], true),
        ambient_light(2.),
        UploadLimits::default(),
        HdrFrameLimits::default(),
    );
    near(
        gpu.render(&mut scene, 32, 32, [0.03, 0.05, 0.1])[16 * 32 + 16],
        display(over([0.7, 0.3, 0.1], 0.5, [0.03, 0.05, 0.1]), 2.),
        2,
    );
}
#[test]
fn hdr_targets_are_budgeted_and_failed_resize_or_camera_blocks_encoding() {
    let gpu = Gpu::new();
    let bytes = fixture(&[Layer::new([0.3, 0.2, 0.1], 0.5, 0., "BLEND", true)], true);
    let stats = prepare(&bytes).stats();
    let limits = UploadLimits {
        max_gpu_bytes: stats.gpu_bytes + 16 + 32 * 32 * 12,
        ..UploadLimits::default()
    };
    let mut scene = gpu.hdr(&bytes, ambient_light(1.), limits, HdrFrameLimits::default());
    assert!(!gpu.encode_ready(&scene));
    scene.resize(&gpu.device, 32, 32).unwrap();
    scene
        .prepare_camera(&gpu.queue, view(), projection())
        .unwrap();
    assert!(gpu.encode_ready(&scene));
    let s = scene.stats();
    assert_eq!(s.frame_target_bytes, 32 * 32 * 12);
    assert_eq!(s.postprocess_bytes, 16);
    assert_eq!(s.total_gpu_bytes, stats.gpu_bytes + 16 + 32 * 32 * 12);
    scene.resize(&gpu.device, 32, 32).unwrap();
    assert_eq!(scene.stats(), s);
    assert!(scene.resize(&gpu.device, 33, 32).is_err());
    assert!(!gpu.encode_ready(&scene));
    scene.resize(&gpu.device, 32, 32).unwrap();
    assert!(gpu.encode_ready(&scene));
    assert!(scene
        .prepare_camera(
            &gpu.queue,
            Mat4::from_cols_array(&[f32::NAN; 16]),
            projection()
        )
        .is_err());
    assert!(!gpu.encode_ready(&scene));
    scene
        .prepare_camera(&gpu.queue, view(), projection())
        .unwrap();
    assert!(gpu.encode_ready(&scene));
    for size in [[0, 32], [u32::MAX, u32::MAX], [16385, 1]] {
        assert!(scene.resize(&gpu.device, size[0], size[1]).is_err());
        assert!(!gpu.encode_ready(&scene));
    }
    scene.resize(&gpu.device, 32, 32).unwrap();
    near(
        gpu.render(&mut scene, 32, 32, [0.; 3])[16 * 32 + 16],
        display(over([0.3, 0.2, 0.1], 0.5, [0.; 3]), 1.),
        2,
    );
}

#[test]
fn hdr_extreme_radiance_clips_before_premultiplication_and_stays_finite() {
    let gpu = Gpu::new();
    let mut bright = Layer::new([1.; 3], 0.5, -0.2, "BLEND", false);
    bright.metallic = 1.;
    bright.roughness = 0.;
    let front = Layer::new([0.; 3], 0.99, 0.2, "BLEND", true);
    let light = ReferenceLighting {
        direction_to_light: [0., 0., 1.],
        directional_radiance: [64.; 3],
        ambient_radiance: [0.; 3],
        exposure: 0.01,
    };
    let mut scene = gpu.hdr(
        &fixture(&[front, bright], true),
        light,
        UploadLimits::default(),
        HdrFrameLimits::default(),
    );
    let pixels = gpu.render(&mut scene, 32, 32, [0.; 3]);
    // This aligned smooth-metal response is above65504. The specified per-fragment
    // straight-radiance clamp precedes coverage, then the black front layer attenuates it.
    let stored = 65504. * 0.5 * (1. - f64::from(0.99f32));
    near(
        pixels[16 * 32 + 16],
        display([stored; 3], f64::from(0.01f32)),
        2,
    );
    assert!(pixels.iter().all(|p| p[3] == 255));
}

#[test]
fn hdr_mask_zero_cutoff_survives_zero_alpha_as_an_opaque_surface() {
    let gpu = Gpu::new();
    let mut mask = Layer::new([0.7, 0.1, 0.2], 0., 0.3, "MASK", true);
    mask.cutoff = 0.;
    let bytes = fixture(
        &[mask, Layer::new([0., 0., 1.], 1., -0.3, "OPAQUE", true)],
        true,
    );
    let mut scene = gpu.hdr(
        &bytes,
        ambient_light(1.),
        UploadLimits::default(),
        HdrFrameLimits::default(),
    );
    near(
        gpu.render(&mut scene, 32, 32, [0.; 3])[16 * 32 + 16],
        display([0.7, 0.1, 0.2], 1.),
        2,
    );
}

#[test]
fn hdr_overlapping_clipped_layers_do_not_turn_the_display_black() {
    let gpu = Gpu::new();
    let mut layers = Vec::new();
    for (alpha, z, mode) in [
        (0.6, 0.2, "BLEND"),
        (1., -0.4, "OPAQUE"),
        (0.73, 0.4, "BLEND"),
        (0.41, 0., "BLEND"),
    ] {
        let mut layer = Layer::new([1.; 3], alpha, z, mode, false);
        layer.metallic = 1.;
        layer.roughness = 0.;
        layers.push(layer);
    }
    let light = ReferenceLighting {
        direction_to_light: [0., 0., 1.],
        directional_radiance: [64.; 3],
        ambient_radiance: [0.; 3],
        exposure: 0.01,
    };
    let mut scene = gpu.hdr(
        &fixture(&layers, true),
        light,
        UploadLimits::default(),
        HdrFrameLimits::default(),
    );
    let pixels = gpu.render(&mut scene, 32, 32, [65504.; 3]);
    // Non-binary source-alpha blending near the half-float ceiling must not produce
    // an infinity/NaN-dependent black display sample on the tested adapter.
    near(
        pixels[16 * 32 + 16],
        display([65504.; 3], f64::from(0.01f32)),
        2,
    );
    assert!(pixels
        .iter()
        .all(|p| p[0] > 250 && p[1] > 250 && p[2] > 250 && p[3] == 255));
}

#[test]
fn hdr_upload_respects_explicit_scene_work_and_texture_limits() {
    let gpu = Gpu::new();
    let bytes = fixture(&[Layer::new([0.2, 0.3, 0.4], 0.5, 0., "BLEND", true)], true);
    for limits in [
        UploadLimits {
            max_draw_commands: 0,
            ..UploadLimits::default()
        },
        UploadLimits {
            max_triangle_instances: 0,
            ..UploadLimits::default()
        },
        UploadLimits {
            max_texture_bytes: 15,
            ..UploadLimits::default()
        },
    ] {
        assert!(
            GpuHdrStaticVisual::upload(
                &gpu.device,
                &gpu.queue,
                prepare(&bytes),
                wgpu::TextureFormat::Rgba8UnormSrgb,
                ambient_light(1.),
                &limits,
                &HdrFrameLimits::default()
            )
            .is_err(),
            "the explicit upload limits must not be ignored when preparation used looser limits"
        );
    }
}

#[test]
fn hdr_upload_respects_image_limit_when_the_white_fallback_fits() {
    use image::ImageEncoder;
    let gpu = Gpu::new();
    let base = fixture(&[Layer::new([0.2, 0.3, 0.4], 0.5, 0., "BLEND", true)], true);
    let json_len = u32::from_le_bytes(base[12..16].try_into().unwrap()) as usize;
    let mut j: Value = serde_json::from_slice(&base[20..20 + json_len]).unwrap();
    let mut bin = base[28 + json_len..].to_vec();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&[128; 16], 2, 2, image::ExtendedColorType::Rgba8)
        .unwrap();
    let view = j["bufferViews"].as_array().unwrap().len();
    j["bufferViews"]
        .as_array_mut()
        .unwrap()
        .push(json!({"buffer":0,"byteOffset":bin.len(),"byteLength":png.len()}));
    bin.extend(&png);
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    j["buffers"][0]["byteLength"] = json!(bin.len());
    j["images"] = json!([{"bufferView":view,"mimeType":"image/png"}]);
    j["textures"] = json!([{"source":0}]);
    j["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"] = json!({"index":0});
    let mut js = serde_json::to_vec(&j).unwrap();
    while !js.len().is_multiple_of(4) {
        js.push(b' ');
    }
    let mut bytes = b"glTF".to_vec();
    bytes.extend(2u32.to_le_bytes());
    bytes.extend(((28 + js.len() + bin.len()) as u32).to_le_bytes());
    bytes.extend((js.len() as u32).to_le_bytes());
    bytes.extend(b"JSON");
    bytes.extend(js);
    bytes.extend((bin.len() as u32).to_le_bytes());
    bytes.extend(b"BIN\0");
    bytes.extend(bin);
    assert_eq!(prepare(&bytes).stats().image_count, 1);
    GpuHdrStaticVisual::upload(
        &gpu.device,
        &gpu.queue,
        prepare(&bytes),
        wgpu::TextureFormat::Rgba8UnormSrgb,
        ambient_light(1.),
        &UploadLimits::default(),
        &HdrFrameLimits::default(),
    )
    .unwrap();
    let limited = UploadLimits {
        max_texture_bytes: 32,
        ..UploadLimits::default()
    };
    let error = GpuHdrStaticVisual::upload(
        &gpu.device,
        &gpu.queue,
        prepare(&bytes),
        wgpu::TextureFormat::Rgba8UnormSrgb,
        ambient_light(1.),
        &limited,
        &HdrFrameLimits::default(),
    )
    .err()
    .expect("2x2 float image costs64bytes, even though the16-byte white fallback fits");
    assert!(error.to_string().contains("image"), "{error:#}");
}
