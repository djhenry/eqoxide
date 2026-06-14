use std::path::Path;

pub struct ZoneMapLine {
    pub east1:  f32,
    pub north1: f32,
    pub east2:  f32,
    pub north2: f32,
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub struct ZoneMapLabel {
    pub east:  f32,
    pub north: f32,
    pub text:  String,
}

pub struct ZoneMap {
    pub lines:  Vec<ZoneMapLine>,
    pub labels: Vec<ZoneMapLabel>,
}

impl ZoneMap {
    /// Load map lines from an EQ map text file.
    /// EQ map coordinate convention: first value = server_x (north), second = server_y (east).
    pub fn load(maps_dir: &Path, zone_name: &str) -> Option<Self> {
        let path = maps_dir.join(format!("{}.txt", zone_name));
        let text = std::fs::read_to_string(&path)
            .map_err(|e| eprintln!("zone_map: failed to load {:?}: {}", path, e))
            .ok()?;

        let mut lines  = Vec::new();
        let mut labels = Vec::new();

        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('L') {
                // L x1, y1, z1,  x2, y2, z2,  r, g, b
                let nums: Vec<f32> = line[1..].split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                if nums.len() >= 9 {
                    lines.push(ZoneMapLine {
                        // EQ map file: L map_x, map_y, z, map_x2, map_y2, z2, r, g, b
                        // Verified against live entity positions:
                        //   entity_x = -map_x  →  minimap north = e.x = -map_x = -nums[0]
                        //   entity_y = -map_y  →  minimap east  = e.y = -map_y = -nums[1]
                        east1:  -nums[1], north1: -nums[0],
                        east2:  -nums[4], north2: -nums[3],
                        r: nums[6] as u8, g: nums[7] as u8, b: nums[8] as u8,
                    });
                }
            } else if line.starts_with('P') {
                // P x, y, z,  r, g, b,  size,  label
                let rest = &line[1..];
                if let Some(label_start) = rest.rfind(',') {
                    let text = rest[label_start + 1..].trim().replace('_', " ").to_string();
                    let nums: Vec<f32> = rest[..label_start].split(',')
                        .filter_map(|s| s.trim().parse().ok())
                        .collect();
                    if nums.len() >= 2 {
                        labels.push(ZoneMapLabel {
                            east:  -nums[1],
                            north: -nums[0],
                            text,
                        });
                    }
                }
            }
        }

        eprintln!("zone_map: loaded {} lines, {} labels for '{}'", lines.len(), labels.len(), zone_name);
        Some(ZoneMap { lines, labels })
    }
}
