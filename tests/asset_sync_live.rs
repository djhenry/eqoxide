//! Requires the eqoxide asset server running at $ASSET_URL (default
//! http://localhost:8088) with account $ASSET_USER/$ASSET_PASS.
//! Run: ASSET_USER=claude ASSET_PASS=REDACTED cargo test --test asset_sync_live -- --ignored

use eqoxide::asset_sync::{login_observed, ManifestFetch, Transport};

#[test]
#[ignore]
fn live_login_manifest_chunk() {
    let base = std::env::var("ASSET_URL").unwrap_or_else(|_| "http://localhost:8088".into());
    let user = std::env::var("ASSET_USER").unwrap_or_else(|_| "claude".into());
    let pass = std::env::var("ASSET_PASS").unwrap_or_else(|_| "REDACTED".into());

    // #731: `AssetSync::login` is private — `login_observed` is the only way in, so that no call
    // site can leave a blocking login unobservable. Tests get a throwaway registry.
    let obs = eqoxide::ipc::asset_sync::new_shared();
    let sync = login_observed(&base, &user, &pass, &obs, "asset_sync_live test").expect("login");
    // Conditional fetch: pass no ETag so a cold fetch always returns the full manifest.
    let m = match sync.get_manifest("common", None).expect("manifest") {
        ManifestFetch::Changed(m) => m,
        ManifestFetch::Unchanged => panic!("cold fetch (no If-None-Match) must return Changed, got Unchanged"),
    };
    assert_eq!(m.set, "common");
    assert!(!m.files.is_empty());
    let first_chunk = &m.files[0].chunks[0];
    let bytes = sync.get_chunk(first_chunk).expect("chunk");
    assert!(!bytes.is_empty());
}
