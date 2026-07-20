//! Parses EQ `.map` files (line segments + labels — the in-game map overlay) for a zone, used to
//! draw the HUD minimap and to convert map coordinates for name/coordinate `/goto`.

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
    /// missing.
    ///
    /// EQ map .txt files (eqmaps/Brewall format) store coordinates as the **negated** server
    /// position: the file's (x, y) is (−server_x, −server_y). `parse_into` negates both back to
    /// true server space so the line art and labels share one coordinate system with entity dots
    /// and the player marker (both drawn from real server coords). Verified against everfrost
    /// landmarks vs the DB, e.g. to_Blackburrow file (525, 3054) → (−525, −3054) ≈ DB (−530, −3061).
    /// (eqoxide#206)
    pub fn load(maps_dir: &Path, zone_name: &str) -> Option<Self> {
        let base = maps_dir.join(format!("{}.txt", zone_name));
        let text = std::fs::read_to_string(&base)
            .map_err(|e| tracing::warn!("zone_map: failed to load {:?}: {}", base, e))
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

        tracing::info!("zone_map: loaded {} lines, {} labels for '{}' (base + layers)",
                  lines.len(), labels.len(), zone_name);
        Some(ZoneMap { lines, labels })
    }

    /// Parse one map file's `L` (line) and `P` (point/label) records into the given
    /// vectors. File coords are the negated server position, so both x and y are negated
    /// here to yield true server space (east, north) = (server_x, server_y). (eqoxide#206)
    fn parse_into(text: &str, lines: &mut Vec<ZoneMapLine>, labels: &mut Vec<ZoneMapLabel>) {
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('L') {
                // L x1, y1, z1, x2, y2, z2, r, g, b — file (x, y) = (−server_x, −server_y); negate.
                let nums: Vec<f32> = line[1..].split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                if nums.len() >= 9 {
                    lines.push(ZoneMapLine {
                        east1:  -nums[0], north1:  -nums[1],
                        east2:  -nums[3], north2:  -nums[4],
                        r: nums[6] as u8, g: nums[7] as u8, b: nums[8] as u8,
                    });
                }
            } else if line.starts_with('P') {
                // P x, y, z, r, g, b, size, label — file (x, y) = (−server_x, −server_y); negate.
                let rest = &line[1..];
                if let Some(label_start) = rest.rfind(',') {
                    let text = rest[label_start + 1..].trim().replace('_', " ").to_string();
                    let nums: Vec<f32> = rest[..label_start].split(',')
                        .filter_map(|s| s.trim().parse().ok())
                        .collect();
                    if nums.len() >= 2 {
                        labels.push(ZoneMapLabel {
                            east:  -nums[0],
                            north: -nums[1],
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
        // File (x, y) = (−server_x, −server_y); parser negates both to true server space.
        assert_eq!((l.east1, l.north1), (-10.0, -20.0));
        assert_eq!((l.east2, l.north2), (-30.0, -40.0));
        assert_eq!((l.r, l.g, l.b), (255, 128, 0));

        assert_eq!(labels.len(), 1);
        let p = &labels[0];
        assert_eq!((p.east, p.north), (-100.0, -200.0));
        assert_eq!(p.text, "North Gate"); // underscores → spaces

        // Layers append rather than replace.
        ZoneMap::parse_into("L 1,2,0,3,4,0,1,1,1", &mut lines, &mut labels);
        assert_eq!(lines.len(), 2);
    }

    /// Regression for eqoxide#206: parsed label coords must land on the DB/server position of
    /// the landmark (so map art aligns with entity dots), not its negation. Landmarks and DB
    /// values are the everfrost zone-line marks measured in the issue.
    #[test]
    fn labels_land_on_server_coords_everfrost_landmarks() {
        // (label text, file x, file y, expected server_x, expected server_y)
        let cases = [
            ("to_Blackburrow", 525.0, 3054.0, -530.0, -3061.0),
            ("to_Permafrost", 7077.0, -2018.0, -7048.0, 2020.0),
            ("Succor", -629.0, -3139.0, 629.0, 3139.0),
            ("to_Halas", -383.0, -3681.0, 370.0, 3700.0),
        ];
        for (name, fx, fy, sx, sy) in cases {
            let text = format!("P {fx}, {fy}, 0, 0, 0, 0, 3, {name}");
            let mut lines = Vec::new();
            let mut labels = Vec::new();
            ZoneMap::parse_into(&text, &mut lines, &mut labels);
            assert_eq!(labels.len(), 1, "{name}: parsed a label");
            let p = &labels[0];
            // The parser must emit the exact negation of the file value…
            assert_eq!((p.east, p.north), (-fx, -fy), "{name}: parser must negate file coords");
            // …which lands within survey-rounding distance of the true DB server position (the
            // hand-made map marks differ from DB by up to a few tens of units).
            assert!((p.east - sx).abs() < 40.0 && (p.north - sy).abs() < 40.0,
                "{name}: parsed ({:.0},{:.0}) should be ≈ server ({sx},{sy})", p.east, p.north);
        }
    }
}
