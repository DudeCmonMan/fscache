mod common;

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tempfile::TempDir;
use tokio::sync::mpsc;

use plex_hot_cache::cache::CacheManager;
use plex_hot_cache::plex_db::PlexDb;
use plex_hot_cache::predictor::{
    parse_season_episode, run_copier_task, AccessEvent, CopyRequest, Predictor,
};
use plex_hot_cache::scheduler::Scheduler;

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

// ---- Plex DB tests ----

fn build_test_db(target_dir: &std::path::Path) -> PlexDb {
    let db = PlexDb::open_in_memory(target_dir).unwrap();
    db.exec(
        r#"
        CREATE TABLE metadata_items (
            id INTEGER PRIMARY KEY,
            metadata_type INTEGER,
            title TEXT,
            "index" INTEGER,
            parent_id INTEGER,
            grandparent_id INTEGER
        );
        CREATE TABLE media_items (id INTEGER PRIMARY KEY, metadata_item_id INTEGER);
        CREATE TABLE media_parts (id INTEGER PRIMARY KEY, media_item_id INTEGER, file TEXT);

        -- Show (type 1)
        INSERT INTO metadata_items VALUES (1, 1, 'TestShow', NULL, NULL, NULL);
        -- Season 1 (type 2, index=1, parent=show)
        INSERT INTO metadata_items VALUES (10, 2, 'Season 1', 1, 1, NULL);
        -- Season 2 (type 2, index=2, parent=show)
        INSERT INTO metadata_items VALUES (20, 2, 'Season 2', 2, 1, NULL);

        -- S01E01
        INSERT INTO metadata_items VALUES (101, 4, 'S01E01', 1, 10, 1);
        INSERT INTO media_items VALUES (1001, 101);
        INSERT INTO media_parts VALUES (10001, 1001, '/mnt/media/tv/TestShow/S01E01.mkv');

        -- S01E02
        INSERT INTO metadata_items VALUES (102, 4, 'S01E02', 2, 10, 1);
        INSERT INTO media_items VALUES (1002, 102);
        INSERT INTO media_parts VALUES (10002, 1002, '/mnt/media/tv/TestShow/S01E02.mkv');

        -- S01E03
        INSERT INTO metadata_items VALUES (103, 4, 'S01E03', 3, 10, 1);
        INSERT INTO media_items VALUES (1003, 103);
        INSERT INTO media_parts VALUES (10003, 1003, '/mnt/media/tv/TestShow/S01E03.mkv');

        -- S02E01
        INSERT INTO metadata_items VALUES (201, 4, 'S02E01', 1, 20, 1);
        INSERT INTO media_items VALUES (2001, 201);
        INSERT INTO media_parts VALUES (20001, 2001, '/mnt/media/tv/TestShow/S02E01.mkv');
        "#,
    )
    .unwrap();
    db
}

#[test]
fn plex_db_finds_next_episodes_in_same_season() {
    let db = build_test_db(std::path::Path::new("/mnt/media"));
    let next = db.next_episodes(std::path::Path::new("tv/TestShow/S01E01.mkv"), 2);
    assert_eq!(next.len(), 2);
    assert!(
        next[0].to_string_lossy().contains("S01E02"),
        "expected S01E02, got {:?}",
        next[0]
    );
    assert!(
        next[1].to_string_lossy().contains("S01E03"),
        "expected S01E03, got {:?}",
        next[1]
    );
}

#[test]
fn plex_db_crosses_season_boundary() {
    let db = build_test_db(std::path::Path::new("/mnt/media"));
    // Lookahead=3 from S01E02: should get S01E03, S02E01 (only 2 remain)
    let next = db.next_episodes(std::path::Path::new("tv/TestShow/S01E02.mkv"), 3);
    assert_eq!(next.len(), 2, "expected S01E03 + S02E01, got {:?}", next);
    assert!(next[0].to_string_lossy().contains("S01E03"));
    assert!(next[1].to_string_lossy().contains("S02E01"));
}

#[test]
fn plex_db_returns_empty_for_unknown_file() {
    let db = build_test_db(std::path::Path::new("/mnt/media"));
    let next = db.next_episodes(std::path::Path::new("tv/Unknown/S01E01.mkv"), 4);
    assert!(next.is_empty());
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

// ---- Integration: predictor + copier ----

fn open_backing_fd(path: &std::path::Path) -> libc::c_int {
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY) }
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

    let backing_fd = open_backing_fd(backing.path());
    assert!(backing_fd >= 0, "failed to open backing_fd");

    let cache = Arc::new(CacheManager::new(
        cache_dir.path().to_path_buf(),
        1.0,
        72,
        0.0,
    ));

    let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
    let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(32);
    let scheduler = Scheduler::new("00:00", "23:59").unwrap();

    let predictor = Predictor::new(
        access_rx,
        copy_tx,
        Arc::clone(&cache),
        4, // lookahead
        None,
        scheduler,
        backing_fd,
    );
    tokio::spawn(predictor.run());
    tokio::spawn(run_copier_task(backing_fd, copy_rx, Arc::clone(&cache)));

    access_tx
        .send(AccessEvent {
            relative_path: PathBuf::from("tv/Show/Show.S01E01.mkv"),
            timestamp: SystemTime::now(),
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(800)).await;

    // E02–E05 should be cached (lookahead=4)
    for i in 2..=5u32 {
        let cached = cache_dir.path().join(format!("tv/Show/Show.S01E0{}.mkv", i));
        assert!(cached.exists(), "expected ep {} in cache", i);
        assert_eq!(
            std::fs::read_to_string(&cached).unwrap(),
            format!("episode {} content", i)
        );
    }
    // E01 itself should NOT be in the cache
    assert!(
        !cache_dir.path().join("tv/Show/Show.S01E01.mkv").exists(),
        "S01E01 should not have been cached"
    );

    unsafe { libc::close(backing_fd) };
}

/// Predictor does not re-copy files already in the cache.
#[tokio::test]
async fn predictor_skips_already_cached() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let ep_dir = backing.path().join("tv/Show");
    std::fs::create_dir_all(&ep_dir).unwrap();
    for i in 1..=3u32 {
        std::fs::write(ep_dir.join(format!("Show.S01E0{}.mkv", i)), b"data").unwrap();
    }

    // Pre-populate E02 with distinct content
    let cache_ep_dir = cache_dir.path().join("tv/Show");
    std::fs::create_dir_all(&cache_ep_dir).unwrap();
    std::fs::write(cache_ep_dir.join("Show.S01E02.mkv"), b"already cached").unwrap();

    let backing_fd = open_backing_fd(backing.path());
    let cache = Arc::new(CacheManager::new(
        cache_dir.path().to_path_buf(),
        1.0,
        72,
        0.0,
    ));
    let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
    let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(32);
    let scheduler = Scheduler::new("00:00", "23:59").unwrap();
    let predictor =
        Predictor::new(access_rx, copy_tx, Arc::clone(&cache), 2, None, scheduler, backing_fd);
    tokio::spawn(predictor.run());
    tokio::spawn(run_copier_task(backing_fd, copy_rx, Arc::clone(&cache)));

    access_tx
        .send(AccessEvent {
            relative_path: PathBuf::from("tv/Show/Show.S01E01.mkv"),
            timestamp: SystemTime::now(),
        })
        .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    // E02 already cached — must not be overwritten
    assert_eq!(
        std::fs::read(cache_ep_dir.join("Show.S01E02.mkv")).unwrap(),
        b"already cached"
    );
    // E03 should now be cached
    assert!(
        cache_ep_dir.join("Show.S01E03.mkv").exists(),
        "E03 should be cached"
    );

    unsafe { libc::close(backing_fd) };
}

/// copier::copy_to_cache correctly copies a file via backing_fd.
#[test]
fn copier_copies_file_correctly() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    std::fs::write(backing.path().join("test.mkv"), b"file content").unwrap();

    let backing_fd = open_backing_fd(backing.path());
    assert!(backing_fd >= 0);

    let dest = cache_dir.path().join("test.mkv");
    plex_hot_cache::copier::copy_to_cache(
        backing_fd,
        std::path::Path::new("test.mkv"),
        &dest,
    )
    .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), b"file content");
    // No .partial file should remain
    let mut partial = dest.as_os_str().to_owned();
    partial.push(".partial");
    assert!(!std::path::Path::new(&partial).exists());

    unsafe { libc::close(backing_fd) };
}
