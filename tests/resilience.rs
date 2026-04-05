/// Resilience and edge-case tests: missing files, concurrent access,
/// partial-copy cleanup, and graceful fallback after cache eviction.
mod common;
use common::{write_backing_file, FuseHarness};
use std::time::Duration;

// ---- Missing / nonexistent files ----

/// Reading a nonexistent file through FUSE returns an error, not a panic.
#[test]
fn read_missing_file_returns_error() {
    let h = FuseHarness::new().unwrap();
    let result = std::fs::read(h.mount_path().join("does/not/exist.mkv"));
    assert!(result.is_err(), "expected error for missing file, got Ok");
}

/// Stat a nonexistent file through FUSE returns an error, not a panic.
#[test]
fn stat_missing_file_returns_error() {
    let h = FuseHarness::new().unwrap();
    let result = std::fs::metadata(h.mount_path().join("ghost.mkv"));
    assert!(result.is_err(), "expected error for missing metadata, got Ok");
}

// ---- Concurrent access ----

/// Multiple threads reading the same file simultaneously through FUSE all
/// receive the correct, complete content.
#[test]
fn concurrent_reads_return_correct_data() {
    let h = FuseHarness::new().unwrap();
    // 64 KiB of recognisable data
    let content: Vec<u8> = (0u32..65536).map(|i| (i % 251) as u8).collect();
    write_backing_file(&h, "movie.mkv", &content);
    std::thread::sleep(Duration::from_millis(100));

    let mount_path = h.mount_path().to_path_buf();
    let expected = content.clone();

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = mount_path.join("movie.mkv");
            let exp = expected.clone();
            std::thread::spawn(move || {
                let data = std::fs::read(&path).unwrap();
                assert_eq!(data, exp, "concurrent read returned wrong content");
            })
        })
        .collect();

    for h in handles {
        h.join().expect("reader thread panicked");
    }
}

/// Multiple threads reading different files simultaneously all get correct content.
#[test]
fn concurrent_reads_different_files() {
    let h = FuseHarness::new().unwrap();
    for i in 0..8u8 {
        write_backing_file(&h, &format!("file{}.bin", i), &[i; 1024]);
    }
    std::thread::sleep(Duration::from_millis(100));

    let mount_path = h.mount_path().to_path_buf();
    let handles: Vec<_> = (0..8u8)
        .map(|i| {
            let path = mount_path.join(format!("file{}.bin", i));
            std::thread::spawn(move || {
                let data = std::fs::read(&path).unwrap();
                assert_eq!(data, vec![i; 1024]);
            })
        })
        .collect();

    for h in handles {
        h.join().expect("reader thread panicked");
    }
}

// ---- Partial-copy cleanup ----

/// copy_to_cache cleans up the .partial file when the source does not exist.
#[test]
fn copier_no_partial_left_on_source_missing() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use tempfile::TempDir;

    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let dest = cache_dir.path().join("nonexistent.mkv");

    let c = CString::new(backing.path().as_os_str().as_bytes()).unwrap();
    let backing_fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY) };
    assert!(backing_fd >= 0);

    // Source file does not exist → copy must fail cleanly
    let result = plex_hot_cache::copier::copy_to_cache(
        backing_fd,
        std::path::Path::new("nonexistent.mkv"),
        &dest,
    );
    assert!(result.is_err(), "expected copy to fail for missing source");

    // No .partial file should be present
    let mut partial = dest.as_os_str().to_owned();
    partial.push(".partial");
    assert!(
        !std::path::Path::new(&partial).exists(),
        ".partial must not exist after a failed copy"
    );

    unsafe { libc::close(backing_fd) };
}

/// A pre-existing .partial file in the cache dir is invisible through FUSE
/// and is removed on the next CacheManager startup cleanup.
#[test]
fn orphaned_partial_is_cleaned_and_invisible() {
    use plex_hot_cache::cache::CacheManager;
    use std::sync::Arc;
    use tempfile::TempDir;

    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    // Plant an orphaned .partial (simulates a previous crash mid-copy)
    let sub = cache_dir.path().join("tv/Show");
    std::fs::create_dir_all(&sub).unwrap();
    let partial = sub.join("Show.S01E02.mkv.partial");
    std::fs::write(&partial, b"incomplete data").unwrap();

    // Also write the backing file so the FUSE mount can serve it
    let backing_sub = backing.path().join("tv/Show");
    std::fs::create_dir_all(&backing_sub).unwrap();
    std::fs::write(backing_sub.join("Show.S01E02.mkv"), b"backing data").unwrap();

    // CacheManager startup_cleanup should remove the .partial
    let mgr = Arc::new(CacheManager::new(cache_dir.path().to_path_buf(), cache_dir.path().to_path_buf(), 1.0, 72, 0.0));
    mgr.startup_cleanup();
    assert!(!partial.exists(), ".partial must be removed by startup_cleanup");

    // FUSE mount should serve the backing file (not the now-deleted partial)
    let mut fs = plex_hot_cache::fuse_fs::PlexHotCacheFs::new(backing.path()).unwrap();
    fs.cache = Some(Arc::clone(&mgr));
    let mount = TempDir::new().unwrap();
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::RO,
        fuser::MountOption::FSName("test".to_string()),
    ];
    config.acl = fuser::SessionACL::Owner;
    let _session = fuser::spawn_mount2(fs, mount.path(), &config).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let data = std::fs::read(mount.path().join("tv/Show/Show.S01E02.mkv")).unwrap();
    assert_eq!(data, b"backing data");
}

// ---- Fallback to backing store ----

/// Files with a cache entry are served from the SSD cache; files without one
/// fall back to the backing store. Both paths are exercised on the same mount.
///
/// Note: we use TWO DISTINCT files to avoid the OS kernel page cache returning
/// stale data from an earlier read of the same file — page caches persist
/// across open/close cycles and are an OS concern, not a FUSE concern.
#[test]
fn fuse_falls_back_to_backing_on_cache_miss() {
    use plex_hot_cache::cache::CacheManager;
    use std::sync::Arc;
    use tempfile::TempDir;

    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();

    // Both files live in the backing store.
    // IMPORTANT: the cache copy must be the same size as the backing copy because
    // getattr/lookup always stats from the backing store and the kernel uses that
    // size to bound its reads. In production, cache files are always exact copies.
    std::fs::write(backing.path().join("cached.mkv"),   b"backing___A").unwrap(); // 11 bytes
    std::fs::write(backing.path().join("uncached.mkv"), b"backing___B").unwrap(); // 11 bytes

    // Only "cached.mkv" has an SSD copy — same size, different byte content
    std::fs::write(cache_dir.path().join("cached.mkv"), b"ssdcached_A").unwrap(); // 11 bytes

    let mgr = Arc::new(CacheManager::new(cache_dir.path().to_path_buf(), cache_dir.path().to_path_buf(), 1.0, 72, 0.0));
    let mut fs = plex_hot_cache::fuse_fs::PlexHotCacheFs::new(backing.path()).unwrap();
    fs.cache = Some(Arc::clone(&mgr));

    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::RO,
        fuser::MountOption::FSName("test".to_string()),
    ];
    config.acl = fuser::SessionACL::Owner;
    let _session = fuser::spawn_mount2(fs, mount.path(), &config).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    // Cache hit: served from SSD
    let data = std::fs::read(mount.path().join("cached.mkv")).unwrap();
    assert_eq!(data, b"ssdcached_A", "cached file should be served from SSD");

    // Cache miss: falls through to backing store
    let data = std::fs::read(mount.path().join("uncached.mkv")).unwrap();
    assert_eq!(data, b"backing___B", "uncached file should be served from backing store");
}
