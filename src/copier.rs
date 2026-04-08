use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

use crate::backing_store::BackingStore;

/// Writes to `{cache_dest}.partial` then atomically renames on success.
/// FUSE ignores `.partial` files, so reads fall through to the backing store until complete.
pub fn copy_to_cache(bs: &BackingStore, rel_path: &Path, cache_dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = cache_dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let partial = partial_path(cache_dest);
    let src_fd = bs.open_file(rel_path)?;

    let file_size_bytes = unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        if libc::fstat(src_fd, &mut stat) == 0 { stat.st_size as u64 } else { 0 }
    };
    tracing::info!(
        "copy starting: {} ({:.1} MB)",
        rel_path.display(),
        file_size_bytes as f64 / 1_048_576.0
    );

    let started = std::time::Instant::now();

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

    let src_stat: Option<libc::stat> = if copy_result.is_ok() {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(src_fd, &mut stat) } == 0 { Some(stat) } else { None }
    } else {
        None
    };

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

    // Preserve source permissions and mtime so getattr returns consistent metadata.
    // LRU tracking is now handled by CacheDb (mark_cached / mark_hit) rather than atime.
    if let Some(ref st) = src_stat {
        if let Ok(c) = CString::new(partial.as_os_str().as_bytes()) {
            unsafe {
                libc::chmod(c.as_ptr(), st.st_mode & 0o7777 as libc::mode_t);
                libc::lchown(c.as_ptr(), st.st_uid, st.st_gid); // no-op if not root
                let times = [
                    libc::timespec { tv_sec: st.st_atime, tv_nsec: st.st_atime_nsec },
                    libc::timespec { tv_sec: st.st_mtime, tv_nsec: st.st_mtime_nsec },
                ];
                libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0);
            }
        }
    }

    if let Err(e) = std::fs::rename(&partial, cache_dest) {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }

    let elapsed = started.elapsed();
    tracing::info!(
        "copy complete: {} ({:.1} MB in {:.1}s)",
        rel_path.display(),
        file_size_bytes as f64 / 1_048_576.0,
        elapsed.as_secs_f64()
    );
    Ok(())
}

fn partial_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".partial");
    PathBuf::from(s)
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
