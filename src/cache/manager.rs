use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::db::CacheDb;

/// Snapshot of cache state returned by [`CacheManager::stats`].
/// Polled by the TUI every few seconds — never called from the FUSE hot path.
pub struct CacheStats {
    pub max_size_bytes:   u64,
    pub min_free_bytes:   u64,
    pub expiry:           Duration,
    pub free_space_bytes: Option<u64>,
    pub used_bytes:       u64,
    pub file_count:       usize,
    /// (relative_path, size_bytes, cached_at, last_hit_at) — timestamps from DB
    pub files:            Vec<(PathBuf, u64, SystemTime, SystemTime)>,
}

/// Filesystem-backed cache manager with SQLite metadata tracking.
///
/// `is_cached()` uses a filesystem existence check (fast, no SQLite on the FUSE hot path).
/// Eviction and budget tracking use the embedded `CacheDb` (DB queries, not filesystem walks).
/// The DB also handles `mark_cached`/`mark_hit` bookkeeping, deferred event persistence,
/// and startup reconciliation.
pub struct CacheManager {
    cache_dir: PathBuf,
    max_size_bytes: u64,
    expiry: Duration,
    min_free_bytes: u64,
    /// Unique identifier for this mount's cache partition within the shared DB.
    mount_id: String,
    db: Arc<CacheDb>,
}

impl CacheManager {
    /// `cache_dir` is this mount's write directory.
    /// `db` is the shared instance-level database (opened once in main, shared across mounts).
    /// `capacity_check_dir` is used for drive capacity checks — typically the cache root.
    pub fn new(
        cache_dir: PathBuf,
        db: Arc<CacheDb>,
        capacity_check_dir: PathBuf,
        max_size_gb: f64,
        expiry_hours: u64,
        min_free_space_gb: f64,
    ) -> Self {
        let configured = (max_size_gb * 1_073_741_824.0) as u64;
        let max_size_bytes = match total_space_bytes(&capacity_check_dir) {
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

        let mount_id = cache_dir.to_string_lossy().into_owned();

        let cm = Self {
            cache_dir,
            max_size_bytes,
            expiry: Duration::from_secs(expiry_hours * 3600),
            min_free_bytes: (min_free_space_gb * 1_073_741_824.0) as u64,
            mount_id,
            db,
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

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn cache_db(&self) -> Arc<CacheDb> {
        Arc::clone(&self.db)
    }

    /// Fast path: filesystem existence check. Not backed by SQLite to keep FUSE open() fast.
    pub fn is_cached(&self, rel_path: &Path) -> bool {
        let p = self.cache_path(rel_path);
        p.exists() && !p.extension().map_or(false, |e| e == "partial")
    }

    /// Record a successful cache population. Called by the copier task after rename.
    pub fn mark_cached(&self, rel_path: &Path, size_bytes: u64) {
        self.db.mark_cached(rel_path, size_bytes, &self.mount_id);
    }

    /// Record a cache hit. Called from FUSE open() when serving from SSD.
    pub fn mark_hit(&self, rel_path: &Path) {
        self.db.mark_hit(rel_path, &self.mount_id);
    }

    /// Reconcile DB with the filesystem, remove .partial files.
    /// Call once at startup before serving requests.
    pub fn startup_cleanup(&self) {
        self.db.reconcile_with_disk(&self.cache_dir, &self.mount_id);
    }

    pub fn evict_if_needed(&self) {
        let mut expiry_count = 0u32;
        let mut size_count = 0u32;
        let mut reclaimed_bytes = 0u64;

        if !self.expiry.is_zero() {
            for (rel_path, size) in self.db.expired_files(&self.mount_id, self.expiry.as_secs()) {
                let abs_path = self.cache_dir.join(&rel_path);
                if let Err(e) = std::fs::remove_file(&abs_path) {
                    tracing::warn!("evict (expiry): failed to delete {}: {e}", abs_path.display());
                } else {
                    tracing::info!(event = crate::telemetry::EVENT_EVICTION, path = %abs_path.display(), reason = "expired", "evict (expired): {}", abs_path.display());
                    self.db.remove(&rel_path, &self.mount_id);
                    expiry_count += 1;
                    reclaimed_bytes += size;
                }
            }
        }

        let mut global_total = self.db.total_cached_bytes_global();

        if global_total > self.max_size_bytes {
            // LRU-ordered candidates from this mount (oldest last_hit_at first).
            for (rel_path, size) in self.db.eviction_candidates(&self.mount_id, usize::MAX) {
                if global_total <= self.max_size_bytes {
                    break;
                }
                let abs_path = self.cache_dir.join(&rel_path);
                if let Err(e) = std::fs::remove_file(&abs_path) {
                    tracing::warn!("evict (size): failed to delete {}: {e}", abs_path.display());
                } else {
                    tracing::info!(event = crate::telemetry::EVENT_EVICTION, path = %abs_path.display(), reason = "size_limit", "evict (size limit): {}", abs_path.display());
                    self.db.remove(&rel_path, &self.mount_id);
                    size_count += 1;
                    reclaimed_bytes += size;
                    global_total = global_total.saturating_sub(size);
                }
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

    /// Returns the total size in bytes of all fully-cached files for this mount (DB query).
    pub fn total_cached_bytes(&self) -> u64 {
        self.db.total_cached_bytes(&self.mount_id)
    }

    /// Returns a full snapshot of cache state for TUI display.
    /// Walks the cache directory — call at low frequency (every few seconds), never from FUSE.
    pub fn stats(&self) -> CacheStats {
        let files_raw = crate::utils::collect_cache_files(&self.cache_dir);
        let mut used_bytes = 0u64;
        let mut files = Vec::with_capacity(files_raw.len());

        // Pull DB timestamps once (single lock, single query) rather than per-file.
        let timestamps = self.db.file_timestamps(&self.mount_id);

        for abs_path in &files_raw {
            if let Ok(meta) = std::fs::metadata(abs_path) {
                let size = meta.len();
                used_bytes += size;
                let rel = abs_path
                    .strip_prefix(&self.cache_dir)
                    .unwrap_or(abs_path)
                    .to_path_buf();
                let (cached_at, last_hit_at) = timestamps
                    .get(&rel)
                    .copied()
                    .unwrap_or((SystemTime::UNIX_EPOCH, SystemTime::UNIX_EPOCH));
                files.push((rel, size, cached_at, last_hit_at));
            }
        }

        CacheStats {
            max_size_bytes:   self.max_size_bytes,
            min_free_bytes:   self.min_free_bytes,
            expiry:           self.expiry,
            free_space_bytes: free_space_bytes(&self.cache_dir),
            used_bytes,
            file_count:       files.len(),
            files,
        }
    }
}

// ---- helpers ----

fn statvfs_query(path: &Path) -> Option<libc::statvfs> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } == 0 { Some(stat) } else { None }
}

fn free_space_bytes(path: &Path) -> Option<u64> {
    statvfs_query(path).map(|s| s.f_bavail * s.f_bsize as u64)
}

fn total_space_bytes(path: &Path) -> Option<u64> {
    statvfs_query(path).map(|s| s.f_blocks * s.f_frsize as u64)
}
