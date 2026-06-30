use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use glam::{Mat4, Quat, Vec3};
use libeq_wld::parser::{
    Dag, DmSpriteDef2, HierarchicalSpriteDef, Track, TrackDef, WldDoc,
};

struct PrimitiveData {
    indices: Vec<u32>,
    material_idx: usize,
}

struct MeshData {
    name: String,
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    primitives: Vec<PrimitiveData>,
}

struct TextureData {
    name: String,
    png_bytes: Vec<u8>,
}

struct MaterialData {
    name: String,
    texture_idx: Option<usize>,
    base_color: [f32; 4],
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        eprintln!("Usage: s3d_to_gltf <input.s3d> <output.glb>");
        eprintln!("       s3d_to_gltf --skinned <input.s3d> <output.glb>  (skinned + animated)");
        eprintln!("       s3d_to_gltf --batch <input_dir> <output_dir>");
        eprintln!("       s3d_to_gltf --list <input.s3d>");
        eprintln!("       s3d_to_gltf --anims <input.s3d>");
        std::process::exit(0);
    }

    if args[0] == "--models" {
        list_models(&PathBuf::from(&args[1]))?;
        return Ok(());
    } else if args[0] == "--model" {
        // --model <CODE> <input.s3d> <output.glb>  (extract one model from a multi-model archive)
        convert_s3d_to_glb_skinned(
            &PathBuf::from(&args[2]),
            &PathBuf::from(&args[3]),
            Some(&args[1]),
        )?;
        return Ok(());
    } else if args[0] == "--skinned" {
        convert_s3d_to_glb_skinned(&PathBuf::from(&args[1]), &PathBuf::from(&args[2]), None)?;
        return Ok(());
    } else if args[0] == "--anims" {
        analyze_anims(&PathBuf::from(&args[1]))?;
        return Ok(());
    } else if args[0] == "--list" {
        let input = PathBuf::from(&args[1]);
        let file = fs::File::open(&input)?;
        let mut pfs = libeq_pfs::PfsReader::open(file)?;
        let files = pfs.filenames()?;
        for f in &files {
            let lower = f.to_lowercase();
            if lower.ends_with(".wld") {
                println!("[wld] {}", f);
            } else if lower.ends_with(".bmp") || lower.ends_with(".dds") || lower.ends_with(".png") {
                println!("[tex] {}", f);
            }
        }
        for f in &files {
            let lower = f.to_lowercase();
            if lower.ends_with(".wld") {
                if let Ok(Some(wld_bytes)) = pfs.get(f) {
                    if let Ok(wld) = libeq_wld::load(&wld_bytes) {
                        for mesh in wld.meshes() {
                            let positions = mesh.positions();
                            let center = mesh.center();
                            let primitives = mesh.primitives();

                            // Compute scaled position bounds
                            let (mut xmin, mut xmax) = (f32::MAX, f32::MIN);
                            let (mut ymin, mut ymax) = (f32::MAX, f32::MIN);
                            let (mut zmin, mut zmax) = (f32::MAX, f32::MIN);
                            for p in &positions {
                                xmin = xmin.min(p[0]); xmax = xmax.max(p[0]);
                                ymin = ymin.min(p[1]); ymax = ymax.max(p[1]);
                                zmin = zmin.min(p[2]); zmax = zmax.max(p[2]);
                            }

                            println!("  mesh '{}':", mesh.name().unwrap_or("?"));
                            println!("    center=({:.4}, {:.4}, {:.4})", center.0, center.1, center.2);
                            println!("    verts={} prims={}", positions.len(), primitives.len());
                            println!("    pos_bounds: X=[{:.4},{:.4}] Y=[{:.4},{:.4}] Z=[{:.4},{:.4}]",
                                xmin, xmax, ymin, ymax, zmin, zmax);
                            println!("    pos_extent: {:.4} x {:.4} x {:.4}",
                                xmax-xmin, ymax-ymin, zmax-zmin);
                            if !positions.is_empty() {
                                println!("    first_pos: ({:.6}, {:.6}, {:.6})",
                                    positions[0][0], positions[0][1], positions[0][2]);
                            }

                            for (pi, prim) in primitives.iter().enumerate() {
                                let mat = prim.material();
                                let tex_name = mat.base_color_texture()
                                    .and_then(|t| t.source())
                                    .unwrap_or_else(|| "?".to_string());
                                let idx = prim.indices();
                                println!("    prim[{}]: indices={} material='{}' tex='{}'",
                                    pi, idx.len(), mat.name().unwrap_or("?"), tex_name);
                            }
                        }
                    }
                }
            }
        }
    } else if args[0] == "--batch" {
        let input_dir = PathBuf::from(&args[1]);
        let output_dir = PathBuf::from(&args[2]);
        fs::create_dir_all(&output_dir)?;
        for entry in fs::read_dir(&input_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "s3d") {
                let stem = path.file_stem().unwrap().to_string_lossy();
                let out_path = output_dir.join(format!("{}.glb", stem));
                eprintln!("Converting {} -> {}", path.display(), out_path.display());
                match convert_s3d_to_glb(&path, &out_path) {
                    Ok(()) => eprintln!("  OK"),
                    Err(e) => eprintln!("  ERROR: {}", e),
                }
            }
        }
    } else {
        let input = PathBuf::from(&args[0]);
        let output = PathBuf::from(&args[1]);
        convert_s3d_to_glb(&input, &output)?;
    }
    Ok(())
}

fn convert_s3d_to_glb(input: &Path, output: &Path) -> Result<()> {
    let file = fs::File::open(input)
        .with_context(|| format!("failed to open {}", input.display()))?;
    let mut pfs = libeq_pfs::PfsReader::open(file)
        .with_context(|| format!("failed to parse PFS: {}", input.display()))?;

    let filenames: Vec<String> = pfs
        .filenames()
        .with_context(|| "failed to list filenames")?;

    let wld_files: Vec<&str> = filenames
        .iter()
        .filter(|f| f.to_lowercase().ends_with(".wld"))
        .map(|f| f.as_str())
        .collect();

    // Merge ALL WLD meshes into a SINGLE glTF mesh.
    // This is critical because the renderer applies one x_center/z_center offset
    // to all primitives — if meshes are separate, the centering shifts smaller
    // meshes (like eyes) away from the body.
    let mut merged_positions: Vec<[f32; 3]> = Vec::new();
    let mut merged_normals: Vec<[f32; 3]> = Vec::new();
    let mut merged_uvs: Vec<[f32; 2]> = Vec::new();
    let mut merged_primitives: Vec<PrimitiveData> = Vec::new();
    // SOLID-face indices for the dedicated `__collision__` mesh. libeq's
    // `Mesh::collision_indices()` keeps every face whose flag bit 0x0010 is CLEAR — i.e. all
    // SOLID faces, INCLUDING invisible-but-solid ones (zone boundaries, invisible walls,
    // doorframes), while dropping PASSABLE faces (water surfaces, foliage). See libeq
    // `DmSpriteDef2FaceEntry::flags` (0x0010 = "player can pass through"). These reference the
    // merged vertex buffer, so the collision mesh shares the render POSITION accessor.
    let mut merged_collision_indices: Vec<u32> = Vec::new();
    let mut texture_map: HashMap<String, usize> = HashMap::new();
    let mut materials: Vec<MaterialData> = Vec::new();
    let mut textures: Vec<TextureData> = Vec::new();
    let mut total_wld_meshes: usize = 0;

    for wld_name in &wld_files {
        let wld_bytes = match pfs.get(wld_name) {
            Ok(Some(b)) => b,
            Ok(None) => continue,
            Err(e) => {
                eprintln!("  warning: failed to read {}: {}", wld_name, e);
                continue;
            }
        };

        let wld = match libeq_wld::load(&wld_bytes) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("  warning: failed to parse {}: {}", wld_name, e);
                continue;
            }
        };

        // Low-level pass: build the skeletal bind pose and pose every skinned
        // mesh into model space. Character meshes (DmSpriteDef2 with skin
        // assignment groups) store vertices in bone-local space; without the
        // bind-pose bone transforms applied they collapse into an overlapping
        // blob. Keyed by mesh name so the high-level loop below can pick up the
        // posed geometry. Zone/object meshes have no skin groups and fall back
        // to raw positions.
        let posed: HashMap<String, (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<bool>)> =
            match WldDoc::parse(&wld_bytes) {
                Ok(doc) => match build_bind_pose(&doc) {
                    Some(bind) => {
                        let mut map = HashMap::new();
                        for frag in doc.fragment_iter::<DmSpriteDef2>() {
                            let name = doc
                                .get_string(frag.name_reference)
                                .unwrap_or("")
                                .to_string();
                            if let Some(geo) = pose_skinned_mesh(frag, &bind) {
                                map.insert(name, geo);
                            }
                        }
                        eprintln!(
                            "  built bind pose ({} bones), posed {} skinned mesh(es)",
                            bind.world.len(),
                            map.len()
                        );
                        map
                    }
                    None => HashMap::new(),
                },
                Err(_) => HashMap::new(),
            };

        for mesh in wld.meshes() {
            let mesh_name = mesh.name().unwrap_or("unnamed").to_string();

            // Use skeletally-posed geometry if available; otherwise fall back to
            // the raw (already Y-up) positions for non-skinned meshes.
            let posed_geo = posed.get(&mesh_name);
            let (all_positions, all_normals, outliers) = match posed_geo {
                Some((p, n, o)) => (p.clone(), n.clone(), o.clone()),
                None => {
                    let p = mesh.positions();
                    let len = p.len();
                    (p, mesh.normals(), vec![false; len])
                }
            };
            if all_positions.is_empty() {
                continue;
            }

            let all_uvs = mesh.texture_coordinates();

            // Skip eye meshes only when they could NOT be posed. A posed eye mesh
            // is correctly placed at its head bone; an unposed one sits at the
            // origin and gets misaligned by the renderer's centering.
            if posed_geo.is_none() && mesh_name.to_uppercase().contains("EYE") {
                eprintln!("  skipping unposed eye mesh '{}'", mesh_name);
                continue;
            }

            // Index offset: new primitives reference vertices starting from
            // the current end of the merged buffer.
            let index_offset = merged_positions.len() as u32;

            // Append vertices to merged buffer.
            merged_positions.extend_from_slice(&all_positions);
            merged_normals.extend_from_slice(&all_normals);
            merged_uvs.extend_from_slice(&all_uvs);

            // Build per-primitive index lists with offset.
            let mut prim_count = 0;
            for primitive in mesh.primitives() {
                let raw_indices: Vec<u32> = primitive.indices();
                if raw_indices.is_empty() {
                    continue;
                }

                // Drop triangles that reference flagged placeholder vertices
                // (stray attachment-point geometry), then offset the surviving
                // indices into the merged vertex buffer.
                let mut prim_indices: Vec<u32> = Vec::with_capacity(raw_indices.len());
                for tri in raw_indices.chunks_exact(3) {
                    let drop = tri
                        .iter()
                        .any(|&i| outliers.get(i as usize).copied().unwrap_or(false));
                    if drop {
                        continue;
                    }
                    for &i in tri {
                        prim_indices.push(i + index_offset);
                    }
                }
                if prim_indices.is_empty() {
                    continue;
                }

                let material = primitive.material();
                let texture_source = material.base_color_texture().and_then(|t| t.source());

                let material_idx = get_or_create_material(
                    &mut materials,
                    &mut texture_map,
                    &mut textures,
                    &material,
                    texture_source.as_deref(),
                    &mut pfs,
                );

                merged_primitives.push(PrimitiveData {
                    indices: prim_indices,
                    material_idx,
                });
                prim_count += 1;
            }

            // Collect SOLID faces (incl. invisible-but-solid) for the collision mesh, offset
            // into the merged buffer. Drop the same placeholder/outlier vertices the render
            // path drops so the two stay consistent (for zone meshes `outliers` is all-false).
            for tri in mesh.collision_indices().chunks_exact(3) {
                let drop = tri
                    .iter()
                    .any(|&i| outliers.get(i as usize).copied().unwrap_or(false));
                if drop {
                    continue;
                }
                for &i in tri {
                    merged_collision_indices.push(i + index_offset);
                }
            }

            if prim_count > 0 {
                eprintln!("  mesh '{}': {} verts, {} primitives (offset={})",
                    mesh_name, all_positions.len(), prim_count, index_offset);
                total_wld_meshes += 1;
            }
        }
    }

    if merged_primitives.is_empty() {
        anyhow::bail!("no meshes found in {}", input.display());
    }

    // Create a single merged mesh.
    let all_meshes = vec![MeshData {
        name: "combined".to_string(),
        positions: merged_positions,
        normals: merged_normals,
        uvs: merged_uvs,
        primitives: merged_primitives,
    }];

    eprintln!(
        "  merged {} WLD meshes into 1 glTF mesh ({} verts, {} prims), {} materials, {} textures, {} collision tris",
        total_wld_meshes,
        all_meshes[0].positions.len(),
        all_meshes[0].primitives.len(),
        materials.len(),
        textures.len(),
        merged_collision_indices.len() / 3,
    );

    write_glb(output, &all_meshes, &materials, &textures, &merged_collision_indices)
}

fn get_or_create_material(
    materials: &mut Vec<MaterialData>,
    texture_map: &mut HashMap<String, usize>,
    textures: &mut Vec<TextureData>,
    material: &libeq_wld::Material<'_>,
    texture_source: Option<&str>,
    pfs: &mut libeq_pfs::PfsReader<fs::File>,
) -> usize {
    let mat_name = material.name().unwrap_or("unnamed").to_string();

    let texture_idx = if let Some(src) = texture_source {
        if let Some(&idx) = texture_map.get(src) {
            Some(idx)
        } else {
            let tex_name = src.to_lowercase();
            match load_texture_from_archive(pfs, &tex_name) {
                Some(png_bytes) => {
                    let idx = textures.len();
                    textures.push(TextureData {
                        name: tex_name,
                        png_bytes,
                    });
                    texture_map.insert(src.to_string(), idx);
                    Some(idx)
                }
                None => None,
            }
        }
    } else {
        None
    };

    let idx = materials.len();
    materials.push(MaterialData {
        name: mat_name,
        texture_idx,
        base_color: [1.0, 1.0, 1.0, 1.0],
    });
    idx
}

fn load_texture_from_archive(pfs: &mut libeq_pfs::PfsReader<fs::File>, name: &str) -> Option<Vec<u8>> {
    let lower = name.to_lowercase();

    // Try the name as-is first
    if let Some(data) = try_load_image(pfs, &lower) {
        return Some(data);
    }

    // Try stripping extension and trying common ones
    let stem = if lower.ends_with(".dds") || lower.ends_with(".bmp") || lower.ends_with(".png") {
        &lower[..lower.len() - 4]
    } else {
        &lower
    };

    for ext in &[".dds", ".bmp", ".png"] {
        let filename = format!("{}{}", stem, ext);
        if let Some(data) = try_load_image(pfs, &filename) {
            return Some(data);
        }
    }
    None
}

fn try_load_image(pfs: &mut libeq_pfs::PfsReader<fs::File>, filename: &str) -> Option<Vec<u8>> {
    let data = pfs.get(filename).ok()??;
    image::load_from_memory(&data).ok().and_then(|img| {
        let rgba = img.to_rgba8();
        let mut png_buf = Cursor::new(Vec::new());
        rgba.write_to(&mut png_buf, image::ImageFormat::Png).ok()?;
        Some(png_buf.into_inner())
    })
}

/// A skeletal bind pose: world-space matrix per bone (indexed by dag index), in
/// EQ native coordinate space (Z-up). Built by walking the HierarchicalSpriteDef
/// dag tree and composing each bone's local transform (frame 0 of its TrackDef)
/// with its parent's world transform.
struct BindPose {
    world: Vec<Mat4>,
}

/// Resolve a dag's TrackDef (the per-bone transform) by following
/// dag.track_reference (1-based fragment index) -> Track (0x13) -> TrackDef (0x12).
fn dag_track_def<'a>(doc: &'a WldDoc, dag: &Dag) -> Option<&'a TrackDef> {
    if dag.track_reference == 0 {
        return None;
    }
    let track = doc
        .at((dag.track_reference - 1) as usize)?
        .as_any()
        .downcast_ref::<Track>()?;
    doc.get(&track.reference)
}

/// Decode a single animation frame (translation, rotation) in EQ-native space.
/// New-format (FrameTransform): rotation quaternion = (x,y,z,denominator) normalized,
/// translation = shift_xyz / shift_denominator. Legacy format stores floats directly.
fn frame_trs(td: &TrackDef, frame: usize) -> (Vec3, Quat) {
    if let Some(frames) = &td.frame_transforms {
        if let Some(f) = frames.get(frame).or_else(|| frames.first()) {
            let rot = if f.rotate_denominator != 0 {
                Quat::from_xyzw(
                    f.rotate_x_numerator as f32,
                    f.rotate_y_numerator as f32,
                    f.rotate_z_numerator as f32,
                    f.rotate_denominator as f32,
                )
                .normalize()
            } else {
                Quat::IDENTITY
            };
            let trans = if f.shift_denominator != 0 {
                let d = f.shift_denominator as f32;
                Vec3::new(
                    f.shift_x_numerator as f32 / d,
                    f.shift_y_numerator as f32 / d,
                    f.shift_z_numerator as f32 / d,
                )
            } else {
                Vec3::ZERO
            };
            return (trans, rot);
        }
    }
    if let Some(frames) = &td.legacy_frame_transforms {
        if let Some(f) = frames.get(frame).or_else(|| frames.first()) {
            let rot = Quat::from_xyzw(f.rotate_x, f.rotate_y, f.rotate_z, f.rotate_w).normalize();
            let trans = if f.shift_denominator != 0.0 {
                Vec3::new(
                    f.shift_x_numerator / f.shift_denominator,
                    f.shift_y_numerator / f.shift_denominator,
                    f.shift_z_numerator / f.shift_denominator,
                )
            } else {
                Vec3::ZERO
            };
            return (trans, rot);
        }
    }
    (Vec3::ZERO, Quat::IDENTITY)
}

/// Build the local (relative-to-parent) bind transform from frame 0 of a TrackDef.
fn track_local_matrix(td: &TrackDef) -> Mat4 {
    let (t, r) = frame_trs(td, 0);
    Mat4::from_rotation_translation(r, t)
}

fn walk_dag(
    doc: &WldDoc,
    dags: &[Dag],
    idx: usize,
    parent: Mat4,
    world: &mut [Mat4],
    visited: &mut [bool],
) {
    if idx >= dags.len() || visited[idx] {
        return;
    }
    visited[idx] = true;
    let local = dag_track_def(doc, &dags[idx])
        .map(track_local_matrix)
        .unwrap_or(Mat4::IDENTITY);
    let w = parent * local;
    world[idx] = w;
    for &child in &dags[idx].sub_dags {
        walk_dag(doc, dags, child as usize, w, world, visited);
    }
}

/// Build the bind pose from the first HierarchicalSpriteDef in the document.
fn build_bind_pose(doc: &WldDoc) -> Option<BindPose> {
    let skel = doc.fragment_iter::<HierarchicalSpriteDef>().next()?;
    let n = skel.dags.len();
    if n == 0 {
        return None;
    }
    let mut world = vec![Mat4::IDENTITY; n];
    let mut visited = vec![false; n];
    walk_dag(doc, &skel.dags, 0, Mat4::IDENTITY, &mut world, &mut visited);
    if std::env::var("DEBUG_SKEL").is_ok() {
        let unreached: Vec<usize> = (0..n).filter(|&i| !visited[i]).collect();
        eprintln!(
            "  [skel] {} dags, {} unreached from root: {:?}",
            n,
            unreached.len(),
            unreached
        );
    }
    Some(BindPose { world })
}

/// Pose a skinned mesh into model space using the bind pose. Returns Y-up
/// positions and normals (matching libeq's high-level convention, which swaps
/// EQ Z-up to glTF Y-up). Returns None for non-skinned meshes (zone geometry).
/// Flag placeholder/degenerate vertices. A handful of vertices in some character
/// meshes are assigned to non-rendering attachment bones (weapon/shield mount
/// points) and sit at extreme positions, producing a stray triangle that wrecks
/// the bounding box (which in turn breaks the renderer's auto-scaling). Flag any
/// vertex whose distance from the median vertex position is a gross outlier,
/// relative to the model's own scale so large creatures aren't harmed.
fn detect_outliers(positions: &[[f32; 3]]) -> Vec<bool> {
    let n = positions.len();
    if n < 8 {
        return vec![false; n];
    }
    let median = |axis: usize| -> f32 {
        let mut vals: Vec<f32> = positions.iter().map(|p| p[axis]).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
        vals[vals.len() / 2]
    };
    let c = [median(0), median(1), median(2)];
    let mut dists: Vec<f32> = positions
        .iter()
        .map(|p| {
            let d = [p[0] - c[0], p[1] - c[1], p[2] - c[2]];
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
        })
        .collect();
    let mut sorted = dists.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median_dist = sorted[sorted.len() / 2];
    let threshold = (median_dist * 6.0).max(4.0);
    dists.iter_mut().map(|d| *d > threshold).collect()
}

fn pose_skinned_mesh(
    frag: &DmSpriteDef2,
    bind: &BindPose,
) -> Option<(Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<bool>)> {
    if frag.skin_assignment_groups.is_empty() {
        return None;
    }
    let scale = 1.0 / ((1u32 << frag.scale) as f32);

    // Expand (count, bone_index) groups into a per-vertex bone index.
    let mut vbone: Vec<usize> = Vec::with_capacity(frag.positions.len());
    for (count, bone) in &frag.skin_assignment_groups {
        for _ in 0..*count {
            vbone.push(*bone as usize);
        }
    }
    let last = vbone.last().copied().unwrap_or(0);
    while vbone.len() < frag.positions.len() {
        vbone.push(last);
    }

    let nbones = bind.world.len();
    if std::env::var("DEBUG_SKEL").is_ok() {
        let max_bone = frag.skin_assignment_groups.iter().map(|g| g.1).max().unwrap_or(0);
        eprintln!(
            "  [skel] mesh skin groups={} max_bone_idx={} nbones={} total_assigned={}",
            frag.skin_assignment_groups.len(),
            max_bone,
            nbones,
            vbone.len()
        );
    }
    let mut positions = Vec::with_capacity(frag.positions.len());
    let mut normals = Vec::with_capacity(frag.positions.len());
    let debug = std::env::var("DEBUG_SKEL").is_ok();
    for (i, p) in frag.positions.iter().enumerate() {
        let b = vbone[i].min(nbones - 1);
        let m = bind.world[b];
        let v = Vec3::new(p.0 as f32 * scale, p.1 as f32 * scale, p.2 as f32 * scale);
        let w = m.transform_point3(v);
        if debug && (v.length() > 4.0 || w.length() > 6.0) {
            let bt = m.w_axis.truncate();
            eprintln!(
                "    [vtx {}] bone={} raw=({:.2},{:.2},{:.2}) |raw|={:.2} posed=({:.2},{:.2},{:.2}) bone_translation=({:.2},{:.2},{:.2})",
                i, b, v.x, v.y, v.z, v.length(), w.x, w.y, w.z, bt.x, bt.y, bt.z
            );
        }
        positions.push([w.x, w.z, w.y]);

        let n = frag
            .vertex_normals
            .get(i)
            .map(|n| Vec3::new(n.0 as f32 / 127.0, n.1 as f32 / 127.0, n.2 as f32 / 127.0))
            .unwrap_or(Vec3::Y);
        let nw = m.transform_vector3(n).normalize_or_zero();
        normals.push([nw.x, nw.z, nw.y]);
    }
    let outliers = detect_outliers(&positions);
    if debug {
        let n_out = outliers.iter().filter(|&&o| o).count();
        if n_out > 0 {
            eprintln!("    [skel] flagged {} outlier (placeholder) vert(s)", n_out);
        }
    }
    Some((positions, normals, outliers))
}

/// Rich skeleton: per-bone parent, rest-local transform (translation+rotation),
/// world bind matrix, and the bone's base track name (used to build the names of
/// animated tracks).
struct Skel {
    #[allow(dead_code)]
    parent: Vec<Option<usize>>,
    local_t: Vec<Vec3>,
    local_r: Vec<Quat>,
    world: Vec<Mat4>,
    base_track: Vec<String>,
    children: Vec<Vec<usize>>,
}

fn build_skel(doc: &WldDoc, code: Option<&str>) -> Option<Skel> {
    let skel = match code {
        // Pick the skeleton whose name starts with the model code (e.g. "BEA_HS_DEF").
        Some(c) => doc.fragment_iter::<HierarchicalSpriteDef>().find(|s| {
            doc.get_string(s.name_reference)
                .map(|n| n.starts_with(c))
                .unwrap_or(false)
        })?,
        None => doc.fragment_iter::<HierarchicalSpriteDef>().next()?,
    };
    let n = skel.dags.len();
    if n == 0 {
        return None;
    }
    let mut parent = vec![None; n];
    let mut local_t = vec![Vec3::ZERO; n];
    let mut local_r = vec![Quat::IDENTITY; n];
    let mut world = vec![Mat4::IDENTITY; n];
    let mut base_track = vec![String::new(); n];
    let mut children = vec![Vec::new(); n];

    for (i, dag) in skel.dags.iter().enumerate() {
        if let Some(td) = dag_track_def(doc, dag) {
            let (t, r) = frame_trs(td, 0);
            local_t[i] = t;
            local_r[i] = r;
        }
        // The bone's base track name (e.g. "DWMPEBIP01_TRACK").
        if dag.track_reference > 0 {
            if let Some(track) = doc
                .at((dag.track_reference - 1) as usize)
                .and_then(|f| f.as_any().downcast_ref::<Track>())
            {
                base_track[i] = doc.get_string(track.name_reference).unwrap_or("").to_string();
            }
        }
        for &c in &dag.sub_dags {
            let c = c as usize;
            if c < n {
                children[i].push(c);
                parent[c] = Some(i);
            }
        }
    }

    // Compute world bind matrices by walking from the root (index 0).
    let mut visited = vec![false; n];
    let mut stack = vec![(0usize, Mat4::IDENTITY)];
    while let Some((idx, parent_world)) = stack.pop() {
        if idx >= n || visited[idx] {
            continue;
        }
        visited[idx] = true;
        let w = parent_world * Mat4::from_rotation_translation(local_r[idx], local_t[idx]);
        world[idx] = w;
        for &c in &children[idx] {
            stack.push((c, w));
        }
    }

    Some(Skel {
        parent,
        local_t,
        local_r,
        world,
        base_track,
        children,
    })
}

/// One animation clip: a 3-char EQ code (e.g. "L01") plus, per bone, an optional
/// list of per-frame (translation, rotation) transforms in EQ-native space.
struct Anim {
    code: String,
    frame_ms: u32,
    /// Indexed by bone; None means the bone has no track in this clip (holds rest).
    bones: Vec<Option<Vec<(Vec3, Quat)>>>,
}

/// Resolve a Track (0x13) fragment by exact name, then its TrackDef (0x12).
fn track_def_by_name<'a>(doc: &'a WldDoc, name: &str) -> Option<(&'a Track, &'a TrackDef)> {
    for t in doc.fragment_iter::<Track>() {
        if doc.get_string(t.name_reference) == Some(name) {
            if let Some(td) = doc.get(&t.reference) {
                return Some((t, td));
            }
        }
    }
    None
}

/// Discover all animation clips by scanning Track names for a leading 3-char code
/// and matching them to each bone's base track name.
fn gather_anims(doc: &WldDoc, skel: &Skel) -> Vec<Anim> {
    use std::collections::{BTreeSet, HashMap};
    // Map each bone's base track name -> bone index. Animated tracks are named
    // <animCode><baseTrackName>; the code length varies (3 chars on classic
    // global_chr models, e.g. "C01"; 4 chars on Luclin high-res, e.g. "C01A").
    // So detect by suffix-matching the (longest) base track name rather than
    // assuming a fixed code length.
    let mut base_to_bone: HashMap<&str, usize> = HashMap::new();
    for (i, b) in skel.base_track.iter().enumerate() {
        if !b.is_empty() {
            base_to_bone.insert(b.as_str(), i);
        }
    }
    // Longest base names first so e.g. "SKEPE_TRACK" wins over "SKE_TRACK".
    let mut bases_by_len: Vec<&str> = base_to_bone.keys().copied().collect();
    bases_by_len.sort_by_key(|b| std::cmp::Reverse(b.len()));

    let is_code = |code: &str| -> bool {
        let len = code.len();
        if !(3..=4).contains(&len) {
            return false;
        }
        let cb = code.as_bytes();
        // letter, digit, digit, [optional letter]
        cb[0].is_ascii_alphabetic()
            && cb[1].is_ascii_digit()
            && cb[2].is_ascii_digit()
            && (len == 3 || (len == 4 && cb[3].is_ascii_alphabetic()))
    };

    let mut codes: BTreeSet<String> = BTreeSet::new();
    for t in doc.fragment_iter::<Track>() {
        let name = match doc.get_string(t.name_reference) {
            Some(n) => n,
            None => continue,
        };
        for b in &bases_by_len {
            if name.len() > b.len() && name.ends_with(b) {
                let code = &name[..name.len() - b.len()];
                if is_code(code) {
                    codes.insert(code.to_string());
                }
                break;
            }
        }
    }

    if std::env::var("DEBUG_SKEL").is_ok() {
        eprintln!(
            "  [anim] base_names={} codes_found={}: {:?}",
            base_to_bone.len(),
            codes.len(),
            codes.iter().take(20).collect::<Vec<_>>()
        );
    }
    let mut anims = Vec::new();
    for code in codes {
        let mut bones: Vec<Option<Vec<(Vec3, Quat)>>> = vec![None; skel.base_track.len()];
        let mut frame_ms = 100u32;
        for (i, base) in skel.base_track.iter().enumerate() {
            if base.is_empty() {
                continue;
            }
            let anim_name = format!("{}{}", code, base);
            if let Some((track, td)) = track_def_by_name(doc, &anim_name) {
                let fc = td.frame_count.max(1) as usize;
                let frames: Vec<(Vec3, Quat)> = (0..fc).map(|f| frame_trs(td, f)).collect();
                bones[i] = Some(frames);
                if let Some(s) = track.sleep {
                    if s > 0 {
                        frame_ms = s;
                    }
                }
            }
        }
        if bones.iter().any(|b| b.is_some()) {
            anims.push(Anim {
                code,
                frame_ms,
                bones,
            });
        }
    }
    anims
}

/// Map an EQ animation code (first 3 chars, e.g. "L01") to a semantic keyword so
/// the renderer's name-substring clip selection ("idle"/"walk"/"run"/...) works.
/// Standard EQ WLD locomotion/passive codes.
fn anim_label(code3: &str) -> Option<&'static str> {
    Some(match code3 {
        "L01" => "walk",
        "L02" => "run",
        "L03" => "jump_run",
        "L04" => "fall",
        "L05" => "duckwalk",
        "L06" => "swim",
        "L07" => "walk_back",
        "L08" => "swim_idle",
        "L09" => "swim",
        "P01" => "idle_neutral",
        "P02" => "sit",
        "P03" => "crouch",
        "P06" => "kneel",
        "P07" => "swim_idle",
        // Only O01/O02 are standing idle fidgets ("standby"/"standby2"). O03 is a low
        // crouched/looting pose, NOT a standing idle — keep it out of the "idle" label so the
        // client's idle cycle never selects it. Confirmed vs the Titanium client + EQEmu.
        "O01" | "O02" => "idle",
        "O03" => "looting",
        c if c.starts_with('C') => "combat",
        "D05" => "death",
        c if c.starts_with('D') => "hit",
        c if c.starts_with('S') => "social",
        c if c.starts_with('T') => "emote",
        _ => return None,
    })
}

/// Per-vertex skin data plus EQ-space bind-pose geometry for one mesh.
struct SkinnedGeo {
    positions: Vec<[f32; 3]>, // EQ-native (z-up) bind pose, model space
    normals: Vec<[f32; 3]>,   // EQ-native
    joints: Vec<u16>,         // assigned bone index per vertex
    outliers: Vec<bool>,
}

fn gather_skinned_geo(frag: &DmSpriteDef2, skel: &Skel) -> Option<SkinnedGeo> {
    if frag.skin_assignment_groups.is_empty() {
        return None;
    }
    let scale = 1.0 / ((1u32 << frag.scale) as f32);
    let mut vbone: Vec<usize> = Vec::with_capacity(frag.positions.len());
    for (count, bone) in &frag.skin_assignment_groups {
        for _ in 0..*count {
            vbone.push(*bone as usize);
        }
    }
    let last = vbone.last().copied().unwrap_or(0);
    while vbone.len() < frag.positions.len() {
        vbone.push(last);
    }

    let nbones = skel.world.len();
    let mut positions = Vec::with_capacity(frag.positions.len());
    let mut normals = Vec::with_capacity(frag.positions.len());
    let mut joints = Vec::with_capacity(frag.positions.len());
    // For outlier detection we evaluate the (swapped) posed positions just like the
    // static path, so the same robust threshold applies.
    let mut posed_for_outlier = Vec::with_capacity(frag.positions.len());
    for (i, p) in frag.positions.iter().enumerate() {
        let b = vbone[i].min(nbones - 1);
        let v = Vec3::new(p.0 as f32 * scale, p.1 as f32 * scale, p.2 as f32 * scale);
        let w = skel.world[b].transform_point3(v);
        positions.push([w.x, w.y, w.z]); // EQ-native bind pose
        posed_for_outlier.push([w.x, w.z, w.y]);
        let n = frag
            .vertex_normals
            .get(i)
            .map(|n| Vec3::new(n.0 as f32 / 127.0, n.1 as f32 / 127.0, n.2 as f32 / 127.0))
            .unwrap_or(Vec3::Y);
        let nw = skel.world[b].transform_vector3(n).normalize_or_zero();
        normals.push([nw.x, nw.y, nw.z]);
        joints.push(b as u16);
    }
    let outliers = detect_outliers(&posed_for_outlier);
    Some(SkinnedGeo {
        positions,
        normals,
        joints,
        outliers,
    })
}

fn convert_s3d_to_glb_skinned(input: &Path, output: &Path, model_code: Option<&str>) -> Result<()> {
    let file = fs::File::open(input)
        .with_context(|| format!("failed to open {}", input.display()))?;
    let mut pfs = libeq_pfs::PfsReader::open(file)?;
    let filenames: Vec<String> = pfs.filenames()?;
    let wld_files: Vec<String> = filenames
        .iter()
        .filter(|f| f.to_lowercase().ends_with(".wld"))
        .cloned()
        .collect();

    for wld_name in &wld_files {
        let wld_bytes = match pfs.get(wld_name) {
            Ok(Some(b)) => b,
            _ => continue,
        };
        let doc = match WldDoc::parse(&wld_bytes) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let skel = match build_skel(&doc, model_code) {
            Some(s) => s,
            None => continue,
        };
        let anims = gather_anims(&doc, &skel);

        // When extracting one model from a multi-model archive, only its meshes
        // (name starts with the code) belong to this skeleton's skin_assignment bones.
        let mesh_belongs = |name: &str| match model_code {
            Some(c) => name.starts_with(c),
            None => true,
        };

        let mut geo_map: HashMap<String, SkinnedGeo> = HashMap::new();
        for frag in doc.fragment_iter::<DmSpriteDef2>() {
            let name = doc.get_string(frag.name_reference).unwrap_or("").to_string();
            if !mesh_belongs(&name) {
                continue;
            }
            if let Some(g) = gather_skinned_geo(frag, &skel) {
                geo_map.insert(name, g);
            }
        }

        let wld = libeq_wld::load(&wld_bytes).map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut positions: Vec<[f32; 3]> = Vec::new();
        let mut normals: Vec<[f32; 3]> = Vec::new();
        let mut uvs: Vec<[f32; 2]> = Vec::new();
        let mut joints: Vec<u16> = Vec::new();
        let mut prims: Vec<PrimitiveData> = Vec::new();
        let mut texture_map: HashMap<String, usize> = HashMap::new();
        let mut materials: Vec<MaterialData> = Vec::new();
        let mut textures: Vec<TextureData> = Vec::new();

        for mesh in wld.meshes() {
            let name = mesh.name().unwrap_or("").to_string();
            let geo = match geo_map.get(&name) {
                Some(g) => g,
                None => continue,
            };
            let mesh_uvs = mesh.texture_coordinates();
            let offset = positions.len() as u32;
            for i in 0..geo.positions.len() {
                positions.push(geo.positions[i]);
                normals.push(geo.normals[i]);
                uvs.push(mesh_uvs.get(i).copied().unwrap_or([0.0, 0.0]));
                joints.push(geo.joints[i]);
            }
            for primitive in mesh.primitives() {
                let raw: Vec<u32> = primitive.indices();
                let mut idxs: Vec<u32> = Vec::with_capacity(raw.len());
                for tri in raw.chunks_exact(3) {
                    if tri
                        .iter()
                        .any(|&i| geo.outliers.get(i as usize).copied().unwrap_or(false))
                    {
                        continue;
                    }
                    for &i in tri {
                        idxs.push(i + offset);
                    }
                }
                if idxs.is_empty() {
                    continue;
                }
                let material = primitive.material();
                let tex = material.base_color_texture().and_then(|t| t.source());
                let midx = get_or_create_material(
                    &mut materials,
                    &mut texture_map,
                    &mut textures,
                    &material,
                    tex.as_deref(),
                    &mut pfs,
                );
                prims.push(PrimitiveData {
                    indices: idxs,
                    material_idx: midx,
                });
            }
        }

        if prims.is_empty() {
            anyhow::bail!("no skinned meshes found in {}", input.display());
        }

        eprintln!(
            "  skinned: {} verts, {} prims, {} bones, {} anim clips ({})",
            positions.len(),
            prims.len(),
            skel.world.len(),
            anims.len(),
            anims.iter().map(|a| a.code.as_str()).collect::<Vec<_>>().join(",")
        );
        return write_glb_skinned(
            output, &positions, &normals, &uvs, &joints, &prims, &materials, &textures, &skel,
            &anims,
        );
    }
    anyhow::bail!("no skeleton found in {}", input.display());
}

// ── glTF buffer authoring helpers ────────────────────────────────────────────

fn align4(buf: &mut Vec<u8>) {
    while buf.len() % 4 != 0 {
        buf.push(0);
    }
}

fn add_view(
    buf: &mut Vec<u8>,
    views: &mut Vec<serde_json::Value>,
    bytes: &[u8],
    target: Option<u32>,
) -> usize {
    align4(buf);
    let offset = buf.len();
    buf.extend_from_slice(bytes);
    let mut v = serde_json::json!({
        "buffer": 0,
        "byteOffset": offset,
        "byteLength": bytes.len(),
    });
    if let Some(t) = target {
        v["target"] = serde_json::json!(t);
    }
    views.push(v);
    views.len() - 1
}

fn add_accessor(
    accessors: &mut Vec<serde_json::Value>,
    view: usize,
    component_type: u32,
    count: usize,
    typ: &str,
    minmax: Option<(serde_json::Value, serde_json::Value)>,
) -> usize {
    let mut a = serde_json::json!({
        "bufferView": view,
        "componentType": component_type,
        "count": count,
        "type": typ,
    });
    if let Some((mn, mx)) = minmax {
        a["min"] = mn;
        a["max"] = mx;
    }
    accessors.push(a);
    accessors.len() - 1
}

fn f32x3_bytes(data: &[[f32; 3]]) -> Vec<u8> {
    let mut b = Vec::with_capacity(data.len() * 12);
    for p in data {
        for c in p {
            b.extend_from_slice(&c.to_le_bytes());
        }
    }
    b
}

#[allow(clippy::too_many_arguments)]
fn write_glb_skinned(
    output: &Path,
    positions: &[[f32; 3]],
    normals: &[[f32; 3]],
    uvs: &[[f32; 2]],
    joints: &[u16],
    prims: &[PrimitiveData],
    materials: &[MaterialData],
    textures: &[TextureData],
    skel: &Skel,
    anims: &[Anim],
) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut views: Vec<serde_json::Value> = Vec::new();
    let mut accessors: Vec<serde_json::Value> = Vec::new();
    let mut images: Vec<serde_json::Value> = Vec::new();
    let mut gltf_textures: Vec<serde_json::Value> = Vec::new();
    let mut gltf_materials: Vec<serde_json::Value> = Vec::new();

    // Textures + materials (same as static path).
    for tex in textures {
        let vi = add_view(&mut buf, &mut views, &tex.png_bytes, None);
        images.push(serde_json::json!({ "bufferView": vi, "mimeType": "image/png", "name": tex.name }));
        gltf_textures.push(serde_json::json!({ "source": images.len() - 1 }));
    }
    for mat in materials {
        let pbr = if let Some(ti) = mat.texture_idx {
            serde_json::json!({ "baseColorTexture": { "index": ti }, "metallicFactor": 0.0, "roughnessFactor": 1.0 })
        } else {
            serde_json::json!({ "baseColorFactor": mat.base_color, "metallicFactor": 0.0, "roughnessFactor": 1.0 })
        };
        gltf_materials.push(serde_json::json!({ "name": mat.name, "pbrMetallicRoughness": pbr, "doubleSided": true }));
    }

    // Everything is authored in EQ-native space (Z-up). To present a Y-up glTF we
    // rotate the whole rig by `rq` (Z-up -> Y-up). This rotation is baked into the
    // vertex data, joint local transforms, inverse-bind matrices and animation
    // frames (by conjugation) rather than a shared root node — a shared ancestor
    // would be cancelled out by glTF's skinning math (skin matrices factor out the
    // mesh node's global transform).
    let rq = Quat::from_axis_angle(Vec3::X, -std::f32::consts::FRAC_PI_2);
    let rmat = Mat4::from_quat(rq);
    let rot_p = |p: &[f32; 3]| -> [f32; 3] {
        let v = rq * Vec3::from_array(*p);
        [v.x, v.y, v.z]
    };
    let rot_positions: Vec<[f32; 3]> = positions.iter().map(rot_p).collect();
    let rot_normals: Vec<[f32; 3]> = normals.iter().map(rot_p).collect();

    // ── Translation normalization (EQ-native Z-up, BEFORE the Y-up rotation) ──────
    // Compute the posed bind bbox over the un-rotated, EQ-native `positions`
    // (z = height). Center the two horizontal axes (X, Y) and ground the height
    // axis (feet to z = 0). The resulting `offset` is added to the skeleton root
    // bone's rest translation and to every root-bone animation keyframe, but NOT to
    // the inverse-bind matrices (those stay derived from the ORIGINAL skel.world).
    // Because world (rest + every animated pose) carries +offset while the inverse-
    // bind does not, the world-offset and inverse-bind-offset do NOT cancel, so the
    // skinned result is uniformly translated by `offset` in bind pose and in every
    // clip. eq_height is the height extent (offset preserves extent).
    let (mut xmin, mut xmax) = (f32::MAX, f32::MIN);
    let (mut ymin, mut ymax) = (f32::MAX, f32::MIN);
    let (mut zmin, mut zmax) = (f32::MAX, f32::MIN);
    for p in positions {
        xmin = xmin.min(p[0]);
        xmax = xmax.max(p[0]);
        ymin = ymin.min(p[1]);
        ymax = ymax.max(p[1]);
        zmin = zmin.min(p[2]);
        zmax = zmax.max(p[2]);
    }
    let eq_height = (zmax - zmin).max(0.0);
    // Conversion-time translation normalization was reverted: it offset the root bone in
    // rest + every animation keyframe, but a single offset cannot center every clip (each
    // has its own root baseline), which displaced the model during animation. Centering is
    // now done at render time from measured bounds. Keep eq_height (height extent) for the
    // renderer's target-height scaling; apply no skeleton offset.
    let _ = (xmin, xmax, ymin, ymax);
    let offset = Vec3::ZERO;

    // Vertex attributes (shared across primitives).
    let (pmin, pmax) = compute_bounds_f32x3(&rot_positions);
    let pos_view = add_view(&mut buf, &mut views, &f32x3_bytes(&rot_positions), Some(34962));
    let pos_acc = add_accessor(
        &mut accessors, pos_view, 5126, rot_positions.len(), "VEC3",
        Some((serde_json::json!(pmin), serde_json::json!(pmax))),
    );
    let nrm_view = add_view(&mut buf, &mut views, &f32x3_bytes(&rot_normals), Some(34962));
    let nrm_acc = add_accessor(&mut accessors, nrm_view, 5126, rot_normals.len(), "VEC3", None);
    let mut uv_bytes = Vec::with_capacity(uvs.len() * 8);
    for u in uvs {
        uv_bytes.extend_from_slice(&u[0].to_le_bytes());
        uv_bytes.extend_from_slice(&u[1].to_le_bytes());
    }
    let uv_view = add_view(&mut buf, &mut views, &uv_bytes, Some(34962));
    let uv_acc = add_accessor(&mut accessors, uv_view, 5126, uvs.len(), "VEC2", None);
    // JOINTS_0 (u16 vec4) and WEIGHTS_0 (f32 vec4).
    let mut j_bytes = Vec::with_capacity(joints.len() * 8);
    let mut w_bytes = Vec::with_capacity(joints.len() * 16);
    for &j in joints {
        for c in [j, 0u16, 0, 0] {
            j_bytes.extend_from_slice(&c.to_le_bytes());
        }
        for c in [1.0f32, 0.0, 0.0, 0.0] {
            w_bytes.extend_from_slice(&c.to_le_bytes());
        }
    }
    let j_view = add_view(&mut buf, &mut views, &j_bytes, Some(34962));
    let j_acc = add_accessor(&mut accessors, j_view, 5123, joints.len(), "VEC4", None);
    let w_view = add_view(&mut buf, &mut views, &w_bytes, Some(34962));
    let w_acc = add_accessor(&mut accessors, w_view, 5126, joints.len(), "VEC4", None);

    // Primitives (per material), each with its own index accessor.
    let mut gltf_prims = Vec::new();
    for prim in prims {
        let mut ib = Vec::with_capacity(prim.indices.len() * 2);
        for &i in &prim.indices {
            ib.extend_from_slice(&(i as u16).to_le_bytes());
        }
        let iv = add_view(&mut buf, &mut views, &ib, Some(34963));
        let ia = add_accessor(&mut accessors, iv, 5123, prim.indices.len(), "SCALAR", None);
        gltf_prims.push(serde_json::json!({
            "attributes": {
                "POSITION": pos_acc, "NORMAL": nrm_acc, "TEXCOORD_0": uv_acc,
                "JOINTS_0": j_acc, "WEIGHTS_0": w_acc,
            },
            "indices": ia,
            "material": prim.material_idx,
        }));
    }

    // Inverse bind matrices in the rotated (Y-up) space, column-major.
    // world' = rmat * world_EQ * rmat^-1; invBind = (world')^-1.
    let n = skel.world.len();
    let rmat_inv = rmat.inverse();
    let mut ibm = Vec::with_capacity(n * 64);
    for w in &skel.world {
        let world_yup = rmat * *w * rmat_inv;
        for c in world_yup.inverse().to_cols_array() {
            ibm.extend_from_slice(&c.to_le_bytes());
        }
    }
    let ibm_view = add_view(&mut buf, &mut views, &ibm, None);
    let ibm_acc = add_accessor(&mut accessors, ibm_view, 5126, n, "MAT4", None);

    // ── Nodes ────────────────────────────────────────────────────────────────
    // Joint nodes 0..n-1, with each local transform conjugated by the Y-up
    // rotation: local' = rmat * local * rmat^-1, i.e. t' = rq*t, r' = rq*r*rq^-1.
    // Then the skinned mesh node (a sibling of the joint root, NOT an ancestor).
    let rq_conj = rq.conjugate();
    let mut nodes: Vec<serde_json::Value> = Vec::with_capacity(n + 1);
    for i in 0..n {
        // Root bone (index 0) carries the normalization offset (EQ-native), applied
        // before the Y-up rotation. Children are relative to the root and inherit it.
        let local_t = if i == 0 { skel.local_t[i] + offset } else { skel.local_t[i] };
        let t = rq * local_t;
        let r = (rq * skel.local_r[i] * rq_conj).normalize();
        let mut node = serde_json::json!({
            "translation": [t.x, t.y, t.z],
            "rotation": [r.x, r.y, r.z, r.w],
        });
        if !skel.children[i].is_empty() {
            node["children"] = serde_json::json!(skel.children[i]);
        }
        nodes.push(node);
    }
    // Record the model's true height (EQ-native height extent) on the skeleton root
    // node so the renderer can scale/ground consistently. node 0 is the root joint.
    nodes[0]["extras"] = serde_json::json!({ "eq_height": eq_height });
    let mesh_idx = n;
    nodes.push(serde_json::json!({ "name": "mesh", "mesh": 0, "skin": 0 }));

    let skin = serde_json::json!({
        "joints": (0..n).collect::<Vec<_>>(),
        "inverseBindMatrices": ibm_acc,
        "skeleton": 0,
    });

    // ── Animations ─────────────────────────────────────────────────────────────
    let mut gltf_anims = Vec::new();
    for anim in anims {
        let mut channels = Vec::new();
        let mut samplers = Vec::new();
        let dt = anim.frame_ms as f32 / 1000.0;
        for (bone, frames_opt) in anim.bones.iter().enumerate() {
            let frames = match frames_opt {
                Some(f) if !f.is_empty() => f,
                _ => continue,
            };
            // Shared time input for this bone's channels.
            let times: Vec<f32> = (0..frames.len()).map(|f| f as f32 * dt).collect();
            let mut tb = Vec::with_capacity(times.len() * 4);
            for t in &times {
                tb.extend_from_slice(&t.to_le_bytes());
            }
            let t_view = add_view(&mut buf, &mut views, &tb, None);
            let tmax = times.last().copied().unwrap_or(0.0);
            let t_acc = add_accessor(
                &mut accessors, t_view, 5126, times.len(), "SCALAR",
                Some((serde_json::json!([0.0f32]), serde_json::json!([tmax]))),
            );
            // Translation output (rotated into Y-up space). The root bone (index 0)
            // carries the normalization offset in EQ-native space, applied before
            // the Y-up rotation — matching the root rest translation above. Clips
            // with no root translation channel keep the offset rest value instead.
            let mut trb = Vec::with_capacity(frames.len() * 12);
            for (t, _) in frames {
                let tt = rq * (if bone == 0 { *t + offset } else { *t });
                for c in [tt.x, tt.y, tt.z] {
                    trb.extend_from_slice(&c.to_le_bytes());
                }
            }
            let tr_view = add_view(&mut buf, &mut views, &trb, None);
            let tr_acc = add_accessor(&mut accessors, tr_view, 5126, frames.len(), "VEC3", None);
            // Rotation output (conjugated into Y-up space: r' = rq*r*rq^-1).
            let mut rb = Vec::with_capacity(frames.len() * 16);
            for (_, r) in frames {
                let rr = (rq * *r * rq_conj).normalize();
                for c in [rr.x, rr.y, rr.z, rr.w] {
                    rb.extend_from_slice(&c.to_le_bytes());
                }
            }
            let r_view = add_view(&mut buf, &mut views, &rb, None);
            let r_acc = add_accessor(&mut accessors, r_view, 5126, frames.len(), "VEC4", None);

            let s0 = samplers.len();
            samplers.push(serde_json::json!({ "input": t_acc, "output": tr_acc, "interpolation": "LINEAR" }));
            samplers.push(serde_json::json!({ "input": t_acc, "output": r_acc, "interpolation": "LINEAR" }));
            channels.push(serde_json::json!({ "sampler": s0, "target": { "node": bone, "path": "translation" } }));
            channels.push(serde_json::json!({ "sampler": s0 + 1, "target": { "node": bone, "path": "rotation" } }));
        }
        if !channels.is_empty() {
            // Name = "<code>_<semantic>" (e.g. "L01A_walk", "P01A_idle_neutral")
            // so the renderer's substring clip lookup resolves idle/walk/run.
            let name = match anim_label(&anim.code[..3]) {
                Some(label) => format!("{}_{}", anim.code, label),
                None => anim.code.clone(),
            };
            gltf_anims.push(serde_json::json!({ "name": name, "channels": channels, "samplers": samplers }));
        }
    }

    align4(&mut buf);
    let mut gltf = serde_json::json!({
        "asset": { "version": "2.0", "generator": "s3d_to_gltf (skinned)" },
        "scene": 0,
        "scenes": [{ "nodes": [0usize, mesh_idx] }],
        "nodes": nodes,
        "meshes": [{ "name": "combined", "primitives": gltf_prims }],
        "skins": [skin],
        "accessors": accessors,
        "bufferViews": views,
        "buffers": [{ "byteLength": buf.len() }],
        "materials": gltf_materials,
        "images": images,
        "textures": gltf_textures,
    });
    if !gltf_anims.is_empty() {
        gltf["animations"] = serde_json::json!(gltf_anims);
    }

    let json_str = serde_json::to_string(&gltf)?;
    let json_bytes = json_str.as_bytes();
    let json_padded = (json_bytes.len() + 3) & !3;
    let total = 12 + 8 + json_padded + 8 + buf.len();

    let mut out = fs::File::create(output)
        .with_context(|| format!("failed to create {}", output.display()))?;
    out.write_all(&0x46546C67u32.to_le_bytes())?;
    out.write_all(&2u32.to_le_bytes())?;
    out.write_all(&(total as u32).to_le_bytes())?;
    out.write_all(&(json_padded as u32).to_le_bytes())?;
    out.write_all(&0x4E4F534Au32.to_le_bytes())?;
    out.write_all(json_bytes)?;
    for _ in json_bytes.len()..json_padded {
        out.write_all(b" ")?;
    }
    out.write_all(&(buf.len() as u32).to_le_bytes())?;
    out.write_all(&0x004E4942u32.to_le_bytes())?;
    out.write_all(&buf)?;
    eprintln!("  wrote {} bytes to {}", total, output.display());
    Ok(())
}

/// List every model (HierarchicalSpriteDef = skeleton) in an archive, its 3-char
/// code, dag count, and whether its body mesh is new-format (skin_assignment_groups)
/// or old-format (rigid per-bone meshes referenced by dags).
fn list_models(input: &Path) -> Result<()> {
    let file = fs::File::open(input)?;
    let mut pfs = libeq_pfs::PfsReader::open(file)?;
    let filenames = pfs.filenames()?;
    for wld_name in filenames.iter().filter(|f| f.to_lowercase().ends_with(".wld")) {
        let bytes = match pfs.get(wld_name) {
            Ok(Some(b)) => b,
            _ => continue,
        };
        let doc = match WldDoc::parse(&bytes) {
            Ok(d) => d,
            Err(_) => continue,
        };
        // Index DmSpriteDef2 by name for quick prefix lookup.
        let meshes: Vec<(&str, &DmSpriteDef2)> = doc
            .fragment_iter::<DmSpriteDef2>()
            .map(|m| (doc.get_string(m.name_reference).unwrap_or(""), m))
            .collect();
        let skels: Vec<_> = doc.fragment_iter::<HierarchicalSpriteDef>().collect();
        println!("=== {} : {} skeletons, {} meshes ===", wld_name, skels.len(), meshes.len());
        for skel in &skels {
            let name = doc.get_string(skel.name_reference).unwrap_or("?");
            let code = name.split('_').next().unwrap_or(name);
            // Find body mesh for this code.
            let body = meshes.iter().find(|(mn, _)| {
                mn.starts_with(code) && !mn.contains("HE") && mn.ends_with("_DMSPRITEDEF")
            });
            let (mesh_kind, verts) = match body {
                Some((_, m)) if !m.skin_assignment_groups.is_empty() =>
                    ("skinned", m.positions.len()),
                Some((_, m)) => ("rigid/old", m.positions.len()),
                None => ("no-body-mesh", 0),
            };
            // Count rigidly-attached meshes (dag.mesh_or_sprite_reference != 0).
            let attached = skel.dags.iter().filter(|d| d.mesh_or_sprite_reference != 0).count();
            println!(
                "  {:<14} code={:<4} dags={:<3} {:<14} body_verts={} dag_meshes={}",
                name, code, skel.dags.len(), mesh_kind, verts, attached
            );
        }
    }
    Ok(())
}

/// Inspect the skeleton and animation track naming inside a character archive.
fn analyze_anims(input: &Path) -> Result<()> {
    use std::collections::BTreeMap;
    let file = fs::File::open(input)?;
    let mut pfs = libeq_pfs::PfsReader::open(file)?;
    let filenames = pfs.filenames()?;
    for wld_name in filenames.iter().filter(|f| f.to_lowercase().ends_with(".wld")) {
        let bytes = match pfs.get(wld_name) {
            Ok(Some(b)) => b,
            _ => continue,
        };
        let doc = match WldDoc::parse(&bytes) {
            Ok(d) => d,
            Err(_) => continue,
        };
        println!("=== {} ===", wld_name);
        // Skeleton + base track names per dag.
        if let Some(skel) = doc.fragment_iter::<HierarchicalSpriteDef>().next() {
            println!("HierarchicalSpriteDef: {} dags", skel.dags.len());
            let mut base_names = Vec::new();
            for (i, dag) in skel.dags.iter().enumerate() {
                let track = if dag.track_reference > 0 {
                    doc.at((dag.track_reference - 1) as usize)
                        .and_then(|f| f.as_any().downcast_ref::<Track>())
                } else {
                    None
                };
                let tname = track
                    .and_then(|t| doc.get_string(t.name_reference))
                    .unwrap_or("?");
                if i < 6 {
                    println!("  dag[{}] base_track='{}'", i, tname);
                }
                base_names.push(tname.to_string());
            }
        }
        // Group ALL track names by leading 3-char animation code (Cxx/Lxx/etc.).
        let mut by_prefix: BTreeMap<String, usize> = BTreeMap::new();
        let mut total = 0;
        for t in doc.fragment_iter::<Track>() {
            let name = doc.get_string(t.name_reference).unwrap_or("");
            total += 1;
            let prefix = name.chars().take(3).collect::<String>();
            *by_prefix.entry(prefix).or_insert(0) += 1;
        }
        println!("Total Track(0x13) fragments: {}", total);
        println!("Track-name prefixes (3-char) -> count:");
        for (p, c) in &by_prefix {
            println!("  '{}' x{}", p, c);
        }
        // Sample some full track names.
        println!("Sample track names:");
        for t in doc.fragment_iter::<Track>().take(10) {
            println!("  {}", doc.get_string(t.name_reference).unwrap_or("?"));
        }
        // All track names matching an optional filter substring.
        if let Ok(filter) = std::env::var("TRACK_FILTER") {
            println!("Tracks containing '{}':", filter);
            for t in doc.fragment_iter::<Track>() {
                let nm = doc.get_string(t.name_reference).unwrap_or("?");
                if nm.contains(&filter) {
                    println!("  {}", nm);
                }
            }
        }
    }
    Ok(())
}

fn write_glb(
    output: &Path,
    meshes: &[MeshData],
    materials: &[MaterialData],
    textures: &[TextureData],
    collision_indices: &[u32],
) -> Result<()> {
    let mut buffer_data: Vec<u8> = Vec::new();
    let mut buffer_views: Vec<serde_json::Value> = Vec::new();
    let mut accessors: Vec<serde_json::Value> = Vec::new();
    let mut images: Vec<serde_json::Value> = Vec::new();
    let mut gltf_textures: Vec<serde_json::Value> = Vec::new();
    let mut gltf_materials: Vec<serde_json::Value> = Vec::new();
    let mut gltf_meshes: Vec<serde_json::Value> = Vec::new();
    let mut nodes: Vec<serde_json::Value> = Vec::new();
    // POSITION accessor of the (single) combined render mesh; the `__collision__` mesh shares it.
    let mut combined_pos_acc: Option<usize> = None;

    // Add textures as images
    for tex in textures {
        let view_idx = buffer_views.len();
        let byte_offset = buffer_data.len() as u32;
        buffer_data.extend_from_slice(&tex.png_bytes);
        while buffer_data.len() % 4 != 0 {
            buffer_data.push(0);
        }
        buffer_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": byte_offset,
            "byteLength": tex.png_bytes.len(),
        }));
        images.push(serde_json::json!({
            "bufferView": view_idx,
            "mimeType": "image/png",
            "name": tex.name,
        }));
        gltf_textures.push(serde_json::json!({
            "source": images.len() - 1,
        }));
    }

    // Add materials
    for mat in materials {
        let pbr = if let Some(tex_idx) = mat.texture_idx {
            serde_json::json!({
                "baseColorTexture": { "index": tex_idx },
                "metallicFactor": 0.0,
                "roughnessFactor": 1.0,
            })
        } else {
            serde_json::json!({
                "baseColorFactor": mat.base_color,
                "metallicFactor": 0.0,
                "roughnessFactor": 1.0,
            })
        };
        gltf_materials.push(serde_json::json!({
            "name": mat.name,
            "pbrMetallicRoughness": pbr,
            "doubleSided": true,
        }));
    }

    // Add meshes — each MeshData becomes one glTF mesh with shared vertices
    // and multiple primitives (one per material group).
    for mesh in meshes {
        let mut attributes = serde_json::Map::new();

        // Positions (shared across all primitives)
        let pos_offset = buffer_data.len() as u32;
        for p in &mesh.positions {
            buffer_data.extend_from_slice(&p[0].to_le_bytes());
            buffer_data.extend_from_slice(&p[1].to_le_bytes());
            buffer_data.extend_from_slice(&p[2].to_le_bytes());
        }
        let pos_byte_len = (mesh.positions.len() * 12) as u32;
        let pos_view_idx = buffer_views.len();
        buffer_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": pos_offset,
            "byteLength": pos_byte_len,
            "target": 34962,
        }));
        let (pos_min, pos_max) = compute_bounds_f32x3(&mesh.positions);
        let pos_acc_idx = accessors.len();
        accessors.push(serde_json::json!({
            "bufferView": pos_view_idx,
            "componentType": 5126,
            "count": mesh.positions.len(),
            "type": "VEC3",
            "min": pos_min,
            "max": pos_max,
        }));
        attributes.insert("POSITION".to_string(), serde_json::json!(pos_acc_idx));
        combined_pos_acc = Some(pos_acc_idx);

        // Normals (shared across all primitives)
        let norm_offset = buffer_data.len() as u32;
        for n in &mesh.normals {
            buffer_data.extend_from_slice(&n[0].to_le_bytes());
            buffer_data.extend_from_slice(&n[1].to_le_bytes());
            buffer_data.extend_from_slice(&n[2].to_le_bytes());
        }
        let norm_byte_len = (mesh.normals.len() * 12) as u32;
        let norm_view_idx = buffer_views.len();
        buffer_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": norm_offset,
            "byteLength": norm_byte_len,
            "target": 34962,
        }));
        let norm_acc_idx = accessors.len();
        accessors.push(serde_json::json!({
            "bufferView": norm_view_idx,
            "componentType": 5126,
            "count": mesh.normals.len(),
            "type": "VEC3",
        }));
        attributes.insert("NORMAL".to_string(), serde_json::json!(norm_acc_idx));

        // UVs (shared across all primitives)
        let uv_offset = buffer_data.len() as u32;
        for u in &mesh.uvs {
            buffer_data.extend_from_slice(&u[0].to_le_bytes());
            buffer_data.extend_from_slice(&u[1].to_le_bytes());
        }
        let uv_byte_len = (mesh.uvs.len() * 8) as u32;
        let uv_view_idx = buffer_views.len();
        buffer_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": uv_offset,
            "byteLength": uv_byte_len,
            "target": 34962,
        }));
        let uv_acc_idx = accessors.len();
        accessors.push(serde_json::json!({
            "bufferView": uv_view_idx,
            "componentType": 5126,
            "count": mesh.uvs.len(),
            "type": "VEC2",
        }));
        attributes.insert("TEXCOORD_0".to_string(), serde_json::json!(uv_acc_idx));

        // One glTF primitive per material group, each with its own index buffer.
        let mut gltf_primitives = Vec::new();
        for prim in &mesh.primitives {
            let idx_offset = buffer_data.len() as u32;
            for &i in &prim.indices {
                buffer_data.extend_from_slice(&(i as u16).to_le_bytes());
            }
            while buffer_data.len() % 4 != 0 {
                buffer_data.push(0);
            }
            let idx_view_idx = buffer_views.len();
            buffer_views.push(serde_json::json!({
                "buffer": 0,
                "byteOffset": idx_offset,
                "byteLength": prim.indices.len() * 2,
                "target": 34963,
            }));
            let idx_acc_idx = accessors.len();
            accessors.push(serde_json::json!({
                "bufferView": idx_view_idx,
                "componentType": 5123,
                "count": prim.indices.len(),
                "type": "SCALAR",
            }));

            gltf_primitives.push(serde_json::json!({
                "attributes": attributes,
                "indices": idx_acc_idx,
                "material": prim.material_idx,
            }));
        }

        gltf_meshes.push(serde_json::json!({
            "name": mesh.name,
            "primitives": gltf_primitives,
        }));

        let node_idx = nodes.len();
        nodes.push(serde_json::json!({
            "mesh": node_idx,
        }));
    }

    // Dedicated collision mesh: a `__collision__` mesh whose single primitive shares the render
    // mesh's POSITION accessor and carries only the SOLID-face indices (incl. invisible-but-solid
    // faces; PASSABLE faces excluded). The client tags it and routes it to collision instead of
    // rendering it. u32 indices (5125) so large terrain collision sets never wrap past 65 535.
    if let (Some(pos_acc), false) = (combined_pos_acc, collision_indices.is_empty()) {
        let idx_offset = buffer_data.len() as u32;
        for &i in collision_indices {
            buffer_data.extend_from_slice(&i.to_le_bytes());
        }
        while buffer_data.len() % 4 != 0 {
            buffer_data.push(0);
        }
        let idx_view_idx = buffer_views.len();
        buffer_views.push(serde_json::json!({
            "buffer": 0,
            "byteOffset": idx_offset,
            "byteLength": collision_indices.len() * 4,
            "target": 34963,
        }));
        let idx_acc_idx = accessors.len();
        accessors.push(serde_json::json!({
            "bufferView": idx_view_idx,
            "componentType": 5125,
            "count": collision_indices.len(),
            "type": "SCALAR",
        }));
        let mesh_idx = gltf_meshes.len();
        gltf_meshes.push(serde_json::json!({
            "name": "__collision__",
            "primitives": [{
                "attributes": { "POSITION": pos_acc },
                "indices": idx_acc_idx,
            }],
        }));
        nodes.push(serde_json::json!({ "name": "__collision__", "mesh": mesh_idx }));
    }

    // Pad buffer to 4 bytes
    while buffer_data.len() % 4 != 0 {
        buffer_data.push(0);
    }

    let gltf = serde_json::json!({
        "asset": {
            "version": "2.0",
            "generator": "s3d_to_gltf",
        },
        "scene": 0,
        "scenes": [{
            "name": "scene",
            "nodes": (0..nodes.len()).collect::<Vec<_>>(),
        }],
        "nodes": nodes,
        "meshes": gltf_meshes,
        "accessors": accessors,
        "bufferViews": buffer_views,
        "buffers": [{
            "byteLength": buffer_data.len(),
        }],
        "materials": gltf_materials,
        "images": images,
        "textures": gltf_textures,
    });

    let json_str = serde_json::to_string(&gltf)?;
    let json_bytes = json_str.as_bytes();
    let json_padded_len = (json_bytes.len() + 3) & !3;

    let bin_padded_len = buffer_data.len();
    let total_len = 12 + 8 + json_padded_len + 8 + bin_padded_len;

    let mut out = fs::File::create(output)
        .with_context(|| format!("failed to create {}", output.display()))?;

    // GLB header
    out.write_all(&0x46546C67u32.to_le_bytes())?;
    out.write_all(&2u32.to_le_bytes())?;
    out.write_all(&(total_len as u32).to_le_bytes())?;

    // JSON chunk
    out.write_all(&(json_padded_len as u32).to_le_bytes())?;
    out.write_all(&0x4E4F534Au32.to_le_bytes())?;
    out.write_all(json_bytes)?;
    for _ in json_bytes.len()..json_padded_len {
        out.write_all(b" ")?;
    }

    // Binary chunk
    out.write_all(&(bin_padded_len as u32).to_le_bytes())?;
    out.write_all(&0x004E4942u32.to_le_bytes())?;
    out.write_all(&buffer_data)?;

    eprintln!("  wrote {} bytes to {}", total_len, output.display());
    Ok(())
}

fn compute_bounds_f32x3(positions: &[[f32; 3]]) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::MAX; 3];
    let mut max = [f32::MIN; 3];
    for p in positions {
        for i in 0..3 {
            if p[i] < min[i] { min[i] = p[i]; }
            if p[i] > max[i] { max[i] = p[i]; }
        }
    }
    (min, max)
}

/// The collision classifier for a WLD face flag, mirroring libeq `Mesh::collision_indices()`:
/// a face is SOLID (collidable) when flag bit 0x0010 is CLEAR. Bit 0x0010 marks PASSABLE faces
/// (water surfaces, foliage — "player can pass through", per libeq `DmSpriteDef2FaceEntry`).
/// Visibility is a *material* property, not a face flag: an invisible-but-solid face (zone
/// boundary, invisible wall) still has 0x0010 clear and is therefore included in collision.
#[cfg(test)]
fn face_is_solid(flags: u16) -> bool {
    flags & 0x0010 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec: one visible-solid + one INVIS-solid + one PASSABLE face must yield a collision set
    /// of exactly the two SOLID faces. The INVIS face is visually distinct (different material)
    /// but collides identically; only the PASSABLE (0x0010) face is excluded.
    #[test]
    fn collision_set_is_the_two_solid_faces() {
        // (flag, label) — render-distinct but collision is purely flag-driven.
        let faces: [(u16, &str); 3] = [
            (0x0000, "visible-solid"),
            (0x0000, "invis-solid"),   // invisible material, but face flag still solid
            (0x0010, "passable"),      // water/foliage — excluded from collision
        ];
        let solid: Vec<&str> = faces
            .iter()
            .filter(|(f, _)| face_is_solid(*f))
            .map(|(_, l)| *l)
            .collect();
        assert_eq!(solid, vec!["visible-solid", "invis-solid"]);
        assert_eq!(solid.len(), 2, "exactly the two SOLID faces collide");
        assert!(!face_is_solid(0x0010), "PASSABLE faces never collide");
        // Other unrelated flag bits do not make a face passable.
        assert!(face_is_solid(0x0001), "non-passable flag bits stay solid");
    }

    #[test]
    #[ignore = "requires extracted assets (set EQ_ASSETS_DIR): globalhum_chr.s3d"]
    fn skinned_conversion_is_centered_grounded() {
        let Some(dir) = std::env::var_os("EQ_ASSETS_DIR") else { return; };
        let inp = std::path::PathBuf::from(dir).join("globalhum_chr.s3d");
        let out = std::path::PathBuf::from("/tmp/test_hum_norm.glb");
        convert_s3d_to_glb_skinned(&inp, &out, None).unwrap();

        // Re-parse with the gltf crate and confirm the root node carries a positive
        // eq_height in its extras.
        let (doc, _buffers, _images) = gltf::import(&out).unwrap();
        let root = doc.nodes().next().expect("at least one node");
        let extras = root.extras().as_ref().expect("root node extras present");
        let v: serde_json::Value = serde_json::from_str(extras.get()).unwrap();
        let eq_height = v["eq_height"].as_f64().expect("eq_height field present");
        assert!(eq_height > 0.0, "eq_height should be > 0, got {eq_height}");
    }
}
