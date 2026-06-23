//! Requires the eqoxide asset server running at $ASSET_URL (default
//! http://localhost:8088) with account $ASSET_USER/$ASSET_PASS.
//! Run: ASSET_USER=claude ASSET_PASS=REDACTED cargo test --test asset_sync_live -- --ignored

use eq_renderer::asset_sync::{AssetSync, Transport};

#[test]
#[ignore]
fn live_login_manifest_chunk() {
    let base = std::env::var("ASSET_URL").unwrap_or_else(|_| "http://localhost:8088".into());
    let user = std::env::var("ASSET_USER").unwrap_or_else(|_| "claude".into());
    let pass = std::env::var("ASSET_PASS").unwrap_or_else(|_| "REDACTED".into());

    let sync = AssetSync::login(&base, &user, &pass).expect("login");
    let m = sync.get_manifest("common").expect("manifest");
    assert_eq!(m.set, "common");
    assert!(!m.files.is_empty());
    let first_chunk = &m.files[0].chunks[0];
    let bytes = sync.get_chunk(first_chunk).expect("chunk");
    assert!(!bytes.is_empty());
}
