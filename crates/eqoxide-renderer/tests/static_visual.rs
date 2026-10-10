use eqoxide_assets::static_visual::DecodeLimits;
use eqoxide_renderer::static_visual::{PreparedStaticVisual, UploadLimits};
const FIXTURE: &[u8] = include_bytes!("../../eqoxide-assets/tests/fixtures/static-visual-v1.glb");

#[test]
fn preparation_preserves_shared_pools_instances_and_color_isolation() {
    let scene =
        PreparedStaticVisual::decode(FIXTURE, &DecodeLimits::default(), &UploadLimits::default())
            .unwrap();
    let s = scene.stats();
    assert_eq!(
        (s.mesh_count, s.shared_vertex_buffers, s.instance_records),
        (1, 1, 2)
    );
    assert_eq!(
        (s.primitive_count, s.index_buffers, s.color_buffers),
        (2, 2, 2)
    );
    assert_eq!((s.image_count, s.texture_count), (1, 1));
    // 60 vertex + 24 index + 96 RGBA + 128 instance + 64 material + 64 camera + 32 texture bytes.
    assert_eq!(s.gpu_bytes, 468);
    assert_eq!(scene.bounds().min, [-3., -7., 2.]);
    assert!((scene.bounds().max[0] - 16.44179).abs() < 0.0001);
}

#[test]
fn upload_payload_limits_reject_before_preparation_and_accept_exact_boundary() {
    let limits = DecodeLimits::default();
    assert!(PreparedStaticVisual::decode(
        FIXTURE,
        &limits,
        &UploadLimits {
            max_gpu_bytes: 467,
            max_texture_bytes: 16,
            ..UploadLimits::default()
        }
    )
    .is_err());
    assert!(PreparedStaticVisual::decode(
        FIXTURE,
        &limits,
        &UploadLimits {
            max_gpu_bytes: 468,
            max_texture_bytes: 15,
            ..UploadLimits::default()
        }
    )
    .is_err());
    assert!(PreparedStaticVisual::decode(
        FIXTURE,
        &limits,
        &UploadLimits {
            max_gpu_bytes: 468,
            max_texture_bytes: 16,
            ..UploadLimits::default()
        }
    )
    .is_ok());
}

#[test]
fn preparation_cannot_bypass_bounded_actual_decoder() {
    let mut limits = DecodeLimits::default();
    limits.max_geometry_bytes = 1;
    assert!(PreparedStaticVisual::decode(FIXTURE, &limits, &UploadLimits::default()).is_err());
    assert!(PreparedStaticVisual::decode(
        b"not an artifact",
        &DecodeLimits::default(),
        &UploadLimits::default()
    )
    .is_err());
}

use eqoxide_renderer::static_visual::GpuStaticVisual;
use glam::{Mat4, Vec3};
use image::ImageEncoder;
use serde_json::{json, Value};

fn parts(bytes: &[u8]) -> (Value, Vec<u8>) {
    let n = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
    (
        serde_json::from_slice(&bytes[20..20 + n]).unwrap(),
        bytes[28 + n..].to_vec(),
    )
}
fn glb(mut json: Value, mut bin: Vec<u8>) -> Vec<u8> {
    while bin.len() % 4 != 0 {
        bin.push(0);
    }
    json["buffers"] = json!([{ "byteLength": bin.len() }]);
    let mut json = serde_json::to_vec(&json).unwrap();
    while json.len() % 4 != 0 {
        json.push(b' ');
    }
    let mut bytes = b"glTF".to_vec();
    bytes.extend(2u32.to_le_bytes());
    bytes.extend(((28 + json.len() + bin.len()) as u32).to_le_bytes());
    bytes.extend((json.len() as u32).to_le_bytes());
    bytes.extend(b"JSON");
    bytes.extend(json);
    bytes.extend((bin.len() as u32).to_le_bytes());
    bytes.extend(b"BIN\0");
    bytes.extend(bin);
    bytes
}
#[test]
fn image_and_texture_aliases_count_allocations_before_upload() {
    let (mut j, b) = parts(FIXTURE);
    let alias = j["images"][0].clone();
    j["images"].as_array_mut().unwrap().push(alias);
    j["textures"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "source": 0 }));
    j["materials"][1]["pbrMetallicRoughness"]["baseColorTexture"] = json!({ "index": 1 });
    let bytes = glb(j, b);
    let s =
        PreparedStaticVisual::decode(&bytes, &DecodeLimits::default(), &UploadLimits::default())
            .unwrap()
            .stats();
    assert_eq!((s.image_count, s.texture_count, s.gpu_bytes), (2, 2, 484));
    assert!(PreparedStaticVisual::decode(
        &bytes,
        &DecodeLimits::default(),
        &UploadLimits {
            max_gpu_bytes: 483,
            max_texture_bytes: 16,
            ..UploadLimits::default()
        }
    )
    .is_err());
}
#[test]
fn actual_static_shader_parses_and_validates_without_optional_float_filtering() {
    let module =
        naga::front::wgsl::parse_str(include_str!("../src/shaders/static_visual.wgsl")).unwrap();
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    )
    .validate(&module)
    .unwrap();
}
fn view(j: &mut Value, bin: &mut Vec<u8>, data: &[u8], target: Option<u32>) -> usize {
    while bin.len() % 4 != 0 {
        bin.push(0);
    }
    let offset = bin.len();
    bin.extend(data);
    let index = j["bufferViews"].as_array().unwrap().len();
    let mut v = json!({ "buffer": 0, "byteOffset": offset, "byteLength": data.len() });
    if let Some(t) = target {
        v["target"] = json!(t);
    }
    j["bufferViews"].as_array_mut().unwrap().push(v);
    index
}
fn accessor(
    j: &mut Value,
    bin: &mut Vec<u8>,
    data: &[u8],
    count: usize,
    kind: &str,
    component: u32,
    target: u32,
) -> usize {
    let v = view(j, bin, data, Some(target));
    let index = j["accessors"].as_array().unwrap().len();
    j["accessors"]
        .as_array_mut()
        .unwrap()
        .push(json!({ "bufferView": v, "count": count, "type": kind, "componentType": component }));
    index
}
fn floats(data: impl IntoIterator<Item = f32>) -> Vec<u8> {
    data.into_iter().flat_map(f32::to_le_bytes).collect()
}
fn material(
    factor: [f32; 4],
    mode: &str,
    cutoff: Option<f32>,
    double_sided: bool,
    texture: bool,
) -> Value {
    let mut m = json!({ "pbrMetallicRoughness": { "baseColorFactor": factor, "metallicFactor": 0, "roughnessFactor": 1 }, "alphaMode": mode, "doubleSided": double_sided });
    if texture {
        m["pbrMetallicRoughness"]["baseColorTexture"] = json!({ "index": 0 });
    }
    if let Some(cutoff) = cutoff {
        m["alphaCutoff"] = json!(cutoff);
    }
    m
}
/// Build a complete common-profile artifact from independent server-space landmarks.
/// Positions/nodes are authored in the inverse asset transform, not preconverted runtime data.
fn scene(
    positions: &[[f32; 3]],
    primitives: &[(Vec<u32>, usize, Option<[f32; 4]>)],
    materials: Vec<Value>,
    translations: &[[f32; 3]],
    uv: [f32; 2],
    png: Option<(Vec<u8>, u32, u32)>,
) -> Vec<u8> {
    let (fixture, _) = parts(FIXTURE);
    let mut j = json!({ "asset": { "version": "2.0" }, "extras": fixture["extras"], "bufferViews": [], "accessors": [], "materials": materials, "meshes": [], "nodes": [], "scene": 0, "scenes": [{ "nodes": [] }], "images": [], "textures": [] });
    let mut bin = Vec::new();
    let p = accessor(
        &mut j,
        &mut bin,
        &floats(positions.iter().flat_map(|p| [p[0], p[2], -p[1]])),
        positions.len(),
        "VEC3",
        5126,
        34962,
    );
    let source: Vec<_> = positions.iter().map(|p| [p[0], p[2], -p[1]]).collect();
    let min = std::array::from_fn::<_, 3, _>(|i| {
        source.iter().map(|p| p[i]).fold(f32::INFINITY, f32::min)
    });
    let max = std::array::from_fn::<_, 3, _>(|i| {
        source
            .iter()
            .map(|p| p[i])
            .fold(f32::NEG_INFINITY, f32::max)
    });
    j["accessors"][p]["min"] = json!(min);
    j["accessors"][p]["max"] = json!(max);
    let n = accessor(
        &mut j,
        &mut bin,
        &floats(positions.iter().flat_map(|_| [0., 1., 0.])),
        positions.len(),
        "VEC3",
        5126,
        34962,
    );
    let u = accessor(
        &mut j,
        &mut bin,
        &floats(positions.iter().flat_map(|_| uv)),
        positions.len(),
        "VEC2",
        5126,
        34962,
    );
    let mut ps = Vec::new();
    for (indices, m, color) in primitives {
        let i = accessor(
            &mut j,
            &mut bin,
            &indices
                .iter()
                .flat_map(|i| i.to_le_bytes())
                .collect::<Vec<_>>(),
            indices.len(),
            "SCALAR",
            5125,
            34963,
        );
        let mut primitive = json!({ "attributes": { "POSITION": p, "NORMAL": n, "TEXCOORD_0": u }, "indices": i, "material": m });
        if let Some(color) = color {
            let c = accessor(
                &mut j,
                &mut bin,
                &floats(positions.iter().flat_map(|_| *color)),
                positions.len(),
                "VEC4",
                5126,
                34962,
            );
            primitive["attributes"]["COLOR_0"] = json!(c);
        }
        ps.push(primitive);
    }
    j["meshes"] = json!([{ "primitives": ps }]);
    for (i, t) in translations.iter().enumerate() {
        j["nodes"].as_array_mut().unwrap().push(json!({ "mesh": 0, "matrix": [1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., t[0], t[2], -t[1], 1.] }));
        j["scenes"][0]["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!(i));
    }
    if let Some((png, _, _)) = png {
        let v = view(&mut j, &mut bin, &png, None);
        j["images"] = json!([{ "bufferView": v, "mimeType": "image/png" }]);
        j["textures"] = json!([{ "source": 0 }]);
    }
    glb(j, bin)
}
fn png8(pixels: &[u8], width: u32, height: u32) -> (Vec<u8>, u32, u32) {
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(pixels, width, height, image::ExtendedColorType::Rgba8)
        .unwrap();
    (png, width, height)
}
fn png16(pixels: &[u16], width: u32, height: u32) -> (Vec<u8>, u32, u32) {
    let mut png = Vec::new();
    let data: Vec<_> = pixels.iter().flat_map(|v| v.to_ne_bytes()).collect();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&data, width, height, image::ExtendedColorType::Rgba16)
        .unwrap();
    (png, width, height)
}
const TRIANGLE: [[f32; 3]; 3] = [[-0.8, -0.8, 0.], [0.8, -0.8, 0.], [0., 0.8, 0.]];
fn front_view() -> Mat4 {
    Mat4::look_at_rh(Vec3::new(0., 0., 4.), Vec3::ZERO, Vec3::Y)
}
fn projection() -> Mat4 {
    Mat4::orthographic_rh(-1., 1., -1., 1., 0.1, 10.)
}
struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
}
impl Gpu {
    fn new(max_buffer: Option<u64>) -> Self {
        Self::with_limits(max_buffer, None)
    }
    fn with_limits(max_buffer: Option<u64>, max_texture: Option<u32>) -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });
        let adapter =
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
                .expect("GPU acceptance requires a Vulkan adapter; adapter absence is a failure");
        eprintln!("GPU acceptance adapter: {:?}", adapter.get_info());
        let mut limits = wgpu::Limits::downlevel_defaults();
        if let Some(n) = max_buffer {
            limits.max_buffer_size = n;
        }
        if let Some(n) = max_texture {
            limits.max_texture_dimension_2d = n;
        }
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("static visual acceptance"),
                required_features: wgpu::Features::empty(),
                required_limits: limits,
                memory_hints: Default::default(),
            },
            None,
        ))
        .expect("GPU acceptance requires a usable device");
        Self { device, queue }
    }
    fn upload(&self, bytes: &[u8], format: wgpu::TextureFormat) -> GpuStaticVisual {
        let prepared =
            PreparedStaticVisual::decode(bytes, &DecodeLimits::default(), &UploadLimits::default())
                .unwrap();
        GpuStaticVisual::upload(&self.device, &self.queue, prepared, format).unwrap()
    }
    fn assert_draw_ready(&self, scene: &GpuStaticVisual, ready: bool) {
        let size = wgpu::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        };
        let color = self.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth = self.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let color = color.create_view(&Default::default());
        let depth = depth.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        assert_eq!(scene.draw(&mut pass).is_ok(), ready);
    }
    fn render(
        &self,
        scene: &mut GpuStaticVisual,
        view: Mat4,
        format: wgpu::TextureFormat,
    ) -> Vec<[u8; 4]> {
        scene
            .prepare_camera(&self.queue, view, projection())
            .unwrap();
        let size = wgpu::Extent3d {
            width: 64,
            height: 64,
            depth_or_array_layers: 1,
        };
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("acceptance color"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let depth = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("acceptance depth"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let target_view = target.create_view(&Default::default());
        let depth_view = depth.create_view(&Default::default());
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("acceptance readback"),
            size: 64 * 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("acceptance draw"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            scene.draw(&mut pass).unwrap();
        }
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(64),
                },
            },
            size,
        );
        self.queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv().unwrap().unwrap();
        let bytes = slice.get_mapped_range();
        let pixels = bytes
            .chunks_exact(4)
            .map(|p| p.try_into().unwrap())
            .collect();
        drop(bytes);
        readback.unmap();
        pixels
    }
}
fn pixel(pixels: &[[u8; 4]], x: usize, y: usize) -> [u8; 4] {
    pixels[y * 64 + x]
}
fn near(label: &str, actual: [u8; 4], expected: [u8; 4], tolerance: u8) {
    eprintln!("{label}: actual {actual:?}, expected {expected:?}");
    for (a, e) in actual.into_iter().zip(expected) {
        assert!(
            a.abs_diff(e) <= tolerance,
            "{label}: {actual:?} != {expected:?}"
        );
    }
}

#[test]
#[ignore = "requires explicit actual GPU acceptance run"]
fn gpu_material_product_mask_cutoff_and_opaque_zero_alpha() {
    let gpu = Gpu::new(None);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let make = |mode: &str, cutoff| {
        scene(
            &TRIANGLE,
            &[(vec![0, 1, 2], 0, Some([0.4, 0.6, 0.8, 0.7]))],
            vec![material([0.8, 0.5, 0.25, 0.65], mode, cutoff, false, true)],
            &[[0.; 3]],
            [0.5; 2],
            Some(png8(&[128, 192, 255, 128], 1, 1)),
        )
    };
    // Independent literal expectation: linear(128/255)*.8*.4, linear(192/255)*.5*.6, 1*.25*.8.
    let mut opaque = gpu.upload(&make("OPAQUE", None), format);
    near(
        "texture * factor * vertex RGB",
        pixel(&gpu.render(&mut opaque, front_view(), format), 32, 32),
        [75, 111, 124, 255],
        2,
    );
    let mut mask = gpu.upload(&make("MASK", Some(0.22)), format);
    near(
        "full alpha product .2284 survives cutoff .22",
        pixel(&gpu.render(&mut mask, front_view(), format), 32, 32),
        [75, 111, 124, 255],
        2,
    );
    let mut mask = gpu.upload(&make("MASK", Some(0.24)), format);
    near(
        "full alpha product .2284 fails cutoff .24",
        pixel(&gpu.render(&mut mask, front_view(), format), 32, 32),
        [0; 4],
        0,
    );
    let bytes = scene(
        &TRIANGLE,
        &[(vec![0, 1, 2], 0, Some([1., 1., 1., 0.]))],
        vec![material([1., 0., 0., 0.], "OPAQUE", None, false, true)],
        &[[0.; 3]],
        [0.5; 2],
        Some(png8(&[255, 255, 255, 0], 1, 1)),
    );
    let mut opaque = gpu.upload(&bytes, format);
    near(
        "OPAQUE ignores all zero alpha inputs",
        pixel(&gpu.render(&mut opaque, front_view(), format), 32, 32),
        [255, 0, 0, 255],
        0,
    );
}

#[test]
#[ignore = "requires explicit actual GPU acceptance run"]
fn gpu_shared_primitive_colors_culling_and_server_instances() {
    let gpu = Gpu::new(None);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let positions = [
        [-0.9, -0.6, 0.],
        [-0.1, -0.6, 0.],
        [-0.5, 0.6, 0.],
        [0.1, -0.6, 0.],
        [0.9, -0.6, 0.],
        [0.5, 0.6, 0.],
    ];
    let bytes = scene(
        &positions,
        &[
            (vec![0, 1, 2], 0, Some([0.2, 0.4, 0.6, 1.])),
            (vec![3, 4, 5], 1, None),
        ],
        vec![
            material([1.; 4], "OPAQUE", None, false, false),
            material([0.8, 0.5, 0.2, 1.], "OPAQUE", None, false, false),
        ],
        &[[0.; 3]],
        [0.; 2],
        None,
    );
    let mut visual = gpu.upload(&bytes, format);
    let pixels = gpu.render(&mut visual, front_view(), format);
    near(
        "colored primitive",
        pixel(&pixels, 16, 34),
        [124, 170, 203, 255],
        1,
    );
    near(
        "uncolored primitive uses separate white stream",
        pixel(&pixels, 48, 34),
        [231, 188, 124, 255],
        1,
    );
    for (double_sided, expected) in [(false, [0; 4]), (true, [255, 0, 0, 255])] {
        let bytes = scene(
            &TRIANGLE,
            &[(vec![0, 2, 1], 0, None)],
            vec![material(
                [1., 0., 0., 1.],
                "OPAQUE",
                None,
                double_sided,
                false,
            )],
            &[[0.; 3]],
            [0.; 2],
            None,
        );
        let mut visual = gpu.upload(&bytes, format);
        near(
            "back winding culling choice",
            pixel(&gpu.render(&mut visual, front_view(), format), 32, 32),
            expected,
            0,
        );
    }
    let small = [[-0.15, -0.15, 0.], [0.15, -0.15, 0.], [0., 0.15, 0.]];
    let bytes = scene(
        &small,
        &[(vec![0, 1, 2], 0, None)],
        vec![material([0., 1., 0., 1.], "OPAQUE", None, false, false)],
        &[[-0.5, 0.4, 0.2], [0.5, -0.4, -0.2]],
        [0.; 2],
        None,
    );
    let mut visual = gpu.upload(&bytes, format);
    assert_eq!(visual.stats().instance_records, 2);
    assert_eq!(visual.stats().shared_vertex_buffers, 1);
    let pixels = gpu.render(&mut visual, front_view(), format);
    near(
        "server placement left/up",
        pixel(&pixels, 16, 20),
        [0, 255, 0, 255],
        0,
    );
    near(
        "server placement right/down",
        pixel(&pixels, 48, 46),
        [0, 255, 0, 255],
        0,
    );
    near(
        "no duplicate or swizzled center placement",
        pixel(&pixels, 32, 32),
        [0; 4],
        0,
    );
    let offset_triangle = [[0.1, -0.1, 0.], [0.3, -0.1, 0.], [0.2, 0.1, 0.]];
    let bytes = scene(
        &offset_triangle,
        &[(vec![0, 1, 2], 0, None)],
        vec![material([0., 1., 0., 1.], "OPAQUE", None, false, false)],
        &[[0.; 3], [0.4, -0.3, 0.]],
        [0.; 2],
        None,
    );
    let (mut j, bin) = parts(&bytes);
    // Asset Y rotation +90 degrees, uniform scale 2: server Z rotation +90.
    j["nodes"][0]["matrix"] =
        json!([0., 0., -2., 0., 0., 2., 0., 0., 2., 0., 0., 0., -0.5, 0., -0.3, 1.]);
    let mut visual = gpu.upload(&glb(j, bin), format);
    let pixels = gpu.render(&mut visual, front_view(), format);
    near(
        "rotated scaled shared instance server landmark",
        pixel(&pixels, 16, 10),
        [0, 255, 0, 255],
        0,
    );
    near(
        "second shared instance retains its own placement",
        pixel(&pixels, 51, 43),
        [0, 255, 0, 255],
        0,
    );
}

#[test]
#[ignore = "requires explicit actual GPU acceptance run"]
fn gpu_blend_sorts_primitive_instances_on_camera_reversal_and_preserves_ties() {
    let gpu = Gpu::new(None);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let positions: Vec<_> = TRIANGLE
        .into_iter()
        .map(|mut p| {
            p[2] = -0.25;
            p
        })
        .chain(TRIANGLE.into_iter().map(|mut p| {
            p[2] = 0.25;
            p
        }))
        .collect();
    let bytes = scene(
        &positions,
        &[(vec![0, 1, 2], 0, None), (vec![3, 4, 5], 1, None)],
        vec![
            material([1., 0., 0., 0.5], "BLEND", None, true, false),
            material([0., 0., 1., 0.5], "BLEND", None, true, false),
        ],
        &[[0., 0., 0.], [0., 0., 0.8]],
        [0.; 2],
        None,
    );
    let mut visual = gpu.upload(&bytes, format);
    near(
        "front camera interleaves shared primitive-instance draws",
        pixel(&gpu.render(&mut visual, front_view(), format), 32, 32),
        [152, 0, 207, 239],
        1,
    );
    let back = Mat4::look_at_rh(Vec3::new(0., 0., -4.), Vec3::ZERO, Vec3::Y);
    near(
        "camera reversal changes primitive-instance order",
        pixel(&gpu.render(&mut visual, back, format), 32, 32),
        [207, 0, 152, 239],
        1,
    );
    let bytes = scene(
        &TRIANGLE,
        &[(vec![0, 1, 2], 0, None), (vec![0, 1, 2], 1, None)],
        vec![
            material([1., 0., 0., 0.5], "BLEND", None, true, false),
            material([0., 0., 1., 0.5], "BLEND", None, true, false),
        ],
        &[[0.; 3]],
        [0.; 2],
        None,
    );
    let mut visual = gpu.upload(&bytes, format);
    near(
        "equal-depth commands keep artifact order",
        pixel(&gpu.render(&mut visual, front_view(), format), 32, 32),
        [137, 0, 188, 191],
        1,
    );
}

#[test]
#[ignore = "requires explicit actual GPU acceptance run"]
fn gpu_repeat_bilinear_and_sixteen_bit_png_precision_reach_pixels() {
    let gpu = Gpu::new(None);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    // 32896 is exactly 128/255. 32895/65535 lies just below this cutoff;
    // The consecutive sample at the cutoff must survive. Eight-bit rounding would
    // make the below sample survive too, so coverage distinguishes retained precision.
    for (alpha, expected) in [(32895, [0; 4]), (32896, [255; 4])] {
        let bytes = scene(
            &TRIANGLE,
            &[(vec![0, 1, 2], 0, None)],
            vec![material(
                [1.; 4],
                "MASK",
                Some(32896. / 65535.),
                false,
                true,
            )],
            &[[0.; 3]],
            [0.5; 2],
            Some(png16(&[65535, 65535, 65535, alpha], 1, 1)),
        );
        let mut visual = gpu.upload(&bytes, format);
        near(
            "16-bit alpha straddles exact MASK cutoff",
            pixel(&gpu.render(&mut visual, front_view(), format), 32, 32),
            expected,
            0,
        );
    }
    // Alpha coverage above discriminates quantization; RGB establishes sRGB conversion.
    let bytes = scene(
        &TRIANGLE,
        &[(vec![0, 1, 2], 0, None)],
        vec![material([1.; 4], "OPAQUE", None, false, true)],
        &[[0.; 3]],
        [0.5; 2],
        Some(png16(&[0x1234, 0xabcd, 0x5678, 65535], 1, 1)),
    );
    let mut visual = gpu.upload(&bytes, format);
    near(
        "16-bit RGB survives linear sampling and sRGB output",
        pixel(&gpu.render(&mut visual, front_view(), format), 32, 32),
        [18, 171, 86, 255],
        1,
    );
    for uv in [[0., 0.5], [-1., 0.5], [1e30, 0.5], [1.25, 0.5]] {
        let bytes = scene(
            &TRIANGLE,
            &[(vec![0, 1, 2], 0, None)],
            vec![material([1.; 4], "OPAQUE", None, false, true)],
            &[[0.; 3]],
            uv,
            Some(png8(&[255, 0, 0, 255, 0, 0, 255, 255], 2, 1)),
        );
        let mut visual = gpu.upload(&bytes, format);
        let expected = if uv[0] == 1.25 {
            [255, 0, 0, 255]
        } else {
            [188, 0, 188, 255]
        };
        near(
            "repeat bilinear handles edge/negative/huge UV",
            pixel(&gpu.render(&mut visual, front_view(), format), 32, 32),
            expected,
            1,
        );
    }
}

#[test]
#[ignore = "requires explicit actual GPU acceptance run"]
fn gpu_rejects_invalid_camera_format_and_device_buffers_before_allocating() {
    let gpu = Gpu::new(None);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut visual = gpu.upload(FIXTURE, format);
    gpu.assert_draw_ready(&visual, false);
    visual
        .prepare_camera(&gpu.queue, front_view(), projection())
        .unwrap();
    gpu.assert_draw_ready(&visual, true);
    assert!(visual
        .prepare_camera(
            &gpu.queue,
            Mat4::from_cols_array(&[f32::NAN; 16]),
            projection()
        )
        .is_err());
    gpu.assert_draw_ready(&visual, false);
    assert!(visual
        .prepare_camera(
            &gpu.queue,
            Mat4::from_cols_array(&[f32::MAX; 16]),
            Mat4::from_cols_array(&[f32::MAX; 16])
        )
        .is_err());
    let p =
        PreparedStaticVisual::decode(FIXTURE, &DecodeLimits::default(), &UploadLimits::default())
            .unwrap();
    assert!(
        GpuStaticVisual::upload(&gpu.device, &gpu.queue, p, wgpu::TextureFormat::Bgra8Unorm)
            .is_err()
    );
    let gpu = Gpu::new(Some(64));
    let p =
        PreparedStaticVisual::decode(FIXTURE, &DecodeLimits::default(), &UploadLimits::default())
            .unwrap();
    assert!(GpuStaticVisual::upload(&gpu.device, &gpu.queue, p, format).is_err());
}

#[test]
fn draw_command_limit_bounds_blend_primitive_instance_product() {
    let primitives = vec![(vec![0, 1, 2], 0, None); 3];
    let bytes = scene(
        &TRIANGLE,
        &primitives,
        vec![material([1.; 4], "BLEND", None, true, false)],
        &[[0.; 3]; 4],
        [0.; 2],
        None,
    );
    assert!(PreparedStaticVisual::decode(
        &bytes,
        &DecodeLimits::default(),
        &UploadLimits {
            max_draw_commands: 11,
            ..UploadLimits::default()
        }
    )
    .is_err());
    let s = PreparedStaticVisual::decode(
        &bytes,
        &DecodeLimits::default(),
        &UploadLimits {
            max_draw_commands: 12,
            ..UploadLimits::default()
        },
    )
    .unwrap();
    assert_eq!(s.stats().draw_commands, 12);
    let bytes = scene(
        &TRIANGLE,
        &primitives,
        vec![material([1.; 4], "OPAQUE", None, true, false)],
        &[[0.; 3]; 4],
        [0.; 2],
        None,
    );
    assert_eq!(
        PreparedStaticVisual::decode(
            &bytes,
            &DecodeLimits::default(),
            &UploadLimits {
                max_draw_commands: 3,
                ..UploadLimits::default()
            }
        )
        .unwrap()
        .stats()
        .draw_commands,
        3
    );
    assert!(PreparedStaticVisual::decode(
        &bytes,
        &DecodeLimits::default(),
        &UploadLimits {
            max_draw_commands: 2,
            ..UploadLimits::default()
        }
    )
    .is_err());
}

#[test]
#[ignore = "requires explicit actual GPU acceptance run"]
fn gpu_depth_writes_preserve_solid_occlusion_but_allow_blend_surfaces() {
    let gpu = Gpu::new(None);
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let positions: Vec<_> = TRIANGLE
        .into_iter()
        .map(|mut p| {
            p[2] = 0.5;
            p
        })
        .chain(TRIANGLE.into_iter().map(|mut p| {
            p[2] = 0.;
            p
        }))
        .collect();
    for mode in ["OPAQUE", "MASK"] {
        let bytes = scene(
            &positions,
            &[(vec![0, 1, 2], 0, None), (vec![3, 4, 5], 1, None)],
            vec![
                material(
                    [1., 0., 0., 0.8],
                    mode,
                    if mode == "MASK" { Some(0.7) } else { None },
                    true,
                    false,
                ),
                material([0., 0., 1., 1.], "OPAQUE", None, true, false),
            ],
            &[[0.; 3]],
            [0.; 2],
            None,
        );
        let mut visual = gpu.upload(&bytes, format);
        near(
            "solid OPAQUE/MASK writes occluding depth",
            pixel(&gpu.render(&mut visual, front_view(), format), 32, 32),
            [255, 0, 0, 255],
            0,
        );
    }
    let positions = [
        [-0.8, -0.8, 0.9],
        [0.8, -0.8, 0.9],
        [0., 0.8, -2.5],
        TRIANGLE[0],
        TRIANGLE[1],
        TRIANGLE[2],
    ];
    let bytes = scene(
        &positions,
        &[(vec![0, 1, 2], 0, None), (vec![3, 4, 5], 1, None)],
        vec![
            material([1., 0., 0., 0.5], "BLEND", None, true, false),
            material([0., 0., 1., 0.5], "BLEND", None, true, false),
        ],
        &[[0.; 3]],
        [0.; 2],
        None,
    );
    let mut visual = gpu.upload(&bytes, format);
    near(
        "BLEND disables depth writes for intersecting surfaces",
        pixel(&gpu.render(&mut visual, front_view(), format), 32, 48),
        [137, 0, 188, 191],
        1,
    );
}

#[test]
#[ignore = "requires explicit actual GPU acceptance run"]
fn gpu_device_texture_dimensions_reject_before_resource_creation() {
    let gpu = Gpu::with_limits(None, Some(1));
    let bytes = scene(
        &TRIANGLE,
        &[(vec![0, 1, 2], 0, None)],
        vec![material([1.; 4], "OPAQUE", None, false, true)],
        &[[0.; 3]],
        [0.; 2],
        Some(png8(&[255; 8], 2, 1)),
    );
    let p =
        PreparedStaticVisual::decode(&bytes, &DecodeLimits::default(), &UploadLimits::default())
            .unwrap();
    let error = GpuStaticVisual::upload(
        &gpu.device,
        &gpu.queue,
        p,
        wgpu::TextureFormat::Rgba8UnormSrgb,
    )
    .err()
    .expect("oversized texture upload must fail");
    assert!(error.to_string().contains("dimension limit"));
}

#[test]
#[ignore = "requires explicit actual GPU acceptance run"]
fn gpu_display_inspection_rejects_linear_unorm_targets() {
    let gpu = Gpu::new(None);
    let prepared =
        PreparedStaticVisual::decode(FIXTURE, &DecodeLimits::default(), &UploadLimits::default())
            .unwrap();
    let error = GpuStaticVisual::upload(
        &gpu.device,
        &gpu.queue,
        prepared,
        wgpu::TextureFormat::Rgba8Unorm,
    )
    .err()
    .expect("linear output must not be mistaken for display-ready pixels");
    assert!(error.to_string().contains("sRGB output"));
}

#[test]
fn baseline_device_contract_guarantees_unfiltered_float_texture_usage() {
    let features =
        wgpu::TextureFormat::Rgba32Float.guaranteed_format_features(wgpu::Features::empty());
    assert!(features
        .allowed_usages
        .contains(wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST));
    assert!(!features
        .flags
        .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE));
}
