use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

/// SQLite-backed cache metadata store.
///
/// Tracks which files are cached (for eviction intelligence) and persists
/// deferred access events across restarts. NOT used for the hot `is_cached()`
/// check in FUSE `open()` — that remains a filesystem existence check for speed.
///
/// Thread safety: wraps `Connection` in a `Mutex`; all methods take `&self`.
pub struct CacheDb {
    conn: Mutex<Connection>,
}

impl CacheDb {
    /// Open (or create) the SQLite database at `path`, applying schema migrations.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        // WAL gives concurrent readers without blocking the single writer.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(r#"
            CREATE TABLE IF NOT EXISTS cache_files (
                rel_path    TEXT    NOT NULL,
                mount_id    TEXT    NOT NULL,
                size_bytes  INTEGER NOT NULL DEFAULT 0,
                cached_at   INTEGER NOT NULL DEFAULT 0,
                last_hit_at INTEGER NOT NULL DEFAULT 0,
                hit_count   INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (rel_path, mount_id)
            );
            CREATE INDEX IF NOT EXISTS idx_cache_files_mount_lru
                ON cache_files (mount_id, last_hit_at);

            CREATE TABLE IF NOT EXISTS deferred_events (
                key         TEXT    PRIMARY KEY,
                path        TEXT    NOT NULL,
                timestamp   INTEGER NOT NULL
            );
        "#)?;
        // Migration: add hit_count to existing databases (silently ignored if already present).
        let _ = conn.execute_batch(
            "ALTER TABLE cache_files ADD COLUMN hit_count INTEGER NOT NULL DEFAULT 0",
        );
        Ok(Self { conn: Mutex::new(conn) })
    }

    // ---- cache file tracking ----

    pub fn mark_cached(&self, rel_path: &Path, size_bytes: u64, mount_id: &str) {
        let now = now_secs() as i64;
        let key = rel_path.to_string_lossy();
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT OR REPLACE INTO cache_files \
             (rel_path, mount_id, size_bytes, cached_at, last_hit_at, hit_count) \
             VALUES (?1, ?2, ?3, ?4, ?4, 1)",
            params![key.as_ref(), mount_id, size_bytes as i64, now],
        );
        tracing::info!(event = crate::telemetry::EVENT_DB_INSERT, path = %rel_path.display(), size_bytes, "db: mark_cached {}", rel_path.display());
    }

    /// Update the last-hit timestamp for a file (called on cache hit in FUSE open).
    pub fn mark_hit(&self, rel_path: &Path, mount_id: &str) {
        let now = now_secs() as i64;
        let key = rel_path.to_string_lossy();
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE cache_files SET last_hit_at = ?1, hit_count = hit_count + 1 \
             WHERE rel_path = ?2 AND mount_id = ?3",
            params![now, key.as_ref(), mount_id],
        );
        tracing::info!(event = crate::telemetry::EVENT_DB_HIT, path = %rel_path.display(), "db: mark_hit {}", rel_path.display());
    }

    /// Remove a file's DB entry (called when evicting from cache).
    pub fn remove(&self, rel_path: &Path, mount_id: &str) {
        let key = rel_path.to_string_lossy();
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "DELETE FROM cache_files WHERE rel_path = ?1 AND mount_id = ?2",
            params![key.as_ref(), mount_id],
        );
        tracing::info!(event = crate::telemetry::EVENT_DB_REMOVE, path = %rel_path.display(), "db: remove {}", rel_path.display());
    }

    /// Return LRU-ordered eviction candidates for a mount (oldest last_hit_at first),
    /// with their recorded size in bytes. Pass `usize::MAX` to retrieve all candidates.
    pub fn eviction_candidates(&self, mount_id: &str, limit: usize) -> Vec<(PathBuf, u64)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT rel_path, size_bytes FROM cache_files \
             WHERE mount_id = ?1 ORDER BY last_hit_at ASC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![mount_id, limit as i64], |row| {
            let s: String = row.get(0)?;
            let size: i64 = row.get(1)?;
            Ok((PathBuf::from(s), size.max(0) as u64))
        })
        .into_iter()
        .flatten()
        .flatten()
        .collect()
    }

    /// Files for a mount whose last_hit_at is older than `expiry_secs` ago.
    pub fn expired_files(&self, mount_id: &str, expiry_secs: u64) -> Vec<(PathBuf, u64)> {
        let cutoff = now_secs().saturating_sub(expiry_secs) as i64;
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT rel_path, size_bytes FROM cache_files \
             WHERE mount_id = ?1 AND last_hit_at < ?2",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![mount_id, cutoff], |row| {
            let s: String = row.get(0)?;
            let size: i64 = row.get(1)?;
            Ok((PathBuf::from(s), size.max(0) as u64))
        })
        .into_iter()
        .flatten()
        .flatten()
        .collect()
    }

    pub fn total_cached_bytes(&self, mount_id: &str) -> u64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM cache_files WHERE mount_id = ?1",
            params![mount_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        .max(0) as u64
    }

    /// Total bytes across ALL mounts (for global budget enforcement).
    pub fn total_cached_bytes_global(&self) -> u64 {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COALESCE(SUM(size_bytes), 0) FROM cache_files",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        .max(0) as u64
    }

    /// Startup reconciliation for a mount:
    /// 1. Remove `.partial` files from `cache_dir`.
    /// 2. Remove DB rows whose files no longer exist on disk.
    /// 3. Insert rows for disk files that have no DB entry (crash recovery).
    pub fn reconcile_with_disk(&self, cache_dir: &Path, mount_id: &str) {
        let removed_partials = remove_partials(cache_dir);

        let conn = self.conn.lock().unwrap();

        // Step 2: purge DB rows that have no corresponding file.
        let db_paths: Vec<String> = {
            let mut stmt = match conn.prepare(
                "SELECT rel_path FROM cache_files WHERE mount_id = ?1",
            ) {
                Ok(s) => s,
                Err(_) => {
                    tracing::warn!("db: reconcile_with_disk prepare failed");
                    return;
                }
            };
            stmt.query_map(params![mount_id], |row| row.get(0))
                .into_iter()
                .flatten()
                .flatten()
                .collect()
        };

        let mut orphan_count = 0usize;
        for rel in &db_paths {
            if !cache_dir.join(rel).exists() {
                let _ = conn.execute(
                    "DELETE FROM cache_files WHERE rel_path = ?1 AND mount_id = ?2",
                    params![rel, mount_id],
                );
                orphan_count += 1;
            }
        }

        // Step 3: add disk files not yet in DB (e.g. after a crash mid-commit).
        let on_disk = crate::utils::collect_cache_files(cache_dir);
        let now = now_secs() as i64;
        let mut new_count = 0usize;
        for abs_path in &on_disk {
            let rel = match abs_path.strip_prefix(cache_dir) {
                Ok(r) => r.to_string_lossy().into_owned(),
                Err(_) => continue,
            };
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) FROM cache_files WHERE rel_path = ?1 AND mount_id = ?2",
                    params![rel, mount_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0;
            if !exists {
                let size = std::fs::metadata(abs_path).map(|m| m.len()).unwrap_or(0) as i64;
                let _ = conn.execute(
                    "INSERT OR IGNORE INTO cache_files \
                     (rel_path, mount_id, size_bytes, cached_at, last_hit_at) \
                     VALUES (?1, ?2, ?3, ?4, ?4)",
                    params![rel, mount_id, size, now],
                );
                new_count += 1;
            }
        }

        let file_count = on_disk.len();
        tracing::info!(
            "Cache startup: {} files ({} partial removed, {} DB orphans purged, {} recovered)",
            file_count, removed_partials, orphan_count, new_count,
        );
    }

    /// Persist a single deferred event entry (key = show-root path).
    pub fn save_deferred(&self, key: &Path, path: &Path, timestamp: u64) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "INSERT OR REPLACE INTO deferred_events (key, path, timestamp) \
             VALUES (?1, ?2, ?3)",
            params![
                key.to_string_lossy().as_ref(),
                path.to_string_lossy().as_ref(),
                timestamp as i64,
            ],
        );
    }

    /// Load deferred events, discarding entries older than `ttl_minutes`.
    /// Returns `(key, path, timestamp)` tuples.
    pub fn load_deferred(&self, ttl_minutes: u64) -> Vec<(PathBuf, PathBuf, u64)> {
        let cutoff = now_secs().saturating_sub(ttl_minutes * 60) as i64;
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT key, path, timestamp FROM deferred_events WHERE timestamp >= ?1",
        ) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        stmt.query_map(params![cutoff], |row| {
            let key: String = row.get(0)?;
            let path: String = row.get(1)?;
            let ts: i64 = row.get(2)?;
            Ok((PathBuf::from(key), PathBuf::from(path), ts as u64))
        })
        .into_iter()
        .flatten()
        .flatten()
        .collect()
    }

    /// Delete all deferred events (called when the caching window opens and
    /// events are flushed to the action engine / predictor).
    pub fn clear_deferred(&self) {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute("DELETE FROM deferred_events", []);
    }

    /// Return a map of `rel_path → (cached_at, last_hit_at)` for all files in a mount.
    /// Used by the TUI stats poll to display accurate insertion and last-access times.
    pub fn file_timestamps(&self, mount_id: &str) -> HashMap<PathBuf, (SystemTime, SystemTime)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT rel_path, cached_at, last_hit_at FROM cache_files WHERE mount_id = ?1",
        ) {
            Ok(s) => s,
            Err(_) => return HashMap::new(),
        };
        stmt.query_map(params![mount_id], |row| {
            let path: String = row.get(0)?;
            let cached_at: i64 = row.get(1)?;
            let last_hit_at: i64 = row.get(2)?;
            Ok((PathBuf::from(path), cached_at, last_hit_at))
        })
        .into_iter()
        .flatten()
        .flatten()
        .map(|(p, ca, lha)| {
            let t_cached  = UNIX_EPOCH + Duration::from_secs(ca.max(0) as u64);
            let t_last    = UNIX_EPOCH + Duration::from_secs(lha.max(0) as u64);
            (p, (t_cached, t_last))
        })
        .collect()
    }

    /// Directly set `last_hit_at` for a cached file. Used in tests to simulate
    /// files that were last accessed at a specific point in time.
    pub fn set_last_hit_at_for_test(&self, rel_path: &Path, mount_id: &str, timestamp_secs: i64) {
        let key = rel_path.to_string_lossy();
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute(
            "UPDATE cache_files SET last_hit_at = ?1 WHERE rel_path = ?2 AND mount_id = ?3",
            params![timestamp_secs, key.as_ref(), mount_id],
        );
    }
}

// ---- private helpers ----

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn remove_partials(dir: &Path) -> usize {
    let mut count = 0usize;
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

