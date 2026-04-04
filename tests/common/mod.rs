use std::path::Path;
use std::sync::Arc;

use fuser::{MountOption, SessionACL};
use plex_hot_cache::cache::CacheManager;
use plex_hot_cache::fuse_fs::PlexHotCacheFs;
use plex_hot_cache::predictor::{run_copier_task, AccessEvent, CopyRequest, Predictor};
use plex_hot_cache::scheduler::Scheduler;
use tempfile::TempDir;
use tokio::sync::mpsc;

fn test_fuse_config() -> fuser::Config {
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("plex-hot-cache-test".to_string()),
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
    /// The FUSE filesystem is mounted here — reads come from backing.
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

        let fs = PlexHotCacheFs::new(backing.path())?;
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

        let mut fs = PlexHotCacheFs::new(backing.path())?;
        let cache_mgr = Arc::new(CacheManager::new(
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
        let backing = TempDir::new()?;
        let mount = TempDir::new()?;
        let cache_dir = TempDir::new()?;

        let mut fs = PlexHotCacheFs::new(backing.path())?;
        let backing_fd = fs.backing_fd;

        let cache_mgr = Arc::new(CacheManager::new(
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

        // Spawn predictor and copier on the current tokio runtime.
        let scheduler = Scheduler::new("00:00", "23:59").unwrap();
        let predictor = Predictor::new(
            access_rx,
            copy_tx,
            Arc::clone(&cache_mgr),
            lookahead,
            None, // no Plex DB in tests — use regex fallback
            scheduler,
            backing_fd,
        );
        tokio::spawn(predictor.run());
        tokio::spawn(run_copier_task(backing_fd, copy_rx, Arc::clone(&cache_mgr)));

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
/// live in, exactly as in production.  The O_PATH fd retained in `PlexHotCacheFs`
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

        let mut fs = PlexHotCacheFs::new(dir.path())?;
        let backing_fd = fs.backing_fd;

        let cache_mgr = Arc::new(CacheManager::new(
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
        let predictor = Predictor::new(
            access_rx,
            copy_tx,
            Arc::clone(&cache_mgr),
            lookahead,
            None,
            scheduler,
            backing_fd,
        );
        tokio::spawn(predictor.run());
        tokio::spawn(run_copier_task(backing_fd, copy_rx, Arc::clone(&cache_mgr)));

        // AutoUnmount requires SessionACL::All (root) so cannot be used in tests.
        // Clean unmount is guaranteed by drop ordering: _session drops first.
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            MountOption::RO,
            MountOption::FSName("plex-hot-cache-overmount-test".to_string()),
        ];
        config.acl = SessionACL::Owner;

        // Overmount: FUSE is mounted on top of the same path the files live in.
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

/// Write a test file to the backing dir and return its path relative to the root.
pub fn write_backing_file(harness: &FuseHarness, rel: &str, content: &[u8]) {
    let path = harness.backing_path().join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// Read a file through the FUSE mount.
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

/// Recursively collect all file paths under a directory.
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
