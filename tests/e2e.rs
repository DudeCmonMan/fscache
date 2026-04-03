/// True end-to-end tests: FUSE mount + cache overlay + predictor + copier all
/// wired together exactly as they run in production.
///
/// The FUSE kernel callbacks call open(), which sends an AccessEvent through
/// the channel to the predictor task, which scans the backing dir via regex,
/// enqueues copy requests, and the copier task writes files into the cache dir.
/// Subsequent reads through the FUSE mount are served from the SSD cache.
mod common;
use common::{write_backing_file, FuseHarness};
use std::time::Duration;

fn wait_for_pipeline() {
    // Generous sleep: predictor scans dir + copier copies N small files.
    std::thread::sleep(Duration::from_millis(800));
}

/// Full pipeline: read E01 through FUSE → predictor caches E02–E05 →
/// subsequent reads of E02–E05 through FUSE are served from SSD cache.
///
/// The cache copies contain DIFFERENT content than the backing store.
/// We verify this by pre-populating the backing store, letting the pipeline
/// cache them, then overwriting the backing copies with new content.
/// The FUSE reads must still return the original (cached) content.
#[tokio::test]
async fn full_pipeline_caches_and_serves_from_ssd() {
    let h = FuseHarness::new_full_pipeline(4).unwrap();

    // Write 5 episodes to the backing store
    for i in 1..=5u32 {
        write_backing_file(
            &h,
            &format!("tv/Show/Show.S01E0{}.mkv", i),
            format!("original content ep{}", i).as_bytes(),
        );
    }

    // Wait for FUSE to see the directory (lookup happens lazily)
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Read E01 through the FUSE mount — this triggers the access event
    let e1_data = tokio::fs::read(h.mount_path().join("tv/Show/Show.S01E01.mkv"))
        .await
        .unwrap();
    assert_eq!(e1_data, b"original content ep1");

    // Give the predictor and copier time to complete all 4 copies
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Verify E02–E05 are now in the cache directory
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

    // Now overwrite the backing files with different content.
    // Subsequent FUSE reads should still return the cached version.
    for i in 2..=5u32 {
        write_backing_file(
            &h,
            &format!("tv/Show/Show.S01E0{}.mkv", i),
            format!("overwritten backing ep{}", i).as_bytes(),
        );
    }

    // Read E02–E05 through FUSE — must come from cache, not the overwritten backing
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

/// Accessing a second episode triggers prediction of the episodes following it,
/// without re-caching episodes that are already cached.
#[tokio::test]
async fn pipeline_advances_lookahead_on_each_access() {
    let h = FuseHarness::new_full_pipeline(2).unwrap(); // lookahead = 2

    for i in 1..=5u32 {
        write_backing_file(
            &h,
            &format!("tv/Show/Show.S01E0{}.mkv", i),
            format!("ep{}", i).as_bytes(),
        );
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Access E01 → should cache E02 and E03
    let _ = tokio::fs::read(h.mount_path().join("tv/Show/Show.S01E01.mkv"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert!(
        h.cache_path().join("tv/Show/Show.S01E02.mkv").exists(),
        "E02 should be cached after accessing E01"
    );
    assert!(
        h.cache_path().join("tv/Show/Show.S01E03.mkv").exists(),
        "E03 should be cached after accessing E01"
    );
    assert!(
        !h.cache_path().join("tv/Show/Show.S01E04.mkv").exists(),
        "E04 should NOT be cached yet (outside lookahead=2)"
    );

    // Access E02 through FUSE (serves from cache) → should cache E04 (E03 already cached)
    let e2_data = tokio::fs::read(h.mount_path().join("tv/Show/Show.S01E02.mkv"))
        .await
        .unwrap();
    assert_eq!(e2_data, b"ep2");

    tokio::time::sleep(Duration::from_millis(600)).await;

    assert!(
        h.cache_path().join("tv/Show/Show.S01E04.mkv").exists(),
        "E04 should be cached after accessing E02"
    );
}
