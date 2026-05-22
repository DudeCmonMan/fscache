mod common;
use common::{FuseHarness, write_backing_file};
use fscache::cache::db::{CacheDb, SourceMetadata};
use fscache::cache::manager::StaleResult;
use std::path::PathBuf;
use std::sync::Arc;

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
    assert_eq!(
        data, b"cached content",
        "expected cached content, got backing content"
    );
}

/// A `.partial` file in the cache must NOT be served — FUSE falls through to backing.
#[test]
fn partial_file_is_ignored() {
    let h = FuseHarness::new_with_cache(1.0, 72).unwrap();
    write_backing_file(&h, "movies/film.mkv", b"backing content");
    write_partial_file(&h, "movies/film.mkv", b"partial junk data");
    wait();

    let data = std::fs::read(h.mount_path().join("movies/film.mkv")).unwrap();
    assert_eq!(
        data, b"backing content",
        "partial file should be ignored, backing content expected"
    );
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
    use fscache::cache::manager::CacheManager;
    use fscache::fuse::fusefs::FsCache;
    use fuser::{MountOption, SessionACL};
    use tempfile::TempDir;

    let backing = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let backing_file = backing.path().join("test.mkv");
    std::fs::write(&backing_file, b"backing content").unwrap();

    let cache_file = cache_dir.path().join("test.mkv");
    std::fs::write(&cache_file, b"cached content").unwrap();

    let mut fs = FsCache::new(backing.path()).unwrap();
    fs.passthrough_mode = true;
    let db = Arc::new(CacheDb::open(&cache_dir.path().join("test.db")).unwrap());
    fs.cache = Some(Arc::new(CacheManager::new(
        cache_dir.path().to_path_buf(),
        db,
        cache_dir.path().to_path_buf(),
        1.0,
        72,
        0.0,
        None,
        &Default::default(),
    )));

    let mut config = fuser::Config::default();
    config.mount_options = vec![MountOption::RO, MountOption::FSName("test".to_string())];
    config.acl = SessionACL::Owner;
    let _session = fuser::spawn_mount2(fs, mount.path(), &config).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(150));

    let data = std::fs::read(mount.path().join("test.mkv")).unwrap();
    assert_eq!(
        data, b"backing content",
        "passthrough_mode should bypass cache"
    );
}

/// Startup cleanup: .partial files in the cache are removed on CacheManager creation.
#[test]
fn startup_cleanup_removes_partials() {
    use fscache::cache::manager::CacheManager;
    use tempfile::TempDir;

    let cache_dir = TempDir::new().unwrap();

    let partial = cache_dir.path().join("movies").join("film.mkv.partial");
    std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
    std::fs::write(&partial, b"interrupted").unwrap();

    let cached = cache_dir.path().join("movies").join("film2.mkv");
    std::fs::write(&cached, b"complete").unwrap();

    let db = Arc::new(CacheDb::open(&cache_dir.path().join("test.db")).unwrap());
    let mgr = CacheManager::new(
        cache_dir.path().to_path_buf(),
        db,
        cache_dir.path().to_path_buf(),
        1.0,
        72,
        0.0,
        None,
        &Default::default(),
    );
    mgr.startup_cleanup();

    assert!(
        !partial.exists(),
        ".partial file should have been removed by startup_cleanup"
    );
    assert!(
        cached.exists(),
        "complete cached file should survive startup_cleanup"
    );
}

#[test]
fn size_eviction_removes_oldest_files() {
    use fscache::cache::manager::CacheManager;
    use std::path::Path;
    use tempfile::TempDir;

    let cache_dir = TempDir::new().unwrap();

    let old_file = cache_dir.path().join("old.mkv");
    let new_file = cache_dir.path().join("new.mkv");
    std::fs::write(&old_file, vec![0u8; 600]).unwrap();
    std::fs::write(&new_file, vec![0u8; 600]).unwrap();

    // Max ~955 bytes (1000 / 1_073_741_824 GB), expiry = 9999 hours (never expires).
    let db = Arc::new(CacheDb::open(&cache_dir.path().join("test.db")).unwrap());
    let mgr = CacheManager::new(
        cache_dir.path().to_path_buf(),
        db,
        cache_dir.path().to_path_buf(),
        1000.0 / 1_073_741_824.0,
        9999,
        0.0,
        None,
        &Default::default(),
    );

    // Register files in the DB. Give old_file an earlier last_hit_at so it
    // is chosen as the LRU eviction candidate.
    let mount_id = cache_dir.path().to_string_lossy().into_owned();
    mgr.mark_cached(Path::new("old.mkv"), SourceMetadata::test_file(600, 0, 0));
    mgr.mark_cached(Path::new("new.mkv"), SourceMetadata::test_file(600, 0, 0));
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
    use fscache::cache::manager::CacheManager;
    use std::path::Path;
    use tempfile::TempDir;

    let cache_dir = TempDir::new().unwrap();

    let expired = cache_dir.path().join("expired.mkv");
    let fresh = cache_dir.path().join("fresh.mkv");
    std::fs::write(&expired, b"old data").unwrap();
    std::fs::write(&fresh, b"new data").unwrap();

    // expiry = 1 hour → `expired` (2 hours ago) is past its window, `fresh` is not.
    let cache_db = Arc::new(CacheDb::open(&cache_dir.path().join("test.db")).unwrap());
    let mgr = CacheManager::new(
        cache_dir.path().to_path_buf(),
        Arc::clone(&cache_db),
        cache_dir.path().to_path_buf(),
        1.0,
        1, // 1 hour expiry
        0.0,
        None,
        &Default::default(),
    );

    // Register both files. Back-date expired's last_hit_at to 2 hours ago.
    let mount_id = cache_dir.path().to_string_lossy().into_owned();
    mgr.mark_cached(Path::new("expired.mkv"), SourceMetadata::test_file(8, 0, 0));
    mgr.mark_cached(Path::new("fresh.mkv"), SourceMetadata::test_file(8, 0, 0));
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
    use fscache::cache::manager::CacheManager;
    use std::path::Path;
    use tempfile::TempDir;

    let cache_dir = TempDir::new().unwrap();
    let cached = cache_dir.path().join("episode.mkv");
    std::fs::write(&cached, b"data").unwrap();

    // expiry = 72 hours — file was just registered (last_hit_at = now) so it should survive.
    let db = Arc::new(CacheDb::open(&cache_dir.path().join("test.db")).unwrap());
    let mgr = CacheManager::new(
        cache_dir.path().to_path_buf(),
        db,
        cache_dir.path().to_path_buf(),
        1.0,
        72,
        0.0,
        None,
        &Default::default(),
    );
    mgr.mark_cached(Path::new("episode.mkv"), SourceMetadata::test_file(4, 0, 0));
    mgr.evict_if_needed();

    assert!(cached.exists(), "freshly cached file should not be evicted");
}

/// Helper: create a CacheManager backed by a real BackingStore (needed for is_stale).
fn make_stale_harness() -> (
    tempfile::TempDir, // backing dir
    tempfile::TempDir, // cache dir
    fscache::cache::manager::CacheManager,
) {
    use fscache::backing_store::BackingStore;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let backing_dir = tempfile::TempDir::new().unwrap();
    let cache_dir = tempfile::TempDir::new().unwrap();

    let c = CString::new(backing_dir.path().as_os_str().as_bytes()).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY) };
    assert!(fd >= 0, "failed to open backing dir O_PATH fd");
    let backing_store = Arc::new(BackingStore::new(fd));

    let db = Arc::new(CacheDb::open(&cache_dir.path().join("test.db")).unwrap());
    let mgr = fscache::cache::manager::CacheManager::new(
        cache_dir.path().to_path_buf(),
        db,
        cache_dir.path().to_path_buf(),
        1.0,
        72,
        0.0,
        Some(backing_store),
        &Default::default(),
    );
    (backing_dir, cache_dir, mgr)
}

/// Write a file to the backing dir and register it in the cache DB with real mtime.
fn register_backing_file(
    backing_dir: &tempfile::TempDir,
    mgr: &fscache::cache::manager::CacheManager,
    rel: &str,
    content: &[u8],
) {
    let backing_path = backing_dir.path().join(rel);
    if let Some(p) = backing_path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(&backing_path, content).unwrap();

    // Read live metadata from backing file to populate fingerprint correctly.
    let m = std::fs::metadata(&backing_path).unwrap();
    let cache_path = mgr.cache_path(std::path::Path::new(rel));
    if let Some(p) = cache_path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(&cache_path, content).unwrap();
    mgr.mark_cached(std::path::Path::new(rel), SourceMetadata::from_metadata(&m));
}

#[test]
fn is_stale_returns_fresh_after_copy() {
    let (backing_dir, _cache_dir, mgr) = make_stale_harness();
    register_backing_file(&backing_dir, &mgr, "episode.mkv", b"video data");

    let result = mgr.is_stale(std::path::Path::new("episode.mkv"));
    assert!(
        matches!(result, StaleResult::Fresh),
        "newly cached file should be Fresh"
    );
}

#[test]
fn is_stale_returns_stale_after_mtime_change() {
    use std::time::{Duration, UNIX_EPOCH};

    let (backing_dir, _cache_dir, mgr) = make_stale_harness();
    register_backing_file(&backing_dir, &mgr, "episode.mkv", b"original data");

    // Wind the backing file's mtime forward by 60 seconds.
    let backing_path = backing_dir.path().join("episode.mkv");
    let m = std::fs::metadata(&backing_path).unwrap();
    use std::os::unix::fs::MetadataExt;
    let new_mtime = UNIX_EPOCH + Duration::from_secs(m.mtime() as u64 + 60);
    filetime::set_file_mtime(
        &backing_path,
        filetime::FileTime::from_system_time(new_mtime),
    )
    .unwrap();

    let result = mgr.is_stale(std::path::Path::new("episode.mkv"));
    assert!(
        matches!(result, StaleResult::Stale),
        "file with changed mtime should be Stale"
    );
}

#[test]
fn is_stale_returns_stale_after_size_change() {
    let (backing_dir, _cache_dir, mgr) = make_stale_harness();
    register_backing_file(&backing_dir, &mgr, "episode.mkv", b"original");

    // Overwrite backing with different-size content but force original mtime back.
    let backing_path = backing_dir.path().join("episode.mkv");
    use std::os::unix::fs::MetadataExt;
    let original_mtime_secs = std::fs::metadata(&backing_path).unwrap().mtime();

    std::fs::write(&backing_path, b"longer backing content than before").unwrap();

    // Reset mtime to original value to isolate the size signal.
    let original_ft = filetime::FileTime::from_unix_time(original_mtime_secs, 0);
    filetime::set_file_mtime(&backing_path, original_ft).unwrap();

    let result = mgr.is_stale(std::path::Path::new("episode.mkv"));
    assert!(
        matches!(result, StaleResult::Stale),
        "file with changed size should be Stale"
    );
}

#[test]
fn is_stale_returns_backing_gone_after_delete() {
    let (backing_dir, _cache_dir, mgr) = make_stale_harness();
    register_backing_file(&backing_dir, &mgr, "episode.mkv", b"data");

    std::fs::remove_file(backing_dir.path().join("episode.mkv")).unwrap();

    let result = mgr.is_stale(std::path::Path::new("episode.mkv"));
    assert!(
        matches!(result, StaleResult::BackingGone),
        "deleted backing file should be BackingGone"
    );
}

#[test]
fn is_stale_returns_needs_backfill_for_zero_fingerprint() {
    let (backing_dir, cache_dir, mgr) = make_stale_harness();
    let backing_path = backing_dir.path().join("episode.mkv");
    let cache_path = cache_dir.path().join("episode.mkv");
    std::fs::write(&backing_path, b"data").unwrap();
    std::fs::write(&cache_path, b"data").unwrap();

    // Register with zero fingerprint to simulate a pre-migration row.
    mgr.mark_cached(
        std::path::Path::new("episode.mkv"),
        SourceMetadata::test_file(4, 0, 0),
    );

    let result = mgr.is_stale(std::path::Path::new("episode.mkv"));
    assert!(
        matches!(result, StaleResult::NeedsBackfill(_)),
        "zero-fingerprint row should be NeedsBackfill"
    );

    // Backfill, then the next check should be Fresh.
    if let StaleResult::NeedsBackfill(st) = result {
        mgr.backfill_fingerprint(std::path::Path::new("episode.mkv"), &st);
    }
    assert!(
        matches!(
            mgr.is_stale(std::path::Path::new("episode.mkv")),
            StaleResult::Fresh
        ),
        "after backfill, file should be Fresh"
    );
}

#[test]
fn maintenance_evict_if_needed_runs_without_copies() {
    // Regression: evict_if_needed must fire even when no copies are in flight.
    // Simulates the lazy-eviction gap that the maintenance task fixes.
    use std::path::Path;
    let cache_dir = tempfile::TempDir::new().unwrap();
    let expired = cache_dir.path().join("expired.mkv");
    std::fs::write(&expired, b"old").unwrap();

    let db = Arc::new(CacheDb::open(&cache_dir.path().join("test.db")).unwrap());
    let mgr = fscache::cache::manager::CacheManager::new(
        cache_dir.path().to_path_buf(),
        Arc::clone(&db),
        cache_dir.path().to_path_buf(),
        1.0,
        1, // 1 hour expiry
        0.0,
        None,
        &Default::default(),
    );
    mgr.mark_cached(Path::new("expired.mkv"), SourceMetadata::test_file(3, 0, 0));

    // Back-date last_hit_at to 2 hours ago.
    let mount_id = cache_dir.path().to_string_lossy().into_owned();
    let two_hours_ago = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64)
        - 7200;
    db.set_last_hit_at_for_test(Path::new("expired.mkv"), &mount_id, two_hours_ago);

    // No copier is running — call evict_if_needed directly (as maintenance task would).
    mgr.evict_if_needed();

    assert!(
        !expired.exists(),
        "evict_if_needed should remove expired file without copier"
    );
}

/// evict_to_fit: given N bytes of incoming content, evicts the minimum LRU candidates
/// needed so the cache can hold the new file within its budget.
///
/// Setup: budget = 1200 bytes, 3 files of 400 bytes each (total = 1200 = exactly at limit).
/// A new file of 500 bytes needs to land → target = 1200 - 500 = 700 bytes remaining.
/// Need to free 500 bytes: evict oldest (400), freed=400 < 500; evict next (400), freed=800 ≥ 500.
/// Result: 2 LRU files evicted, 1 (newest) remains.
#[test]
fn evict_to_fit_frees_lru_files_to_fit_incoming() {
    use fscache::cache::manager::CacheManager;
    use std::path::Path;
    use tempfile::TempDir;

    let cache_dir = TempDir::new().unwrap();

    let file1 = cache_dir.path().join("a.mkv");
    let file2 = cache_dir.path().join("b.mkv");
    let file3 = cache_dir.path().join("c.mkv");
    std::fs::write(&file1, vec![1u8; 400]).unwrap();
    std::fs::write(&file2, vec![2u8; 400]).unwrap();
    std::fs::write(&file3, vec![3u8; 400]).unwrap();

    // Budget = 1200 bytes exactly — all three files fit, nothing spare.
    let budget_gb = 1200.0_f64 / 1_073_741_824.0;
    let db = Arc::new(CacheDb::open(&cache_dir.path().join("test.db")).unwrap());
    let mgr = CacheManager::new(
        cache_dir.path().to_path_buf(),
        Arc::clone(&db),
        cache_dir.path().to_path_buf(),
        budget_gb,
        9999, // effectively no TTL expiry
        0.0,
        None,
        &Default::default(),
    );

    mgr.mark_cached(Path::new("a.mkv"), SourceMetadata::test_file(400, 0, 0));
    mgr.mark_cached(Path::new("b.mkv"), SourceMetadata::test_file(400, 0, 0));
    mgr.mark_cached(Path::new("c.mkv"), SourceMetadata::test_file(400, 0, 0));

    // Make LRU order explicit: a.mkv is oldest, c.mkv is newest.
    let mount_id = cache_dir.path().to_string_lossy().into_owned();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    db.set_last_hit_at_for_test(Path::new("a.mkv"), &mount_id, now - 3600);
    db.set_last_hit_at_for_test(Path::new("b.mkv"), &mount_id, now - 1800);
    db.set_last_hit_at_for_test(Path::new("c.mkv"), &mount_id, now);

    let freed = mgr.evict_to_fit(500);

    assert!(
        freed >= 500,
        "evict_to_fit should have freed at least 500 bytes, freed {freed}"
    );
    assert!(!file1.exists(), "a.mkv (oldest) should be evicted");
    assert!(!file2.exists(), "b.mkv (second oldest) should be evicted");
    assert!(file3.exists(), "c.mkv (newest) should survive");
}
