/// End-to-end tests for the `prefetch` preset.
///
/// Each test exercises the full pipeline:
///   FUSE open() → AccessEvent → ActionEngine → Prefetch::on_miss()
///   → CopyRequest → copier → mark_cached() → cache dir populated.
///
/// Tests are split by mode so failures are immediately attributable:
///   - cache-hit-only   — only the accessed file lands in the cache
///   - cache-neighbors  — all siblings in the same directory are cached
///   - cache-parent-recursively — parent directory is recursively cached
use std::sync::Arc;
use std::time::Duration;

use fscache::presets::prefetch::{Prefetch, PrefetchMode};

use crate::common::{write_backing_file, FuseHarness};

fn make_preset(
    mode: PrefetchMode,
    max_depth: usize,
    whitelist: &[&str],
    blacklist: &[&str],
) -> Arc<Prefetch> {
    Arc::new(
        Prefetch::new(
            mode,
            max_depth,
            vec![],
            &whitelist.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &blacklist.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .expect("patterns should compile"),
    )
}

// ---------------------------------------------------------------------------
// cache-hit-only
// ---------------------------------------------------------------------------

/// Accessing one file through FUSE caches only that file.
/// Sibling files in the same directory must not be pulled.
#[tokio::test]
async fn cache_hit_only_caches_only_accessed_file() {
    let preset = make_preset(PrefetchMode::CacheHitOnly, 3, &[], &[]);
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "movies/Movie.mkv", b"the movie");
    write_backing_file(&h, "movies/Other.mkv", b"other movie");
    write_backing_file(&h, "movies/Subtitle.srt", b"subtitles");
    std::thread::sleep(Duration::from_millis(100));

    let _ = std::fs::read(h.mount_path().join("movies/Movie.mkv")).unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(
        h.cache_path().join("movies/Movie.mkv").exists(),
        "accessed file must be cached"
    );
    assert!(
        !h.cache_path().join("movies/Other.mkv").exists(),
        "cache-hit-only must not cache sibling files"
    );
    assert!(
        !h.cache_path().join("movies/Subtitle.srt").exists(),
        "cache-hit-only must not cache sibling files"
    );
}

/// Overwriting the backing file after caching — subsequent reads return the cached version.
#[tokio::test]
async fn cache_hit_only_serves_cached_content_after_backing_change() {
    let preset = make_preset(PrefetchMode::CacheHitOnly, 3, &[], &[]);
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "ep01.mkv", b"original");
    std::thread::sleep(Duration::from_millis(100));

    let _ = std::fs::read(h.mount_path().join("ep01.mkv")).unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    write_backing_file(&h, "ep01.mkv", b"overwritten");
    let data = std::fs::read(h.mount_path().join("ep01.mkv")).unwrap();
    assert_eq!(data, b"original", "second read must come from the SSD cache");
}

#[tokio::test]
async fn cache_hit_only_respects_blocklist() {
    let preset = Arc::new(
        Prefetch::new(
            PrefetchMode::CacheHitOnly,
            3,
            vec!["cat".to_string()],
            &[],
            &[],
        )
        .unwrap(),
    );
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "ep01.mkv", b"content");
    std::thread::sleep(Duration::from_millis(100));

    let path = h.mount_path().join("ep01.mkv");
    let mut child = std::process::Command::new("cat")
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let _ = child.wait();

    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(
        !h.cache_path().join("ep01.mkv").exists(),
        "blocklisted process (cat) must not trigger caching"
    );
}

// ---------------------------------------------------------------------------
// cache-neighbors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cache_neighbors_caches_all_siblings() {
    let preset = make_preset(PrefetchMode::CacheNeighbors, 3, &[], &[]);
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "show/season1/ep01.mkv", b"ep01 data");
    write_backing_file(&h, "show/season1/ep02.mkv", b"ep02 data");
    write_backing_file(&h, "show/season1/ep03.mkv", b"ep03 data");
    std::thread::sleep(Duration::from_millis(100));

    let _ = std::fs::read(h.mount_path().join("show/season1/ep01.mkv")).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;

    assert!(
        h.cache_path().join("show/season1/ep01.mkv").exists(),
        "accessed file must be cached"
    );
    assert!(
        h.cache_path().join("show/season1/ep02.mkv").exists(),
        "sibling ep02 must be cached by cache-neighbors"
    );
    assert!(
        h.cache_path().join("show/season1/ep03.mkv").exists(),
        "sibling ep03 must be cached by cache-neighbors"
    );
}

/// Files in a *different* directory must not be pulled by cache-neighbors.
#[tokio::test]
async fn cache_neighbors_does_not_cross_directory_boundary() {
    let preset = make_preset(PrefetchMode::CacheNeighbors, 3, &[], &[]);
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "show/season1/ep01.mkv", b"s1e01");
    write_backing_file(&h, "show/season2/ep01.mkv", b"s2e01");
    std::thread::sleep(Duration::from_millis(100));

    let _ = std::fs::read(h.mount_path().join("show/season1/ep01.mkv")).unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(h.cache_path().join("show/season1/ep01.mkv").exists());
    assert!(
        !h.cache_path().join("show/season2/ep01.mkv").exists(),
        "files in a different directory must not be cached by cache-neighbors"
    );
}

#[tokio::test]
async fn cache_neighbors_whitelist_filters_siblings() {
    let preset = make_preset(PrefetchMode::CacheNeighbors, 3, &[r"\.mkv$"], &[]);
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "ep01.mkv", b"video");
    write_backing_file(&h, "ep02.mkv", b"video");
    write_backing_file(&h, "meta.nfo", b"metadata");
    write_backing_file(&h, "thumb.jpg", b"thumbnail");
    std::thread::sleep(Duration::from_millis(100));

    let _ = std::fs::read(h.mount_path().join("ep01.mkv")).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;

    assert!(h.cache_path().join("ep01.mkv").exists(), "accessed .mkv must be cached");
    assert!(h.cache_path().join("ep02.mkv").exists(), "sibling .mkv must be cached");
    assert!(
        !h.cache_path().join("meta.nfo").exists(),
        ".nfo does not match whitelist — must not be cached"
    );
    assert!(
        !h.cache_path().join("thumb.jpg").exists(),
        ".jpg does not match whitelist — must not be cached"
    );
}

/// Blacklist regex excludes matching siblings even when whitelist is empty.
#[tokio::test]
async fn cache_neighbors_blacklist_excludes_matching_files() {
    let preset = make_preset(PrefetchMode::CacheNeighbors, 3, &[], &[r"\.(nfo|jpg)$"]);
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "ep01.mkv", b"video");
    write_backing_file(&h, "ep02.mkv", b"video");
    write_backing_file(&h, "meta.nfo", b"metadata");
    write_backing_file(&h, "thumb.jpg", b"thumbnail");
    std::thread::sleep(Duration::from_millis(100));

    let _ = std::fs::read(h.mount_path().join("ep01.mkv")).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;

    assert!(h.cache_path().join("ep01.mkv").exists());
    assert!(h.cache_path().join("ep02.mkv").exists());
    assert!(!h.cache_path().join("meta.nfo").exists(), ".nfo is blacklisted");
    assert!(!h.cache_path().join("thumb.jpg").exists(), ".jpg is blacklisted");
}

/// Subdirectories inside the accessed file's directory are skipped — only
/// flat siblings (regular files) are cached.
#[tokio::test]
async fn cache_neighbors_skips_subdirectory_contents() {
    let preset = make_preset(PrefetchMode::CacheNeighbors, 3, &[], &[]);
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "ep01.mkv", b"video");
    write_backing_file(&h, "extras/bonus.mkv", b"bonus");
    std::thread::sleep(Duration::from_millis(100));

    let _ = std::fs::read(h.mount_path().join("ep01.mkv")).unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(h.cache_path().join("ep01.mkv").exists());
    assert!(
        !h.cache_path().join("extras/bonus.mkv").exists(),
        "files inside a subdirectory must not be cached by cache-neighbors"
    );
}

// ---------------------------------------------------------------------------
// cache-parent-recursively
// ---------------------------------------------------------------------------

/// Accessing a file caches every file in the parent's tree up to max_depth.
///
/// Tree layout (walk starts from `show/` — the accessed file's parent):
///   show/
///     ep01.mkv       ← accessed
///     banner.jpg     ← sibling file
///     season1/
///       ep02.mkv     ← one level deep
///       season1.nfo  ← one level deep
#[tokio::test]
async fn cache_parent_recursively_caches_whole_tree() {
    let preset = make_preset(PrefetchMode::CacheParentRecursively, 4, &[], &[]);
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "show/ep01.mkv", b"ep01");
    write_backing_file(&h, "show/banner.jpg", b"banner");
    write_backing_file(&h, "show/season1/ep02.mkv", b"ep02");
    write_backing_file(&h, "show/season1/season1.nfo", b"nfo");
    std::thread::sleep(Duration::from_millis(100));

    let _ = std::fs::read(h.mount_path().join("show/ep01.mkv")).unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(h.cache_path().join("show/ep01.mkv").exists(), "accessed file");
    assert!(h.cache_path().join("show/banner.jpg").exists(), "flat sibling");
    assert!(h.cache_path().join("show/season1/ep02.mkv").exists(), "file in subdir");
    assert!(h.cache_path().join("show/season1/season1.nfo").exists(), "nfo in subdir");
}

/// max_depth=1: only files at the immediate parent level are cached; deeper
/// subdirectories are not entered.
#[tokio::test]
async fn cache_parent_recursively_depth_limit_respected() {
    let preset = make_preset(PrefetchMode::CacheParentRecursively, 1, &[], &[]);
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    // parent/
    //   ep01.mkv         ← in parent, depth=1 from parent
    //   sub/
    //     ep02.mkv       ← in sub, requires depth=2
    write_backing_file(&h, "parent/ep01.mkv", b"ep01");
    write_backing_file(&h, "parent/sub/ep02.mkv", b"ep02");
    std::thread::sleep(Duration::from_millis(100));

    let _ = std::fs::read(h.mount_path().join("parent/ep01.mkv")).unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert!(h.cache_path().join("parent/ep01.mkv").exists(), "file at depth=1 must be cached");
    assert!(
        !h.cache_path().join("parent/sub/ep02.mkv").exists(),
        "file at depth=2 must not be cached when max_depth=1"
    );
}

/// Walk starts from `show/` (parent of accessed file):
///   show/
///     ep01.mkv   ← accessed, matches whitelist
///     ep02.mkv   ← matches whitelist
///     meta.nfo   ← does NOT match
///     season1/
///       ep03.mkv ← matches whitelist
///       info.jpg ← does NOT match
#[tokio::test]
async fn cache_parent_recursively_whitelist_filters_tree() {
    let preset = make_preset(PrefetchMode::CacheParentRecursively, 4, &[r"\.mkv$"], &[]);
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "show/ep01.mkv", b"ep01");
    write_backing_file(&h, "show/ep02.mkv", b"ep02");
    write_backing_file(&h, "show/meta.nfo", b"nfo");
    write_backing_file(&h, "show/season1/ep03.mkv", b"ep03");
    write_backing_file(&h, "show/season1/info.jpg", b"thumb");
    std::thread::sleep(Duration::from_millis(100));

    let _ = std::fs::read(h.mount_path().join("show/ep01.mkv")).unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(h.cache_path().join("show/ep01.mkv").exists());
    assert!(h.cache_path().join("show/ep02.mkv").exists());
    assert!(h.cache_path().join("show/season1/ep03.mkv").exists());
    assert!(!h.cache_path().join("show/meta.nfo").exists(), ".nfo does not match .mkv whitelist");
    assert!(!h.cache_path().join("show/season1/info.jpg").exists(), ".jpg does not match .mkv whitelist");
}

/// Walk starts from `show/`:
///   show/
///     ep01.mkv   ← accessed
///     ep02.mkv
///     meta.nfo   ← blacklisted
///     season1/
///       ep03.mkv
///       banner.jpg ← blacklisted
#[tokio::test]
async fn cache_parent_recursively_blacklist_excludes_files() {
    let preset = make_preset(PrefetchMode::CacheParentRecursively, 4, &[], &[r"\.(nfo|jpg)$"]);
    let h = FuseHarness::new_full_pipeline_with_preset(preset).unwrap();

    write_backing_file(&h, "show/ep01.mkv", b"ep01");
    write_backing_file(&h, "show/ep02.mkv", b"ep02");
    write_backing_file(&h, "show/meta.nfo", b"nfo");
    write_backing_file(&h, "show/season1/ep03.mkv", b"ep03");
    write_backing_file(&h, "show/season1/banner.jpg", b"banner");
    std::thread::sleep(Duration::from_millis(100));

    let _ = std::fs::read(h.mount_path().join("show/ep01.mkv")).unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(h.cache_path().join("show/ep01.mkv").exists());
    assert!(h.cache_path().join("show/ep02.mkv").exists());
    assert!(h.cache_path().join("show/season1/ep03.mkv").exists());
    assert!(!h.cache_path().join("show/meta.nfo").exists(), ".nfo is blacklisted");
    assert!(!h.cache_path().join("show/season1/banner.jpg").exists(), ".jpg is blacklisted");
}
