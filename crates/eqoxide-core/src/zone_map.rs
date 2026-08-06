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

// Concise on purpose, same reasoning as `region_map::RegionMap`'s `Debug`: a real zone map can
// carry thousands of line segments, and what a reader needs from a `{:?}` (e.g. `unwrap_err` on a
// `Result<ZoneMap, _>` in a test) is "there IS a map, and it read this much", not a full dump.
impl std::fmt::Debug for ZoneMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ZoneMap({} lines, {} labels)", self.lines.len(), self.labels.len())
    }
}

/// **Why a zone's map `.txt` is NOT available** — the fact the old lossy `ZoneMap::load`'s
/// `Option` threw away (eqoxide#816; that function is now deleted, mirroring #762/#803's
/// `RegionMap::load`).
///
/// A `None` from the old loader collapsed two different facts into one value: *there is no
/// `<zone>.txt`* and *the file is there but couldn't be read* (permission error, bad mount, a
/// directory in its place, invalid UTF-8). Both used to read, to `sync_zone_points`' caller, as
/// "this zone's map contributes no fallback exits" — indistinguishable from the equally common and
/// legitimate case of a map that loaded fine and simply has no `"to "` labels. See the module docs
/// on [`ZoneMap::try_load`] for where the failure goes now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZoneMapLoadError {
    /// No readable `<zone>.txt` at `<maps_dir>/<zone>.txt` — specifically, `std::fs::read_to_string`
    /// failed with `ErrorKind::NotFound`.
    Missing,
    /// `std::fs::read_to_string` failed for a reason OTHER than "not found" — a permission error, a
    /// corrupt mount, a directory sitting where the file should be, or invalid UTF-8. Collapsing
    /// this into `Missing` would report "no map for this zone" for a fact that isn't that at all.
    /// Carries the raw `io::ErrorKind` so the message names what actually happened.
    Unreadable(std::io::ErrorKind),
}

impl ZoneMapLoadError {
    /// The machine-readable `reason` an agent reads off `/v1/observe/debug`'s `zone_map_load`
    /// field. Distinct per cause on purpose, same rationale as
    /// [`RegionDataAbsent::as_str`](crate::region_map::RegionDataAbsent::as_str): "the file isn't
    /// there" and "the file exists but can't be read" call for different operator action.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Missing         => "zone_map_missing",
            Self::Unreadable(_)   => "zone_map_unreadable",
        }
    }
}

impl std::fmt::Display for ZoneMapLoadError {
    // Deliberately names no path: these strings land in logs, an HTTP response and PR bodies, and
    // the repo is public (the zone + the failure kind are the whole diagnostic anyway).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "no .txt map file for this zone"),
            Self::Unreadable(kind) =>
                write!(f, ".txt map file present but unreadable ({kind:?}), not confirmed absent"),
        }
    }
}

impl ZoneMap {
    /// Load an EQ map, **keeping the failure** ([`ZoneMapLoadError`]) instead of collapsing it to a
    /// bare absence (#816). This is the ONLY loader — the lossy `Option`-returning `load` this used
    /// to be is gone, so a caller can no longer answer off a map it never read without first
    /// writing the discard by hand.
    ///
    /// EQ map packs split a zone across `<zone>.txt` (base geometry) plus optional
    /// `<zone>_1/_2/_3.txt` detail layers — labels and POIs usually live in the layers, so all of
    /// them are merged here. **Only the base file's failure is carried**: the detail layers are
    /// documented as optional and stay silently skipped when absent, same as before #816 — a
    /// missing `_1.txt` is not evidence of anything, unlike a missing base file.
    ///
    /// EQ map .txt files (eqmaps/Brewall format) store coordinates as the **negated** server
    /// position: the file's (x, y) is (−server_x, −server_y). `parse_into` negates both back to
    /// true server space so the line art and labels share one coordinate system with entity dots
    /// and the player marker (both drawn from real server coords). Verified against everfrost
    /// landmarks vs the DB, e.g. to_Blackburrow file (525, 3054) → (−525, −3054) ≈ DB (−530, −3061).
    /// (eqoxide#206)
    pub fn try_load(maps_dir: &Path, zone_name: &str) -> Result<Self, ZoneMapLoadError> {
        let base = maps_dir.join(format!("{}.txt", zone_name));
        let text = std::fs::read_to_string(&base).map_err(|e| {
            let err = if e.kind() == std::io::ErrorKind::NotFound {
                ZoneMapLoadError::Missing
            } else {
                ZoneMapLoadError::Unreadable(e.kind())
            };
            tracing::warn!("zone_map: failed to load {:?}: {} ({err})", base, e);
            err
        })?;

        let mut lines  = Vec::new();
        let mut labels = Vec::new();
        Self::parse_into(&text, &mut lines, &mut labels);

        // Merge detail layers if present (silently skipped when absent — documented optional,
        // unlike the base file above).
        for suffix in ["_1", "_2", "_3"] {
            let layer = maps_dir.join(format!("{}{}.txt", zone_name, suffix));
            if let Ok(t) = std::fs::read_to_string(&layer) {
                Self::parse_into(&t, &mut lines, &mut labels);
            }
        }

        tracing::info!("zone_map: loaded {} lines, {} labels for '{}' (base + layers)",
                  lines.len(), labels.len(), zone_name);
        Ok(ZoneMap { lines, labels })
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

    /// **#816: a missing `.txt` and an unreadable-but-present `.txt` are DISTINCT values, and
    /// neither is "loaded with nothing in it".**
    ///
    /// The bug this pins: the old `load` answered `None` for both, and the one caller storing that
    /// `None` (`ActionLoop::sync_zone_points`) silently skipped adding fallback exits — the exact
    /// same shape as a zone whose map genuinely has zero `"to "` labels. `try_load` names the
    /// failure instead.
    ///
    /// Mutation check: make `try_load` return `Ok(ZoneMap { lines: vec![], labels: vec![] })` on
    /// either failure below (the moral equivalent of the old silent `None` reaching
    /// `sync_zone_points`) and the matching assertion goes RED.
    #[test]
    fn missing_and_unreadable_txt_are_distinct_named_values_816() {
        let dir = tempfile::tempdir().unwrap();

        // 1. No file at all.
        assert_eq!(ZoneMap::try_load(dir.path(), "absent").unwrap_err(), ZoneMapLoadError::Missing);

        // 2. A file that exists but can't be READ as the base map — reproduced without touching
        // real permissions: a DIRECTORY named `isadir.txt` makes `std::fs::read_to_string` fail
        // with an io error whose kind is NOT `NotFound`.
        std::fs::create_dir(dir.path().join("isadir.txt")).unwrap();
        match ZoneMap::try_load(dir.path(), "isadir").unwrap_err() {
            ZoneMapLoadError::Unreadable(kind) => assert_ne!(kind, std::io::ErrorKind::NotFound,
                "a directory that exists must not be reported as the NotFound kind"),
            other => panic!("a directory in place of the file must not read as {other:?} (Missing \
                would claim the map is confirmed absent, which is false here)"),
        }

        // …and a GOOD file loads, so the taxonomy is not just "everything fails".
        std::fs::write(dir.path().join("ok.txt"), "P 1.0, 2.0, 0, 0, 0, 0, 3, to_Somewhere").unwrap();
        let zm = ZoneMap::try_load(dir.path(), "ok").expect("a real .txt must load");
        assert_eq!(zm.labels.len(), 1);

        // Every failure prints its own sentence — a caller that logs it can always tell WHICH
        // failure happened without matching on the enum.
        assert_ne!(ZoneMapLoadError::Missing.to_string(),
                   ZoneMapLoadError::Unreadable(std::io::ErrorKind::PermissionDenied).to_string());
        // …and the machine-readable reason strings the HTTP surface publishes are likewise distinct.
        assert_ne!(ZoneMapLoadError::Missing.as_str(),
                   ZoneMapLoadError::Unreadable(std::io::ErrorKind::PermissionDenied).as_str());
    }

    /// #816: the OPTIONAL detail layers (`_1/_2/_3.txt`) must stay silently skipped when absent —
    /// only the base file's failure is carried. A base-only zone (the overwhelmingly common case)
    /// must load successfully with no detail-layer files present at all.
    #[test]
    fn missing_detail_layers_do_not_fail_the_load_816() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("basezone.txt"), "L 1,2,0,3,4,0,1,1,1").unwrap();
        // No basezone_1/_2/_3.txt written at all.
        let zm = ZoneMap::try_load(dir.path(), "basezone").expect("base-only zone must still load");
        assert_eq!(zm.lines.len(), 1);
    }
}
