//! Client-side asset sync: pulls the derived `common` model set from the asset
//! server into a cwd-independent XDG cache. Pure logic here is unit-tested; the
//! HTTP transport is a trait (see UreqTransport).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub struct CacheDirs {
    pub root: PathBuf,
}

impl CacheDirs {
    pub fn resolve() -> Self {
        let root = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("eqoxide")
            .join("assets");
        CacheDirs { root }
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        CacheDirs { root: root.into() }
    }

    pub fn cas_dir(&self) -> PathBuf {
        self.root.join("cas")
    }

    pub fn models_dir(&self) -> PathBuf {
        self.root.join("models")
    }

    fn cas_path(&self, hash: &str) -> PathBuf {
        self.cas_dir().join(hash)
    }

    pub fn cas_has(&self, hash: &str) -> bool {
        self.cas_path(hash).exists()
    }

    pub fn cas_get(&self, hash: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.cas_path(hash))
    }

    pub fn cas_put(&self, bytes: &[u8]) -> std::io::Result<String> {
        let hash = blake3_hex(bytes);
        let path = self.cas_path(&hash);
        if !path.exists() {
            std::fs::create_dir_all(self.cas_dir())?;
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(hash)
    }

    // ── per-set synced state (the staleness cursor) ───────────────────────────
    // Stored in the same cache root as the reassembled files, so clearing the whole cache clears
    // both together. But the digest alone is not proof the files are present: this record also
    // carries the file list from the last successful sync, so an "unchanged digest" response can
    // be checked against what's actually in `models/` before being trusted (#601) — a digest is a
    // claim about the SERVER's content identity, not about the state of our local disk.
    fn synced_path(&self) -> PathBuf {
        self.root.join("synced.json")
    }

    /// Records without a full manifest envelope are a cold manifest cache. CAS chunks
    /// remain reusable. Even a structurally valid record is revalidated against today's
    /// reader support before sending its revision or accepting an unchanged response.
    fn load_synced(&self) -> std::collections::HashMap<String, SyncedSet> {
        std::fs::read(self.synced_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// The digest last successfully synced for `set`, if any (missing/malformed file → `None`).
    pub fn synced_digest(&self, set: &str) -> Option<String> {
        self.synced(set).map(|m| m.digest)
    }

    /// The digest and file list recorded at the last successful sync for `set`, if any. The file
    /// list is what lets `sync_set` verify a server-reported "unchanged" against the real
    /// contents of `models/` instead of trusting the digest blindly.
    fn synced(&self, set: &str) -> Option<Manifest> {
        self.load_synced().remove(set).map(|s| s.manifest)
            .filter(|m| crate::asset_compatibility::validate(m, set).is_ok())
    }

    fn set_synced(&self, manifest: &Manifest) {
        let mut map = self.load_synced();
        map.insert(manifest.set.clone(), SyncedSet { manifest: manifest.clone() });
        if let Ok(bytes) = serde_json::to_vec_pretty(&map) {
            let _ = std::fs::create_dir_all(&self.root);
            let _ = std::fs::write(self.synced_path(), bytes);
        }
    }

    /// Test-only cold-cache reset; preserves CAS and assembled artifacts.
    #[cfg(test)]
    fn clear_synced(&self, set: &str) {
        let mut map = self.load_synced();
        if map.remove(set).is_some() {
            if let Ok(bytes) = serde_json::to_vec_pretty(&map) {
                let _ = std::fs::create_dir_all(&self.root);
                let _ = std::fs::write(self.synced_path(), bytes);
            }
        }
    }
}

/// Full envelope retained for compatibility and identity revalidation before conditional reuse.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct SyncedSet {
    manifest: Manifest,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub blake3: String,
    pub chunks: Vec<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct Manifest {
    pub schema_version: u32,
    pub revision: String,
    pub requirements: crate::asset_compatibility::ReaderRequirements,
    pub set: String,
    /// Content identity of the set (see `set_digest`); conditional identity uses `revision`.
    pub digest: String,
    pub files: Vec<FileEntry>,
}

/// The set's content identity: blake3 over the files sorted by path, each contributing
/// `"{path}\0{blake3}\n"`. MUST stay byte-identical to the server's `ManifestStore::set_digest`.
pub fn set_digest(files: &[FileEntry]) -> String {
    let mut sorted: Vec<&FileEntry> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut h = blake3::Hasher::new();
    for f in sorted {
        h.update(f.path.as_bytes());
        h.update(b"\0");
        h.update(f.blake3.as_bytes());
        h.update(b"\n");
    }
    h.finalize().to_hex().to_string()
}

pub fn missing_chunks(files: &[FileEntry], cache: &CacheDirs) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in files {
        for h in &f.chunks {
            if !cache.cas_has(h) && !out.contains(h) {
                out.push(h.clone());
            }
        }
    }
    out
}

/// Whether every entry in `files` has an assembled artifact in `models_dir` whose size matches
/// what the (last-synced) manifest recorded. This is a cheap, stat-only check — not a full re-hash
/// of potentially large GLBs on every launch — so it catches the case this issue is about (the
/// file deleted/evicted entirely) and gross corruption (truncated/resized), at the cost of not
/// catching a same-size bit-flip. A same-size corruption would need a whole-file hash, which the
/// client doesn't otherwise compute at startup; not worth adding here for that narrower case.
fn artifacts_intact(cache: &CacheDirs, files: &[FileEntry]) -> bool {
    files.iter().all(|f| {
        std::fs::metadata(cache.models_dir().join(&f.path))
            .map(|m| m.len() == f.size)
            .unwrap_or(false)
    })
}

/// Where `fetch_and_reassemble`'s download-phase `elapsed` reads "now" from (#796).
///
/// Production always uses [`DownloadClock::WALL`] — the real monotonic clock — and behaviour is
/// byte-for-byte unchanged from before this type existed: `WALL.now()` is `Instant::now()`. The
/// pinned variant exists only so a test can assert the "elapsed is scoped to THIS phase, not
/// carried over from a prior one" property (#708) EXACTLY, instead of proving it by sleeping real
/// wall time and asserting a soft upper bound on how long the actual download loop's own work took
/// — which is what made `elapsed_does_not_carry_over_from_a_slow_prior_download` fail under
/// remote-builder contention: the assertion raced the machine's load, not the code path it names.
///
/// `DownloadClock` lives in its own private submodule (`download_clock`) rather than merely being
/// a private-field struct in this file. A private tuple field is visible everywhere IN THE MODULE
/// that declares it — and that module used to be all ~1300 lines of `asset_sync.rs`, including
/// every production function here, so nothing actually stopped `sync_set` (or any future call
/// site added to this file) from writing `DownloadClock(Some(t))` directly and silently zeroing
/// every real download's `elapsed`/rate. That was checked directly: pointing `sync_set` at a
/// pinned clock instead of `WALL` compiled clean and the whole `--lib` suite still passed. Scoping
/// the field to `download_clock` instead makes that unreachable rather than merely undone: outside
/// this submodule the only ways to name a `DownloadClock` are `WALL` and the `#[cfg(test)]`-gated
/// `frozen_at`, so in a non-test build `WALL` is the *only* constructible value — the compiler
/// rejects the mutation above (verified: `sync_set` calling `DownloadClock(Some(Instant::now()))`
/// fails with `E0423: cannot initialize a tuple struct which contains private fields`) instead of
/// accepting it.
mod download_clock {
    /// `None` is the real wall clock ([`DownloadClock::WALL`]); `Some` is a pinned instant, only
    /// ever constructible via the `#[cfg(test)]` [`DownloadClock::frozen_at`]. See the module doc
    /// for why the field stays private to this submodule rather than to all of `asset_sync.rs`.
    #[derive(Clone, Copy)]
    pub(super) struct DownloadClock(Option<std::time::Instant>);

    impl DownloadClock {
        pub(super) const WALL: DownloadClock = DownloadClock(None);

        /// A clock pinned at `t` — test fixtures only. See the module doc for why this is gated.
        #[cfg(test)]
        pub(super) fn frozen_at(t: std::time::Instant) -> Self {
            DownloadClock(Some(t))
        }

        pub(super) fn now(self) -> std::time::Instant {
            self.0.unwrap_or_else(std::time::Instant::now)
        }
    }
}
use download_clock::DownloadClock;

/// Downloads whatever chunks in `files` aren't already in the CAS, then reassembles every file.
/// Shared by both the normal (server digest Changed) sync path and the repair path taken when an
/// unchanged digest's own artifacts turn out to be missing/corrupt (#601).
///
/// Takes an explicit [`DownloadClock`] (#796) rather than reading `Instant::now()` directly, so a
/// test can pin what `elapsed` reads "now" as instead of racing the real clock against machine
/// load. Every production call site (both inside this file) passes [`DownloadClock::WALL`], which
/// is exactly `Instant::now()` — behaviour there is unchanged.
fn fetch_and_reassemble_with_clock(
    t: &dyn Transport,
    cache: &CacheDirs,
    files: &[FileEntry],
    progress: &mut dyn FnMut(SyncProgress),
    clock: DownloadClock,
) -> anyhow::Result<()> {
    let missing = missing_chunks(files, cache);
    let total = missing.len();
    let mut bytes = 0u64;
    // Started right as THIS download phase begins (before any chunk fetch), so `elapsed` on every
    // tick below is scoped entirely to this phase's own transfer time — never diluted by time spent
    // verifying or idle in a prior phase, and never carried into the next one (#708). When `missing`
    // is empty (every chunk already cached) this loop body never runs at all, so `progress` is never
    // called with `Downloading` — the caller sees no download tick, not a misleading "0 B/s" one
    // (#708 requirement 2: "nothing to download" must not render like a stall).
    let start = clock.now();
    for (i, hash) in missing.iter().enumerate() {
        let data = t.get_chunk(hash)?;
        // The server is content-addressed: a chunk's bytes must hash to its id.
        let got = blake3_hex(&data);
        if &got != hash {
            anyhow::bail!("chunk {hash} content mismatch (got {got})");
        }
        cache.cas_put(&data)?;
        bytes += data.len() as u64;
        progress(SyncProgress::Downloading {
            done: i + 1,
            total,
            bytes,
            elapsed: clock.now().saturating_duration_since(start),
        });
    }
    for entry in files {
        reassemble(cache, entry)?;
    }
    Ok(())
}

pub fn reassemble(cache: &CacheDirs, entry: &FileEntry) -> anyhow::Result<()> {
    let mut bytes = Vec::with_capacity(entry.size as usize);
    for h in &entry.chunks {
        let chunk = cache
            .cas_get(h)
            .map_err(|e| anyhow::anyhow!("missing chunk {h} for {}: {e}", entry.path))?;
        bytes.extend_from_slice(&chunk);
    }
    // `artifacts_intact` treats a length mismatch against `entry.size` as "not there" and
    // triggers a repair — but pre-#601, `size` was never actually verified here, only used as a
    // `Vec::with_capacity` hint. A manifest whose `size` disagrees with the real assembled length
    // (server bug, hand-edited manifest, ...) would keep "succeeding" here while never satisfying
    // `artifacts_intact`, so every future launch would repair it again — forever, silently,
    // without ever converging or surfacing an error (#601 D-5). Reject it loudly and immediately
    // instead of building a state that can never be recognized as intact.
    if bytes.len() as u64 != entry.size {
        anyhow::bail!(
            "size mismatch for {}: manifest says {}, assembled {} bytes",
            entry.path,
            entry.size,
            bytes.len()
        );
    }
    let got = blake3_hex(&bytes);
    if got != entry.blake3 {
        anyhow::bail!("blake3 mismatch for {}: expected {} got {got}", entry.path, entry.blake3);
    }
    let out_path = cache.models_dir().join(&entry.path);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write to a sibling temp file, then atomically rename into place. A concurrent reader (the
    // zone-load path parses this same file right after sync) must never observe a half-written
    // GLB: with an in-place `write`, a large file (qeynos ~50 MB) could be parsed mid-write under
    // I/O contention → "failed to parse zone glb" → fallback grass. Rename within the same
    // directory is atomic on the same filesystem, so a reader sees either the old complete file or
    // the new complete file. The `.part` suffix keeps a crashed/aborted write out of the real path.
    // (eqoxide#223)
    let tmp_path = out_path.with_extension(format!(
        "{}.part",
        out_path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp_path, &bytes)?;
    if let Err(e) = std::fs::rename(&tmp_path, &out_path) {
        let _ = std::fs::remove_file(&tmp_path); // don't leave a stray .part behind
        return Err(e.into());
    }
    Ok(())
}

/// Conditional fetch keyed by manifest revision, including requirements and chunk layout.
pub enum ManifestFetch {
    Unchanged,
    Changed(Manifest),
}

pub trait Transport {
    fn get_manifest(&self, set: &str, if_none_match: Option<&str>) -> anyhow::Result<ManifestFetch>;
    fn get_chunk(&self, hash: &str) -> anyhow::Result<Vec<u8>>;
}

pub struct AssetSync {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl AssetSync {
    /// Log in to the asset server at `base`; the returned handle carries the session token.
    pub fn login(base: &str, username: &str, password: &str) -> anyhow::Result<Self> {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(30))
            .build();
        let resp: serde_json::Value = agent
            .post(&format!("{base}/auth"))
            .send_json(serde_json::json!({ "username": username, "password": password }))
            .map_err(|e| anyhow::anyhow!("asset auth failed: {e}"))?
            .into_json()?;
        let token = resp
            .get("token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("no token in auth response"))?
            .to_string();
        Ok(Self { base: base.to_string(), token, agent })
    }
}

impl Transport for AssetSync {
    fn get_manifest(&self, set: &str, if_none_match: Option<&str>) -> anyhow::Result<ManifestFetch> {
        let mut req = self
            .agent
            .get(&format!("{}/manifest/{set}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("X-Eqoxide-Asset-Readers", crate::asset_compatibility::READERS_HEADER)
            .set("X-Eqoxide-Asset-Capabilities", crate::asset_compatibility::CAPABILITIES_HEADER);
        if let Some(d) = if_none_match {
            req = req.set("If-None-Match", &format!("\"{d}\""));
        }
        // ureq returns 2xx/3xx as Ok and 4xx/5xx as Err(Status). 304 can surface either way
        // depending on version — handle both.
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(304, _)) => return Ok(ManifestFetch::Unchanged),
            Err(ureq::Error::Status(409, response)) => {
                let detail: serde_json::Value = response.into_json()?;
                anyhow::bail!("manifest {set} incompatible: {detail}; client supports readers [{}] capabilities [{}]",
                    crate::asset_compatibility::READERS_HEADER, crate::asset_compatibility::CAPABILITIES_HEADER);
            }
            Err(e) => return Err(anyhow::anyhow!("manifest {set} failed: {e}")),
        };
        if resp.status() == 304 {
            return Ok(ManifestFetch::Unchanged);
        }
        let manifest: Manifest = resp.into_json()?;
        crate::asset_compatibility::validate(&manifest, set)?;
        Ok(ManifestFetch::Changed(manifest))
    }

    fn get_chunk(&self, hash: &str) -> anyhow::Result<Vec<u8>> {
        let resp = self
            .agent
            .get(&format!("{}/chunk/{hash}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .call()
            .map_err(|e| anyhow::anyhow!("chunk {hash} failed: {e}"))?;
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf)?;
        Ok(buf)
    }
}

/// A single progress tick from a sync operation (#708). `Downloading` is the ONLY variant that
/// carries transfer data (`done`/`total`/`bytes`/`elapsed`) — `Verifying` has none. This is not a
/// style choice: it makes a stale download rate structurally unrepresentable rather than merely
/// something a caller must remember to clear. There is no long-lived "current rate" field anywhere
/// that a phase change would need to reset — a caller can only ever read `elapsed`/`bytes` from the
/// one tick that is actually in `Downloading`, and every other phase's tick has nowhere to keep one.
///
/// `elapsed` is measured from the start of the CURRENT downloading phase only (see
/// `fetch_and_reassemble`), so a rate derived from it is a phase-local session-average — not an
/// instantaneous or windowed rate (#708 requirement 3) — and cannot be inflated by idle time spent
/// verifying beforehand.
pub enum SyncProgress {
    Verifying,
    Downloading { done: usize, total: usize, bytes: u64, elapsed: std::time::Duration },
}

/// Minimum elapsed time before a `Downloading` tick's `bytes`/`elapsed` are considered stable enough
/// to divide into a rate. The very first tick of a phase can land well under a millisecond after
/// `Instant::now()` was captured (a tiny/local chunk, warm caches) — dividing by that would produce
/// an absurd, noisy spike rather than an honest "not enough data yet" (#708 requirement 4).
const MIN_RATE_ELAPSED: std::time::Duration = std::time::Duration::from_millis(100);

/// Pure function: derives a bytes/sec rate from cumulative bytes transferred and elapsed time, or
/// `None` if `elapsed` is too small to divide by safely (see `MIN_RATE_ELAPSED`). Deliberately takes
/// plain `(u64, Duration)` rather than a `SyncProgress` so it's trivial to unit-test every edge
/// (zero elapsed, near-zero elapsed, zero bytes) without constructing a whole sync pipeline.
pub fn download_rate_bytes_per_sec(bytes: u64, elapsed: std::time::Duration) -> Option<f64> {
    if elapsed < MIN_RATE_ELAPSED {
        return None;
    }
    Some(bytes as f64 / elapsed.as_secs_f64())
}

/// Sync one named set from `t` into `cache`, reporting each producer tick to `progress`.
pub fn sync_set(
    t: &dyn Transport,
    set: &str,
    cache: &CacheDirs,
    progress: &mut dyn FnMut(SyncProgress),
) -> anyhow::Result<()> {
    sync_set_with_clock(t, set, cache, progress, DownloadClock::WALL)
}

/// The real body of `sync_set` — separated out so a test can supply a pinned [`DownloadClock`]
/// instead of racing the real one (#796). `sync_set` is the only production entry point and always passes [`DownloadClock::WALL`].
fn sync_set_with_clock(
    t: &dyn Transport,
    set: &str,
    cache: &CacheDirs,
    progress: &mut dyn FnMut(SyncProgress),
    clock: DownloadClock,
) -> anyhow::Result<()> {
    let prev = cache.synced(set);
    // Only offer a revision from a complete manifest validated against current reader support.
    let if_none_match = prev.as_ref().map(|m| m.revision.as_str());

    match t.get_manifest(set, if_none_match)? {
        ManifestFetch::Unchanged => {
            // The server saying "unchanged" is a claim about ITS content identity — it says
            // nothing about whether our own assembled artifact is still on disk. Trusting the
            // digest alone here is exactly the #601 bug: if the file is deleted/evicted (or
            // corrupted) while the server digest stays put, the digest never changes on its own,
            // so the client would report "up to date" forever and clearing the local cache (the
            // obvious recovery move) would not help, because the digest — not the artifact — is
            // what gets consulted. Verify the artifacts before honoring the short-circuit.
            //
            // `Unchanged` is only a valid reply to a request that carried an `If-None-Match` — if
            // we sent none (no prior record), a server/intermediary claiming Unchanged anyway is
            // a protocol violation, not a state this code can trust or repair from (there is no
            // recorded file list to check). Fail loudly rather than assume: `prev` being `None`
            // here must surface as an ordinary `Err`, never a panic. `sync_set` is called from
            // three background threads in `src/app.rs`; only the zone-asset loader is backstopped
            // against a panic (`catch_unwind` around its body at src/app.rs:643, added by #595) —
            // the common-asset-loader (src/app.rs:846-884) and the model-sync-worker
            // (src/app.rs:820-833) have no such guard, so a panic there unwinds the whole thread:
            // the `Err`-publishing code after the `sync_set` call (which sets `self.sync_done` /
            // logs via the model_rx loop) never runs, and the client sits on "Verifying..."/loading
            // forever — the exact pending-forever falsehood #579 exists to prevent (#601 D-1).
            let Some(manifest) = prev else {
                anyhow::bail!(
                    "server reported set {set} as unchanged but we hold no prior synced record \
                     for it (no If-None-Match was sent) — protocol violation by the server or an \
                     intermediary, cannot verify or repair"
                );
            };
            // Defense in depth: keep the 304 acceptance boundary safe if cache lookup changes.
            crate::asset_compatibility::validate(&manifest, set)?;
            let files = manifest.files;
            if artifacts_intact(cache, &files) {
                return Ok(()); // content AND local artifacts both unchanged — genuine no-op
            }
            progress(SyncProgress::Verifying);
            if let Err(repair_err) = fetch_and_reassemble_with_clock(t, cache, &files, progress, clock) {
                // The recorded chunk list can itself be stale even though the digest is not: the
                // digest (`set_digest`) hashes only `path\0<file-blake3>`, never chunk ids, so a
                // server that re-chunks a file (different FastCDC params, a re-bake, ...) while
                // its *content* is unchanged produces an identical digest with entirely different
                // chunk hashes. Retrying the SAME recorded chunk list would then fail on every
                // future launch, permanently (#601 D-2). Bypass the stale conditional and try
                // one fresh, unconditional manifest fetch — the server's CURRENT chunk ids — rather
                // than keep repairing with data the server can no longer serve.
                // Preserve the previous record until a fresh manifest is validated and applied.
                return match t.get_manifest(set, None)? {
                    ManifestFetch::Changed(m) =>
                        sync_changed_manifest_with_clock(t, set, cache, m, progress, clock),
                    ManifestFetch::Unchanged => Err(repair_err.context(format!(
                        "repair of set {set} failed, and a fresh (unconditional) manifest fetch \
                         still reports Unchanged — server cannot supply working chunk data"
                    ))),
                };
            }
            Ok(()) // digest/file-list unchanged from what's already recorded — nothing new to persist
        }
        ManifestFetch::Changed(m) => sync_changed_manifest_with_clock(t, set, cache, m, progress, clock),
    }
}

/// Verifies a freshly-fetched manifest against its own claimed digest, downloads/reassembles its
/// files, and records the new synced state. Shared by the normal (digest Changed) path and the
/// repair-failure fallback above (#601 D-2), both of which end up with a manifest they need to
/// fully (re)apply.
///
/// Takes an explicit [`DownloadClock`] (#796), forwarded to `fetch_and_reassemble_with_clock`; see
/// that function's doc. Both call sites (inside `sync_set_with_clock`) pass through whatever clock
/// they themselves were given, so a pinned test clock reaches all the way down.
fn sync_changed_manifest_with_clock(
    t: &dyn Transport,
    set: &str,
    cache: &CacheDirs,
    manifest: Manifest,
    progress: &mut dyn FnMut(SyncProgress),
    clock: DownloadClock,
) -> anyhow::Result<()> {
    crate::asset_compatibility::validate(&manifest, set)?;
    progress(SyncProgress::Verifying);

    fetch_and_reassemble_with_clock(t, cache, &manifest.files, progress, clock)?;

    // Record the synced identity so a future unchanged fetch (304) can skip the whole set.
    cache.set_synced(&manifest);
    Ok(())
}

#[cfg(test)]
fn test_manifest(set: &str, files: Vec<FileEntry>) -> Manifest {
    let mut manifest = Manifest { schema_version: 1, revision: String::new(),
        requirements: crate::asset_compatibility::ReaderRequirements::legacy(),
        set: set.into(), digest: set_digest(&files), files };
    manifest.revision = crate::asset_compatibility::revision(&manifest);
    manifest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_is_under_data_dir_and_cwd_independent() {
        let c = CacheDirs::resolve();
        let data = dirs::data_dir().unwrap();
        assert!(c.root.starts_with(&data), "{:?} not under {:?}", c.root, data);
        assert!(c.root.ends_with("eqoxide/assets"));
    }

    #[test]
    fn cas_put_get_has_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let c = CacheDirs::with_root(dir.path());
        let h = c.cas_put(b"hello").unwrap();
        assert_eq!(h, blake3_hex(b"hello"));
        assert!(c.cas_has(&h));
        assert_eq!(c.cas_get(&h).unwrap(), b"hello");
        assert!(!c.cas_has("deadbeef"));
    }
}

#[cfg(test)]
mod manifest_tests {
    use super::*;

    fn manifest_with(cache: &CacheDirs) -> (Manifest, Vec<u8>) {
        // store two chunks for one file
        let part_a = vec![1u8; 10];
        let part_b = vec![2u8; 20];
        let ha = cache.cas_put(&part_a).unwrap();
        let hb = cache.cas_put(&part_b).unwrap();
        let mut whole = part_a.clone();
        whole.extend_from_slice(&part_b);
        let files = vec![FileEntry {
            path: "humanoid.glb".into(),
            size: whole.len() as u64,
            blake3: blake3_hex(&whole),
            chunks: vec![ha, hb],
        }];
        let m = test_manifest("common", files);
        (m, whole)
    }

    #[test]
    fn missing_chunks_lists_only_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let (m, _) = manifest_with(&cache);
        // both chunks were just put -> nothing missing
        assert!(missing_chunks(&m.files, &cache).is_empty());

        // a manifest referencing an absent chunk
        let mut m2 = m.clone();
        m2.files[0].chunks.push("absenthash".into());
        assert_eq!(missing_chunks(&m2.files, &cache), vec!["absenthash".to_string()]);
    }

    #[test]
    fn reassemble_writes_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let (m, whole) = manifest_with(&cache);
        reassemble(&cache, &m.files[0]).unwrap();
        let out = std::fs::read(cache.models_dir().join("humanoid.glb")).unwrap();
        assert_eq!(out, whole);
    }

    #[test]
    fn reassemble_is_atomic_and_leaves_no_part_file() {
        // eqoxide#223: reassemble writes via a temp file + atomic rename. After success the final
        // file is complete and no ".part" temp remains (a leftover would signal a non-atomic path).
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let (m, whole) = manifest_with(&cache);
        reassemble(&cache, &m.files[0]).unwrap();
        let out_path = cache.models_dir().join("humanoid.glb");
        assert_eq!(std::fs::read(&out_path).unwrap(), whole, "final file is the complete content");
        // No sibling .part temp left behind.
        let part = out_path.with_extension("glb.part");
        assert!(!part.exists(), "no .part temp should remain after a successful reassemble");
        // Re-running over an existing complete file replaces it atomically (still complete, no part).
        reassemble(&cache, &m.files[0]).unwrap();
        assert_eq!(std::fs::read(&out_path).unwrap(), whole);
        assert!(!part.exists());
    }

    #[test]
    fn reassemble_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let (mut m, _) = manifest_with(&cache);
        m.files[0].blake3 = "0".repeat(64); // wrong expected hash
        assert!(reassemble(&cache, &m.files[0]).is_err());
    }

    #[test]
    fn reassemble_rejects_size_mismatch_instead_of_looping_forever() {
        // #601 D-5: pre-fix, `entry.size` was only ever used as a `Vec::with_capacity` hint here —
        // never actually checked against the assembled length. But `artifacts_intact` (this same
        // PR) DOES compare the on-disk file's length against `entry.size`. A manifest whose `size`
        // disagrees with the true assembled length (server bug, hand-edited manifest, a chunk
        // that's the wrong length for its hash slot, ...) would then "succeed" here while never
        // satisfying `artifacts_intact` — silently repairing the same set again on every future
        // launch, forever, with no error ever surfacing. `reassemble` must instead reject a
        // size-mismatched manifest loudly, immediately, so the failure is visible instead of an
        // invisible non-converging loop.
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let (mut m, _) = manifest_with(&cache);
        m.files[0].size += 1; // claims one byte more than the chunks actually assemble to
        let err = reassemble(&cache, &m.files[0]).unwrap_err();
        assert!(
            err.to_string().contains("size mismatch"),
            "expected a size-mismatch error, got: {err}"
        );
        assert!(
            !cache.models_dir().join("humanoid.glb").exists(),
            "a size-mismatched artifact must not be written at all"
        );
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeTransport {
        manifest: Manifest,
        chunks: HashMap<String, Vec<u8>>,
        chunk_calls: std::cell::RefCell<usize>,
        manifest_calls: std::cell::RefCell<usize>,
    }
    impl Transport for FakeTransport {
        // Mirrors the real server: 304 (Unchanged) when the client's If-None-Match equals the digest.
        fn get_manifest(&self, _set: &str, inm: Option<&str>) -> anyhow::Result<ManifestFetch> {
            *self.manifest_calls.borrow_mut() += 1;
            if inm == Some(self.manifest.revision.as_str()) {
                return Ok(ManifestFetch::Unchanged);
            }
            Ok(ManifestFetch::Changed(self.manifest.clone()))
        }
        fn get_chunk(&self, hash: &str) -> anyhow::Result<Vec<u8>> {
            *self.chunk_calls.borrow_mut() += 1;
            self.chunks.get(hash).cloned()
                .ok_or_else(|| anyhow::anyhow!("no chunk {hash}"))
        }
    }

    fn fixture() -> FakeTransport {
        let a = vec![1u8; 10];
        let b = vec![2u8; 20];
        let ha = blake3_hex(&a);
        let hb = blake3_hex(&b);
        let mut whole = a.clone(); whole.extend_from_slice(&b);
        let mut chunks = HashMap::new();
        chunks.insert(ha.clone(), a);
        chunks.insert(hb.clone(), b);
        let files = vec![FileEntry {
            path: "humanoid.glb".into(), size: whole.len() as u64,
            blake3: blake3_hex(&whole), chunks: vec![ha, hb],
        }];
        FakeTransport {
            manifest: test_manifest("common", files),
            chunks,
            chunk_calls: std::cell::RefCell::new(0),
            manifest_calls: std::cell::RefCell::new(0),
        }
    }

    #[test]
    fn set_digest_is_order_independent() {
        let f = |p: &str, b: &str| FileEntry { path: p.into(), size: 1, blake3: b.into(), chunks: vec![] };
        let a = vec![f("b", "22"), f("a", "11")];
        let mut rev = a.clone(); rev.reverse();
        assert_eq!(set_digest(&a), set_digest(&rev));
        assert_eq!(set_digest(&a).len(), 64);
    }

    #[test]
    fn synced_digest_round_trips_and_tolerates_missing() {
        let dir = tempfile::tempdir().unwrap();
        let c = CacheDirs::with_root(dir.path());
        assert_eq!(c.synced_digest("zone/qeynos"), None);
        c.set_synced(&test_manifest("zone/qeynos", vec![]));
        c.set_synced(&test_manifest("gamedata", vec![]));
        assert_eq!(c.synced_digest("zone/qeynos").as_deref(), Some(set_digest(&[]).as_str()));
        assert_eq!(c.synced_digest("gamedata").as_deref(), Some(set_digest(&[]).as_str()));
    }

    #[test]
    fn old_format_synced_json_is_discarded_not_trusted_or_fatal() {
        // Before #601, `synced.json` stored `{set: "digest-string"}`. Older installs
        // may still have files in that shape on disk. The current format retains a complete versioned manifest envelope.
        // A schema change to a persisted file must not paper over the mismatch: an old record must
        // not be silently (mis)trusted as valid new-format data, and reading it must not panic or
        // error out the client on first launch after upgrade. It must read as "never synced" and
        // let the set re-sync once — a one-time slow, self-healing recovery, not a crash and not a
        // silent partial state.
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        std::fs::create_dir_all(&cache.root).unwrap();
        std::fs::write(
            cache.root.join("synced.json"),
            r#"{"common": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"}"#,
        )
        .unwrap();

        // The old-shaped record must not surface as a trusted digest...
        assert_eq!(
            cache.synced_digest("common"),
            None,
            "an old-format (pre-#601) record must not be mistaken for a valid new-format digest"
        );

        // ...and a sync against it must not panic/error — it must simply treat the set as
        // never-synced and do a full (one-time) re-sync.
        let t = fixture();
        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
        assert!(cache.models_dir().join("humanoid.glb").exists());
        assert_eq!(cache.synced_digest("common").as_deref(), Some(t.manifest.digest.as_str()));
    }

    #[test]
    fn warm_cache_upgrade_costs_no_chunk_refetch() {
        // #601 D-4: quantifies the migration cost documented on `load_synced`. A warm cache (CAS
        // chunks + assembled artifact already present, from before an upgrade) that happens to
        // still carry an OLD-FORMAT synced.json must re-verify via exactly one extra manifest
        // fetch, but must NOT re-download any chunk — they're already in the local CAS,
        // content-addressed by hash and entirely independent of what synced.json records.
        for pre_envelope in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let cache = CacheDirs::with_root(dir.path());
            let t = fixture();

            // Establish a fully-synced, new-format state first (this is the pre-upgrade state).
            sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
            assert_eq!(*t.chunk_calls.borrow(), 2);

            // Cover both pre-#601 strings and the immediately preceding digest/files records,
            // leaving the CAS and the assembled artifact untouched — a real upgrade changes only the
            // code reading synced.json, never the CAS or models/ contents.
            std::fs::write(
                cache.root.join("synced.json"),
                serde_json::to_vec(&if pre_envelope {
                    serde_json::json!({"common": {"digest": t.manifest.digest, "files": t.manifest.files}})
                } else {
                    serde_json::json!({"common": t.manifest.digest})
                }).unwrap(),
            )
            .unwrap();

            assert!(cache.synced_digest("common").is_none(), "old records must not authorize reuse");

            let chunk_calls_before = *t.chunk_calls.borrow();
            let manifest_calls_before = *t.manifest_calls.borrow();
            sync_set(&t, "common", &cache, &mut |_| {}).unwrap();

            assert_eq!(
                *t.chunk_calls.borrow(),
                chunk_calls_before,
                "an upgrade must not re-download any chunk — they're already content-addressed in the local CAS"
            );
            assert_eq!(
                *t.manifest_calls.borrow(),
                manifest_calls_before + 1,
                "an upgrade costs exactly one extra manifest fetch (the old-format record can't be \
                 trusted, so no If-None-Match is sent, so the server can't reply 304)"
            );
            let mut whole = vec![1u8; 10];
            whole.extend_from_slice(&[2u8; 20]);
            assert_eq!(
                std::fs::read(cache.models_dir().join("humanoid.glb")).unwrap(),
                whole,
                "the artifact is byte-identical after the migration re-sync"
            );
            assert_eq!(
                cache.synced_digest("common").as_deref(),
                Some(t.manifest.digest.as_str()),
                "the new-format record is restored after the one-time re-verification"
            );
        }
    }

    #[test]
    fn unchanged_digest_with_intact_artifact_still_skips() {
        // The short-circuit exists for good reason (no redundant chunk fetches/rewrites on every
        // launch) and must be preserved when the artifact really is fine.
        //
        // #601 D-3: a `chunk_calls` count alone is NOT sufficient to prove the short-circuit ran —
        // it stays flat even if the whole `artifacts_intact` check were deleted and `reassemble`
        // were called unconditionally, because `reassemble` never fetches chunks on its own (it
        // only reads whatever's already in the local CAS, which is still fully populated from the
        // first sync). So also assert the artifact's inode is untouched across the second
        // `sync_set` call: `reassemble` always writes through a temp file + `rename` (for atomicity
        // — see eqoxide#223), and a `rename`-over-an-existing-path always allocates a fresh inode
        // even when the resulting bytes and mtime happen to coincide with the old file. An
        // unchanged inode is therefore proof `reassemble` did not run a second time, not just proof
        // no *chunks* were fetched.
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let t = fixture();
        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
        assert_eq!(cache.synced_digest("common").as_deref(), Some(t.manifest.digest.as_str()));

        let artifact_path = cache.models_dir().join("humanoid.glb");
        let ino_before = std::fs::metadata(&artifact_path).unwrap().ino();

        let before = *t.chunk_calls.borrow();
        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
        assert_eq!(*t.chunk_calls.borrow(), before, "Unchanged + intact artifact must not fetch chunks");
        let ino_after = std::fs::metadata(&artifact_path).unwrap().ino();
        assert_eq!(
            ino_before, ino_after,
            "Unchanged + intact artifact must not rewrite the artifact at all (same inode)"
        );
    }

    #[test]
    fn unchanged_digest_with_missing_artifact_rebuilds() {
        // #601: an unchanged server digest is a claim about server-side content identity only —
        // it says nothing about whether OUR assembled artifact is still on disk. Simulates the
        // "obvious" recovery move (delete the reassembled file, expect a relaunch to fix it) while
        // the server-side content hasn't changed; that recovery must actually work, not wedge
        // forever behind a digest that will never change on its own.
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let t = fixture();
        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
        assert!(cache.models_dir().join("humanoid.glb").exists());

        std::fs::remove_file(cache.models_dir().join("humanoid.glb")).unwrap();
        assert_eq!(
            cache.synced_digest("common").as_deref(),
            Some(t.manifest.digest.as_str()),
            "the recorded digest is untouched by deleting the artifact"
        );

        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
        assert!(
            cache.models_dir().join("humanoid.glb").exists(),
            "an unchanged digest must not be trusted over a missing artifact — it must be rebuilt"
        );
    }

    #[test]
    fn unchanged_digest_with_wrong_sized_artifact_rebuilds() {
        // A same-name-but-corrupt (here: truncated) artifact must also be treated as "not there"
        // rather than trusted just because the path exists.
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let t = fixture();
        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
        let path = cache.models_dir().join("humanoid.glb");
        std::fs::write(&path, b"corrupt").unwrap(); // right name, wrong size

        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
        let out = std::fs::read(&path).unwrap();
        assert_eq!(out.len(), 30, "the correctly-sized artifact (10+20 bytes) must be restored");
        assert_ne!(out, b"corrupt", "the corrupt bytes must not survive the sync");
    }

    #[test]
    fn rejects_manifest_with_wrong_digest() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let mut t = fixture();
        t.manifest.digest = "0".repeat(64); // no longer matches its files
        assert!(sync_set(&t, "common", &cache, &mut |_| {}).is_err());
    }

    #[test]
    fn cold_then_warm_sync() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let t = fixture();
        let mut last = None;
        sync_set(&t, "common", &cache, &mut |p| {
            if let SyncProgress::Downloading { done, total, .. } = p {
                last = Some((done, total));
            }
        }).unwrap();
        // cold: both chunks fetched, file reassembled
        assert_eq!(*t.chunk_calls.borrow(), 2);
        assert!(cache.models_dir().join("humanoid.glb").exists());
        assert_eq!(last, Some((2, 2)));

        // warm: nothing missing -> no further chunk fetches
        let before = *t.chunk_calls.borrow();
        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
        assert_eq!(*t.chunk_calls.borrow(), before);
    }

    #[test]
    fn rejects_chunk_with_wrong_hash() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let mut t = fixture();
        // corrupt one chunk's bytes so its content no longer matches its hash key
        let key = t.manifest.files[0].chunks[0].clone();
        t.chunks.insert(key, vec![9u8; 10]);
        assert!(sync_set(&t, "common", &cache, &mut |_| {}).is_err());
    }

    #[test]
    fn unchanged_with_no_prior_record_is_an_honest_error_not_a_panic() {
        // #601 D-1: `Unchanged` is only a legitimate reply to a request that carried an
        // `If-None-Match` derived from a prior record. A transport (misbehaving server, broken
        // proxy, ...) that claims Unchanged even when we sent NO If-None-Match at all is lying
        // about the protocol, and `sync_set` has no recorded file list to verify or repair from in
        // that case. This must surface as an ordinary `Err`, never a panic: of the three
        // background threads in src/app.rs that call `sync_set`, only the zone-asset loader is
        // backstopped against a panic (`catch_unwind` around its body, added by #595, src/app.rs:643)
        // — the common-asset-loader (src/app.rs:846-884) and the model-sync-worker
        // (src/app.rs:820-833) have no such guard, so a panic there unwinds the whole thread and
        // the code that would otherwise publish an honest failure never runs, leaving the client
        // stuck on "Verifying..."/loading forever — the exact pending-forever falsehood this
        // project treats as worse than a crash.
        struct LyingUnchangedTransport;
        impl Transport for LyingUnchangedTransport {
            fn get_manifest(&self, _set: &str, _inm: Option<&str>) -> anyhow::Result<ManifestFetch> {
                Ok(ManifestFetch::Unchanged) // lies even on a bare (no If-None-Match) request
            }
            fn get_chunk(&self, hash: &str) -> anyhow::Result<Vec<u8>> {
                anyhow::bail!("should never be called: {hash}")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path()); // cold cache: no prior record for "common"
        let t = LyingUnchangedTransport;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sync_set(&t, "common", &cache, &mut |_| {})
        }));

        let returned = match result {
            Ok(r) => r,
            Err(_) => panic!(
                "sync_set panicked instead of returning an Err — this is exactly the #601 D-1 \
                 pending-forever hazard (a panic escapes the background-thread Err arm in src/app.rs)"
            ),
        };
        assert!(
            returned.is_err(),
            "a lying Unchanged reply with no prior record must be a hard Err, not Ok(())"
        );
    }

    #[test]
    fn repair_failure_recovers_via_fresh_manifest_after_rechunk() {
        // #601 D-2: the recorded file list can go stale even though the digest stays the same,
        // because `set_digest` hashes only `path\0<whole-file-blake3>` — it never covers chunk ids.
        // If the server re-chunks a file (different FastCDC params, a re-bake, ...) while the
        // file's *content* is unchanged, the digest the client already has on record is still
        // correct, but the specific chunk hashes recorded alongside it may no longer be servable.
        // If a local repair is then needed (artifact deleted/evicted) and the ONLY thing tried is
        // re-fetching those exact stale chunk hashes, it fails — and since the digest never
        // changes, it would fail again on every future launch, forever, with no path to recovery.
        // The fix: on repair failure, bypass the stale conditional and fetch one fresh,
        // unconditional manifest fetch (current chunk ids), then fully apply that.
        use std::cell::Cell;

        struct RechunkingTransport {
            v1_files: Vec<FileEntry>,
            v2_files: Vec<FileEntry>,
            v1_chunks: HashMap<String, Vec<u8>>,
            v2_chunks: HashMap<String, Vec<u8>>,
            rechunked: Cell<bool>,
        }
        impl Transport for RechunkingTransport {
            fn get_manifest(&self, _set: &str, inm: Option<&str>) -> anyhow::Result<ManifestFetch> {
                if inm == Some(test_manifest("common", self.v1_files.clone()).revision.as_str()) {
                    return Ok(ManifestFetch::Unchanged);
                }
                let files = if self.rechunked.get() { self.v2_files.clone() } else { self.v1_files.clone() };
                Ok(ManifestFetch::Changed(test_manifest("common", files)))
            }
            fn get_chunk(&self, hash: &str) -> anyhow::Result<Vec<u8>> {
                let map = if self.rechunked.get() { &self.v2_chunks } else { &self.v1_chunks };
                map.get(hash).cloned().ok_or_else(|| anyhow::anyhow!("no chunk {hash}"))
            }
        }

        let whole = { let mut w = vec![1u8; 10]; w.extend_from_slice(&[2u8; 20]); w }; // 30 bytes
        let whole_blake3 = blake3_hex(&whole);

        // v1 chunking: [0..10] / [10..30]
        let (a1, b1) = (whole[0..10].to_vec(), whole[10..30].to_vec());
        let (ha1, hb1) = (blake3_hex(&a1), blake3_hex(&b1));
        let v1_files = vec![FileEntry {
            path: "humanoid.glb".into(), size: whole.len() as u64,
            blake3: whole_blake3.clone(), chunks: vec![ha1.clone(), hb1.clone()],
        }];
        let mut v1_chunks = HashMap::new();
        v1_chunks.insert(ha1, a1);
        v1_chunks.insert(hb1, b1);

        // v2 chunking: [0..15] / [15..30] — same content, different chunk boundaries/hashes.
        let (a2, b2) = (whole[0..15].to_vec(), whole[15..30].to_vec());
        let (ha2, hb2) = (blake3_hex(&a2), blake3_hex(&b2));
        let v2_files = vec![FileEntry {
            path: "humanoid.glb".into(), size: whole.len() as u64,
            blake3: whole_blake3.clone(), chunks: vec![ha2.clone(), hb2.clone()],
        }];
        let mut v2_chunks = HashMap::new();
        v2_chunks.insert(ha2, a2);
        v2_chunks.insert(hb2, b2);

        let digest_v1 = set_digest(&v1_files);
        let digest_v2 = set_digest(&v2_files);
        assert_eq!(
            digest_v1, digest_v2,
            "set_digest must be chunk-agnostic (path+file-blake3 only) for this scenario to be real"
        );

        let t = RechunkingTransport {
            v1_files,
            v2_files,
            v1_chunks,
            v2_chunks,
            rechunked: Cell::new(false),
        };

        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());

        // Cold sync using v1 chunking — establishes the record with the (soon to be stale) v1
        // chunk hashes.
        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
        assert_eq!(std::fs::read(cache.models_dir().join("humanoid.glb")).unwrap(), whole);

        // Simulate: the local CAS gets evicted/cleared (independent recovery action, or just disk
        // pressure) AND the assembled artifact goes missing — so a repair is needed — AND, at the
        // same time, the server has since re-chunked (its old chunk ids are no longer servable).
        std::fs::remove_dir_all(cache.cas_dir()).unwrap();
        std::fs::remove_file(cache.models_dir().join("humanoid.glb")).unwrap();
        t.rechunked.set(true);

        // Simulate a stale conditional responder despite a newer revision. Without the
        // D-2 fix, `sync_set` would try to repair using the stale v1 chunk hashes, get an Err from
        // `get_chunk` (the server only serves v2 hashes now), and propagate that Err — permanently,
        // since the digest never changes to trigger a normal re-sync.
        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();

        let out = std::fs::read(cache.models_dir().join("humanoid.glb")).unwrap();
        assert_eq!(out, whole, "content is identical post-recovery even though it was re-chunked");
        assert!(cache.cas_has(&blake3_hex(&whole[0..15])), "recovered via the NEW (v2) chunk ids");
    }
}

#[cfg(test)]
mod download_rate_tests {
    use super::*;
    use std::time::Duration;

    // ── pure function: download_rate_bytes_per_sec (#708 req 4: guard the near-zero divide) ──

    #[test]
    fn zero_elapsed_is_guarded_not_divided() {
        assert_eq!(download_rate_bytes_per_sec(1_048_576, Duration::ZERO), None);
    }

    #[test]
    fn near_zero_elapsed_below_threshold_is_guarded() {
        // Just under MIN_RATE_ELAPSED (100ms) — the very first tick after a phase starts can land
        // here; dividing would produce a noisy, meaningless spike instead of "not enough data yet".
        assert_eq!(download_rate_bytes_per_sec(1_048_576, Duration::from_millis(99)), None);
    }

    #[test]
    fn elapsed_at_threshold_yields_a_real_rate() {
        // At/above MIN_RATE_ELAPSED the guard lifts and the function must divide correctly.
        let rate = download_rate_bytes_per_sec(1_048_576, Duration::from_millis(100)).unwrap();
        assert!((rate - 10_485_760.0).abs() < 1.0, "expected ~10 MB/s, got {rate}");
    }

    #[test]
    fn rate_is_bytes_over_elapsed_seconds() {
        let rate = download_rate_bytes_per_sec(2_000_000, Duration::from_secs(2)).unwrap();
        assert!((rate - 1_000_000.0).abs() < 1.0, "expected 1,000,000 B/s, got {rate}");
    }

    // ── property: an all-cached sync never emits a Downloading tick (#708 req 2) ──────────────

    struct FakeTransport {
        manifest: Manifest,
        chunks: std::collections::HashMap<String, Vec<u8>>,
    }
    impl Transport for FakeTransport {
        fn get_manifest(&self, _set: &str, inm: Option<&str>) -> anyhow::Result<ManifestFetch> {
            if inm == Some(self.manifest.revision.as_str()) {
                return Ok(ManifestFetch::Unchanged);
            }
            Ok(ManifestFetch::Changed(self.manifest.clone()))
        }
        fn get_chunk(&self, hash: &str) -> anyhow::Result<Vec<u8>> {
            self.chunks.get(hash).cloned().ok_or_else(|| anyhow::anyhow!("no chunk {hash}"))
        }
    }

    fn fixture() -> FakeTransport {
        let a = vec![1u8; 10];
        let b = vec![2u8; 20];
        let (ha, hb) = (blake3_hex(&a), blake3_hex(&b));
        let mut whole = a.clone();
        whole.extend_from_slice(&b);
        let mut chunks = std::collections::HashMap::new();
        chunks.insert(ha.clone(), a);
        chunks.insert(hb.clone(), b);
        let files = vec![FileEntry {
            path: "humanoid.glb".into(), size: whole.len() as u64,
            blake3: blake3_hex(&whole), chunks: vec![ha, hb],
        }];
        FakeTransport { manifest: test_manifest("common", files), chunks }
    }

    #[test]
    fn all_chunks_already_cached_never_emits_a_downloading_tick() {
        // #708 req 2: when nothing is missing, `fetch_and_reassemble`'s download loop has zero
        // iterations — `progress` must never be called with `Downloading` at all, so a caller has
        // no bytes/elapsed to turn into a misleading "0 B/s". "Complete" and "stalled" must not be
        // representationally identical.
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let t = fixture();

        // Prime the CAS + models dir so every chunk this manifest needs is already on disk.
        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();

        // Force a second, unconditional (Changed) sync of the SAME content so `fetch_and_reassemble`
        // actually runs its download loop over `missing_chunks` — which must come back empty.
        cache.clear_synced("common");
        let mut saw_downloading = false;
        sync_set(&t, "common", &cache, &mut |p| {
            if matches!(p, SyncProgress::Downloading { .. }) {
                saw_downloading = true;
            }
        }).unwrap();
        assert!(!saw_downloading, "an all-cached sync must never emit a Downloading progress tick");
    }

    // ── property: elapsed is scoped to the CURRENT downloading phase, not global (#708 req 1/3) ──

    #[test]
    fn elapsed_does_not_carry_over_from_a_slow_prior_download() {
        // If `elapsed` were measured from process start (or any point before this phase began)
        // instead of from the start of THIS call, a slow first sync would make a fast SECOND sync's
        // very first tick already look like it had been running for a long time — a stale-rate bug
        // by another name (the rate would be computed against the wrong clock, not just the wrong
        // phase). Reproduce with two back-to-back sync_set_with_clock calls against two different
        // manifests (so the second one also has real work to do).
        //
        // #796: this used to reproduce "slow prior work" with a real `std::thread::sleep`, which
        // meant the assertion afterward ("elapsed stays under a soft bound") was really a bet on how
        // long the download loop's OWN real wall-clock work took on this run — and that bet lost
        // under remote-builder contention with nothing wrong in the code under test. There is no
        // sleep anywhere in this test now: "slow prior work" is simulated by pinning the FIRST
        // sync's `DownloadClock` far ahead of an arbitrary reference instant, and "more time passing
        // between syncs" by pinning the SECOND sync's clock further ahead still. Because each pinned
        // clock's `now()` always returns the SAME instant, a correctly phase-scoped
        // `start = clock.now()` makes every `elapsed` in that call EXACTLY `Duration::ZERO` —
        // deterministically, independent of how fast or slow the machine actually runs the loop
        // body. The property under test (fresh `start` per call) is what the assertion measures now,
        // not machine speed.
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());

        // First manifest/content.
        let a = vec![1u8; 5];
        let ha = blake3_hex(&a);
        let files1 = vec![FileEntry {
            path: "one.glb".into(), size: a.len() as u64, blake3: blake3_hex(&a), chunks: vec![ha.clone()],
        }];
        let mut chunks1 = std::collections::HashMap::new();
        chunks1.insert(ha, a);
        let t1 = FakeTransport {
            manifest: test_manifest("s1", files1),
            chunks: chunks1,
        };

        // Simulated: "10 seconds had already passed" before this sync starts — no real sleep, just
        // a clock pinned 10s ahead of an arbitrary reference instant.
        let reference = std::time::Instant::now();
        let slow_prior_pin = reference + Duration::from_secs(10);

        let mut first_elapsed = Duration::MAX; // deliberately not ZERO, so a missed tick still fails
        sync_set_with_clock(&t1, "s1", &cache, &mut |p| {
            if let SyncProgress::Downloading { elapsed, .. } = p {
                first_elapsed = elapsed;
            }
        }, DownloadClock::frozen_at(slow_prior_pin)).unwrap();
        assert_eq!(first_elapsed, Duration::ZERO,
            "first sync's own elapsed must be EXACTLY zero against a clock pinned at a single \
             instant — got {first_elapsed:?}, which can only happen if `start` was read from a \
             DIFFERENT clock reading than the tick that reported it");

        // Simulated: 5 MORE seconds pass between the two syncs. If `start` were ever hoisted to
        // something that persists across calls (a `static`/`OnceLock`, a field on some long-lived
        // context) instead of being captured fresh inside THIS call to `fetch_and_reassemble_with_clock`,
        // the leaked `start` would still be `slow_prior_pin` while `clock.now()` on the second call's
        // ticks reads `second_pin` — a `elapsed` of 5s, not zero.
        let second_pin = slow_prior_pin + Duration::from_secs(5);

        // Second, independent manifest/content — its phase starts fresh.
        let b = vec![2u8; 5];
        let hb = blake3_hex(&b);
        let files2 = vec![FileEntry {
            path: "two.glb".into(), size: b.len() as u64, blake3: blake3_hex(&b), chunks: vec![hb.clone()],
        }];
        let mut chunks2 = std::collections::HashMap::new();
        chunks2.insert(hb, b);
        let t2 = FakeTransport {
            manifest: test_manifest("s2", files2),
            chunks: chunks2,
        };

        let mut second_elapsed = Duration::MAX;
        sync_set_with_clock(&t2, "s2", &cache, &mut |p| {
            if let SyncProgress::Downloading { elapsed, .. } = p {
                second_elapsed = elapsed;
            }
        }, DownloadClock::frozen_at(second_pin)).unwrap();
        assert_eq!(second_elapsed, Duration::ZERO,
            "second sync's elapsed must be EXACTLY zero — a nonzero value would mean `start` was \
             NOT captured fresh from THIS call's own clock reading (the #708 carry-over bug this \
             test is named for), got {second_elapsed:?}");
    }
}

#[cfg(test)]
mod manifest_compatibility_tests {
    use super::*;
    use std::cell::RefCell;

    struct Source {
        manifest: Manifest,
        conditional: RefCell<Vec<Option<String>>>,
        chunks: RefCell<usize>,
        unchanged: bool,
    }
    impl Transport for Source {
        fn get_manifest(&self, _: &str, revision: Option<&str>) -> anyhow::Result<ManifestFetch> {
            self.conditional.borrow_mut().push(revision.map(str::to_owned));
            Ok(if self.unchanged { ManifestFetch::Unchanged } else { ManifestFetch::Changed(self.manifest.clone()) })
        }
        fn get_chunk(&self, _: &str) -> anyhow::Result<Vec<u8>> {
            *self.chunks.borrow_mut() += 1;
            Ok(b"asset".to_vec())
        }
    }
    fn source() -> Source {
        Source { manifest: test_manifest("common", vec![FileEntry {
            path: "fixture.glb".into(), size: 5, blake3: blake3_hex(b"asset"), chunks: vec![blake3_hex(b"asset")],
        }]), conditional: RefCell::new(vec![]), chunks: RefCell::new(0), unchanged: false }
    }

    #[test]
    fn incompatible_or_corrupt_fresh_manifest_never_changes_cache_or_requests_chunks() {
        let dir = tempfile::tempdir().unwrap(); let cache = CacheDirs::with_root(dir.path());
        let mut transport = source();
        sync_set(&transport, "common", &cache, &mut |_| {}).unwrap();
        let before = std::fs::read(cache.synced_path()).unwrap();
        for case in 0..5 {
            transport.manifest = source().manifest;
            match case {
                0 => transport.manifest.requirements.reader_version = 2,
                1 => transport.manifest.requirements.capabilities.push("future".into()),
                2 => transport.manifest.set = "other".into(),
                3 => transport.manifest.digest = "0".repeat(64),
                _ => transport.manifest.schema_version = 2,
            }
            transport.manifest.revision = crate::asset_compatibility::revision(&transport.manifest);
            *transport.chunks.borrow_mut() = 0;
            assert!(sync_set(&transport, "common", &cache, &mut |_| {}).is_err());
            assert_eq!(*transport.chunks.borrow(), 0);
            assert_eq!(std::fs::read(cache.synced_path()).unwrap(), before);
            assert_eq!(std::fs::read(cache.models_dir().join("fixture.glb")).unwrap(), b"asset");
        }
    }

    #[test]
    fn cached_other_reader_cannot_authorize_conditional_or_unsolicited_304() {
        let dir = tempfile::tempdir().unwrap(); let cache = CacheDirs::with_root(dir.path());
        let mut transport = source();
        sync_set(&transport, "common", &cache, &mut |_| {}).unwrap();
        let mut incompatible = transport.manifest.clone();
        incompatible.requirements.reader_version = 2;
        incompatible.revision = crate::asset_compatibility::revision(&incompatible);
        cache.set_synced(&incompatible);
        let before = std::fs::read(cache.synced_path()).unwrap();
        transport.unchanged = true; transport.conditional.borrow_mut().clear();
        *transport.chunks.borrow_mut() = 0;
        assert!(sync_set(&transport, "common", &cache, &mut |_| {}).is_err());
        assert_eq!(*transport.conditional.borrow(), [None]);
        assert_eq!(*transport.chunks.borrow(), 0);
        assert_eq!(std::fs::read(cache.synced_path()).unwrap(), before);
    }

    #[test]
    fn pre_envelope_warm_cache_is_revalidated_without_chunk_downloads() {
        let dir = tempfile::tempdir().unwrap(); let cache = CacheDirs::with_root(dir.path());
        let transport = source();
        sync_set(&transport, "common", &cache, &mut |_| {}).unwrap();
        std::fs::write(cache.synced_path(), serde_json::to_vec(&serde_json::json!({
            "common": {"digest":transport.manifest.digest,"files":transport.manifest.files}
        })).unwrap()).unwrap();
        transport.conditional.borrow_mut().clear(); *transport.chunks.borrow_mut() = 0;
        sync_set(&transport, "common", &cache, &mut |_| {}).unwrap();
        assert_eq!(*transport.conditional.borrow(), [None]);
        assert_eq!(*transport.chunks.borrow(), 0);
        assert_eq!(cache.synced("common").unwrap().revision, transport.manifest.revision);
    }
}

#[cfg(test)]
mod compatibility_transport_tests {
    use super::*;

    #[test]
    fn actual_transport_advertises_support_and_reports_409_requirements() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
            let mut request = Vec::new(); let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") { stream.read_exact(&mut byte).unwrap(); request.push(byte[0]); }
            let body = r#"{"reason":"asset_reader_incompatible","reader_version":2,"capabilities":["future-material"]}"#;
            write!(stream, "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
            String::from_utf8(request).unwrap().to_ascii_lowercase()
        });
        let transport = AssetSync { base: format!("http://{address}"), token: "fixture".into(),
            agent: ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(5)).build() };
        let error = transport.get_manifest("common", Some(&"a".repeat(64))).err().unwrap().to_string();
        assert!(error.contains("asset_reader_incompatible") && error.contains("future-material") && error.contains("reader_version"));
        let request = handle.join().unwrap();
        assert!(request.contains("x-eqoxide-asset-readers: 1\r\n"));
        assert!(request.contains("x-eqoxide-asset-capabilities: legacy-assets-v1\r\n"));
        assert!(request.contains(&format!("if-none-match: \"{}\"\r\n", "a".repeat(64))));
    }

    #[test]
    fn fresh_recovery_revalidates_requirements_and_preserves_old_record() {
        struct Recovery { manifest: Manifest, calls: std::cell::Cell<usize> }
        impl Transport for Recovery {
            fn get_manifest(&self, _: &str, conditional: Option<&str>) -> anyhow::Result<ManifestFetch> {
                Ok(if conditional.is_some() { ManifestFetch::Unchanged } else { ManifestFetch::Changed(self.manifest.clone()) })
            }
            fn get_chunk(&self, _: &str) -> anyhow::Result<Vec<u8>> {
                self.calls.set(self.calls.get() + 1); anyhow::bail!("old chunk unavailable")
            }
        }
        let dir = tempfile::tempdir().unwrap(); let cache = CacheDirs::with_root(dir.path());
        let mut manifest = test_manifest("common", vec![FileEntry { path: "asset.glb".into(), size: 1,
            blake3: blake3_hex(b"a"), chunks: vec![blake3_hex(b"a")] }]);
        cache.set_synced(&manifest);
        let before = std::fs::read(cache.synced_path()).unwrap();
        manifest.requirements.reader_version = 2;
        manifest.revision = crate::asset_compatibility::revision(&manifest);
        let transport = Recovery { manifest, calls: std::cell::Cell::new(0) };
        let error = sync_set(&transport, "common", &cache, &mut |_| {}).unwrap_err().to_string();
        assert!(error.contains("asset_reader_incompatible"), "{error}");
        assert_eq!(transport.calls.get(), 1, "only attempted old repair, never incompatible fresh chunks");
        assert_eq!(std::fs::read(cache.synced_path()).unwrap(), before);
        assert!(!cache.models_dir().join("asset.glb").exists());
    }
}
