use std::path::Path;
use std::sync::Arc;

use fuser::{MountOption, SessionACL};
use fscache::action_engine::{run_copier_task, AccessEvent, ActionEngine, CopyRequest};
use fscache::cache::CacheManager;
use fscache::fuse_fs::FsCache;
use fscache::preset::CachePreset;
use fscache::presets::plex_episode_prediction::PlexEpisodePrediction;
use fscache::scheduler::Scheduler;
use tempfile::TempDir;
use tokio::sync::mpsc;

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
    /// Optional separate cache dir for cache overlay tests.
    pub cache: Option<TempDir>,
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
        let cache_mgr = Arc::new(CacheManager::new(
            cache_dir.path().to_path_buf(),
            cache_dir.path().to_path_buf(),
            max_size_gb,
            expiry_hours,
            0.0, // no min-free-space check in tests
        ));
        cache_mgr.startup_cleanup();
        fs.cache = Some(Arc::clone(&cache_mgr));

        let session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config())?;

        Ok(Self {
            backing,
            mount,
            cache: Some(cache_dir),
            _session: session,
        })
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
    /// other than PlexEpisodePrediction (e.g. CacheOnMiss) against a live FUSE mount.
    ///
    /// Must be called from inside `#[tokio::test]`.
    pub fn new_full_pipeline_with_preset(preset: Arc<dyn CachePreset>) -> anyhow::Result<Self> {
        let backing = TempDir::new()?;
        let mount = TempDir::new()?;
        let cache_dir = TempDir::new()?;

        let mut fs = FsCache::new(backing.path())?;
        let backing_store = Arc::clone(&fs.backing_store);
        fs.preset = Some(Arc::clone(&preset));

        let cache_mgr = Arc::new(CacheManager::new(
            cache_dir.path().to_path_buf(),
            cache_dir.path().to_path_buf(),
            1.0,
            72,
            0.0,
        ));
        cache_mgr.startup_cleanup();
        fs.cache = Some(Arc::clone(&cache_mgr));

        let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
        let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(64);
        fs.access_tx = Some(access_tx);

        let scheduler = Scheduler::new("00:00", "23:59").unwrap();
        let engine = ActionEngine::new(
            access_rx,
            copy_tx,
            Arc::clone(&cache_mgr),
            Some(preset),
            scheduler,
            Arc::clone(&backing_store),
            0,
            0,
            0,
            0,
        );
        tokio::spawn(engine.run());
        tokio::spawn(run_copier_task(backing_store, copy_rx, Arc::clone(&cache_mgr)));

        let session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config())?;

        Ok(Self {
            backing,
            mount,
            cache: Some(cache_dir),
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

        let cache_mgr = Arc::new(CacheManager::new(
            cache_dir.path().to_path_buf(),
            cache_dir.path().to_path_buf(),
            1.0,
            72,
            0.0,
        ));
        cache_mgr.startup_cleanup();
        fs.cache = Some(Arc::clone(&cache_mgr));

        let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
        let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(64);
        fs.access_tx = Some(access_tx);

        let scheduler = Scheduler::new("00:00", "23:59").unwrap();
        let engine = ActionEngine::new(
            access_rx,
            copy_tx,
            Arc::clone(&cache_mgr),
            Some(preset as Arc<dyn CachePreset>),
            scheduler,
            Arc::clone(&backing_store),
            0,
            0,
            0,
            0,
        );
        tokio::spawn(engine.run());
        tokio::spawn(run_copier_task(backing_store, copy_rx, Arc::clone(&cache_mgr)));

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
        let mut mounts = Vec::with_capacity(n);
        for i in 0..n {
            let backing = TempDir::new()?;
            let mount = TempDir::new()?;
            let cache_subdir = shared_cache_base.path().join(i.to_string());
            std::fs::create_dir_all(&cache_subdir)?;

            let mut fs = FsCache::new(backing.path())?;
            let cache_mgr = Arc::new(CacheManager::new(
                cache_subdir,
                shared_cache_base.path().to_path_buf(),
                max_size_gb,
                expiry_hours,
                0.0,
            ));
            cache_mgr.startup_cleanup();
            fs.cache = Some(Arc::clone(&cache_mgr));

            let session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config())?;
            mounts.push(FuseHarness { backing, mount, cache: None, _session: session });
        }
        Ok(Self { mounts, shared_cache_base })
    }

    /// Create `n` independent full-pipeline mounts (FUSE + cache + predictor + copier).
    /// Must be called from inside `#[tokio::test]`.
    pub fn new_full_pipeline(n: usize, lookahead: usize) -> anyhow::Result<Self> {
        let shared_cache_base = TempDir::new()?;
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
                shared_cache_base.path().to_path_buf(),
                1.0,
                72,
                0.0,
            ));
            cache_mgr.startup_cleanup();
            fs.cache = Some(Arc::clone(&cache_mgr));

            let (access_tx, access_rx) = mpsc::unbounded_channel::<AccessEvent>();
            let (copy_tx, copy_rx) = mpsc::channel::<CopyRequest>(64);
            fs.access_tx = Some(access_tx);

            let scheduler = Scheduler::new("00:00", "23:59").unwrap();
            let engine = ActionEngine::new(
                access_rx,
                copy_tx,
                Arc::clone(&cache_mgr),
                Some(preset as Arc<dyn CachePreset>),
                scheduler,
                Arc::clone(&backing_store),
                0,
                0,
                0,
                0,
            );
            tokio::spawn(engine.run());
            tokio::spawn(run_copier_task(backing_store, copy_rx, Arc::clone(&cache_mgr)));

            let session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config())?;
            mounts.push(FuseHarness { backing, mount, cache: None, _session: session });
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

/// SHA-256 hash of a file's contents.
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
