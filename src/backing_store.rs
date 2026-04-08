use std::ffi::{CString, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::RawFd;
use std::path::Path;

use libc::{AT_EMPTY_PATH, AT_SYMLINK_NOFOLLOW};

/// Wraps the O_PATH fd opened to the backing directory before the FUSE overmount.
/// All backing-store I/O goes through this struct.
pub struct BackingStore {
    fd: RawFd,
}

// Safe: fd is an int; all operations (fstatat, openat, readlinkat) are thread-safe.
unsafe impl Send for BackingStore {}
unsafe impl Sync for BackingStore {}

impl BackingStore {
    /// Takes ownership of an already-opened O_PATH directory fd.
    pub fn new(fd: RawFd) -> Self {
        Self { fd }
    }

    /// Raw fd for callers that still need it directly (readlinkat, statfs, opendir).
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    /// stat a path relative to the backing root. Handles empty path (root dir).
    /// Does not follow symlinks for regular paths.
    pub fn stat(&self, rel: &Path) -> Option<libc::stat> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let rc = if rel == Path::new("") {
            let empty = CString::new("").unwrap();
            unsafe { libc::fstatat(self.fd, empty.as_ptr(), &mut stat, AT_EMPTY_PATH) }
        } else {
            let c = rel_to_cstring(rel);
            unsafe { libc::fstatat(self.fd, c.as_ptr(), &mut stat, AT_SYMLINK_NOFOLLOW) }
        };
        if rc == 0 { Some(stat) } else { None }
    }

    /// Open a regular file relative to the backing root for reading.
    /// Returns a raw fd the caller is responsible for closing.
    pub fn open_file(&self, rel: &Path) -> io::Result<RawFd> {
        let c = rel_to_cstring(rel);
        let fd = unsafe { libc::openat(self.fd, c.as_ptr(), libc::O_RDONLY) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(fd)
        }
    }

    /// Return the size in bytes of a file in the backing store, or None on error.
    pub fn file_size(&self, rel: &Path) -> Option<u64> {
        let fd = self.open_file(rel).ok()?;
        let size = unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut stat) == 0 { Some(stat.st_size as u64) } else { None }
        };
        unsafe { libc::close(fd) };
        size
    }

    /// List entry names in a directory relative to the backing root (excludes `.` and `..`).
    pub fn list_dir(&self, rel_dir: &Path) -> Vec<OsString> {
        let c_dir = if rel_dir == Path::new("") {
            CString::new(".").unwrap()
        } else {
            let bytes = rel_dir.as_os_str().as_bytes();
            let bytes = bytes.strip_prefix(b"/").unwrap_or(bytes);
            CString::new(bytes).unwrap_or_else(|_| CString::new(".").unwrap())
        };

        let dir_fd = unsafe {
            libc::openat(self.fd, c_dir.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
        };
        if dir_fd < 0 {
            return vec![];
        }

        let dir = unsafe { libc::fdopendir(dir_fd) };
        if dir.is_null() {
            unsafe { libc::close(dir_fd) };
            return vec![];
        }
        unsafe { libc::rewinddir(dir) };

        let mut out = Vec::new();
        loop {
            unsafe { *libc::__errno_location() = 0 };
            let dirent = unsafe { libc::readdir(dir) };
            if dirent.is_null() {
                break;
            }
            let name_bytes = unsafe {
                std::ffi::CStr::from_ptr((*dirent).d_name.as_ptr())
                    .to_bytes()
                    .to_vec()
            };
            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }
            out.push(OsString::from_vec(name_bytes));
        }
        unsafe { libc::closedir(dir) };
        out
    }
}

impl Drop for BackingStore {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

fn rel_to_cstring(rel: &Path) -> CString {
    let bytes = rel.as_os_str().as_bytes();
    let bytes = bytes.strip_prefix(b"/").unwrap_or(bytes);
    CString::new(bytes).unwrap_or_else(|_| CString::new(".").unwrap())
}
