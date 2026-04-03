use std::path::Path;

use fuser::{MountOption, SessionACL};
use plex_hot_cache::fuse_fs::PlexHotCacheFs;
use tempfile::TempDir;

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
    /// Kept alive to hold the FUSE mount; dropped at end of test to unmount.
    _session: fuser::BackgroundSession,
}

impl FuseHarness {
    pub fn new() -> anyhow::Result<Self> {
        let backing = TempDir::new()?;
        let mount = TempDir::new()?;

        let fs = PlexHotCacheFs::new(backing.path())?;
        let session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config())?;

        Ok(Self {
            backing,
            mount,
            _session: session,
        })
    }

    pub fn backing_path(&self) -> &Path {
        self.backing.path()
    }

    pub fn mount_path(&self) -> &Path {
        self.mount.path()
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
