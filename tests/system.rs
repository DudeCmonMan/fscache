/// System-level tests: graceful shutdown (lazy unmount), fd survival,
/// clean remount, and log rate limiting.
///
/// Tests 1-2 require FUSE privileges — run with `sudo cargo test`.
mod common;

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use plex_hot_cache::fuse_fs::PlexHotCacheFs;
use tempfile::TempDir;

fn test_fuse_config() -> fuser::Config {
    let mut config = fuser::Config::default();
    config.mount_options = vec![
        fuser::MountOption::RO,
        fuser::MountOption::FSName("plex-hot-cache-system-test".to_string()),
    ];
    config.acl = fuser::SessionACL::Owner;
    config
}

// ---- Graceful shutdown: lazy unmount keeps open fds valid ----

/// After `fusermount -uz`, an already-opened file descriptor can still be read
/// to completion. New opens through the mount point fail.
#[test]
fn lazy_unmount_keeps_open_fds_valid() {
    let backing = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();

    // Write a test file to the backing store.
    let content = b"streaming media content that should survive lazy unmount";
    std::fs::write(backing.path().join("movie.mkv"), content).unwrap();

    let fs = PlexHotCacheFs::new(backing.path()).unwrap();
    let _session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config()).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    // Open the file through FUSE — simulates Plex holding an fd.
    let mut file = std::fs::File::open(mount.path().join("movie.mkv")).unwrap();

    // Lazy unmount — detaches the mount but keeps existing fds alive.
    let status = std::process::Command::new("fusermount")
        .args(["-uz", "--"])
        .arg(mount.path())
        .status()
        .expect("fusermount not found");
    assert!(status.success(), "fusermount -uz failed");

    // The already-open fd should still be readable.
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, content, "open fd should still return correct data after lazy unmount");

    // New opens through the mount point should either fail or hit the
    // underlying (empty) tmpdir — not the FUSE filesystem.
    let new_open = std::fs::File::open(mount.path().join("movie.mkv"));
    assert!(
        new_open.is_err(),
        "new opens should fail after lazy unmount (mount detached)"
    );
}

// ---- Clean restart after lazy unmount ----

/// After lazy unmount + session drop, a fresh FUSE mount on the same path
/// works correctly with no fd conflicts.
#[test]
fn lazy_unmount_then_remount_no_conflict() {
    let backing = TempDir::new().unwrap();
    let mount = TempDir::new().unwrap();

    let content = b"original content";
    std::fs::write(backing.path().join("file.mkv"), content).unwrap();

    // First mount
    let fs = PlexHotCacheFs::new(backing.path()).unwrap();
    let session = fuser::spawn_mount2(fs, mount.path(), &test_fuse_config()).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    // Open a file (simulates Plex streaming)
    let mut file = std::fs::File::open(mount.path().join("file.mkv")).unwrap();

    // Lazy unmount
    let status = std::process::Command::new("fusermount")
        .args(["-uz", "--"])
        .arg(mount.path())
        .status()
        .expect("fusermount not found");
    assert!(status.success(), "fusermount -uz failed");

    // Grace period (shorter than production for test speed)
    std::thread::sleep(Duration::from_millis(500));

    // Old fd still works
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).unwrap();
    assert_eq!(buf, content);
    drop(file);

    // Drop old session
    drop(session);
    std::thread::sleep(Duration::from_millis(200));

    // Remount on the same path
    let fs2 = PlexHotCacheFs::new(backing.path()).unwrap();
    let _session2 = fuser::spawn_mount2(fs2, mount.path(), &test_fuse_config()).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    // New mount serves files correctly
    let data = std::fs::read(mount.path().join("file.mkv")).unwrap();
    assert_eq!(data, content, "remounted FUSE should serve correct data");
}

// ---- Log rate limiting ----

/// First call to should_suppress_log returns false (INFO); subsequent calls
/// within the window return true (suppressed to DEBUG).
#[test]
fn repeat_log_window_suppresses_duplicate_access_logs() {
    let backing = TempDir::new().unwrap();
    let mut fs = PlexHotCacheFs::new(backing.path()).unwrap();
    fs.repeat_log_window = Duration::from_secs(60);

    let path = Path::new("Movies/Some Movie (2024)/movie.mkv");

    // First access — should NOT be suppressed
    assert!(
        !fs.should_suppress_log(path),
        "first access should log at INFO"
    );

    // Same path again — should be suppressed
    assert!(
        fs.should_suppress_log(path),
        "repeated access within window should be suppressed"
    );

    // Different path — should NOT be suppressed
    let other = Path::new("tv/Show/Show.S01E01.mkv");
    assert!(
        !fs.should_suppress_log(other),
        "different path should log at INFO"
    );
}

/// With repeat_log_window = 0, suppression is disabled — every call returns false.
#[test]
fn repeat_log_window_zero_disables_suppression() {
    let backing = TempDir::new().unwrap();
    let mut fs = PlexHotCacheFs::new(backing.path()).unwrap();
    fs.repeat_log_window = Duration::ZERO;

    let path = Path::new("Movies/Some Movie (2024)/movie.mkv");

    assert!(!fs.should_suppress_log(path), "should not suppress with window=0");
    assert!(!fs.should_suppress_log(path), "should not suppress with window=0 (second call)");
    assert!(!fs.should_suppress_log(path), "should not suppress with window=0 (third call)");
}
