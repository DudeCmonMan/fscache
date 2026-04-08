mod common;

use std::sync::Arc;
use std::time::Duration;
use common::{
    MultiFuseHarness, write_multi_backing_file, read_multi_mount_file, collect_files,
};
use fscache::cache::db::CacheDb;
use fscache::utils::{mount_cache_name, validate_targets};

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
// Global cache budget
// ---------------------------------------------------------------------------

#[test]
fn global_eviction_respects_total_budget() {
    use fscache::cache::manager::CacheManager;

    // Two mounts sharing a 2 KB global budget.  Each file is ~600 bytes,
    // so after writing 2 files to each mount (4 files total, ~2.4 KB) the
    // global total exceeds the budget and eviction must trim below it.
    let budget_bytes: u64 = 2048;
    let budget_gb = budget_bytes as f64 / 1_073_741_824.0;

    let harness = MultiFuseHarness::new_with_cache(2, budget_gb, 9999).unwrap();
    let content = vec![b'x'; 600];

    // Write files directly into each mount's cache subdir (bypassing FUSE copy
    // pipeline — we just want to test the eviction math).
    for mount_idx in 0..2usize {
        let cache_dir = harness.cache_subdir(mount_idx);
        for file_idx in 0..2u32 {
            let path = cache_dir.join(format!("file{file_idx}.mkv"));
            std::fs::write(&path, &content).unwrap();
        }
    }

    let global_total: u64 = (0..2)
        .flat_map(|i| collect_files(&harness.cache_subdir(i)))
        .filter_map(|p| std::fs::metadata(&p).ok())
        .map(|m| m.len())
        .sum();
    assert!(global_total > budget_bytes, "test setup: expected to exceed budget ({global_total} bytes)");

    // Re-create the manager with the same dirs and budget so we can call evict directly.
    // Both managers share the same DB (same shared_cache_base) for global budget tracking.
    let shared_db = Arc::new(CacheDb::open(&harness.shared_cache_base.path().join("test.db")).unwrap());
    let mgr0 = CacheManager::new(
        harness.cache_subdir(0),
        Arc::clone(&shared_db),
        harness.shared_cache_base.path().to_path_buf(),
        budget_gb,
        9999,
        0.0,
    );
    let mgr1 = CacheManager::new(
        harness.cache_subdir(1),
        Arc::clone(&shared_db),
        harness.shared_cache_base.path().to_path_buf(),
        budget_gb,
        9999,
        0.0,
    );

    // Register all files in the DB so eviction candidates are visible.
    for (mgr, mount_idx) in [(&mgr0, 0usize), (&mgr1, 1usize)] {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for file_idx in 0..2u32 {
            let rel_str = format!("file{file_idx}.mkv");
            let rel = std::path::Path::new(&rel_str);
            mgr.mark_cached(rel, content.len() as u64);
            // Stagger timestamps so LRU ordering is deterministic.
            let ts = now - (mount_idx as i64 * 2 + file_idx as i64) * 10;
            let mount_id = harness.cache_subdir(mount_idx).to_string_lossy().into_owned();
            mgr.cache_db().set_last_hit_at_for_test(rel, &mount_id, ts);
        }
    }

    mgr0.evict_if_needed();

    let after: u64 = (0..2)
        .flat_map(|i| collect_files(&harness.cache_subdir(i)))
        .filter_map(|p| std::fs::metadata(&p).ok())
        .map(|m| m.len())
        .sum();
    assert!(
        after <= budget_bytes,
        "global total after eviction ({after} bytes) still exceeds budget ({budget_bytes} bytes)"
    );
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

// ---------------------------------------------------------------------------
// Config validation and cache naming
// ---------------------------------------------------------------------------

#[test]
fn mount_cache_name_same_basename_different_paths_are_distinct() {
    let a = std::path::Path::new("/mnt/a/media");
    let b = std::path::Path::new("/mnt/b/media");
    let name_a = mount_cache_name(a);
    let name_b = mount_cache_name(b);
    assert_ne!(name_a, name_b, "same-basename paths must get distinct cache names");
    // Both names should be human-readable (contain the basename).
    assert!(name_a.contains("media"), "cache name should contain path component: {name_a}");
    assert!(name_b.contains("media"), "cache name should contain path component: {name_b}");
}

#[test]
fn mount_cache_name_same_path_is_stable() {
    let p = std::path::Path::new("/mnt/nas1/movies");
    assert_eq!(mount_cache_name(p), mount_cache_name(p));
}

#[test]
fn same_basename_mounts_get_separate_cache_dirs() {
    let parent_a = tempfile::TempDir::new().unwrap();
    let parent_b = tempfile::TempDir::new().unwrap();
    let dir_a = parent_a.path().join("media");
    let dir_b = parent_b.path().join("media");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    let name_a = mount_cache_name(&dir_a);
    let name_b = mount_cache_name(&dir_b);
    assert_ne!(name_a, name_b, "same-basename temp dirs must get distinct cache names");
}

#[test]
fn duplicate_targets_are_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let targets = vec![dir.path().to_path_buf(), dir.path().to_path_buf()];
    let err = validate_targets(&targets).unwrap_err();
    assert!(err.to_string().contains("duplicate"), "expected duplicate error, got: {err}");
}

#[test]
fn nonexistent_target_is_rejected() {
    let targets = vec![std::path::PathBuf::from("/this/path/does/not/exist/ever")];
    let err = validate_targets(&targets).unwrap_err();
    assert!(err.to_string().contains("does not exist"), "expected not-found error, got: {err}");
}

#[test]
fn empty_target_list_is_rejected() {
    let err = validate_targets(&[]).unwrap_err();
    assert!(err.to_string().contains("empty"), "expected empty-list error, got: {err}");
}
