use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::Duration;

use crate::cache::CacheManager;
use crate::plex_db::PlexDb;
use crate::scheduler::Scheduler;

#[derive(Serialize, Deserialize)]
struct DeferredRecord {
    path: PathBuf,
    timestamp: u64, // unix epoch seconds
}

pub struct AccessEvent {
    pub relative_path: PathBuf,
}

pub struct CopyRequest {
    pub rel_path: PathBuf,
    pub cache_dest: PathBuf,
}

pub struct Predictor {
    rx: mpsc::UnboundedReceiver<AccessEvent>,
    copy_tx: mpsc::Sender<CopyRequest>,
    cache: Arc<CacheManager>,
    lookahead: usize,
    plex_db: Option<PlexDb>,
    scheduler: Scheduler,
    backing_fd: RawFd,
    max_cache_pull_bytes: u64,  // 0 = disabled
    cache_dir: PathBuf,
    deferred_ttl_minutes: u64,
}

impl Predictor {
    pub fn new(
        rx: mpsc::UnboundedReceiver<AccessEvent>,
        copy_tx: mpsc::Sender<CopyRequest>,
        cache: Arc<CacheManager>,
        lookahead: usize,
        plex_db: Option<PlexDb>,
        scheduler: Scheduler,
        backing_fd: RawFd,
        max_cache_pull_bytes: u64,
        cache_dir: PathBuf,
        deferred_ttl_minutes: u64,
    ) -> Self {
        Self {
            rx, copy_tx, cache, lookahead, plex_db, scheduler, backing_fd,
            max_cache_pull_bytes, cache_dir, deferred_ttl_minutes,
        }
    }

    pub async fn run(self) {
        let Predictor {
            mut rx, copy_tx, cache, lookahead, plex_db, scheduler,
            backing_fd, max_cache_pull_bytes, cache_dir, deferred_ttl_minutes,
        } = self;

        let mut in_flight: HashSet<PathBuf> = HashSet::new();
        let mut deferred = load_deferred(&cache_dir, deferred_ttl_minutes);
        if !deferred.is_empty() {
            tracing::info!("predictor: loaded {} deferred event(s) from disk", deferred.len());
        }
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        // Consume the immediate first tick so it doesn't fire instantly on startup.
        tick.tick().await;

        loop {
            let mut to_process: Vec<AccessEvent> = Vec::new();

            tokio::select! {
                result = rx.recv() => {
                    match result {
                        Some(event) => {
                            tracing::debug!("predictor: received access event for {:?}", event.relative_path);
                            if scheduler.is_caching_allowed() {
                                if !deferred.is_empty() {
                                    tracing::info!(
                                        "predictor: caching window open, flushing {} deferred event(s)",
                                        deferred.len()
                                    );
                                    to_process.extend(deferred.drain().map(|(_, ev)| ev));
                                    clear_deferred(&cache_dir);
                                }
                                to_process.push(event);
                            } else {
                                buffer_event(&mut deferred, event);
                                save_deferred(&cache_dir, &deferred);
                                tracing::debug!(
                                    "predictor: outside caching window, {} show(s) buffered",
                                    deferred.len()
                                );
                            }
                        }
                        None => break,
                    }
                }
                _ = tick.tick() => {
                    if scheduler.is_caching_allowed() && !deferred.is_empty() {
                        tracing::info!(
                            "predictor: caching window opened, flushing {} deferred event(s)",
                            deferred.len()
                        );
                        to_process.extend(deferred.drain().map(|(_, ev)| ev));
                        clear_deferred(&cache_dir);
                    }
                }
            }

            for event in to_process {
                let next = find_next_episodes_free(&event.relative_path, &plex_db, backing_fd, lookahead);
                if next.is_empty() {
                    tracing::debug!("predictor: no upcoming episodes found for {:?}", event.relative_path);
                }

                let budget_active = max_cache_pull_bytes > 0;
                let mut running_total = if budget_active { cache.total_cached_bytes() } else { 0 };
                let mut first_candidate = true;

                for rel in next {
                    if cache.is_cached(&rel) {
                        tracing::debug!("predictor: {} already cached, skipping", rel.display());
                        continue;
                    }
                    if in_flight.contains(&rel) {
                        tracing::debug!("predictor: {} already in-flight, skipping", rel.display());
                        continue;
                    }
                    if budget_active {
                        let file_size = stat_backing_file(backing_fd, &rel).unwrap_or(0);
                        if !first_candidate && running_total + file_size > max_cache_pull_bytes {
                            tracing::info!(
                                "predictor: budget exhausted ({:.1} MB used of {:.1} MB), stopping before {}",
                                running_total as f64 / 1_048_576.0,
                                max_cache_pull_bytes as f64 / 1_048_576.0,
                                rel.display(),
                            );
                            break;
                        }
                        running_total += file_size;
                    }
                    first_candidate = false;

                    let cache_dest = cache.cache_path(&rel);
                    tracing::info!("predictor: queuing {} for caching", rel.display());
                    in_flight.insert(rel.clone());
                    let _ = copy_tx.send(CopyRequest { rel_path: rel, cache_dest }).await;
                }
            }
        }
    }

}

pub async fn run_copier_task(
    backing_fd: RawFd,
    mut rx: mpsc::Receiver<CopyRequest>,
    cache: Arc<CacheManager>,
) {
    while let Some(req) = rx.recv().await {
        if cache.is_cached(&req.rel_path) {
            tracing::debug!("copier: {} already cached, skipping", req.rel_path.display());
            continue;
        }

        if !cache.has_free_space() {
            tracing::warn!(
                "copier: insufficient free space, skipping {}",
                req.rel_path.display()
            );
            continue;
        }

        cache.evict_if_needed();

        let rel = req.rel_path.clone();
        let dest = req.cache_dest.clone();
        tracing::info!("copier: caching {}", rel.display());

        let result =
            tokio::task::spawn_blocking(move || crate::copier::copy_to_cache(backing_fd, &rel, &dest))
                .await;

        match result {
            Ok(Ok(())) => tracing::info!("copier: cached {}", req.rel_path.display()),
            Ok(Err(e)) => tracing::warn!("copier: copy failed {}: {e}", req.rel_path.display()),
            Err(e) => tracing::warn!("copier: task panicked {}: {e}", req.rel_path.display()),
        }
    }
}

// ---- deferred event helpers ----

/// Returns the buffer key for an access event: the parent directory of the file.
/// Files in the same directory with SxxExx names are treated as the same show.
pub fn show_root(rel_path: &Path) -> PathBuf {
    rel_path.parent().unwrap_or(Path::new("")).to_path_buf()
}

/// Insert an event into the deferred buffer, keeping only the most advanced
/// (highest season/episode) event per show directory.
pub fn buffer_event(deferred: &mut HashMap<PathBuf, AccessEvent>, event: AccessEvent) {
    let file_name = event.relative_path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let new_pos = parse_season_episode(&file_name);
    let key = match new_pos {
        Some(_) => show_root(&event.relative_path),
        None => event.relative_path.clone(), // non-episode: key by full path
    };

    if let Some(existing) = deferred.get(&key) {
        let existing_name = existing.relative_path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let old_pos = parse_season_episode(&existing_name);
        if new_pos >= old_pos {
            deferred.insert(key, event);
        }
    } else {
        deferred.insert(key, event);
    }
}

fn deferred_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("deferred_events.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Persist the deferred buffer to disk atomically.
fn save_deferred(cache_dir: &Path, deferred: &HashMap<PathBuf, AccessEvent>) {
    let records: Vec<DeferredRecord> = deferred
        .values()
        .map(|ev| DeferredRecord { path: ev.relative_path.clone(), timestamp: now_secs() })
        .collect();
    let json = match serde_json::to_string(&records) {
        Ok(j) => j,
        Err(e) => { tracing::warn!("predictor: failed to serialize deferred events: {e}"); return; }
    };
    let dest = deferred_path(cache_dir);
    let tmp = dest.with_extension("json.tmp");
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, &dest);
    }
}

/// Load persisted deferred events, discarding entries older than `ttl_minutes`.
pub fn load_deferred(cache_dir: &Path, ttl_minutes: u64) -> HashMap<PathBuf, AccessEvent> {
    let path = deferred_path(cache_dir);
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return HashMap::new(),
    };
    let records: Vec<DeferredRecord> = match serde_json::from_str(&data) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("predictor: corrupt deferred events file, discarding: {e}");
            let _ = std::fs::remove_file(&path);
            return HashMap::new();
        }
    };
    let cutoff = now_secs().saturating_sub(ttl_minutes * 60);
    let mut result = HashMap::new();
    for rec in records {
        if rec.timestamp >= cutoff {
            buffer_event(&mut result, AccessEvent { relative_path: rec.path });
        } else {
            tracing::debug!("predictor: discarding stale deferred event for {:?}", rec.path);
        }
    }
    result
}

/// Remove the persisted deferred events file.
fn clear_deferred(cache_dir: &Path) {
    let _ = std::fs::remove_file(deferred_path(cache_dir));
}

// ---- episode prediction helpers ----

fn find_next_episodes_free(
    rel_path: &Path,
    plex_db: &Option<PlexDb>,
    backing_fd: RawFd,
    lookahead: usize,
) -> Vec<PathBuf> {
    if let Some(db) = plex_db {
        let found = db.next_episodes(rel_path, lookahead);
        if !found.is_empty() {
            tracing::info!(
                "predictor: Plex DB found {} upcoming episode(s) after {:?}",
                found.len(), rel_path
            );
            return found;
        }
        tracing::debug!("predictor: Plex DB returned no results for {:?}, trying regex", rel_path);
    }
    let found = regex_fallback_free(rel_path, backing_fd, lookahead);
    if !found.is_empty() {
        tracing::info!(
            "predictor: regex found {} upcoming episode(s) after {:?}",
            found.len(), rel_path
        );
    }
    found
}

fn regex_fallback_free(rel_path: &Path, backing_fd: RawFd, lookahead: usize) -> Vec<PathBuf> {
    let name = match rel_path.file_name() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => return vec![],
    };
    let (season, episode) = match parse_season_episode(&name) {
        Some(se) => se,
        None => return vec![],
    };
    let dir = rel_path.parent().unwrap_or(Path::new(""));

    // Phase 1: same-season, higher-episode files in the current directory.
    let entries = list_backing_dir(backing_fd, dir);
    let mut candidates: Vec<(u32, u32, PathBuf)> = entries
        .into_iter()
        .filter_map(|entry_name| {
            let s = entry_name.to_string_lossy();
            let (s_num, e_num) = parse_season_episode(&s)?;
            if s_num == season && e_num > episode {
                Some((s_num, e_num, dir.join(&*entry_name)))
            } else {
                None
            }
        })
        .collect();
    candidates.sort_by_key(|(s, e, _)| (*s, *e));
    candidates.truncate(lookahead);

    // Phase 2: cross-season, if we still need more episodes.
    if candidates.len() < lookahead {
        let needed = lookahead - candidates.len();
        let parent_dir_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if parse_season_dir(&parent_dir_name).is_some() {
            // Structured layout: Season X folders under a show directory.
            let show_dir = dir.parent().unwrap_or(Path::new(""));
            let show_entries = list_backing_dir(backing_fd, show_dir);

            let mut next_seasons: Vec<(u32, PathBuf)> = show_entries
                .into_iter()
                .filter_map(|entry_name| {
                    let s_num = parse_season_dir(&entry_name.to_string_lossy())?;
                    if s_num > season {
                        Some((s_num, show_dir.join(&*entry_name)))
                    } else {
                        None
                    }
                })
                .collect();
            next_seasons.sort_by_key(|(s, _)| *s);

            'outer: for (_, season_dir) in next_seasons {
                let season_entries = list_backing_dir(backing_fd, &season_dir);
                let mut eps: Vec<(u32, u32, PathBuf)> = season_entries
                    .into_iter()
                    .filter_map(|entry_name| {
                        let s = entry_name.to_string_lossy();
                        let (s_num, e_num) = parse_season_episode(&s)?;
                        Some((s_num, e_num, season_dir.join(&*entry_name)))
                    })
                    .collect();
                eps.sort_by_key(|(s, e, _)| (*s, *e));
                for ep in eps {
                    if candidates.len() >= lookahead {
                        break 'outer;
                    }
                    candidates.push(ep);
                }
            }
        } else {
            // Flat layout: all seasons in one directory. Scan for higher-season episodes.
            let flat_entries = list_backing_dir(backing_fd, dir);
            let mut flat_candidates: Vec<(u32, u32, PathBuf)> = flat_entries
                .into_iter()
                .filter_map(|entry_name| {
                    let s = entry_name.to_string_lossy();
                    let (s_num, e_num) = parse_season_episode(&s)?;
                    if s_num > season {
                        Some((s_num, e_num, dir.join(&*entry_name)))
                    } else {
                        None
                    }
                })
                .collect();
            flat_candidates.sort_by_key(|(s, e, _)| (*s, *e));
            for ep in flat_candidates.into_iter().take(needed) {
                candidates.push(ep);
            }
        }
    }

    candidates.into_iter().take(lookahead).map(|(_, _, p)| p).collect()
}

// ---- helpers ----

static SEASON_EP_RE: OnceLock<Regex> = OnceLock::new();
static SEASON_DIR_RE: OnceLock<Regex> = OnceLock::new();

fn season_ep_re() -> &'static Regex {
    SEASON_EP_RE.get_or_init(|| Regex::new(r"(?i)[Ss](\d{1,2})[Ee](\d{1,3})").unwrap())
}

fn season_dir_re() -> &'static Regex {
    SEASON_DIR_RE.get_or_init(|| Regex::new(r"(?i)^Season\s+0*(\d+)$").unwrap())
}

pub fn parse_season_episode(name: &str) -> Option<(u32, u32)> {
    let cap = season_ep_re().captures(name)?;
    Some((cap[1].parse().ok()?, cap[2].parse().ok()?))
}

pub fn parse_season_dir(name: &str) -> Option<u32> {
    let cap = season_dir_re().captures(name)?;
    cap[1].parse().ok()
}

/// Returns the file size in bytes for a file on the backing store, or None on error.
fn stat_backing_file(backing_fd: RawFd, rel_path: &Path) -> Option<u64> {
    let bytes = rel_path.as_os_str().as_bytes();
    let bytes = bytes.strip_prefix(b"/").unwrap_or(bytes);
    let c = CString::new(bytes).ok()?;
    let fd = unsafe { libc::openat(backing_fd, c.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return None;
    }
    let size = unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut stat) == 0 { Some(stat.st_size as u64) } else { None }
    };
    unsafe { libc::close(fd) };
    size
}

fn list_backing_dir(backing_fd: RawFd, rel_dir: &Path) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let c_dir = if rel_dir == Path::new("") {
        CString::new(".").unwrap()
    } else {
        let bytes = rel_dir.as_os_str().as_bytes();
        let bytes = bytes.strip_prefix(b"/").unwrap_or(bytes);
        CString::new(bytes).unwrap_or_else(|_| CString::new(".").unwrap())
    };

    let dir_fd =
        unsafe { libc::openat(backing_fd, c_dir.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
    if dir_fd < 0 {
        return vec![];
    }

    let dir = unsafe { libc::fdopendir(dir_fd) };
    if dir.is_null() {
        unsafe { libc::close(dir_fd) };
        return vec![];
    }
    unsafe { libc::rewinddir(dir) };

    let mut out = Vec::new();
    loop {
        unsafe { *libc::__errno_location() = 0 };
        let dirent = unsafe { libc::readdir(dir) };
        if dirent.is_null() {
            break;
        }
        let name_bytes = unsafe {
            std::ffi::CStr::from_ptr((*dirent).d_name.as_ptr())
                .to_bytes()
                .to_vec()
        };
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        out.push(OsString::from_vec(name_bytes));
    }
    unsafe { libc::closedir(dir) };
    out
}
