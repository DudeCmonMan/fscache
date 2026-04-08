use std::path::{Path, PathBuf};

use crate::backing_store::BackingStore;
use crate::db::CacheDb;

/// Information about the process that opened a file, captured synchronously in open().
pub struct ProcessInfo {
    pub pid: u32,
    pub name: Option<String>,
    /// Raw /proc/<pid>/cmdline bytes (NUL-delimited argv).
    pub cmdline: Option<Vec<u8>>,
    /// Parent chain process names, up to 16 levels (nearest ancestor first).
    pub ancestors: Vec<String>,
}

impl ProcessInfo {
    /// Capture process information from /proc for the given PID.
    /// Best-effort: missing fields are None/empty if the process has already exited.
    pub fn capture(pid: u32) -> Self {
        let name = proc_name(pid);
        let cmdline = std::fs::read(format!("/proc/{}/cmdline", pid)).ok();
        let ancestors = collect_ancestors(pid);
        Self { pid, name, cmdline, ancestors }
    }

    /// Returns true if this process or any ancestor is in `blocklist`.
    pub fn is_blocked_by(&self, blocklist: &[String]) -> bool {
        if let Some(ref name) = self.name {
            if blocklist.iter().any(|b| name == b) {
                return true;
            }
        }
        self.ancestors.iter().any(|a| blocklist.iter().any(|b| a == b))
    }
}

fn proc_name(pid: u32) -> Option<String> {
    let exe = std::fs::read_link(format!("/proc/{}/exe", pid)).ok()?;
    exe.file_name()?.to_str().map(|s| s.to_string())
}

fn proc_parent_pid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in status.lines() {
        if let Some(ppid_str) = line.strip_prefix("PPid:\t") {
            return ppid_str.trim().parse().ok();
        }
    }
    None
}

fn collect_ancestors(pid: u32) -> Vec<String> {
    let mut ancestors = Vec::new();
    let mut current = pid;
    for _ in 0..16 {
        if current <= 1 { break; }
        match proc_parent_pid(current) {
            Some(ppid) if ppid != current && ppid > 1 => {
                if let Some(name) = proc_name(ppid) {
                    ancestors.push(name);
                }
                current = ppid;
            }
            _ => break,
        }
    }
    ancestors
}

pub enum CacheAction {
    Cache(Vec<PathBuf>),
    Evict(Vec<PathBuf>),
}

pub struct RuleContext<'a> {
    pub backing_store: &'a BackingStore,
    pub cache_db: &'a CacheDb,
}

/// Pluggable caching behavior. All methods have default no-op implementations.
pub trait CachePreset: Send + Sync + 'static {
    fn name(&self) -> &str;

    /// Called synchronously in FUSE open(). Return true to skip caching for this open.
    /// Must be fast — no blocking I/O.
    fn should_filter(&self, _process: &ProcessInfo) -> bool {
        false
    }

    /// Called when a file is opened and not in cache.
    fn on_miss(&self, _path: &Path, _ctx: &RuleContext) -> Vec<CacheAction> {
        vec![]
    }

    /// Called when a file is opened and already in cache.
    fn on_hit(&self, _path: &Path, _ctx: &RuleContext) -> Vec<CacheAction> {
        vec![]
    }

    /// Called when a file handle is closed.
    fn on_close(&self, _path: &Path, _bytes_read: u64, _ctx: &RuleContext) -> Vec<CacheAction> {
        vec![]
    }

    /// Called on the 30-second scheduler tick.
    fn on_tick(&self, _ctx: &RuleContext) -> Vec<CacheAction> {
        vec![]
    }

    /// Key used for deferred-event deduplication. Default: full path.
    /// Override to group related paths (e.g. by show directory).
    fn deduplicate_key(&self, path: &Path) -> PathBuf {
        path.to_path_buf()
    }
}
