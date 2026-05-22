use std::collections::HashMap;
use std::ffi::{CString, OsString};
use std::num::NonZeroUsize;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};
use lru::LruCache;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::backing_store::BackingStore;
use crate::cache::io::CacheIO;
use crate::cache::manager::CacheManager;
use crate::config::BackingWatchConfig;
use crate::telemetry;

pub enum WatchRequest {
    Touch { rel_path: PathBuf, ino: u64 },
}

/// Lightweight handle sent to `FsCache`. Cheap to clone; sends requests to the watcher task.
#[derive(Clone)]
pub struct BackingWatchHandle {
    tx: mpsc::UnboundedSender<WatchRequest>,
}

impl BackingWatchHandle {
    pub fn new(tx: mpsc::UnboundedSender<WatchRequest>) -> Self {
        Self { tx }
    }

    /// Called from `Filesystem::lookup` for every directory lookup. Idempotent.
    pub fn touch(&self, rel_path: PathBuf, ino: u64) {
        let _ = self.tx.send(WatchRequest::Touch { rel_path, ino });
    }
}

/// Severity ordering for debounce coalescing. Higher severity wins when multiple events
/// arrive within the same debounce window.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    CreateOrMovedIn = 0,
    Attr = 1,
    Modify = 2,
    Delete = 3,
}

struct PendingEvent {
    deadline: Instant,
    severity: Severity,
    parent_ino: u64,
    parent_rel: PathBuf,
}

struct WdEntry {
    rel_path: PathBuf,
    ino: u64,
}

/// Main watcher loop. Spawned once per FUSE mount after `spawn_mount2` returns.
///
/// Watches backing directories for external changes and fans each coalesced event to:
/// - `Notifier::inval_entry` (kernel fsnotify — upstream inotify consumers see changes)
/// - `CacheManager::drop_stale` (cache coherence — stale entries evicted immediately)
/// - `tracing::info!(EVENT_BACKING_CHANGED, ...)` (TUI/IPC telemetry)
///
/// ## Watch seeding
///
/// The root backing directory is watched immediately on startup. Subdirectory watches
/// are seeded lazily: `Filesystem::lookup` sends a `Touch` request whenever it resolves
/// a child with `S_IFDIR` set, which is triggered by any FUSE operation that descends
/// into a directory (`open`, `stat`, `readdir`, etc.) — but only when the kernel
/// performs a `lookup` call first.
///
/// **Coverage gap:** a consumer that calls `readdir` on a path already in the kernel
/// dcache (i.e. no `lookup` round-trip) will not trigger watch seeding for directories
/// it enumerates. Such directories remain unwatched until any FUSE operation causes a
/// fresh `lookup`. In practice this only affects directories the kernel has cached
/// across a fscache restart; they become watched on the next access that misses the dcache.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    mut rx: mpsc::UnboundedReceiver<WatchRequest>,
    notifier: fuser::Notifier,
    backing_store: Arc<BackingStore>,
    cache: Arc<CacheManager>,
    cache_io: CacheIO,
    config: BackingWatchConfig,
    shutdown: CancellationToken,
    mount_id: String,
) {
    let inotify = match Inotify::init() {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("backing_watch[{mount_id}]: inotify init failed: {e}");
            return;
        }
    };

    let buf = vec![0u8; 4096];
    let mut stream = match inotify.into_event_stream(buf) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("backing_watch[{mount_id}]: event stream init failed: {e}");
            return;
        }
    };

    let cap = NonZeroUsize::new(config.max_dirs.max(1)).unwrap();
    let mut lru: LruCache<PathBuf, WatchDescriptor> = LruCache::new(cap);
    let mut wd_map: HashMap<WatchDescriptor, WdEntry> = HashMap::new();
    let mut debounce: HashMap<(u64, OsString), PendingEvent> = HashMap::new();
    let debounce_dur = Duration::from_millis(config.debounce_ms);

    // Periodic tick to drain debounce windows that have reached their deadline.
    let mut debounce_tick = tokio::time::interval(debounce_dur);
    debounce_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    debounce_tick.tick().await; // discard the immediate first tick

    // Seed the root backing directory immediately so files at the root level are covered
    // without requiring a FUSE lookup to happen first. Root is FUSE ino=1.
    touch_or_add(
        &mut stream,
        &mut lru,
        &mut wd_map,
        &backing_store,
        PathBuf::new(),
        1,
        config.max_dirs,
    );

    tracing::info!(
        "backing_watch[{mount_id}]: started (max_dirs={}, debounce_ms={})",
        config.max_dirs,
        config.debounce_ms,
    );

    loop {
        tokio::select! {
            biased;

            _ = shutdown.cancelled() => break,

            _ = debounce_tick.tick() => {
                let now = Instant::now();
                let expired: Vec<_> = debounce
                    .iter()
                    .filter(|(_, p)| p.deadline <= now)
                    .map(|(k, p)| (k.clone(), p.severity, p.parent_ino, p.parent_rel.clone()))
                    .collect();
                for (key, severity, parent_ino, parent_rel) in expired {
                    debounce.remove(&key);
                    if shutdown.is_cancelled() { break; }
                    dispatch(
                        &notifier,
                        &cache,
                        &cache_io,
                        &parent_rel,
                        parent_ino,
                        &key.1,
                        severity,
                        &mount_id,
                    ).await;
                }
            }

            msg = rx.recv() => {
                match msg {
                    Some(WatchRequest::Touch { rel_path, ino }) => {
                        touch_or_add(
                            &mut stream,
                            &mut lru,
                            &mut wd_map,
                            &backing_store,
                            rel_path,
                            ino,
                            config.max_dirs,
                        );
                    }
                    None => break,
                }
            }

            event = stream.next() => {
                match event {
                    Some(Ok(ev)) => handle_event(ev, &wd_map, &mut debounce, debounce_dur),
                    Some(Err(e)) => tracing::debug!("backing_watch[{mount_id}]: inotify error: {e}"),
                    None => break,
                }
            }
        }
    }

    tracing::info!("backing_watch[{mount_id}]: stopped");
}

/// Add or refresh an inotify watch for `rel_path`. On first call, opens the directory
/// via the backing_store O_PATH fd (bypassing the FUSE overmount) and calls
/// `inotify_add_watch` through `/proc/self/fd/<n>`. Subsequent calls for the same path
/// only update the LRU order.
fn touch_or_add(
    stream: &mut inotify::EventStream<Vec<u8>>,
    lru: &mut LruCache<PathBuf, WatchDescriptor>,
    wd_map: &mut HashMap<WatchDescriptor, WdEntry>,
    backing_store: &BackingStore,
    rel_path: PathBuf,
    ino: u64,
    max_dirs: usize,
) {
    if lru.get(&rel_path).is_some() {
        return; // already watched; LRU order updated by get()
    }

    if lru.len() >= max_dirs.max(1)
        && let Some((_, evicted_wd)) = lru.pop_lru()
    {
        wd_map.remove(&evicted_wd);
        let _ = stream.watches().remove(evicted_wd);
    }

    // Build a /proc/self/fd/<n> path that bypasses the FUSE overmount.
    // The backing_store holds an O_PATH fd opened before the FUSE overlay.
    let is_root = rel_path == std::path::Path::new("");
    let borrowed_fd: i32;
    let watch_path: String;

    if is_root {
        borrowed_fd = -1;
        watch_path = format!("/proc/self/fd/{}", backing_store.fd());
    } else {
        let cstr = match CString::new(rel_path.as_os_str().as_bytes().to_vec()) {
            Ok(c) => c,
            Err(_) => return,
        };
        let fd = unsafe {
            libc::openat(
                backing_store.fd(),
                cstr.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return;
        }
        borrowed_fd = fd;
        watch_path = format!("/proc/self/fd/{fd}");
    }

    let mask = WatchMask::CREATE
        | WatchMask::DELETE
        | WatchMask::MODIFY
        | WatchMask::ATTRIB
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO;

    let result = stream
        .watches()
        .add(std::path::Path::new(&watch_path), mask);

    if borrowed_fd >= 0 {
        unsafe { libc::close(borrowed_fd) };
    }

    match result {
        Ok(wd) => {
            lru.put(rel_path.clone(), wd.clone());
            wd_map.insert(wd, WdEntry { rel_path, ino });
        }
        Err(e) => {
            tracing::debug!("backing_watch: add_watch({watch_path}) failed: {e}");
        }
    }
}

/// Record or coalesce a raw inotify event into the debounce map.
fn handle_event(
    event: inotify::Event<OsString>,
    wd_map: &HashMap<WatchDescriptor, WdEntry>,
    debounce: &mut HashMap<(u64, OsString), PendingEvent>,
    debounce_dur: Duration,
) {
    // Events without a name refer to the watched directory itself (e.g. IN_DELETE_SELF).
    // inval_entry requires a child name, so skip these — inval_entry on the parent of this
    // watched dir would require knowledge of its own parent ino which we don't have.
    let Some(name) = event.name else { return };
    let Some(entry) = wd_map.get(&event.wd) else {
        return;
    };

    let severity = if event
        .mask
        .intersects(EventMask::DELETE | EventMask::MOVED_FROM)
    {
        Severity::Delete
    } else if event.mask.intersects(EventMask::MODIFY) {
        Severity::Modify
    } else if event.mask.intersects(EventMask::ATTRIB) {
        Severity::Attr
    } else {
        // CREATE, MOVED_TO, and anything else that adds or replaces a name
        Severity::CreateOrMovedIn
    };

    let key = (entry.ino, name.clone());

    debounce
        .entry(key)
        .and_modify(|p| {
            // Window already open: merge to highest severity; keep original deadline
            // (leading-edge fixed window — not sliding).
            if severity > p.severity {
                p.severity = severity;
            }
        })
        .or_insert(PendingEvent {
            deadline: Instant::now() + debounce_dur,
            severity,
            parent_ino: entry.ino,
            parent_rel: entry.rel_path.clone(),
        });
}

/// Fan out a coalesced event to kernel fsnotify, cache invalidation, and telemetry.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    notifier: &fuser::Notifier,
    cache: &Arc<CacheManager>,
    cache_io: &CacheIO,
    parent_rel: &std::path::Path,
    parent_ino: u64,
    name: &OsString,
    severity: Severity,
    mount_id: &str,
) {
    let child_rel = if parent_rel == std::path::Path::new("") {
        PathBuf::from(name)
    } else {
        parent_rel.join(name)
    };

    let kind_str = match severity {
        Severity::Delete => "delete",
        Severity::Modify => "modify",
        Severity::Attr => "attr",
        Severity::CreateOrMovedIn => "create",
    };

    // ENOENT is swallowed by fuser (entry already evicted). ENODEV after unmount is benign.
    let _ = notifier.inval_entry(fuser::INodeNo(parent_ino), name.as_ref());

    let queued_validation = if cache.is_cached(&child_rel) {
        cache_io.mark_abort(&child_rel).await;
        cache_io.submit_validation(child_rel.clone()).await;
        true
    } else {
        false
    };

    let action_str = if queued_validation {
        "kernel_notified_validation_queued"
    } else {
        "kernel_notified"
    };

    tracing::info!(
        event = telemetry::EVENT_BACKING_CHANGED,
        mount_id = %mount_id,
        path = %child_rel.display(),
        kind = %kind_str,
        action = %action_str,
        "backing_watch[{mount_id}]: {kind_str} {}",
        child_rel.display(),
    );
}
