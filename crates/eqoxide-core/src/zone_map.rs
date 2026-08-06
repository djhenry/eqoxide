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
    /// #816 round 2 (PR #869 review, B1/B2): the BASE file loaded fine, but one of the optional
    /// `<zone>_1/_2/_3.txt` detail layers exists and could not be read for a reason OTHER than "does
    /// not exist". Silently continuing with just the base content here would reproduce #816's exact
    /// silent-partial shape ONE LEVEL DOWN, inside the fix for #816: most zones carry their labels in
    /// the detail layers, not the base file — measured over the live client's real maps cache,
    /// `erudsxing` and `qeytoqrg` (2 of the only 5 zones anywhere in the pack whose map contributes
    /// a fallback zone point at all) have **100%** of their qualifying labels in `_1.txt` and **zero**
    /// in the base file, so an unreadable `_1.txt` for either would otherwise silently read as "this
    /// zone's map has no fallback exits" — false. Carries the layer suffix (`"_1"`/`"_2"`/`"_3"`) and
    /// the raw `io::ErrorKind`.
    LayerUnreadable(&'static str, std::io::ErrorKind),
}

impl ZoneMapLoadError {
    /// The machine-readable `reason` an agent reads off `/v1/observe/debug`'s `zone_map_load`
    /// field. Distinct per cause on purpose, same rationale as
    /// [`RegionDataAbsent::as_str`](crate::region_map::RegionDataAbsent::as_str): "the file isn't
    /// there" and "the file exists but can't be read" call for different operator action.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Missing            => "zone_map_missing",
            Self::Unreadable(_)      => "zone_map_unreadable",
            Self::LayerUnreadable(..) => "zone_map_layer_unreadable",
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
            Self::LayerUnreadable(suffix, kind) =>
                write!(f, "base .txt map loaded, but its {suffix}.txt detail layer is present and \
                    unreadable ({kind:?}) — this zone's fallback labels may be incomplete, not \
                    confirmed as this zone's full set"),
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
    /// them are merged here. **A MISSING layer is not evidence of anything** and stays silently
    /// skipped, same as before #816 — the overwhelming majority of zones simply have none. **A
    /// PRESENT-but-unreadable layer is a different fact and is NOT silently skipped** (#816 round
    /// 2/B2): treating it the same as "absent" would reproduce this issue's exact silent-partial
    /// shape one level down, since most zones keep their labels in the layers rather than the base
    /// file (see [`ZoneMapLoadError::LayerUnreadable`]) — such a layer turns the WHOLE load into
    /// `Err(LayerUnreadable)`, even though the base file itself read fine, because "some content
    /// plus an unknown gap" is not the same claim as "this zone's full, complete label set".
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

        // Merge detail layers if present. A MISSING layer (`NotFound`) is silently skipped —
        // documented optional, unlike the base file above. A PRESENT-but-unreadable layer is NOT
        // silently skipped (#816 round 2/B2): it fails the whole load, because we cannot otherwise
        // tell "this zone's map has no more labels" from "some of this zone's labels are stuck
        // behind an unreadable file" — see the doc comment above and `ZoneMapLoadError::LayerUnreadable`.
        for suffix in ["_1", "_2", "_3"] {
            let layer = maps_dir.join(format!("{}{}.txt", zone_name, suffix));
            match std::fs::read_to_string(&layer) {
                Ok(t) => Self::parse_into(&t, &mut lines, &mut labels),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!("zone_map: layer {:?} present but unreadable: {} — the base map \
                        for '{}' loaded, but this load is failing rather than serving a possibly \
                        INCOMPLETE label set", layer, e, zone_name);
                    return Err(ZoneMapLoadError::LayerUnreadable(suffix, e.kind()));
                }
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

    /// #816 round 2 (PR #869 review, B2): a PRESENT-but-unreadable detail layer must fail the WHOLE
    /// load, not silently serve just the base content — the exact silent-partial shape #816 itself
    /// fixed one level up, reproduced one level down. Measured over the live client's real maps
    /// cache: `erudsxing`/`qeytoqrg` have zero qualifying labels in their base file and all of them
    /// in `_1.txt` (see the `LayerUnreadable` doc comment), so before this fix an unreadable `_1.txt`
    /// for either would have silently read as "this zone's map has no fallback exits" — false.
    ///
    /// Mutation check: revert the `Err(e) => return Err(...)` arm in `try_load`'s layer loop back to
    /// `if let Ok(t) = ...` (silently skip unreadable layers same as missing ones) and this goes RED.
    #[test]
    fn present_but_unreadable_detail_layer_fails_the_whole_load_816() {
        let dir = tempfile::tempdir().unwrap();
        // Base file loads fine but (like erudsxing/qeytoqrg) contributes nothing itself.
        std::fs::write(dir.path().join("goodbase.txt"), "L 1,2,0,3,4,0,1,1,1").unwrap();
        // A DIRECTORY in place of the `_1.txt` layer — present, but not readable as a file.
        std::fs::create_dir(dir.path().join("goodbase_1.txt")).unwrap();

        match ZoneMap::try_load(dir.path(), "goodbase").unwrap_err() {
            ZoneMapLoadError::LayerUnreadable(suffix, kind) => {
                assert_eq!(suffix, "_1");
                assert_ne!(kind, std::io::ErrorKind::NotFound,
                    "a directory that exists must not be reported as the NotFound kind");
            }
            other => panic!("an unreadable-but-present layer must not read as {other:?} — Missing/ \
                Unreadable both describe the BASE file, which loaded fine here"),
        }

        // The reason/detail strings must be distinct from the base-file failure modes too — a
        // caller must be able to tell "the base is fine but a layer is stuck" from "no map at all".
        assert_eq!(
            ZoneMapLoadError::LayerUnreadable("_1", std::io::ErrorKind::PermissionDenied).as_str(),
            "zone_map_layer_unreadable"
        );
        assert_ne!(
            ZoneMapLoadError::LayerUnreadable("_1", std::io::ErrorKind::PermissionDenied).as_str(),
            ZoneMapLoadError::Missing.as_str()
        );
    }

    /// #872 (agent-honesty, measured surviving mutant): the `LayerUnreadable` suffix must name the
    /// layer that ACTUALLY broke, not just echo back whichever suffix the ONE existing regression
    /// test above happens to break. That test only ever breaks `_1`, so a mutant that hardcodes the
    /// reported suffix to `"_1"` left the whole suite green (PR #869 round 2 review) — the field
    /// exists to tell a caller which file to go look at, and an unpinned wrong answer there sends
    /// them to a file that is fine while the real broken layer keeps being skipped. Pin all three
    /// positions so the suffix can only be correct by actually being read off `e.kind()`'s loop
    /// variable, not by matching the one case every other test exercises.
    #[test]
    fn layer_unreadable_names_the_layer_that_actually_broke_872() {
        for broken in ["_1", "_2", "_3"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("z.txt"), "L 1,2,0,3,4,0,1,1,1").unwrap();
            // Only the layer under test is present (as a directory, so it exists but can't be read
            // as a file); every OTHER layer is left absent. Absent layers are silently skipped, so
            // the loop reaches `broken` regardless of its position (`_1`, `_2`, or `_3`), and no
            // other layer can supply a stray `LayerUnreadable` that would make this pin vacuous.
            std::fs::create_dir(dir.path().join(format!("z{broken}.txt"))).unwrap();

            match ZoneMap::try_load(dir.path(), "z").unwrap_err() {
                ZoneMapLoadError::LayerUnreadable(suffix, kind) => {
                    assert_eq!(suffix, broken,
                        "layer '{broken}' is the one that was made unreadable, but the reported \
                         suffix was '{suffix}' — a wrong suffix sends whoever reads it to inspect \
                         the WRONG file while the actually-broken layer keeps being silently skipped");
                    assert_ne!(kind, std::io::ErrorKind::NotFound,
                        "a directory that exists must not be reported as the NotFound kind");
                }
                other => panic!(
                    "breaking layer '{broken}' must report LayerUnreadable({broken:?}, _), got {other:?}"
                ),
            }
        }
    }

    /// A layer that is simply ABSENT (the overwhelmingly common case, no `_1.txt` at all) must be
    /// unaffected by the B2 fix above — only a PRESENT-but-unreadable layer fails the load.
    /// Companion to `missing_detail_layers_do_not_fail_the_load_816`, pinned again here so the two
    /// "layer is missing" vs "layer is present but broken" cases sit side by side.
    #[test]
    fn absent_layer_is_unaffected_by_the_present_but_unreadable_fix_816() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("z.txt"), "P 1.0, 2.0, 0, 0, 0, 0, 3, to_Somewhere").unwrap();
        // No z_1/_2/_3.txt written — genuinely absent, not present-but-broken.
        let zm = ZoneMap::try_load(dir.path(), "z").expect("an absent layer must not fail the load");
        assert_eq!(zm.labels.len(), 1);
    }

    /// Kept (not thrown away as scaffolding) because #816 round-2 review found a prose claim about
    /// which zones' maps contribute fallback entries that was flat wrong ("only North/South Qeynos"
    /// — actually five zones' packs qualify, see `docs/http-api.md`'s `zone_map_load` section). A
    /// sentence can drift from the code again; this test is the reproducible way to re-check it: it
    /// runs the REAL `try_load` plus the SAME destination-matching rule
    /// `crates/eqoxide-net/src/action_loop.rs`'s `sync_zone_points` uses
    /// (`"to "` + north/south Qeynos / Qeynos2 text match) over the real, LOCAL client maps cache —
    /// not a re-derivation by reading the heuristic. **This is a hand copy, not a verbatim one — say
    /// so rather than overclaim it** (#869 round 2, N7): it counts qualifying LABELS present in a
    /// zone's pack, which is what the "five zones contribute" claim is about, but it deliberately
    /// omits `sync_zone_points`'s dedup-against-already-advertised-point check (that check decides
    /// how many of those labels turn into a NEW zone_point entry at runtime, a different, narrower
    /// question this diagnostic does not answer). Keep the matching rule in sync by hand if
    /// `sync_zone_points`'s ever changes. Needs the asset-sync maps dir to exist, so it is
    /// `#[ignore]`d (CI has no client cache) — run explicitly with `cargo test -p eqoxide-core --lib
    /// zone_map::tests::diagnostic_measure_contributing_zones_869 -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn diagnostic_measure_contributing_zones_869() {
        let maps_dir = std::path::PathBuf::from(
            std::env::var("HOME").unwrap()
        ).join(".local/share/eqoxide/assets/models/maps");
        let mut base_names: Vec<String> = std::fs::read_dir(&maps_dir).unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".txt"))
            .filter(|n| !n.ends_with("_1.txt") && !n.ends_with("_2.txt") && !n.ends_with("_3.txt"))
            .map(|n| n.trim_end_matches(".txt").to_string())
            .collect();
        base_names.sort();
        let reach = base_names.len();
        assert!(reach > 400, "REACH CONTROL: only scanned {reach} base zone packs, expected >400");

        let mut contributing: Vec<(String, usize)> = Vec::new();
        for zone in &base_names {
            let zm = match ZoneMap::try_load(&maps_dir, zone) {
                Ok(zm) => zm,
                Err(_) => continue,
            };
            let mut count = 0;
            for label in &zm.labels {
                let lower = label.text.to_lowercase();
                if !lower.starts_with("to ") { continue; }
                let dest_zone_id: u16 =
                    if lower.contains("north qeynos") || lower.contains("qeynos2") { 2 }
                    else if lower.contains("south qeynos") { 1 }
                    else { 0 };
                if dest_zone_id != 0 { count += 1; }
            }
            if count > 0 { contributing.push((zone.clone(), count)); }
        }
        eprintln!("REACH: {reach} base zone packs scanned");
        eprintln!("CONTRIBUTING ({}): {:?}", contributing.len(), contributing);
    }
}
