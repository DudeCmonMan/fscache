use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Manages the SSD cache directory.
///
/// - No persistent database; uses filesystem timestamps exclusively.
/// - `.partial` files are invisible to FUSE and cleaned up on startup.
/// - Eviction: delete files older than `expiry_hours`, then by oldest atime
///   until under `max_size_bytes`.
pub struct CacheManager {
    cache_dir: PathBuf,
    max_size_bytes: u64,
    expiry: Duration,
    min_free_bytes: u64,
}

impl CacheManager {
    pub fn new(
        cache_dir: PathBuf,
        max_size_gb: f64,
        expiry_hours: u64,
        min_free_space_gb: f64,
    ) -> Self {
        Self {
            cache_dir,
            max_size_bytes: (max_size_gb * 1_073_741_824.0) as u64,
            expiry: Duration::from_secs(expiry_hours * 3600),
            min_free_bytes: (min_free_space_gb * 1_073_741_824.0) as u64,
        }
    }

    /// Path where a cached copy of `rel_path` would live.
    pub fn cache_path(&self, rel_path: &Path) -> PathBuf {
        self.cache_dir.join(rel_path)
    }

    /// Returns true if a complete cached copy exists (not .partial).
    pub fn is_cached(&self, rel_path: &Path) -> bool {
        let p = self.cache_path(rel_path);
        p.exists() && !p.extension().map_or(false, |e| e == "partial")
    }

    /// Delete all `.partial` files left over from interrupted copies.
    pub fn startup_cleanup(&self) {
        remove_partials(&self.cache_dir);
        tracing::info!("Cache startup cleanup complete for {}", self.cache_dir.display());
    }

    /// Evict expired files, then enforce max size.
    /// Call before starting a new copy or on the periodic janitor tick.
    pub fn evict_if_needed(&self) {
        let now = SystemTime::now();

        // Phase 1 of eviction: delete files past expiry_hours.
        let mut all = collect_cache_files(&self.cache_dir);
        all.retain(|entry| {
            if let Some(age) = mtime_age(entry, now) {
                if age > self.expiry {
                    if let Err(e) = std::fs::remove_file(entry) {
                        tracing::warn!("evict (expiry): failed to delete {}: {e}", entry.display());
                    } else {
                        tracing::debug!("evict (expiry): deleted {}", entry.display());
                    }
                    return false;
                }
            }
            true
        });

        // Phase 2: enforce max_size_bytes — delete by oldest atime first.
        let mut total: u64 = all
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();

        if total <= self.max_size_bytes {
            return;
        }

        // Sort by atime ascending (oldest first).
        all.sort_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.accessed())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        });

        for path in all {
            if total <= self.max_size_bytes {
                break;
            }
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!("evict (size): failed to delete {}: {e}", path.display());
            } else {
                tracing::debug!("evict (size): deleted {}", path.display());
                total = total.saturating_sub(size);
            }
        }
    }

    /// True if the underlying filesystem has enough free space to allow a copy.
    pub fn has_free_space(&self) -> bool {
        free_space_bytes(&self.cache_dir)
            .map(|free| free >= self.min_free_bytes)
            .unwrap_or(false)
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}

// ---- helpers ----

fn remove_partials(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                remove_partials(&path);
            } else if path.extension().map_or(false, |e| e == "partial") {
                let _ = std::fs::remove_file(&path);
                tracing::debug!("startup_cleanup: removed {}", path.display());
            }
        }
    }
}

/// Recursively collect all regular (non-.partial) files under `dir`.
fn collect_cache_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_inner(dir, &mut out);
    out
}

fn collect_inner(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_inner(&path, out);
            } else if !path.extension().map_or(false, |e| e == "partial") {
                out.push(path);
            }
        }
    }
}

fn mtime_age(path: &Path, now: SystemTime) -> Option<Duration> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    now.duration_since(mtime).ok()
}

fn free_space_bytes(path: &Path) -> Option<u64> {
    // Use statvfs to get available bytes on the filesystem containing `path`.
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
    if rc == 0 {
        Some(stat.f_bavail * stat.f_bsize as u64)
    } else {
        None
    }
}
