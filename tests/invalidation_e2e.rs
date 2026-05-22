/// End-to-end tests for cache invalidation.
///
/// Every test here goes through a real FUSE mount — reads are `open()` + `read()`
/// syscalls that traverse the kernel and hit the fuser session, exactly as they
/// would in production. Cache hits are authoritative on the hot path: cached
/// lookup/getattr/open do not synchronously stat backing. Validation happens via
/// explicit maintenance/validation paths.
///
mod common;
use common::{FuseHarness, write_backing_file};
use fscache::cache::db::SourceMetadata;
use fscache::config::InvalidationConfig;
use std::path::Path;
use std::time::Duration;

/// Build a harness and plant a hot cache entry.
///
/// Returns `(harness, backing_path, cache_file_path)`.
/// After this call:
///   - backing file contains `backing_content`
///   - cache file contains `cache_content` (intentionally different bytes, SAME size)
///   - DB fingerprint matches the current backing mtime/size → `is_stale()` returns Fresh
///   - FUSE read returns `cache_content`
///
/// **Both byte slices must be the same length.** FUSE `getattr` reads the file
/// size from the backing store; the kernel uses that size to bound reads, so a
/// size mismatch between backing and cache would produce truncated reads.
fn setup_hot_cache(
    invalidation: &InvalidationConfig,
    rel: &str,
    backing_content: &[u8],
    cache_content: &[u8],
) -> (FuseHarness, std::path::PathBuf, std::path::PathBuf) {
    assert_eq!(
        backing_content.len(),
        cache_content.len(),
        "setup_hot_cache: backing and cache content must be the same length \
         (kernel bounds reads by the size returned from getattr → backing)"
    );

    let h = FuseHarness::new_with_cache_and_invalidation(1.0, 72, invalidation).unwrap();

    // Write the backing file.
    write_backing_file(&h, rel, backing_content);
    let backing_path = h.backing_path().join(rel);

    // Plant a cache file with distinct content so we can tell which path was served.
    let cache_file = h.cache_path().join(rel);
    if let Some(p) = cache_file.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(&cache_file, cache_content).unwrap();

    // Register the fingerprint from the BACKING file's real mtime/size.
    let meta = std::fs::metadata(&backing_path).unwrap();
    h.cache_mgr()
        .mark_cached(Path::new(rel), SourceMetadata::from_metadata(&meta));

    // Brief settle so the FUSE session is ready.
    std::thread::sleep(Duration::from_millis(50));

    (h, backing_path, cache_file)
}

// check_on_hit tests

/// Baseline: a hot cache entry is served from the SSD cache, not the backing store.
/// This is the precondition for every other test in this file.
#[test]
fn e2e_hot_cache_entry_is_served_from_cache() {
    let cfg = InvalidationConfig {
        check_on_hit: true,
        check_on_maintenance: false,
    };
    let (h, _backing, _cache_file) =
        setup_hot_cache(&cfg, "movie.mkv", b"backing_v1____", b"cached___v1___");

    let data = std::fs::read(h.mount_path().join("movie.mkv")).unwrap();
    assert_eq!(
        data, b"cached___v1___",
        "expected cache hit to serve cached content"
    );
}

/// `check_on_hit=true`: stale backing is not checked synchronously from FUSE
/// open. The cache hit is served immediately; validation happens asynchronously
/// through CacheIO/maintenance.
#[test]
fn e2e_check_on_hit_serves_cached_data_after_mtime_change() {
    let cfg = InvalidationConfig {
        check_on_hit: true,
        check_on_maintenance: false,
    };
    let (h, backing_path, cache_file) =
        setup_hot_cache(&cfg, "movie.mkv", b"backing_v1____", b"cached___v1___");

    let before = std::fs::read(h.mount_path().join("movie.mkv")).unwrap();
    assert_eq!(before, b"cached___v1___");

    std::fs::write(&backing_path, b"backing__v2___").unwrap();

    let after = std::fs::read(h.mount_path().join("movie.mkv")).unwrap();
    assert_eq!(after, b"cached___v1___");
    assert!(
        cache_file.exists(),
        "cache hit must not synchronously drop stale cache"
    );
}

/// `check_on_hit=false` (shipped default): stale backing content does NOT evict the
/// cached copy.  This is the negative-control test — it proves the default is opt-in.
#[test]
fn e2e_check_on_hit_disabled_serves_stale_data() {
    let cfg = InvalidationConfig::default(); // check_on_hit: false
    let (h, backing_path, cache_file) =
        setup_hot_cache(&cfg, "movie.mkv", b"backing_v1____", b"cached___v1___");

    std::fs::write(&backing_path, b"backing__v2___").unwrap();

    // With check_on_hit=false the stale cached bytes are still served.
    let data = std::fs::read(h.mount_path().join("movie.mkv")).unwrap();
    assert_eq!(
        data, b"cached___v1___",
        "stale cache must still be served when check_on_hit=false"
    );

    assert!(
        cache_file.exists(),
        "cache file must not be deleted when check_on_hit=false"
    );
}

/// `check_on_hit=true`: size changes are not checked synchronously on FUSE open.
/// The cache hit is served immediately and validation is deferred.
#[test]
fn e2e_check_on_hit_serves_cached_data_after_size_change() {
    let cfg = InvalidationConfig {
        check_on_hit: true,
        check_on_maintenance: false,
    };
    let h = FuseHarness::new_with_cache_and_invalidation(1.0, 72, &cfg).unwrap();

    write_backing_file(&h, "movie.mkv", b"short");
    let backing_path = h.backing_path().join("movie.mkv");
    let cache_file = h.cache_path().join("movie.mkv");
    std::fs::write(&cache_file, b"cache").unwrap();

    let meta = std::fs::metadata(&backing_path).unwrap();
    h.cache_mgr()
        .mark_cached(Path::new("movie.mkv"), SourceMetadata::from_metadata(&meta));
    std::thread::sleep(Duration::from_millis(50));

    let before = std::fs::read(h.mount_path().join("movie.mkv")).unwrap();
    assert_eq!(before, b"cache");

    std::fs::write(&backing_path, b"this_content_is_longer_than_before").unwrap();

    let result = std::fs::read(h.mount_path().join("movie.mkv")).unwrap();
    assert_eq!(&result[..], b"cache");
    assert!(
        cache_file.exists(),
        "cache hit must not synchronously drop stale cache"
    );
}

/// `check_on_hit=true` with a zero fingerprint still serves cached data on the
/// hot path. Fingerprint backfill is deferred to validation/maintenance.
#[test]
fn e2e_check_on_hit_zero_fingerprint_serves_cache() {
    let cfg = InvalidationConfig {
        check_on_hit: true,
        check_on_maintenance: false,
    };
    let h = FuseHarness::new_with_cache_and_invalidation(1.0, 72, &cfg).unwrap();

    write_backing_file(&h, "episode.mkv", b"backing_data__");
    let cache_file = h.cache_path().join("episode.mkv");
    std::fs::write(&cache_file, b"cached__data__").unwrap();

    let meta = std::fs::metadata(h.backing_path().join("episode.mkv")).unwrap();
    h.cache_mgr().mark_cached(
        Path::new("episode.mkv"),
        SourceMetadata::test_file(meta.len(), 0, 0),
    );

    std::thread::sleep(Duration::from_millis(50));

    let data = std::fs::read(h.mount_path().join("episode.mkv")).unwrap();
    assert_eq!(data, b"cached__data__");
    assert!(cache_file.exists(), "cache file must not be deleted on hit");
}

/// When the backing file is deleted, cached metadata and bytes remain visible until
/// validation/maintenance decides the backing is truly gone and removes the cache.
#[test]
fn e2e_backing_deleted_cache_served_until_sweep_cleans() {
    let cfg = InvalidationConfig {
        check_on_hit: true,
        check_on_maintenance: true,
    };
    let (h, backing_path, cache_file) =
        setup_hot_cache(&cfg, "movie.mkv", b"backing_v1____", b"cached___v1___");

    std::fs::remove_file(&backing_path).unwrap();

    let result = std::fs::read(h.mount_path().join("movie.mkv")).unwrap();
    assert_eq!(result, b"cached___v1___");
    assert!(
        cache_file.exists(),
        "cache remains until validation/maintenance cleanup"
    );

    let (checked, dropped) = h.cache_mgr().sweep_stale();
    assert_eq!(checked, 1);
    assert_eq!(
        dropped, 1,
        "sweep must drop the entry whose backing was deleted"
    );
    assert!(
        !cache_file.exists(),
        "cache file must be removed by sweep after backing is gone"
    );

    let result2 = std::fs::read(h.mount_path().join("movie.mkv"));
    assert!(
        result2.is_err(),
        "FUSE read must fail after sweep removes cache and backing is gone"
    );
}

// Direct stale-scan tests

/// `sweep_stale()` drops a stale entry and the next FUSE read returns the updated
/// backing content. Production maintenance queues validation through CacheIO; this
/// test covers the lower-level synchronous scanner used by direct cleanup callers.
#[test]
fn e2e_direct_sweep_drops_stale_and_next_read_hits_backing() {
    let cfg = InvalidationConfig {
        check_on_hit: false,
        check_on_maintenance: true,
    };
    let (h, backing_path, cache_file) =
        setup_hot_cache(&cfg, "movie.mkv", b"backing_v1____", b"cached___v1___");

    // Update backing (same size — mtime change is the staleness signal).
    std::fs::write(&backing_path, b"backing__v2___").unwrap();

    // With check_on_hit=false, first read returns stale cached content.
    let stale = std::fs::read(h.mount_path().join("movie.mkv")).unwrap();
    assert_eq!(
        stale, b"cached___v1___",
        "precondition: stale content must be served when check_on_hit=false"
    );

    // Maintenance sweep — same call as run_maintenance_task.
    let (checked, dropped) = h.cache_mgr().sweep_stale();
    assert_eq!(checked, 1, "sweep must check 1 entry");
    assert_eq!(dropped, 1, "sweep must drop the 1 stale entry");
    assert!(!cache_file.exists(), "cache file must be removed by sweep");

    // Second read falls through to backing store.
    let fresh = std::fs::read(h.mount_path().join("movie.mkv")).unwrap();
    assert_eq!(
        fresh, b"backing__v2___",
        "read after sweep must return fresh backing content"
    );
}

/// `sweep_stale()` does nothing when all entries are fresh: the cache file survives
/// and reads continue to return the cached content.
#[test]
fn e2e_direct_sweep_preserves_fresh_entries() {
    let cfg = InvalidationConfig {
        check_on_hit: false,
        check_on_maintenance: true,
    };
    let (h, _backing, cache_file) =
        setup_hot_cache(&cfg, "movie.mkv", b"backing_v1____", b"cached___v1___");

    let (checked, dropped) = h.cache_mgr().sweep_stale();
    assert_eq!(checked, 1);
    assert_eq!(dropped, 0, "sweep must not drop a fresh entry");
    assert!(cache_file.exists(), "cache file must survive a no-op sweep");

    let data = std::fs::read(h.mount_path().join("movie.mkv")).unwrap();
    assert_eq!(
        data, b"cached___v1___",
        "cache must still be served after a no-op sweep"
    );
}

/// `sweep_stale()` backfills a zero-fingerprint (pre-migration) row without dropping
/// the cache entry.  After backfill, reads continue to return the cached content.
#[test]
fn e2e_direct_sweep_backfills_zero_fingerprint() {
    let cfg = InvalidationConfig {
        check_on_hit: false,
        check_on_maintenance: true,
    };
    let h = FuseHarness::new_with_cache_and_invalidation(1.0, 72, &cfg).unwrap();

    write_backing_file(&h, "show.mkv", b"backing_data__");
    let cache_file = h.cache_path().join("show.mkv");
    std::fs::write(&cache_file, b"cached__data__").unwrap();

    let meta = std::fs::metadata(h.backing_path().join("show.mkv")).unwrap();
    // Simulate pre-migration row: size set, mtime fields zero.
    h.cache_mgr().mark_cached(
        Path::new("show.mkv"),
        SourceMetadata::test_file(meta.len(), 0, 0),
    );
    std::thread::sleep(Duration::from_millis(50));

    // Sweep must backfill, NOT drop.
    let (checked, dropped) = h.cache_mgr().sweep_stale();
    assert_eq!(checked, 1);
    assert_eq!(
        dropped, 0,
        "sweep must not drop a zero-fingerprint row — it should backfill"
    );
    assert!(
        cache_file.exists(),
        "cache file must survive a backfill sweep"
    );

    // Fingerprint must now be populated.
    let fp = h
        .cache_mgr()
        .cache_db()
        .fingerprint_row(Path::new("show.mkv"), h.cache_mgr().mount_id())
        .expect("fingerprint row must exist after backfill sweep");
    assert_ne!(
        fp.source_mtime_secs, 0,
        "mtime_secs must be non-zero after sweep backfill"
    );

    // Read through FUSE still returns cached content.
    let data = std::fs::read(h.mount_path().join("show.mkv")).unwrap();
    assert_eq!(
        data, b"cached__data__",
        "cache must still be served after backfill sweep"
    );
}

/// `sweep_stale()` drops an entry whose backing file was deleted.
#[test]
fn e2e_direct_sweep_drops_entry_when_backing_deleted() {
    let cfg = InvalidationConfig {
        check_on_hit: false,
        check_on_maintenance: true,
    };
    let (h, backing_path, cache_file) =
        setup_hot_cache(&cfg, "movie.mkv", b"backing_v1____", b"cached___v1___");

    std::fs::remove_file(&backing_path).unwrap();

    let (checked, dropped) = h.cache_mgr().sweep_stale();
    assert_eq!(checked, 1);
    assert_eq!(
        dropped, 1,
        "sweep must drop entry when backing file is gone"
    );
    assert!(
        !cache_file.exists(),
        "cache file must be deleted when backing is gone"
    );

    // After sweep, FUSE returns ENOENT.
    let result = std::fs::read(h.mount_path().join("movie.mkv"));
    assert!(
        result.is_err(),
        "expected ENOENT after cache and backing are both gone"
    );
}

/// Sweep with multiple files correctly distinguishes fresh and stale entries,
/// dropping only the stale ones and leaving fresh entries intact.
#[test]
fn e2e_direct_sweep_mixed_fresh_and_stale() {
    let cfg = InvalidationConfig {
        check_on_hit: false,
        check_on_maintenance: true,
    };
    let h = FuseHarness::new_with_cache_and_invalidation(1.0, 72, &cfg).unwrap();

    for name in &["fresh.mkv", "stale.mkv"] {
        write_backing_file(&h, name, b"backing_v1____");
        std::fs::write(h.cache_path().join(name), b"cached___v1___").unwrap();
        let meta = std::fs::metadata(h.backing_path().join(name)).unwrap();
        h.cache_mgr()
            .mark_cached(Path::new(name), SourceMetadata::from_metadata(&meta));
    }
    std::thread::sleep(Duration::from_millis(50));

    // Update only the stale file (same size — mtime is the signal).
    std::fs::write(h.backing_path().join("stale.mkv"), b"backing__v2___").unwrap();

    let (checked, dropped) = h.cache_mgr().sweep_stale();
    assert_eq!(checked, 2);
    assert_eq!(dropped, 1, "only the stale entry should be dropped");

    assert!(
        h.cache_path().join("fresh.mkv").exists(),
        "fresh entry must survive sweep"
    );
    assert!(
        !h.cache_path().join("stale.mkv").exists(),
        "stale entry must be removed by sweep"
    );

    // Fresh file still served from cache.
    let fresh = std::fs::read(h.mount_path().join("fresh.mkv")).unwrap();
    assert_eq!(fresh, b"cached___v1___");

    // Stale file now falls through to updated backing.
    let stale = std::fs::read(h.mount_path().join("stale.mkv")).unwrap();
    assert_eq!(stale, b"backing__v2___");
}
