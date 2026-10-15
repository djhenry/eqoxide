//! Private mechanics shared by the strict visual and collision profiles.
use anyhow::{bail, ensure, Context, Result};
use glam::{DMat4, DVec3, Mat4};
use serde_json::{value::RawValue, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct GlbChunks<'a> {
    pub json: &'a [u8],
    pub bin: Option<&'a [u8]>,
}
pub(super) fn glb_chunks(
    bytes: &[u8],
    max_glb_bytes: usize,
    max_json_bytes: usize,
) -> Result<GlbChunks<'_>> {
    ensure!(
        bytes.len() <= max_glb_bytes,
        "encoded GLB exceeds byte limit"
    );
    ensure!(bytes.len() >= 20, "truncated GLB header");
    ensure!(
        &bytes[..4] == b"glTF" && word(bytes, 4)? == 2,
        "expected GLB version 2"
    );
    ensure!(
        word(bytes, 8)? as usize == bytes.len(),
        "GLB declared length mismatch"
    );
    let json_len = word(bytes, 12)? as usize;
    ensure!(
        json_len <= max_json_bytes && json_len % 4 == 0,
        "invalid or excessive JSON chunk"
    );
    ensure!(&bytes[16..20] == b"JSON", "first GLB chunk must be JSON");
    let json_end = 20usize
        .checked_add(json_len)
        .context("JSON size overflow")?;
    let json = bytes
        .get(20..json_end)
        .context("truncated GLB JSON chunk")?;
    if json_end == bytes.len() {
        return Ok(GlbChunks { json, bin: None });
    }
    let next = json_end.checked_add(8).context("GLB chunk overflow")?;
    ensure!(next <= bytes.len(), "truncated GLB BIN chunk");
    ensure!(
        &bytes[json_end + 4..next] == b"BIN\0",
        "second GLB chunk must be BIN"
    );
    let bin_len = word(bytes, json_end)? as usize;
    ensure!(
        bin_len % 4 == 0 && next.checked_add(bin_len) == Some(bytes.len()),
        "invalid BIN length or unsupported trailing chunk"
    );
    Ok(GlbChunks {
        json,
        bin: Some(&bytes[next..]),
    })
}
fn word(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset.checked_add(4).context("word offset overflow")?;
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..end)
            .context("truncated word")?
            .try_into()?,
    ))
}
pub(super) fn object<'a>(
    value: &'a Value,
    keys: &[&str],
    label: &str,
) -> Result<&'a Map<String, Value>> {
    let o = value
        .as_object()
        .with_context(|| format!("{label} must be an object"))?;
    for key in o.keys() {
        ensure!(
            keys.contains(&key.as_str()),
            "{label}: unsupported field {key}"
        );
    }
    Ok(o)
}
pub(super) fn req<'a>(o: &'a Map<String, Value>, key: &str) -> Result<&'a Value> {
    o.get(key).with_context(|| format!("missing {key}"))
}
pub(super) fn string<'a>(v: &'a Value, label: &str) -> Result<&'a str> {
    v.as_str()
        .with_context(|| format!("{label} must be a string"))
}
// Resource descriptors use canonical JSON unsigned integers. Floating-point spellings
// are deliberately excluded; only fixed header numbers have decimal equivalence.
pub(super) fn integer(v: &Value, label: &str) -> Result<usize> {
    let n = v
        .as_u64()
        .with_context(|| format!("{label} must be a JSON unsigned integer"))?;
    usize::try_from(n).with_context(|| format!("{label} out of range"))
}
pub(super) fn idx(o: &Map<String, Value>, key: &str) -> Result<usize> {
    integer(req(o, key)?, key)
}
pub(super) fn optional_idx(o: &Map<String, Value>, key: &str, default: usize) -> Result<usize> {
    o.get(key)
        .map(|v| integer(v, key))
        .transpose()
        .map(|v| v.unwrap_or(default))
}
pub(super) fn number(v: &Value, label: &str) -> Result<f32> {
    let n = v
        .as_f64()
        .with_context(|| format!("{label} must be numeric"))? as f32;
    ensure!(n.is_finite(), "{label} must be finite f32");
    Ok(n)
}
pub(super) fn array<'a>(
    o: &'a Map<String, Value>,
    key: &str,
    max_resources: usize,
) -> Result<&'a [Value]> {
    let v = match o.get(key) {
        Some(v) => v
            .as_array()
            .with_context(|| format!("{key} must be an array"))?,
        None => return Ok(&[]),
    };
    ensure!(
        v.len() <= max_resources,
        "{key} resource count exceeds limit"
    );
    Ok(v)
}
pub(super) fn name(o: &Map<String, Value>) -> Result<String> {
    o.get("name")
        .map(|v| string(v, "name").map(str::to_owned))
        .transpose()
        .map(|n| n.unwrap_or_default())
}
pub(super) fn float_array<const N: usize>(v: &Value, label: &str) -> Result<[f32; N]> {
    let a = v
        .as_array()
        .with_context(|| format!("{label} must be an array"))?;
    ensure!(a.len() == N, "{label} must contain {N} values");
    let mut out = [0.; N];
    for (i, n) in a.iter().enumerate() {
        out[i] = number(n, label)?;
    }
    Ok(out)
}
// Validate original object keys before either metadata traversal can overwrite a key.
// The GLB JSON byte limit runs first, and serde_json keeps its normal recursion limit.
pub(super) fn reject_duplicate_keys(json: &[u8]) -> Result<()> {
    use serde::{
        de::{Error as _, MapAccess, SeqAccess, Visitor},
        Deserialize, Deserializer,
    };
    struct UniqueJson;
    impl<'de> Deserialize<'de> for UniqueJson {
        fn deserialize<D: Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            deserializer.deserialize_any(UniqueVisitor)
        }
    }
    struct UniqueVisitor;
    impl<'de> Visitor<'de> for UniqueVisitor {
        type Value = UniqueJson;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("JSON with unique object keys")
        }
        fn visit_map<A: MapAccess<'de>>(
            self,
            mut object: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut keys = BTreeSet::new();
            while let Some(key) = object.next_key::<String>()? {
                if !keys.insert(key) {
                    return Err(A::Error::custom("duplicate object key"));
                }
                object.next_value::<UniqueJson>()?;
            }
            Ok(UniqueJson)
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut array: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            while array.next_element::<UniqueJson>()?.is_some() {}
            Ok(UniqueJson)
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
            Ok(UniqueJson)
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
            Ok(UniqueJson)
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
            Ok(UniqueJson)
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
            Ok(UniqueJson)
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
            Ok(UniqueJson)
        }
        fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(UniqueJson)
        }
    }
    let _: UniqueJson = serde_json::from_slice(json)?;
    Ok(())
}
pub(super) fn raw_map(raw: &RawValue) -> Result<BTreeMap<String, &RawValue>> {
    serde_json::from_str(raw.get()).context("asset header must be an object")
}
pub(super) fn raw_field<'a>(
    map: &BTreeMap<String, &'a RawValue>,
    key: &str,
) -> Result<&'a RawValue> {
    map.get(key)
        .copied()
        .with_context(|| format!("missing {key}"))
}
pub(super) fn exact_fixed(raw: &RawValue, expected: &str, label: &str) -> Result<()> {
    ensure!(
        matches!(expected, "1" | "2"),
        "unsupported fixed-number expectation"
    );
    let token = raw.get();
    let (mantissa, exponent) = match token.find(['e', 'E']) {
        Some(e) => (
            &token[..e],
            token[e + 1..]
                .parse::<i64>()
                .context("header exponent out of range")?,
        ),
        None => (token, 0),
    };
    let fraction = mantissa
        .find('.')
        .map(|dot| mantissa.len() - dot - 1)
        .unwrap_or(0);
    ensure!(
        mantissa.bytes().all(|b| b.is_ascii_digit() || b == b'.'),
        "{label} must be exact {expected}"
    );
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let digits = digits.trim_start_matches('0');
    let significant = digits.trim_end_matches('0');
    let zeros = digits.len() - significant.len();
    let scale = exponent
        .checked_sub(i64::try_from(fraction)?)
        .and_then(|s| s.checked_add(zeros as i64))
        .context("header decimal scale overflow")?;
    ensure!(
        significant == expected && scale == 0,
        "{label} must be exact {expected}"
    );
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) struct View {
    pub offset: usize,
    pub len: usize,
    pub stride: Option<usize>,
    pub target: Option<usize>,
}
#[derive(Clone, Copy, Debug)]
pub(super) enum AccessorProfile {
    Visual,
    Collision,
}
#[derive(Clone, Copy)]
pub(super) struct Accessor<'bin> {
    bin: &'bin [u8],
    buffer_view: usize,
    offset: usize,
    count: usize,
    stride: usize,
    lanes: usize,
    component: usize,
    normalized: bool,
    target: Option<usize>,
    has_bounds: bool,
}
impl<'bin> Accessor<'bin> {
    pub fn count(self) -> usize {
        self.count
    }
    pub fn buffer_view(self) -> usize {
        self.buffer_view
    }
    pub fn lanes(self) -> usize {
        self.lanes
    }
    pub fn component(self) -> usize {
        self.component
    }
    pub fn normalized(self) -> bool {
        self.normalized
    }
    pub fn target(self) -> Option<usize> {
        self.target
    }
    pub fn has_bounds(self) -> bool {
        self.has_bounds
    }
    fn at(self, i: usize, lane: usize, width: usize) -> Result<&'bin [u8]> {
        ensure!(
            i < self.count && lane < self.lanes,
            "accessor read out of bounds"
        );
        let offset = self
            .offset
            .checked_add(
                i.checked_mul(self.stride)
                    .context("accessor read overflow")?,
            )
            .and_then(|o| o.checked_add(lane.checked_mul(width)?))
            .context("accessor read overflow")?;
        self.bin
            .get(
                offset
                    ..offset
                        .checked_add(width)
                        .context("accessor read overflow")?,
            )
            .context("accessor read exceeds BIN")
    }
    pub fn float<const N: usize>(self, i: usize) -> Result<[f32; N]> {
        ensure!(
            N == self.lanes && (self.component == 5126 || self.normalized),
            "unsupported float accessor read"
        );
        let mut out = [0.; N];
        for (lane, value) in out.iter_mut().enumerate() {
            *value = match self.component {
                5121 => self.at(i, lane, 1)?[0] as f32 / 255.,
                5123 => u16::from_le_bytes(self.at(i, lane, 2)?.try_into()?) as f32 / 65535.,
                5126 => f32::from_le_bytes(self.at(i, lane, 4)?.try_into()?),
                _ => bail!("unsupported float accessor component"),
            };
        }
        Ok(out)
    }
    pub fn index(self, i: usize) -> Result<u32> {
        ensure!(
            self.lanes == 1 && !self.normalized,
            "unsupported index accessor read"
        );
        Ok(match self.component {
            5121 => self.at(i, 0, 1)?[0] as u32,
            5123 => u16::from_le_bytes(self.at(i, 0, 2)?.try_into()?) as u32,
            5125 => u32::from_le_bytes(self.at(i, 0, 4)?.try_into()?),
            _ => bail!("unsupported index accessor component"),
        })
    }
}
pub(super) fn buffer_views(
    root: &Map<String, Value>,
    bin: &[u8],
    max_resources: usize,
    profile: AccessorProfile,
) -> Result<Vec<View>> {
    let buffers = array(root, "buffers", max_resources)?;
    ensure!(
        buffers.len() == 1,
        "static component requires exactly one BIN buffer"
    );
    let b = object(&buffers[0], &["byteLength", "name"], "buffer")?;
    name(b)?;
    if matches!(profile, AccessorProfile::Collision) {
        ensure!(!b.contains_key("name"), "collision buffer name unsupported");
    }
    let len = idx(b, "byteLength")?;
    ensure!(
        len <= bin.len() && bin.len() - len <= 3,
        "BIN buffer length mismatch"
    );
    ensure!(bin[len..].iter().all(|b| *b == 0), "invalid BIN padding");
    array(root, "bufferViews", max_resources)?
        .iter()
        .map(|v| {
            let v = object(
                v,
                &[
                    "buffer",
                    "byteOffset",
                    "byteLength",
                    "byteStride",
                    "target",
                    "name",
                ],
                "bufferView",
            )?;
            name(v)?;
            ensure!(
                idx(v, "buffer")? == 0,
                "bufferView must reference BIN buffer"
            );
            let offset = optional_idx(v, "byteOffset", 0)?;
            let size = idx(v, "byteLength")?;
            ensure!(
                size > 0 && offset.checked_add(size).is_some_and(|end| end <= len),
                "bufferView exceeds BIN bounds"
            );
            let stride = v
                .get("byteStride")
                .map(|s| integer(s, "byteStride"))
                .transpose()?;
            ensure!(
                stride.is_none_or(|s| (4..=252).contains(&s) && s % 4 == 0),
                "invalid bufferView byteStride"
            );
            let target = v.get("target").map(|s| integer(s, "target")).transpose()?;
            ensure!(
                target.is_none_or(|s| s == 34962 || s == 34963),
                "invalid bufferView target"
            );
            if matches!(profile, AccessorProfile::Collision) {
                ensure!(
                    offset % 4 == 0 && stride.is_none(),
                    "collision bufferView must be aligned without stride"
                );
                ensure!(
                    !v.contains_key("name"),
                    "collision bufferView name unsupported"
                );
            }
            Ok(View {
                offset,
                len: size,
                stride,
                target,
            })
        })
        .collect()
}
pub(super) fn accessors<'bin>(
    root: &Map<String, Value>,
    views: &[View],
    bin: &'bin [u8],
    max_resources: usize,
    profile: AccessorProfile,
    max_accessor_validation_bytes: usize,
) -> Result<Vec<Accessor<'bin>>> {
    let raw = array(root, "accessors", max_resources)?;
    let mut validation_bytes = 0;
    // Validate every descriptor and the aggregate alias work budget before reading
    // any accessor payload. Unused entries still consume this validation budget.
    let accessors =
        raw.iter()
            .enumerate()
            .map(|(ai, v)| {
                let v = object(
                    v,
                    &[
                        "bufferView",
                        "byteOffset",
                        "componentType",
                        "count",
                        "type",
                        "min",
                        "max",
                        "normalized",
                        "name",
                    ],
                    "accessor",
                )?;
                name(v)?;
                let buffer_view = idx(v, "bufferView")?;
                let view = views
                    .get(buffer_view)
                    .context("accessor bufferView out of bounds")?;
                let component = idx(v, "componentType")?;
                let width = match component {
                    5121 => 1,
                    5123 => 2,
                    5125 | 5126 => 4,
                    _ => bail!("accessor {ai}: unsupported componentType"),
                };
                let lanes = match string(req(v, "type")?, "accessor type")? {
                    "SCALAR" => 1,
                    "VEC2" => 2,
                    "VEC3" => 3,
                    "VEC4" => 4,
                    _ => bail!("accessor {ai}: unsupported type"),
                };
                let normalized = v
                    .get("normalized")
                    .map(|n| n.as_bool().context("normalized must be boolean"))
                    .transpose()?
                    .unwrap_or(false);
                ensure!((component==5126 && !normalized) || (lanes==1 && !normalized) ||
        ([5121, 5123].contains(&component) && normalized && [2, 4].contains(&lanes)),
        "accessor requires float vectors, unsigned indices, or normalized u8/u16 UV2/RGBA4");
                let count = idx(v, "count")?;
                ensure!(count > 0, "empty accessor");
                let offset = optional_idx(v, "byteOffset", 0)?;
                let absolute = view
                    .offset
                    .checked_add(offset)
                    .context("accessor offset overflow")?;
                ensure!(
                    offset % width == 0 && absolute % width == 0,
                    "misaligned accessor"
                );
                let packed = width * lanes;
                let stride = view.stride.unwrap_or(packed);
                ensure!(stride >= packed, "accessor exceeds byteStride");
                ensure!(
                    view.target != Some(34963) || view.stride.is_none(),
                    "indices cannot be interleaved"
                );
                let span = (count - 1)
                    .checked_mul(stride)
                    .and_then(|n| n.checked_add(packed))
                    .context("accessor size overflow")?;
                ensure!(
                    offset.checked_add(span).is_some_and(|end| end <= view.len),
                    "accessor {ai} exceeds bufferView bounds"
                );
                if matches!(profile, AccessorProfile::Collision) {
                    ensure!(
                        !normalized && matches!((component, lanes), (5126, 3) | (5125, 1)),
                        "collision accessors require FLOAT VEC3 or UNSIGNED_INT SCALAR"
                    );
                    ensure!(
                        !v.contains_key("name"),
                        "collision accessor name unsupported"
                    );
                    if lanes == 3 {
                        ensure!(
                            v.contains_key("min") && v.contains_key("max"),
                            "collision POSITION accessor requires min and max"
                        );
                    }
                    if lanes == 1 {
                        ensure!(
                            view.target != Some(34962),
                            "invalid collision index accessor target"
                        );
                    }
                }
                ensure!(
                    absolute
                        .checked_add(span)
                        .is_some_and(|end| end <= bin.len()),
                    "accessor exceeds borrowed BIN bounds"
                );
                let a = Accessor {
                    bin,
                    buffer_view,
                    offset: absolute,
                    count,
                    stride,
                    lanes,
                    component,
                    normalized,
                    target: view.target,
                    has_bounds: v.contains_key("min") && v.contains_key("max"),
                };
                if lanes > 1 {
                    // All supported vector accessors are vertex attributes in this profile.
                    // Validate even unused descriptors before scanning their payloads.
                    ensure!(offset % 4 == 0, "invalid vertex accessor layout/target");
                    vertex_layout(a)?;
                }
                budget(
                    &mut validation_bytes,
                    count,
                    packed,
                    max_accessor_validation_bytes,
                    "accessor validation bytes",
                )?;
                Ok(a)
            })
            .collect::<Result<Vec<_>>>()?;
    for (ai, a) in accessors.iter().enumerate() {
        let v = raw[ai].as_object().context("accessor must be an object")?;
        let Accessor {
            offset: absolute,
            count,
            stride,
            lanes,
            component,
            ..
        } = *a;
        let mut actual_min = [f64::INFINITY; 4];
        let mut actual_max = [f64::NEG_INFINITY; 4];
        for i in 0..count {
            let values: [f64; 4] = if component == 5126 {
                std::array::from_fn(|n| {
                    if n < lanes {
                        let o = absolute + i * stride + n * 4;
                        f32::from_le_bytes(bin[o..o + 4].try_into().unwrap()) as f64
                    } else {
                        0.
                    }
                })
            } else if lanes == 1 {
                [a.index(i)? as f64, 0., 0., 0.]
            } else {
                let width = if component == 5121 { 1 } else { 2 };
                std::array::from_fn(|n| {
                    if n < lanes {
                        let o = absolute + i * stride + n * width;
                        if component == 5121 {
                            bin[o] as f64
                        } else {
                            u16::from_le_bytes(bin[o..o + 2].try_into().unwrap()) as f64
                        }
                    } else {
                        0.
                    }
                })
            };
            for lane in 0..lanes {
                ensure!(values[lane].is_finite(), "accessor {ai} nonfinite value");
                actual_min[lane] = actual_min[lane].min(values[lane]);
                actual_max[lane] = actual_max[lane].max(values[lane]);
            }
        }
        for (key, actual) in [("min", actual_min), ("max", actual_max)] {
            if let Some(bounds) = v.get(key) {
                let bounds = bounds
                    .as_array()
                    .context("accessor bounds must be arrays")?;
                ensure!(bounds.len() == lanes, "invalid accessor bounds count");
                for (lane, value) in bounds.iter().enumerate() {
                    let declared = if component == 5126 {
                        number(value, "accessor bound")? as f64
                    } else {
                        integer(value, "accessor bound")? as f64
                    };
                    ensure!(
                        declared == actual[lane],
                        "accessor {ai}: {key} does not match actual data"
                    );
                }
            }
        }
    }
    Ok(accessors)
}
pub(super) fn budget(
    total: &mut usize,
    count: usize,
    stride: usize,
    limit: usize,
    label: &str,
) -> Result<()> {
    *total = total
        .checked_add(
            count
                .checked_mul(stride)
                .with_context(|| format!("{label} size overflow"))?,
        )
        .with_context(|| format!("{label} aggregate overflow"))?;
    ensure!(*total <= limit, "{label} exceeds limit");
    Ok(())
}
pub(super) fn vertex_layout(a: Accessor) -> Result<()> {
    ensure!(
        a.target != Some(34963) && a.offset % 4 == 0 && a.stride % 4 == 0,
        "invalid vertex accessor layout/target"
    );
    Ok(())
}
pub(super) fn map_axes(p: [f32; 3]) -> [f32; 3] {
    [p[0], -p[2], p[1]]
}
pub(super) fn convert_matrix(m: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    // This inverse signed permutation has determinant +1: winding is unchanged.
    let inverse = Mat4::from_cols_array_2d(&[
        [1., 0., 0., 0.],
        [0., 0., 1., 0.],
        [0., -1., 0., 0.],
        [0., 0., 0., 1.],
    ]);
    (inverse * Mat4::from_cols_array_2d(&m) * inverse.transpose()).to_cols_array_2d()
}
pub(super) fn matrix(v: Option<&Value>) -> Result<[[f32; 4]; 4]> {
    let out = if let Some(v) = v {
        Mat4::from_cols_array(&float_array::<16>(v, "node matrix")?).to_cols_array_2d()
    } else {
        Mat4::IDENTITY.to_cols_array_2d()
    };
    ensure!(
        out[0][3] == 0. && out[1][3] == 0. && out[2][3] == 0. && out[3][3] == 1.,
        "projective node transform"
    );
    let m = DMat4::from_cols_array(
        &Mat4::from_cols_array_2d(&out)
            .to_cols_array()
            .map(f64::from),
    );
    let columns = [
        m.x_axis.truncate(),
        m.y_axis.truncate(),
        m.z_axis.truncate(),
    ];
    let lengths = columns.map(DVec3::length);
    let scale = lengths[0];
    ensure!(
        scale > 0. && lengths.iter().all(|l| (l / scale - 1.).abs() <= 1e-5),
        "nonuniform or zero node scale"
    );
    let axes = std::array::from_fn::<_, 3, _>(|i| columns[i] / lengths[i]);
    ensure!(
        axes[0].dot(axes[1]).abs() <= 1e-5
            && axes[0].dot(axes[2]).abs() <= 1e-5
            && axes[1].dot(axes[2]).abs() <= 1e-5,
        "sheared node transform"
    );
    ensure!(
        axes[0].cross(axes[1]).dot(axes[2]) > 0.,
        "reflected node transform"
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_json() -> (Value, &'static [u8]) {
        let bytes = include_bytes!("../tests/fixtures/static-collision-v1.glb");
        let c = glb_chunks(bytes, 1 << 20, 1 << 20).unwrap();
        (serde_json::from_slice(c.json).unwrap(), c.bin.unwrap())
    }
    #[test]
    fn validated_handles_refuse_wrong_width_type_and_out_of_range_reads() {
        let (j, bin) = fixture_json();
        let root = j.as_object().unwrap();
        let views = buffer_views(root, bin, 100, AccessorProfile::Collision).unwrap();
        let a = accessors(root, &views, bin, 100, AccessorProfile::Collision, 1 << 20).unwrap();
        assert_eq!(a[0].float::<3>(1).unwrap(), [2., 0., 0.]);
        assert!(a[0].float::<2>(0).is_err());
        assert!(a[0].float::<4>(0).is_err());
        assert!(a[0].index(0).is_err());
        assert!(a[1].float::<1>(0).is_err());
        assert!(a[0].float::<3>(a[0].count()).is_err());
        assert!(a[1].index(a[1].count()).is_err());
        assert!(a[0].float::<3>(usize::MAX).is_err());
        assert!(a[1].index(usize::MAX).is_err());
    }
    #[test]
    fn json_only_and_present_empty_bin_are_distinct_and_extra_chunks_are_refused() {
        let bytes = include_bytes!("../tests/fixtures/static-collision-empty-v1.glb");
        let c = glb_chunks(bytes, 1 << 20, 1 << 20).unwrap();
        assert!(c.bin.is_none());
        let mut with_bin = bytes.to_vec();
        with_bin.extend(0u32.to_le_bytes());
        with_bin.extend(b"BIN\0");
        let len = with_bin.len() as u32;
        with_bin[8..12].copy_from_slice(&len.to_le_bytes());
        assert_eq!(
            glb_chunks(&with_bin, 1 << 20, 1 << 20).unwrap().bin,
            Some(&[][..])
        );
        let mut trailing = with_bin.clone();
        trailing.extend(0u32.to_le_bytes());
        trailing.extend(b"BIN\0");
        let len = trailing.len() as u32;
        trailing[8..12].copy_from_slice(&len.to_le_bytes());
        assert!(glb_chunks(&trailing, 1 << 20, 1 << 20).is_err());
        let last = with_bin.len() - 4;
        with_bin[last..].copy_from_slice(b"JSON");
        assert!(glb_chunks(&with_bin, 1 << 20, 1 << 20).is_err());
    }
    #[test]
    fn checked_budget_preserves_overflow_and_alias_refusal() {
        let mut n = 0;
        assert!(budget(&mut n, usize::MAX, 2, usize::MAX, "test").is_err());
        let mut n = usize::MAX;
        assert!(budget(&mut n, 1, 1, usize::MAX, "test").is_err());
        let mut n = 0;
        budget(&mut n, 3, 4, 12, "test").unwrap();
        assert!(budget(&mut n, 1, 4, 12, "test").is_err());
    }
    #[test]
    fn unrelated_short_bin_cannot_validate_a_descriptor_collection() {
        let (j, bin) = fixture_json();
        let root = j.as_object().unwrap();
        let views = buffer_views(root, bin, 100, AccessorProfile::Collision).unwrap();
        assert!(accessors(
            root,
            &views,
            &bin[..4],
            100,
            AccessorProfile::Collision,
            1 << 20
        )
        .is_err());
    }
}
