use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

/// Copy `rel_path` from the backing store (via `backing_fd`) to `cache_dest`.
///
/// Writes to `{cache_dest}.partial` during the copy, then atomically renames to
/// `cache_dest` on success. FUSE ignores `.partial` files, so reads fall through
/// to the original backing store until the copy completes.
///
/// This function is synchronous and intended for use with `spawn_blocking`.
pub fn copy_to_cache(backing_fd: RawFd, rel_path: &Path, cache_dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = cache_dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let partial = partial_path(cache_dest);
    let src_fd = open_via_backing(backing_fd, rel_path)?;

    let mut dst_file = match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&partial)
    {
        Ok(f) => f,
        Err(e) => {
            unsafe { libc::close(src_fd) };
            return Err(e);
        }
    };

    let copy_result = copy_by_pread(src_fd, &mut dst_file);
    unsafe { libc::close(src_fd) };

    if let Err(e) = copy_result {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }

    if let Err(e) = dst_file.sync_all() {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    drop(dst_file);

    if let Err(e) = std::fs::rename(&partial, cache_dest) {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }

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

/// Copy all bytes from `src_fd` to `dst_file` using pread, advancing offset manually.
/// Avoids wrapping src_fd in File (which would cause a double-close).
fn copy_by_pread(src_fd: RawFd, dst_file: &mut std::fs::File) -> std::io::Result<()> {
    use std::io::Write;
    let mut buf = vec![0u8; 256 * 1024];
    let mut offset: libc::off_t = 0;
    loop {
        let n = unsafe {
            libc::pread(
                src_fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len() as libc::size_t,
                offset,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if n == 0 {
            break;
        }
        dst_file.write_all(&buf[..n as usize])?;
        offset += n as libc::off_t;
    }
    Ok(())
}
