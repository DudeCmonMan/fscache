use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// No persistent database — uses filesystem timestamps exclusively.
pub struct CacheManager {
    cache_dir: PathBuf,
    /// Root directory measured for global size budgeting. In single-mount mode
    /// this equals `cache_dir`. In multi-mount mode this is the shared parent so
    /// eviction respects the total budget across all mounts.
    global_cache_dir: PathBuf,
    max_size_bytes: u64,
    expiry: Duration,
    min_free_bytes: u64,
}

impl CacheManager {
    /// `cache_dir` is this mount's write directory; `global_cache_dir` is the
    /// root measured for budget enforcement (pass `cache_dir.clone()` for
    /// single-mount setups where local and global are the same).
    pub fn new(
        cache_dir: PathBuf,
        global_cache_dir: PathBuf,
        max_size_gb: f64,
        expiry_hours: u64,
        min_free_space_gb: f64,
    ) -> Self {
        let configured = (max_size_gb * 1_073_741_824.0) as u64;
        let max_size_bytes = match total_space_bytes(&global_cache_dir) {
            Some(total) if configured == 0 || configured > total => {
                let fallback = total / 2;
                tracing::warn!(
                    "max_size_gb={max_size_gb:.2} is invalid for this drive \
                     (total capacity {:.2} GB); falling back to 50% ({:.2} GB)",
                    total as f64 / 1_073_741_824.0,
                    fallback as f64 / 1_073_741_824.0,
                );
                fallback
            }
            _ => configured,
        };
        let cm = Self {
            cache_dir,
            global_cache_dir,
            max_size_bytes,
            expiry: Duration::from_secs(expiry_hours * 3600),
            min_free_bytes: (min_free_space_gb * 1_073_741_824.0) as u64,
        };
        tracing::info!(
            "Cache manager: dir={}, max={:.1} GB, expiry={}h, min_free={:.1} GB",
            cm.cache_dir.display(),
            cm.max_size_bytes as f64 / 1_073_741_824.0,
            expiry_hours,
            min_free_space_gb,
        );
        cm
    }

    pub fn cache_path(&self, rel_path: &Path) -> PathBuf {
        self.cache_dir.join(rel_path)
    }

    pub fn is_cached(&self, rel_path: &Path) -> bool {
        let p = self.cache_path(rel_path);
        p.exists() && !p.extension().map_or(false, |e| e == "partial")
    }

    pub fn startup_cleanup(&self) {
        let removed = remove_partials(&self.cache_dir);
        let existing = collect_cache_files(&self.cache_dir);
        let total_bytes: u64 = existing.iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        tracing::info!(
            "Cache startup: {} files cached ({:.1} MB used), {} partial files removed",
            existing.len(),
            total_bytes as f64 / 1_048_576.0,
            removed,
        );
    }

    pub fn evict_if_needed(&self) {
        let now = SystemTime::now();
        let mut expiry_count = 0u32;
        let mut size_count = 0u32;
        let mut reclaimed_bytes = 0u64;

        let mut local = collect_cache_files(&self.cache_dir);
        local.retain(|entry| {
            if let Some(age) = mtime_age(entry, now) {
                if age > self.expiry {
                    let file_size = std::fs::metadata(entry).map(|m| m.len()).unwrap_or(0);
                    if let Err(e) = std::fs::remove_file(entry) {
                        tracing::warn!("evict (expiry): failed to delete {}: {e}", entry.display());
                    } else {
                        tracing::info!("evict (expired): {}", entry.display());
                        expiry_count += 1;
                        reclaimed_bytes += file_size;
                    }
                    return false;
                }
            }
            true
        });

        // Measure global total across all mounts to enforce the shared budget.
        let mut global_total: u64 = collect_cache_files(&self.global_cache_dir)
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();

        if global_total <= self.max_size_bytes {
            if expiry_count > 0 {
                tracing::info!(
                    "eviction complete: {} expired files removed ({:.1} MB reclaimed)",
                    expiry_count,
                    reclaimed_bytes as f64 / 1_048_576.0,
                );
            }
            return;
        }

        // Global budget exceeded — evict oldest files from this mount's cache dir.
        local.sort_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.accessed())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        });

        for path in local {
            if global_total <= self.max_size_bytes {
                break;
            }
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!("evict (size): failed to delete {}: {e}", path.display());
            } else {
                tracing::info!("evict (size limit): {}", path.display());
                size_count += 1;
                reclaimed_bytes += size;
                global_total = global_total.saturating_sub(size);
            }
        }

        if expiry_count > 0 || size_count > 0 {
            tracing::info!(
                "eviction complete: {} expired + {} size-limit files removed ({:.1} MB reclaimed)",
                expiry_count,
                size_count,
                reclaimed_bytes as f64 / 1_048_576.0,
            );
        }
    }

    pub fn has_free_space(&self) -> bool {
        free_space_bytes(&self.cache_dir)
            .map(|free| free >= self.min_free_bytes)
            .unwrap_or(false)
    }

    /// Returns the total size in bytes of all fully-cached files.
    pub fn total_cached_bytes(&self) -> u64 {
        collect_cache_files(&self.cache_dir)
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum()
    }

}

// ---- helpers ----

fn remove_partials(dir: &Path) -> u32 {
    let mut count = 0u32;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                count += remove_partials(&path);
            } else if path.extension().map_or(false, |e| e == "partial") {
                if std::fs::remove_file(&path).is_ok() {
                    tracing::debug!("startup_cleanup: removed {}", path.display());
                    count += 1;
                }
            }
        }
    }
    count
}

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

fn total_space_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut stat) };
    if rc == 0 {
        Some(stat.f_blocks * stat.f_frsize as u64)
    } else {
        None
    }
}
