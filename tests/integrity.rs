mod common;
use common::{collect_files, file_hash, write_backing_file, FuseHarness};
use std::collections::HashMap;

/// Core guarantee: source files in the backing store are NEVER modified, deleted,
/// or created as a side effect of any FUSE read or directory operation.
#[test]
fn source_files_never_modified() {
    let h = FuseHarness::new().expect("FUSE mount failed");

    // Populate backing store with a variety of files
    write_backing_file(&h, "movies/film.mkv", b"film data 1234");
    write_backing_file(&h, "tv/Show/S01E01.mkv", b"episode data 5678");
    write_backing_file(&h, "tv/Show/S01E02.mkv", b"episode data 9012");
    write_backing_file(&h, "music/track.flac", b"audio data 3456");

    std::thread::sleep(std::time::Duration::from_millis(100));

    // Record hashes of all backing files BEFORE any FUSE access
    let before: HashMap<String, String> = collect_files(h.backing_path())
        .iter()
        .map(|p| {
            let rel = p.strip_prefix(h.backing_path()).unwrap();
            (rel.to_string_lossy().to_string(), file_hash(p))
        })
        .collect();

    // Perform a variety of read operations through the FUSE mount
    let _ = std::fs::read_dir(h.mount_path()).unwrap().count();
    let _ = std::fs::read(h.mount_path().join("movies/film.mkv")).unwrap();
    let _ = std::fs::read(h.mount_path().join("tv/Show/S01E01.mkv")).unwrap();
    let _ = std::fs::read_dir(h.mount_path().join("tv/Show")).unwrap().count();
    let _ = std::fs::metadata(h.mount_path().join("music/track.flac")).unwrap();

    // Re-hash all backing files AFTER FUSE access
    let after: HashMap<String, String> = collect_files(h.backing_path())
        .iter()
        .map(|p| {
            let rel = p.strip_prefix(h.backing_path()).unwrap();
            (rel.to_string_lossy().to_string(), file_hash(p))
        })
        .collect();

    assert_eq!(
        before, after,
        "source files were modified by FUSE operations"
    );
}

/// No extra files are created in the backing store by FUSE operations.
#[test]
fn no_files_created_in_backing() {
    let h = FuseHarness::new().expect("FUSE mount failed");

    write_backing_file(&h, "tv/Show/S01E01.mkv", b"episode one");

    std::thread::sleep(std::time::Duration::from_millis(100));

    let before_count = collect_files(h.backing_path()).len();

    // Read through FUSE
    let _ = std::fs::read(h.mount_path().join("tv/Show/S01E01.mkv")).unwrap();
    let _ = std::fs::read_dir(h.mount_path()).unwrap().count();

    let after_count = collect_files(h.backing_path()).len();
    assert_eq!(
        before_count, after_count,
        "FUSE operations created files in the backing store"
    );
}

/// Dropping the FUSE session cleanly unmounts and restores access to backing files.
#[test]
fn unmount_restores_backing_access() {
    let backing_dir = tempfile::TempDir::new().unwrap();
    let mount_dir = tempfile::TempDir::new().unwrap();

    // Write a file to the backing dir
    let backing_file = backing_dir.path().join("test.txt");
    std::fs::write(&backing_file, b"accessible after unmount").unwrap();

    // Mount and drop
    {
        use fuser::{MountOption, SessionACL};
        use fscache::fuse_fs::FsCache;

        let fs = FsCache::new(backing_dir.path()).unwrap();
        let mut config = fuser::Config::default();
        config.mount_options = vec![MountOption::RO, MountOption::FSName("test".to_string())];
        config.acl = SessionACL::Owner;
        let _session = fuser::spawn_mount2(fs, mount_dir.path(), &config).unwrap();
        // _session drops here → FUSE unmounts
    }

    std::thread::sleep(std::time::Duration::from_millis(200));

    // Backing files are still intact and readable directly
    let content = std::fs::read(&backing_file).unwrap();
    assert_eq!(content, b"accessible after unmount");
}
