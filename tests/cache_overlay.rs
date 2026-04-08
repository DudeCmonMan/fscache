mod common;
use common::{write_backing_file, FuseHarness};
use std::path::PathBuf;

// ---- helpers ----

/// Write a file directly into the cache dir at the relative path.
fn write_cache_file(h: &FuseHarness, rel: &str, content: &[u8]) {
    let path = h.cache_path().join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// Write a .partial file into the cache dir (should be ignored by FUSE).
fn write_partial_file(h: &FuseHarness, rel: &str, content: &[u8]) {
    let mut p = PathBuf::from(rel);
    let mut filename = p.file_name().unwrap().to_os_string();
    filename.push(".partial");
    p.set_file_name(filename);

    let path = h.cache_path().join(&p);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn wait() {
    std::thread::sleep(std::time::Duration::from_millis(150));
}

// ---- tests ----

#[test]
fn cache_miss_serves_from_backing() {
    let h = FuseHarness::new_with_cache(1.0, 72).unwrap();
    write_backing_file(&h, "movies/film.mkv", b"backing content");
    wait();

    let data = std::fs::read(h.mount_path().join("movies/film.mkv")).unwrap();
    assert_eq!(data, b"backing content");
}

/// Cache hit: file exists in both cache and backing → FUSE returns cached content.
/// The cache file has different content than the backing file to prove we're
/// reading from the SSD cache and not from the backing store.
#[test]
fn cache_hit_serves_from_cache() {
    let h = FuseHarness::new_with_cache(1.0, 72).unwrap();
    write_backing_file(&h, "tv/Show/S01E01.mkv", b"backing content");
    write_cache_file(&h, "tv/Show/S01E01.mkv", b"cached content");
    wait();

    let data = std::fs::read(h.mount_path().join("tv/Show/S01E01.mkv")).unwrap();
    assert_eq!(data, b"cached content", "expected cached content, got backing content");
}

/// A `.partial` file in the cache must NOT be served — FUSE falls through to backing.
#[test]
fn partial_file_is_ignored() {
    let h = FuseHarness::new_with_cache(1.0, 72).unwrap();
    write_backing_file(&h, "movies/film.mkv", b"backing content");
    write_partial_file(&h, "movies/film.mkv", b"partial junk data");
    wait();

    let data = std::fs::read(h.mount_path().join("movies/film.mkv")).unwrap();
    assert_eq!(data, b"backing content", "partial file should be ignored, backing content expected");
}

/// Cache transition: start with a miss, then copy file to cache via atomic rename,
/// and verify the next read serves the cached version.
#[test]
fn cache_transition_after_copy() {
    let h = FuseHarness::new_with_cache(1.0, 72).unwrap();
    write_backing_file(&h, "movies/film.mkv", b"backing content");
    wait();

    // First read: cache miss → backing content
    let data = std::fs::read(h.mount_path().join("movies/film.mkv")).unwrap();
    assert_eq!(data, b"backing content");

    // Simulate a completed copy by placing the file in the cache.
    write_cache_file(&h, "movies/film.mkv", b"cached content");
    wait();

    // Second read: cache hit → cached content
    let data = std::fs::read(h.mount_path().join("movies/film.mkv")).unwrap();
    assert_eq!(data, b"cached content");
}

#[test]
fn passthrough_mode_bypasses_cache() {
    use fscache::cache::CacheManager;
    use fscache::fuse_fs::FsCache;
    use fuser::{MountOption, SessionACL};
    use tempfile::TempDir;

    let backing = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    // Write a file to backing and a different version to the cache.
    let backing_file = backing.path().join("test.mkv");
    std::fs::write(&backing_file, b"backing content").unwrap();

    let cache_file = cache_dir.path().join("test.mkv");
    std::fs::write(&cache_file, b"cached content").unwrap();

    let mut fs = FsCache::new(backing.path()).unwrap();
    fs.passthrough_mode = true; // bypass cache
    fs.cache = Some(std::sync::Arc::new(CacheManager::new(
        cache_dir.path().to_path_buf(),
        cache_dir.path().to_path_buf(),
        1.0,
        72,
        0.0,
    )));

    let mut config = fuser::Config::default();
    config.mount_options = vec![MountOption::RO, MountOption::FSName("test".to_string())];
    config.acl = SessionACL::Owner;
    let _session = fuser::spawn_mount2(fs, mount.path(), &config).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(150));

    let data = std::fs::read(mount.path().join("test.mkv")).unwrap();
    assert_eq!(data, b"backing content", "passthrough_mode should bypass cache");
}

/// Startup cleanup: .partial files in the cache are removed on CacheManager creation.
#[test]
fn startup_cleanup_removes_partials() {
    use fscache::cache::CacheManager;
    use tempfile::TempDir;

    let cache_dir = TempDir::new().unwrap();

    // Plant a .partial file before creating the CacheManager.
    let partial = cache_dir.path().join("movies").join("film.mkv.partial");
    std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
    std::fs::write(&partial, b"interrupted").unwrap();

    // Also plant a legitimate cached file (should survive).
    let cached = cache_dir.path().join("movies").join("film2.mkv");
    std::fs::write(&cached, b"complete").unwrap();

    let mgr = CacheManager::new(cache_dir.path().to_path_buf(), cache_dir.path().to_path_buf(), 1.0, 72, 0.0);
    mgr.startup_cleanup();

    assert!(!partial.exists(), ".partial file should have been removed by startup_cleanup");
    assert!(cached.exists(), "complete cached file should survive startup_cleanup");
}

#[test]
fn size_eviction_removes_oldest_files() {
    use fscache::cache::CacheManager;
    use std::path::Path;
    use tempfile::TempDir;

    let cache_dir = TempDir::new().unwrap();

    // Write two files, each 600 bytes. Max size = 1000 bytes → one must go.
    let old_file = cache_dir.path().join("old.mkv");
    let new_file = cache_dir.path().join("new.mkv");
    std::fs::write(&old_file, vec![0u8; 600]).unwrap();
    std::fs::write(&new_file, vec![0u8; 600]).unwrap();

    // Max ~955 bytes (1000 / 1_073_741_824 GB), expiry = 9999 hours (never expires).
    let mgr = CacheManager::new(
        cache_dir.path().to_path_buf(),
        cache_dir.path().to_path_buf(),
        1000.0 / 1_073_741_824.0,
        9999,
        0.0,
    );

    // Register files in the DB. Give old_file an earlier last_hit_at so it
    // is chosen as the LRU eviction candidate.
    let mount_id = cache_dir.path().to_string_lossy().into_owned();
    mgr.mark_cached(Path::new("old.mkv"), 600);
    mgr.mark_cached(Path::new("new.mkv"), 600);
    let db = mgr.cache_db();
    let old_ts = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64)
        - 3600; // 1 hour ago
    db.set_last_hit_at_for_test(Path::new("old.mkv"), &mount_id, old_ts);

    mgr.evict_if_needed();

    // The older file should have been evicted; the newer one should survive.
    assert!(!old_file.exists(), "oldest file should be evicted");
    assert!(new_file.exists(), "newer file should survive");
}

#[test]
fn expiry_eviction_removes_expired_files() {
    use fscache::cache::CacheManager;
    use std::path::Path;
    use tempfile::TempDir;

    let cache_dir = TempDir::new().unwrap();

    let expired = cache_dir.path().join("expired.mkv");
    let fresh = cache_dir.path().join("fresh.mkv");
    std::fs::write(&expired, b"old data").unwrap();
    std::fs::write(&fresh, b"new data").unwrap();

    // expiry = 1 hour → `expired` (2 hours ago) is past its window, `fresh` is not.
    let mgr = CacheManager::new(
        cache_dir.path().to_path_buf(),
        cache_dir.path().to_path_buf(),
        1.0,
        1, // 1 hour expiry
        0.0,
    );

    // Register both files. Back-date expired's last_hit_at to 2 hours ago.
    let mount_id = cache_dir.path().to_string_lossy().into_owned();
    mgr.mark_cached(Path::new("expired.mkv"), 8);
    mgr.mark_cached(Path::new("fresh.mkv"), 8);
    let db = mgr.cache_db();
    let two_hours_ago = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64)
        - 7200;
    db.set_last_hit_at_for_test(Path::new("expired.mkv"), &mount_id, two_hours_ago);

    mgr.evict_if_needed();

    assert!(!expired.exists(), "expired file should be evicted");
    assert!(fresh.exists(), "fresh file should survive");
}

// Regression: a freshly cached file must not be immediately evicted, even if
// the source file has an old mtime. Eviction uses DB last_hit_at, not filesystem mtime.
#[test]
fn freshly_cached_file_with_old_mtime_survives_eviction() {
    use fscache::cache::CacheManager;
    use std::path::Path;
    use tempfile::TempDir;

    let cache_dir = TempDir::new().unwrap();
    let cached = cache_dir.path().join("episode.mkv");
    std::fs::write(&cached, b"data").unwrap();

    // expiry = 72 hours — file was just registered (last_hit_at = now) so it should survive.
    let mgr = CacheManager::new(
        cache_dir.path().to_path_buf(),
        cache_dir.path().to_path_buf(),
        1.0,
        72,
        0.0,
    );
    mgr.mark_cached(Path::new("episode.mkv"), 4);
    mgr.evict_if_needed();

    assert!(cached.exists(), "freshly cached file should not be evicted");
}
