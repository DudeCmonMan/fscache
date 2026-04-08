/// End-to-end tests for the `prefetch` preset.
///
/// Tests are behavioural: construct a real BackingStore from a temp directory,
/// exercise each mode (CacheHitOnly, CacheNeighbors, CacheParentRecursively),
/// and verify which paths are returned by on_miss / on_hit.
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use fscache::backing_store::BackingStore;
use fscache::cache::db::CacheDb;
use fscache::preset::{CacheAction, CachePreset, ProcessInfo, RuleContext};
use fscache::presets::prefetch::{parse_mode, Prefetch, PrefetchMode};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open_backing_store(dir: &Path) -> BackingStore {
    let c = CString::new(dir.as_os_str().as_bytes()).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    assert!(fd >= 0, "failed to open backing dir {}", dir.display());
    BackingStore::new(fd)
}

fn in_memory_db() -> CacheDb {
    CacheDb::open(Path::new(":memory:")).expect("in-memory DB")
}

fn make_prefetch(
    mode: PrefetchMode,
    max_depth: usize,
    whitelist: &[&str],
    blacklist: &[&str],
) -> Prefetch {
    Prefetch::new(
        mode,
        max_depth,
        vec![],
        &whitelist.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        &blacklist.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    )
    .expect("patterns should compile")
}

fn cache_files(actions: Vec<CacheAction>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for a in actions {
        if let CacheAction::Cache(files) = a {
            out.extend(files);
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// parse_mode
// ---------------------------------------------------------------------------

#[test]
fn test_parse_mode_valid() {
    assert_eq!(parse_mode("cache-hit-only").unwrap(), PrefetchMode::CacheHitOnly);
    assert_eq!(parse_mode("cache-neighbors").unwrap(), PrefetchMode::CacheNeighbors);
    assert_eq!(parse_mode("cache-parent-recursively").unwrap(), PrefetchMode::CacheParentRecursively);
}

#[test]
fn test_parse_mode_invalid() {
    assert!(parse_mode("unknown-mode").is_err());
    assert!(parse_mode("").is_err());
}

// ---------------------------------------------------------------------------
// Startup validation: invalid regex must fail at construction
// ---------------------------------------------------------------------------

#[test]
fn test_invalid_whitelist_regex_fails_construction() {
    let result = Prefetch::new(
        PrefetchMode::CacheHitOnly,
        3,
        vec![],
        &["[unclosed bracket".to_string()],
        &[],
    );
    assert!(result.is_err(), "invalid whitelist regex should fail");
    let msg = result.err().unwrap().to_string();
    assert!(msg.contains("[unclosed bracket"), "error should name the bad pattern");
}

#[test]
fn test_invalid_blacklist_regex_fails_construction() {
    let result = Prefetch::new(
        PrefetchMode::CacheHitOnly,
        3,
        vec![],
        &[],
        &["(?invalid".to_string()],
    );
    assert!(result.is_err(), "invalid blacklist regex should fail");
}

#[test]
fn test_valid_patterns_compile() {
    let result = Prefetch::new(
        PrefetchMode::CacheNeighbors,
        3,
        vec![],
        &[r"\.mkv$".to_string(), r"\.mp4$".to_string()],
        &[r"\.nfo$".to_string(), r"\.jpg$".to_string()],
    );
    assert!(result.is_ok(), "valid patterns should compile without error");
}

// ---------------------------------------------------------------------------
// CacheHitOnly: only the accessed file is returned
// ---------------------------------------------------------------------------

#[test]
fn test_cache_hit_only_returns_single_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("ep01.mkv"), b"data").unwrap();
    std::fs::write(dir.join("ep02.mkv"), b"data").unwrap();
    std::fs::write(dir.join("ep03.mkv"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    let preset = make_prefetch(PrefetchMode::CacheHitOnly, 3, &[], &[]);
    let files = cache_files(preset.on_miss(Path::new("ep01.mkv"), &ctx));
    assert_eq!(files, vec![PathBuf::from("ep01.mkv")]);
}

#[test]
fn test_cache_hit_only_ignores_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("ep01.nfo"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    // Blacklist .nfo — but CacheHitOnly should still return the directly accessed file.
    let preset = make_prefetch(PrefetchMode::CacheHitOnly, 3, &[], &[r"\.nfo$"]);
    let files = cache_files(preset.on_miss(Path::new("ep01.nfo"), &ctx));
    assert_eq!(files, vec![PathBuf::from("ep01.nfo")],
        "CacheHitOnly always returns the accessed path regardless of filters");
}

// ---------------------------------------------------------------------------
// CacheNeighbors: siblings in the same directory
// ---------------------------------------------------------------------------

#[test]
fn test_cache_neighbors_all_siblings() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("ep01.mkv"), b"data").unwrap();
    std::fs::write(dir.join("ep02.mkv"), b"data").unwrap();
    std::fs::write(dir.join("ep03.mkv"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    let preset = make_prefetch(PrefetchMode::CacheNeighbors, 3, &[], &[]);
    let files = cache_files(preset.on_miss(Path::new("ep01.mkv"), &ctx));
    assert_eq!(files, vec![
        PathBuf::from("ep01.mkv"),
        PathBuf::from("ep02.mkv"),
        PathBuf::from("ep03.mkv"),
    ]);
}

#[test]
fn test_cache_neighbors_whitelist() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("ep01.mkv"), b"data").unwrap();
    std::fs::write(dir.join("ep02.mkv"), b"data").unwrap();
    std::fs::write(dir.join("thumb.jpg"), b"data").unwrap();
    std::fs::write(dir.join("meta.nfo"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    let preset = make_prefetch(PrefetchMode::CacheNeighbors, 3, &[r"\.mkv$"], &[]);
    let files = cache_files(preset.on_miss(Path::new("ep01.mkv"), &ctx));
    assert_eq!(files, vec![
        PathBuf::from("ep01.mkv"),
        PathBuf::from("ep02.mkv"),
    ]);
}

#[test]
fn test_cache_neighbors_blacklist() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("ep01.mkv"), b"data").unwrap();
    std::fs::write(dir.join("ep02.mkv"), b"data").unwrap();
    std::fs::write(dir.join("thumb.jpg"), b"data").unwrap();
    std::fs::write(dir.join("meta.nfo"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    let preset = make_prefetch(PrefetchMode::CacheNeighbors, 3, &[], &[r"\.(jpg|nfo)$"]);
    let files = cache_files(preset.on_miss(Path::new("ep01.mkv"), &ctx));
    assert_eq!(files, vec![
        PathBuf::from("ep01.mkv"),
        PathBuf::from("ep02.mkv"),
    ]);
}

#[test]
fn test_cache_neighbors_blacklist_takes_precedence_over_whitelist() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("ep01.mkv"), b"data").unwrap();
    std::fs::write(dir.join("ep02.mkv"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    // Both whitelist and blacklist match .mkv — blacklist wins, nothing passes.
    let preset = make_prefetch(PrefetchMode::CacheNeighbors, 3, &[r"\.mkv$"], &[r"\.mkv$"]);
    let files = cache_files(preset.on_miss(Path::new("ep01.mkv"), &ctx));
    assert!(files.is_empty(), "blacklist should take precedence over whitelist");
}

#[test]
fn test_cache_neighbors_skips_subdirectories() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("ep01.mkv"), b"data").unwrap();
    std::fs::create_dir(dir.join("extras")).unwrap();
    std::fs::write(dir.join("extras").join("bonus.mkv"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    let preset = make_prefetch(PrefetchMode::CacheNeighbors, 3, &[], &[]);
    let files = cache_files(preset.on_miss(Path::new("ep01.mkv"), &ctx));
    assert!(!files.iter().any(|f| f.starts_with("extras")),
        "CacheNeighbors should not descend into subdirectories");
    assert!(files.contains(&PathBuf::from("ep01.mkv")));
}

#[test]
fn test_cache_neighbors_nested_path() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::create_dir_all(dir.join("show/season1")).unwrap();
    std::fs::write(dir.join("show/season1/ep01.mkv"), b"data").unwrap();
    std::fs::write(dir.join("show/season1/ep02.mkv"), b"data").unwrap();
    std::fs::write(dir.join("show/season1/ep03.mkv"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    let preset = make_prefetch(PrefetchMode::CacheNeighbors, 3, &[], &[]);
    let files = cache_files(preset.on_miss(Path::new("show/season1/ep01.mkv"), &ctx));
    assert_eq!(files, vec![
        PathBuf::from("show/season1/ep01.mkv"),
        PathBuf::from("show/season1/ep02.mkv"),
        PathBuf::from("show/season1/ep03.mkv"),
    ]);
}

// ---------------------------------------------------------------------------
// CacheParentRecursively: recursive walk bounded by max_depth
//
// Depth semantics: depth=1 enters subdirectories once; depth=0 is empty.
// Example tree:
//   root/
//     a.mkv            (depth=1 from root)
//     sub/
//       b.mkv          (depth=2 from root; requires recursing into sub/)
//       deep/
//         c.mkv        (depth=3 from root; requires recursing into sub/deep/)
// ---------------------------------------------------------------------------

#[test]
fn test_cache_parent_recursively_depth_1() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("a.mkv"), b"data").unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub").join("b.mkv"), b"data").unwrap();
    std::fs::create_dir(dir.join("sub").join("deep")).unwrap();
    std::fs::write(dir.join("sub").join("deep").join("c.mkv"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    let preset = make_prefetch(PrefetchMode::CacheParentRecursively, 1, &[], &[]);
    let files = cache_files(preset.on_miss(Path::new("a.mkv"), &ctx));

    assert!(files.contains(&PathBuf::from("a.mkv")), "a.mkv should be included");
    assert!(!files.contains(&PathBuf::from("sub/b.mkv")),
        "sub/b.mkv requires depth 2, should not be included at depth=1");
    assert!(!files.contains(&PathBuf::from("sub/deep/c.mkv")),
        "sub/deep/c.mkv is too deep");
}

#[test]
fn test_cache_parent_recursively_depth_2() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("a.mkv"), b"data").unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub").join("b.mkv"), b"data").unwrap();
    std::fs::create_dir(dir.join("sub").join("deep")).unwrap();
    std::fs::write(dir.join("sub").join("deep").join("c.mkv"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    let preset = make_prefetch(PrefetchMode::CacheParentRecursively, 2, &[], &[]);
    let files = cache_files(preset.on_miss(Path::new("a.mkv"), &ctx));

    assert!(files.contains(&PathBuf::from("a.mkv")), "a.mkv should be included");
    assert!(files.contains(&PathBuf::from("sub/b.mkv")), "sub/b.mkv should be included at depth=2");
    assert!(!files.contains(&PathBuf::from("sub/deep/c.mkv")),
        "sub/deep/c.mkv requires depth=3, not included at depth=2");
}

#[test]
fn test_cache_parent_recursively_depth_3() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("a.mkv"), b"data").unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub").join("b.mkv"), b"data").unwrap();
    std::fs::create_dir(dir.join("sub").join("deep")).unwrap();
    std::fs::write(dir.join("sub").join("deep").join("c.mkv"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    let preset = make_prefetch(PrefetchMode::CacheParentRecursively, 3, &[], &[]);
    let files = cache_files(preset.on_miss(Path::new("a.mkv"), &ctx));

    assert!(files.contains(&PathBuf::from("a.mkv")));
    assert!(files.contains(&PathBuf::from("sub/b.mkv")));
    assert!(files.contains(&PathBuf::from("sub/deep/c.mkv")),
        "c.mkv should be reachable at depth=3");
}

#[test]
fn test_cache_parent_recursively_with_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::create_dir_all(dir.join("show/season1")).unwrap();
    std::fs::write(dir.join("show/season1/ep01.mkv"), b"data").unwrap();
    std::fs::write(dir.join("show/season1/ep02.mkv"), b"data").unwrap();
    std::fs::write(dir.join("show/season1/thumb.jpg"), b"data").unwrap();
    std::fs::write(dir.join("show/season1/ep01.nfo"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    let preset = make_prefetch(PrefetchMode::CacheParentRecursively, 3, &[r"\.mkv$"], &[]);
    let files = cache_files(preset.on_miss(Path::new("show/season1/ep01.mkv"), &ctx));
    assert_eq!(files, vec![
        PathBuf::from("show/season1/ep01.mkv"),
        PathBuf::from("show/season1/ep02.mkv"),
    ]);
}

// ---------------------------------------------------------------------------
// on_hit delegates to the same logic as on_miss
// ---------------------------------------------------------------------------

#[test]
fn test_on_hit_same_result_as_on_miss() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("ep01.mkv"), b"data").unwrap();
    std::fs::write(dir.join("ep02.mkv"), b"data").unwrap();

    let bs = open_backing_store(dir);
    let db = in_memory_db();
    let ctx = RuleContext { backing_store: &bs, cache_db: &db };

    let preset = make_prefetch(PrefetchMode::CacheNeighbors, 3, &[], &[]);
    let miss_files = cache_files(preset.on_miss(Path::new("ep01.mkv"), &ctx));
    let hit_files  = cache_files(preset.on_hit(Path::new("ep01.mkv"), &ctx));
    assert_eq!(miss_files, hit_files, "on_hit and on_miss should return the same files");
}

// ---------------------------------------------------------------------------
// Process blocklist
// ---------------------------------------------------------------------------

#[test]
fn test_blocklist_filters_process() {
    let preset = Prefetch::new(
        PrefetchMode::CacheHitOnly,
        3,
        vec!["Plex Media Scanner".to_string()],
        &[],
        &[],
    )
    .unwrap();

    let blocked = ProcessInfo {
        pid: 1,
        name: Some("Plex Media Scanner".to_string()),
        cmdline: None,
        ancestors: vec![],
    };
    let allowed = ProcessInfo {
        pid: 2,
        name: Some("Plex Media Server".to_string()),
        cmdline: None,
        ancestors: vec![],
    };
    let child_of_blocked = ProcessInfo {
        pid: 3,
        name: Some("ffmpeg".to_string()),
        cmdline: None,
        ancestors: vec!["Plex Media Scanner".to_string()],
    };

    assert!(preset.should_filter(&blocked), "blocked process should be filtered");
    assert!(!preset.should_filter(&allowed), "unrelated process should not be filtered");
    assert!(preset.should_filter(&child_of_blocked), "child of blocked process should be filtered");
}

#[test]
fn test_empty_blocklist_never_filters() {
    let preset = make_prefetch(PrefetchMode::CacheHitOnly, 3, &[], &[]);
    let any_process = ProcessInfo {
        pid: 1,
        name: Some("anything".to_string()),
        cmdline: None,
        ancestors: vec!["root".to_string()],
    };
    assert!(!preset.should_filter(&any_process));
}
