//! CI-only asset fetcher for the asset-gated `#[ignore]`d test suite (see #654).
//!
//! `tests/water_capability.rs` and other asset-gated tests need baked zone `.glb` + `.wtr` files
//! that are `.gitignore`d and absent on a fresh checkout — they normally come from the eqoxide
//! asset server at runtime (see the `asset-server-stack` skill / `eqoxide_asset_server`). This
//! binary fetches ONLY the specific zones a caller names, into the exact cache location the
//! client (and these tests, via their `$EQZONES`-or-default fallback) already reads from
//! (`CacheDirs::resolve()`, i.e. `$XDG_DATA_HOME/eqoxide/assets` or `~/.local/share/eqoxide/assets`),
//! so no extra env var wiring is needed on either side.
//!
//! For each zone name given, it fetches:
//!   - `zone/<name>` — the baked terrain+object `.glb` (one file, via the existing `sync_set`).
//!   - the single `maps/water/<name>.wtr` entry out of the `gamedata` set — NOT the whole set.
//!     `gamedata` bundles every zone's minimap + water-region data in one manifest (measured at
//!     243.3 MB / 2054 files); syncing it whole to test two zones would be a needless ~240 MB of
//!     unrelated downloads. Instead this fetches just the two files' own manifest entries and their
//!     content-addressed chunks directly, reusing the crate's own `AssetSync`/`reassemble` (the
//!     same blake3-verified reassembly path `sync_set` uses), so this is not new download/parsing
//!     logic — just a narrower selection of which manifest entries to reassemble.
//!
//! Usage:
//!   ASSET_URL=http://localhost:8088 ASSET_USER=claude ASSET_PASS=... \
//!     cargo run --release --bin fetch_ci_assets -- qcat freportw
//!
//! Exits non-zero (loudly) on any fetch/verify failure — this must never silently leave a zone
//! partially fetched and let the caller's test run interpret that as "assets absent, skip".

use eqoxide::asset_sync::{reassemble, sync_set, AssetSync, CacheDirs, ManifestFetch, Transport};

fn main() -> anyhow::Result<()> {
    let zones: Vec<String> = std::env::args().skip(1).collect();
    if zones.is_empty() {
        anyhow::bail!("usage: fetch_ci_assets <zone> [<zone> ...] (e.g. qcat freportw)");
    }

    let url = std::env::var("ASSET_URL").unwrap_or_else(|_| "http://localhost:8088".into());
    let user = std::env::var("ASSET_USER").unwrap_or_else(|_| "claude".into());
    let pass = std::env::var("ASSET_PASS").unwrap_or_else(|_| "ci".into());

    println!("fetch_ci_assets: logging into asset server at {url}");
    let sync = AssetSync::login(&url, &user, &pass)
        .map_err(|e| anyhow::anyhow!("asset server login failed ({url}): {e}"))?;
    let cache = CacheDirs::resolve();
    println!("fetch_ci_assets: cache root = {}", cache.root.display());

    let mut total_bytes = 0u64;

    // 1. Zone terrain GLBs — one small named set per zone, via the ordinary sync path.
    for zone in &zones {
        let set = format!("zone/{zone}");
        print!("fetch_ci_assets: syncing {set} ... ");
        use std::io::Write;
        std::io::stdout().flush().ok();
        let mut bytes = 0u64;
        sync_set(&sync, &set, &cache, &mut |p| {
            if matches!(p.phase, eqoxide::asset_sync::Phase::Downloading) {
                bytes = p.bytes;
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to sync {set}: {e}"))?;
        total_bytes += bytes;
        println!("OK ({:.1} MB)", bytes as f64 / 1_048_576.0);
    }

    // 2. Water-region files — ONE selective pass over the `gamedata` manifest, not the whole set.
    print!("fetch_ci_assets: fetching gamedata manifest ... ");
    {
        use std::io::Write;
        std::io::stdout().flush().ok();
    }
    let manifest = match sync.get_manifest("gamedata", None)? {
        ManifestFetch::Changed(m) => m,
        ManifestFetch::Unchanged => anyhow::bail!(
            "gamedata manifest reported Unchanged on an unconditional request (no If-None-Match \
             sent) — protocol violation by the server, cannot proceed"
        ),
    };
    println!("OK ({} files total in gamedata)", manifest.files.len());

    for zone in &zones {
        let wanted = format!("maps/water/{zone}.wtr");
        let entry = manifest
            .files
            .iter()
            .find(|f| f.path == wanted)
            .ok_or_else(|| anyhow::anyhow!("gamedata manifest has no entry for {wanted}"))?;
        print!("fetch_ci_assets: fetching {wanted} ({} bytes, {} chunks) ... ",
            entry.size, entry.chunks.len());
        {
            use std::io::Write;
            std::io::stdout().flush().ok();
        }
        for hash in &entry.chunks {
            if cache.cas_has(hash) {
                continue; // already fetched (e.g. a chunk shared with another wanted file)
            }
            let data = sync
                .get_chunk(hash)
                .map_err(|e| anyhow::anyhow!("chunk {hash} for {wanted} failed: {e}"))?;
            let got = eqoxide::asset_sync::blake3_hex(&data);
            if got != *hash {
                anyhow::bail!("chunk {hash} for {wanted} content mismatch (got {got})");
            }
            cache.cas_put(&data)?;
        }
        reassemble(&cache, entry)
            .map_err(|e| anyhow::anyhow!("failed to reassemble {wanted}: {e}"))?;
        total_bytes += entry.size;
        println!("OK ({:.1} MB)", entry.size as f64 / 1_048_576.0);
    }

    println!(
        "fetch_ci_assets: done — {} zone(s), {:.1} MB downloaded total",
        zones.len(),
        total_bytes as f64 / 1_048_576.0
    );
    Ok(())
}
