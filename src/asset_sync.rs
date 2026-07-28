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

    /// Migration note (#601 D-4): `SyncedSet` gained a `files` field alongside the pre-existing
    /// `digest`. An old-format `synced.json` (written before this change, `files` absent) fails
    /// `serde_json::from_slice` — `files: Vec<FileEntry>` has no `#[serde(default)]`, so a missing
    /// field is a hard deserialize error, not a partial load. That error is swallowed by `.ok()`
    /// below, so the outcome is: every existing record is silently DISCARDED (not trusted, not
    /// fatal) and `load_synced` returns an empty map, exactly as if the cache were cold. The next
    /// sync for every previously-synced set therefore sends no `If-None-Match`, costing one
    /// unconditional manifest fetch per set — but zero extra chunk downloads, since `cas_put` is
    /// idempotent on hashes already on disk and `missing_chunks` finds them all still present.
    /// One re-verification pass on first launch after upgrade, then the new-format record takes
    /// over and subsequent launches short-circuit exactly as before. See
    /// `old_format_synced_json_is_discarded_not_trusted_or_fatal` and
    /// `warm_cache_upgrade_costs_no_chunk_refetch` for the pinned behavior.
    fn load_synced(&self) -> std::collections::HashMap<String, SyncedSet> {
        std::fs::read(self.synced_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// The digest last successfully synced for `set`, if any (missing/malformed file → `None`).
    pub fn synced_digest(&self, set: &str) -> Option<String> {
        self.load_synced().get(set).map(|s| s.digest.clone())
    }

    /// The digest and file list recorded at the last successful sync for `set`, if any. The file
    /// list is what lets `sync_set` verify a server-reported "unchanged" against the real
    /// contents of `models/` instead of trusting the digest blindly.
    fn synced(&self, set: &str) -> Option<(String, Vec<FileEntry>)> {
        self.load_synced().get(set).map(|s| (s.digest.clone(), s.files.clone()))
    }

    /// Record `digest` and `files` as the last-synced identity for `set` (call only after a
    /// successful sync or repair).
    fn set_synced(&self, set: &str, digest: &str, files: &[FileEntry]) {
        let mut map = self.load_synced();
        map.insert(
            set.to_string(),
            SyncedSet { digest: digest.to_string(), files: files.to_vec() },
        );
        if let Ok(bytes) = serde_json::to_vec_pretty(&map) {
            let _ = std::fs::create_dir_all(&self.root);
            let _ = std::fs::write(self.synced_path(), bytes);
        }
    }

    /// Drop the recorded synced identity for `set`, forcing the next sync to treat it as never
    /// synced (unconditional manifest fetch, no `If-None-Match`). Used when a recorded file list
    /// turns out to be unrepairable — e.g. the server has since re-chunked the same content under
    /// different chunk ids, which the digest alone can't detect (#601 D-2) — so the only way
    /// forward is to forget the stale record and re-derive everything from a fresh manifest.
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

/// On-disk record for one synced set: the server digest it corresponds to, plus the file list
/// used to verify (and, if needed, rebuild) the assembled artifacts on a later "unchanged" fetch.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct SyncedSet {
    digest: String,
    files: Vec<FileEntry>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
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

/// Downloads whatever chunks in `files` aren't already in the CAS, then reassembles every file.
/// Shared by both the normal (server digest Changed) sync path and the repair path taken when an
/// unchanged digest's own artifacts turn out to be missing/corrupt (#601).
fn fetch_and_reassemble(
    t: &dyn Transport,
    cache: &CacheDirs,
    files: &[FileEntry],
    progress: &mut dyn FnMut(SyncProgress),
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
    let start = std::time::Instant::now();
    for (i, hash) in missing.iter().enumerate() {
        let data = t.get_chunk(hash)?;
        // The server is content-addressed: a chunk's bytes must hash to its id.
        let got = blake3_hex(&data);
        if &got != hash {
            anyhow::bail!("chunk {hash} content mismatch (got {got})");
        }
        cache.cas_put(&data)?;
        bytes += data.len() as u64;
        progress(SyncProgress::Downloading { done: i + 1, total, bytes, elapsed: start.elapsed() });
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
    /// **Private on purpose (#731), for the reason `sync_set` is (#726 review N3).** [`login_observed`]
    /// is the only way in.
    ///
    /// This call was the last unobserved blocking step in the asset pipeline: it precedes every
    /// `sync_set`, it does a full HTTP round trip to `/auth`, and while it was blocked
    /// `GET /v1/observe/asset_sync` answered `{"active": false}` — "no asset sync is running" — to an
    /// agent that has no other channel to reality. Leaving it public and merely *documenting* that
    /// callers should wrap it would put the property back where #726 found it: enforced by the
    /// author's care, and quietly lost by the next call site added.
    fn login(base: &str, username: &str, password: &str) -> anyhow::Result<Self> {
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

/// The #708 rate helper, unchanged in behavior but now DEFINED in `eqoxide-ipc` and re-exported
/// here (#715), so every existing `crate::asset_sync::download_rate_bytes_per_sec(…)` call site —
/// both HUD loaders in `src/app.rs`, and this module's own `download_rate_tests` — resolves exactly
/// as before.
///
/// Why it moved down rather than being copied: `GET /v1/observe/asset_sync` serves the same rate to
/// an agent, and `eqoxide-http` sits BELOW this crate and cannot call up into it. A second copy of
/// the divide (and of its 100 ms `MIN_RATE_ELAPSED` guard) in the HTTP layer is exactly the kind of
/// duplicate that drifts: the screen and the API would eventually disagree about what "the download
/// rate" means. One definition, two readers.
pub use crate::ipc::asset_sync::download_rate_bytes_per_sec;

/// **Private on purpose (#726 review N3).** [`sync_set_observed`] is the only way in.
///
/// The review's mutation M11 reverted two of the four call sites from `sync_set_observed` back to
/// this function and the entire 1442-test suite still passed: the "every in-session sync is
/// observed" property — the one that makes `active: false` mean anything at all — was enforced by
/// nothing but the author's care, and the next `sync_set` added anywhere in `app.rs` would have
/// reintroduced the falsehood silently. This project's verification hierarchy puts "make the bad
/// state unrepresentable" above any test, so an unobserved sync is now a compile error rather than
/// a thing to remember. There is no `Option<&AssetSyncShared>` escape hatch either: the registry is
/// constructed before the very first sync in `main.rs`, so every caller has one to pass.
fn sync_set(
    t: &dyn Transport,
    set: &str,
    cache: &CacheDirs,
    progress: &mut dyn FnMut(SyncProgress),
) -> anyhow::Result<()> {
    let prev = cache.synced(set);
    // Only offer the server our digest when we also still have the file list it refers to — that
    // list is what lets an "Unchanged" reply be checked locally. (In practice the two are always
    // recorded together, so this is really just "do we have a prior sync at all", but it also
    // gracefully re-syncs once if the on-disk record ever fails to parse.)
    let if_none_match = prev.as_ref().map(|(d, _)| d.as_str());

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
            let Some((_, files)) = prev else {
                anyhow::bail!(
                    "server reported set {set} as unchanged but we hold no prior synced record \
                     for it (no If-None-Match was sent) — protocol violation by the server or an \
                     intermediary, cannot verify or repair"
                );
            };
            if artifacts_intact(cache, &files) {
                return Ok(()); // content AND local artifacts both unchanged — genuine no-op
            }
            progress(SyncProgress::Verifying);
            if let Err(repair_err) = fetch_and_reassemble(t, cache, &files, progress) {
                // The recorded chunk list can itself be stale even though the digest is not: the
                // digest (`set_digest`) hashes only `path\0<file-blake3>`, never chunk ids, so a
                // server that re-chunks a file (different FastCDC params, a re-bake, ...) while
                // its *content* is unchanged produces an identical digest with entirely different
                // chunk hashes. Retrying the SAME recorded chunk list would then fail on every
                // future launch, permanently (#601 D-2). Drop the stale record and fall through to
                // one fresh, unconditional manifest fetch — the server's CURRENT chunk ids — rather
                // than keep repairing with data the server can no longer serve.
                cache.clear_synced(set);
                return match t.get_manifest(set, None)? {
                    ManifestFetch::Changed(m) => sync_changed_manifest(t, set, cache, m, progress),
                    ManifestFetch::Unchanged => Err(repair_err.context(format!(
                        "repair of set {set} failed, and a fresh (unconditional) manifest fetch \
                         still reports Unchanged — server cannot supply working chunk data"
                    ))),
                };
            }
            Ok(()) // digest/file-list unchanged from what's already recorded — nothing new to persist
        }
        ManifestFetch::Changed(m) => sync_changed_manifest(t, set, cache, m, progress),
    }
}

/// Run a `sync_set`, with the agent-observable activity (#715) published for the whole call.
///
/// This is the ONLY way to run a sync — `sync_set` itself is private (#726 review N3) — and the
/// ONLY writer of [`crate::ipc::AssetSyncShared`]. It exists so the observable window is bounded by
/// an RAII guard rather than by remembering to clear at each return: this sync's entry leaves the
/// registry on the success return, on the error return, and on a panic unwinding out of the calling
/// loader thread, and the guard can only ever remove ITS OWN entry, so a short sync nested inside a
/// long one cannot blank the long one on its way out (#726 review finding 1). The alternative —
/// publishing progress with no clear-on-completion — turns `/v1/observe/asset_sync` into a
/// confident report of a sync that finished long ago, which is a worse answer than the nothing it
/// replaced.
///
/// `progress` is still called for every producer tick, unchanged, so the existing loading-screen
/// status lines are unaffected: this wrapper only ADDS an observer.
pub fn sync_set_observed(
    t: &dyn Transport,
    set: &str,
    cache: &CacheDirs,
    observable: &crate::ipc::AssetSyncShared,
    progress: &mut dyn FnMut(SyncProgress),
) -> anyhow::Result<()> {
    let guard = crate::ipc::AssetSyncGuard::begin(observable, set);
    sync_set(t, set, cache, &mut |p| {
        // Derived from the tick BEFORE it is forwarded, so the observable phase can never disagree
        // with what the HUD's callback is about to be handed.
        guard.tick(match &p {
            SyncProgress::Verifying => crate::ipc::AssetSyncPhase::Verifying,
            SyncProgress::Downloading { done, total, bytes, elapsed } =>
                crate::ipc::AssetSyncPhase::Downloading {
                    chunks_done: *done, chunks_total: *total, bytes: *bytes, elapsed: *elapsed,
                },
        });
        progress(p);
    })
    // `guard` drops here — on BOTH arms of the result.
}

/// Log in to the asset server, with the login itself agent-observable for its whole duration (#731).
///
/// This is the only way to log in from **outside this module** — [`AssetSync::login`] is private to
/// `src/asset_sync.rs` — for the same reason [`sync_set_observed`] is the only way to sync: the
/// property that `active: false` means "nothing is running" is worth nothing if any new call site
/// can silently opt out of it. All four production login sites are elsewhere (`src/main.rs`,
/// `src/app.rs`), so for them the compiler enforces it: an unobserved login does not compile.
///
/// **The limit, stated rather than implied (#743 review N5):** privacy here is module-scoped, not
/// crate-scoped. Code added inside this file — including its own tests — can still call
/// `AssetSync::login` directly and bypass the registry. The guarantee is "no *other* module can",
/// which is what actually covers the call sites; it is not "nothing can".
///
/// `purpose` is free text describing what this login is for (`"zone load: qeynos2"`,
/// `"common asset load"`). It is deliberately **not** a set name and deliberately not set-shaped:
/// three of this client's four logins serve more than one set — the model-sync worker logs in once
/// for an unbounded queue of `charmodel/<key>` sets, startup logs in once for `gamedata` *and*
/// `gameequip`, and the zone loader's login covers both `zone/<z>` and `zonedoors/<z>` — so there is
/// no set to name, and a set-shaped label would be found by a caller looking that set up in `syncs`
/// and read as a transfer that had begun. That is why #731's suggested shape (an
/// `AssetSyncPhase::Connecting` on an entry that has a `set`, with one guard spanning login and
/// sync) was not taken: it does not generalise past the one 1:1 call site.
///
/// The outcome is recorded from the `Result` this function can see, so a caller polling after the
/// entry is gone can still tell a failed login from a successful one — otherwise both are the same
/// `active: false` and #731's falsehood simply moves a moment later.
pub fn login_observed(
    base: &str,
    username: &str,
    password: &str,
    observable: &crate::ipc::AssetSyncShared,
    purpose: &str,
) -> anyhow::Result<AssetSync> {
    let guard = crate::ipc::AssetConnectGuard::begin(observable, purpose);
    let r = AssetSync::login(base, username, password);
    // `finish` consumes the guard, so the entry is removed by the `Drop` that follows — and if this
    // line is never reached (a panic unwinding out of `login`) the same `Drop` still removes it,
    // with the outcome left at `Unknown` rather than defaulted to a real answer.
    guard.finish(if r.is_ok() {
        crate::ipc::ConnectOutcome::Succeeded
    } else {
        crate::ipc::ConnectOutcome::Failed
    });
    r
}

/// Verifies a freshly-fetched manifest against its own claimed digest, downloads/reassembles its
/// files, and records the new synced state. Shared by the normal (digest Changed) path and the
/// repair-failure fallback above (#601 D-2), both of which end up with a manifest they need to
/// fully (re)apply.
fn sync_changed_manifest(
    t: &dyn Transport,
    set: &str,
    cache: &CacheDirs,
    manifest: Manifest,
    progress: &mut dyn FnMut(SyncProgress),
) -> anyhow::Result<()> {
    // Defense against a lying/corrupt server: the manifest must hash to its claimed digest.
    let recomputed = set_digest(&manifest.files);
    if recomputed != manifest.digest {
        anyhow::bail!("manifest digest mismatch for {set}: claimed {} got {recomputed}", manifest.digest);
    }
    progress(SyncProgress::Verifying);

    fetch_and_reassemble(t, cache, &manifest.files, progress)?;

    // Record the synced identity so a future unchanged fetch (304) can skip the whole set.
    cache.set_synced(set, &manifest.digest, &manifest.files);
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
        c.set_synced("zone/qeynos", "abc123", &[]);
        c.set_synced("gamedata", "def456", &[]);
        assert_eq!(c.synced_digest("zone/qeynos").as_deref(), Some("abc123"));
        assert_eq!(c.synced_digest("gamedata").as_deref(), Some("def456"));
    }

    #[test]
    fn old_format_synced_json_is_discarded_not_trusted_or_fatal() {
        // Before #601, `synced.json` stored `{set: "digest-string"}` — every dev box and jimbo
        // already have files in that shape on disk. The new format is `{set: {digest, files}}`.
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
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let t = fixture();

        // Establish a fully-synced, new-format state first (this is the pre-upgrade state).
        sync_set(&t, "common", &cache, &mut |_| {}).unwrap();
        assert_eq!(*t.chunk_calls.borrow(), 2);

        // Simulate what an upgrade finds on disk: overwrite the record with the pre-#601 shape,
        // leaving the CAS and the assembled artifact untouched — a real upgrade changes only the
        // code reading synced.json, never the CAS or models/ contents.
        std::fs::write(
            cache.root.join("synced.json"),
            format!(r#"{{"common": "{}"}}"#, t.manifest.digest),
        )
        .unwrap();

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
        // The fix: on repair failure, drop the stale record and fall through to one fresh,
        // unconditional manifest fetch (current chunk ids), then fully apply that.
        use std::cell::Cell;

        struct RechunkingTransport {
            digest: String,
            v1_files: Vec<FileEntry>,
            v2_files: Vec<FileEntry>,
            v1_chunks: HashMap<String, Vec<u8>>,
            v2_chunks: HashMap<String, Vec<u8>>,
            rechunked: Cell<bool>,
        }
        impl Transport for RechunkingTransport {
            fn get_manifest(&self, _set: &str, inm: Option<&str>) -> anyhow::Result<ManifestFetch> {
                if inm == Some(self.digest.as_str()) {
                    return Ok(ManifestFetch::Unchanged);
                }
                let files = if self.rechunked.get() { self.v2_files.clone() } else { self.v1_files.clone() };
                Ok(ManifestFetch::Changed(Manifest { set: "common".into(), digest: self.digest.clone(), files }))
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
            digest: digest_v1,
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

        // The digest is unchanged, so the server still (correctly) reports Unchanged. Without the
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
            if inm == Some(self.manifest.digest.as_str()) {
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
        FakeTransport { manifest: Manifest { set: "common".into(), digest: set_digest(&files), files }, chunks }
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
        // phase). Reproduce with two back-to-back sync_set calls against two different manifests
        // (so the second one also has real work to do), inserting a real sleep before the first
        // call begins so any global/leaked clock would show up as inflated elapsed on the second.
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
            manifest: Manifest { set: "s1".into(), digest: set_digest(&files1), files: files1 },
            chunks: chunks1,
        };

        std::thread::sleep(Duration::from_millis(150)); // simulate time passing before ANY sync runs

        let mut first_elapsed = Duration::ZERO;
        sync_set(&t1, "s1", &cache, &mut |p| {
            if let SyncProgress::Downloading { elapsed, .. } = p {
                first_elapsed = elapsed;
            }
        }).unwrap();
        assert!(first_elapsed < Duration::from_millis(100),
            "first sync's own elapsed should be small (just its own download loop), got {first_elapsed:?}");

        // A SECOND sleep, between the two syncs, is the actual discriminator: if `start` were ever
        // hoisted to something that persists across calls (a `static`/`OnceLock`, a field on some
        // long-lived context) instead of being captured fresh inside THIS call to
        // `fetch_and_reassemble`, this sleep would show up added into the second sync's `elapsed`
        // even though the second sync's own download loop never ran during it.
        std::thread::sleep(Duration::from_millis(150));

        // Second, independent manifest/content — its phase starts fresh.
        let b = vec![2u8; 5];
        let hb = blake3_hex(&b);
        let files2 = vec![FileEntry {
            path: "two.glb".into(), size: b.len() as u64, blake3: blake3_hex(&b), chunks: vec![hb.clone()],
        }];
        let mut chunks2 = std::collections::HashMap::new();
        chunks2.insert(hb, b);
        let t2 = FakeTransport {
            manifest: Manifest { set: "s2".into(), digest: set_digest(&files2), files: files2 },
            chunks: chunks2,
        };

        let mut second_elapsed = Duration::ZERO;
        sync_set(&t2, "s2", &cache, &mut |p| {
            if let SyncProgress::Downloading { elapsed, .. } = p {
                second_elapsed = elapsed;
            }
        }).unwrap();
        assert!(second_elapsed < Duration::from_millis(100),
            "second sync's elapsed must NOT carry over the 300ms slept before/between syncs — got \
             {second_elapsed:?}, which would mean the clock is global/shared, not phase-scoped");
    }
}

/// #715 — `sync_set_observed`'s observable-activity contract, exercised through the REAL
/// `sync_set` against a fake transport. These are the "does the endpoint's data source have a live
/// writer, and does it get CLEARED" tests: an observable that is published but never cleared turns
/// `GET /v1/observe/asset_sync` into a confident report of a sync that finished long ago, which is
/// worse than the nothing it replaced.
#[cfg(test)]
mod observed_sync_tests {
    use super::*;
    use crate::ipc::{AssetSyncPhase, AssetSyncShared};
    use std::collections::HashMap;

    struct FakeTransport {
        manifest: Manifest,
        chunks: HashMap<String, Vec<u8>>,
    }
    impl Transport for FakeTransport {
        fn get_manifest(&self, _set: &str, _inm: Option<&str>) -> anyhow::Result<ManifestFetch> {
            Ok(ManifestFetch::Changed(self.manifest.clone()))
        }
        fn get_chunk(&self, hash: &str) -> anyhow::Result<Vec<u8>> {
            self.chunks.get(hash).cloned().ok_or_else(|| anyhow::anyhow!("no chunk {hash}"))
        }
    }

    /// A transport whose manifest never hashes to its claimed digest, so `sync_set` bails with an
    /// ordinary `Err` — the failure path.
    struct FailingTransport;
    impl Transport for FailingTransport {
        fn get_manifest(&self, _set: &str, _inm: Option<&str>) -> anyhow::Result<ManifestFetch> {
            Ok(ManifestFetch::Changed(Manifest {
                set: "zone/qeynos2".into(),
                digest: "0".repeat(64), // deliberately does not match `files`
                files: vec![FileEntry {
                    path: "qeynos2.glb".into(), size: 1, blake3: blake3_hex(&[7u8]), chunks: vec![],
                }],
            }))
        }
        fn get_chunk(&self, _hash: &str) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("never called")
        }
    }

    fn fixture() -> FakeTransport {
        let a = vec![1u8; 10];
        let b = vec![2u8; 20];
        let (ha, hb) = (blake3_hex(&a), blake3_hex(&b));
        let mut whole = a.clone();
        whole.extend_from_slice(&b);
        let mut chunks = HashMap::new();
        chunks.insert(ha.clone(), a);
        chunks.insert(hb.clone(), b);
        let files = vec![FileEntry {
            path: "qeynos2.glb".into(), size: whole.len() as u64,
            blake3: blake3_hex(&whole), chunks: vec![ha, hb],
        }];
        FakeTransport {
            manifest: Manifest { set: "zone/qeynos2".into(), digest: set_digest(&files), files },
            chunks,
        }
    }

    fn slot() -> AssetSyncShared {
        crate::ipc::asset_sync::new_shared()
    }

    /// The sets currently reported as in flight, oldest-started first — i.e. what
    /// `GET /v1/observe/asset_sync` would list at this instant.
    fn live_sets(obs: &AssetSyncShared) -> Vec<String> {
        crate::ipc::asset_sync::snapshot(obs).live.iter()
            .filter_map(|a| a.set().map(str::to_string)).collect()
    }

    #[test]
    fn a_running_sync_is_observable_live_and_names_its_set() {
        // The producer must have a LIVE writer, not one that publishes a single value at the end:
        // the whole point is watching a load PROGRESS. Sampled from inside the progress callback,
        // i.e. at the moments the real HUD is being updated.
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let obs = slot();
        let seen: std::cell::RefCell<Vec<crate::ipc::AssetSyncActivity>> = Default::default();

        sync_set_observed(&fixture(), "zone/qeynos2", &cache, &obs, &mut |_p| {
            seen.borrow_mut().extend(crate::ipc::asset_sync::snapshot(&obs).live);
        }).unwrap();

        let seen = seen.into_inner();
        assert!(!seen.is_empty(), "the slot must be populated WHILE the sync runs, not only after");
        assert!(seen.iter().all(|a| a.set() == Some("zone/qeynos2")),
            "every sample must name the set it describes: {seen:?}");
        assert!(seen.iter().any(|a| matches!(a.phase(), Some(AssetSyncPhase::Downloading { .. }))),
            "a cold sync downloads chunks — that phase must be observable: {seen:?}");
        let last_dl = seen.iter().rev()
            .find_map(|a| match a.phase() {
                Some(&AssetSyncPhase::Downloading { chunks_done, chunks_total, .. }) => Some((chunks_done, chunks_total)),
                _ => None,
            })
            .expect("a downloading sample");
        assert_eq!(last_dl, (2, 2), "chunk counts must be the producer's own, not invented");
    }

    #[test]
    // #743: capitalised on purpose — SUCCEEDS vs FAILS are the two exit paths that must BOTH clear
    // the slot, and the pair of shouted names is what makes the second test obviously not a
    // duplicate of the first. Function-scoped like #730/#738's two `movement.rs` allows, never
    // module- or crate-scoped: a wide allow would silently tolerate accidental `non_snake_case`
    // anywhere in this 1500-line test module. (Warning pre-dates this PR — introduced by #715 in
    // this PR's own merge base, and disclosed as unattributed in #738's body.)
    #[allow(non_snake_case)]
    fn the_slot_is_cleared_when_a_sync_SUCCEEDS() {
        // Transition OUT of the phase, not into it. Without this, a finished load keeps reading as
        // an in-flight one forever and an agent waits on a download that is long over.
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let obs = slot();
        sync_set_observed(&fixture(), "zone/qeynos2", &cache, &obs, &mut |_| {}).unwrap();
        assert!(live_sets(&obs).is_empty(),
            "a COMPLETED sync must read as 'no sync in progress', not as its last phase");
    }

    #[test]
    // #743: capitalised on purpose — the ERROR return is a separate code path from the success
    // return, and it is the one a hand-written "clear at the end of the happy path" would miss.
    // Function-scoped for the same reason as its sibling above.
    #[allow(non_snake_case)]
    fn the_slot_is_cleared_when_a_sync_FAILS() {
        // The error return is a separate code path from the success return, and it is the one a
        // hand-written "clear at the end of the happy path" would miss. A failed sync frozen on
        // 'downloading' is an agent waiting forever for something that already gave up.
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let obs = slot();
        let r = sync_set_observed(&FailingTransport, "zone/qeynos2", &cache, &obs, &mut |_| {});
        assert!(r.is_err(), "the fixture must actually fail (digest mismatch)");
        assert!(live_sets(&obs).is_empty(),
            "a FAILED sync must read as 'no sync in progress', not as its last phase");
    }

    #[test]
    fn the_window_before_the_first_producer_tick_is_observable_rather_than_reading_as_idle() {
        // `sync_set` does a manifest round trip — which CAN hang — before it emits any progress
        // tick. If the slot were written only from producer ticks, that whole stretch would read
        // as 'no sync in progress' while one demonstrably was. Sampled from inside `get_manifest`,
        // i.e. exactly in that window.
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let obs = slot();

        let sampled: std::cell::RefCell<Option<crate::ipc::AssetSyncActivity>> = Default::default();
        struct Warm<'a> { inner: FakeTransport, sample: &'a dyn Fn() }
        impl Transport for Warm<'_> {
            fn get_manifest(&self, set: &str, inm: Option<&str>) -> anyhow::Result<ManifestFetch> {
                (self.sample)();   // observed from INSIDE the call, as an HTTP reader would be
                self.inner.get_manifest(set, inm)
            }
            fn get_chunk(&self, hash: &str) -> anyhow::Result<Vec<u8>> { self.inner.get_chunk(hash) }
        }
        let sample = || {
            *sampled.borrow_mut() = crate::ipc::asset_sync::snapshot(&obs).live.into_iter().next();
        };
        let warm = Warm { inner: fixture(), sample: &sample };
        sync_set_observed(&warm, "zone/qeynos2", &cache, &obs, &mut |_| {}).unwrap();

        let a = sampled.into_inner()
            .expect("a sync in flight must be observable even before its first progress tick");
        assert_eq!(a.set(), Some("zone/qeynos2"));
        assert_eq!(a.phase(), Some(&AssetSyncPhase::Starting));
        assert!(live_sets(&obs).is_empty(), "…and cleared once it returns");
    }

    #[test]
    fn a_previous_zones_loader_finishing_does_not_erase_the_current_zones_progress() {
        // Zone change: the old zone's loader keeps running and finishes at an arbitrary later
        // point. An unconditional clear there would blank out the CURRENT zone's live progress and
        // report 'no sync in progress' for the very load the agent is waiting on.
        let dir = tempfile::tempdir().unwrap();
        let cache = CacheDirs::with_root(dir.path());
        let obs = slot();
        // The OLD zone's sync is running; partway through, the NEW zone's loader starts too. Then
        // the old one returns while the new one is still in flight.
        let newer: std::cell::RefCell<Option<crate::ipc::AssetSyncGuard>> = Default::default();
        sync_set_observed(&fixture(), "zone/qeynos2", &cache, &obs, &mut |p| {
            if let SyncProgress::Downloading { done, total, .. } = p {
                if done == total && newer.borrow().is_none() {
                    *newer.borrow_mut() =
                        Some(crate::ipc::AssetSyncGuard::begin(&obs, "zone/freportw"));
                }
            }
        }).unwrap();
        assert_eq!(live_sets(&obs), ["zone/freportw"],
            "the finishing loader must only remove ITS OWN activity");
    }

    /// #726 review finding 1, through the REAL wrapper rather than a hand-built guard — the
    /// converse of the test above and the one the client runs routinely. The model-sync worker's
    /// short `charmodel/<key>` sync begins AND ends inside the zone loader's long `zone/<zone>`
    /// download. Under the old set-scoped single slot the nested sync's `Drop` saw its own set
    /// published, cleared, and `GET /v1/observe/asset_sync` answered `active: false` for the rest
    /// of a 31 MB zone download — and forever, if that download was wedged.
    #[test]
    fn reviewer_a_nested_charmodel_sync_reports_idle_while_the_zone_sync_still_runs() {
        let outer_dir = tempfile::tempdir().unwrap();
        let inner_dir = tempfile::tempdir().unwrap();
        let (outer_cache, inner_cache) =
            (CacheDirs::with_root(outer_dir.path()), CacheDirs::with_root(inner_dir.path()));
        let obs = slot();

        // Sampled from INSIDE the outer sync, immediately after the nested one has fully returned —
        // exactly where an HTTP poll would land during a real zone load.
        let after_nested: std::cell::RefCell<Option<Vec<String>>> = Default::default();
        let mut ran_nested = false;
        sync_set_observed(&fixture(), "zone/neriakc", &outer_cache, &obs, &mut |_p| {
            if !ran_nested {
                ran_nested = true;
                sync_set_observed(&fixture(), "charmodel/hum", &inner_cache, &obs, &mut |_| {})
                    .expect("the nested model sync must complete");
                *after_nested.borrow_mut() = Some(live_sets(&obs));
            }
        }).unwrap();

        let live = after_nested.into_inner().expect("the nested sync must actually have run");
        assert!(live.iter().any(|s| s == "zone/neriakc"),
            "GET /v1/observe/asset_sync answered 'active:false' while zone/neriakc was \
             mid-download — live sets were {live:?}");
        assert!(!live.iter().any(|s| s == "charmodel/hum"),
            "…and the nested sync, which really is over, must NOT still be listed: {live:?}");
    }

    // ── #731: the LOGIN window, through the real `login_observed` against a real socket ──────────

    /// Accept one connection, read the request, and hand the caller the stream WITHOUT answering —
    /// so the client is genuinely blocked inside `AssetSync::login()`, in a real `ureq` HTTP call on
    /// a real TCP socket. This is the live failure mode (a slow/hung/unreachable asset server)
    /// reproduced in-process, not a mock of the registry.
    fn hung_asset_server() -> (String, std::net::TcpListener) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let url = format!("http://{}", l.local_addr().unwrap());
        (url, l)
    }

    /// #731, end to end. Before the fix this loop could never succeed: the registry stayed EMPTY
    /// for the whole blocked login and `GET /v1/observe/asset_sync` answered `{"active": false}`
    /// while the loader thread sat inside the `/auth` round trip.
    #[test]
    fn a_login_blocked_on_a_real_socket_is_observable_and_then_clears_with_its_verdict() {
        use std::io::Read;
        let (url, listener) = hung_asset_server();
        let obs = slot();
        let obs_for_thread = obs.clone();
        let h = std::thread::spawn(move || {
            login_observed(&url, "u", "p", &obs_for_thread, "zone load: qeynos2")
        });

        // The server accepts and reads the POST /auth, then says nothing. The client is now blocked.
        let (mut stream, _) = listener.accept().expect("the client must actually connect");
        let _ = stream.read(&mut [0u8; 4096]);

        // Poll the registry the way the HTTP handler does. Bounded so a regression FAILS rather
        // than hanging the suite.
        let mut seen = None;
        for _ in 0..500 {
            if let Some(a) = crate::ipc::asset_sync::snapshot(&obs).live.into_iter().next() {
                seen = Some(a);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let a = seen.expect(
            "the client is BLOCKED inside login() and the registry is empty — that is #731: the \
             endpoint answers 'no asset sync is running' while the client is stuck");
        assert_eq!(a.connecting_purpose(), Some("zone load: qeynos2"));
        assert_eq!(a.set(), None, "a login is not a sync of any set");
        assert_eq!(a.phase(), None, "…and has no phase, so no bytes or rate can be read from it");

        // Cut the connection: the login fails fast and the whole window must close honestly.
        drop(stream);
        drop(listener);
        let r = h.join().expect("the login thread must not panic");
        assert!(r.is_err(), "a server that hangs up must surface as an Err");
        let snap = crate::ipc::asset_sync::snapshot(&obs);
        assert!(snap.live.is_empty(),
            "a finished login must stop being reported as in flight: {:?}", snap.live);
        assert_eq!(snap.last_ended.expect("an ended login must be recorded").what,
            crate::ipc::EndedWhat::Connect {
                purpose: "zone load: qeynos2".into(),
                outcome: crate::ipc::ConnectOutcome::Failed,
            },
            "…and a FAILED login must stay distinguishable from a successful one, or an agent \
             polling after the fact is back to 'nothing happened'");
    }

    /// The opposite verdict on the same path, so `outcome` is a real reading of the `Result` rather
    /// than a constant that happens to satisfy the assertion above. A minimal `/auth` reply is
    /// enough for `AssetSync::login` to succeed.
    #[test]
    fn a_successful_login_records_succeeded_and_leaves_nothing_in_flight() {
        use std::io::{Read, Write};
        let (url, listener) = hung_asset_server();
        let obs = slot();
        let obs_for_thread = obs.clone();
        let h = std::thread::spawn(move || {
            login_observed(&url, "u", "p", &obs_for_thread, "common asset load")
        });

        let (mut stream, _) = listener.accept().expect("connect");
        let _ = stream.read(&mut [0u8; 4096]);
        let body = br#"{"token":"t"}"#;
        stream.write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n", body.len()).as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
        drop(stream);

        assert!(h.join().expect("no panic").is_ok(), "the fixture must actually authenticate");
        let snap = crate::ipc::asset_sync::snapshot(&obs);
        assert!(snap.live.is_empty(), "the login is over: {:?}", snap.live);
        assert_eq!(snap.last_ended.unwrap().what, crate::ipc::EndedWhat::Connect {
            purpose: "common asset load".into(),
            outcome: crate::ipc::ConnectOutcome::Succeeded,
        });
    }
}
