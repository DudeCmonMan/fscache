use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{Mutex, Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::backing_store::BackingStore;
use crate::cache::db::SourceMetadata;
use crate::cache::manager::{CacheManager, StaleResult};
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

/// Coordination handle for a single in-flight copy. Shared between the copy worker and the
/// backing watcher. The watcher sets `abort` when a backing change is detected mid-copy;
/// the worker checks the flag before committing the rename.
pub struct CopyControl {
    pub abort: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheJobKind {
    Populate,
    Validate,
    Replace,
}

#[derive(Debug, Clone)]
struct CacheJob {
    rel_path: PathBuf,
    kind: CacheJobKind,
    enqueue_ts: u64,
}

/// Single source of truth for all pending and in-flight cache work.
///
/// The `queue` is a FIFO of paths waiting for the caching window to open.
/// `known` tracks every path that CacheIO has seen — queued or in flight —
/// to prevent duplicate submissions. A path is removed from `known` only when
/// its copy worker finishes (success or failure), so dedup is airtight across
/// the queue-to-in-flight transition.
struct PipelineState {
    /// FIFO of cache jobs.
    queue: VecDeque<CacheJob>,
    /// All jobs currently queued OR in flight. Dedupe includes job kind so validation does not suppress replacement.
    /// Value is the abort-control handle for in-flight entries (None while still queued).
    known: HashMap<(PathBuf, CacheJobKind), Option<Arc<CopyControl>>>,
}

impl PipelineState {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            known: HashMap::new(),
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
    cache: Arc<CacheManager>,
    backing_store: Arc<BackingStore>,
    scheduler: Scheduler,
    state: Arc<Mutex<PipelineState>>,
    notify: Arc<Notify>,
    semaphore: Arc<Semaphore>,
    evict_lock: Arc<Mutex<()>>,
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
            let key = (path.clone(), CacheJobKind::Populate);
            if let std::collections::hash_map::Entry::Vacant(entry) = initial.known.entry(key) {
                entry.insert(None);
                initial.queue.push_back(CacheJob {
                    rel_path: path,
                    kind: CacheJobKind::Populate,
                    enqueue_ts: ts,
                });
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
    pub async fn submit_cache(&self, rel_path: PathBuf) {
        self.submit_job(rel_path, CacheJobKind::Populate).await;
    }

    pub async fn submit_validation(&self, rel_path: PathBuf) {
        self.submit_job(rel_path, CacheJobKind::Validate).await;
    }

    pub async fn submit_replace(&self, rel_path: PathBuf) {
        self.submit_job(rel_path, CacheJobKind::Replace).await;
    }

    pub async fn submit_maintenance_validation(&self) {
        for (rel, _) in self
            .cache
            .cache_db()
            .all_fingerprints(self.cache.mount_id())
        {
            self.submit_validation(rel).await;
        }
    }

    async fn submit_job(&self, rel_path: PathBuf, kind: CacheJobKind) {
        // Cheap check before taking the lock — avoids contention on cache hits.
        if kind == CacheJobKind::Populate && self.cache.is_cached(&rel_path) {
            tracing::debug!("cache_io: {} already cached, skipping", rel_path.display());
            return;
        }

        let queue_len = {
            let mut st = self.state.lock().await;
            let key = (rel_path.clone(), kind);
            if st.known.contains_key(&key) {
                tracing::debug!(
                    "cache_io: {:?} {} already queued or in flight, skipping",
                    kind,
                    rel_path.display()
                );
                return;
            }
            st.known.insert(key, None);
            let now = now_secs();
            st.queue.push_back(CacheJob {
                rel_path: rel_path.clone(),
                kind,
                enqueue_ts: now,
            });
            let len = st.queue.len() as u64;

            // Only populate jobs are persisted across restart; validation/replacement
            // are cheap coherence work and should not rehydrate as populate jobs.
            if kind == CacheJobKind::Populate {
                self.cache
                    .cache_db()
                    .save_deferred(&rel_path, &rel_path, now);
            }
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

    /// Signal in-flight copy jobs for `rel` to abort before committing. No-op if not in flight.
    pub async fn mark_abort(&self, rel: &Path) {
        let state = self.state.lock().await;
        for kind in [CacheJobKind::Populate, CacheJobKind::Replace] {
            if let Some(Some(ctrl)) = state.known.get(&(rel.to_path_buf(), kind)) {
                ctrl.abort.store(true, Ordering::Release);
            }
        }
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
            while let Some(job) = st.queue.front() {
                if job.enqueue_ts >= cutoff {
                    break;
                }
                let job = st.queue.pop_front().unwrap();
                st.known.remove(&(job.rel_path.clone(), job.kind));
                if job.kind == CacheJobKind::Populate {
                    expired.push(job.rel_path);
                }
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

/// The dispatcher runs validation whenever work arrives, but only starts copy work
/// while the caching window is open.
async fn dispatcher(io: CacheIO, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            _ = io.notify.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(10)) => {}
            _ = shutdown.cancelled() => break,
        }

        // Always run TTL sweep, even when the window is closed.
        io.expire_stale().await;

        loop {
            if shutdown.is_cancelled() {
                break;
            }

            let allowed = io.scheduler.is_caching_allowed();
            tracing::debug!(
                event = telemetry::EVENT_CACHING_WINDOW,
                allowed,
                "cache_io: window check",
            );

            let next = {
                let mut st = io.state.lock().await;
                pop_next_job(&mut st.queue, allowed)
            };
            let Some(job) = next else { break };

            let io2 = io.clone();
            if job.kind == CacheJobKind::Validate {
                tokio::spawn(async move {
                    job_worker(io2, job).await;
                });
                continue;
            }

            let permit = io
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("semaphore closed");

            tokio::spawn(async move {
                job_worker(io2, job).await;
                drop(permit);
            });
        }
    }
}

fn pop_next_job(queue: &mut VecDeque<CacheJob>, copy_allowed: bool) -> Option<CacheJob> {
    if copy_allowed {
        return queue.pop_front();
    }
    let idx = queue
        .iter()
        .position(|job| job.kind == CacheJobKind::Validate)?;
    queue.remove(idx)
}

async fn populate_worker(io: CacheIO, rel_path: PathBuf, kind: CacheJobKind) {
    if kind == CacheJobKind::Populate && io.cache.is_cached(&rel_path) {
        tracing::debug!("cache_io: {} already cached, skipping", rel_path.display());
        finish_known(&io, &rel_path, kind).await;
        return;
    }

    // Promote this entry from queued (None) to in-flight (Some(CopyControl)).
    let control = Arc::new(CopyControl {
        abort: AtomicBool::new(false),
    });
    io.state
        .lock()
        .await
        .known
        .insert((rel_path.clone(), kind), Some(Arc::clone(&control)));

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
        finish_known(&io, &rel_path, kind).await;
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
        let ticker_path = rel_path.clone();
        let ticker_size = size_bytes;
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
    let ctrl = control.clone();
    let result =
        tokio::task::spawn_blocking(move || perform_copy(&bs, &rel, &dest, &progress, Some(ctrl)))
            .await;

    // Stop the ticker before emitting COPY_COMPLETE / COPY_FAILED so no
    // late progress event races past the completion signal.
    let _ = done_tx.send(());

    match result {
        Ok(Ok(source_meta)) => {
            io.cache.mark_cached(&rel_path, source_meta);
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

    finish_known(&io, &rel_path, kind).await;
}

async fn finish_known(io: &CacheIO, rel_path: &Path, kind: CacheJobKind) {
    io.state
        .lock()
        .await
        .known
        .remove(&(rel_path.to_path_buf(), kind));
    if kind == CacheJobKind::Populate {
        io.cache.cache_db().remove_deferred(rel_path);
    }
}

async fn job_worker(io: CacheIO, job: CacheJob) {
    match job.kind {
        CacheJobKind::Populate => populate_worker(io, job.rel_path, job.kind).await,
        CacheJobKind::Replace => populate_worker(io, job.rel_path, job.kind).await,
        CacheJobKind::Validate => validate_worker(io, job.rel_path, job.kind).await,
    }
}

async fn validate_worker(io: CacheIO, rel_path: PathBuf, kind: CacheJobKind) {
    match io.cache.is_stale(&rel_path) {
        StaleResult::Fresh | StaleResult::NotTracked => {}
        StaleResult::NeedsBackfill(st) => io.cache.backfill_fingerprint(&rel_path, &st),
        StaleResult::BackingGone => io
            .cache
            .drop_stale(&rel_path, telemetry::EVICTION_REASON_STALE_PERIODIC),
        StaleResult::Stale => io.submit_replace(rel_path.clone()).await,
    }
    finish_known(&io, &rel_path, kind).await;
}

async fn eviction_worker(
    cache: Arc<CacheManager>,
    interval_secs: u64,
    shutdown: CancellationToken,
) {
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
    control: Option<Arc<CopyControl>>,
) -> std::io::Result<SourceMetadata> {
    if let Some(parent) = cache_dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let partial = partial_path(cache_dest);
    let result = perform_copy_inner(bs, rel_path, cache_dest, &partial, bytes_copied, control);
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}

fn perform_copy_inner(
    bs: &BackingStore,
    rel_path: &Path,
    cache_dest: &Path,
    partial: &Path,
    bytes_copied: &AtomicU64,
    control: Option<Arc<CopyControl>>,
) -> std::io::Result<SourceMetadata> {
    let src_fd = bs.open_file(rel_path)?;
    // Safety: bs.open_file returns an owned fd. File::from_raw_fd takes
    // ownership, so Drop closes it on all return paths — no libc::close needed.
    let mut src = unsafe { File::from_raw_fd(src_fd) };

    let src_meta = src.metadata()?;
    let source_snapshot = SourceMetadata::from_metadata(&src_meta);
    let file_size_bytes = src_meta.len();
    let initial_mtime = src_meta.mtime();
    let initial_size = src_meta.len() as i64;
    tracing::info!(
        "copy starting: {} ({:.1} MB)",
        rel_path.display(),
        file_size_bytes as f64 / 1_048_576.0,
    );

    let started = std::time::Instant::now();

    let mut dst = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&partial)?;

    let copy_result: std::io::Result<()> = (|| {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
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

    apply_source_metadata(&partial, &src_meta)?;

    // Before committing, verify the backing hasn't changed since the copy started.
    // Catches rename-replace and signals from the backing watcher (abort flag).
    let aborted = control
        .as_ref()
        .is_some_and(|c| c.abort.load(Ordering::Acquire));
    let initial_mtime_nsec = src_meta.mtime_nsec();
    let stale = bs.stat(rel_path).is_some_and(|s| {
        s.st_size != initial_size
            || s.st_mtime != initial_mtime
            || s.st_mtime_nsec != initial_mtime_nsec
    });
    if aborted || stale {
        let _ = std::fs::remove_file(&partial);
        return Err(std::io::Error::other("backing changed during copy"));
    }

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
    Ok(source_snapshot)
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".partial");
    PathBuf::from(s)
}

fn apply_source_metadata(path: &Path, meta: &std::fs::Metadata) -> std::io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    unsafe {
        if libc::chmod(c.as_ptr(), (meta.mode() & 0o7777) as libc::mode_t) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::lchown(c.as_ptr(), meta.uid(), meta.gid()) != 0 {
            tracing::debug!(
                "cache_io: lchown({}) failed: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        let times = [
            libc::timespec {
                tv_sec: meta.atime() as libc::time_t,
                tv_nsec: meta.atime_nsec() as libc::c_long,
            },
            libc::timespec {
                tv_sec: meta.mtime() as libc::time_t,
                tv_nsec: meta.mtime_nsec() as libc::c_long,
            },
        ];
        if libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Thin wrapper around `perform_copy` for integration tests.
/// Production copies go through `CacheIO::submit_cache` / `copy_worker`.
#[doc(hidden)]
#[allow(dead_code)]
pub fn copy_for_tests(
    bs: &BackingStore,
    rel_path: &Path,
    cache_dest: &Path,
) -> std::io::Result<SourceMetadata> {
    let dummy = AtomicU64::new(0);
    perform_copy(bs, rel_path, cache_dest, &dummy, None)
}
