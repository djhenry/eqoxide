use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("Usage: validate_glb <file.glb>");
        std::process::exit(1);
    }
    let path = PathBuf::from(&args[0]);
    match gltf::import(&path) {
        Ok((doc, buffers, images)) => {
            println!("OK: loaded {}", path.display());
            println!("  meshes: {}", doc.meshes().count());
            println!("  materials: {}", doc.materials().count());
            println!("  images: {}", images.len());
            println!("  buffers: {}", buffers.len());
            // Skin + animation summary.
            for skin in doc.skins() {
                println!("  skin: {} joints, inverse_bind={}",
                    skin.joints().count(),
                    skin.inverse_bind_matrices().is_some());
            }
            let anims: Vec<_> = doc.animations().collect();
            println!("  animations: {}", anims.len());
            for (i, anim) in anims.iter().enumerate().take(6) {
                let chans = anim.channels().count();
                let samps = anim.samplers().count();
                println!("    anim[{}] '{}': {} channels, {} samplers",
                    i, anim.name().unwrap_or("?"), chans, samps);
            }
            for (i, buf) in buffers.iter().enumerate() {
                println!("    buffer[{}]: {} bytes", i, buf.len());
            }
            for mesh in doc.meshes() {
                println!("  mesh '{}': {} primitives", 
                    mesh.name().unwrap_or("?"), mesh.primitives().count());
                for prim in mesh.primitives() {
                    let mut attrs: Vec<_> = prim.attributes().map(|a| format!("{:?}", a.0)).collect();
                    attrs.sort();
                    println!("    prim: attrs=[{}], indices={:?}, material={:?}",
                        attrs.join(", "),
                        prim.indices().is_some(),
                        prim.material().name());
                }
            }
        }
        Err(e) => {
            eprintln!("FAILED: {}", e);
            // Try to get more detail
            let data = std::fs::read(&path).expect("failed to read file");
            eprintln!("  file size: {} bytes", data.len());
            if data.len() >= 12 {
                let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                let version = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                let length = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
                eprintln!("  magic: {:#x}, version: {}, declared length: {}", magic, version, length);
            }
            std::process::exit(1);
        }
    }
}
