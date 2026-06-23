//! Client-side asset sync: pulls the derived `common` model set from the asset
//! server into a cwd-independent XDG cache. Pure logic here is unit-tested; the
//! HTTP transport is a trait (see UreqTransport).

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
