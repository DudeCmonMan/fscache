use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

use crate::backing_store::BackingStore;
use crate::cache::io::CacheIO;
use crate::cache::manager::CacheManager;
use crate::preset::{CacheAction, CachePreset, RuleContext};
use crate::telemetry;

/// Which preset handler this event should invoke.
pub enum EventKind {
    Miss,
    Hit,
    Close { bytes_read: u64 },
}

pub struct AccessEvent {
    pub relative_path: PathBuf,
    pub kind: EventKind,
}

impl AccessEvent {
    pub fn miss(relative_path: PathBuf) -> Self {
        Self { relative_path, kind: EventKind::Miss }
    }
    pub fn hit(relative_path: PathBuf) -> Self {
        Self { relative_path, kind: EventKind::Hit }
    }
    pub fn close(relative_path: PathBuf, bytes_read: u64) -> Self {
        Self { relative_path, kind: EventKind::Close { bytes_read } }
    }
}

pub struct ActionEngine {
    rx: mpsc::UnboundedReceiver<AccessEvent>,
    cache_io: CacheIO,
    cache: Arc<CacheManager>,
    preset: Option<Arc<dyn CachePreset>>,
    backing_store: Arc<BackingStore>,
    max_cache_pull_bytes: u64,
    min_access_secs: u64,
    min_file_size_mb: u64,
}

/// Pending access entry: tracks overlapping FUSE handles for the same path so that
/// short-lived handles (e.g. PMS relay-read pattern for direct play) don't cancel
/// before min_access_secs elapses.
struct PendingEntry {
    event: AccessEvent,
    inserted_at: Instant,
    open_handles: u32,
    /// Bytes read across all handles (accumulated on each close).
    total_bytes_read: u64,
}

impl ActionEngine {
    pub fn new(
        rx: mpsc::UnboundedReceiver<AccessEvent>,
        cache_io: CacheIO,
        cache: Arc<CacheManager>,
        preset: Option<Arc<dyn CachePreset>>,
        backing_store: Arc<BackingStore>,
        max_cache_pull_bytes: u64,
        min_access_secs: u64,
        min_file_size_mb: u64,
    ) -> Self {
        Self {
            rx, cache_io, cache, preset, backing_store,
            max_cache_pull_bytes, min_access_secs, min_file_size_mb,
        }
    }

    pub async fn run(self, shutdown: CancellationToken) {
        let ActionEngine {
            mut rx, cache_io, cache, preset,
            backing_store, max_cache_pull_bytes,
            min_access_secs, min_file_size_mb,
        } = self;

        // Access filter: pending buffer for min_access_secs gate.
        let mut pending: HashMap<PathBuf, PendingEntry> = HashMap::new();
        let mut access_check_tick = tokio::time::interval(Duration::from_secs(5));
        access_check_tick.tick().await; // consume the immediate first tick

        loop {
            let mut to_process: Vec<AccessEvent> = Vec::new();

            tokio::select! {
                _ = shutdown.cancelled() => break,

                result = rx.recv() => {
                    match result {
                        Some(event) => {
                            tracing::debug!("action_engine: received access event for {:?}", event.relative_path);

                            // Extract bytes_read without consuming event.kind (avoids partial move).
                            let close_bytes: Option<u64> = match &event.kind {
                                EventKind::Close { bytes_read } => Some(*bytes_read),
                                _ => None,
                            };

                            if let Some(bytes_read) = close_bytes {
                                // Close: check pending regardless of caching window.
                                if let Some(entry) = pending.get_mut(&event.relative_path) {
                                    entry.open_handles = entry.open_handles.saturating_sub(1);
                                    entry.total_bytes_read += bytes_read;
                                    tracing::debug!(
                                        "access_filter: close for {}, open_handles={}, total_bytes_read={}",
                                        event.relative_path.display(), entry.open_handles, entry.total_bytes_read
                                    );
                                    if entry.open_handles == 0 {
                                        let entry = pending.remove(&event.relative_path).unwrap();
                                        let file_size = backing_store.file_size(&event.relative_path).unwrap_or(0);
                                        if file_size > 0 && entry.total_bytes_read >= file_size {
                                            tracing::info!("access_filter: promoting {} (fully consumed, {} bytes across all handles)",
                                                event.relative_path.display(), entry.total_bytes_read);
                                            to_process.push(entry.event);
                                        } else {
                                            tracing::info!("access_filter: cancelled {} (closed before {}s threshold)",
                                                event.relative_path.display(), min_access_secs);
                                        }
                                    }
                                } else {
                                    to_process.push(event);
                                }
                            } else {
                                let passes_size_floor = if min_file_size_mb > 0 {
                                    let size = backing_store.file_size(&event.relative_path).unwrap_or(0);
                                    if size < min_file_size_mb * 1_048_576 {
                                        tracing::debug!("access_filter: skipped {} — below {} MB size floor",
                                            event.relative_path.display(), min_file_size_mb);
                                        false
                                    } else {
                                        true
                                    }
                                } else {
                                    true
                                };

                                if passes_size_floor {
                                    if min_access_secs > 0 {
                                        match pending.get_mut(&event.relative_path) {
                                            Some(entry) => {
                                                entry.open_handles += 1;
                                                tracing::debug!(
                                                    "access_filter: open handle for {} (open_handles={})",
                                                    event.relative_path.display(), entry.open_handles
                                                );
                                            }
                                            None => {
                                                pending.insert(event.relative_path.clone(), PendingEntry {
                                                    event,
                                                    inserted_at: Instant::now(),
                                                    open_handles: 1,
                                                    total_bytes_read: 0,
                                                });
                                            }
                                        }
                                    } else {
                                        to_process.push(event);
                                    }
                                }
                            }
                        }
                        // Test-only exit path: production exits via shutdown.cancelled()
                        // because access_tx is held by the FUSE session until drop(mounts).
                        None => break,
                    }
                }
                _ = access_check_tick.tick(), if min_access_secs > 0 => {
                    let cutoff = Instant::now() - Duration::from_secs(min_access_secs);
                    let ready: Vec<PathBuf> = pending.iter()
                        .filter(|(_, entry)| entry.inserted_at <= cutoff)
                        .map(|(k, _)| k.clone())
                        .collect();
                    for key in ready {
                        if let Some(entry) = pending.remove(&key) {
                            tracing::info!(
                                "access_filter: promoting {} ({}s threshold reached, {} handle(s) open)",
                                entry.event.relative_path.display(), min_access_secs, entry.open_handles
                            );
                            to_process.push(entry.event);
                        }
                    }
                }
            }

            for event in to_process {
                let next: Vec<PathBuf> = if let Some(ref preset) = preset {
                    let cache_db = cache.cache_db();
                    let ctx = RuleContext {
                        backing_store: &backing_store,
                        cache_db: &cache_db,
                    };
                    let actions = match event.kind {
                        EventKind::Miss => preset.on_miss(&event.relative_path, &ctx),
                        EventKind::Hit  => preset.on_hit(&event.relative_path, &ctx),
                        EventKind::Close { bytes_read } => preset.on_close(&event.relative_path, bytes_read, &ctx),
                    };
                    actions
                        .into_iter()
                        .filter_map(|action| match action {
                            CacheAction::Cache(paths) => Some(paths),
                            CacheAction::Evict(_) => None, // not handled in action loop yet
                        })
                        .flatten()
                        .collect()
                } else {
                    vec![]
                };

                if next.is_empty() {
                    tracing::debug!("action_engine: no upcoming episodes found for {:?}", event.relative_path);
                }

                let budget_active = max_cache_pull_bytes > 0;
                let mut running_total = if budget_active { cache.total_cached_bytes() } else { 0 };
                let mut first_candidate = true;

                for rel in next {
                    if cache.is_cached(&rel) {
                        tracing::debug!("action_engine: {} already cached, skipping", rel.display());
                        continue;
                    }
                    if budget_active {
                        let file_size = backing_store.file_size(&rel).unwrap_or(0);
                        if !first_candidate && running_total + file_size > max_cache_pull_bytes {
                            tracing::info!(
                                event = telemetry::EVENT_BUDGET_UPDATED,
                                used_bytes = running_total,
                                max_bytes = max_cache_pull_bytes,
                                "action_engine: budget exhausted ({:.1} GB used of {:.1} GB), stopping before {}",
                                running_total as f64 / 1_073_741_824.0,
                                max_cache_pull_bytes as f64 / 1_073_741_824.0,
                                rel.display(),
                            );
                            break;
                        }
                        running_total += file_size;
                    }
                    first_candidate = false;

                    cache_io.submit_cache(rel).await;
                }
            }
        }
    }
}

/// Periodic maintenance task: runs a stale-file sweep on a fixed interval.
/// Eviction is now handled independently by the CacheIO eviction worker.
///
/// Spawned once per mount when `poll_interval_secs > 0` and
/// `invalidation.check_on_maintenance` is true.
pub async fn run_maintenance_task(
    cache: Arc<CacheManager>,
    poll_interval_secs: u64,
    shutdown: CancellationToken,
) {
    if poll_interval_secs == 0 {
        return;
    }
    let interval = Duration::from_secs(poll_interval_secs);
    loop {
        tokio::select! {
            _ = tokio::time::sleep(interval) => {
                let cache_clone = Arc::clone(&cache);
                tokio::task::spawn_blocking(move || {
                    let (checked, dropped) = cache_clone.sweep_stale();
                    tracing::info!(
                        "maintenance stale sweep: checked={checked} dropped={dropped}"
                    );
                })
                .await
                .ok();
            }
            _ = shutdown.cancelled() => break,
        }
    }
}
