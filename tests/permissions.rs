mod common;
use common::FuseHarness;
use std::os::unix::fs::PermissionsExt;

#[test]
fn new_objects_use_caller_umask_only() {
    // The test process is the daemon; mirror the systemd service umask.
    unsafe { libc::umask(0o022) };
    let h = FuseHarness::new().expect("FUSE mount failed");
    let mount = h.mount_path().display().to_string();

    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "umask 002 && mkdir '{mount}/d' && touch '{mount}/d/f' && mkfifo '{mount}/d/p'"
        ))
        .status()
        .unwrap();
    assert!(status.success());

    let mode = |rel: &str| {
        std::fs::symlink_metadata(h.backing_path().join(rel))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode("d"), 0o775);
    assert_eq!(mode("d/f"), 0o664);
    assert_eq!(mode("d/p"), 0o664);
}
