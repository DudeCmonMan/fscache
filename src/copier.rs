use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

/// Copy `rel_path` from the backing store (via `backing_fd`) to `cache_dest`.
///
/// Writes to `{cache_dest}.partial` during the copy, then atomically renames
/// to `cache_dest` on success. FUSE ignores `.partial` files, so reads fall
/// through to the original backing store until the copy completes.
///
/// This function is synchronous and should be called from `spawn_blocking`.
pub fn copy_to_cache(backing_fd: RawFd, rel_path: &Path, cache_dest: &Path) -> std::io::Result<()> {
    // Ensure destination directory exists.
    if let Some(parent) = cache_dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let partial_path = partial_path(cache_dest);

    // Open source via backing fd.
    let src_fd = open_via_backing(backing_fd, rel_path)?;

    // Open/create the .partial destination file.
    let dst_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&partial_path)?;

    // Copy using sendfile / fallback to read-write loop.
    let result = copy_fd_to_file(src_fd, &dst_file);

    // Always close the source fd.
    unsafe { libc::close(src_fd) };

    result?;

    // Sync before rename so the file is complete on disk.
    dst_file.sync_all()?;
    drop(dst_file);

    // Atomic rename: .partial → final destination.
    std::fs::rename(&partial_path, cache_dest)?;
    tracing::debug!("copy_to_cache: {} -> {}", rel_path.display(), cache_dest.display());
    Ok(())
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".partial");
    PathBuf::from(s)
}

fn open_via_backing(backing_fd: RawFd, rel_path: &Path) -> std::io::Result<RawFd> {
    let bytes = rel_path.as_os_str().as_bytes();
    let bytes = bytes.strip_prefix(b"/").unwrap_or(bytes);
    let c = CString::new(bytes)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid path"))?;
    let fd = unsafe { libc::openat(backing_fd, c.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

/// Copy all bytes from `src_fd` to `dst_file` using a read/write loop.
fn copy_fd_to_file(src_fd: RawFd, dst_file: &std::fs::File) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::io::FromRawFd;

    // Wrap src_fd in a File for safe reading. We close it manually after, so
    // we need to avoid double-close: wrap in ManuallyDrop.
    let mut src = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(src_fd) });
    let mut dst = std::mem::ManuallyDrop::new(
        // Safety: we hold an exclusive reference to dst_file for this call's duration.
        unsafe { std::fs::File::from_raw_fd(std::os::unix::io::IntoRawFd::into_raw_fd(
            dst_file.try_clone()?
        ))}
    );

    let mut buf = vec![0u8; 256 * 1024]; // 256 KiB chunks
    loop {
        use std::io::Read;
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
    }
    Ok(())
}
