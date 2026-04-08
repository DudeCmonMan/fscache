use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Mutex;
use std::time::SystemTime;

/// All state displayed by the TUI. Populated by MetricsLayer (tracing events)
/// and a polling task (CacheManager::stats() every 3s).
pub struct DashboardState {
    // -- FUSE counters (updated by MetricsLayer from tracing events) --
    pub fuse_opens:   AtomicU64,
    pub cache_hits:   AtomicU64,
    pub cache_misses: AtomicU64,
    pub bytes_read:   AtomicU64,
    pub open_handles: AtomicU64,

    // -- Cache stats (updated by polling task every 3s) --
    pub cache_used_bytes:      AtomicU64,
    pub cache_max_bytes:       AtomicU64,
    pub cache_free_bytes:      AtomicU64,
    pub cache_min_free_bytes:  AtomicU64,
    pub cache_file_count:      AtomicU64,
    pub cached_files:          Mutex<Vec<CachedFileInfo>>,

    // -- Action Engine (updated by MetricsLayer) --
    pub in_flight_count:   AtomicU64,
    pub deferred_count:    AtomicU64,
    pub budget_used_bytes: AtomicU64,
    pub budget_max_bytes:  AtomicU64,
    pub preset_name:       Mutex<String>,

    // -- Copier (updated by MetricsLayer) --
    pub active_copies:    Mutex<HashMap<PathBuf, CopyProgress>>,
    pub completed_copies: AtomicU64,
    pub failed_copies:    AtomicU64,

    // -- Evictions (updated by MetricsLayer) --
    pub evictions_expired: AtomicU64,
    pub evictions_size:    AtomicU64,

    // -- Scheduler (updated by MetricsLayer) --
    pub caching_allowed: AtomicBool,
    pub window_start:    Mutex<String>,
    pub window_end:      Mutex<String>,

    // -- Mounts (set once at startup in main.rs) --
    pub mounts: Mutex<Vec<MountInfo>>,

    // -- Log capture (ring buffer, max 200 entries) --
    pub recent_logs: Mutex<VecDeque<LogEntry>>,

    // -- Set to true by app::run() after terminal is restored so LoggingLayer
    //    falls back to stderr instead of the ring buffer. --
    pub tui_exited: AtomicBool,
}

impl DashboardState {
    pub fn new() -> Self {
        Self {
            fuse_opens:   AtomicU64::new(0),
            cache_hits:   AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            bytes_read:   AtomicU64::new(0),
            open_handles: AtomicU64::new(0),

            cache_used_bytes:     AtomicU64::new(0),
            cache_max_bytes:      AtomicU64::new(0),
            cache_free_bytes:     AtomicU64::new(0),
            cache_min_free_bytes: AtomicU64::new(0),
            cache_file_count:     AtomicU64::new(0),
            cached_files:         Mutex::new(Vec::new()),

            in_flight_count:   AtomicU64::new(0),
            deferred_count:    AtomicU64::new(0),
            budget_used_bytes: AtomicU64::new(0),
            budget_max_bytes:  AtomicU64::new(0),
            preset_name:       Mutex::new(String::new()),

            active_copies:    Mutex::new(HashMap::new()),
            completed_copies: AtomicU64::new(0),
            failed_copies:    AtomicU64::new(0),

            evictions_expired: AtomicU64::new(0),
            evictions_size:    AtomicU64::new(0),

            caching_allowed: AtomicBool::new(false),
            window_start:    Mutex::new(String::new()),
            window_end:      Mutex::new(String::new()),

            mounts:      Mutex::new(Vec::new()),
            recent_logs: Mutex::new(VecDeque::new()),
            tui_exited:  AtomicBool::new(false),
        }
    }

    pub fn push_log(&self, entry: LogEntry) {
        let mut logs = self.recent_logs.lock().unwrap();
        if logs.len() >= 200 {
            logs.pop_front();
        }
        logs.push_back(entry);
    }

}

/// One entry in the recent-logs ring buffer.
pub struct LogEntry {
    pub timestamp: String,
    pub level:     String,
    pub message:   String,
}

/// A file currently in the cache — for the Cache page file list.
pub struct CachedFileInfo {
    pub path:        PathBuf,
    pub size_bytes:  u64,
    /// When the file was first inserted into the cache (from DB `cached_at`).
    pub cached_at:   SystemTime,
    /// When the file was last read from the cache (from DB `last_hit_at`).
    pub last_hit_at: SystemTime,
    /// When this file will be evicted (last_hit_at + expiry duration).
    pub evicts_at:   SystemTime,
}

pub struct CopyProgress {
    pub path:       PathBuf,
    pub size_bytes: u64,
    pub started_at: std::time::Instant,
}

impl CopyProgress {
    pub fn elapsed_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

/// One FUSE mount — target path and whether the FUSE session is active.
pub struct MountInfo {
    pub target:  PathBuf,
    pub active:  bool,
}

/// Sort order for the Cache page file list.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CacheSort {
    Newest,
    Oldest,
    Largest,
    Smallest,
    NameAz,
}

impl CacheSort {
    pub fn next(self) -> Self {
        match self {
            Self::Newest   => Self::Oldest,
            Self::Oldest   => Self::Largest,
            Self::Largest  => Self::Smallest,
            Self::Smallest => Self::NameAz,
            Self::NameAz   => Self::Newest,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Newest   => "newest",
            Self::Oldest   => "oldest",
            Self::Largest  => "largest",
            Self::Smallest => "smallest",
            Self::NameAz   => "name A-Z",
        }
    }
}

/// Which of the 3 pages is currently displayed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Status,
    Cache,
    Logs,
}

impl Page {
    pub fn next(self) -> Self {
        match self {
            Self::Status => Self::Cache,
            Self::Cache  => Self::Logs,
            Self::Logs   => Self::Status,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Status => Self::Logs,
            Self::Cache  => Self::Status,
            Self::Logs   => Self::Cache,
        }
    }
}
