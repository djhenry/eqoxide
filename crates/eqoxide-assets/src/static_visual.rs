//! Validated, bounded CPU data for the source-independent static visual contract.
use anyhow::{bail, ensure, Context, Result};
use glam::{DMat4, DVec3, Mat4, Vec3};
use image::ImageDecoder;
use serde_json::{value::RawValue, Map, Value};
use std::{collections::BTreeMap, io::Cursor, panic::{catch_unwind, AssertUnwindSafe}};

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
            max_world_vertices: 50_000_000
        }
    }
}
#[derive(Debug, Clone)]
pub struct ReaderRequirements {
    pub reader_version: u32,
    pub capabilities: Vec<String>
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
    /// Finite world bounds in server geometry axes, including all instance pools.
    pub bounds: Bounds,
}
#[derive(Debug, Clone)]
pub struct Bounds {
    pub min: [f32; 3],
    pub max: [f32; 3]
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
#[derive(Debug, Clone)]
pub struct StaticMaterial {
    pub name: String,
    pub base_color: [f32; 4],
    pub texture_index: Option<usize>,
    pub alpha_mode: StaticAlphaMode,
    pub double_sided: bool,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StaticAlphaMode {
    Opaque,
    Mask { cutoff: f32 },
    Blend
}
#[derive(Debug, Clone)]
pub struct StaticImage {
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Original validated payload; 16-bit PNG input is preserved without quantization.
    pub png_bytes: Vec<u8>
}
#[derive(Debug, Clone)]
pub struct StaticTexture {
    pub image_index: usize
}
#[derive(Debug, Clone)]
pub struct StaticInstance {
    pub mesh_index: usize,
    /// Column-major positive-uniform affine placement in server geometry axes.
    pub matrix: [[f32; 4]; 4]
}
/// Decode only the static visual profile. This does not establish renderer or gameplay support.
pub fn decode_static_visual(bytes: &[u8], limits: &DecodeLimits) -> Result<StaticVisual> {
    catch_unwind(AssertUnwindSafe(|| decode(bytes, limits)))
    .map_err(|_| anyhow::anyhow!("static visual dependency panicked while decoding"))?
}
// Parsing does not use glTF import: document URIs can never cause file/network reads.
fn chunks<'a>(bytes: &'a [u8], limits: &DecodeLimits) -> Result<(&'a [u8], &'a [u8])> {
    ensure!(bytes.len() <= limits.max_glb_bytes, "encoded GLB exceeds byte limit");
    ensure!(bytes.len() >= 28, "truncated GLB header");
    ensure!(&bytes[..4] == b"glTF" && word(bytes, 4)? == 2, "expected GLB version 2");
    ensure!(word(bytes, 8)? as usize == bytes.len(), "GLB declared length mismatch");
    let json_len = word(bytes, 12)? as usize;
    ensure!(json_len <= limits.max_json_bytes && json_len % 4 == 0, "invalid or excessive JSON chunk");
    ensure!(&bytes[16..20] == b"JSON", "first GLB chunk must be JSON");
    let json_end = 20usize.checked_add(json_len).context("JSON size overflow")?;
    let next = json_end.checked_add(8).context("GLB chunk overflow")?;
    ensure!(next <= bytes.len(), "truncated GLB JSON/BIN chunks");
    ensure!(&bytes[json_end + 4..next] == b"BIN\0", "second GLB chunk must be BIN");
    let bin_len = word(bytes, json_end)? as usize;
    ensure!(bin_len % 4 == 0 && next.checked_add(bin_len) == Some(bytes.len()),
    "invalid BIN length or unsupported trailing chunk");
    Ok((&bytes[20..json_end], &bytes[next..]))
}
fn word(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset.checked_add(4).context("word offset overflow")?;
    Ok(u32::from_le_bytes(bytes.get(offset..end).context("truncated word")?.try_into()?))
}
fn object<'a>(value: &'a Value, keys: &[&str], label: &str) -> Result<&'a Map<String, Value>> {
    let o = value.as_object().with_context(|| format!("{label} must be an object"))?;
    for key in o.keys() {
        ensure!(keys.contains(&key.as_str()), "{label}: unsupported field {key}");
    }
    Ok(o)
}
fn req<'a>(o: &'a Map<String, Value>, key: &str) -> Result<&'a Value> {
    o.get(key).with_context(|| format!("missing {key}"))
}
fn string<'a>(v: &'a Value, label: &str) -> Result<&'a str> {
    v.as_str().with_context(|| format!("{label} must be a string"))
}
// Resource descriptors use canonical JSON unsigned integers. Floating-point spellings
// are deliberately excluded; only fixed header numbers have decimal equivalence.
fn integer(v: &Value, label: &str) -> Result<usize> {
    let n = v.as_u64().with_context(|| format!("{label} must be a JSON unsigned integer"))?;
    usize::try_from(n).with_context(|| format!("{label} out of range"))
}
fn idx(o: &Map<String, Value>, key: &str) -> Result<usize> {
    integer(req(o, key)?, key)
}
fn optional_idx(o: &Map<String, Value>, key: &str, default: usize) -> Result<usize> {
    o.get(key).map(|v|integer(v, key)).transpose().map(|v|v.unwrap_or(default))
}
fn number(v: &Value, label: &str) -> Result<f32> {
    let n = v.as_f64().with_context(|| format!("{label} must be numeric"))? as f32;
    ensure!(n.is_finite(), "{label} must be finite f32");
    Ok(n)
}
fn array<'a>(o: &'a Map<String, Value>, key: &str, limits: &DecodeLimits) -> Result<&'a [Value]> {
    let v = match o.get(key) {
        Some(v) => v.as_array().with_context(||format!("{key} must be an array"))?,
        None => return Ok(&[])
    };
    ensure!(v.len() <= limits.max_resources, "{key} resource count exceeds limit");
    Ok(v)
}
fn name(o: &Map<String, Value>) -> Result<String> {
    o.get("name").map(|v|string(v, "name").map(str::to_owned)).transpose().map(|n|n.unwrap_or_default())
}
fn normalized(n: f32) -> bool {
    n.is_finite() && (0.0..=1.0).contains(&n)
}
fn float_array<const N: usize>(v: &Value, label: &str) -> Result<[f32;N]> {
    let a = v.as_array().with_context(||format!("{label} must be an array"))?;
    ensure!(a.len()==N, "{label} must contain {N} values");
    let mut out=[0.;N];
    for (i, n) in a.iter().enumerate() {
        out[i]=number(n, label)?;
    } Ok(out)
}
// Fixed header numbers are checked from original JSON tokens, before f64 parsing.
// No global arbitrary-precision feature alters existing production JSON behavior.
fn exact_header_numbers(json: &[u8]) -> Result<()> {
    fn map(raw: &RawValue) -> Result<BTreeMap<String,
    &RawValue>> {
        serde_json::from_str(raw.get()).context("asset header must be an object")
    }
    fn field<'a>(map: &BTreeMap<String, &'a RawValue>, key: &str) -> Result<&'a RawValue> {
        map.get(key).copied().with_context(||format!("missing {key}"))
    }
    fn exact(raw: &RawValue, expected: &str, label: &str) -> Result<()> {
        let token=raw.get();
        let (mantissa, exponent)=match token.find(['e', 'E']) {
            Some(e)=>( &token[..e], token[e+1..].parse::<i64>().context("header exponent out of range")?),
            None=>(token, 0),
        };
        let fraction=mantissa.find('.').map(|dot|mantissa.len()-dot-1).unwrap_or(0);
        ensure!(mantissa.bytes().all(|b|b.is_ascii_digit() || b==b'.'), "{label} must be exact {expected}");
        let digits: String=mantissa.chars().filter(|c|*c!='.').collect();
        let digits=digits.trim_start_matches('0');
        let significant=digits.trim_end_matches('0');
        let zeros=digits.len()-significant.len();
        let scale=exponent.checked_sub(i64::try_from(fraction)?).and_then(|s|s.checked_add(zeros as i64))
        .context("header decimal scale overflow")?;
        ensure!(significant==expected && scale==0, "{label} must be exact {expected}");
        Ok(())
    }
    let raw: &RawValue=serde_json::from_slice(json).context("invalid GLB JSON")?;
    let root=map(raw)?;
    let extras=map(field(&root, "extras")?)?;
    let header=map(field(&extras, "eqoxideAsset")?)?;
    exact(field(&header, "schemaVersion")?, "1", "schemaVersion")?;
    exact(field(&header, "unitScale")?, "1", "unitScale")?;
    let requirements=map(field(&header, "requirements")?)?;
    exact(field(&requirements, "reader_version")?, "2", "reader_version")?;
    Ok(())
}
fn header(root: &Map<String, Value>) -> Result<VisualHeader> {
    let extras=object(req(root, "extras")?, &["eqoxideAsset"], "document extras")?;
    let h=object(req(extras, "eqoxideAsset")?, &["schemaVersion", "role", "coordinateProfile", "unitScale", "bakeRevision", "requirements"], "asset header")?;
    // Original numeric tokens were checked by exact_header_numbers before this model.
    ensure!(req(h, "schemaVersion")?.as_f64() == Some(1.), "unsupported static visual schemaVersion");
    ensure!(string(req(h, "role")?, "role")? == "visual", "unsupported artifact role");
    ensure!(string(req(h, "coordinateProfile")?, "coordinateProfile")? == "eqoxide-static-y-up-v1", "unsupported coordinate profile");
    ensure!(req(h, "unitScale")?.as_f64()==Some(1.), "unsupported unitScale");
    let bake=string(req(h, "bakeRevision")?, "bakeRevision")?;
    ensure!(bake.len()==64 && bake.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "invalid bake revision");
    let r=object(req(h, "requirements")?, &["reader_version", "capabilities"], "requirements")?;
    ensure!(req(r, "reader_version")?.as_f64() == Some(2.), "unsupported static visual reader_version");
    let caps=req(r, "capabilities")?.as_array().context("capabilities must be an array")?;
    ensure!(!caps.is_empty() && caps.len() <= 128, "invalid capability count");
    let mut capabilities=Vec::with_capacity(caps.len());
    for c in caps {
        let c=string(c, "capability")?;
        ensure!(["static-visual-v1", "vertex-rgba-v1"].contains(&c), "unsupported required capability {c}");
        ensure!(!capabilities.iter().any(|s|s==c), "duplicate required capability {c}");
        capabilities.push(c.to_owned());
    }
    ensure!(capabilities.iter().any(|s|s=="static-visual-v1"), "missing static-visual-v1 requirement");
    Ok(VisualHeader {
        schema_version: 1,
        role: "visual".into(),
        coordinate_profile: "eqoxide-static-y-up-v1".into(),
        unit_scale: 1.,
        bake_revision: bake.into(),
        requirements: ReaderRequirements{
            reader_version: 2,
            capabilities
        }
    })
}
#[derive(Clone, Copy)]
struct View {
    offset: usize,
    len: usize,
    stride: Option<usize>,
    target: Option<usize>
}
#[derive(Clone, Copy)]
struct Accessor {
    offset: usize,
    count: usize,
    stride: usize,
    lanes: usize,
    component: usize,
    normalized: bool,
    target: Option<usize>,
    has_bounds: bool
}
impl Accessor {
    fn float<const N: usize>(self, bin: &[u8], i: usize) -> [f32;N] {
        let offset=self.offset+i*self.stride;
        std::array::from_fn(|n| match self.component {
            5121 => bin[offset+n] as f32 / 255.,
            5123 => u16::from_le_bytes(bin[offset+n*2..offset+n*2+2].try_into().unwrap()) as f32 / 65535.,
            _ => f32::from_le_bytes(bin[offset+n*4..offset+n*4+4].try_into().unwrap()),
        })
    }
    fn index(self, bin: &[u8], i: usize) -> u32 {
        let o=self.offset+i*self.stride;
        match self.component {
            5121=>bin[o] as u32,
            5123=>u16::from_le_bytes(bin[o..o+2].try_into().unwrap()) as u32,
            _=>u32::from_le_bytes(bin[o..o+4].try_into().unwrap())
        }
    }
}
fn buffer_views(root: &Map<String, Value>, bin: &[u8], limits: &DecodeLimits) -> Result<Vec<View>> {
    let buffers=array(root, "buffers", limits)?;
    ensure!(buffers.len()==1, "static visual requires exactly one BIN buffer");
    let b=object(&buffers[0], &["byteLength", "name"], "buffer")?;
    name(b)?;
    let len=idx(b, "byteLength")?;
    ensure!(len <= bin.len() && bin.len()-len <= 3, "BIN buffer length mismatch");
    ensure!(bin[len..].iter().all(|b|*b==0), "invalid BIN padding");
    array(root, "bufferViews", limits)?.iter().map(|v| {
        let v=object(v, &["buffer", "byteOffset", "byteLength", "byteStride", "target", "name"], "bufferView")?;
        name(v)?;
        ensure!(idx(v, "buffer")?==0, "bufferView must reference BIN buffer");
        let offset=optional_idx(v, "byteOffset", 0)?;
        let size=idx(v, "byteLength")?;
        ensure!(size>0 && offset.checked_add(size).is_some_and(|end|end<=len), "bufferView exceeds BIN bounds");
        let stride=v.get("byteStride").map(|s|integer(s, "byteStride")).transpose()?;
        ensure!(stride.is_none_or(|s|(4..=252).contains(&s) && s%4==0), "invalid bufferView byteStride");
        let target=v.get("target").map(|s|integer(s, "target")).transpose()?;
        ensure!(target.is_none_or(|s|s==34962 || s==34963), "invalid bufferView target");
        Ok(View {
            offset,
            len: size,
            stride,
            target
        })
    }).collect()
}
fn accessors(root: &Map<String, Value>, views: &[View], bin: &[u8], limits: &DecodeLimits) -> Result<Vec<Accessor>> {
    let raw = array(root, "accessors", limits)?;
    let mut validation_bytes = 0;
    // Validate every descriptor and the aggregate alias work budget before reading
    // any accessor payload. Unused entries still consume this validation budget.
    let accessors = raw.iter().enumerate().map(|(ai, v)| {
        let v=object(v, &["bufferView", "byteOffset", "componentType", "count", "type", "min", "max", "normalized", "name"], "accessor")?;
        name(v)?;
        let view=views.get(idx(v, "bufferView")?).context("accessor bufferView out of bounds")?;
        let component=idx(v, "componentType")?;
        let width=match component {
            5121=>1,
            5123=>2,
            5125|5126=>4,
            _=>bail!("accessor {ai}: unsupported componentType")
        };
        let lanes=match string(req(v, "type")?, "accessor type")? {
            "SCALAR"=>1,
            "VEC2"=>2,
            "VEC3"=>3,
            "VEC4"=>4,
            _=>bail!("accessor {ai}: unsupported type")
        };
        let normalized=v.get("normalized").map(|n|n.as_bool().context("normalized must be boolean")).transpose()?.unwrap_or(false);
        ensure!((component==5126 && !normalized) || (lanes==1 && !normalized) ||
        ([5121, 5123].contains(&component) && normalized && [2, 4].contains(&lanes)),
        "accessor requires float vectors, unsigned indices, or normalized u8/u16 UV2/RGBA4");
        let count=idx(v, "count")?;
        ensure!(count>0, "empty accessor");
        let offset=optional_idx(v, "byteOffset", 0)?;
        let absolute=view.offset.checked_add(offset).context("accessor offset overflow")?;
        ensure!(offset%width==0 && absolute%width==0, "misaligned accessor");
        let packed=width*lanes;
        let stride=view.stride.unwrap_or(packed);
        ensure!(stride>=packed, "accessor exceeds byteStride");
        ensure!(view.target!=Some(34963) || view.stride.is_none(), "indices cannot be interleaved");
        let span=(count-1).checked_mul(stride).and_then(|n|n.checked_add(packed)).context("accessor size overflow")?;
        ensure!(offset.checked_add(span).is_some_and(|end|end<=view.len), "accessor {ai} exceeds bufferView bounds");
        let a=Accessor {
            offset: absolute,
            count,
            stride,
            lanes,
            component,
            normalized,
            target: view.target,
            has_bounds: v.contains_key("min") && v.contains_key("max")
        };
        budget(&mut validation_bytes, count, packed,
            limits.max_accessor_validation_bytes, "accessor validation bytes")?;
        Ok(a)
    }).collect::<Result<Vec<_>>>()?;
    for (ai, a) in accessors.iter().enumerate() {
        let v = raw[ai].as_object().context("accessor must be an object")?;
        let Accessor { offset: absolute, count, stride, lanes, component, .. } = *a;
        let mut actual_min=[f64::INFINITY;4];
        let mut actual_max=[f64::NEG_INFINITY;4];
        for i in 0..count {
            let values: [f64;4]=if component==5126 {
                std::array::from_fn(|n|if n<lanes {
                    let o=absolute+i*stride+n*4;
                    f32::from_le_bytes(bin[o..o+4].try_into().unwrap()) as f64
                } else {
                    0.
                })
            } else if lanes==1 {
                [a.index(bin, i) as f64, 0., 0., 0.]
            } else {
                let width=if component==5121 {
                    1
                } else {
                    2
                };
                std::array::from_fn(|n|if n<lanes {
                    let o=absolute+i*stride+n*width;
                    if component==5121 {
                        bin[o] as f64
                    } else {
                        u16::from_le_bytes(bin[o..o+2].try_into().unwrap()) as f64
                    }
                } else {
                    0.
                })
            };
            for lane in 0..lanes {
                ensure!(values[lane].is_finite(), "accessor {ai} nonfinite value");
                actual_min[lane]=actual_min[lane].min(values[lane]);
                actual_max[lane]=actual_max[lane].max(values[lane]);
            }
        }
        for (key, actual) in [("min", actual_min), ("max", actual_max)] {
            if let Some(bounds)=v.get(key) {
                let bounds=bounds.as_array().context("accessor bounds must be arrays")?;
                ensure!(bounds.len()==lanes, "invalid accessor bounds count");
                for (lane, value) in bounds.iter().enumerate() {
                    let declared=if component==5126 {
                        number(value, "accessor bound")? as f64
                    } else {
                        integer(value, "accessor bound")? as f64
                    };
                    ensure!(declared==actual[lane], "accessor {ai}: {key} does not match actual data");
                }
            }
        }
    }
    Ok(accessors)
}
fn budget(total: &mut usize, count: usize, stride: usize, limit: usize, label: &str) -> Result<()> {
    *total=total.checked_add(count.checked_mul(stride).with_context(||format!("{label} size overflow"))?)
    .with_context(||format!("{label} aggregate overflow"))?;
    ensure!(*total<=limit, "{label} exceeds limit");
    Ok(())
}
fn typed(a: &[Accessor], index: usize, lanes: usize, floats: bool) -> Result<Accessor> {
    let a=*a.get(index).context("accessor index out of bounds")?;
    ensure!(a.lanes==lanes && (a.component==5126)==floats, "accessor has unsupported type for binding");
    if floats {
        ensure!(a.target!=Some(34963) && a.offset%4==0 && a.stride%4==0, "invalid vertex accessor layout/target");
    } else {
        ensure!(a.target!=Some(34962), "invalid index accessor target");
    }
    Ok(a)
}
#[derive(Clone)]
struct PrimitiveSpec {
    indices: Accessor,
    material: usize,
    colors: Option<Accessor>
}
struct MeshSpec {
    name: String,
    positions: Accessor,
    normals: Accessor,
    uvs: Accessor,
    primitives: Vec<PrimitiveSpec>
}
fn vector(a: &[Accessor], index: usize, lanes: usize) -> Result<Accessor> {
    let a=*a.get(index).context("accessor index out of bounds")?;
    ensure!(a.lanes==lanes && (a.component==5126 || a.normalized), "UV/RGBA accessor requires float or normalized u8/u16");
    ensure!(a.target!=Some(34963) && a.offset%4==0 && a.stride%4==0, "invalid vertex accessor layout/target");
    Ok(a)
}
fn meshes(root: &Map<String, Value>, a: &[Accessor], bin: &[u8], material_count: usize, h: &VisualHeader, limits: &DecodeLimits) -> Result<Vec<MeshSpec>> {
    let meshes=array(root, "meshes", limits)?;
    ensure!(!meshes.is_empty(), "no static visual meshes");
    let mut bytes=0;
    let mut primitive_count=0;
    let has_color_support=h.requirements.capabilities.iter().any(|s|s=="vertex-rgba-v1");
    meshes.iter().enumerate().map(|(mi, m)| {
        let m=object(m, &["name", "primitives"], "mesh")?;
        let primitives=array(m, "primitives", limits)?;
        ensure!(!primitives.is_empty(), "empty mesh");
        budget(&mut primitive_count, primitives.len(), 1, limits.max_resources, "primitive resource count")?;
        let mut pool=None;
        let mut specs=Vec::with_capacity(primitives.len());
        for (pi, p) in primitives.iter().enumerate() {
            let p=object(p, &["attributes", "indices", "material", "mode"], "primitive")?;
            ensure!(optional_idx(p, "mode", 4)?==4, "only indexed triangles are supported");
            let attr=object(req(p, "attributes")?, &["POSITION", "NORMAL", "TEXCOORD_0", "COLOR_0"], "primitive attributes")?;
            let indices=[idx(attr, "POSITION")?, idx(attr, "NORMAL")?, idx(attr, "TEXCOORD_0")?];
            if let Some(pool)=pool {
                ensure!(pool==indices, "mesh {mi}: primitives must share vertex pool");
            } else {
                pool=Some(indices);
                let positions=typed(a, indices[0], 3, true)?;
                ensure!(positions.has_bounds, "POSITION accessor requires min and max");
                let normals=typed(a, indices[1], 3, true)?;
                let uvs=vector(a, indices[2], 2)?;
                ensure!(positions.count==normals.count && positions.count==uvs.count, "mesh attribute counts differ");
                budget(&mut bytes, positions.count, 32, limits.max_geometry_bytes, "decoded geometry")?;
                for i in 0..normals.count {
                    let n=Vec3::from_array(normals.float(bin, i));
                    ensure!((n.length_squared()-1.).abs()<=1e-4, "mesh normals must be unit length");
                }
            }
            let positions=typed(a, indices[0], 3, true)?;
            let idxs=typed(a, idx(p, "indices")?, 1, false)?;
            ensure!(idxs.count%3==0, "primitive must contain triangles");
            budget(&mut bytes, idxs.count, 4, limits.max_geometry_bytes, "decoded geometry")?;
            for i in 0..idxs.count {
                ensure!((idxs.index(bin, i) as usize)<positions.count, "triangle index out of bounds");
            }
            let material=idx(p, "material")?;
            ensure!(material<material_count, "primitive material out of bounds");
            let colors=attr.get("COLOR_0").map(|c|vector(a, integer(c, "COLOR_0")?, 4)).transpose()?;
            if let Some(c)=colors {
                ensure!(has_color_support, "mesh {mi} primitive {pi}: COLOR_0 requires vertex-rgba-v1");
                ensure!(c.count==positions.count, "RGBA color count differs from vertex pool");
                budget(&mut bytes, c.count, 16, limits.max_geometry_bytes, "decoded geometry")?;
                for i in 0..c.count {
                    ensure!(c.float::<4>(bin, i).into_iter().all(normalized), "invalid normalized RGBA colors");
                }
            }
            specs.push(PrimitiveSpec {
                indices: idxs,
                material,
                colors
            });
        }
        let pool=pool.context("missing mesh vertex pool")?;
        Ok(MeshSpec {
            name: name(m)?,
            positions: typed(a, pool[0], 3, true)?,
            normals: typed(a, pool[1], 3, true)?,
            uvs: vector(a, pool[2], 2)?,
            primitives: specs
        })
    }).collect()
}
fn textures(root: &Map<String, Value>, image_count: usize, limits: &DecodeLimits) -> Result<Vec<StaticTexture>> {
    // There is no sampler choice in this profile; even unused explicit samplers are rejected.
    array(root, "textures", limits)?.iter().map(|t| {
        let t=object(t, &["source", "name"], "texture")?;
        name(t)?;
        let source=idx(t, "source")?;
        ensure!(source<image_count, "texture image out of bounds");
        Ok(StaticTexture{
            image_index: source
        })
    }).collect()
}
fn materials(root: &Map<String, Value>, texture_count: usize, limits: &DecodeLimits) -> Result<Vec<StaticMaterial>> {
    array(root, "materials", limits)?.iter().map(|m| {
        let m=object(m, &["name", "pbrMetallicRoughness", "alphaMode", "alphaCutoff", "doubleSided"], "material")?;
        let pbr=object(req(m, "pbrMetallicRoughness")?, &["baseColorFactor", "baseColorTexture", "metallicFactor", "roughnessFactor"], "material PBR")?;
        ensure!(req(pbr, "metallicFactor")?.as_f64()==Some(0.), "static material requires metallicFactor 0");
        ensure!(req(pbr, "roughnessFactor")?.as_f64()==Some(1.), "static material requires roughnessFactor 1");
        let factor=req(pbr, "baseColorFactor")?;
        ensure!(factor.as_array().is_some_and(|a|a.iter().all(|v|v.as_f64().is_some_and(|n|(0.0..=1.0).contains(&n)))), "baseColorFactor must be normalized");
        let base_color=float_array(factor, "baseColorFactor")?;
        ensure!(base_color.into_iter().all(normalized), "material base color must be normalized RGBA");
        let texture_index=pbr.get("baseColorTexture").map(|t| {
            let t=object(t, &["index", "texCoord"], "baseColorTexture")?;
            ensure!(optional_idx(t, "texCoord", 0)?==0, "only primary UVs are supported");
            let index=idx(t, "index")?;
            ensure!(index<texture_count, "material texture out of bounds");
            Ok(index)
        }).transpose()?;
        let mode=m.get("alphaMode").map(|v|string(v, "alphaMode")).transpose()?.unwrap_or("OPAQUE");
        let cutoff=m.get("alphaCutoff").map(|v| {
            ensure!(v.as_f64().is_some_and(|n|(0.0..=1.0).contains(&n)), "alphaCutoff must be normalized");
            number(v, "alphaCutoff")
        }).transpose()?;
        ensure!(cutoff.is_none_or(normalized), "material alpha cutoff must be normalized");
        let alpha_mode=match mode {
            "OPAQUE"=>StaticAlphaMode::Opaque,
            "BLEND"=>StaticAlphaMode::Blend,
            "MASK"=>StaticAlphaMode::Mask{
                cutoff: cutoff.unwrap_or(0.5)
            },
            _=>bail!("unsupported alphaMode {mode}")
        };
        // A cutoff only affects MASK. Reject irrelevant authoring fields rather than discard them.
        ensure!(mode=="MASK" || cutoff.is_none(), "alphaCutoff requires MASK mode");
        let double_sided=m.get("doubleSided").map(|v|v.as_bool().context("doubleSided must be boolean")).transpose()?.unwrap_or(false);
        Ok(StaticMaterial {
            name: name(m)?,
            base_color,
            texture_index,
            alpha_mode,
            double_sided
        })
    }).collect()
}
struct ImageSpec {
    name: String,
    view: View,
    width: u32,
    height: u32,
    decoded_bytes: usize
}
// Image decoding may stop after IDAT. Validate the complete original stream too,
// so missing IEND, trailing payloads, bad ancillary CRCs and APNG cannot be hidden.
fn png_stream(png: &[u8], limits: &DecodeLimits) -> Result<()> {
    ensure!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "invalid PNG signature");
    let mut offset=8usize;
    let mut chunks=0usize;
    let mut ended=false;
    while offset<png.len() {
        chunks=chunks.checked_add(1).context("PNG chunk count overflow")?;
        ensure!(chunks<=limits.max_resources, "PNG chunk count exceeds limit");
        let header=offset.checked_add(8).context("PNG chunk overflow")?;
        let bytes=png.get(offset..header).context("truncated PNG chunk header")?;
        let len=u32::from_be_bytes(bytes[..4].try_into()?) as usize;
        let data_end=header.checked_add(len).context("PNG chunk size overflow")?;
        let end=data_end.checked_add(4).context("PNG CRC offset overflow")?;
        ensure!(end<=png.len(), "truncated PNG chunk payload");
        let kind=&bytes[4..];
        ensure!(kind.iter().all(u8::is_ascii_alphabetic), "invalid PNG chunk type");
        ensure!(![b"acTL", b"fcTL", b"fdAT"].contains(&kind.try_into()?), "animated PNG is outside static profile");
        if chunks==1 {
            ensure!(kind==b"IHDR" && len==13, "PNG must start with IHDR");
        }
        else {
            ensure!(kind!=b"IHDR", "duplicate PNG IHDR");
        }
        let crc=u32::from_be_bytes(png[data_end..end].try_into()?);
        ensure!(crc32fast::hash(&png[offset+4..data_end])==crc, "invalid PNG chunk CRC");
        if kind==b"IEND" {
            ensure!(len==0 && end==png.len(), "invalid PNG end or trailing data");
            ended=true;
        }
        offset=end;
    }
    ensure!(ended, "PNG missing IEND");
    Ok(())
}
fn png_decoder<'a>(png: &'a [u8], limits: &DecodeLimits) -> Result<impl ImageDecoder + 'a> {
    let mut r=image::ImageReader::with_format(Cursor::new(png), image::ImageFormat::Png);
    let mut l=image::Limits::default();
    l.max_image_width=Some(limits.max_png_dimension);
    l.max_image_height=Some(limits.max_png_dimension);
    l.max_alloc=Some(limits.max_single_png_bytes as u64);
    r.limits(l);
    r.into_decoder().context("invalid or excessive PNG header")
}
fn images(root: &Map<String, Value>, views: &[View], bin: &[u8], limits: &DecodeLimits) -> Result<Vec<ImageSpec>> {
    let mut retained=0;
    let mut decoded=0;
    let images=array(root, "images", limits)?.iter().enumerate().map(|(i, v)| {
        let v=object(v, &["bufferView", "mimeType", "name"], "image")?;
        ensure!(string(req(v, "mimeType")?, "image mimeType")?=="image/png", "only embedded PNG images are supported");
        let view=*views.get(idx(v, "bufferView")?).context("image bufferView out of bounds")?;
        ensure!(view.stride.is_none() && view.target.is_none(), "PNG bufferView must have no stride or target");
        ensure!(view.len<=limits.max_single_png_bytes, "image {i} encoded PNG exceeds limit");
        budget(&mut retained, view.len, 1, limits.max_png_bytes, "retained PNG bytes")?;
        let png=&bin[view.offset..view.offset+view.len];
        png_stream(png, limits).with_context(||format!("image {i}"))?;
        let decoder=png_decoder(png, limits).with_context(||format!("image {i}"))?;
        let (width, height)=decoder.dimensions();
        ensure!(width>0 && height>0, "empty PNG dimensions");
        let decoded_bytes=usize::try_from(decoder.total_bytes()).context("PNG decoded size overflow")?;
        ensure!(decoded_bytes<=limits.max_single_png_bytes, "image {i} decoded PNG exceeds allocation limit");
        budget(&mut decoded, decoded_bytes, 1, limits.max_decoded_png_bytes, "decoded PNG validation bytes")?;
        Ok(ImageSpec {
            name: name(v)?,
            view,
            width,
            height,
            decoded_bytes
        })
    }).collect::<Result<Vec<_>>>()?;
    // Every image header and aggregate allocation has passed before a full decode or payload copy.
    for (i, im) in images.iter().enumerate() {
        let png=&bin[im.view.offset..im.view.offset+im.view.len];
        let decoder=png_decoder(png, limits)?;
        let mut pixels=vec![0;im.decoded_bytes];
        decoder.read_image(&mut pixels).with_context(||format!("image {i} invalid PNG pixels"))?;
    }
    Ok(images)
}
fn map_axes(p: [f32;3]) -> [f32;3] {
    [p[0], -p[2], p[1]]
}
fn convert_matrix(m: [[f32;4];4]) -> [[f32;4];4] {
    // This inverse signed permutation has determinant +1: winding is unchanged.
    let inverse=Mat4::from_cols_array_2d(&[
    [1., 0., 0., 0.], [0., 0., 1., 0.], [0., -1., 0., 0.], [0., 0., 0., 1.],
    ]);
    (inverse*Mat4::from_cols_array_2d(&m)*inverse.transpose()).to_cols_array_2d()
}
fn matrix(v: Option<&Value>) -> Result<[[f32;4];4]> {
    let out=if let Some(v)=v {
        Mat4::from_cols_array(&float_array::<16>(v, "node matrix")?).to_cols_array_2d()
    } else {
        Mat4::IDENTITY.to_cols_array_2d()
    };
    ensure!(out[0][3]==0. && out[1][3]==0. && out[2][3]==0. && out[3][3]==1., "projective node transform");
    let m=DMat4::from_cols_array(&Mat4::from_cols_array_2d(&out).to_cols_array().map(f64::from));
    let columns=[m.x_axis.truncate(), m.y_axis.truncate(), m.z_axis.truncate()];
    let lengths=columns.map(DVec3::length);
    let scale=lengths[0];
    ensure!(scale>0. && lengths.iter().all(|l|(l/scale-1.).abs()<=1e-5), "nonuniform or zero node scale");
    let axes=std::array::from_fn::<_,
    3,
    _>(|i|columns[i]/lengths[i]);
    ensure!(axes[0].dot(axes[1]).abs()<=1e-5 && axes[0].dot(axes[2]).abs()<=1e-5 && axes[1].dot(axes[2]).abs()<=1e-5, "sheared node transform");
    ensure!(axes[0].cross(axes[1]).dot(axes[2])>0., "reflected node transform");
    Ok(out)
}
fn instances(root: &Map<String, Value>, meshes: &[MeshSpec], bin: &[u8], limits: &DecodeLimits) -> Result<(Vec<StaticInstance>, Bounds)> {
    let nodes=array(root, "nodes", limits)?;
    ensure!(!nodes.is_empty(), "no static instances");
    let scenes=array(root, "scenes", limits)?;
    ensure!(scenes.len()==1 && idx(root, "scene")?==0, "one active flat scene is required");
    let scene=object(&scenes[0], &["nodes", "name"], "scene")?;
    name(scene)?;
    let roots=array(scene, "nodes", limits)?;
    ensure!(roots.len()==nodes.len(), "all nodes must be flat scene instances");
    let mut seen=vec![false;nodes.len()];
    for n in roots {
        let n=integer(n, "scene node")?;
        ensure!(n<nodes.len() && !seen[n], "invalid or duplicate scene node");
        seen[n]=true;
    }
    // Preflight the instance × shared-pool work product before transforming any vertex.
    let mut world_vertices=0;
    for node in nodes {
        let node=object(node, &["mesh", "matrix", "name"], "node")?;
        let mesh=meshes.get(idx(node, "mesh")?).context("node mesh out of bounds")?;
        budget(&mut world_vertices, mesh.positions.count, 1, limits.max_world_vertices, "world vertex evaluations")?;
    }
    let mut bounds=Bounds {
        min: [f32::INFINITY;3],
        max: [f32::NEG_INFINITY;3]
    };
    let instances=nodes.iter().enumerate().map(|(ni, node)| {
        let node=object(node, &["mesh", "matrix", "name"], "node")?;
        name(node)?;
        let mesh_index=idx(node, "mesh")?;
        let mesh=meshes.get(mesh_index).context("node mesh out of bounds")?;
        let source=matrix(node.get("matrix")).with_context(||format!("node {ni}"))?;
        let converted=convert_matrix(source);
        let source=Mat4::from_cols_array_2d(&source);
        let server=Mat4::from_cols_array_2d(&converted);
        ensure!(server.is_finite(), "converted node matrix is nonfinite");
        for i in 0..mesh.positions.count {
            let p=mesh.positions.float(bin, i);
            ensure!(source.transform_point3(Vec3::from_array(p)).is_finite(), "nonfinite asset world position");
            let p=server.transform_point3(Vec3::from_array(map_axes(p))).to_array();
            ensure!(p.into_iter().all(f32::is_finite), "nonfinite server world position");
            for k in 0..3 {
                bounds.min[k]=bounds.min[k].min(p[k]);
                bounds.max[k]=bounds.max[k].max(p[k]);
            }
        }
        Ok(StaticInstance {
            mesh_index,
            matrix: converted
        })
    }).collect::<Result<Vec<_>>>()?;
    Ok((instances, bounds))
}
fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<StaticVisual> {
    let (json, bin)=chunks(bytes, limits)?;
    exact_header_numbers(json)?;
    let json: Value=serde_json::from_slice(json).context("invalid GLB JSON")?;
    let root=object(&json, &["asset", "extras", "scene", "scenes", "nodes", "meshes", "materials", "textures", "images", "buffers", "bufferViews", "accessors"], "static visual document")?;
    let asset=object(req(root, "asset")?, &["version", "generator", "copyright", "minVersion"], "glTF asset")?;
    ensure!(string(req(asset, "version")?, "asset version")?=="2.0", "unsupported glTF asset version");
    for key in ["generator", "copyright"] {
        if let Some(s)=asset.get(key) {
            string(s, key)?;
        }
    }
    if let Some(v)=asset.get("minVersion") {
        ensure!(string(v, "minVersion")?=="2.0", "unsupported minVersion");
    }
    let h=header(root)?;
    let views=buffer_views(root, bin, limits)?;
    let accessors=accessors(root, &views, bin, limits)?;
    // No retained binary arrays are copied until all resources and budgets have passed.
    let image_count=array(root, "images", limits)?.len();
    let textures=textures(root, image_count, limits)?;
    let materials=materials(root, textures.len(), limits)?;
    let specs=meshes(root, &accessors, bin, materials.len(), &h, limits)?;
    let (instances, bounds)=instances(root, &specs, bin, limits)?;
    let image_specs=images(root, &views, bin, limits)?;
    let meshes=specs.into_iter().map(|m| StaticMesh {
        name: m.name,
        positions: (0..m.positions.count).map(|i|map_axes(m.positions.float(bin, i))).collect(),
        normals: (0..m.normals.count).map(|i|map_axes(m.normals.float(bin, i))).collect(),
        uvs: (0..m.uvs.count).map(|i|m.uvs.float(bin, i)).collect(),
        primitives: m.primitives.into_iter().map(|p|StaticPrimitive {
            indices: (0..p.indices.count).map(|i|p.indices.index(bin, i)).collect(),
            material_index: p.material,
            colors: p.colors.map(|c|(0..c.count).map(|i|c.float(bin, i)).collect()),
        }).collect(),
    }).collect();
    let images=image_specs.into_iter().map(|i|StaticImage {
        name: i.name,
        width: i.width,
        height: i.height,
        png_bytes: bin[i.view.offset..i.view.offset+i.view.len].to_vec(),
    }).collect();
    Ok(StaticVisual {
        header: h,
        meshes,
        materials,
        images,
        textures,
        instances,
        bounds
    })
}
