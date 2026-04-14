use std::path::Path;
use std::sync::Arc;

use fuser::{MountOption, SessionACL};
use fscache::cache::io::{CacheIO, CacheIoConfig};
use fscache::engine::action::{AccessEvent, ActionEngine};
use fscache::cache::db::CacheDb;
use fscache::cache::manager::CacheManager;
use fscache::config::InvalidationConfig;
use fscache::fuse::fusefs::FsCache;
use fscache::preset::CachePreset;
use fscache::presets::plex_episode_prediction::PlexEpisodePrediction;
use fscache::engine::scheduler::Scheduler;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

fn test_fuse_config() -> fuser::Config {
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("fscache-test".to_string()),
    ];
    config.acl = SessionACL::Owner;
    config
}

/// Test harness: a FUSE mount with a separate backing dir and mount point.
///
/// In production, FUSE is mounted *over* the backing path (overmount).
/// In tests, we use two separate temp dirs so the backing files remain
/// directly accessible for comparison and integrity checks.
pub struct FuseHarness {
    /// The original source files live here — never touched by the FUSE fs.
    pub backing: TempDir,
    pub mount: TempDir,
    pub cache: Option<TempDir>,
    /// Exposed for tests that need to call sweep_stale(), mark_cached(), etc. directly.
    pub cache_mgr: Option<Arc<CacheManager>>,
    /// Kept alive to hold the FUSE mount; dropped at end of test to unmount.
    _session: fuser::BackgroundSession,
}

impl FuseHarness {
    /// Plain passthrough harness (no cache overlay).
    pub fn new() -> anyhow::Result<Self> {
        let backing = TempDir::new()?;
        let mount = TempDir::new()?;

        let fs = FsCache::new(backing.path())?;
        let session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config())?;

        Ok(Self {
            backing,
            mount,
            cache: None,
            cache_mgr: None,
            _session: session,
        })
    }

    /// Harness with a cache overlay.  `max_size_gb` and `expiry_hours` are
    /// configurable so tests can trigger eviction with small values.
    pub fn new_with_cache(max_size_gb: f64, expiry_hours: u64) -> anyhow::Result<Self> {
        let backing = TempDir::new()?;
        let mount = TempDir::new()?;
        let cache_dir = TempDir::new()?;

        let mut fs = FsCache::new(backing.path())?;
        let db = Arc::new(CacheDb::open(&cache_dir.path().join("test.db"))?);
        let cache_mgr = Arc::new(CacheManager::new(
            cache_dir.path().to_path_buf(),
            db,
            cache_dir.path().to_path_buf(),
            max_size_gb,
            expiry_hours,
            0.0, // no min-free-space check in tests
            None,
            &InvalidationConfig::default(),
        ));
        cache_mgr.startup_cleanup();
        fs.cache = Some(Arc::clone(&cache_mgr));

        let session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config())?;

        Ok(Self {
            backing,
            mount,
            cache: Some(cache_dir),
            cache_mgr: Some(Arc::clone(&cache_mgr)),
            _session: session,
        })
    }

    /// Harness with a cache overlay and a fully-wired `CacheManager` (backing store connected),
    /// matching the production setup in `main.rs`. Required for E2E invalidation tests where
    /// `is_stale()` must be able to stat the backing file.
    pub fn new_with_cache_and_invalidation(
        max_size_gb: f64,
        expiry_hours: u64,
        invalidation: &InvalidationConfig,
    ) -> anyhow::Result<Self> {
        let backing = TempDir::new()?;
        let mount = TempDir::new()?;
        let cache_dir = TempDir::new()?;

        let mut fs = FsCache::new(backing.path())?;
        let backing_store = Arc::clone(&fs.backing_store);
        let db = Arc::new(CacheDb::open(&cache_dir.path().join("test.db"))?);
        let cache_mgr = Arc::new(CacheManager::new(
            cache_dir.path().to_path_buf(),
            db,
            cache_dir.path().to_path_buf(),
            max_size_gb,
            expiry_hours,
            0.0,
            Some(backing_store),
            invalidation,
        ));
        cache_mgr.startup_cleanup();
        fs.cache = Some(Arc::clone(&cache_mgr));

        let session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config())?;

        Ok(Self {
            backing,
            mount,
            cache: Some(cache_dir),
            cache_mgr: Some(Arc::clone(&cache_mgr)),
            _session: session,
        })
    }

    /// Panics if the harness was created without a cache (e.g. via `new()`).
    pub fn cache_mgr(&self) -> &Arc<CacheManager> {
        self.cache_mgr.as_ref().expect("harness has no cache manager")
    }

    pub fn backing_path(&self) -> &Path {
        self.backing.path()
    }

    pub fn mount_path(&self) -> &Path {
        self.mount.path()
    }

    pub fn cache_path(&self) -> &Path {
        self.cache.as_ref().expect("harness has no cache dir").path()
    }

    /// Full pipeline harness: FUSE + cache overlay + predictor + copier all wired together.
    ///
    /// The predictor/copier tasks are spawned on the current tokio runtime — call this
    /// from inside `#[tokio::test]`.
    pub fn new_full_pipeline(lookahead: usize) -> anyhow::Result<Self> {
        let preset = Arc::new(PlexEpisodePrediction::new(lookahead, vec![], false));
        Self::new_full_pipeline_with_preset(preset)
    }

    /// Full pipeline with a process blocklist.
    /// Blocked processes (and their children) never trigger prediction.
    pub fn new_full_pipeline_with_blocklist(
        lookahead: usize,
        blocklist: Vec<String>,
    ) -> anyhow::Result<Self> {
        let preset = Arc::new(PlexEpisodePrediction::new(lookahead, blocklist, false));
        Self::new_full_pipeline_with_preset(preset)
    }

    /// Full pipeline with a caller-supplied preset. Use this to test presets
    /// other than PlexEpisodePrediction against a live FUSE mount.
    ///
    /// Must be called from inside `#[tokio::test]`.
    pub fn new_full_pipeline_with_preset(preset: Arc<dyn CachePreset>) -> anyhow::Result<Self> {
        Self::new_full_pipeline_with_config(preset, 1.0, 72, 0)
    }

    /// Full pipeline with explicit cache budget, expiry, and eviction-worker cadence.
    ///
    /// Use `eviction_interval_secs > 0` when a test needs the autonomous eviction
    /// worker to fire (e.g. LRU trim or TTL expiry tests).  Use 0 to disable it and
    /// call `cache_mgr().evict_if_needed()` directly for synchronous control.
    ///
    /// Must be called from inside `#[tokio::test]`.
    pub fn new_full_pipeline_with_config(
        preset: Arc<dyn CachePreset>,
        max_size_gb: f64,
        expiry_hours: u64,
        eviction_interval_secs: u64,
    ) -> anyhow::Result<Self> {
        let backing = TempDir::new()?;
        let mount = TempDir::new()?;
        let cache_dir = TempDir::new()?;

        let mut fs = FsCache::new(backing.path())?;
        let backing_store = Arc::clone(&fs.backing_store);
        fs.preset = Some(Arc::clone(&preset));

        let db = Arc::new(CacheDb::open(&cache_dir.path().join("test.db"))?);
        let cache_mgr = Arc::new(CacheManager::new(
            cache_dir.path().to_path_buf(),
            db,
            cache_dir.path().to_path_buf(),
            max_size_gb,
            expiry_hours,
            0.0,
            None,
            &InvalidationConfig::default(),
        ));
        cache_mgr.startup_cleanup();
        fs.cache = Some(Arc::clone(&cache_mgr));

        let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
        fs.access_tx = Some(access_tx);

        let scheduler = Scheduler::new("00:00", "23:59").unwrap();
        let shutdown = CancellationToken::new();
        let (cache_io, _io_handles) = CacheIO::spawn(
            CacheIoConfig {
                max_concurrent_copies: 1,
                eviction_interval_secs,
                deferred_ttl_minutes: 0,
            },
            Arc::clone(&cache_mgr),
            Arc::clone(&backing_store),
            scheduler,
            shutdown.clone(),
        );
        let engine = ActionEngine::new(
            access_rx,
            cache_io,
            Arc::clone(&cache_mgr),
            Some(preset),
            Arc::clone(&backing_store),
            0,
            0,
            0,
        );
        tokio::spawn(engine.run(shutdown));

        let session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config())?;

        Ok(Self {
            backing,
            mount,
            cache: Some(cache_dir),
            cache_mgr: Some(Arc::clone(&cache_mgr)),
            _session: session,
        })
    }
}

/// Overmount harness: FUSE is mounted ON TOP of the same directory the files
/// live in, exactly as in production.  The O_PATH fd retained in `FsCache`
/// provides access to the real files underneath after the overmount.
///
/// ## Drop ordering
/// `_session` is the FIRST field — Rust drops fields in declaration order, so
/// the FUSE session unmounts before `dir` (and `cache`) are cleaned up.
/// Without this ordering, `TempDir::drop` would try to remove an active mountpoint.
pub struct OvermountHarness {
    /// Dropped FIRST — unmounts FUSE before the TempDir cleanup below.
    _session: fuser::BackgroundSession,
    /// The single directory: real files live here AND FUSE is mounted here.
    pub dir: TempDir,
    /// Separate SSD cache dir — not overmounted.
    pub cache: TempDir,
}

impl OvermountHarness {
    /// Create an overmount harness.
    ///
    /// `populate` is called with the directory path BEFORE the FUSE mount so
    /// the closure can write test files while the path is still directly writable.
    /// After `spawn_mount2` the path goes through FUSE (read-only), so no further
    /// writes are possible via normal filesystem calls.
    ///
    /// Must be called from inside a `#[tokio::test]` — predictor/copier tasks
    /// are spawned on the current tokio runtime.
    pub fn new<F>(lookahead: usize, populate: F) -> anyhow::Result<Self>
    where
        F: FnOnce(&Path),
    {
        let dir = TempDir::new()?;
        let cache_dir = TempDir::new()?;

        // Write all test files BEFORE the overmount — after mounting, the path
        // goes through FUSE (RO) and std::fs::write would return EACCES.
        populate(dir.path());

        let mut fs = FsCache::new(dir.path())?;
        let backing_store = Arc::clone(&fs.backing_store);

        let preset = Arc::new(PlexEpisodePrediction::new(lookahead, vec![], false));
        fs.preset = Some(Arc::clone(&preset) as Arc<dyn CachePreset>);

        let db = Arc::new(CacheDb::open(&cache_dir.path().join("test.db"))?);
        let cache_mgr = Arc::new(CacheManager::new(
            cache_dir.path().to_path_buf(),
            db,
            cache_dir.path().to_path_buf(),
            1.0,
            72,
            0.0,
            None,
            &InvalidationConfig::default(),
        ));
        cache_mgr.startup_cleanup();
        fs.cache = Some(Arc::clone(&cache_mgr));

        let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
        fs.access_tx = Some(access_tx);

        let scheduler = Scheduler::new("00:00", "23:59").unwrap();
        let shutdown = CancellationToken::new();
        let (cache_io, _io_handles) = CacheIO::spawn(
            CacheIoConfig { max_concurrent_copies: 1, eviction_interval_secs: 0, deferred_ttl_minutes: 0 },
            Arc::clone(&cache_mgr),
            Arc::clone(&backing_store),
            scheduler,
            shutdown.clone(),
        );
        let engine = ActionEngine::new(
            access_rx,
            cache_io,
            Arc::clone(&cache_mgr),
            Some(preset as Arc<dyn CachePreset>),
            Arc::clone(&backing_store),
            0,
            0,
            0,
        );
        tokio::spawn(engine.run(shutdown));

        // AutoUnmount requires SessionACL::All (root) so cannot be used in tests.
        // Clean unmount is guaranteed by drop ordering: _session drops first.
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            MountOption::RO,
            MountOption::FSName("fscache-overmount-test".to_string()),
        ];
        config.acl = SessionACL::Owner;

        let session = fuser::spawn_mount2(fs, dir.path(), &config)?;

        Ok(Self {
            _session: session, // FIRST field — drops first
            dir,
            cache: cache_dir,
        })
    }

    /// The overmounted directory path. Reads go through FUSE; the real files
    /// underneath are accessible to the filesystem via the O_PATH backing fd.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn cache_path(&self) -> &Path {
        self.cache.path()
    }
}

/// Multi-mount harness: N independent FUSE mounts sharing one cache base directory.
///
/// Each mount has its own backing dir, mount point, and cache subdirectory
/// (`shared_cache_base/<index>/`).  All sessions are dropped at end of test.
///
/// Use `new_full_pipeline` when you need the predictor + copier wired up.
pub struct MultiFuseHarness {
    pub mounts: Vec<FuseHarness>,
    /// Parent directory — each mount's cache lives at `shared_cache_base/<index>/`.
    pub shared_cache_base: TempDir,
}

impl MultiFuseHarness {
    /// Create `n` independent FUSE passthrough mounts sharing a cache base dir.
    pub fn new_with_cache(n: usize, max_size_gb: f64, expiry_hours: u64) -> anyhow::Result<Self> {
        let shared_cache_base = TempDir::new()?;
        let shared_db = Arc::new(CacheDb::open(&shared_cache_base.path().join("test.db"))?);
        let mut mounts = Vec::with_capacity(n);
        for i in 0..n {
            let backing = TempDir::new()?;
            let mount = TempDir::new()?;
            let cache_subdir = shared_cache_base.path().join(i.to_string());
            std::fs::create_dir_all(&cache_subdir)?;

            let mut fs = FsCache::new(backing.path())?;
            let cache_mgr = Arc::new(CacheManager::new(
                cache_subdir,
                Arc::clone(&shared_db),
                shared_cache_base.path().to_path_buf(),
                max_size_gb,
                expiry_hours,
                0.0,
                None,
                &InvalidationConfig::default(),
            ));
            cache_mgr.startup_cleanup();
            fs.cache = Some(Arc::clone(&cache_mgr));

            let session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config())?;
            mounts.push(FuseHarness { backing, mount, cache: None, cache_mgr: Some(Arc::clone(&cache_mgr)), _session: session });
        }
        Ok(Self { mounts, shared_cache_base })
    }

    /// Create `n` independent full-pipeline mounts (FUSE + cache + predictor + copier).
    /// Must be called from inside `#[tokio::test]`.
    pub fn new_full_pipeline(n: usize, lookahead: usize) -> anyhow::Result<Self> {
        let shared_cache_base = TempDir::new()?;
        let shared_db = Arc::new(CacheDb::open(&shared_cache_base.path().join("test.db"))?);
        let mut mounts = Vec::with_capacity(n);
        for i in 0..n {
            let backing = TempDir::new()?;
            let mount = TempDir::new()?;
            let cache_subdir = shared_cache_base.path().join(i.to_string());
            std::fs::create_dir_all(&cache_subdir)?;

            let mut fs = FsCache::new(backing.path())?;
            let backing_store = Arc::clone(&fs.backing_store);

            let preset = Arc::new(PlexEpisodePrediction::new(lookahead, vec![], false));
            fs.preset = Some(Arc::clone(&preset) as Arc<dyn CachePreset>);

            let cache_mgr = Arc::new(CacheManager::new(
                cache_subdir.clone(),
                Arc::clone(&shared_db),
                shared_cache_base.path().to_path_buf(),
                1.0,
                72,
                0.0,
                None,
                &InvalidationConfig::default(),
            ));
            cache_mgr.startup_cleanup();
            fs.cache = Some(Arc::clone(&cache_mgr));

            let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
            fs.access_tx = Some(access_tx);

            let scheduler = Scheduler::new("00:00", "23:59").unwrap();
            let shutdown = CancellationToken::new();
            let (cache_io, _io_handles) = CacheIO::spawn(
                CacheIoConfig { max_concurrent_copies: 1, eviction_interval_secs: 0, deferred_ttl_minutes: 0 },
                Arc::clone(&cache_mgr),
                Arc::clone(&backing_store),
                scheduler,
                shutdown.clone(),
            );
            let engine = ActionEngine::new(
                access_rx,
                cache_io,
                Arc::clone(&cache_mgr),
                Some(preset as Arc<dyn CachePreset>),
                Arc::clone(&backing_store),
                0,
                0,
                0,
            );
            tokio::spawn(engine.run(shutdown));

            let session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config())?;
            mounts.push(FuseHarness { backing, mount, cache: None, cache_mgr: Some(Arc::clone(&cache_mgr)), _session: session });
        }
        Ok(Self { mounts, shared_cache_base })
    }

    pub fn backing_path(&self, idx: usize) -> &std::path::Path {
        self.mounts[idx].backing.path()
    }

    pub fn mount_path(&self, idx: usize) -> &std::path::Path {
        self.mounts[idx].mount.path()
    }

    pub fn cache_subdir(&self, idx: usize) -> std::path::PathBuf {
        self.shared_cache_base.path().join(idx.to_string())
    }
}

pub fn write_multi_backing_file(harness: &MultiFuseHarness, mount_idx: usize, rel: &str, content: &[u8]) {
    let path = harness.backing_path(mount_idx).join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

pub fn read_multi_mount_file(harness: &MultiFuseHarness, mount_idx: usize, rel: &str) -> Vec<u8> {
    std::fs::read(harness.mount_path(mount_idx).join(rel)).unwrap()
}

pub fn write_backing_file(harness: &FuseHarness, rel: &str, content: &[u8]) {
    let path = harness.backing_path().join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

pub fn read_mount_file(harness: &FuseHarness, rel: &str) -> Vec<u8> {
    std::fs::read(harness.mount_path().join(rel)).unwrap()
}

pub fn file_hash(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path).unwrap();
    let digest = Sha256::digest(&data);
    hex::encode(digest)
}

pub fn collect_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    collect_files_inner(dir, &mut out);
    out.sort();
    out
}

fn collect_files_inner(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                out.push(path);
            } else if path.is_dir() {
                collect_files_inner(&path, out);
            }
        }
    }
}
