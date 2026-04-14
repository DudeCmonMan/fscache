use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::config::Config;

/// All state displayed by the TUI. Populated via IPC events from the daemon
/// and a DB polling task for cache file stats.
pub struct DashboardState {
    // -- Full daemon config (from Hello, authoritative for all static values) --
    pub config: Arc<Config>,

    // -- FUSE counters --
    pub fuse_opens:   AtomicU64,
    pub cache_hits:   AtomicU64,
    pub cache_misses: AtomicU64,
    pub bytes_read:   AtomicU64,
    pub open_handles: AtomicU64,

    // -- Cache stats (DB polling) --
    pub cache_used_bytes:  AtomicU64,
    pub cache_free_bytes:  AtomicU64,
    pub cache_file_count:  AtomicU64,
    pub cached_files:      Mutex<Vec<CachedFileInfo>>,

    // -- Action Engine (live counters updated from telemetry events) --
    pub in_flight_count:   AtomicU64,
    pub deferred_count:    AtomicU64,
    pub budget_used_bytes: AtomicU64,
    /// Effective prediction budget in bytes. Seeded from config on connect, then
    /// updated live by `TelemetryEvent::BudgetUpdated`.
    pub budget_max_bytes:  AtomicU64,

    // -- Copier --
    pub active_copies:    Mutex<HashMap<PathBuf, CopyProgress>>,
    pub completed_copies: AtomicU64,
    pub failed_copies:    AtomicU64,

    // -- Evictions --
    pub evictions_expired: AtomicU64,
    pub evictions_size:    AtomicU64,

    // -- Scheduler --
    pub caching_allowed: AtomicBool,

    // -- Mounts (set once at startup) --
    pub mounts: Mutex<Vec<MountInfo>>,

    // -- Log capture --
    pub recent_logs: Mutex<VecDeque<LogEntry>>,
}

impl DashboardState {
    pub fn new(config: Arc<Config>) -> Self {
        let budget_max = (config.cache.max_cache_pull_per_mount_gb * 1_073_741_824.0) as u64;
        Self {
            config,

            fuse_opens:   AtomicU64::new(0),
            cache_hits:   AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            bytes_read:   AtomicU64::new(0),
            open_handles: AtomicU64::new(0),

            cache_used_bytes:  AtomicU64::new(0),
            cache_free_bytes:  AtomicU64::new(0),
            cache_file_count:  AtomicU64::new(0),
            cached_files:      Mutex::new(Vec::new()),

            in_flight_count:   AtomicU64::new(0),
            deferred_count:    AtomicU64::new(0),
            budget_used_bytes: AtomicU64::new(0),
            budget_max_bytes:  AtomicU64::new(budget_max),

            active_copies:    Mutex::new(HashMap::new()),
            completed_copies: AtomicU64::new(0),
            failed_copies:    AtomicU64::new(0),

            evictions_expired: AtomicU64::new(0),
            evictions_size:    AtomicU64::new(0),

            caching_allowed: AtomicBool::new(false),

            mounts:      Mutex::new(Vec::new()),
            recent_logs: Mutex::new(VecDeque::new()),
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

pub struct LogEntry {
    pub timestamp: String,
    pub level:     String,
    pub message:   String,
}

pub struct CachedFileInfo {
    pub path:        PathBuf,
    pub size_bytes:  u64,
    pub cached_at:   SystemTime,
    pub last_hit_at: SystemTime,
    pub evicts_at:   SystemTime,
    /// DB `mount_id` for this file (the mount's cache subdirectory path string).
    pub mount_id:    String,
}

pub struct CopyProgress {
    pub path:         PathBuf,
    pub size_bytes:   u64,
    pub bytes_copied: u64,
    pub started_at:   std::time::Instant,
}

impl CopyProgress {
    pub fn elapsed_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

pub struct MountInfo {
    pub target:    PathBuf,
    /// The mount's cache subdirectory; also the `mount_id` in the database.
    pub cache_dir: PathBuf,
    pub active:    bool,
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

/// Which page is currently displayed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Status,
    Cache,
    CacheIo,
    Logs,
}

impl Page {
    pub fn next(self) -> Self {
        match self {
            Self::Status  => Self::Cache,
            Self::Cache   => Self::CacheIo,
            Self::CacheIo => Self::Logs,
            Self::Logs    => Self::Status,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Status  => Self::Logs,
            Self::Cache   => Self::Status,
            Self::CacheIo => Self::Cache,
            Self::Logs    => Self::CacheIo,
        }
    }
}
