//! Owned, bounded geometry for the declared static collision profile.
//! Recognized policy metadata does not implement queries, movement or navigation.
use super::static_decode::*;
use anyhow::{ensure, Context, Result};
use glam::{DVec3, Mat4, Vec3};
use serde_json::{value::RawValue, Map, Value};
use std::{
    collections::BTreeSet,
    panic::{catch_unwind, AssertUnwindSafe},
};

/// Logical payload and validation work limits, not allocator overhead or total RSS.
#[derive(Debug, Clone)]
pub struct CollisionDecodeLimits {
    pub max_glb_bytes: usize,
    pub max_json_bytes: usize,
    /// Retained server-axis position pools and u32 index lists, including shared aliases.
    pub max_geometry_bytes: usize,
    /// All declared accessor payload visits, including unused and aliased descriptors.
    pub max_accessor_validation_bytes: usize,
    /// Each collection and the aggregate number of primitives.
    pub max_resources: usize,
    pub max_vertices: usize,
    pub max_indices: usize,
    /// Triangles summed over all instances, without constructing triangle soup.
    pub max_expanded_triangles: usize,
    /// Paired asset/server world evaluations: three indexed references per instanced triangle.
    pub max_world_vertices: usize,
}
impl Default for CollisionDecodeLimits {
    fn default() -> Self {
        Self {
            max_glb_bytes: 128 << 20,
            max_json_bytes: 8 << 20,
            max_geometry_bytes: 128 << 20,
            max_accessor_validation_bytes: 256 << 20,
            max_resources: 100_000,
            max_vertices: 5_000_000,
            max_indices: 15_000_000,
            max_expanded_triangles: 5_000_000,
            max_world_vertices: 50_000_000,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollisionAvailability {
    Available,
    AvailableEmpty,
}
/// Declared inspection semantics only; this decoder provides no segment operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectionQueryPolicy {
    FrontFaces,
    BothFaces,
}
#[derive(Debug, Clone)]
pub struct CollisionReaderRequirements {
    pub reader_version: u32,
    pub capabilities: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct CollisionHeader {
    pub schema_version: u32,
    pub role: String,
    pub coordinate_profile: String,
    pub unit_scale: f32,
    pub bake_revision: String,
    pub requirements: CollisionReaderRequirements,
    pub collision_version: u32,
    pub scope: String,
}
#[derive(Debug, Clone, PartialEq)]
pub struct CollisionBounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
}
#[derive(Debug, Clone)]
pub struct CollisionPrimitive {
    pub indices: Vec<u32>,
}
#[derive(Debug, Clone)]
pub struct CollisionMesh {
    /// Shared finite f32 position pool in server geometry axes; unused vertices are retained.
    pub positions: Vec<[f32; 3]>,
    pub primitives: Vec<CollisionPrimitive>,
}
#[derive(Debug, Clone)]
pub struct CollisionInstance {
    pub mesh_index: usize,
    /// Stable opaque authored/imported owner, never inferred from a node index or name.
    pub owner_id: String,
    /// Column-major proper rotation, positive uniform scale and translation in server axes.
    pub matrix: [[f32; 4]; 4],
}
#[derive(Debug, Clone)]
pub struct StaticCollision {
    pub header: CollisionHeader,
    pub availability: CollisionAvailability,
    pub query_policy: InspectionQueryPolicy,
    pub meshes: Vec<CollisionMesh>,
    pub instances: Vec<CollisionInstance>,
    /// Indexed transformed surfaces only; absent for the explicit known-empty dataset.
    pub bounds: Option<CollisionBounds>,
}
/// Decode a collision component without activating it or advertising query support.
/// Dependency panics become errors with unwinding; `panic = "abort"` cannot recover.
pub fn decode_static_collision(
    bytes: &[u8],
    limits: &CollisionDecodeLimits,
) -> Result<StaticCollision> {
    catch_unwind(AssertUnwindSafe(|| decode(bytes, limits)))
        .map_err(|_| anyhow::anyhow!("static collision dependency panicked while decoding"))?
}
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
    let collision = raw_map(raw_field(&extras, "eqoxideCollision")?)?;
    exact_fixed(raw_field(&collision, "version")?, "1", "collision version")
}
fn header(
    root: &Map<String, Value>,
    limits: &CollisionDecodeLimits,
) -> Result<(
    CollisionHeader,
    CollisionAvailability,
    InspectionQueryPolicy,
)> {
    let extras = object(
        req(root, "extras")?,
        &["eqoxideAsset", "eqoxideCollision"],
        "document extras",
    )?;
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
    ensure!(
        string(req(h, "role")?, "role")? == "collision",
        "unsupported artifact role"
    );
    ensure!(
        string(req(h, "coordinateProfile")?, "coordinateProfile")? == "eqoxide-static-y-up-v1",
        "unsupported coordinate profile"
    );
    let revision = string(req(h, "bakeRevision")?, "bakeRevision")?;
    ensure!(
        revision.len() == 64
            && revision
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid bake revision"
    );
    let r = object(
        req(h, "requirements")?,
        &["reader_version", "capabilities"],
        "requirements",
    )?;
    let caps = array(r, "capabilities", limits.max_resources)?;
    ensure!(
        caps.len() == 1 && string(&caps[0], "capability")? == "static-collision-triangles-v1",
        "unsupported collision requirements"
    );
    let c = object(
        req(extras, "eqoxideCollision")?,
        &[
            "version",
            "scope",
            "availability",
            "selectedNodes",
            "queryPolicy",
        ],
        "collision metadata",
    )?;
    ensure!(
        string(req(c, "scope")?, "collision scope")? == "declared-static-triangle-candidates-v1",
        "unsupported collision scope"
    );
    let availability = match string(req(c, "availability")?, "availability")? {
        "available" => CollisionAvailability::Available,
        "available-empty" => CollisionAvailability::AvailableEmpty,
        _ => anyhow::bail!("unsupported collision availability"),
    };
    let policy = match string(req(c, "queryPolicy")?, "queryPolicy")? {
        "inspection-segment-front-v1" => InspectionQueryPolicy::FrontFaces,
        "inspection-segment-both-v1" => InspectionQueryPolicy::BothFaces,
        _ => anyhow::bail!("unsupported collision query policy metadata"),
    };
    // Fixed header numbers have already been compared as original mathematical tokens.
    Ok((
        CollisionHeader {
            schema_version: 1,
            role: "collision".into(),
            coordinate_profile: "eqoxide-static-y-up-v1".into(),
            unit_scale: 1.,
            bake_revision: revision.into(),
            requirements: CollisionReaderRequirements {
                reader_version: 2,
                capabilities: vec!["static-collision-triangles-v1".into()],
            },
            collision_version: 1,
            scope: "declared-static-triangle-candidates-v1".into(),
        },
        availability,
        policy,
    ))
}
fn selection(
    root: &Map<String, Value>,
    limits: &CollisionDecodeLimits,
    count: usize,
    empty: bool,
) -> Result<()> {
    let scenes = array(root, "scenes", limits.max_resources)?;
    ensure!(
        scenes.len() == 1 && idx(root, "scene")? == 0,
        "one active flat scene is required"
    );
    let scene = object(&scenes[0], &["nodes"], "collision scene")?;
    if empty {
        ensure!(scene.is_empty(), "known-empty scene must omit nodes");
    } else {
        ensure!(
            scene.contains_key("nodes"),
            "collision scene nodes are required"
        );
    }
    let extras = root["extras"]
        .as_object()
        .context("document extras must be an object")?;
    let collision = extras["eqoxideCollision"]
        .as_object()
        .context("collision metadata must be an object")?;
    ensure!(
        collision.contains_key("selectedNodes"),
        "missing selectedNodes"
    );
    for (o, key) in [(scene, "nodes"), (collision, "selectedNodes")] {
        let selected = array(o, key, limits.max_resources)?;
        ensure!(selected.len() == count, "selection must cover every node");
        let mut seen = vec![false; count];
        for n in selected {
            let n = integer(n, "selected node")?;
            ensure!(n < count && !seen[n], "invalid or duplicate selected node");
            seen[n] = true;
        }
    }
    Ok(())
}
struct MeshSpec<'bin> {
    positions: Accessor<'bin>,
    indices: Vec<Accessor<'bin>>,
    triangles: usize,
}
fn meshes<'bin>(
    root: &Map<String, Value>,
    accessors: &[Accessor<'bin>],
    limits: &CollisionDecodeLimits,
) -> Result<Vec<MeshSpec<'bin>>> {
    let raw = array(root, "meshes", limits.max_resources)?;
    ensure!(!raw.is_empty(), "available collision requires meshes");
    let mut total_bytes = 0;
    let mut total_vertices = 0;
    let mut total_indices = 0;
    let mut primitive_count = 0;
    // Charge every mesh/primitive before corresponding index or triangle scans, including aliases.
    let mut specs = Vec::with_capacity(raw.len());
    for m in raw {
        let m = object(m, &["primitives"], "collision mesh")?;
        let primitives = array(m, "primitives", limits.max_resources)?;
        ensure!(!primitives.is_empty(), "empty collision mesh");
        budget(
            &mut primitive_count,
            primitives.len(),
            1,
            limits.max_resources,
            "primitive resource count",
        )?;
        let mut pool = None;
        let mut indices = Vec::with_capacity(primitives.len());
        let mut triangles = 0usize;
        for p in primitives {
            let p = object(p, &["attributes", "indices", "mode"], "collision primitive")?;
            ensure!(idx(p, "mode")? == 4, "collision requires indexed TRIANGLES");
            let attributes = object(req(p, "attributes")?, &["POSITION"], "collision attributes")?;
            let pool_index = idx(attributes, "POSITION")?;
            if let Some((previous, _)) = pool {
                ensure!(
                    pool_index == previous,
                    "collision primitives must share one position pool"
                );
            } else {
                let a = *accessors
                    .get(pool_index)
                    .context("position accessor out of bounds")?;
                ensure!(
                    a.component() == 5126
                        && a.lanes() == 3
                        && a.has_bounds()
                        && a.target() != Some(34963),
                    "unsupported collision position binding"
                );
                budget(
                    &mut total_vertices,
                    a.count(),
                    1,
                    limits.max_vertices,
                    "collision vertices",
                )?;
                budget(
                    &mut total_bytes,
                    a.count(),
                    12,
                    limits.max_geometry_bytes,
                    "decoded collision geometry",
                )?;
                pool = Some((pool_index, a));
            }
            let a = *accessors
                .get(idx(p, "indices")?)
                .context("index accessor out of bounds")?;
            ensure!(
                a.component() == 5125 && a.lanes() == 1 && a.target() != Some(34962),
                "unsupported collision index binding"
            );
            ensure!(
                a.count() % 3 == 0,
                "collision primitive must contain triangles"
            );
            budget(
                &mut total_indices,
                a.count(),
                1,
                limits.max_indices,
                "collision indices",
            )?;
            budget(
                &mut total_bytes,
                a.count(),
                4,
                limits.max_geometry_bytes,
                "decoded collision geometry",
            )?;
            triangles = triangles
                .checked_add(a.count() / 3)
                .context("mesh triangle count overflow")?;
            indices.push(a);
        }
        specs.push(MeshSpec {
            positions: pool.context("missing collision position pool")?.1,
            indices,
            triangles,
        });
    }
    Ok(specs)
}
fn nondegenerate(points: [[f32; 3]; 3], label: &str) -> Result<()> {
    let [a, b, c] = points.map(|p| DVec3::from_array(p.map(f64::from)));
    ensure!(
        (b - a).cross(c - a).length_squared() > 0.,
        "degenerate {label} triangle"
    );
    Ok(())
}
fn local_triangles(specs: &[MeshSpec<'_>]) -> Result<()> {
    for m in specs {
        for a in &m.indices {
            for start in (0..a.count()).step_by(3) {
                let mut points = [[0.; 3]; 3];
                for (lane, p) in points.iter_mut().enumerate() {
                    let i = a.index(start + lane)? as usize;
                    ensure!(
                        i < m.positions.count(),
                        "collision triangle index out of bounds"
                    );
                    *p = m.positions.float(i)?;
                }
                nondegenerate(points, "local")?;
            }
        }
    }
    Ok(())
}
struct InstanceSpec {
    mesh_index: usize,
    owner_id: String,
    asset: [[f32; 4]; 4],
    server: [[f32; 4]; 4],
}
fn instances(
    root: &Map<String, Value>,
    meshes: &[MeshSpec<'_>],
    limits: &CollisionDecodeLimits,
) -> Result<Vec<InstanceSpec>> {
    let raw = array(root, "nodes", limits.max_resources)?;
    ensure!(!raw.is_empty(), "available collision requires instances");
    selection(root, limits, raw.len(), false)?;
    let mut owners = BTreeSet::new();
    let mut triangles = 0usize;
    let mut world_vertices = 0;
    let mut out = Vec::with_capacity(raw.len());
    for node in raw {
        let n = object(node, &["mesh", "matrix", "extras"], "collision node")?;
        let mesh_index = idx(n, "mesh")?;
        let mesh = meshes
            .get(mesh_index)
            .context("collision node mesh out of bounds")?;
        budget(
            &mut triangles,
            mesh.triangles,
            1,
            limits.max_expanded_triangles,
            "expanded collision triangles",
        )?;
        budget(
            &mut world_vertices,
            mesh.triangles,
            3,
            limits.max_world_vertices,
            "collision world vertex evaluations",
        )?;
        let extras = object(
            req(n, "extras")?,
            &["eqoxideCollisionInstance"],
            "node extras",
        )?;
        let record = object(
            req(extras, "eqoxideCollisionInstance")?,
            &["ownerId"],
            "collision owner",
        )?;
        let owner = string(req(record, "ownerId")?, "ownerId")?;
        ensure!(
            (1..=128).contains(&owner.len())
                && (owner.as_bytes()[0].is_ascii_lowercase()
                    || owner.as_bytes()[0].is_ascii_digit()),
            "invalid ownerId"
        );
        ensure!(
            owner
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)),
            "invalid ownerId"
        );
        ensure!(owners.insert(owner), "duplicate collision ownerId");
        let asset = matrix(Some(req(n, "matrix")?))?;
        let server = convert_matrix(asset);
        ensure!(
            Mat4::from_cols_array_2d(&server).is_finite(),
            "converted collision matrix is nonfinite"
        );
        out.push(InstanceSpec {
            mesh_index,
            owner_id: owner.into(),
            asset,
            server,
        });
    }
    Ok(out)
}
fn world_bounds(meshes: &[MeshSpec<'_>], instances: &[InstanceSpec]) -> Result<CollisionBounds> {
    let mut bounds = CollisionBounds {
        min: [f32::INFINITY; 3],
        max: [f32::NEG_INFINITY; 3],
    };
    for instance in instances {
        let m = &meshes[instance.mesh_index];
        let asset = Mat4::from_cols_array_2d(&instance.asset);
        let server = Mat4::from_cols_array_2d(&instance.server);
        for indices in &m.indices {
            for start in (0..indices.count()).step_by(3) {
                let mut source_world = [[0.; 3]; 3];
                let mut server_world = [[0.; 3]; 3];
                for lane in 0..3 {
                    let p = m.positions.float(indices.index(start + lane)? as usize)?;
                    source_world[lane] = asset.transform_point3(Vec3::from_array(p)).to_array();
                    server_world[lane] = server
                        .transform_point3(Vec3::from_array(map_axes(p)))
                        .to_array();
                    ensure!(
                        source_world[lane].into_iter().all(f32::is_finite),
                        "nonfinite asset world position"
                    );
                    ensure!(
                        server_world[lane].into_iter().all(f32::is_finite),
                        "nonfinite server world position"
                    );
                    for k in 0..3 {
                        bounds.min[k] = bounds.min[k].min(server_world[lane][k]);
                        bounds.max[k] = bounds.max[k].max(server_world[lane][k]);
                    }
                }
                nondegenerate(source_world, "asset world")?;
                nondegenerate(server_world, "server world")?;
            }
        }
    }
    Ok(bounds)
}
fn decode(bytes: &[u8], limits: &CollisionDecodeLimits) -> Result<StaticCollision> {
    let chunks = glb_chunks(bytes, limits.max_glb_bytes, limits.max_json_bytes)?;
    reject_duplicate_keys(chunks.json)?;
    exact_header_numbers(chunks.json)?;
    let j: Value = serde_json::from_slice(chunks.json).context("invalid GLB JSON")?;
    let root = object(
        &j,
        &[
            "asset",
            "extras",
            "scene",
            "scenes",
            "nodes",
            "meshes",
            "accessors",
            "bufferViews",
            "buffers",
        ],
        "static collision document",
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
    let (header, availability, query_policy) = header(root, limits)?;
    if availability == CollisionAvailability::AvailableEmpty {
        for key in ["nodes", "meshes", "accessors", "bufferViews", "buffers"] {
            ensure!(
                !root.contains_key(key),
                "known-empty collision must omit {key}"
            );
        }
        ensure!(
            chunks.bin.is_none(),
            "known-empty collision must omit BIN chunk"
        );
        selection(root, limits, 0, true)?;
        return Ok(StaticCollision {
            header,
            availability,
            query_policy,
            meshes: vec![],
            instances: vec![],
            bounds: None,
        });
    }
    let bin = chunks
        .bin
        .context("available collision requires BIN chunk")?;
    let views = buffer_views(root, bin, limits.max_resources, AccessorProfile::Collision)?;
    let a = accessors(
        root,
        &views,
        bin,
        limits.max_resources,
        AccessorProfile::Collision,
        limits.max_accessor_validation_bytes,
    )?;
    let specs = meshes(root, &a, limits)?;
    let instances = instances(root, &specs, limits)?;
    local_triangles(&specs)?;
    let bounds = world_bounds(&specs, &instances)?;
    // Retained geometry copies follow complete descriptor, budget, local and world validation.
    let meshes = specs
        .into_iter()
        .map(|m| -> Result<CollisionMesh> {
            Ok(CollisionMesh {
                positions: (0..m.positions.count())
                    .map(|i| Ok(map_axes(m.positions.float(i)?)))
                    .collect::<Result<_>>()?,
                primitives: m
                    .indices
                    .into_iter()
                    .map(|a| -> Result<CollisionPrimitive> {
                        Ok(CollisionPrimitive {
                            indices: (0..a.count()).map(|i| a.index(i)).collect::<Result<_>>()?,
                        })
                    })
                    .collect::<Result<_>>()?,
            })
        })
        .collect::<Result<_>>()?;
    let instances = instances
        .into_iter()
        .map(|i| CollisionInstance {
            mesh_index: i.mesh_index,
            owner_id: i.owner_id,
            matrix: i.server,
        })
        .collect();
    Ok(StaticCollision {
        header,
        availability,
        query_policy,
        meshes,
        instances,
        bounds: Some(bounds),
    })
}
