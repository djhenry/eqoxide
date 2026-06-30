//! Client-side asset sync: pulls the derived `common` model set from the asset
//! server into a cwd-independent XDG cache. Pure logic here is unit-tested; the
//! HTTP transport is a trait (see UreqTransport).

use serde::Deserialize;
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

    // ── per-set synced digest (the staleness cursor) ──────────────────────────
    // Stored in the same cache root as the reassembled files, so clearing the cache clears both —
    // a recorded digest therefore always corresponds to files that are actually present.
    fn synced_path(&self) -> PathBuf {
        self.root.join("synced.json")
    }

    fn load_synced(&self) -> std::collections::HashMap<String, String> {
        std::fs::read(self.synced_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// The digest last successfully synced for `set`, if any (missing/malformed file → `None`).
    pub fn synced_digest(&self, set: &str) -> Option<String> {
        self.load_synced().get(set).cloned()
    }

    /// Record `digest` as the last-synced identity for `set` (call only after a successful sync).
    pub fn set_synced_digest(&self, set: &str, digest: &str) {
        let mut map = self.load_synced();
        map.insert(set.to_string(), digest.to_string());
        if let Ok(bytes) = serde_json::to_vec_pretty(&map) {
            let _ = std::fs::create_dir_all(&self.root);
            let _ = std::fs::write(self.synced_path(), bytes);
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub blake3: String,
    pub chunks: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Manifest {
    pub set: String,
    /// Content identity of the set (see `set_digest`). The client records the last-synced digest
    /// per set and skips a set whose digest is unchanged — correct across servers with diverging assets.
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

pub fn missing_chunks(manifest: &Manifest, cache: &CacheDirs) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in &manifest.files {
        for h in &f.chunks {
            if !cache.cas_has(h) && !out.contains(h) {
                out.push(h.clone());
            }
        }
    }
    out
}

pub fn reassemble(cache: &CacheDirs, entry: &FileEntry) -> anyhow::Result<()> {
    let mut bytes = Vec::with_capacity(entry.size as usize);
    for h in &entry.chunks {
        let chunk = cache
            .cas_get(h)
            .map_err(|e| anyhow::anyhow!("missing chunk {h} for {}: {e}", entry.path))?;
        bytes.extend_from_slice(&chunk);
    }
    let got = blake3_hex(&bytes);
    if got != entry.blake3 {
        anyhow::bail!("blake3 mismatch for {}: expected {} got {got}", entry.path, entry.blake3);
    }
    let out_path = cache.models_dir().join(&entry.path);
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out_path, &bytes)?;
    Ok(())
}

/// Result of a conditional manifest fetch. `Unchanged` (HTTP 304) means the client's stored digest
/// still matches the server, so the whole set can be skipped.
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
            .set("Authorization", &format!("Bearer {}", self.token));
        if let Some(d) = if_none_match {
            req = req.set("If-None-Match", &format!("\"{d}\""));
        }
        // ureq returns 2xx/3xx as Ok and 4xx/5xx as Err(Status). 304 can surface either way
        // depending on version — handle both.
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(304, _)) => return Ok(ManifestFetch::Unchanged),
            Err(e) => return Err(anyhow::anyhow!("manifest {set} failed: {e}")),
        };
        if resp.status() == 304 {
            return Ok(ManifestFetch::Unchanged);
        }
        Ok(ManifestFetch::Changed(resp.into_json()?))
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

pub enum Phase {
    Verifying,
    Downloading,
}

pub struct SyncProgress {
    pub phase: Phase,
    pub done: usize,
    pub total: usize,
    pub bytes: u64,
}

pub fn sync_set(
    t: &dyn Transport,
    set: &str,
    cache: &CacheDirs,
    progress: &mut dyn FnMut(SyncProgress),
) -> anyhow::Result<()> {
    let prev = cache.synced_digest(set);
    let manifest = match t.get_manifest(set, prev.as_deref())? {
        ManifestFetch::Unchanged => return Ok(()), // identical to what we already have — skip
        ManifestFetch::Changed(m) => m,
    };
    // Defense against a lying/corrupt server: the manifest must hash to its claimed digest.
    let recomputed = set_digest(&manifest.files);
    if recomputed != manifest.digest {
        anyhow::bail!("manifest digest mismatch for {set}: claimed {} got {recomputed}", manifest.digest);
    }
    progress(SyncProgress { phase: Phase::Verifying, done: 0, total: 0, bytes: 0 });

    let missing = missing_chunks(&manifest, cache);
    let total = missing.len();
    let mut bytes = 0u64;
    for (i, hash) in missing.iter().enumerate() {
        let data = t.get_chunk(hash)?;
        // The server is content-addressed: a chunk's bytes must hash to its id.
        let got = blake3_hex(&data);
        if &got != hash {
            anyhow::bail!("chunk {hash} content mismatch (got {got})");
        }
        cache.cas_put(&data)?;
        bytes += data.len() as u64;
        progress(SyncProgress { phase: Phase::Downloading, done: i + 1, total, bytes });
    }

    for entry in &manifest.files {
        reassemble(cache, entry)?;
    }
    // Record the synced identity so a future unchanged fetch (304) can skip the whole set.
    cache.set_synced_digest(set, &manifest.digest);
    Ok(())
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
        let m = Manifest { set: "common".into(), digest: set_digest(&files), files };
        (m, whole)
    }

    #[test]
    fn missing_chunks_lists_only_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let (m, _) = manifest_with(&cache);
        // both chunks were just put -> nothing missing
        assert!(missing_chunks(&m, &cache).is_empty());

        // a manifest referencing an absent chunk
        let mut m2 = m.clone();
        m2.files[0].chunks.push("absenthash".into());
        assert_eq!(missing_chunks(&m2, &cache), vec!["absenthash".to_string()]);
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
    fn reassemble_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let (mut m, _) = manifest_with(&cache);
        m.files[0].blake3 = "0".repeat(64); // wrong expected hash
        assert!(reassemble(&cache, &m.files[0]).is_err());
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
    }
    impl Transport for FakeTransport {
        // Mirrors the real server: 304 (Unchanged) when the client's If-None-Match equals the digest.
        fn get_manifest(&self, _set: &str, inm: Option<&str>) -> anyhow::Result<ManifestFetch> {
            if inm == Some(self.manifest.digest.as_str()) {
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
            manifest: Manifest { set: "common".into(), digest: set_digest(&files), files },
            chunks,
            chunk_calls: std::cell::RefCell::new(0),
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
        c.set_synced_digest("zone/qeynos", "abc123");
        c.set_synced_digest("gamedata", "def456");
        assert_eq!(c.synced_digest("zone/qeynos").as_deref(), Some("abc123"));
        assert_eq!(c.synced_digest("gamedata").as_deref(), Some("def456"));
    }

    #[test]
    fn unchanged_after_first_sync_skips() {
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let t = fixture();
        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
        assert_eq!(cache.synced_digest("common").as_deref(), Some(t.manifest.digest.as_str()));
        // Delete the reassembled file; an Unchanged (304) sync must NOT touch it (true skip).
        std::fs::remove_file(cache.models_dir().join("humanoid.glb")).unwrap();
        let before = *t.chunk_calls.borrow();
        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
        assert_eq!(*t.chunk_calls.borrow(), before, "Unchanged must not fetch chunks");
        assert!(!cache.models_dir().join("humanoid.glb").exists(), "Unchanged must skip reassembly");
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
        sync_set(&t, "common", &cache, &mut |p| last = Some((p.done, p.total))).unwrap();
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
}
