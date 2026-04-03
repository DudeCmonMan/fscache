mod common;
use common::{write_backing_file, read_mount_file, FuseHarness};

/// Files read through the FUSE mount must match the backing store byte-for-byte.
#[test]
fn file_content_matches() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "hello.txt", b"hello world");

    // Give FUSE a moment to settle
    std::thread::sleep(std::time::Duration::from_millis(100));

    let got = read_mount_file(&h, "hello.txt");
    assert_eq!(got, b"hello world");
}

/// Large binary content is passed through correctly.
#[test]
fn large_file_content_matches() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    let data: Vec<u8> = (0..=255u8).cycle().take(4 * 1024 * 1024).collect(); // 4 MB
    write_backing_file(&h, "big.bin", &data);

    std::thread::sleep(std::time::Duration::from_millis(100));

    let got = read_mount_file(&h, "big.bin");
    assert_eq!(got, data);
}

/// Directory listing through FUSE matches the backing directory.
#[test]
fn directory_listing_matches() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "a.txt", b"a");
    write_backing_file(&h, "b.txt", b"b");
    write_backing_file(&h, "sub/c.txt", b"c");

    std::thread::sleep(std::time::Duration::from_millis(100));

    // Root listing should contain a.txt, b.txt, sub
    let mut entries: Vec<_> = std::fs::read_dir(h.mount_path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    entries.sort();

    assert!(entries.contains(&"a.txt".to_string()), "a.txt missing: {:?}", entries);
    assert!(entries.contains(&"b.txt".to_string()), "b.txt missing: {:?}", entries);
    assert!(entries.contains(&"sub".to_string()), "sub/ missing: {:?}", entries);

    // Sub-directory should contain c.txt
    let sub_entries: Vec<_> = std::fs::read_dir(h.mount_path().join("sub"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().into_string().unwrap())
        .collect();
    assert!(sub_entries.contains(&"c.txt".to_string()));
}

/// Nested directory file contents are correct.
#[test]
fn nested_file_content_matches() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "tv/Show/Season 01/S01E01.mkv", b"episode one data");
    write_backing_file(&h, "tv/Show/Season 01/S01E02.mkv", b"episode two data");

    std::thread::sleep(std::time::Duration::from_millis(100));

    assert_eq!(
        read_mount_file(&h, "tv/Show/Season 01/S01E01.mkv"),
        b"episode one data"
    );
    assert_eq!(
        read_mount_file(&h, "tv/Show/Season 01/S01E02.mkv"),
        b"episode two data"
    );
}

/// File metadata (size) reported through FUSE matches the real file.
#[test]
fn file_metadata_matches() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    let content = b"some content here";
    write_backing_file(&h, "meta.txt", content);

    std::thread::sleep(std::time::Duration::from_millis(100));

    let meta = std::fs::metadata(h.mount_path().join("meta.txt")).unwrap();
    assert_eq!(meta.len(), content.len() as u64);
}

/// Write operations through the FUSE mount must be rejected.
#[test]
fn write_is_rejected() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "existing.txt", b"original");

    std::thread::sleep(std::time::Duration::from_millis(100));

    let result = std::fs::write(h.mount_path().join("new.txt"), b"should fail");
    assert!(result.is_err(), "write should have been rejected");

    let result = std::fs::write(h.mount_path().join("existing.txt"), b"overwrite");
    assert!(result.is_err(), "overwrite should have been rejected");
}
