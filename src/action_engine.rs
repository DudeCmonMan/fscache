use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::backing_store::BackingStore;
use crate::cache::CacheManager;
use crate::db::CacheDb;
use crate::prediction_utils::parse_season_episode;
use crate::preset::{CacheAction, CachePreset, RuleContext};
use crate::scheduler::Scheduler;
use crate::telemetry;

/// Which preset handler this event should invoke.
pub enum EventKind {
    Miss,
    Hit,
    Close { bytes_read: u64 },
}

pub struct AccessEvent {
    pub relative_path: PathBuf,
    /// Unix timestamp (seconds) when this event was first buffered outside the caching window.
    /// 0 means the event is fresh from FUSE and has not been buffered yet.
    pub buffered_at: u64,
    pub kind: EventKind,
}

impl AccessEvent {
    pub fn miss(relative_path: PathBuf) -> Self {
        Self { relative_path, buffered_at: 0, kind: EventKind::Miss }
    }
    pub fn hit(relative_path: PathBuf) -> Self {
        Self { relative_path, buffered_at: 0, kind: EventKind::Hit }
    }
    pub fn close(relative_path: PathBuf, bytes_read: u64) -> Self {
        Self { relative_path, buffered_at: 0, kind: EventKind::Close { bytes_read } }
    }
}

pub struct CopyRequest {
    pub rel_path: PathBuf,
    pub cache_dest: PathBuf,
    /// Sender to notify the action engine when this copy completes or fails.
    pub done_tx: mpsc::UnboundedSender<PathBuf>,
}

pub struct ActionEngine {
    rx: mpsc::UnboundedReceiver<AccessEvent>,
    copy_tx: mpsc::Sender<CopyRequest>,
    cache: Arc<CacheManager>,
    preset: Option<Arc<dyn CachePreset>>,
    scheduler: Scheduler,
    backing_store: Arc<BackingStore>,
    max_cache_pull_bytes: u64,
    deferred_ttl_minutes: u64,
    min_access_secs: u64,
    min_file_size_mb: u64,
}

/// Pending access entry: tracks overlapping FUSE handles for the same path so that
/// short-lived handles (e.g. PMS relay-read pattern for direct play) don't cancel
/// before min_access_secs elapses.
struct PendingEntry {
    event: AccessEvent,
    inserted_at: Instant,
    /// Number of FUSE handles currently open for this path.
    open_handles: u32,
    /// Bytes read across all handles (accumulated on each close).
    total_bytes_read: u64,
}

impl ActionEngine {
    pub fn new(
        rx: mpsc::UnboundedReceiver<AccessEvent>,
        copy_tx: mpsc::Sender<CopyRequest>,
        cache: Arc<CacheManager>,
        preset: Option<Arc<dyn CachePreset>>,
        scheduler: Scheduler,
        backing_store: Arc<BackingStore>,
        max_cache_pull_bytes: u64,
        deferred_ttl_minutes: u64,
        min_access_secs: u64,
        min_file_size_mb: u64,
    ) -> Self {
        Self {
            rx, copy_tx, cache, preset, scheduler, backing_store,
            max_cache_pull_bytes, deferred_ttl_minutes, min_access_secs, min_file_size_mb,
        }
    }

    pub async fn run(self) {
        let ActionEngine {
            mut rx, copy_tx, cache, preset, scheduler,
            backing_store, max_cache_pull_bytes, deferred_ttl_minutes,
            min_access_secs, min_file_size_mb,
        } = self;

        let db = cache.cache_db();
        let mut in_flight: HashSet<PathBuf> = HashSet::new();
        let mut deferred = load_deferred_from_db(&db, deferred_ttl_minutes, cache.cache_dir());
        if !deferred.is_empty() {
            tracing::info!("action_engine: loaded {} deferred event(s) from DB", deferred.len());
        }
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.tick().await; // consume the immediate first tick

        // Access filter: pending buffer for min_access_secs gate.
        let mut pending: HashMap<PathBuf, PendingEntry> = HashMap::new();
        let mut access_check_tick = tokio::time::interval(Duration::from_secs(5));
        access_check_tick.tick().await; // consume the immediate first tick

        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<PathBuf>();

        loop {
            let mut to_process: Vec<AccessEvent> = Vec::new();

            tokio::select! {
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
                                // Close: always check pending regardless of caching window.
                                if let Some(entry) = pending.get_mut(&event.relative_path) {
                                    entry.open_handles = entry.open_handles.saturating_sub(1);
                                    entry.total_bytes_read += bytes_read;
                                    tracing::debug!(
                                        "access_filter: close for {}, open_handles={}, total_bytes_read={}",
                                        event.relative_path.display(), entry.open_handles, entry.total_bytes_read
                                    );
                                    if entry.open_handles == 0 {
                                        // Last handle closed — evaluate now.
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
                                    // else: handles still open — let timer decide, skip on_close.
                                } else {
                                    // Not in pending — forward Close normally if allowed.
                                    let allowed = scheduler.is_caching_allowed();
                                    tracing::debug!(event = telemetry::EVENT_CACHING_WINDOW, allowed, "caching window check");
                                    if allowed {
                                        to_process.push(event);
                                    }
                                }
                            } else {
                                // Size floor: skip files below the configured minimum.
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
                                    let allowed = scheduler.is_caching_allowed();
                                    tracing::debug!(event = telemetry::EVENT_CACHING_WINDOW, allowed, "caching window check");
                                    if allowed {
                                        if !deferred.is_empty() {
                                            tracing::info!(
                                                "action_engine: caching window open, flushing {} deferred event(s)",
                                                deferred.len()
                                            );
                                            to_process.extend(deferred.drain().map(|(_, ev)| ev));
                                            db.clear_deferred();
                                            tracing::debug!(event = telemetry::EVENT_DEFERRED_CHANGED, count = 0u64, "deferred cleared");
                                        }
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
                                    } else if matches!(event.kind, EventKind::Miss) {
                                        // Only Miss events are worth deferring — hits don't replay.
                                        let key = deferred_key(&event.relative_path);
                                        buffer_event(&mut deferred, event);
                                        if let Some(ev) = deferred.get(&key) {
                                            db.save_deferred(&key, &ev.relative_path, ev.buffered_at);
                                        }
                                        let count = deferred.len() as u64;
                                        tracing::debug!(event = telemetry::EVENT_DEFERRED_CHANGED, count, "action_engine: outside caching window, {} show(s) buffered", count);
                                    }
                                }
                            }
                        }
                        None => break,
                    }
                }
                _ = tick.tick() => {
                    let allowed = scheduler.is_caching_allowed();
                    tracing::debug!(event = telemetry::EVENT_CACHING_WINDOW, allowed, "caching window tick");
                    if allowed && !deferred.is_empty() {
                        tracing::info!(
                            "action_engine: caching window opened, flushing {} deferred event(s)",
                            deferred.len()
                        );
                        to_process.extend(deferred.drain().map(|(_, ev)| ev));
                        db.clear_deferred();
                        tracing::debug!(event = telemetry::EVENT_DEFERRED_CHANGED, count = 0u64, "deferred cleared");
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
                Some(completed) = done_rx.recv() => {
                    in_flight.remove(&completed);
                    tracing::debug!("action_engine: in_flight cleanup for {}", completed.display());
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
                    if in_flight.contains(&rel) {
                        tracing::debug!("action_engine: {} already in-flight, skipping", rel.display());
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

                    let cache_dest = cache.cache_path(&rel);
                    tracing::info!(event = telemetry::EVENT_COPY_QUEUED, path = %rel.display(), "action_engine: queuing {} for caching", rel.display());
                    in_flight.insert(rel.clone());
                    let _ = copy_tx.send(CopyRequest {
                        rel_path: rel,
                        cache_dest,
                        done_tx: done_tx.clone(),
                    }).await;
                }
            }
        }
    }
}

pub async fn run_copier_task(
    backing_store: Arc<BackingStore>,
    mut rx: mpsc::Receiver<CopyRequest>,
    cache: Arc<CacheManager>,
) {
    while let Some(req) = rx.recv().await {
        if cache.is_cached(&req.rel_path) {
            tracing::debug!("copier: {} already cached, skipping", req.rel_path.display());
            let _ = req.done_tx.send(req.rel_path);
            continue;
        }

        // Evict first so that the subsequent free-space check reflects reclaimed space.
        cache.evict_if_needed();

        if !cache.has_free_space() {
            tracing::warn!(
                "copier: insufficient free space after eviction, skipping {}",
                req.rel_path.display()
            );
            let _ = req.done_tx.send(req.rel_path);
            continue;
        }

        let rel = req.rel_path.clone();
        let dest = req.cache_dest.clone();
        let done_tx = req.done_tx.clone();
        let size_bytes = backing_store.file_size(&rel).unwrap_or(0);
        tracing::info!(event = telemetry::EVENT_COPY_STARTED, path = %rel.display(), size_bytes, "copier: caching {}", rel.display());

        let bs = Arc::clone(&backing_store);
        let result =
            tokio::task::spawn_blocking(move || crate::copier::copy_to_cache(&bs, &rel, &dest))
                .await;

        match result {
            Ok(Ok(())) => {
                let size = std::fs::metadata(&req.cache_dest).map(|m| m.len()).unwrap_or(0);
                cache.mark_cached(&req.rel_path, size);
                tracing::info!(event = telemetry::EVENT_COPY_COMPLETE, path = %req.rel_path.display(), "copier: cached {}", req.rel_path.display());
            }
            Ok(Err(e)) => {
                tracing::warn!(event = telemetry::EVENT_COPY_FAILED, path = %req.rel_path.display(), "copier: copy failed {}: {e}", req.rel_path.display());
            }
            Err(e) => {
                tracing::warn!(event = telemetry::EVENT_COPY_FAILED, path = %req.rel_path.display(), "copier: task panicked {}: {e}", req.rel_path.display());
            }
        }
        let _ = done_tx.send(req.rel_path);
    }
}

// ---- deferred event helpers ----

pub fn show_root(rel_path: &Path) -> PathBuf {
    crate::prediction_utils::show_root(rel_path)
}

/// Compute the deduplication key for a deferred event:
/// episode files are keyed by show directory, everything else by full path.
fn deferred_key(path: &Path) -> PathBuf {
    let file_name = path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if parse_season_episode(&file_name).is_some() {
        show_root(path)
    } else {
        path.to_path_buf()
    }
}

/// Insert an event into the deferred buffer, keeping only the most advanced
/// (highest season/episode) event per show directory.
/// The earliest `buffered_at` timestamp across all events for the same show is preserved
/// so TTL expiry works correctly even as newer episodes replace older ones.
pub fn buffer_event(deferred: &mut HashMap<PathBuf, AccessEvent>, mut event: AccessEvent) {
    // Stamp with current time on first buffer; preserve timestamp on reload from disk.
    if event.buffered_at == 0 {
        event.buffered_at = now_secs();
    }

    let file_name = event.relative_path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let new_pos = parse_season_episode(&file_name);
    let key = deferred_key(&event.relative_path);

    if let Some(existing) = deferred.get(&key) {
        let existing_name = existing.relative_path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let old_pos = parse_season_episode(&existing_name);
        if new_pos >= old_pos {
            // Advance to the later episode but keep the earliest buffering timestamp.
            event.buffered_at = event.buffered_at.min(existing.buffered_at);
            deferred.insert(key, event);
        }
    } else {
        deferred.insert(key, event);
    }
}

/// Load deferred events from the SQLite DB. If a legacy JSON file exists from a
/// prior version, it is discarded and removed.
fn load_deferred_from_db(db: &CacheDb, ttl_minutes: u64, cache_dir: &Path) -> HashMap<PathBuf, AccessEvent> {
    // Clean up legacy JSON file if present.
    let json_path = cache_dir.join("deferred_events.json");
    if json_path.exists() {
        tracing::info!("action_engine: removing legacy deferred_events.json (now stored in SQLite)");
        let _ = std::fs::remove_file(&json_path);
    }

    db.load_deferred(ttl_minutes)
        .into_iter()
        .map(|(key, path, ts)| (key, AccessEvent { relative_path: path, buffered_at: ts, kind: EventKind::Miss }))
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
