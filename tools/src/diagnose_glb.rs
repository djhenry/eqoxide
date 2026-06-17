use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        eprintln!("Usage: diagnose_glb <file.glb>");
        eprintln!("  Comprehensive geometry analysis of a glTF binary file.");
        eprintln!("  Computes per-primitive bounds using only indexed vertices.");
        std::process::exit(0);
    }
    let path = PathBuf::from(&args[0]);

    let (doc, buffers, _images) = match gltf::import(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("FAILED to load {}: {}", path.display(), e);
            std::process::exit(1);
        }
    };

    println!("=== GLB Geometry Diagnosis: {} ===", path.display());
    println!("Meshes: {}", doc.meshes().count());
    println!("Materials: {}", doc.materials().count());
    println!();

    // Global bounds (union of all mesh bounds)
    let (mut gx0, mut gx1) = (f32::MAX, f32::MIN);
    let (mut gy0, mut gy1) = (f32::MAX, f32::MIN);
    let (mut gz0, mut gz1) = (f32::MAX, f32::MIN);
    let mut total_verts = 0usize;
    let mut total_indices = 0usize;
    let mut total_prims = 0usize;

    for mesh in doc.meshes() {
        println!("--- Mesh '{}' (index {}) ---", mesh.name().unwrap_or("?"), mesh.index());

        // Mesh-level bounds (union of all prims in this mesh)
        let (mut mx0, mut mx1) = (f32::MAX, f32::MIN);
        let (mut my0, mut my1) = (f32::MAX, f32::MIN);
        let (mut mz0, mut mz1) = (f32::MAX, f32::MIN);
        let mut mesh_verts = 0usize;

        for prim in mesh.primitives() {
            let reader = prim.reader(|buf| Some(&buffers[buf.index()]));

            // Read ALL positions from the shared accessor
            let all_positions: Vec<[f32; 3]> = match reader.read_positions() {
                Some(p) => p.collect(),
                None => {
                    println!("  prim {}: no positions!", prim.index());
                    continue;
                }
            };

            // Read indices
            let indices: Vec<u32> = match reader.read_indices() {
                Some(idx) => idx.into_u32().collect(),
                None => vec![],
            };

            // Read normals
            let all_normals: Vec<[f32; 3]> = reader.read_normals()
                .map(|n| n.collect())
                .unwrap_or_default();

            // Read UVs
            let all_uvs: Vec<[f32; 2]> = reader.read_tex_coords(0)
                .map(|tc| tc.into_f32().collect())
                .unwrap_or_default();

            // Compute bounds using ONLY the indexed vertices
            let (mut px0, mut px1) = (f32::MAX, f32::MIN);
            let (mut py0, mut py1) = (f32::MAX, f32::MIN);
            let (mut pz0, mut pz1) = (f32::MAX, f32::MIN);
            let mut prim_vert_count = 0usize;

            if indices.is_empty() {
                // No indices: use all positions
                for p in &all_positions {
                    px0 = px0.min(p[0]); px1 = px1.max(p[0]);
                    py0 = py0.min(p[1]); py1 = py1.max(p[1]);
                    pz0 = pz0.min(p[2]); pz1 = pz1.max(p[2]);
                }
                prim_vert_count = all_positions.len();
            } else {
                // Use only indexed vertices
                for &idx in &indices {
                    let i = idx as usize;
                    if i < all_positions.len() {
                        let p = &all_positions[i];
                        px0 = px0.min(p[0]); px1 = px1.max(p[0]);
                        py0 = py0.min(p[1]); py1 = py1.max(p[1]);
                        pz0 = pz0.min(p[2]); pz1 = pz1.max(p[2]);
                        prim_vert_count += 1;
                    }
                }
            }

            let mat_name = prim.material().name().unwrap_or("?");
            let pbr = prim.material().pbr_metallic_roughness();
            let tex_name = pbr.base_color_texture()
                .map(|t| t.texture().source().name().unwrap_or("?"))
                .unwrap_or("(none)");

            let px_ext = if px0 < px1 { px1 - px0 } else { 0.0 };
            let py_ext = if py0 < py1 { py1 - py0 } else { 0.0 };
            let pz_ext = if pz0 < pz1 { pz1 - pz0 } else { 0.0 };

            println!("  prim {}: {} indexed verts, {} indices, material='{}', tex='{}'",
                prim.index(), prim_vert_count, indices.len(), mat_name, tex_name);
            println!("    bounds: X=[{:.4}, {:.4}] Y=[{:.4}, {:.4}] Z=[{:.4}, {:.4}]",
                px0, px1, py0, py1, pz0, pz1);
            println!("    extent: {:.4} x {:.4} x {:.4}", px_ext, py_ext, pz_ext);

            // Update mesh bounds
            if px0 < mx0 { mx0 = px0; } if px1 > mx1 { mx1 = px1; }
            if py0 < my0 { my0 = py0; } if py1 > my1 { my1 = py1; }
            if pz0 < mz0 { mz0 = pz0; } if pz1 > mz1 { mz1 = pz1; }
            mesh_verts += prim_vert_count;
            total_indices += indices.len();
            total_prims += 1;

            // Check for degenerate triangles
            if indices.len() >= 3 {
                let mut degenerate = 0u32;
                for tri in indices.chunks(3) {
                    if tri.len() == 3 && (tri[0] == tri[1] || tri[1] == tri[2] || tri[0] == tri[2]) {
                        degenerate += 1;
                    }
                }
                if degenerate > 0 {
                    println!("    *** WARNING: {} degenerate triangles ***", degenerate);
                }
            }
        }

        total_verts += mesh_verts;

        // Mesh-level summary
        let mx_ext = if mx0 < mx1 { mx1 - mx0 } else { 0.0 };
        let my_ext = if my0 < my1 { my1 - my0 } else { 0.0 };
        let mz_ext = if mz0 < mz1 { mz1 - mz0 } else { 0.0 };
        println!("  MESH SUMMARY: {} verts, {} prims", mesh_verts, mesh.primitives().count());
        println!("  MESH BOUNDS: X=[{:.4}, {:.4}] Y=[{:.4}, {:.4}] Z=[{:.4}, {:.4}]",
            mx0, mx1, my0, my1, mz0, mz1);
        println!("  MESH EXTENT: {:.4} x {:.4} x {:.4}", mx_ext, my_ext, mz_ext);

        // Update global bounds
        if mx0 < gx0 { gx0 = mx0; } if mx1 > gx1 { gx1 = mx1; }
        if my0 < gy0 { gy0 = my0; } if my1 > gy1 { gy1 = my1; }
        if mz0 < gz0 { gz0 = mz0; } if mz1 > gz1 { gz1 = mz1; }

        println!();
    }

    // Global summary
    let gx_ext = if gx0 < gx1 { gx1 - gx0 } else { 0.0 };
    let gy_ext = if gy0 < gy1 { gy1 - gy0 } else { 0.0 };
    let gz_ext = if gz0 < gz1 { gz1 - gz0 } else { 0.0 };

    println!("=== GLOBAL SUMMARY ===");
    println!("Total vertices: {}", total_verts);
    println!("Total indices: {}", total_indices);
    println!("Total primitives: {}", total_prims);
    println!("GLOBAL BOUNDS: X=[{:.4}, {:.4}] Y=[{:.4}, {:.4}] Z=[{:.4}, {:.4}]",
        gx0, gx1, gy0, gy1, gz0, gz1);
    println!("GLOBAL EXTENT: {:.4} x {:.4} x {:.4}", gx_ext, gy_ext, gz_ext);

    // Sanity checks
    println!();
    println!("=== SANITY CHECKS ===");
    if total_verts == 0 {
        println!("FAIL: No vertices found!");
    } else {
        println!("OK: {} vertices found", total_verts);
    }
    if total_prims == 0 {
        println!("FAIL: No primitives found!");
    } else {
        println!("OK: {} primitives found", total_prims);
    }
    for (i, ext) in [gx_ext, gy_ext, gz_ext].iter().enumerate() {
        let axis = ['X', 'Y', 'Z'][i];
        if *ext < 0.001 {
            println!("FAIL: Extent along {} axis is near zero ({:.6}) — model is degenerate!", axis, ext);
        } else {
            println!("OK: {} axis extent = {:.4}", axis, ext);
        }
    }

    // Y should be the tallest axis for a standing humanoid
    if gy_ext > gx_ext && gy_ext > gz_ext {
        println!("OK: Y axis is tallest ({:.4}) — correct for Y-up humanoid", gy_ext);
    } else {
        println!("INFO: Y axis is NOT the tallest — X={:.4}, Y={:.4}, Z={:.4}", gx_ext, gy_ext, gz_ext);
        if gx_ext > gy_ext && gx_ext > gz_ext {
            println!("  X axis (width) is tallest — expected for T-pose characters (arms extended)");
        }
    }

    // Proportions check
    if gy_ext > 0.1 {
        let wh = gx_ext / gy_ext;
        let dh = gz_ext / gy_ext;
        println!("PROPORTIONS: W/H={:.3}, D/H={:.3}", wh, dh);
        if wh > 2.0 {
            println!("  WARNING: W/H > 2.0 — model is unusually wide");
        }
        if dh > 1.5 {
            println!("  WARNING: D/H > 1.5 — model is unusually deep");
        }
    }
}
