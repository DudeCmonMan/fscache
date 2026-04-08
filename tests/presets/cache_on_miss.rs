/// End-to-end tests for the `cache-on-miss` preset.
///
/// CacheOnMiss is a generic preset: every cache miss caches the accessed file
/// exactly (no lookahead). Blocked processes are still filtered.
///
/// These tests exercise the full pipeline:
///   FUSE open() → AccessEvent → ActionEngine → CacheOnMiss::on_miss()
///   → CopyRequest → copier → mark_cached() → subsequent reads served from SSD.
use crate::common::{write_backing_file, FuseHarness};
use fscache::presets::cache_on_miss::CacheOnMiss;
use std::sync::Arc;
use std::time::Duration;

/// Reading a file through FUSE triggers an AccessEvent. CacheOnMiss.on_miss()
/// returns CacheAction::Cache([path]), so the copier copies the exact file.
/// Subsequent reads are served from SSD (even after the backing file changes).
#[tokio::test]
async fn cache_on_miss_caches_accessed_file() {
    let preset = Arc::new(CacheOnMiss::new(vec![]));
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "movies/Movie.mkv", b"original content");
    std::thread::sleep(Duration::from_millis(100));

    // First read: cache miss — triggers caching via pipeline.
    let data = std::fs::read(h.mount_path().join("movies/Movie.mkv")).unwrap();
    assert_eq!(data, b"original content");

    // Wait for the pipeline (AccessEvent → on_miss → copier → mark_cached).
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(
        h.cache_path().join("movies/Movie.mkv").exists(),
        "CacheOnMiss must copy the accessed file to the cache dir"
    );

    // Overwrite the backing file — subsequent FUSE reads must still return the cached original.
    write_backing_file(&h, "movies/Movie.mkv", b"overwritten backing");

    let data = std::fs::read(h.mount_path().join("movies/Movie.mkv")).unwrap();
    assert_eq!(
        data, b"original content",
        "second read must come from SSD cache, not the overwritten backing file"
    );
}

/// A process on the blocklist must not trigger caching, even with CacheOnMiss.
/// Uses `cat` as the blocked process — available on all Linux CI systems.
#[tokio::test]
async fn cache_on_miss_respects_blocklist() {
    let preset = Arc::new(CacheOnMiss::new(vec!["cat".to_string()]));
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "movies/Movie.mkv", b"some content");
    std::thread::sleep(Duration::from_millis(100));

    // `cat` opens the file through FUSE. Because "cat" is blocklisted,
    // open() must not send an AccessEvent — nothing should be cached.
    let path = h.mount_path().join("movies/Movie.mkv");
    let mut child = std::process::Command::new("cat")
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _ = child.wait();

    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(
        !h.cache_path().join("movies/Movie.mkv").exists(),
        "blocklisted process (cat) must not trigger caching"
    );
}
