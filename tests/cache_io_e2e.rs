/// End-to-end tests for CacheIO pipeline correctness.
///
/// These tests target the deferred-write machinery specifically:
///
///   1. DB persistence on submit — every `submit_cache` call writes a deferred row
///      that survives until the copy completes.
///
///   2. Restart rehydration — on `CacheIO::spawn`, the in-memory queue is seeded
///      from persisted DB rows, so jobs queued before a restart are not lost.
///
///   3. finish_known cleanup (copy success) — after a successful copy the DB row is
///      removed. If it weren't, the path would be permanently stuck in `known`.
///
///   4. finish_known cleanup (copy failure) — if the backing file doesn't exist the
///      copy fails; `finish_known` must still remove the DB row and release `known`
///      so the path can be re-submitted after the backing becomes available.
///
///   5. is_cached short-circuit cleans up — if the file arrives in the cache between
///      submit and copy_worker execution, the worker short-circuits and must still
///      call `finish_known`. Without the fix the path was permanently stuck in `known`
///      and all future submissions of the same path were silently dropped.
///
///   6. Dedup — submitting the same path twice while the first is still in-flight
///      results in exactly one copy, not two concurrent writes.
///
///   7. Copy fidelity — the cached file is byte-for-byte identical to the backing file.
///
///   8. Metadata preservation — mode and mtime on the backing file survive the copy.
///
///   9. No .partial on failure — a failed copy leaves no .partial file in the cache dir.
mod common;

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;

use fscache::backing_store::BackingStore;
use fscache::cache::db::CacheDb;
use fscache::cache::io::{CacheIO, CacheIoConfig};
use fscache::cache::manager::CacheManager;
use fscache::config::InvalidationConfig;
use fscache::engine::scheduler::Scheduler;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use tokio_util::sync::CancellationToken;

fn open_backing_store(dir: &std::path::Path) -> Arc<BackingStore> {
    let c = CString::new(dir.as_os_str().as_bytes()).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY) };
    assert!(fd >= 0, "O_PATH open of backing dir failed");
    Arc::new(BackingStore::new(fd))
}

/// Spawn a `CacheIO` with an always-open scheduling window.
fn spawn_cache_io(
    cache_mgr: Arc<CacheManager>,
    backing_store: Arc<BackingStore>,
    deferred_ttl_minutes: u64,
) -> CacheIO {
    let scheduler = Scheduler::new("00:00", "23:59").unwrap();
    let (cache_io, _io_handles) = CacheIO::spawn(
        CacheIoConfig {
            max_concurrent_copies: 1,
            eviction_interval_secs: 0,
            deferred_ttl_minutes,
        },
        cache_mgr,
        backing_store,
        scheduler,
        CancellationToken::new(),
    );
    cache_io
}

fn make_db(cache_dir: &std::path::Path) -> Arc<CacheDb> {
    Arc::new(CacheDb::open(&cache_dir.join("test.db")).unwrap())
}

fn make_cache_mgr(cache_dir: &std::path::Path, db: Arc<CacheDb>) -> Arc<CacheManager> {
    let mgr = Arc::new(CacheManager::new(
        cache_dir.to_path_buf(),
        db,
        cache_dir.to_path_buf(),
        1.0,  // max_size_gb
        9999, // expiry_hours
        0.0,  // min_free_space_gb
        None,
        &InvalidationConfig::default(),
    ));
    mgr.startup_cleanup();
    mgr
}

/// Submit a real backing file and verify the deferred DB row is written immediately
/// and then cleaned up after the copy completes successfully.
#[tokio::test]
async fn deferred_db_row_written_on_submit_and_cleared_after_copy() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let rel = PathBuf::from("show/ep01.mkv");
    std::fs::create_dir_all(backing.path().join("show")).unwrap();
    std::fs::write(backing.path().join(&rel), b"episode content").unwrap();

    let db = make_db(cache_dir.path());
    let cache_mgr = make_cache_mgr(cache_dir.path(), Arc::clone(&db));
    let backing_store = open_backing_store(backing.path());

    let cache_io = spawn_cache_io(Arc::clone(&cache_mgr), backing_store, 1440);

    cache_io.submit_cache(rel.clone()).await;

    // DB row must exist right after submit (before any copy completes).
    let rows_after_submit = db.load_deferred(1440);
    assert_eq!(
        rows_after_submit.len(),
        1,
        "deferred DB row should be written on submit"
    );

    // Wait for copy_worker to finish.
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        cache_dir.path().join(&rel).exists(),
        "file should be in cache after successful copy"
    );
    let rows_after_copy = db.load_deferred(1440);
    assert!(
        rows_after_copy.is_empty(),
        "deferred DB row should be cleared after successful copy, got {:?}",
        rows_after_copy
    );
}

/// On `CacheIO::spawn`, persisted deferred rows are reloaded from the DB and the
/// corresponding files are copied — simulating daemon restart with queued work.
#[tokio::test]
async fn deferred_jobs_rehydrated_from_db_on_spawn() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let rel = PathBuf::from("show/ep02.mkv");
    std::fs::create_dir_all(backing.path().join("show")).unwrap();
    std::fs::write(backing.path().join(&rel), b"episode data").unwrap();

    let db = make_db(cache_dir.path());

    // Seed the DB directly, bypassing CacheIO — simulates a job queued before restart.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    db.save_deferred(&rel, &rel, now);

    // Confirm the row is in the DB before spawning.
    assert_eq!(
        db.load_deferred(1440).len(),
        1,
        "seeded row should be in DB"
    );

    // Spawn CacheIO — it should rehydrate the job and process it.
    let cache_mgr = make_cache_mgr(cache_dir.path(), Arc::clone(&db));
    let backing_store = open_backing_store(backing.path());
    let _cache_io = spawn_cache_io(Arc::clone(&cache_mgr), backing_store, 1440);

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        cache_dir.path().join(&rel).exists(),
        "rehydrated job should have been processed: file not found in cache"
    );
    assert!(
        db.load_deferred(1440).is_empty(),
        "deferred DB row should be cleared after rehydrated copy completes"
    );
}

/// If the backing file does not exist, copy_worker fails. `finish_known` must still
/// remove the DB row and release the path from `known`, otherwise the path is
/// permanently stuck and can never be re-queued after the backing becomes available.
#[tokio::test]
async fn deferred_db_row_cleared_after_copy_failure() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    // Intentionally do NOT create the backing file.
    let rel = PathBuf::from("missing/ep01.mkv");

    let db = make_db(cache_dir.path());
    let cache_mgr = make_cache_mgr(cache_dir.path(), Arc::clone(&db));
    let backing_store = open_backing_store(backing.path());

    let cache_io = spawn_cache_io(Arc::clone(&cache_mgr), backing_store, 1440);
    cache_io.submit_cache(rel.clone()).await;

    // Row should be written immediately on submit.
    assert_eq!(
        db.load_deferred(1440).len(),
        1,
        "row should exist right after submit"
    );

    // Wait for copy_worker to fail and call finish_known.
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        db.load_deferred(1440).is_empty(),
        "deferred DB row must be cleared even after a copy failure"
    );

    // Verify the path is no longer stuck in `known`: submitting again should
    // write a new DB row (rather than being silently dropped).
    std::fs::create_dir_all(backing.path().join("missing")).unwrap();
    std::fs::write(backing.path().join(&rel), b"now it exists").unwrap();

    cache_io.submit_cache(rel.clone()).await;
    let rows = db.load_deferred(1440);
    assert_eq!(
        rows.len(),
        1,
        "after a failed copy the path must be re-submittable (not stuck in known)"
    );
}

/// If the file arrives in the cache between `submit_cache` and `copy_worker` execution,
/// the worker's `is_cached` short-circuit fires. `finish_known` must be called in that
/// branch — without the fix the path was permanently stuck in `known` and any later
/// submission of the same path was silently dropped.
///
/// Single-threaded tokio runtime: other tasks only run at `.await` points, so we can
/// place the file in the cache after `submit_cache` returns but before the next `.await`,
/// guaranteeing the file is already cached when `copy_worker` first checks.
#[tokio::test]
async fn deferred_db_row_cleared_when_copy_worker_finds_file_already_cached() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let rel = PathBuf::from("show/ep03.mkv");
    std::fs::create_dir_all(backing.path().join("show")).unwrap();
    std::fs::write(backing.path().join(&rel), b"content").unwrap();

    let db = make_db(cache_dir.path());
    let cache_mgr = make_cache_mgr(cache_dir.path(), Arc::clone(&db));
    let backing_store = open_backing_store(backing.path());

    let cache_io = spawn_cache_io(Arc::clone(&cache_mgr), Arc::clone(&backing_store), 1440);

    // submit_cache queues the path and writes the DB row, then returns.
    // No await has happened yet so the dispatcher hasn't run.
    cache_io.submit_cache(rel.clone()).await;

    // Place the file in the cache dir directly — copy_worker will see is_cached = true.
    let cache_dest = cache_mgr.cache_path(&rel);
    std::fs::create_dir_all(cache_dest.parent().unwrap()).unwrap();
    std::fs::copy(backing.path().join(&rel), &cache_dest).unwrap();

    // Yield: dispatcher runs, dequeues, spawns copy_worker which short-circuits.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        db.load_deferred(1440).is_empty(),
        "deferred DB row must be cleared even when copy_worker short-circuits on is_cached"
    );

    // Verify `known` was released: evict the cache file, then re-submit.
    // If the path were still stuck in `known`, submit_cache would silently drop it
    // and no new DB row would be written.
    std::fs::remove_file(&cache_dest).unwrap();
    cache_io.submit_cache(rel.clone()).await;
    assert_eq!(
        db.load_deferred(1440).len(),
        1,
        "after is_cached short-circuit, the path must be re-submittable (not stuck in known)"
    );
}

/// Submitting the same path twice while the first is still in-flight must result in
/// exactly one copy. The `known` HashSet in `PipelineState` is the dedup gate.
#[tokio::test]
async fn duplicate_submit_produces_one_copy() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let rel = PathBuf::from("show/ep04.mkv");
    std::fs::create_dir_all(backing.path().join("show")).unwrap();
    std::fs::write(backing.path().join(&rel), b"unique content").unwrap();

    let db = make_db(cache_dir.path());
    let cache_mgr = make_cache_mgr(cache_dir.path(), Arc::clone(&db));
    let backing_store = open_backing_store(backing.path());

    let cache_io = spawn_cache_io(Arc::clone(&cache_mgr), backing_store, 1440);

    // Submit twice before any copy can start (no await between the two submits).
    cache_io.submit_cache(rel.clone()).await;
    cache_io.submit_cache(rel.clone()).await;

    // Only one deferred row should exist.
    assert_eq!(
        db.load_deferred(1440).len(),
        1,
        "duplicate submit should produce exactly one deferred DB row"
    );

    tokio::time::sleep(Duration::from_millis(500)).await;

    // File is present and correct — not corrupted by a double-write race.
    let content = std::fs::read(cache_dir.path().join(&rel)).unwrap();
    assert_eq!(content, b"unique content", "copied content must be correct");

    // DB is clean after the single copy.
    assert!(
        db.load_deferred(1440).is_empty(),
        "deferred DB row must be cleared after the single copy completes"
    );
}

/// The cached file must be a byte-for-byte copy of the backing file. Tests that
/// `perform_copy`'s explicit read/write loop produces a correct result end-to-end.
#[tokio::test]
async fn copy_produces_correct_byte_content() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let rel = PathBuf::from("movies/film.mkv");
    std::fs::create_dir_all(backing.path().join("movies")).unwrap();
    // 300 KiB of patterned data — large enough to span multiple 256 KiB copy chunks.
    let content: Vec<u8> = (0u32..307_200).map(|i| (i % 251) as u8).collect();
    std::fs::write(backing.path().join(&rel), &content).unwrap();

    let db = make_db(cache_dir.path());
    let cache_mgr = make_cache_mgr(cache_dir.path(), Arc::clone(&db));
    let backing_store = open_backing_store(backing.path());

    let cache_io = spawn_cache_io(Arc::clone(&cache_mgr), backing_store, 1440);
    cache_io.submit_cache(rel.clone()).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let cached =
        std::fs::read(cache_dir.path().join(&rel)).expect("cached file not found after copy");
    assert_eq!(
        cached, content,
        "cached file content must be identical to backing file"
    );
}

/// Mode and mtime set on the backing file must survive the CacheIO copy pipeline.
/// This exercises `apply_source_metadata` end-to-end via `copy_worker` → `perform_copy`.
#[tokio::test]
async fn copy_preserves_metadata() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    let rel = PathBuf::from("tv/Show/S01E01.mkv");
    std::fs::create_dir_all(backing.path().join("tv/Show")).unwrap();
    std::fs::write(backing.path().join(&rel), b"episode content").unwrap();

    let backing_path = backing.path().join(&rel);
    std::fs::set_permissions(&backing_path, std::fs::Permissions::from_mode(0o640))
        .expect("set_permissions failed");
    filetime::set_file_mtime(
        &backing_path,
        filetime::FileTime::from_unix_time(1_700_000_000, 0),
    )
    .expect("set_file_mtime failed");

    let db = make_db(cache_dir.path());
    let cache_mgr = make_cache_mgr(cache_dir.path(), Arc::clone(&db));
    let backing_store = open_backing_store(backing.path());

    let cache_io = spawn_cache_io(Arc::clone(&cache_mgr), backing_store, 1440);
    cache_io.submit_cache(rel.clone()).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let cache_path = cache_dir.path().join(&rel);
    assert!(cache_path.exists(), "cached file not found after copy");

    let cache_meta = std::fs::metadata(&cache_path).expect("metadata on cache file failed");
    let backing_meta = std::fs::metadata(&backing_path).expect("metadata on backing file failed");

    assert_eq!(
        cache_meta.permissions().mode() & 0o7777,
        backing_meta.permissions().mode() & 0o7777,
        "cached file mode must match backing file"
    );
    assert_eq!(
        cache_meta.mtime(),
        backing_meta.mtime(),
        "cached file mtime must match backing file"
    );
}

/// A copy that fails (backing file missing) must not leave a `.partial` file behind.
#[tokio::test]
async fn copy_failure_leaves_no_partial_on_disk() {
    let backing = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();

    // Intentionally do NOT create the backing file.
    let rel = PathBuf::from("missing/episode.mkv");

    let db = make_db(cache_dir.path());
    let cache_mgr = make_cache_mgr(cache_dir.path(), Arc::clone(&db));
    let backing_store = open_backing_store(backing.path());

    let cache_io = spawn_cache_io(Arc::clone(&cache_mgr), backing_store, 1440);
    cache_io.submit_cache(rel.clone()).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Walk the entire cache dir and fail if any .partial file exists.
    fn find_partials(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    found.extend(find_partials(&path));
                } else if path.to_string_lossy().ends_with(".partial") {
                    found.push(path);
                }
            }
        }
        found
    }

    let partials = find_partials(cache_dir.path());
    assert!(
        partials.is_empty(),
        "failed copy must not leave .partial files behind: {:?}",
        partials
    );
}
