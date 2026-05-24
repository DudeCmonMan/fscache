mod common;
use std::ffi::CString;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;

use common::{FuseHarness, read_mount_file, write_backing_file};

fn c_path(path: &std::path::Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap()
}

fn c_name(name: &str) -> CString {
    CString::new(name).unwrap()
}

fn set_user_xattr(path: &std::path::Path, name: &str, value: &[u8]) -> std::io::Result<()> {
    let path = c_path(path);
    let name = c_name(name);
    let rc = unsafe {
        libc::setxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn get_user_xattr(path: &std::path::Path, name: &str) -> std::io::Result<Vec<u8>> {
    let path = c_path(path);
    let name = c_name(name);
    let size = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buf = vec![0; size as usize];
    let rc = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        buf.truncate(rc as usize);
        Ok(buf)
    }
}

fn lget_user_xattr(path: &std::path::Path, name: &str) -> std::io::Result<Vec<u8>> {
    let path = c_path(path);
    let name = c_name(name);
    let size = unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buf = vec![0; size as usize];
    let rc = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        buf.truncate(rc as usize);
        Ok(buf)
    }
}

fn renameat2_noreplace(old: &std::path::Path, new: &std::path::Path) -> std::io::Result<()> {
    let old = c_path(old);
    let new = c_path(new);
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            old.as_ptr(),
            libc::AT_FDCWD,
            new.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn list_user_xattrs(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let path = c_path(path);
    let size = unsafe { libc::listxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut buf = vec![0; size as usize];
    let rc = unsafe { libc::listxattr(path.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        buf.truncate(rc as usize);
        Ok(buf)
    }
}

fn remove_user_xattr(path: &std::path::Path, name: &str) -> std::io::Result<()> {
    let path = c_path(path);
    let name = c_name(name);
    let rc = unsafe { libc::removexattr(path.as_ptr(), name.as_ptr()) };
    if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

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

    assert!(
        entries.contains(&"a.txt".to_string()),
        "a.txt missing: {:?}",
        entries
    );
    assert!(
        entries.contains(&"b.txt".to_string()),
        "b.txt missing: {:?}",
        entries
    );
    assert!(
        entries.contains(&"sub".to_string()),
        "sub/ missing: {:?}",
        entries
    );

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

/// Write operations through the FUSE mount are forwarded to the backing store.
#[test]
fn write_passthrough_updates_backing() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "existing.txt", b"original");

    std::thread::sleep(std::time::Duration::from_millis(100));

    std::fs::write(h.mount_path().join("new.txt"), b"created").unwrap();
    assert_eq!(
        std::fs::read(h.backing.path().join("new.txt")).unwrap(),
        b"created"
    );

    std::fs::write(h.mount_path().join("existing.txt"), b"overwrite").unwrap();
    assert_eq!(
        std::fs::read(h.backing.path().join("existing.txt")).unwrap(),
        b"overwrite"
    );
}

/// Namespace mutations through the FUSE mount are forwarded to the backing store.
#[test]
fn namespace_mutations_passthrough() {
    let h = FuseHarness::new().expect("FUSE mount failed");

    let dir = h.mount_path().join("dir");
    std::fs::create_dir(&dir).unwrap();
    assert!(h.backing.path().join("dir").is_dir());

    std::fs::write(dir.join("file.txt"), b"hello").unwrap();
    std::fs::rename(dir.join("file.txt"), h.mount_path().join("renamed.txt")).unwrap();
    assert_eq!(
        std::fs::read(h.backing.path().join("renamed.txt")).unwrap(),
        b"hello"
    );
    assert!(!h.backing.path().join("dir/file.txt").exists());

    std::fs::remove_file(h.mount_path().join("renamed.txt")).unwrap();
    assert!(!h.backing.path().join("renamed.txt").exists());

    std::fs::remove_dir(dir).unwrap();
    assert!(!h.backing.path().join("dir").exists());
}

/// Hardlinks through the FUSE mount are forwarded to the backing store.
#[test]
fn hardlink_passthrough() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "source.txt", b"movie");

    std::thread::sleep(std::time::Duration::from_millis(100));

    std::fs::hard_link(
        h.mount_path().join("source.txt"),
        h.mount_path().join("linked.txt"),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(h.backing.path().join("linked.txt")).unwrap(),
        b"movie"
    );

    let source_meta = std::fs::metadata(h.backing.path().join("source.txt")).unwrap();
    let linked_meta = std::fs::metadata(h.backing.path().join("linked.txt")).unwrap();
    assert_eq!(source_meta.ino(), linked_meta.ino());
}

/// Open flags on write-intent handles are preserved when forwarded to backing.
#[test]
fn write_open_flags_passthrough() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "flags.txt", b"original");

    std::thread::sleep(std::time::Duration::from_millis(100));

    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(h.mount_path().join("flags.txt"))
        .unwrap()
        .write_all(b"short")
        .unwrap();
    assert_eq!(
        std::fs::read(h.backing.path().join("flags.txt")).unwrap(),
        b"short"
    );

    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(h.mount_path().join("exclusive.txt"))
        .unwrap()
        .write_all(b"new")
        .unwrap();
    let exists = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(h.mount_path().join("exclusive.txt"))
        .unwrap_err();
    assert_eq!(exists.kind(), std::io::ErrorKind::AlreadyExists);

    let mut append = std::fs::OpenOptions::new()
        .append(true)
        .read(true)
        .open(h.mount_path().join("exclusive.txt"))
        .unwrap();
    append.seek(SeekFrom::Start(0)).unwrap();
    append.write_all(b"+tail").unwrap();
    drop(append);
    assert_eq!(
        std::fs::read(h.backing.path().join("exclusive.txt")).unwrap(),
        b"new+tail"
    );
}

/// Directory renames update populated child inode path mappings.
#[test]
fn directory_rename_updates_inode_subtree() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "dir/sub/file.txt", b"nested");

    std::thread::sleep(std::time::Duration::from_millis(100));

    assert_eq!(read_mount_file(&h, "dir/sub/file.txt"), b"nested");
    let before = std::fs::metadata(h.mount_path().join("dir/sub/file.txt"))
        .unwrap()
        .ino();

    std::fs::rename(h.mount_path().join("dir"), h.mount_path().join("dir2")).unwrap();

    assert_eq!(read_mount_file(&h, "dir2/sub/file.txt"), b"nested");
    let after = std::fs::metadata(h.mount_path().join("dir2/sub/file.txt"))
        .unwrap()
        .ino();
    assert_eq!(before, after);
    assert_eq!(
        std::fs::metadata(h.mount_path().join("dir/sub/file.txt"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound
    );
}

/// Symlink creation through the FUSE mount is forwarded to backing.
#[test]
fn symlink_create_passthrough() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "target.txt", b"target");

    std::thread::sleep(std::time::Duration::from_millis(100));

    std::os::unix::fs::symlink("target.txt", h.mount_path().join("link.txt")).unwrap();
    assert_eq!(
        std::fs::read_link(h.backing.path().join("link.txt")).unwrap(),
        std::path::PathBuf::from("target.txt")
    );
    assert_eq!(
        std::fs::read(h.mount_path().join("link.txt")).unwrap(),
        b"target"
    );
}

/// xattrs through the FUSE mount round-trip to backing when supported.
#[test]
fn xattr_passthrough_roundtrip() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    std::fs::write(h.mount_path().join("xattr.txt"), b"data").unwrap();

    let attr = "user.fscache_test";
    let value = b"ok";
    match set_user_xattr(&h.mount_path().join("xattr.txt"), attr, value) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => return,
        Err(e) => panic!("setxattr through FUSE failed: {e}"),
    }

    assert_eq!(
        get_user_xattr(&h.backing.path().join("xattr.txt"), attr).unwrap(),
        value
    );
    assert_eq!(
        get_user_xattr(&h.mount_path().join("xattr.txt"), attr).unwrap(),
        value
    );

    let listed = list_user_xattrs(&h.mount_path().join("xattr.txt")).unwrap();
    assert!(
        listed
            .split(|b| *b == 0)
            .any(|entry| entry == attr.as_bytes()),
        "xattr list did not include {attr}: {listed:?}"
    );

    remove_user_xattr(&h.mount_path().join("xattr.txt"), attr).unwrap();
    assert_eq!(
        get_user_xattr(&h.backing.path().join("xattr.txt"), attr)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENODATA)
    );
}

/// Xattr operations on symlinks do not follow to the target inode.
#[test]
fn xattr_symlink_no_follow_passthrough() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "target.txt", b"target");
    std::os::unix::fs::symlink("target.txt", h.backing.path().join("link.txt")).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(100));

    let attr = "user.fscache_target_only";
    match set_user_xattr(&h.backing.path().join("target.txt"), attr, b"target-value") {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => return,
        Err(e) => panic!("setxattr on backing target failed: {e}"),
    }

    assert_eq!(
        lget_user_xattr(&h.mount_path().join("link.txt"), attr)
            .unwrap_err()
            .raw_os_error(),
        Some(libc::ENODATA)
    );
    assert_eq!(
        get_user_xattr(&h.mount_path().join("link.txt"), attr).unwrap(),
        b"target-value"
    );
}

/// Rename flags are forwarded to backing instead of rejected by FUSE.
#[test]
fn rename_noreplace_flag_passthrough() {
    let h = FuseHarness::new().expect("FUSE mount failed");
    write_backing_file(&h, "old.txt", b"old");
    write_backing_file(&h, "existing.txt", b"existing");

    std::thread::sleep(std::time::Duration::from_millis(100));

    let err = renameat2_noreplace(
        &h.mount_path().join("old.txt"),
        &h.mount_path().join("existing.txt"),
    )
    .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EEXIST));
    assert_eq!(
        std::fs::read(h.backing.path().join("old.txt")).unwrap(),
        b"old"
    );
    assert_eq!(
        std::fs::read(h.backing.path().join("existing.txt")).unwrap(),
        b"existing"
    );

    renameat2_noreplace(
        &h.mount_path().join("old.txt"),
        &h.mount_path().join("new.txt"),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(h.backing.path().join("new.txt")).unwrap(),
        b"old"
    );
    assert!(!h.backing.path().join("old.txt").exists());
}
