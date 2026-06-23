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
            .join("eq_client_lite")
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
    pub version: u64,
    pub files: Vec<FileEntry>,
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

pub trait Transport {
    fn get_manifest(&self, set: &str) -> anyhow::Result<Manifest>;
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
    fn get_manifest(&self, set: &str) -> anyhow::Result<Manifest> {
        let m: Manifest = self
            .agent
            .get(&format!("{}/manifest/{set}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .call()
            .map_err(|e| anyhow::anyhow!("manifest {set} failed: {e}"))?
            .into_json()?;
        Ok(m)
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
    let manifest = t.get_manifest(set)?;
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
        assert!(c.root.ends_with("eq_client_lite/assets"));
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
        let m = Manifest {
            set: "common".into(),
            version: 1,
            files: vec![FileEntry {
                path: "humanoid.glb".into(),
                size: whole.len() as u64,
                blake3: blake3_hex(&whole),
                chunks: vec![ha, hb],
            }],
        };
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
        fn get_manifest(&self, _set: &str) -> anyhow::Result<Manifest> {
            Ok(self.manifest.clone())
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
        FakeTransport {
            manifest: Manifest {
                set: "common".into(), version: 1,
                files: vec![FileEntry {
                    path: "humanoid.glb".into(), size: whole.len() as u64,
                    blake3: blake3_hex(&whole), chunks: vec![ha, hb],
                }],
            },
            chunks,
            chunk_calls: std::cell::RefCell::new(0),
        }
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
