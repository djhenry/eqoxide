//! Exercise the real client sync transport against an isolated loopback fixture server.
//! The server must use no-auth fixture mode; this probe never reads account credentials.
use eqoxide::asset_sync::{login_observed, sync_set_observed, CacheDirs, SyncProgress};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    anyhow::ensure!(args.len() == 3, "usage: asset_sync_probe PORT SET CACHE_DIRECTORY");
    let port: u16 = args[0].parse()?;
    let cache = CacheDirs::with_root(&args[2]);
    let observed = eqoxide::ipc::asset_sync::new_shared();
    let transport = login_observed(
        &format!("http://127.0.0.1:{port}"), "fixture", "fixture", &observed, "manifest acceptance",
    )?;
    let mut chunks_done = 0;
    let mut bytes = 0;
    sync_set_observed(&transport, &args[1], &cache, &observed, &mut |progress| {
        if let SyncProgress::Downloading { done, bytes: downloaded, .. } = progress {
            chunks_done = chunks_done.max(done);
            bytes = bytes.max(downloaded);
        }
    })?;
    println!("{}", serde_json::json!({
        "set": args[1], "content_digest": cache.synced_digest(&args[1]),
        "downloaded_chunks": chunks_done, "downloaded_bytes": bytes,
    }));
    Ok(())
}
