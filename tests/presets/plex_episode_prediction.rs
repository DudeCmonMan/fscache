/// End-to-end tests for the `plex-episode-prediction` preset.
///
/// PlexEpisodePrediction uses SxxExx regex lookahead: on every cache miss it
/// scans the backing store for the next N episodes and queues them for caching.
/// Supports same-season and cross-season prediction, structured and flat layouts.
///
/// Additionally filters Plex Transcoder analysis processes (intro detection,
/// thumbnails, fingerprinting) while allowing real playback through.
/// Unit tests for the cmdline detection logic live in:
///   src/presets/plex_episode_prediction.rs
///
/// These tests exercise the full pipeline:
///   FUSE open() → AccessEvent → ActionEngine → PlexEpisodePrediction::on_miss()
///   → find_next_episodes() → CopyRequest → copier → mark_cached()
///   → subsequent reads served from SSD.
use crate::common::{write_backing_file, FuseHarness, OvermountHarness};
use std::time::Duration;

// ---- Core prediction behavior ----

/// Full pipeline: read E01 through FUSE → predictor caches E02–E05 →
/// subsequent reads of E02–E05 through FUSE are served from SSD cache.
///
/// The cache copies contain DIFFERENT content than the backing store.
/// We verify this by pre-populating the backing store, letting the pipeline
/// cache them, then overwriting the backing copies with new content.
/// The FUSE reads must still return the original (cached) content.
#[tokio::test]
async fn caches_and_serves_from_ssd() {
    let h = FuseHarness::new_full_pipeline(4).unwrap();

    for i in 1..=5u32 {
        write_backing_file(
            &h,
            &format!("tv/Show/Show.S01E0{}.mkv", i),
            format!("original content ep{}", i).as_bytes(),
        );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Read E01 through the FUSE mount — this triggers the access event.
    let e1_data = tokio::fs::read(h.mount_path().join("tv/Show/Show.S01E01.mkv"))
        .await
        .unwrap();
    assert_eq!(e1_data, b"original content ep1");

    // Give the predictor and copier time to complete all 4 copies.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Verify E02–E05 are now in the cache directory.
    for i in 2..=5u32 {
        let cached = h.cache_path().join(format!("tv/Show/Show.S01E0{}.mkv", i));
        assert!(cached.exists(), "expected ep{} in cache dir", i);
        assert_eq!(
            std::fs::read(&cached).unwrap(),
            format!("original content ep{}", i).as_bytes(),
            "cached content mismatch for ep{}",
            i
        );
    }

    // Overwrite the backing files. Subsequent FUSE reads must return the cached version.
    for i in 2..=5u32 {
        write_backing_file(
            &h,
            &format!("tv/Show/Show.S01E0{}.mkv", i),
            format!("overwritten backing ep{}", i).as_bytes(),
        );
    }

    for i in 2..=5u32 {
        let data = tokio::fs::read(h.mount_path().join(format!("tv/Show/Show.S01E0{}.mkv", i)))
            .await
            .unwrap();
        assert_eq!(
            data,
            format!("original content ep{}", i).as_bytes(),
            "ep{} should be served from cache, not overwritten backing",
            i
        );
    }
}

/// Cache hits do NOT trigger further prediction. Only misses advance the lookahead.
/// E01 miss → caches E02+E03. Reading E02 (hit) → no new caching. E04 miss → caches E05+E06.
#[tokio::test]
async fn advances_lookahead_only_on_miss() {
    let h = FuseHarness::new_full_pipeline(2).unwrap(); // lookahead = 2

    for i in 1..=6u32 {
        write_backing_file(
            &h,
            &format!("tv/Show/Show.S01E0{}.mkv", i),
            format!("ep{}", i).as_bytes(),
        );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    // E01 miss → should cache E02 and E03.
    let _ = tokio::fs::read(h.mount_path().join("tv/Show/Show.S01E01.mkv"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert!(h.cache_path().join("tv/Show/Show.S01E02.mkv").exists(), "E02 should be cached after E01 miss");
    assert!(h.cache_path().join("tv/Show/Show.S01E03.mkv").exists(), "E03 should be cached after E01 miss");
    assert!(!h.cache_path().join("tv/Show/Show.S01E04.mkv").exists(), "E04 should NOT be cached yet");

    // E02 cache hit → no new prediction.
    let e2_data = tokio::fs::read(h.mount_path().join("tv/Show/Show.S01E02.mkv"))
        .await
        .unwrap();
    assert_eq!(e2_data, b"ep2");
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert!(!h.cache_path().join("tv/Show/Show.S01E04.mkv").exists(), "E04 should NOT be cached after a cache hit");

    // E04 miss → should cache E05 and E06.
    let _ = tokio::fs::read(h.mount_path().join("tv/Show/Show.S01E04.mkv"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert!(h.cache_path().join("tv/Show/Show.S01E05.mkv").exists(), "E05 should be cached after E04 miss");
    assert!(h.cache_path().join("tv/Show/Show.S01E06.mkv").exists(), "E06 should be cached after E04 miss");
}

/// Prediction fires on the first open (immediate mode — no threshold).
#[tokio::test]
async fn triggers_prediction_immediately() {
    let h = FuseHarness::new_full_pipeline(4).unwrap();

    for i in 1..=4u32 {
        write_backing_file(&h, &format!("Show/Show.S01E0{}.mkv", i), b"episode data");
    }
    std::thread::sleep(Duration::from_millis(100));

    let _ = std::fs::read(h.mount_path().join("Show/Show.S01E01.mkv")).unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(
        h.cache_path().join("Show/Show.S01E02.mkv").exists(),
        "E02 must be cached immediately after opening E01"
    );
}

// ---- Overmount (production scenario) ----

/// True overmount E2E: FUSE is mounted ON TOP of the same directory the media
/// files live in, exactly as in production.
///
/// Reading a file goes through FUSE, which opens the real file via the
/// pre-mount O_PATH fd. The access event triggers the predictor, which copies
/// upcoming episodes to the SSD cache. Subsequent reads are served from cache.
///
/// This is the scenario Plex sees: it reads from `/mnt/media/...` unaware
/// that FUSE is intercepting every syscall.
#[tokio::test]
async fn overmount_caches_and_serves() {
    let h = OvermountHarness::new(4, |dir| {
        // All files must be written BEFORE the overmount — once FUSE is mounted
        // over this path the directory appears read-only to normal file operations.
        for i in 1..=5u32 {
            let path = dir.join(format!("tv/Show/Show.S01E0{}.mkv", i));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("original content ep{}", i)).unwrap();
        }
    })
    .unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let data = tokio::fs::read(h.path().join("tv/Show/Show.S01E01.mkv"))
        .await
        .unwrap();
    assert_eq!(data, b"original content ep1");

    tokio::time::sleep(Duration::from_millis(800)).await;

    for i in 2..=5u32 {
        let cached = h.cache_path().join(format!("tv/Show/Show.S01E0{}.mkv", i));
        assert!(cached.exists(), "E0{} should be in the cache dir", i);
        assert_eq!(
            std::fs::read(&cached).unwrap(),
            format!("original content ep{}", i).as_bytes(),
        );
    }

    for i in 2..=5u32 {
        let data = tokio::fs::read(h.path().join(format!("tv/Show/Show.S01E0{}.mkv", i)))
            .await
            .unwrap();
        assert_eq!(
            data,
            format!("original content ep{}", i).as_bytes(),
            "E0{} read through overmount should return original content",
            i
        );
    }
}

// ---- Process filtering (blocklist) ----

/// A process on the blocklist must never trigger prediction.
/// Uses `cat` as the blocked process — available on all Linux systems.
#[tokio::test]
async fn blocked_process_does_not_trigger_prediction() {
    let h = FuseHarness::new_full_pipeline_with_blocklist(4, vec!["cat".to_string()]).unwrap();

    for i in 1..=5u32 {
        write_backing_file(&h, &format!("Show/Show.S01E0{}.mkv", i), b"episode data");
    }
    std::thread::sleep(Duration::from_millis(100));

    let ep_path = h.mount_path().join("Show/Show.S01E01.mkv");
    let mut child = std::process::Command::new("cat")
        .arg(&ep_path)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _ = child.wait();

    tokio::time::sleep(Duration::from_secs(2)).await;

    for i in 2..=5u32 {
        assert!(
            !h.cache_path().join(format!("Show/Show.S01E0{}.mkv", i)).exists(),
            "E0{} must not be cached when opener is on the process blocklist", i
        );
    }
}

/// A child of a blocklisted process must also be blocked (ancestor walk).
/// Simulates Plex Media Scanner spawning Plex Transcoder for analysis.
#[tokio::test]
async fn child_of_blocked_process_is_also_blocked() {
    let h = FuseHarness::new_full_pipeline_with_blocklist(4, vec!["bash".to_string()]).unwrap();

    for i in 1..=5u32 {
        write_backing_file(&h, &format!("Show/Show.S01E0{}.mkv", i), b"episode data");
    }
    std::thread::sleep(Duration::from_millis(100));

    let ep_path = h.mount_path().join("Show/Show.S01E01.mkv");
    let mut child = std::process::Command::new("bash")
        .arg("-c")
        .arg(format!("cat {}", ep_path.display()))
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _ = child.wait();

    tokio::time::sleep(Duration::from_secs(2)).await;

    for i in 2..=5u32 {
        assert!(
            !h.cache_path().join(format!("Show/Show.S01E0{}.mkv", i)).exists(),
            "E0{} must not be cached when opener's parent (bash) is blocklisted", i
        );
    }
}
