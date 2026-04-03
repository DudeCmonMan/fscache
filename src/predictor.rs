use std::collections::HashSet;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::SystemTime;

use regex::Regex;
use tokio::sync::mpsc;

use crate::cache::CacheManager;
use crate::plex_db::PlexDb;
use crate::scheduler::Scheduler;

pub struct AccessEvent {
    pub relative_path: PathBuf,
    pub timestamp: SystemTime,
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
    ) -> Self {
        Self { rx, copy_tx, cache, lookahead, plex_db, scheduler, backing_fd }
    }

    pub async fn run(mut self) {
        let mut in_flight: HashSet<PathBuf> = HashSet::new();

        while let Some(event) = self.rx.recv().await {
            if !self.scheduler.is_caching_allowed() {
                tracing::debug!("predictor: outside caching window, skipping");
                continue;
            }

            let next = self.find_next_episodes(&event.relative_path);
            for rel in next {
                if self.cache.is_cached(&rel) || in_flight.contains(&rel) {
                    continue;
                }
                let cache_dest = self.cache.cache_path(&rel);
                tracing::info!("predictor: queuing {} for caching", rel.display());
                in_flight.insert(rel.clone());
                let _ = self.copy_tx.send(CopyRequest { rel_path: rel, cache_dest }).await;
            }
        }
    }

    fn find_next_episodes(&self, rel_path: &Path) -> Vec<PathBuf> {
        if let Some(ref db) = self.plex_db {
            let found = db.next_episodes(rel_path, self.lookahead);
            if !found.is_empty() {
                return found;
            }
        }
        self.regex_fallback(rel_path)
    }

    fn regex_fallback(&self, rel_path: &Path) -> Vec<PathBuf> {
        let name = match rel_path.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => return vec![],
        };
        let (season, episode) = match parse_season_episode(&name) {
            Some(se) => se,
            None => return vec![],
        };
        let dir = rel_path.parent().unwrap_or(Path::new(""));

        let entries = list_backing_dir(self.backing_fd, dir);

        let mut candidates: Vec<(u32, PathBuf)> = entries
            .into_iter()
            .filter_map(|entry_name| {
                let s = entry_name.to_string_lossy();
                let (s_num, e_num) = parse_season_episode(&s)?;
                if s_num == season && e_num > episode && e_num <= episode + self.lookahead as u32 {
                    Some((e_num, dir.join(&*entry_name)))
                } else {
                    None
                }
            })
            .collect();

        candidates.sort_by_key(|(e, _)| *e);
        candidates.into_iter().take(self.lookahead).map(|(_, p)| p).collect()
    }
}

/// Copier task: processes CopyRequests one at a time.
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

// ---- helpers ----

static SEASON_EP_RE: OnceLock<Regex> = OnceLock::new();

fn season_ep_re() -> &'static Regex {
    SEASON_EP_RE.get_or_init(|| Regex::new(r"(?i)[Ss](\d{1,2})[Ee](\d{1,3})").unwrap())
}

/// Parse season and episode number from a filename containing SxxExx.
pub fn parse_season_episode(name: &str) -> Option<(u32, u32)> {
    let cap = season_ep_re().captures(name)?;
    Some((cap[1].parse().ok()?, cap[2].parse().ok()?))
}

/// List filenames in a directory relative to `backing_fd`.
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
