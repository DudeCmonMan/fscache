use std::collections::{HashSet, VecDeque};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex, Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::backing_store::BackingStore;
use crate::cache::manager::CacheManager;
use crate::engine::scheduler::Scheduler;
use crate::telemetry;

pub struct CacheIoConfig {
    /// Maximum number of backing→cache copies running concurrently.
    pub max_concurrent_copies: usize,
    /// How often the autonomous eviction worker runs (seconds). 0 disables it.
    pub eviction_interval_secs: u64,
    /// Discard deferred jobs older than this many minutes on startup and during TTL sweep.
    pub deferred_ttl_minutes: u64,
}

/// Single source of truth for all pending and in-flight cache work.
///
/// The `queue` is a FIFO of paths waiting for the caching window to open.
/// `known` tracks every path that CacheIO has seen — queued or in flight —
/// to prevent duplicate submissions. A path is removed from `known` only when
/// its copy worker finishes (success or failure), so dedup is airtight across
/// the queue-to-in-flight transition.
struct PipelineState {
    /// FIFO of `(rel_path, enqueue_timestamp_secs)`.
    queue: VecDeque<(PathBuf, u64)>,
    /// All paths currently queued OR in flight. Drives submit-time dedup.
    known: HashSet<PathBuf>,
}

impl PipelineState {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            known: HashSet::new(),
        }
    }
}

/// Owner of all cache filesystem I/O: the copy pipeline and the eviction pipeline.
///
/// Cloning produces a second handle backed by the same internal state — safe to
/// share across tasks.
///
/// Every path submitted via `submit_cache` is enqueued unconditionally (dedup
/// aside). The caching window only controls whether the dispatcher is allowed to
/// drain the queue. There is exactly one code path into the copy pipeline.
#[derive(Clone)]
pub struct CacheIO {
    cache:                Arc<CacheManager>,
    backing_store:        Arc<BackingStore>,
    scheduler:            Scheduler,
    state:                Arc<Mutex<PipelineState>>,
    notify:               Arc<Notify>,
    semaphore:            Arc<Semaphore>,
    evict_lock:           Arc<Mutex<()>>,
    deferred_ttl_minutes: u64,
}

impl CacheIO {
    /// Spawn the dispatcher and eviction worker tasks, then return a shareable handle.
    ///
    /// Returns `(handle, task_handles)`. In production, callers should move the task
    /// handles into a `JoinSet` so they can be drained on shutdown. In tests, dropping
    /// the handles is fine — the tasks run until the tokio runtime ends.
    pub fn spawn(
        cfg: CacheIoConfig,
        cache: Arc<CacheManager>,
        backing_store: Arc<BackingStore>,
        scheduler: Scheduler,
        shutdown: CancellationToken,
    ) -> (Self, Vec<tokio::task::JoinHandle<()>>) {
        let db = cache.cache_db();

        let mut initial = PipelineState::new();
        let loaded = db.load_deferred(cfg.deferred_ttl_minutes);
        if !loaded.is_empty() {
            tracing::info!("cache_io: loaded {} deferred job(s) from DB", loaded.len());
        }
        for (_, path, ts) in loaded {
            if initial.known.insert(path.clone()) {
                initial.queue.push_back((path, ts));
            }
        }
        let initial_count = initial.queue.len() as u64;
        if initial_count > 0 {
            tracing::debug!(
                event = telemetry::EVENT_DEFERRED_CHANGED,
                count = initial_count,
                "cache_io: {} deferred job(s) restored from DB",
                initial_count,
            );
        }

        let handle = CacheIO {
            cache: Arc::clone(&cache),
            backing_store: Arc::clone(&backing_store),
            scheduler,
            state: Arc::new(Mutex::new(initial)),
            notify: Arc::new(Notify::new()),
            semaphore: Arc::new(Semaphore::new(cfg.max_concurrent_copies)),
            evict_lock: Arc::new(Mutex::new(())),
            deferred_ttl_minutes: cfg.deferred_ttl_minutes,
        };

        let mut task_handles = Vec::new();
        task_handles.push(tokio::spawn(dispatcher(handle.clone(), shutdown.clone())));

        // If we rehydrated entries from the DB, wake the dispatcher immediately
        // rather than waiting for the first submit_cache call or the 10-second tick.
        if initial_count > 0 {
            handle.notify.notify_one();
        }

        if cfg.eviction_interval_secs > 0 {
            let evict_cache = Arc::clone(&cache);
            let interval_secs = cfg.eviction_interval_secs;
            let evict_shutdown = shutdown.clone();
            task_handles.push(tokio::spawn(async move {
                eviction_worker(evict_cache, interval_secs, evict_shutdown).await;
            }));
        }

        (handle, task_handles)
    }

    /// Submit a path for caching. This is the single entrypoint for all cache requests.
    ///
    /// The path is pushed onto the queue unconditionally (subject to dedup and
    /// already-cached checks). The caching window check happens in the dispatcher —
    /// not here. There is no separate "deferred vs. immediate" branch.
    pub async fn submit_cache(&self, rel_path: PathBuf) {
        // Cheap check before taking the lock — avoids contention on cache hits.
        if self.cache.is_cached(&rel_path) {
            tracing::debug!("cache_io: {} already cached, skipping", rel_path.display());
            return;
        }

        let queue_len = {
            let mut st = self.state.lock().await;
            if !st.known.insert(rel_path.clone()) {
                tracing::debug!(
                    "cache_io: {} already queued or in flight, skipping",
                    rel_path.display()
                );
                return;
            }
            let now = now_secs();
            st.queue.push_back((rel_path.clone(), now));
            let len = st.queue.len() as u64;

            // Persist while holding the lock so the DB row and the in-memory
            // entry are always in sync.
            self.cache.cache_db().save_deferred(&rel_path, &rel_path, now);
            len
        };

        tracing::info!(
            event = telemetry::EVENT_COPY_QUEUED,
            path = %rel_path.display(),
            "cache_io: queued {} for caching ({} pending)",
            rel_path.display(),
            queue_len,
        );
        tracing::debug!(
            event = telemetry::EVENT_DEFERRED_CHANGED,
            count = queue_len,
            "cache_io: queue depth {}",
            queue_len,
        );

        self.notify.notify_one();
    }

    /// Remove TTL-expired entries from the front of the queue.
    ///
    /// The queue is FIFO-ordered by enqueue time, so all stale entries will be
    /// at the front. This is called from the dispatcher on every tick.
    async fn expire_stale(&self) {
        if self.deferred_ttl_minutes == 0 {
            return;
        }
        let cutoff = now_secs().saturating_sub(self.deferred_ttl_minutes * 60);
        let removed: Vec<PathBuf> = {
            let mut st = self.state.lock().await;
            let mut expired = Vec::new();
            while let Some((_, ts)) = st.queue.front() {
                if *ts >= cutoff {
                    break;
                }
                let (path, _) = st.queue.pop_front().unwrap();
                st.known.remove(&path);
                expired.push(path);
            }
            expired
        };

        if !removed.is_empty() {
            let db = self.cache.cache_db();
            for path in &removed {
                tracing::debug!("cache_io: deferred job expired (TTL): {}", path.display());
                db.remove_deferred(path);
            }
            let count = self.state.lock().await.queue.len() as u64;
            tracing::debug!(
                event = telemetry::EVENT_DEFERRED_CHANGED,
                count,
                "cache_io: {} job(s) after TTL expiry",
                count,
            );
        }
    }
}

/// The dispatcher is the only place the caching window is consulted.
/// It wakes on every `submit_cache` call and every 10 s tick (to re-check the
/// window when no submissions are arriving), draining the queue whenever allowed.
async fn dispatcher(io: CacheIO, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            _ = io.notify.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            _ = shutdown.cancelled() => break,
        }

        let allowed = io.scheduler.is_caching_allowed();
        tracing::debug!(
            event = telemetry::EVENT_CACHING_WINDOW,
            allowed,
            "cache_io: window check",
        );

        // Always run TTL sweep, even when the window is closed.
        io.expire_stale().await;

        if !allowed {
            continue;
        }

        // Drain the queue. Acquiring the semaphore permit before spawning
        // provides backpressure: when all workers are busy the dispatcher
        // blocks here (rather than pre-spawning unbounded tasks), so the
        // queue reflects real backlog instead of just "in-flight + queued".
        loop {
            if shutdown.is_cancelled() { break; }
            let next = { io.state.lock().await.queue.pop_front() };
            let Some((rel_path, _)) = next else { break };

            let permit = io.semaphore.clone().acquire_owned().await
                .expect("semaphore closed");

            let io2 = io.clone();
            tokio::spawn(async move {
                copy_worker(io2, rel_path).await;
                drop(permit);
            });
        }
    }
}

async fn copy_worker(io: CacheIO, rel_path: PathBuf) {
    if io.cache.is_cached(&rel_path) {
        tracing::debug!("cache_io: {} already cached, skipping", rel_path.display());
        finish_known(&io, &rel_path).await;
        return;
    }

    // On-demand eviction: make room if needed. The evict_lock serialises concurrent
    // workers so they don't compute the same deletion set and over-evict.
    if !io.cache.has_free_space() {
        let size_bytes = io.backing_store.file_size(&rel_path).unwrap_or(0);
        let _guard = io.evict_lock.lock().await;
        if !io.cache.has_free_space() {
            let freed = io.cache.evict_to_fit(size_bytes);
            tracing::info!(
                "cache_io: evicted {:.1} MB on demand to fit {}",
                freed as f64 / 1_048_576.0,
                rel_path.display(),
            );
        }
    }

    if !io.cache.has_free_space() {
        tracing::warn!(
            event = telemetry::EVENT_COPY_FAILED,
            path = %rel_path.display(),
            "cache_io: insufficient free space after eviction, skipping {}",
            rel_path.display(),
        );
        finish_known(&io, &rel_path).await;
        return;
    }

    let size_bytes = io.backing_store.file_size(&rel_path).unwrap_or(0);
    let cache_dest = io.cache.cache_path(&rel_path);

    tracing::info!(
        event = telemetry::EVENT_COPY_STARTED,
        path = %rel_path.display(),
        size_bytes,
        "cache_io: caching {}",
        rel_path.display(),
    );

    let bytes_copied = Arc::new(AtomicU64::new(0));
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

    // Fires zero times for copies that complete in under 500 ms.
    {
        let ticker_bytes = Arc::clone(&bytes_copied);
        let ticker_path  = rel_path.clone();
        let ticker_size  = size_bytes;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await; // discard immediate first tick
            let mut done = done_rx;
            loop {
                tokio::select! {
                    _ = &mut done => break,
                    _ = interval.tick() => {
                        tracing::debug!(
                            event = telemetry::EVENT_COPY_PROGRESS,
                            path = %ticker_path.display(),
                            bytes_copied = ticker_bytes.load(Ordering::Relaxed),
                            size_bytes = ticker_size,
                            "cache_io: progress",
                        );
                    }
                }
            }
        });
    }

    let bs = Arc::clone(&io.backing_store);
    let rel = rel_path.clone();
    let dest = cache_dest.clone();
    let progress = Arc::clone(&bytes_copied);
    let result = tokio::task::spawn_blocking(move || {
        perform_copy(&bs, &rel, &dest, &progress)
    })
    .await;

    // Stop the ticker before emitting COPY_COMPLETE / COPY_FAILED so no
    // late progress event races past the completion signal.
    let _ = done_tx.send(());

    match result {
        Ok(Ok(())) => {
            let (size, mtime_secs, mtime_nsecs) = match std::fs::metadata(&cache_dest) {
                Ok(m) => (m.len(), m.mtime(), m.mtime_nsec()),
                Err(_) => (0, 0, 0),
            };
            io.cache.mark_cached(&rel_path, size, mtime_secs, mtime_nsecs);
            tracing::info!(
                event = telemetry::EVENT_COPY_COMPLETE,
                path = %rel_path.display(),
                "cache_io: cached {}",
                rel_path.display(),
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(
                event = telemetry::EVENT_COPY_FAILED,
                path = %rel_path.display(),
                "cache_io: copy failed {}: {e}",
                rel_path.display(),
            );
        }
        Err(e) => {
            tracing::warn!(
                event = telemetry::EVENT_COPY_FAILED,
                path = %rel_path.display(),
                "cache_io: task panicked {}: {e}",
                rel_path.display(),
            );
        }
    }

    finish_known(&io, &rel_path).await;
}

async fn finish_known(io: &CacheIO, rel_path: &Path) {
    io.state.lock().await.known.remove(rel_path);
    io.cache.cache_db().remove_deferred(rel_path);
}

async fn eviction_worker(cache: Arc<CacheManager>, interval_secs: u64, shutdown: CancellationToken) {
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs));
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let cache_clone = Arc::clone(&cache);
                tokio::task::spawn_blocking(move || cache_clone.evict_if_needed())
                    .await
                    .ok();
            }
            _ = shutdown.cancelled() => break,
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Uses an explicit read/write loop rather than `std::io::copy` so that
/// byte-level progress reporting can be added (tick `bytes_done` per chunk)
/// without restructuring this function.
fn perform_copy(
    bs: &BackingStore,
    rel_path: &Path,
    cache_dest: &Path,
    bytes_copied: &AtomicU64,
) -> std::io::Result<()> {
    if let Some(parent) = cache_dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let partial = partial_path(cache_dest);

    let src_fd = bs.open_file(rel_path)?;
    // Safety: bs.open_file returns an owned fd. File::from_raw_fd takes
    // ownership, so Drop closes it on all return paths — no libc::close needed.
    let mut src = unsafe { File::from_raw_fd(src_fd) };

    let src_meta = src.metadata()?;
    let file_size_bytes = src_meta.len();
    tracing::info!(
        "copy starting: {} ({:.1} MB)",
        rel_path.display(),
        file_size_bytes as f64 / 1_048_576.0,
    );

    let started = std::time::Instant::now();

    let mut dst = match OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&partial)
    {
        Ok(f) => f,
        Err(e) => return Err(e),
    };

    let copy_result: std::io::Result<()> = (|| {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 { break; }
            dst.write_all(&buf[..n])?;
            bytes_copied.fetch_add(n as u64, Ordering::Relaxed);
        }
        Ok(())
    })();

    if let Err(e) = copy_result {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }

    if let Err(e) = dst.sync_all() {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    drop(dst);

    apply_source_metadata(&partial, &src_meta);

    if let Err(e) = std::fs::rename(&partial, cache_dest) {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }

    tracing::info!(
        "copy complete: {} ({:.1} MB in {:.1}s)",
        rel_path.display(),
        file_size_bytes as f64 / 1_048_576.0,
        started.elapsed().as_secs_f64(),
    );
    Ok(())
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".partial");
    PathBuf::from(s)
}

/// Best-effort: all syscalls are fire-and-forget.
fn apply_source_metadata(path: &Path, meta: &std::fs::Metadata) {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return };
    unsafe {
        libc::chmod(c.as_ptr(), (meta.mode() & 0o7777) as libc::mode_t);
        libc::lchown(c.as_ptr(), meta.uid(), meta.gid());
        let times = [
            libc::timespec {
                tv_sec:  meta.atime()      as libc::time_t,
                tv_nsec: meta.atime_nsec() as libc::c_long,
            },
            libc::timespec {
                tv_sec:  meta.mtime()      as libc::time_t,
                tv_nsec: meta.mtime_nsec() as libc::c_long,
            },
        ];
        libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0);
    }
}

/// Thin wrapper around `perform_copy` for integration tests.
/// Production copies go through `CacheIO::submit_cache` / `copy_worker`.
#[doc(hidden)]
#[allow(dead_code)]
pub fn copy_for_tests(
    bs: &BackingStore,
    rel_path: &Path,
    cache_dest: &Path,
) -> std::io::Result<()> {
    let dummy = AtomicU64::new(0);
    perform_copy(bs, rel_path, cache_dest, &dummy)
}
