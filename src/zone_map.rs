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

#[allow(dead_code)]
pub struct ZoneMapLabel {
    pub east:  f32,
    pub north: f32,
    pub text:  String,
}

pub struct ZoneMap {
    pub lines:  Vec<ZoneMapLine>,
    #[allow(dead_code)]
    pub labels: Vec<ZoneMapLabel>,
}

impl ZoneMap {
    /// Load an EQ map. EQ map packs split a zone across `<zone>.txt` (base geometry) plus
    /// optional `<zone>_1/_2/_3.txt` detail layers — labels and POIs usually live in the
    /// layers, so all of them are merged here. Returns None only if the base file is
    /// missing. EQ map coords: first value = Y (north), second = X (west).
    pub fn load(maps_dir: &Path, zone_name: &str) -> Option<Self> {
        let base = maps_dir.join(format!("{}.txt", zone_name));
        let text = std::fs::read_to_string(&base)
            .map_err(|e| eprintln!("zone_map: failed to load {:?}: {}", base, e))
            .ok()?;

        let mut lines  = Vec::new();
        let mut labels = Vec::new();
        Self::parse_into(&text, &mut lines, &mut labels);

        // Merge detail layers if present (silently skipped when absent).
        for suffix in ["_1", "_2", "_3"] {
            let layer = maps_dir.join(format!("{}{}.txt", zone_name, suffix));
            if let Ok(t) = std::fs::read_to_string(&layer) {
                Self::parse_into(&t, &mut lines, &mut labels);
            }
        }

        eprintln!("zone_map: loaded {} lines, {} labels for '{}' (base + layers)",
                  lines.len(), labels.len(), zone_name);
        Some(ZoneMap { lines, labels })
    }

    /// Parse one map file's `L` (line) and `P` (point/label) records into the given
    /// vectors, applying the map→scene coordinate transform. Pure, for unit testing.
    fn parse_into(text: &str, lines: &mut Vec<ZoneMapLine>, labels: &mut Vec<ZoneMapLabel>) {
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('L') {
                // L y1, x1, z1,  y2, x2, z2,  r, g, b
                let nums: Vec<f32> = line[1..].split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                if nums.len() >= 9 {
                    lines.push(ZoneMapLine {
                        // EQ map file: L Y, X, Z, ... (matches /loc Y X Z display).
                        // Y = north-south (+Y = north) → geo_north = nums[0].
                        // X = east-west  (+X = west)  → geo_east  = -nums[1].
                        east1:  -nums[1], north1:  nums[0],
                        east2:  -nums[4], north2:  nums[3],
                        r: nums[6] as u8, g: nums[7] as u8, b: nums[8] as u8,
                    });
                }
            } else if line.starts_with('P') {
                // P y, x, z,  r, g, b,  size,  label
                let rest = &line[1..];
                if let Some(label_start) = rest.rfind(',') {
                    let text = rest[label_start + 1..].trim().replace('_', " ").to_string();
                    let nums: Vec<f32> = rest[..label_start].split(',')
                        .filter_map(|s| s.trim().parse().ok())
                        .collect();
                    if nums.len() >= 2 {
                        labels.push(ZoneMapLabel {
                            east:  -nums[1],
                            north:  nums[0],
                            text,
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_into_reads_lines_and_labels_with_transform() {
        let text = "\
L 10.0, 20.0, 0, 30.0, 40.0, 0, 255, 128, 0
P 100.0, 200.0, 0, 0, 0, 0, 3, North_Gate";
        let mut lines = Vec::new();
        let mut labels = Vec::new();
        ZoneMap::parse_into(text, &mut lines, &mut labels);

        assert_eq!(lines.len(), 1);
        let l = &lines[0];
        // EQ map L record: Y, X, Z, ... → east = X = nums[1], north = Y = nums[0]
        assert_eq!((l.east1, l.north1), (-20.0, 10.0));
        assert_eq!((l.east2, l.north2), (-40.0, 30.0));
        assert_eq!((l.r, l.g, l.b), (255, 128, 0));

        assert_eq!(labels.len(), 1);
        let p = &labels[0];
        assert_eq!((p.east, p.north), (-200.0, 100.0));
        assert_eq!(p.text, "North Gate"); // underscores → spaces

        // Layers append rather than replace.
        ZoneMap::parse_into("L 1,2,0,3,4,0,1,1,1", &mut lines, &mut labels);
        assert_eq!(lines.len(), 2);
    }
}
