use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

use crate::fuse::util::rel_to_cstring;

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

    /// Returns a raw fd the caller is responsible for closing.
    pub fn open_file(&self, rel: &Path) -> io::Result<RawFd> {
        self.open_file_with_flags(rel, libc::O_RDONLY, 0)
    }

    /// Returns a raw fd the caller is responsible for closing.
    pub fn open_file_with_flags(&self, rel: &Path, flags: i32, mode: u32) -> io::Result<RawFd> {
        let c = rel_to_cstring(rel);
        let fd = unsafe { libc::openat(self.fd, c.as_ptr(), flags | libc::O_CLOEXEC, mode) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(fd)
        }
    }

    pub fn mkdir(&self, rel: &Path, mode: u32) -> io::Result<()> {
        let c = rel_to_cstring(rel);
        syscall_unit(unsafe { libc::mkdirat(self.fd, c.as_ptr(), mode) })
    }

    pub fn unlink_file(&self, rel: &Path) -> io::Result<()> {
        let c = rel_to_cstring(rel);
        syscall_unit(unsafe { libc::unlinkat(self.fd, c.as_ptr(), 0) })
    }

    pub fn rmdir(&self, rel: &Path) -> io::Result<()> {
        let c = rel_to_cstring(rel);
        syscall_unit(unsafe { libc::unlinkat(self.fd, c.as_ptr(), libc::AT_REMOVEDIR) })
    }

    pub fn rename(&self, old: &Path, new: &Path, flags: u32) -> io::Result<()> {
        let old = rel_to_cstring(old);
        let new = rel_to_cstring(new);
        if flags == 0 {
            return syscall_unit(unsafe {
                libc::renameat(self.fd, old.as_ptr(), self.fd, new.as_ptr())
            });
        }
        #[cfg(target_os = "linux")]
        {
            syscall_unit(unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    self.fd,
                    old.as_ptr(),
                    self.fd,
                    new.as_ptr(),
                    flags,
                ) as libc::c_int
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(io::Error::from_raw_os_error(libc::EINVAL))
        }
    }

    pub fn link(&self, old: &Path, new: &Path) -> io::Result<()> {
        let old = rel_to_cstring(old);
        let new = rel_to_cstring(new);
        syscall_unit(unsafe { libc::linkat(self.fd, old.as_ptr(), self.fd, new.as_ptr(), 0) })
    }

    pub fn symlink(&self, target: &OsStr, link_rel: &Path) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let target = CString::new(target.as_bytes())?;
        let link_rel = rel_to_cstring(link_rel);
        syscall_unit(unsafe { libc::symlinkat(target.as_ptr(), self.fd, link_rel.as_ptr()) })
    }

    pub fn mknod(&self, rel: &Path, mode: u32, rdev: u32) -> io::Result<()> {
        let rel = rel_to_cstring(rel);
        syscall_unit(unsafe { libc::mknodat(self.fd, rel.as_ptr(), mode, rdev as libc::dev_t) })
    }

    pub fn chmod(&self, rel: &Path, mode: u32) -> io::Result<()> {
        let rel = rel_to_cstring(rel);
        // Linux fchmodat does not support AT_SYMLINK_NOFOLLOW without fchmodat2;
        // follow symlinks here to match chmod(2) behavior.
        syscall_unit(unsafe { libc::fchmodat(self.fd, rel.as_ptr(), mode, 0) })
    }

    pub fn chown(&self, rel: &Path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()> {
        let rel = rel_to_cstring(rel);
        syscall_unit(unsafe {
            libc::fchownat(
                self.fd,
                rel.as_ptr(),
                uid.map(|v| v as libc::uid_t).unwrap_or(!0),
                gid.map(|v| v as libc::gid_t).unwrap_or(!0),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })
    }

    pub fn truncate(&self, rel: &Path, size: u64) -> io::Result<()> {
        let fd = self.open_file_with_flags(rel, libc::O_WRONLY, 0)?;
        let rc = unsafe { libc::ftruncate(fd, size as libc::off_t) };
        let err = if rc == 0 {
            None
        } else {
            Some(io::Error::last_os_error())
        };
        unsafe { libc::close(fd) };
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    pub fn utimens(
        &self,
        rel: &Path,
        atime: libc::timespec,
        mtime: libc::timespec,
    ) -> io::Result<()> {
        let rel = rel_to_cstring(rel);
        let times = [atime, mtime];
        syscall_unit(unsafe {
            libc::utimensat(
                self.fd,
                rel.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })
    }

    pub fn backing_proc_path(&self, rel: &Path) -> PathBuf {
        Path::new("/proc/self/fd")
            .join(self.fd.to_string())
            .join(rel)
    }

    pub fn setxattr(&self, rel: &Path, name: &OsStr, value: &[u8], flags: i32) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let path = path_to_cstring(&self.backing_proc_path(rel))?;
        let name = CString::new(name.as_bytes())?;
        syscall_unit(unsafe {
            libc::lsetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                flags,
            )
        })
    }

    pub fn getxattr(&self, rel: &Path, name: &OsStr, size: u32) -> io::Result<Vec<u8>> {
        use std::os::unix::ffi::OsStrExt;
        let path = path_to_cstring(&self.backing_proc_path(rel))?;
        let name = CString::new(name.as_bytes())?;
        let mut buf = vec![0; size as usize];
        let ptr = if size == 0 {
            std::ptr::null_mut()
        } else {
            buf.as_mut_ptr().cast()
        };
        let rc = unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), ptr, buf.len()) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else if size == 0 {
            Ok(vec![0; rc as usize])
        } else {
            buf.truncate(rc as usize);
            Ok(buf)
        }
    }

    pub fn listxattr(&self, rel: &Path, size: u32) -> io::Result<Vec<u8>> {
        let path = path_to_cstring(&self.backing_proc_path(rel))?;
        let mut buf = vec![0; size as usize];
        let ptr = if size == 0 {
            std::ptr::null_mut()
        } else {
            buf.as_mut_ptr().cast()
        };
        let rc = unsafe { libc::llistxattr(path.as_ptr(), ptr, buf.len()) };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else if size == 0 {
            Ok(vec![0; rc as usize])
        } else {
            buf.truncate(rc as usize);
            Ok(buf)
        }
    }

    pub fn removexattr(&self, rel: &Path, name: &OsStr) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let path = path_to_cstring(&self.backing_proc_path(rel))?;
        let name = CString::new(name.as_bytes())?;
        syscall_unit(unsafe { libc::lremovexattr(path.as_ptr(), name.as_ptr()) })
    }

    pub fn file_size(&self, rel: &Path) -> Option<u64> {
        let fd = self.open_file(rel).ok()?;
        let size = unsafe {
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut stat) == 0 {
                Some(stat.st_size as u64)
            } else {
                None
            }
        };
        unsafe { libc::close(fd) };
        size
    }

    pub fn is_dir(&self, rel: &Path) -> bool {
        self.stat(rel)
            .map(|s| (s.st_mode & libc::S_IFMT) == libc::S_IFDIR)
            .unwrap_or(false)
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

        let dir_fd =
            unsafe { libc::openat(self.fd, c_dir.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
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

fn syscall_unit(rc: i32) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn path_to_cstring(path: &Path) -> io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    Ok(CString::new(path.as_os_str().as_bytes())?)
}
