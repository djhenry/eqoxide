//! Validated, bounded CPU data for the source-independent static visual contract.
use super::static_decode::{
    accessors, budget, buffer_views, convert_matrix, exact_fixed, float_array, glb_chunks, idx,
    integer, map_axes, matrix, name, number, object, optional_idx, raw_field, raw_map,
    reject_duplicate_keys, req, string, vertex_layout, Accessor, AccessorProfile, View,
};
use anyhow::{bail, ensure, Context, Result};
use glam::{Mat4, Vec3};
use image::ImageDecoder;
use serde_json::{value::RawValue, Map, Value};
use std::{
    collections::BTreeMap,
    io::Cursor,
    panic::{catch_unwind, AssertUnwindSafe},
};

/// Payload and validation-work limits. These do not bound total process memory.
#[derive(Debug, Clone)]
pub struct DecodeLimits {
    /// Encoded artifact bytes, checked before JSON parsing.
    pub max_glb_bytes: usize,
    /// Encoded JSON chunk bytes, checked before allocating the JSON model.
    pub max_json_bytes: usize,
    /// Retained float attributes, u32 indices and optional float RGBA storage.
    pub max_geometry_bytes: usize,
    /// Logical bytes scanned across all accessors, including unused/aliased entries.
    pub max_accessor_validation_bytes: usize,
    /// Original PNG bytes retained across image entries, including aliases.
    pub max_png_bytes: usize,
    /// Sum of pixel-buffer sizes used to validate all image entries.
    pub max_decoded_png_bytes: usize,
    /// Per-image encoded bytes, decoded pixels and decoder allocation limit.
    pub max_single_png_bytes: usize,
    /// Maximum PNG width and height, in pixels.
    pub max_png_dimension: u32,
    /// Entries per resource collection, total primitives, and per-image PNG chunks.
    pub max_resources: usize,
    /// Paired asset/server world-position evaluations, summed over instances.
    pub max_world_vertices: usize,
}
impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_glb_bytes: 128 << 20,
            max_json_bytes: 8 << 20,
            max_geometry_bytes: 128 << 20,
            max_accessor_validation_bytes: 256 << 20,
            max_png_bytes: 128 << 20,
            max_decoded_png_bytes: 256 << 20,
            max_single_png_bytes: 64 << 20,
            max_png_dimension: 8192,
            max_resources: 100_000,
            max_world_vertices: 50_000_000,
        }
    }
}
#[derive(Debug, Clone)]
pub struct ReaderRequirements {
    pub reader_version: u32,
    pub capabilities: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct VisualHeader {
    pub schema_version: u32,
    pub role: String,
    pub coordinate_profile: String,
    pub unit_scale: f32,
    pub bake_revision: String,
    pub requirements: ReaderRequirements,
}
#[derive(Debug, Clone)]
pub struct StaticVisual {
    pub header: VisualHeader,
    pub meshes: Vec<StaticMesh>,
    pub materials: Vec<StaticMaterial>,
    pub images: Vec<StaticImage>,
    pub textures: Vec<StaticTexture>,
    pub instances: Vec<StaticInstance>,
    /// Finite server-axis framing bounds over every instanced mesh vertex pool.
    /// Unreferenced vertices are included; these are not indexed-surface or collision bounds.
    pub bounds: Bounds,
}
#[derive(Debug, Clone)]
pub struct Bounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
}
#[derive(Debug, Clone)]
pub struct StaticMesh {
    pub name: String,
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub primitives: Vec<StaticPrimitive>,
}
#[derive(Debug, Clone)]
pub struct StaticPrimitive {
    pub indices: Vec<u32>,
    pub material_index: usize,
    /// Full normalized RGBA for this primitive only, over its shared vertex pool.
    pub colors: Option<Vec<[f32; 4]>>,
}
/// Additive standard factor/explicit-unlit profile; legacy artifacts retain inspection behavior.
pub const PBR_FACTORS_CAPABILITY: &str = "pbr-metallic-roughness-factors-v1";
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StaticSurfaceModel {
    Unlit,
    Pbr { metallic: f32, roughness: f32 },
}
#[derive(Debug, Clone)]
pub struct StaticMaterial {
    pub name: String,
    pub base_color: [f32; 4],
    pub texture_index: Option<usize>,
    pub alpha_mode: StaticAlphaMode,
    pub double_sided: bool,
    pub surface: StaticSurfaceModel,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StaticAlphaMode {
    Opaque,
    Mask { cutoff: f32 },
    Blend,
}
#[derive(Debug, Clone)]
pub struct StaticImage {
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Original validated payload; 16-bit PNG input is preserved without quantization.
    pub png_bytes: Vec<u8>,
}
#[derive(Debug, Clone)]
pub struct StaticTexture {
    pub image_index: usize,
}
#[derive(Debug, Clone)]
pub struct StaticInstance {
    pub mesh_index: usize,
    /// Column-major positive-uniform affine placement in server geometry axes.
    pub matrix: [[f32; 4]; 4],
}
/// Decode only the static visual profile. This does not establish renderer or gameplay support.
/// Dependency panics become errors only when compiled with unwinding enabled;
/// `panic = "abort"` terminates the process before `catch_unwind` can recover.
pub fn decode_static_visual(bytes: &[u8], limits: &DecodeLimits) -> Result<StaticVisual> {
    catch_unwind(AssertUnwindSafe(|| decode(bytes, limits)))
        .map_err(|_| anyhow::anyhow!("static visual dependency panicked while decoding"))?
}
fn chunks<'a>(bytes: &'a [u8], limits: &DecodeLimits) -> Result<(&'a [u8], &'a [u8])> {
    let chunks = glb_chunks(bytes, limits.max_glb_bytes, limits.max_json_bytes)?;
    Ok((
        chunks.json,
        chunks.bin.context("static visual requires BIN chunk")?,
    ))
}
fn array<'a>(o: &'a Map<String, Value>, key: &str, limits: &DecodeLimits) -> Result<&'a [Value]> {
    super::static_decode::array(o, key, limits.max_resources)
}
fn normalized(n: f32) -> bool {
    n.is_finite() && (0.0..=1.0).contains(&n)
}
// Fixed header numbers are checked from original JSON tokens, before f64 parsing.
// No global arbitrary-precision feature alters existing production JSON behavior.
fn exact_header_numbers(json: &[u8]) -> Result<()> {
    let raw: &RawValue = serde_json::from_slice(json).context("invalid GLB JSON")?;
    let root = raw_map(raw)?;
    let extras = raw_map(raw_field(&root, "extras")?)?;
    let header = raw_map(raw_field(&extras, "eqoxideAsset")?)?;
    exact_fixed(raw_field(&header, "schemaVersion")?, "1", "schemaVersion")?;
    exact_fixed(raw_field(&header, "unitScale")?, "1", "unitScale")?;
    let requirements = raw_map(raw_field(&header, "requirements")?)?;
    exact_fixed(
        raw_field(&requirements, "reader_version")?,
        "2",
        "reader_version",
    )?;
    Ok(())
}
fn header(root: &Map<String, Value>) -> Result<VisualHeader> {
    let extras = object(req(root, "extras")?, &["eqoxideAsset"], "document extras")?;
    let h = object(
        req(extras, "eqoxideAsset")?,
        &[
            "schemaVersion",
            "role",
            "coordinateProfile",
            "unitScale",
            "bakeRevision",
            "requirements",
        ],
        "asset header",
    )?;
    // Original numeric tokens were checked by exact_header_numbers before this model.
    ensure!(
        req(h, "schemaVersion")?.as_f64() == Some(1.),
        "unsupported static visual schemaVersion"
    );
    ensure!(
        string(req(h, "role")?, "role")? == "visual",
        "unsupported artifact role"
    );
    ensure!(
        string(req(h, "coordinateProfile")?, "coordinateProfile")? == "eqoxide-static-y-up-v1",
        "unsupported coordinate profile"
    );
    ensure!(
        req(h, "unitScale")?.as_f64() == Some(1.),
        "unsupported unitScale"
    );
    let bake = string(req(h, "bakeRevision")?, "bakeRevision")?;
    ensure!(
        bake.len() == 64
            && bake
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid bake revision"
    );
    let r = object(
        req(h, "requirements")?,
        &["reader_version", "capabilities"],
        "requirements",
    )?;
    ensure!(
        req(r, "reader_version")?.as_f64() == Some(2.),
        "unsupported static visual reader_version"
    );
    let caps = req(r, "capabilities")?
        .as_array()
        .context("capabilities must be an array")?;
    ensure!(
        !caps.is_empty() && caps.len() <= 128,
        "invalid capability count"
    );
    let mut capabilities = Vec::with_capacity(caps.len());
    for c in caps {
        let c = string(c, "capability")?;
        ensure!(
            ["static-visual-v1", "vertex-rgba-v1", PBR_FACTORS_CAPABILITY].contains(&c),
            "unsupported required capability {c}"
        );
        ensure!(
            !capabilities.iter().any(|s| s == c),
            "duplicate required capability {c}"
        );
        capabilities.push(c.to_owned());
    }
    ensure!(
        capabilities.iter().any(|s| s == "static-visual-v1"),
        "missing static-visual-v1 requirement"
    );
    Ok(VisualHeader {
        schema_version: 1,
        role: "visual".into(),
        coordinate_profile: "eqoxide-static-y-up-v1".into(),
        unit_scale: 1.,
        bake_revision: bake.into(),
        requirements: ReaderRequirements {
            reader_version: 2,
            capabilities,
        },
    })
}
fn typed<'bin>(
    a: &[Accessor<'bin>],
    index: usize,
    lanes: usize,
    floats: bool,
) -> Result<Accessor<'bin>> {
    let a = *a.get(index).context("accessor index out of bounds")?;
    ensure!(
        a.lanes() == lanes && (a.component() == 5126) == floats,
        "accessor has unsupported type for binding"
    );
    if floats {
        vertex_layout(a)?;
    } else {
        ensure!(a.target() != Some(34962), "invalid index accessor target");
    }
    Ok(a)
}
#[derive(Clone)]
struct PrimitiveSpec<'bin> {
    indices: Accessor<'bin>,
    material: usize,
    colors: Option<Accessor<'bin>>,
}
struct MeshSpec<'bin> {
    name: String,
    positions: Accessor<'bin>,
    normals: Accessor<'bin>,
    uvs: Accessor<'bin>,
    primitives: Vec<PrimitiveSpec<'bin>>,
}
fn vector<'bin>(a: &[Accessor<'bin>], index: usize, lanes: usize) -> Result<Accessor<'bin>> {
    let a = *a.get(index).context("accessor index out of bounds")?;
    ensure!(
        a.lanes() == lanes && (a.component() == 5126 || a.normalized()),
        "UV/RGBA accessor requires float or normalized u8/u16"
    );
    vertex_layout(a)?;
    Ok(a)
}
fn meshes<'bin>(
    root: &Map<String, Value>,
    a: &[Accessor<'bin>],
    views: &[View],
    material_count: usize,
    h: &VisualHeader,
    limits: &DecodeLimits,
) -> Result<Vec<MeshSpec<'bin>>> {
    let meshes = array(root, "meshes", limits)?;
    ensure!(!meshes.is_empty(), "no static visual meshes");
    // Count distinct bound attribute accessors across the document. Reusing an
    // accessor in several primitives is not interleaving, and unused aliases
    // and index accessors do not impose a vertex buffer stride.
    let mut vertex_views = BTreeMap::new();
    for mesh in meshes {
        let mesh = mesh.as_object().context("mesh must be an object")?;
        for primitive in array(mesh, "primitives", limits)? {
            let primitive = primitive
                .as_object()
                .context("primitive must be an object")?;
            let attributes = object(
                req(primitive, "attributes")?,
                &["POSITION", "NORMAL", "TEXCOORD_0", "COLOR_0"],
                "primitive attributes",
            )?;
            for value in attributes.values() {
                let index = integer(value, "attribute accessor")?;
                let accessor = a.get(index).context("accessor index out of bounds")?;
                if let Some(previous) = vertex_views.insert(accessor.buffer_view(), index) {
                    ensure!(
                        previous == index || views[accessor.buffer_view()].stride.is_some(),
                        "shared vertex attribute bufferView requires byteStride"
                    );
                }
            }
        }
    }
    let mut bytes = 0;
    let mut primitive_count = 0;
    let has_color_support = h
        .requirements
        .capabilities
        .iter()
        .any(|s| s == "vertex-rgba-v1");
    meshes
        .iter()
        .enumerate()
        .map(|(mi, m)| {
            let m = object(m, &["name", "primitives"], "mesh")?;
            let primitives = array(m, "primitives", limits)?;
            ensure!(!primitives.is_empty(), "empty mesh");
            budget(
                &mut primitive_count,
                primitives.len(),
                1,
                limits.max_resources,
                "primitive resource count",
            )?;
            let mut pool = None;
            let mut specs = Vec::with_capacity(primitives.len());
            for (pi, p) in primitives.iter().enumerate() {
                let p = object(
                    p,
                    &["attributes", "indices", "material", "mode"],
                    "primitive",
                )?;
                ensure!(
                    optional_idx(p, "mode", 4)? == 4,
                    "only indexed triangles are supported"
                );
                let attr = object(
                    req(p, "attributes")?,
                    &["POSITION", "NORMAL", "TEXCOORD_0", "COLOR_0"],
                    "primitive attributes",
                )?;
                let indices = [
                    idx(attr, "POSITION")?,
                    idx(attr, "NORMAL")?,
                    idx(attr, "TEXCOORD_0")?,
                ];
                if let Some(pool) = pool {
                    ensure!(
                        pool == indices,
                        "mesh {mi}: primitives must share vertex pool"
                    );
                } else {
                    pool = Some(indices);
                    let positions = typed(a, indices[0], 3, true)?;
                    ensure!(
                        positions.has_bounds(),
                        "POSITION accessor requires min and max"
                    );
                    let normals = typed(a, indices[1], 3, true)?;
                    let uvs = vector(a, indices[2], 2)?;
                    ensure!(
                        positions.count() == normals.count() && positions.count() == uvs.count(),
                        "mesh attribute counts differ"
                    );
                    budget(
                        &mut bytes,
                        positions.count(),
                        32,
                        limits.max_geometry_bytes,
                        "decoded geometry",
                    )?;
                    for i in 0..normals.count() {
                        let n = Vec3::from_array(normals.float(i)?);
                        ensure!(
                            (n.length_squared() - 1.).abs() <= 1e-4,
                            "mesh normals must be unit length"
                        );
                    }
                }
                let positions = typed(a, indices[0], 3, true)?;
                let idxs = typed(a, idx(p, "indices")?, 1, false)?;
                ensure!(idxs.count() % 3 == 0, "primitive must contain triangles");
                budget(
                    &mut bytes,
                    idxs.count(),
                    4,
                    limits.max_geometry_bytes,
                    "decoded geometry",
                )?;
                for i in 0..idxs.count() {
                    ensure!(
                        (idxs.index(i)? as usize) < positions.count(),
                        "triangle index out of bounds"
                    );
                }
                let material = idx(p, "material")?;
                ensure!(
                    material < material_count,
                    "primitive material out of bounds"
                );
                let colors = attr
                    .get("COLOR_0")
                    .map(|c| vector(a, integer(c, "COLOR_0")?, 4))
                    .transpose()?;
                if let Some(c) = colors {
                    ensure!(
                        has_color_support,
                        "mesh {mi} primitive {pi}: COLOR_0 requires vertex-rgba-v1"
                    );
                    ensure!(
                        c.count() == positions.count(),
                        "RGBA color count differs from vertex pool"
                    );
                    budget(
                        &mut bytes,
                        c.count(),
                        16,
                        limits.max_geometry_bytes,
                        "decoded geometry",
                    )?;
                    for i in 0..c.count() {
                        ensure!(
                            c.float::<4>(i)?.into_iter().all(normalized),
                            "invalid normalized RGBA colors"
                        );
                    }
                }
                specs.push(PrimitiveSpec {
                    indices: idxs,
                    material,
                    colors,
                });
            }
            let pool = pool.context("missing mesh vertex pool")?;
            Ok(MeshSpec {
                name: name(m)?,
                positions: typed(a, pool[0], 3, true)?,
                normals: typed(a, pool[1], 3, true)?,
                uvs: vector(a, pool[2], 2)?,
                primitives: specs,
            })
        })
        .collect()
}
fn textures(
    root: &Map<String, Value>,
    image_count: usize,
    limits: &DecodeLimits,
) -> Result<Vec<StaticTexture>> {
    // There is no sampler choice in this profile; even unused explicit samplers are rejected.
    array(root, "textures", limits)?
        .iter()
        .map(|t| {
            let t = object(t, &["source", "name"], "texture")?;
            name(t)?;
            let source = idx(t, "source")?;
            ensure!(source < image_count, "texture image out of bounds");
            Ok(StaticTexture {
                image_index: source,
            })
        })
        .collect()
}
fn materials(
    root: &Map<String, Value>,
    texture_count: usize,
    modern: bool,
    limits: &DecodeLimits,
) -> Result<Vec<StaticMaterial>> {
    array(root, "materials", limits)?
        .iter()
        .map(|m| {
            let m = object(
                m,
                &[
                    "name",
                    "pbrMetallicRoughness",
                    "alphaMode",
                    "alphaCutoff",
                    "doubleSided",
                    "extensions",
                ],
                "material",
            )?;
            let pbr = object(
                req(m, "pbrMetallicRoughness")?,
                &[
                    "baseColorFactor",
                    "baseColorTexture",
                    "metallicFactor",
                    "roughnessFactor",
                ],
                "material PBR",
            )?;
            let unlit = if let Some(extension) = m.get("extensions") {
                ensure!(modern, "explicit unlit requires PBR factor capability");
                let extensions =
                    object(extension, &["KHR_materials_unlit"], "material extensions")?;
                let value = req(extensions, "KHR_materials_unlit")?;
                object(value, &[], "KHR_materials_unlit")?;
                true
            } else {
                false
            };
            let surface = if modern {
                let scalar = |key: &str| -> Result<f32> {
                    let Some(value) = pbr.get(key) else {
                        return Ok(1.0);
                    };
                    ensure!(
                        value
                            .as_f64()
                            .is_some_and(|v| v.is_finite() && (0.0..=1.0).contains(&v)),
                        "{key} must be normalized"
                    );
                    number(value, key)
                };
                let metallic = scalar("metallicFactor")?;
                let roughness = scalar("roughnessFactor")?;
                if unlit {
                    StaticSurfaceModel::Unlit
                } else {
                    StaticSurfaceModel::Pbr {
                        metallic,
                        roughness,
                    }
                }
            } else {
                ensure!(
                    req(pbr, "metallicFactor")?.as_f64() == Some(0.),
                    "static material requires metallicFactor 0"
                );
                ensure!(
                    req(pbr, "roughnessFactor")?.as_f64() == Some(1.),
                    "static material requires roughnessFactor 1"
                );
                StaticSurfaceModel::Unlit
            };
            let factor = req(pbr, "baseColorFactor")?;
            ensure!(
                factor.as_array().is_some_and(|a| a
                    .iter()
                    .all(|v| v.as_f64().is_some_and(|n| (0.0..=1.0).contains(&n)))),
                "baseColorFactor must be normalized"
            );
            let base_color = float_array(factor, "baseColorFactor")?;
            ensure!(
                base_color.into_iter().all(normalized),
                "material base color must be normalized RGBA"
            );
            let texture_index = pbr
                .get("baseColorTexture")
                .map(|t| {
                    let t = object(t, &["index", "texCoord"], "baseColorTexture")?;
                    ensure!(
                        optional_idx(t, "texCoord", 0)? == 0,
                        "only primary UVs are supported"
                    );
                    let index = idx(t, "index")?;
                    ensure!(index < texture_count, "material texture out of bounds");
                    Ok(index)
                })
                .transpose()?;
            let mode = m
                .get("alphaMode")
                .map(|v| string(v, "alphaMode"))
                .transpose()?
                .unwrap_or("OPAQUE");
            let cutoff = m
                .get("alphaCutoff")
                .map(|v| {
                    ensure!(
                        v.as_f64().is_some_and(|n| (0.0..=1.0).contains(&n)),
                        "alphaCutoff must be normalized"
                    );
                    number(v, "alphaCutoff")
                })
                .transpose()?;
            ensure!(
                cutoff.is_none_or(normalized),
                "material alpha cutoff must be normalized"
            );
            let alpha_mode = match mode {
                "OPAQUE" => StaticAlphaMode::Opaque,
                "BLEND" => StaticAlphaMode::Blend,
                "MASK" => StaticAlphaMode::Mask {
                    cutoff: cutoff.unwrap_or(0.5),
                },
                _ => bail!("unsupported alphaMode {mode}"),
            };
            // A cutoff only affects MASK. Reject irrelevant authoring fields rather than discard them.
            ensure!(
                mode == "MASK" || cutoff.is_none(),
                "alphaCutoff requires MASK mode"
            );
            ensure!(
                !matches!(surface, StaticSurfaceModel::Pbr { .. })
                    || alpha_mode != StaticAlphaMode::Blend,
                "lit BLEND requires a later HDR transparency profile"
            );
            let double_sided = m
                .get("doubleSided")
                .map(|v| v.as_bool().context("doubleSided must be boolean"))
                .transpose()?
                .unwrap_or(false);
            Ok(StaticMaterial {
                name: name(m)?,
                base_color,
                texture_index,
                alpha_mode,
                double_sided,
                surface,
            })
        })
        .collect()
}
struct ImageSpec {
    name: String,
    view: View,
    width: u32,
    height: u32,
    decoded_bytes: usize,
}
// Image decoding may stop after IDAT. Validate the complete original stream too,
// so missing IEND, trailing payloads, bad ancillary CRCs and APNG cannot be hidden.
fn png_stream(png: &[u8], limits: &DecodeLimits) -> Result<()> {
    ensure!(
        png.starts_with(b"\x89PNG\r\n\x1a\n"),
        "invalid PNG signature"
    );
    let mut offset = 8usize;
    let mut chunks = 0usize;
    let mut ended = false;
    while offset < png.len() {
        chunks = chunks.checked_add(1).context("PNG chunk count overflow")?;
        ensure!(
            chunks <= limits.max_resources,
            "PNG chunk count exceeds limit"
        );
        let header = offset.checked_add(8).context("PNG chunk overflow")?;
        let bytes = png
            .get(offset..header)
            .context("truncated PNG chunk header")?;
        let len = u32::from_be_bytes(bytes[..4].try_into()?) as usize;
        let data_end = header.checked_add(len).context("PNG chunk size overflow")?;
        let end = data_end.checked_add(4).context("PNG CRC offset overflow")?;
        ensure!(end <= png.len(), "truncated PNG chunk payload");
        let kind = &bytes[4..];
        ensure!(
            kind.iter().all(u8::is_ascii_alphabetic),
            "invalid PNG chunk type"
        );
        ensure!(
            ![b"acTL", b"fcTL", b"fdAT"].contains(&kind.try_into()?),
            "animated PNG is outside static profile"
        );
        if chunks == 1 {
            ensure!(kind == b"IHDR" && len == 13, "PNG must start with IHDR");
        } else {
            ensure!(kind != b"IHDR", "duplicate PNG IHDR");
        }
        let crc = u32::from_be_bytes(png[data_end..end].try_into()?);
        ensure!(
            crc32fast::hash(&png[offset + 4..data_end]) == crc,
            "invalid PNG chunk CRC"
        );
        if kind == b"IEND" {
            ensure!(
                len == 0 && end == png.len(),
                "invalid PNG end or trailing data"
            );
            ended = true;
        }
        offset = end;
    }
    ensure!(ended, "PNG missing IEND");
    Ok(())
}
fn png_decoder<'a>(png: &'a [u8], limits: &DecodeLimits) -> Result<impl ImageDecoder + 'a> {
    let mut r = image::ImageReader::with_format(Cursor::new(png), image::ImageFormat::Png);
    let mut l = image::Limits::default();
    l.max_image_width = Some(limits.max_png_dimension);
    l.max_image_height = Some(limits.max_png_dimension);
    l.max_alloc = Some(limits.max_single_png_bytes as u64);
    r.limits(l);
    r.into_decoder().context("invalid or excessive PNG header")
}
fn images(
    root: &Map<String, Value>,
    views: &[View],
    bin: &[u8],
    limits: &DecodeLimits,
) -> Result<Vec<ImageSpec>> {
    let mut retained = 0;
    let mut decoded = 0;
    let images = array(root, "images", limits)?
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let v = object(v, &["bufferView", "mimeType", "name"], "image")?;
            ensure!(
                string(req(v, "mimeType")?, "image mimeType")? == "image/png",
                "only embedded PNG images are supported"
            );
            let view = *views
                .get(idx(v, "bufferView")?)
                .context("image bufferView out of bounds")?;
            ensure!(
                view.stride.is_none() && view.target.is_none(),
                "PNG bufferView must have no stride or target"
            );
            ensure!(
                view.len <= limits.max_single_png_bytes,
                "image {i} encoded PNG exceeds limit"
            );
            budget(
                &mut retained,
                view.len,
                1,
                limits.max_png_bytes,
                "retained PNG bytes",
            )?;
            let png = &bin[view.offset..view.offset + view.len];
            png_stream(png, limits).with_context(|| format!("image {i}"))?;
            let decoder = png_decoder(png, limits).with_context(|| format!("image {i}"))?;
            let (width, height) = decoder.dimensions();
            ensure!(width > 0 && height > 0, "empty PNG dimensions");
            let decoded_bytes =
                usize::try_from(decoder.total_bytes()).context("PNG decoded size overflow")?;
            ensure!(
                decoded_bytes <= limits.max_single_png_bytes,
                "image {i} decoded PNG exceeds allocation limit"
            );
            budget(
                &mut decoded,
                decoded_bytes,
                1,
                limits.max_decoded_png_bytes,
                "decoded PNG validation bytes",
            )?;
            Ok(ImageSpec {
                name: name(v)?,
                view,
                width,
                height,
                decoded_bytes,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    // Every image header and aggregate allocation has passed before a full decode or payload copy.
    for (i, im) in images.iter().enumerate() {
        let png = &bin[im.view.offset..im.view.offset + im.view.len];
        let decoder = png_decoder(png, limits)?;
        let mut pixels = vec![0; im.decoded_bytes];
        decoder
            .read_image(&mut pixels)
            .with_context(|| format!("image {i} invalid PNG pixels"))?;
    }
    Ok(images)
}
fn instances(
    root: &Map<String, Value>,
    meshes: &[MeshSpec],
    limits: &DecodeLimits,
) -> Result<(Vec<StaticInstance>, Bounds)> {
    let nodes = array(root, "nodes", limits)?;
    ensure!(!nodes.is_empty(), "no static instances");
    let scenes = array(root, "scenes", limits)?;
    ensure!(
        scenes.len() == 1 && idx(root, "scene")? == 0,
        "one active flat scene is required"
    );
    let scene = object(&scenes[0], &["nodes", "name"], "scene")?;
    name(scene)?;
    let roots = array(scene, "nodes", limits)?;
    ensure!(
        roots.len() == nodes.len(),
        "all nodes must be flat scene instances"
    );
    let mut seen = vec![false; nodes.len()];
    for n in roots {
        let n = integer(n, "scene node")?;
        ensure!(
            n < nodes.len() && !seen[n],
            "invalid or duplicate scene node"
        );
        seen[n] = true;
    }
    // Preflight the instance × shared-pool work product before transforming any vertex.
    let mut world_vertices = 0;
    for node in nodes {
        let node = object(node, &["mesh", "matrix", "name"], "node")?;
        let mesh = meshes
            .get(idx(node, "mesh")?)
            .context("node mesh out of bounds")?;
        budget(
            &mut world_vertices,
            mesh.positions.count(),
            1,
            limits.max_world_vertices,
            "world vertex evaluations",
        )?;
    }
    let mut bounds = Bounds {
        min: [f32::INFINITY; 3],
        max: [f32::NEG_INFINITY; 3],
    };
    let instances = nodes
        .iter()
        .enumerate()
        .map(|(ni, node)| {
            let node = object(node, &["mesh", "matrix", "name"], "node")?;
            name(node)?;
            let mesh_index = idx(node, "mesh")?;
            let mesh = meshes.get(mesh_index).context("node mesh out of bounds")?;
            let source = matrix(node.get("matrix")).with_context(|| format!("node {ni}"))?;
            let converted = convert_matrix(source);
            let source = Mat4::from_cols_array_2d(&source);
            let server = Mat4::from_cols_array_2d(&converted);
            ensure!(server.is_finite(), "converted node matrix is nonfinite");
            for i in 0..mesh.positions.count() {
                let p = mesh.positions.float(i)?;
                ensure!(
                    source.transform_point3(Vec3::from_array(p)).is_finite(),
                    "nonfinite asset world position"
                );
                let p = server
                    .transform_point3(Vec3::from_array(map_axes(p)))
                    .to_array();
                ensure!(
                    p.into_iter().all(f32::is_finite),
                    "nonfinite server world position"
                );
                for k in 0..3 {
                    bounds.min[k] = bounds.min[k].min(p[k]);
                    bounds.max[k] = bounds.max[k].max(p[k]);
                }
            }
            Ok(StaticInstance {
                mesh_index,
                matrix: converted,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((instances, bounds))
}
// Compare decimal source values before binary rounding can hide a range violation.
fn exact_unit_interval(token: &str) -> bool {
    let (mantissa, exponent) = token
        .find(['e', 'E'])
        .map_or((token, "0"), |i| (&token[..i], &token[i + 1..]));
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.strip_prefix('-').unwrap_or(mantissa);
    if !mantissa.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return false;
    }
    let integer_digits = mantissa.find('.').unwrap_or(mantissa.len()) as i64;
    let mut digits = mantissa.bytes().filter(|b| *b != b'.');
    let Some((first_position, first)) = digits.by_ref().enumerate().find(|(_, b)| *b != b'0')
    else {
        return mantissa.bytes().any(|b| b.is_ascii_digit());
    };
    if negative {
        return false;
    }
    let sign = if exponent.starts_with('-') { -1i64 } else { 1 };
    let exponent_digits = exponent.trim_start_matches(['+', '-']);
    let Some(exp) = exponent_digits.bytes().try_fold(0i64, |n, b| {
        b.is_ascii_digit()
            .then(|| n.saturating_mul(10).saturating_add(i64::from(b - b'0')))
    }) else {
        return false;
    };
    let exponent = exp.saturating_mul(sign);
    let order = integer_digits
        .saturating_sub(first_position as i64)
        .saturating_sub(1)
        .saturating_add(exponent);
    order < 0 || (order == 0 && first == b'1' && digits.all(|b| b == b'0'))
}
fn exact_factor_bounds(bytes: &[u8]) -> Result<()> {
    let raw: &RawValue = serde_json::from_slice(bytes)?;
    let root = raw_map(raw)?;
    // The typed material array's resource bound has already been checked.
    let materials: Vec<&RawValue> = serde_json::from_str(raw_field(&root, "materials")?.get())?;
    for material in materials {
        let material = raw_map(material)?;
        let pbr = raw_map(raw_field(&material, "pbrMetallicRoughness")?)?;
        for key in ["metallicFactor", "roughnessFactor"] {
            if let Some(value) = pbr.get(key) {
                ensure!(
                    exact_unit_interval(value.get()),
                    "{key} source value must lie in [0,1]"
                );
            }
        }
    }
    Ok(())
}
fn validate_unlit_declarations(
    root: &Map<String, Value>,
    modern: bool,
    limits: &DecodeLimits,
) -> Result<()> {
    let used = array(root, "extensionsUsed", limits)?;
    let required = array(root, "extensionsRequired", limits)?;
    let uses_unlit = array(root, "materials", limits)?
        .iter()
        .any(|m| m.get("extensions").is_some());
    if uses_unlit {
        ensure!(
            modern && used == [Value::String("KHR_materials_unlit".into())] && required == used,
            "unlit materials require matching used/required KHR_materials_unlit declarations"
        );
    } else {
        ensure!(
            used.is_empty() && required.is_empty(),
            "unused or unsupported required material extension"
        );
    }
    Ok(())
}
fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<StaticVisual> {
    let (json_bytes, bin) = chunks(bytes, limits)?;
    reject_duplicate_keys(json_bytes)?;
    exact_header_numbers(json_bytes)?;
    let json: Value = serde_json::from_slice(json_bytes).context("invalid GLB JSON")?;
    let root = object(
        &json,
        &[
            "asset",
            "extras",
            "scene",
            "scenes",
            "nodes",
            "meshes",
            "materials",
            "textures",
            "images",
            "buffers",
            "bufferViews",
            "accessors",
            "extensionsUsed",
            "extensionsRequired",
        ],
        "static visual document",
    )?;
    let asset = object(
        req(root, "asset")?,
        &["version", "generator", "copyright", "minVersion"],
        "glTF asset",
    )?;
    ensure!(
        string(req(asset, "version")?, "asset version")? == "2.0",
        "unsupported glTF asset version"
    );
    for key in ["generator", "copyright"] {
        if let Some(s) = asset.get(key) {
            string(s, key)?;
        }
    }
    if let Some(v) = asset.get("minVersion") {
        ensure!(string(v, "minVersion")? == "2.0", "unsupported minVersion");
    }
    let h = header(root)?;
    let modern = h
        .requirements
        .capabilities
        .iter()
        .any(|c| c == PBR_FACTORS_CAPABILITY);
    if modern {
        array(root, "materials", limits)?;
        exact_factor_bounds(json_bytes)?;
    } else {
        ensure!(
            !root.contains_key("extensionsUsed") && !root.contains_key("extensionsRequired"),
            "legacy inspection does not declare extensions"
        );
    }
    let views = buffer_views(root, bin, limits.max_resources, AccessorProfile::Visual)?;
    let accessors = accessors(
        root,
        &views,
        bin,
        limits.max_resources,
        AccessorProfile::Visual,
        limits.max_accessor_validation_bytes,
    )?;
    // No retained binary arrays are copied until all resources and budgets have passed.
    let image_count = array(root, "images", limits)?.len();
    let textures = textures(root, image_count, limits)?;
    let materials = materials(root, textures.len(), modern, limits)?;
    validate_unlit_declarations(root, modern, limits)?;
    let specs = meshes(root, &accessors, &views, materials.len(), &h, limits)?;
    let (instances, bounds) = instances(root, &specs, limits)?;
    let image_specs = images(root, &views, bin, limits)?;
    let meshes = specs
        .into_iter()
        .map(|m| -> Result<StaticMesh> {
            Ok(StaticMesh {
                name: m.name,
                positions: (0..m.positions.count())
                    .map(|i| Ok(map_axes(m.positions.float(i)?)))
                    .collect::<Result<_>>()?,
                normals: (0..m.normals.count())
                    .map(|i| Ok(map_axes(m.normals.float(i)?)))
                    .collect::<Result<_>>()?,
                uvs: (0..m.uvs.count())
                    .map(|i| m.uvs.float(i))
                    .collect::<Result<_>>()?,
                primitives: m
                    .primitives
                    .into_iter()
                    .map(|p| -> Result<StaticPrimitive> {
                        Ok(StaticPrimitive {
                            indices: (0..p.indices.count())
                                .map(|i| p.indices.index(i))
                                .collect::<Result<_>>()?,
                            material_index: p.material,
                            colors: p
                                .colors
                                .map(|c| {
                                    (0..c.count())
                                        .map(|i| c.float(i))
                                        .collect::<Result<Vec<_>>>()
                                })
                                .transpose()?,
                        })
                    })
                    .collect::<Result<_>>()?,
            })
        })
        .collect::<Result<_>>()?;
    let images = image_specs
        .into_iter()
        .map(|i| StaticImage {
            name: i.name,
            width: i.width,
            height: i.height,
            png_bytes: bin[i.view.offset..i.view.offset + i.view.len].to_vec(),
        })
        .collect();
    Ok(StaticVisual {
        header: h,
        meshes,
        materials,
        images,
        textures,
        instances,
        bounds,
    })
}
