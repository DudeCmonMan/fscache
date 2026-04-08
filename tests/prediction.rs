mod common;

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::mpsc;

use f_cache::action_engine::{
    buffer_event, run_copier_task, show_root, AccessEvent, ActionEngine, CopyRequest,
};
use f_cache::backing_store::BackingStore;
use f_cache::cache::CacheManager;
use f_cache::db::CacheDb;
use f_cache::preset::CachePreset;
use f_cache::prediction_utils::{parse_season_dir, parse_season_episode};
use f_cache::presets::plex_episode_prediction::PlexEpisodePrediction;
use f_cache::scheduler::Scheduler;

// ---- Scheduler tests ----

#[test]
fn scheduler_allows_inside_normal_window() {
    let s = Scheduler::new("08:00", "22:00").unwrap();
    assert!(s.is_allowed_at(8, 0));
    assert!(s.is_allowed_at(12, 0));
    assert!(s.is_allowed_at(21, 59));
}

#[test]
fn scheduler_blocks_outside_normal_window() {
    let s = Scheduler::new("08:00", "22:00").unwrap();
    assert!(!s.is_allowed_at(7, 59));
    assert!(!s.is_allowed_at(22, 0));
    assert!(!s.is_allowed_at(0, 0));
}

#[test]
fn scheduler_midnight_wrap_allows() {
    let s = Scheduler::new("22:00", "02:00").unwrap();
    assert!(s.is_allowed_at(22, 0));
    assert!(s.is_allowed_at(23, 59));
    assert!(s.is_allowed_at(0, 0));
    assert!(s.is_allowed_at(1, 59));
}

#[test]
fn scheduler_midnight_wrap_blocks() {
    let s = Scheduler::new("22:00", "02:00").unwrap();
    assert!(!s.is_allowed_at(2, 0));
    assert!(!s.is_allowed_at(12, 0));
    assert!(!s.is_allowed_at(21, 59));
}

// ---- Regex fallback tests ----

#[test]
fn regex_parses_standard_formats() {
    assert_eq!(parse_season_episode("Show.S01E03.mkv"), Some((1, 3)));
    assert_eq!(parse_season_episode("show.s02e11.mkv"), Some((2, 11)));
    assert_eq!(parse_season_episode("S01E01"), Some((1, 1)));
    assert_eq!(parse_season_episode("S12E100.mkv"), Some((12, 100)));
}

#[test]
fn regex_returns_none_for_non_episode() {
    assert_eq!(parse_season_episode("movie.2024.mkv"), None);
    assert_eq!(parse_season_episode("no_episode_here.mp4"), None);
}

// ---- Integration: action engine + copier ----

fn make_backing_store(path: &std::path::Path) -> std::sync::Arc<BackingStore> {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY) };
    assert!(fd >= 0, "failed to open backing dir");
    std::sync::Arc::new(BackingStore::new(fd))
}

fn make_engine(
    access_rx: mpsc::UnboundedReceiver<AccessEvent>,
    copy_tx: mpsc::Sender<CopyRequest>,
    cache: Arc<CacheManager>,
    lookahead: usize,
    backing_store: Arc<BackingStore>,
    max_cache_pull_bytes: u64,
) -> ActionEngine {
    let preset = Arc::new(PlexEpisodePrediction::new(lookahead, vec![], false));
    let scheduler = Scheduler::new("00:00", "23:59").unwrap();
    ActionEngine::new(
        access_rx,
        copy_tx,
        cache,
        Some(preset as Arc<dyn CachePreset>),
        scheduler,
        backing_store,
        max_cache_pull_bytes,
        0,
        0,
        0,
    )
}

/// Full end-to-end: access S01E01 → S01E02..E05 appear in cache via regex fallback.
#[tokio::test]
async fn predictor_caches_next_episodes_via_regex() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let ep_dir = backing.path().join("tv/Show");
    std::fs::create_dir_all(&ep_dir).unwrap();
    for i in 1..=5u32 {
        std::fs::write(
            ep_dir.join(format!("Show.S01E0{}.mkv", i)),
            format!("episode {} content", i),
        )
        .unwrap();
    }

    let backing_store = make_backing_store(backing.path());

    let cache = Arc::new(CacheManager::new(
        cache_dir.path().to_path_buf(),
        cache_dir.path().to_path_buf(),
        1.0,
        72,
        0.0,
    ));

    let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
    let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(32);

    let engine = make_engine(access_rx, copy_tx, Arc::clone(&cache), 4, Arc::clone(&backing_store), 0);
    tokio::spawn(engine.run());
    tokio::spawn(run_copier_task(Arc::clone(&backing_store), copy_rx, Arc::clone(&cache)));

    access_tx
        .send(AccessEvent::miss(PathBuf::from("tv/Show/Show.S01E01.mkv")))
        .unwrap();

    tokio::time::sleep(Duration::from_millis(800)).await;

    for i in 2..=5u32 {
        let cached = cache_dir.path().join(format!("tv/Show/Show.S01E0{}.mkv", i));
        assert!(cached.exists(), "expected ep {} in cache", i);
        assert_eq!(
            std::fs::read_to_string(&cached).unwrap(),
            format!("episode {} content", i)
        );
    }
    assert!(
        cache_dir.path().join("tv/Show/Show.S01E01.mkv").exists(),
        "S01E01 (current episode) should also be cached"
    );
}

#[tokio::test]
async fn predictor_skips_already_cached() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let ep_dir = backing.path().join("tv/Show");
    std::fs::create_dir_all(&ep_dir).unwrap();
    for i in 1..=3u32 {
        std::fs::write(ep_dir.join(format!("Show.S01E0{}.mkv", i)), b"data").unwrap();
    }

    let cache_ep_dir = cache_dir.path().join("tv/Show");
    std::fs::create_dir_all(&cache_ep_dir).unwrap();
    std::fs::write(cache_ep_dir.join("Show.S01E02.mkv"), b"already cached").unwrap();

    let backing_store = make_backing_store(backing.path());
    let cache = Arc::new(CacheManager::new(
        cache_dir.path().to_path_buf(),
        cache_dir.path().to_path_buf(),
        1.0,
        72,
        0.0,
    ));
    let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
    let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(32);
    let engine = make_engine(access_rx, copy_tx, Arc::clone(&cache), 2, Arc::clone(&backing_store), 0);
    tokio::spawn(engine.run());
    tokio::spawn(run_copier_task(Arc::clone(&backing_store), copy_rx, Arc::clone(&cache)));

    access_tx
        .send(AccessEvent::miss(PathBuf::from("tv/Show/Show.S01E01.mkv")))
        .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        std::fs::read(cache_ep_dir.join("Show.S01E02.mkv")).unwrap(),
        b"already cached"
    );
    assert!(
        cache_ep_dir.join("Show.S01E03.mkv").exists(),
        "E03 should be cached"
    );
}

// ---- parse_season_dir tests ----

#[test]
fn season_dir_parses_standard_formats() {
    assert_eq!(parse_season_dir("Season 1"), Some(1));
    assert_eq!(parse_season_dir("Season 01"), Some(1));
    assert_eq!(parse_season_dir("season 3"), Some(3));
    assert_eq!(parse_season_dir("SEASON 12"), Some(12));
}

#[test]
fn season_dir_returns_none_for_non_season() {
    assert_eq!(parse_season_dir("Breaking Bad"), None);
    assert_eq!(parse_season_dir("S01E01.mkv"), None);
    assert_eq!(parse_season_dir("Specials"), None);
    assert_eq!(parse_season_dir(""), None);
}

// ---- Cross-season regex fallback tests ----

#[tokio::test]
async fn regex_crosses_season_boundary_structured_layout() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let s1 = backing.path().join("Show/Season 1");
    let s2 = backing.path().join("Show/Season 2");
    std::fs::create_dir_all(&s1).unwrap();
    std::fs::create_dir_all(&s2).unwrap();
    for i in 1..=3u32 {
        std::fs::write(s1.join(format!("Show.S01E0{}.mkv", i)), format!("s1e{}", i)).unwrap();
    }
    for i in 1..=3u32 {
        std::fs::write(s2.join(format!("Show.S02E0{}.mkv", i)), format!("s2e{}", i)).unwrap();
    }

    let backing_store = make_backing_store(backing.path());
    let cache = Arc::new(CacheManager::new(cache_dir.path().to_path_buf(), cache_dir.path().to_path_buf(), 1.0, 72, 0.0));
    let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
    let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(32);
    let engine = make_engine(access_rx, copy_tx, Arc::clone(&cache), 4, Arc::clone(&backing_store), 0);
    tokio::spawn(engine.run());
    tokio::spawn(run_copier_task(Arc::clone(&backing_store), copy_rx, Arc::clone(&cache)));

    access_tx.send(AccessEvent::miss(PathBuf::from("Show/Season 1/Show.S01E03.mkv"))).unwrap();

    tokio::time::sleep(Duration::from_millis(800)).await;

    for i in 1..=3u32 {
        let cached = cache_dir.path().join(format!("Show/Season 2/Show.S02E0{}.mkv", i));
        assert!(cached.exists(), "expected S02E0{} in cache", i);
        assert_eq!(std::fs::read_to_string(&cached).unwrap(), format!("s2e{}", i));
    }
    assert!(cache_dir.path().join("Show/Season 1/Show.S01E03.mkv").exists(),
        "S01E03 (current episode) should also be cached");
}

#[tokio::test]
async fn regex_crosses_season_boundary_flat_layout() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let show_dir = backing.path().join("Show");
    std::fs::create_dir_all(&show_dir).unwrap();
    for i in 1..=3u32 {
        std::fs::write(show_dir.join(format!("Show.S01E0{}.mkv", i)), format!("s1e{}", i)).unwrap();
    }
    for i in 1..=3u32 {
        std::fs::write(show_dir.join(format!("Show.S02E0{}.mkv", i)), format!("s2e{}", i)).unwrap();
    }

    let backing_store = make_backing_store(backing.path());
    let cache = Arc::new(CacheManager::new(cache_dir.path().to_path_buf(), cache_dir.path().to_path_buf(), 1.0, 72, 0.0));
    let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
    let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(32);
    let engine = make_engine(access_rx, copy_tx, Arc::clone(&cache), 4, Arc::clone(&backing_store), 0);
    tokio::spawn(engine.run());
    tokio::spawn(run_copier_task(Arc::clone(&backing_store), copy_rx, Arc::clone(&cache)));

    access_tx.send(AccessEvent::miss(PathBuf::from("Show/Show.S01E03.mkv"))).unwrap();

    tokio::time::sleep(Duration::from_millis(800)).await;

    for i in 1..=3u32 {
        let cached = cache_dir.path().join(format!("Show/Show.S02E0{}.mkv", i));
        assert!(cached.exists(), "expected S02E0{} in cache (flat layout)", i);
        assert_eq!(std::fs::read_to_string(&cached).unwrap(), format!("s2e{}", i));
    }
    assert!(cache_dir.path().join("Show/Show.S01E03.mkv").exists(),
        "S01E03 (current episode) should also be cached");
}

#[test]
fn copier_copies_file_correctly() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    std::fs::write(backing.path().join("test.mkv"), b"file content").unwrap();

    let backing_store = make_backing_store(backing.path());

    let dest = cache_dir.path().join("test.mkv");
    f_cache::copier::copy_to_cache(
        &backing_store,
        std::path::Path::new("test.mkv"),
        &dest,
    )
    .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), b"file content");
    let mut partial = dest.as_os_str().to_owned();
    partial.push(".partial");
    assert!(!std::path::Path::new(&partial).exists());
}

// ---- max_cache_pull budget tests ----

#[tokio::test]
async fn predictor_budget_zero_means_disabled() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let ep_dir = backing.path().join("tv/Show");
    std::fs::create_dir_all(&ep_dir).unwrap();
    for i in 1..=5u32 {
        std::fs::write(ep_dir.join(format!("Show.S01E0{}.mkv", i)), vec![b'x'; 100]).unwrap();
    }

    let backing_store = make_backing_store(backing.path());
    let cache = Arc::new(CacheManager::new(cache_dir.path().to_path_buf(), cache_dir.path().to_path_buf(), 1.0, 72, 0.0));
    let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
    let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(32);
    let engine = make_engine(access_rx, copy_tx, Arc::clone(&cache), 4, Arc::clone(&backing_store), 0);
    tokio::spawn(engine.run());
    tokio::spawn(run_copier_task(Arc::clone(&backing_store), copy_rx, Arc::clone(&cache)));

    access_tx.send(AccessEvent::miss(PathBuf::from("tv/Show/Show.S01E01.mkv"))).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    for i in 2..=5u32 {
        let cached = cache_dir.path().join(format!("tv/Show/Show.S01E0{}.mkv", i));
        assert!(cached.exists(), "expected ep {} in cache with budget=0", i);
    }
}

#[tokio::test]
async fn predictor_respects_max_cache_pull_budget() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let ep_dir = backing.path().join("tv/Show");
    std::fs::create_dir_all(&ep_dir).unwrap();
    for i in 1..=5u32 {
        std::fs::write(ep_dir.join(format!("Show.S01E0{}.mkv", i)), vec![b'x'; 100]).unwrap();
    }

    let backing_store = make_backing_store(backing.path());
    let cache = Arc::new(CacheManager::new(cache_dir.path().to_path_buf(), cache_dir.path().to_path_buf(), 1.0, 72, 0.0));
    let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
    let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(32);
    let engine = make_engine(access_rx, copy_tx, Arc::clone(&cache), 4, Arc::clone(&backing_store), 250);
    tokio::spawn(engine.run());
    tokio::spawn(run_copier_task(Arc::clone(&backing_store), copy_rx, Arc::clone(&cache)));

    access_tx.send(AccessEvent::miss(PathBuf::from("tv/Show/Show.S01E01.mkv"))).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert!(cache_dir.path().join("tv/Show/Show.S01E01.mkv").exists(), "E01 (current) must be cached (first-candidate exemption)");
    assert!(cache_dir.path().join("tv/Show/Show.S01E02.mkv").exists(), "E02 must be cached");
    assert!(!cache_dir.path().join("tv/Show/Show.S01E03.mkv").exists(), "E03 must NOT be cached (budget exceeded)");
    assert!(!cache_dir.path().join("tv/Show/Show.S01E04.mkv").exists(), "E04 must NOT be cached");
    assert!(!cache_dir.path().join("tv/Show/Show.S01E05.mkv").exists(), "E05 must NOT be cached");
}

#[tokio::test]
async fn predictor_first_candidate_always_queued() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let ep_dir = backing.path().join("tv/Show");
    std::fs::create_dir_all(&ep_dir).unwrap();
    for i in 1..=3u32 {
        std::fs::write(ep_dir.join(format!("Show.S01E0{}.mkv", i)), vec![b'x'; 200]).unwrap();
    }

    let backing_store = make_backing_store(backing.path());
    let cache = Arc::new(CacheManager::new(cache_dir.path().to_path_buf(), cache_dir.path().to_path_buf(), 1.0, 72, 0.0));
    let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
    let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(32);
    let engine = make_engine(access_rx, copy_tx, Arc::clone(&cache), 2, Arc::clone(&backing_store), 50);
    tokio::spawn(engine.run());
    tokio::spawn(run_copier_task(Arc::clone(&backing_store), copy_rx, Arc::clone(&cache)));

    access_tx.send(AccessEvent::miss(PathBuf::from("tv/Show/Show.S01E01.mkv"))).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert!(cache_dir.path().join("tv/Show/Show.S01E01.mkv").exists(), "E01 (current) must be cached (first-candidate exemption)");
    assert!(!cache_dir.path().join("tv/Show/Show.S01E02.mkv").exists(), "E02 must NOT be cached (budget exceeded)");
    assert!(!cache_dir.path().join("tv/Show/Show.S01E03.mkv").exists(), "E03 must NOT be cached");
}

#[tokio::test]
async fn predictor_budget_includes_existing_cache() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let ep_dir = backing.path().join("tv/Show");
    std::fs::create_dir_all(&ep_dir).unwrap();
    for i in 1..=5u32 {
        std::fs::write(ep_dir.join(format!("Show.S01E0{}.mkv", i)), vec![b'x'; 100]).unwrap();
    }

    let cache_ep_dir = cache_dir.path().join("tv/Show");
    std::fs::create_dir_all(&cache_ep_dir).unwrap();
    std::fs::write(cache_ep_dir.join("Show.S01E02.mkv"), vec![b'x'; 100]).unwrap();

    let backing_store = make_backing_store(backing.path());
    let cache = Arc::new(CacheManager::new(cache_dir.path().to_path_buf(), cache_dir.path().to_path_buf(), 1.0, 72, 0.0));
    // Reconcile the pre-placed E02 into the DB so total_cached_bytes() accounts for it.
    cache.startup_cleanup();
    let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
    let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(32);
    let engine = make_engine(access_rx, copy_tx, Arc::clone(&cache), 4, Arc::clone(&backing_store), 250);
    tokio::spawn(engine.run());
    tokio::spawn(run_copier_task(Arc::clone(&backing_store), copy_rx, Arc::clone(&cache)));

    access_tx.send(AccessEvent::miss(PathBuf::from("tv/Show/Show.S01E02.mkv"))).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert_eq!(
        std::fs::read(cache_ep_dir.join("Show.S01E02.mkv")).unwrap(),
        vec![b'x'; 100],
        "E02 must not be overwritten"
    );
    assert!(cache_ep_dir.join("Show.S01E03.mkv").exists(), "E03 must be cached");
    assert!(!cache_ep_dir.join("Show.S01E04.mkv").exists(), "E04 must NOT be cached (budget includes existing E02)");
    assert!(!cache_ep_dir.join("Show.S01E05.mkv").exists(), "E05 must NOT be cached");
}

// ---- deferred event tests ----

#[test]
fn show_root_uses_parent_dir() {
    assert_eq!(show_root(std::path::Path::new("Show/Season 1/S01E03.mkv")),
               std::path::PathBuf::from("Show/Season 1"));
    assert_eq!(show_root(std::path::Path::new("tv/Show/Show.S01E03.mkv")),
               std::path::PathBuf::from("tv/Show"));
    assert_eq!(show_root(std::path::Path::new("file.mkv")),
               std::path::PathBuf::from(""));
}

#[test]
fn buffer_event_keeps_most_advanced() {
    use std::collections::HashMap;
    let mut deferred = HashMap::new();

    buffer_event(&mut deferred, AccessEvent::miss(PathBuf::from("Show/Show.S01E01.mkv")));
    buffer_event(&mut deferred, AccessEvent::miss(PathBuf::from("Show/Show.S01E04.mkv")));
    buffer_event(&mut deferred, AccessEvent::miss(PathBuf::from("Show/Show.S01E02.mkv")));

    assert_eq!(deferred.len(), 1);
    let kept = deferred.values().next().unwrap();
    assert_eq!(kept.relative_path, PathBuf::from("Show/Show.S01E04.mkv"));
}

#[test]
fn buffer_event_different_dirs_are_separate() {
    use std::collections::HashMap;
    let mut deferred = HashMap::new();

    buffer_event(&mut deferred, AccessEvent::miss(PathBuf::from("ShowA/Show.S01E01.mkv")));
    buffer_event(&mut deferred, AccessEvent::miss(PathBuf::from("ShowB/Show.S01E01.mkv")));

    assert_eq!(deferred.len(), 2);
}

#[test]
fn buffer_event_cross_season_keeps_higher() {
    use std::collections::HashMap;
    let mut deferred = HashMap::new();

    buffer_event(&mut deferred, AccessEvent::miss(PathBuf::from("Show/Show.S01E08.mkv")));
    buffer_event(&mut deferred, AccessEvent::miss(PathBuf::from("Show/Show.S02E01.mkv")));

    assert_eq!(deferred.len(), 1);
    let kept = deferred.values().next().unwrap();
    assert_eq!(kept.relative_path, PathBuf::from("Show/Show.S02E01.mkv"));
}

#[test]
fn buffer_event_non_episode_keyed_by_full_path() {
    use std::collections::HashMap;
    let mut deferred = HashMap::new();

    buffer_event(&mut deferred, AccessEvent::miss(PathBuf::from("movies/Movie.mkv")));
    buffer_event(&mut deferred, AccessEvent::miss(PathBuf::from("movies/Other.mkv")));

    assert_eq!(deferred.len(), 2);
}

#[test]
fn load_deferred_discards_stale_entries() {
    let db = CacheDb::open(std::path::Path::new(":memory:")).unwrap();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Stale entry (timestamp=0, far older than any TTL).
    db.save_deferred(
        &PathBuf::from("Show"),
        &PathBuf::from("Show/Show.S01E01.mkv"),
        0,
    );
    // Fresh entry (current timestamp).
    db.save_deferred(
        &PathBuf::from("Show"),
        &PathBuf::from("Show/Show.S01E04.mkv"),
        now,
    );

    let loaded = db.load_deferred(60);
    assert_eq!(loaded.len(), 1, "stale entry should be discarded");
    let (_, kept_path, _) = &loaded[0];
    assert_eq!(*kept_path, PathBuf::from("Show/Show.S01E04.mkv"));
}
