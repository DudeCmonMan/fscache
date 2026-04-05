mod common;
use common::{write_backing_file, FuseHarness};
use std::time::Duration;

// These tests use a short threshold (2s) to keep test runtime reasonable.

/// A file read briefly (< threshold) must NOT trigger prediction.
/// Simulates a Plex Media Scanner header probe.
#[tokio::test]
async fn short_read_does_not_trigger_prediction() {
    let threshold = Duration::from_secs(2);
    let h = FuseHarness::new_full_pipeline_with_threshold(4, threshold).unwrap();

    // Write episodes so there is something for the predictor to queue.
    for i in 1..=4u32 {
        write_backing_file(&h, &format!("Show/Show.S01E0{}.mkv", i), b"data");
    }
    std::thread::sleep(Duration::from_millis(100));

    // Open and immediately close E01 — simulates a scanner probe.
    let _ = std::fs::read(h.mount_path().join("Show/Show.S01E01.mkv")).unwrap();

    // Wait less than the threshold, then confirm nothing was cached.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let cache_path = h.cache_path();
    for i in 2..=4u32 {
        assert!(
            !cache_path.join(format!("Show/Show.S01E0{}.mkv", i)).exists(),
            "E0{} must not be cached after a sub-threshold read", i
        );
    }
}

/// A file read for longer than the threshold MUST trigger prediction.
/// Simulates real user playback.
#[tokio::test]
async fn sustained_read_triggers_prediction() {
    let threshold = Duration::from_secs(2);
    let h = FuseHarness::new_full_pipeline_with_threshold(4, threshold).unwrap();

    for i in 1..=5u32 {
        write_backing_file(&h, &format!("Show/Show.S01E0{}.mkv", i), b"episode data");
    }
    std::thread::sleep(Duration::from_millis(100));

    // Open the file (inserts tracking entry with opened_at = now), sleep past the
    // threshold, then issue one read.  The threshold check fires on that read because
    // opened_at.elapsed() >= threshold at that point.
    //
    // We can't use a tight read loop here: after the first full pass over a small
    // file the kernel page cache serves subsequent reads without calling FUSE read(),
    // so the threshold check would never fire on re-reads of the same pages.
    use std::io::Read;
    let ep_path = h.mount_path().join("Show/Show.S01E01.mkv");
    let mut file = std::fs::File::open(&ep_path).unwrap();
    tokio::time::sleep(threshold + Duration::from_millis(200)).await;
    let mut buf = vec![0u8; 4096];
    let _ = file.read(&mut buf).unwrap();
    drop(file);

    // Give the predictor time to queue and copy the lookahead episodes.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let cache_path = h.cache_path();
    assert!(
        cache_path.join("Show/Show.S01E02.mkv").exists(),
        "E02 must be cached after sustained playback"
    );
}

/// Zero threshold (legacy mode): prediction fires on the first open, no reading required.
#[tokio::test]
async fn zero_threshold_fires_immediately() {
    let h = FuseHarness::new_full_pipeline(4).unwrap(); // threshold = 0 by default

    for i in 1..=4u32 {
        write_backing_file(&h, &format!("Show/Show.S01E0{}.mkv", i), b"episode data");
    }
    std::thread::sleep(Duration::from_millis(100));

    // Single quick read of E01.
    let _ = std::fs::read(h.mount_path().join("Show/Show.S01E01.mkv")).unwrap();

    tokio::time::sleep(Duration::from_millis(800)).await;

    let cache_path = h.cache_path();
    assert!(
        cache_path.join("Show/Show.S01E02.mkv").exists(),
        "E02 must be cached immediately with zero threshold"
    );
}

/// A process on the blocklist must never trigger prediction, even past the threshold.
/// Uses `cat` as the blocked process — available on all Linux systems.
#[tokio::test]
async fn blocked_process_does_not_trigger_prediction() {
    let threshold = Duration::from_secs(2);
    let h = FuseHarness::new_full_pipeline_with_blocklist(
        4, threshold, vec!["cat".to_string()]
    ).unwrap();

    for i in 1..=5u32 {
        write_backing_file(&h, &format!("Show/Show.S01E0{}.mkv", i), b"episode data");
    }
    std::thread::sleep(Duration::from_millis(100));

    // `cat` opens and reads the file through the FUSE mount. Because "cat" is on the
    // blocklist, open() skips creating an OpenFileState — no prediction ever fires.
    let ep_path = h.mount_path().join("Show/Show.S01E01.mkv");
    let mut child = std::process::Command::new("cat")
        .arg(&ep_path)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _ = child.wait();

    // Wait well past the threshold to ensure no deferred event sneaks through.
    tokio::time::sleep(threshold + Duration::from_secs(3)).await;

    let cache_path = h.cache_path();
    for i in 2..=5u32 {
        assert!(
            !cache_path.join(format!("Show/Show.S01E0{}.mkv", i)).exists(),
            "E0{} must not be cached when opener is on the process blocklist", i
        );
    }
}

/// With CacheMissOnly strategy and non-zero threshold, reading a cached file past
/// the threshold must NOT fire a prediction event (the handle is not tracked).
#[tokio::test]
async fn cache_hit_not_tracked_under_cache_miss_only() {
    use plex_hot_cache::cache::CacheManager;
    use plex_hot_cache::fuse_fs::PlexHotCacheFs;
    use plex_hot_cache::predictor::{run_copier_task, AccessEvent, CopyRequest, Predictor};
    use plex_hot_cache::scheduler::Scheduler;
    use fuser::{MountOption, SessionACL};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    let backing = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    // Pre-populate E01 in both backing and cache.
    let content = vec![b'x'; 4 * 1024 * 1024]; // 4 MB
    std::fs::write(backing.path().join("Show.S01E01.mkv"), &content).unwrap();
    std::fs::write(cache_dir.path().join("Show.S01E01.mkv"), &content).unwrap();

    // Write E02 in backing only (would be the prediction target if the event fired).
    std::fs::write(backing.path().join("Show.S01E02.mkv"), b"next ep").unwrap();

    let mut fs = PlexHotCacheFs::new(backing.path()).unwrap();
    fs.playback_threshold = Duration::from_secs(2);
    let backing_fd = fs.backing_fd;

    let cache_mgr = Arc::new(CacheManager::new(
        cache_dir.path().to_path_buf(),
        cache_dir.path().to_path_buf(),
        1.0,
        72,
        0.0,
    ));
    cache_mgr.startup_cleanup();
    fs.cache = Some(Arc::clone(&cache_mgr));

    let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
    let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(64);
    fs.access_tx = Some(access_tx);

    let scheduler = Scheduler::new("00:00", "23:59").unwrap();
    let predictor = Predictor::new(
        access_rx, copy_tx, Arc::clone(&cache_mgr), 2, None, scheduler,
        backing_fd, 0, cache_dir.path().to_path_buf(), 0,
    );
    tokio::spawn(predictor.run());
    tokio::spawn(run_copier_task(backing_fd, copy_rx, Arc::clone(&cache_mgr)));

    let mut config = fuser::Config::default();
    config.mount_options = vec![MountOption::RO, MountOption::FSName("test".to_string())];
    config.acl = SessionACL::Owner;
    let _session = fuser::spawn_mount2(fs, mount.path(), &config).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    // Read the cached E01 repeatedly for well past the threshold.
    let ep_path = mount.path().join("Show.S01E01.mkv");
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let _ = std::fs::read(&ep_path).unwrap();
    }

    tokio::time::sleep(Duration::from_millis(800)).await;

    // E02 must NOT be cached — cache-hit handles are not tracked under CacheMissOnly.
    assert!(
        !cache_dir.path().join("Show.S01E02.mkv").exists(),
        "E02 must not be cached: cache-hit handles are not tracked under CacheMissOnly"
    );
}
