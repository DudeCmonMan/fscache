/// Tests verifying that FUSE metadata (timestamps, permissions, file types, symlinks,
/// statfs) is faithfully passed through from the backing store.
mod common;
use common::{write_backing_file, FuseHarness};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::Duration;

// ---- Timestamp fidelity ----

/// mtime nanoseconds must survive the round-trip through FUSE getattr.
/// Previously Duration::from_secs() discarded the nanosecond component.
#[test]
fn mtime_nanoseconds_preserved() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "timestamped.mkv", b"media data");

    let backing_path = h.backing_path().join("timestamped.mkv");
    let target_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 123_456_789);
    filetime::set_file_mtime(&backing_path, target_mtime).expect("set_file_mtime failed");

    std::thread::sleep(Duration::from_millis(100));

    let meta = std::fs::metadata(h.mount_path().join("timestamped.mkv"))
        .expect("metadata through FUSE failed");

    assert_eq!(meta.mtime(), 1_700_000_000, "mtime seconds should match");
    assert_eq!(meta.mtime_nsec(), 123_456_789, "mtime nanoseconds should be preserved (not truncated to 0)");
}

/// Same check for atime.
#[test]
fn atime_nanoseconds_preserved() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "atimed.mkv", b"data");

    let backing_path = h.backing_path().join("atimed.mkv");
    let target_atime = filetime::FileTime::from_unix_time(1_600_000_000, 987_654_321);
    filetime::set_file_atime(&backing_path, target_atime).expect("set_file_atime failed");

    std::thread::sleep(Duration::from_millis(100));

    let meta = std::fs::metadata(h.mount_path().join("atimed.mkv"))
        .expect("metadata through FUSE failed");

    assert_eq!(meta.atime(), 1_600_000_000, "atime seconds should match");
    assert_eq!(meta.atime_nsec(), 987_654_321, "atime nanoseconds should be preserved");
}

// ---- Permissions and ownership ----

#[test]
fn file_permissions_match_backing() {
    use std::os::unix::fs::PermissionsExt;

    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "perms.mkv", b"data");

    let backing_path = h.backing_path().join("perms.mkv");
    std::fs::set_permissions(&backing_path, std::fs::Permissions::from_mode(0o750))
        .expect("set_permissions failed");

    std::thread::sleep(Duration::from_millis(100));

    let backing_meta = std::fs::metadata(&backing_path).unwrap();
    let mount_meta = std::fs::metadata(h.mount_path().join("perms.mkv"))
        .expect("metadata through FUSE failed");

    assert_eq!(
        mount_meta.permissions().mode() & 0o7777,
        backing_meta.permissions().mode() & 0o7777,
        "permissions through FUSE should match backing file"
    );
    assert_eq!(mount_meta.uid(), backing_meta.uid(), "uid should match backing file");
    assert_eq!(mount_meta.gid(), backing_meta.gid(), "gid should match backing file");
}

// ---- Symlinks ----

/// A symlink in the backing dir must be reported as a symlink and be resolvable
/// through the FUSE mount. This is the core readlink fix.
#[test]
fn symlink_readlink_works() {
    use std::os::unix::fs::symlink;

    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "real.mkv", b"real media content");

    symlink(
        h.backing_path().join("real.mkv"),
        h.backing_path().join("link.mkv"),
    ).expect("symlink creation failed");

    std::thread::sleep(Duration::from_millis(100));

    let link_meta = std::fs::symlink_metadata(h.mount_path().join("link.mkv"))
        .expect("symlink_metadata through FUSE failed");
    assert!(link_meta.file_type().is_symlink(), "FUSE should report link.mkv as a symlink");

    let target = std::fs::read_link(h.mount_path().join("link.mkv"))
        .expect("read_link through FUSE failed");
    assert_eq!(target, h.backing_path().join("real.mkv"), "readlink should return the correct symlink target");

    let data = std::fs::read(h.mount_path().join("link.mkv"))
        .expect("reading through symlink via FUSE failed");
    assert_eq!(data, b"real media content");
}

/// A directory symlink must allow traversal into the subtree below it.
/// This covers the case where a Plex library directory is itself a symlink.
#[test]
fn directory_symlink_traversal() {
    use std::os::unix::fs::symlink;

    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "real_dir/episode.mkv", b"episode content");

    symlink(
        h.backing_path().join("real_dir"),
        h.backing_path().join("linked_dir"),
    ).expect("directory symlink creation failed");

    std::thread::sleep(Duration::from_millis(100));

    let link_meta = std::fs::symlink_metadata(h.mount_path().join("linked_dir"))
        .expect("symlink_metadata for directory symlink failed");
    assert!(link_meta.file_type().is_symlink(), "FUSE should report linked_dir as a symlink");

    let entries: Vec<_> = std::fs::read_dir(h.mount_path().join("linked_dir"))
        .expect("read_dir through directory symlink failed")
        .flatten()
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    assert!(entries.contains(&"episode.mkv".to_string()), "directory symlink traversal should expose underlying files");

    let data = std::fs::read(h.mount_path().join("linked_dir/episode.mkv"))
        .expect("reading file through directory symlink failed");
    assert_eq!(data, b"episode content");
}

// ---- statfs ----

/// statvfs on the FUSE mount must return valid, non-zero filesystem stats.
/// Validates that `df` and free-space checks (Sonarr/Radarr) work correctly.
#[test]
fn statfs_returns_valid_data() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "dummy.mkv", b"x");
    std::thread::sleep(Duration::from_millis(100));

    let mount_path = h.mount_path().to_path_buf();
    let c_path = std::ffi::CString::new(mount_path.as_os_str().as_bytes()).unwrap();

    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };

    assert_eq!(rc, 0, "statvfs on FUSE mount should succeed (df / free-space checks would fail otherwise)");
    assert!(buf.f_bsize > 0, "block size must be positive");
    assert!(buf.f_blocks > 0, "total blocks must be positive");
    assert!(buf.f_bavail <= buf.f_blocks, "available blocks cannot exceed total blocks");
    assert!(buf.f_namemax > 0, "max filename length must be positive");
}

// ---- Backing-store stat ----

/// getattr always reflects the backing store's current stat, not the cached copy.
/// Cached files are exact metadata mirrors of the source at copy time, so for the
/// common case (backing file unchanged) both return identical values. This test
/// verifies that after the backing mtime changes, getattr returns the NEW backing
/// mtime rather than a stale cached value.
#[tokio::test]
async fn getattr_returns_backing_store_mtime() {
    let h = FuseHarness::new_with_cache(1.0, 72).expect("FUSE harness with cache failed");

    write_backing_file(&h, "tv/Show/S01E01.mkv", b"episode content");
    let backing_path = h.backing_path().join("tv/Show/S01E01.mkv");

    // Set initial mtime and copy to cache.
    let initial_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&backing_path, initial_mtime).unwrap();

    let cache_dest = h.cache_path().join("tv/Show/S01E01.mkv");
    {
        let c = std::ffi::CString::new(h.backing_path().as_os_str().as_bytes()).unwrap();
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY) };
        assert!(fd >= 0);
        let bs = f_cache::backing_store::BackingStore::new(fd);
        f_cache::copier::copy_to_cache(&bs, std::path::Path::new("tv/Show/S01E01.mkv"), &cache_dest).unwrap();
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Now change the backing mtime — getattr must reflect the current backing stat.
    let new_mtime = filetime::FileTime::from_unix_time(1_600_000_000, 0);
    filetime::set_file_mtime(&backing_path, new_mtime).unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let meta = std::fs::metadata(h.mount_path().join("tv/Show/S01E01.mkv"))
        .expect("getattr through FUSE failed");

    assert_eq!(
        meta.mtime(), 1_600_000_000,
        "getattr should return the current backing file mtime (1_600_000_000)"
    );
}

/// When a file is copied to the SSD cache, its permissions and mtime should
/// match the original backing file. getattr now prefers the cached copy, so
/// correct metadata on the cache is load-bearing, not just defensive.
#[tokio::test]
async fn cached_file_preserves_metadata() {
    let h = FuseHarness::new_with_cache(1.0, 72).expect("FUSE harness with cache failed");

    write_backing_file(&h, "tv/Show/S01E01.mkv", b"episode one content");
    let backing_path = h.backing_path().join("tv/Show/S01E01.mkv");
    std::fs::set_permissions(&backing_path, std::fs::Permissions::from_mode(0o644))
        .expect("set_permissions failed");
    let target_mtime = filetime::FileTime::from_unix_time(1_700_000_000, 500_000_000);
    filetime::set_file_mtime(&backing_path, target_mtime).expect("set_file_mtime failed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let cache_dest = h.cache_path().join("tv/Show/S01E01.mkv");
    {
        // FuseHarness doesn't expose backing_store, so open one ourselves.
        let c = std::ffi::CString::new(h.backing_path().as_os_str().as_bytes()).unwrap();
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY) };
        assert!(fd >= 0, "failed to open backing dir");
        let bs = f_cache::backing_store::BackingStore::new(fd);
        f_cache::copier::copy_to_cache(&bs, std::path::Path::new("tv/Show/S01E01.mkv"), &cache_dest)
            .expect("copy_to_cache failed");
    }

    assert!(cache_dest.exists(), "cached file should exist after copy_to_cache");

    let backing_meta = std::fs::metadata(&backing_path).unwrap();
    let cache_meta = std::fs::metadata(&cache_dest).unwrap();

    assert_eq!(
        cache_meta.permissions().mode() & 0o7777,
        backing_meta.permissions().mode() & 0o7777,
        "cached file permissions should match source"
    );
    // utimensat preserves nanoseconds; second precision is sufficient to assert here.
    assert_eq!(cache_meta.mtime(), backing_meta.mtime(), "cached file mtime should match source");
}
