mod common;

use std::time::Duration;
use common::{
    MultiFuseHarness, write_multi_backing_file, read_multi_mount_file, collect_files,
};

// ---------------------------------------------------------------------------
// Basic multi-mount operation
// ---------------------------------------------------------------------------

#[test]
fn two_mounts_independent_reads() {
    let harness = MultiFuseHarness::new_with_cache(2, 1.0, 72).unwrap();
    write_multi_backing_file(&harness, 0, "a.mkv", b"content-from-mount-0");
    write_multi_backing_file(&harness, 1, "a.mkv", b"content-from-mount-1");

    assert_eq!(read_multi_mount_file(&harness, 0, "a.mkv"), b"content-from-mount-0");
    assert_eq!(read_multi_mount_file(&harness, 1, "a.mkv"), b"content-from-mount-1");
}

#[test]
fn three_mounts_all_serve_files() {
    let harness = MultiFuseHarness::new_with_cache(3, 1.0, 72).unwrap();
    for i in 0..3 {
        write_multi_backing_file(&harness, i, "video.mkv", format!("mount-{i}").as_bytes());
    }
    for i in 0..3 {
        let got = read_multi_mount_file(&harness, i, "video.mkv");
        assert_eq!(got, format!("mount-{i}").as_bytes());
    }
}

#[test]
fn mounts_have_independent_inodes() {
    // Same relative path on two mounts can have different content — no inode collision.
    let harness = MultiFuseHarness::new_with_cache(2, 1.0, 72).unwrap();
    write_multi_backing_file(&harness, 0, "show/S01E01.mkv", b"episode-on-drive-0");
    write_multi_backing_file(&harness, 1, "show/S01E01.mkv", b"episode-on-drive-1");

    assert_eq!(read_multi_mount_file(&harness, 0, "show/S01E01.mkv"), b"episode-on-drive-0");
    assert_eq!(read_multi_mount_file(&harness, 1, "show/S01E01.mkv"), b"episode-on-drive-1");
}

// ---------------------------------------------------------------------------
// Cache isolation
// ---------------------------------------------------------------------------

#[test]
fn cache_dirs_are_namespaced() {
    let harness = MultiFuseHarness::new_with_cache(2, 1.0, 72).unwrap();
    write_multi_backing_file(&harness, 0, "file.mkv", b"data0");
    write_multi_backing_file(&harness, 1, "file.mkv", b"data1");

    // Trigger a read through each mount to populate caches.
    let _ = read_multi_mount_file(&harness, 0, "file.mkv");
    let _ = read_multi_mount_file(&harness, 1, "file.mkv");

    // Brief wait for copy tasks to flush.
    std::thread::sleep(Duration::from_millis(200));

    let cache0 = harness.cache_subdir(0);
    let cache1 = harness.cache_subdir(1);

    // Each cache subdir is distinct and neither is inside the other.
    assert_ne!(cache0, cache1);
    assert!(!cache0.starts_with(&cache1));
    assert!(!cache1.starts_with(&cache0));
}

#[test]
fn cache_hit_on_one_mount_no_effect_on_other() {
    let harness = MultiFuseHarness::new_with_cache(2, 1.0, 72).unwrap();
    write_multi_backing_file(&harness, 0, "video.mkv", b"from-mount-0");
    // No corresponding file on mount 1's backing dir.

    let _ = read_multi_mount_file(&harness, 0, "video.mkv");

    // Mount 1's cache dir must remain empty.
    std::thread::sleep(Duration::from_millis(100));
    let cache1_files = collect_files(&harness.cache_subdir(1));
    assert!(
        cache1_files.is_empty(),
        "mount-1 cache should be empty but found: {:?}",
        cache1_files
    );
}

#[test]
fn cache_subdirs_serve_correct_content() {
    // Write a pre-cached file directly into each mount's cache subdir and verify
    // that reads through the FUSE mount serve the cache content, not the (absent)
    // backing content.  This confirms each mount reads from its own cache subdir.
    let harness = MultiFuseHarness::new_with_cache(2, 1.0, 72).unwrap();

    // Write backing files so the FUSE FS has an inode to look up.
    write_multi_backing_file(&harness, 0, "video.mkv", b"backing-0");
    write_multi_backing_file(&harness, 1, "video.mkv", b"backing-1");

    // Pre-populate each mount's own cache subdir with distinct content.
    let cache0_path = harness.cache_subdir(0).join("video.mkv");
    let cache1_path = harness.cache_subdir(1).join("video.mkv");
    std::fs::write(&cache0_path, b"cached-0").unwrap();
    std::fs::write(&cache1_path, b"cached-1").unwrap();

    // Reads should hit the cache (each mount's own subdir).
    assert_eq!(read_multi_mount_file(&harness, 0, "video.mkv"), b"cached-0");
    assert_eq!(read_multi_mount_file(&harness, 1, "video.mkv"), b"cached-1");
}

// ---------------------------------------------------------------------------
// Prediction isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prediction_scoped_to_mount() {
    let harness = MultiFuseHarness::new_full_pipeline(2, 2).unwrap();

    // Populate episodes on mount 0 only.
    for ep in 1..=4u32 {
        write_multi_backing_file(
            &harness,
            0,
            &format!("Show/Show - S01E{ep:02}.mkv"),
            b"data",
        );
    }

    // Access E01 on mount 0 — predictor should queue E02/E03 for caching on mount 0.
    let _ = read_multi_mount_file(&harness, 0, "Show/Show - S01E01.mkv");

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Cache subdir for mount 1 should be empty.
    let cache1_files = collect_files(&harness.cache_subdir(1));
    assert!(
        cache1_files.is_empty(),
        "prediction on mount-0 must not write to mount-1 cache, found: {:?}",
        cache1_files
    );
}

#[tokio::test]
async fn concurrent_prediction_both_mounts() {
    let harness = MultiFuseHarness::new_full_pipeline(2, 2).unwrap();

    // Populate episodes on both mounts.
    for ep in 1..=4u32 {
        write_multi_backing_file(&harness, 0, &format!("ShowA/ShowA - S01E{ep:02}.mkv"), b"data-a");
        write_multi_backing_file(&harness, 1, &format!("ShowB/ShowB - S01E{ep:02}.mkv"), b"data-b");
    }

    // Trigger prediction on both mounts simultaneously.
    let _ = read_multi_mount_file(&harness, 0, "ShowA/ShowA - S01E01.mkv");
    let _ = read_multi_mount_file(&harness, 1, "ShowB/ShowB - S01E01.mkv");

    tokio::time::sleep(Duration::from_millis(800)).await;

    // Each mount's cache should have files; neither should bleed into the other.
    let cache0_files = collect_files(&harness.cache_subdir(0));
    let cache1_files = collect_files(&harness.cache_subdir(1));

    assert!(
        cache0_files.iter().any(|p| p.to_string_lossy().contains("ShowA")),
        "ShowA not found in cache0: {:?}",
        cache0_files
    );
    assert!(
        cache1_files.iter().any(|p| p.to_string_lossy().contains("ShowB")),
        "ShowB not found in cache1: {:?}",
        cache1_files
    );
    assert!(
        !cache0_files.iter().any(|p| p.to_string_lossy().contains("ShowB")),
        "ShowB leaked into cache0: {:?}",
        cache0_files
    );
    assert!(
        !cache1_files.iter().any(|p| p.to_string_lossy().contains("ShowA")),
        "ShowA leaked into cache1: {:?}",
        cache1_files
    );
}

// ---------------------------------------------------------------------------
// Graceful degradation
// ---------------------------------------------------------------------------

#[test]
fn missing_file_on_one_mount_does_not_affect_other() {
    let harness = MultiFuseHarness::new_with_cache(2, 1.0, 72).unwrap();
    write_multi_backing_file(&harness, 1, "healthy.mkv", b"healthy-data");
    // Mount 0 has no files.

    // Read from mount 0 should fail gracefully (ENOENT).
    let result = std::fs::read(harness.mount_path(0).join("missing.mkv"));
    assert!(result.is_err(), "expected ENOENT on mount 0");

    // Mount 1 should still serve its file correctly.
    assert_eq!(read_multi_mount_file(&harness, 1, "healthy.mkv"), b"healthy-data");
}

#[test]
fn concurrent_reads_across_mounts() {
    use std::sync::Arc;
    use std::thread;

    let harness = Arc::new(MultiFuseHarness::new_with_cache(2, 1.0, 72).unwrap());

    for i in 0..2usize {
        for j in 0..4u32 {
            write_multi_backing_file(
                &harness,
                i,
                &format!("file{j}.mkv"),
                format!("mount{i}-file{j}").as_bytes(),
            );
        }
    }

    let mut handles = Vec::new();
    for mount_idx in 0..2usize {
        for _ in 0..4 {
            let h = Arc::clone(&harness);
            handles.push(thread::spawn(move || {
                for j in 0..4u32 {
                    let got = std::fs::read(h.mount_path(mount_idx).join(format!("file{j}.mkv")))
                        .unwrap();
                    let expected = format!("mount{mount_idx}-file{j}");
                    assert_eq!(got, expected.as_bytes());
                }
            }));
        }
    }
    for handle in handles {
        handle.join().expect("thread panicked");
    }
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

#[test]
fn all_sessions_drop_cleanly() {
    let mount_paths: Vec<_>;
    {
        let harness = MultiFuseHarness::new_with_cache(3, 1.0, 72).unwrap();
        mount_paths = (0..3).map(|i| harness.mount_path(i).to_path_buf()).collect();

        // Write and read through each mount to confirm they're active.
        for i in 0..3 {
            write_multi_backing_file(&harness, i, "test.mkv", b"data");
            assert_eq!(read_multi_mount_file(&harness, i, "test.mkv"), b"data");
        }
        // harness drops here — all sessions unmounted, TempDirs cleaned up
    }

    // After drop, the former mount paths (now cleaned TempDirs) should not be
    // accessible as FUSE mounts. We just verify no panic occurred during drop.
    // (TempDir removal would fail if a FUSE mount were still active.)
    drop(mount_paths);
}
