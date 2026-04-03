use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use fuser::{
    Errno, FileAttr, FileType, FileHandle, FopenFlags, Generation, INodeNo, KernelConfig,
    OpenFlags, LockOwner, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, Request,
};
use libc::{AT_EMPTY_PATH, AT_SYMLINK_NOFOLLOW};

use crate::cache::CacheManager;
use crate::inode::InodeTable;
use crate::predictor::AccessEvent;

/// Short TTL so the kernel re-checks after a cache file appears.
const TTL: Duration = Duration::from_secs(1);

pub struct PlexHotCacheFs {
    /// O_PATH fd opened to target_directory *before* the FUSE overmount.
    /// All backing-store access uses openat(backing_fd, relative_path, ...).
    pub backing_fd: RawFd,
    inodes: Arc<Mutex<InodeTable>>,
    pub passthrough_mode: bool,
    /// Optional SSD cache overlay.  When set, `open()` checks this directory
    /// first and serves cached files from SSD when available.
    pub cache: Option<Arc<CacheManager>>,
    /// Channel to send access events to the predictor task.
    pub access_tx: Option<tokio::sync::mpsc::UnboundedSender<AccessEvent>>,
}

impl PlexHotCacheFs {
    /// Opens an O_PATH fd to `backing_path`.
    /// MUST be called before mounting FUSE over that path.
    pub fn new(backing_path: &Path) -> anyhow::Result<Self> {
        let c_path = CString::new(backing_path.as_os_str().as_bytes())
            .map_err(|_| anyhow::anyhow!("invalid backing path"))?;

        let fd = unsafe {
            libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_DIRECTORY)
        };
        if fd < 0 {
            return Err(anyhow::anyhow!(
                "failed to open backing path {}: {}",
                backing_path.display(),
                std::io::Error::last_os_error()
            ));
        }

        tracing::debug!("Opened O_PATH fd {} for {}", fd, backing_path.display());
        Ok(Self {
            backing_fd: fd,
            inodes: Arc::new(Mutex::new(InodeTable::new())),
            passthrough_mode: false,
            cache: None,
            access_tx: None,
        })
    }

    // ---- backing store helpers ----

    /// Stat a relative path against the backing fd.
    /// Empty path stats the backing fd itself (the root directory).
    fn stat_backing(&self, rel_path: &Path) -> Option<libc::stat> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let rc = if rel_path == Path::new("") {
            let empty = CString::new("").unwrap();
            unsafe {
                libc::fstatat(self.backing_fd, empty.as_ptr(), &mut stat, AT_EMPTY_PATH)
            }
        } else {
            let c = path_to_cstring(rel_path);
            unsafe {
                libc::fstatat(self.backing_fd, c.as_ptr(), &mut stat, AT_SYMLINK_NOFOLLOW)
            }
        };
        if rc == 0 { Some(stat) } else { None }
    }

    fn stat_to_attr(&self, ino: u64, s: &libc::stat) -> FileAttr {
        let kind = match s.st_mode & libc::S_IFMT {
            libc::S_IFDIR => FileType::Directory,
            libc::S_IFLNK => FileType::Symlink,
            _ => FileType::RegularFile,
        };
        FileAttr {
            ino: INodeNo(ino),
            size: s.st_size as u64,
            blocks: s.st_blocks as u64,
            atime: UNIX_EPOCH + Duration::from_secs(s.st_atime as u64),
            mtime: UNIX_EPOCH + Duration::from_secs(s.st_mtime as u64),
            ctime: UNIX_EPOCH + Duration::from_secs(s.st_ctime as u64),
            crtime: UNIX_EPOCH,
            kind,
            perm: (s.st_mode & 0o7777) as u16,
            nlink: s.st_nlink as u32,
            uid: s.st_uid,
            gid: s.st_gid,
            rdev: s.st_rdev as u32,
            blksize: s.st_blksize as u32,
            flags: 0,
        }
    }

    /// Read all directory entries via the directory fd.
    /// Returns (name, ino, kind) including "." and "..".
    fn list_dir_entries(
        &self,
        dir_fd: RawFd,
        parent_path: &Path,
    ) -> Vec<(std::ffi::OsString, u64, FileType)> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut entries: Vec<(OsString, u64, FileType)> = Vec::new();

        // "." and ".."
        let dot_ino = self.inodes.lock().unwrap()
            .get_path_ino(parent_path)
            .unwrap_or(InodeTable::root_ino().0);
        entries.push((OsString::from("."), dot_ino, FileType::Directory));

        let dotdot_path = parent_path.parent().unwrap_or(Path::new(""));
        let dotdot_ino = self.inodes.lock().unwrap()
            .get_path_ino(dotdot_path)
            .unwrap_or(InodeTable::root_ino().0);
        entries.push((OsString::from(".."), dotdot_ino, FileType::Directory));

        // fdopendir takes ownership of the fd, so dup first
        let dir = unsafe { libc::fdopendir(libc::dup(dir_fd)) };
        if dir.is_null() {
            tracing::warn!("fdopendir failed: {}", std::io::Error::last_os_error());
            return entries;
        }
        unsafe { libc::rewinddir(dir) };

        loop {
            unsafe { *libc::__errno_location() = 0 };
            let dirent = unsafe { libc::readdir(dir) };
            if dirent.is_null() {
                break;
            }

            let name_bytes = unsafe {
                CStr::from_ptr((*dirent).d_name.as_ptr())
                    .to_bytes()
                    .to_vec()
            };
            if name_bytes == b"." || name_bytes == b".." {
                continue;
            }

            let name_os = OsString::from_vec(name_bytes);
            let child_path = if parent_path == Path::new("") {
                PathBuf::from(&name_os)
            } else {
                parent_path.join(&name_os)
            };

            let kind = match unsafe { (*dirent).d_type } {
                libc::DT_DIR => FileType::Directory,
                libc::DT_LNK => FileType::Symlink,
                libc::DT_UNKNOWN => match self.stat_backing(&child_path) {
                    Some(s) if s.st_mode & libc::S_IFMT == libc::S_IFDIR => FileType::Directory,
                    _ => FileType::RegularFile,
                },
                _ => FileType::RegularFile,
            };

            let ino = self.inodes.lock().unwrap().get_or_create(&child_path);
            entries.push((name_os, ino, kind));
        }

        unsafe { libc::closedir(dir) };
        entries
    }
}

impl Drop for PlexHotCacheFs {
    fn drop(&mut self) {
        unsafe { libc::close(self.backing_fd) };
    }
}

impl Filesystem for PlexHotCacheFs {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::io::Result<()> {
        tracing::info!("FUSE filesystem initialized");
        Ok(())
    }

    fn destroy(&mut self) {
        tracing::info!("FUSE filesystem destroyed");
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent_path = match self.inodes.lock().unwrap().get_path(parent.0) {
            Some(p) => p.to_path_buf(),
            None => { reply.error(Errno::ENOENT); return; }
        };

        let child_path = if parent_path == Path::new("") {
            PathBuf::from(name)
        } else {
            parent_path.join(name)
        };

        let Some(stat) = self.stat_backing(&child_path) else {
            reply.error(Errno::ENOENT);
            return;
        };

        let ino = self.inodes.lock().unwrap().get_or_create(&child_path);
        let attr = self.stat_to_attr(ino, &stat);
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.inodes.lock().unwrap().forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let path = match self.inodes.lock().unwrap().get_path(ino.0) {
            Some(p) => p.to_path_buf(),
            None => { reply.error(Errno::ENOENT); return; }
        };
        let Some(stat) = self.stat_backing(&path) else {
            reply.error(Errno::ENOENT);
            return;
        };
        reply.attr(&TTL, &self.stat_to_attr(ino.0, &stat));
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        // Reject write operations — this is a read-only filesystem
        if flags.acc_mode() != fuser::OpenAccMode::O_RDONLY {
            reply.error(Errno::EACCES);
            return;
        }

        let path = match self.inodes.lock().unwrap().get_path(ino.0) {
            Some(p) => p.to_path_buf(),
            None => { reply.error(Errno::ENOENT); return; }
        };

        // Emit access event for the predictor (fire-and-forget).
        if let Some(ref tx) = self.access_tx {
            let _ = tx.send(AccessEvent {
                relative_path: path.clone(),
                timestamp: std::time::SystemTime::now(),
            });
        }

        // Cache overlay: serve from SSD if a complete cached copy exists.
        if !self.passthrough_mode {
            if let Some(ref cache) = self.cache {
                if cache.is_cached(&path) {
                    let cache_path = cache.cache_path(&path);
                    let c = path_to_cstring_abs(&cache_path);
                    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY) };
                    if fd >= 0 {
                        tracing::debug!("open ino={} path={:?} => cache hit fd={}", ino.0, path, fd);
                        reply.opened(FileHandle(fd as u64), FopenFlags::empty());
                        return;
                    }
                    // Cache file vanished between check and open — fall through to backing store.
                    tracing::warn!("cache hit race for {:?}, falling back to backing store", path);
                }
            }
        }

        // Backing store passthrough.
        let c_path = path_to_cstring(&path);
        let fd = unsafe { libc::openat(self.backing_fd, c_path.as_ptr(), libc::O_RDONLY) };

        if fd < 0 {
            reply.error(Errno::from_i32(last_errno()));
            return;
        }

        tracing::debug!("open ino={} path={:?} fd={}", ino.0, path, fd);
        reply.opened(FileHandle(fd as u64), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let mut buf = vec![0u8; size as usize];
        let n = unsafe {
            libc::pread(
                fh.0 as RawFd,
                buf.as_mut_ptr() as *mut libc::c_void,
                size as libc::size_t,
                offset as libc::off_t,
            )
        };
        if n < 0 {
            reply.error(Errno::from_i32(last_errno()));
        } else {
            buf.truncate(n as usize);
            reply.data(&buf);
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        unsafe { libc::close(fh.0 as RawFd) };
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let path = match self.inodes.lock().unwrap().get_path(ino.0) {
            Some(p) => p.to_path_buf(),
            None => { reply.error(Errno::ENOENT); return; }
        };

        // O_PATH fds can't be used with fdopendir, so always open via openat with real flags.
        // For root, open "." relative to the backing fd.
        let c_path = if path == Path::new("") {
            CString::new(".").unwrap()
        } else {
            path_to_cstring(&path)
        };
        let fd = unsafe {
            libc::openat(self.backing_fd, c_path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
        };

        if fd < 0 {
            reply.error(Errno::from_i32(last_errno()));
            return;
        }

        reply.opened(FileHandle(fd as u64), FopenFlags::empty());
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let parent_path = match self.inodes.lock().unwrap().get_path(ino.0) {
            Some(p) => p.to_path_buf(),
            None => { reply.error(Errno::ENOENT); return; }
        };

        let entries = self.list_dir_entries(fh.0 as RawFd, &parent_path);

        for (i, (name, entry_ino, kind)) in entries.iter().enumerate() {
            let next_offset = (i + 1) as u64;
            if next_offset <= offset {
                continue; // already returned in a prior readdir call
            }
            if reply.add(INodeNo(*entry_ino), next_offset, *kind, name) {
                break; // reply buffer full
            }
        }

        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        unsafe { libc::close(fh.0 as RawFd) };
        reply.ok();
    }
}

// ---- helpers ----

fn path_to_cstring(path: &Path) -> CString {
    let bytes = path.as_os_str().as_bytes();
    let bytes = bytes.strip_prefix(b"/").unwrap_or(bytes);
    CString::new(bytes).unwrap_or_else(|_| CString::new(".").unwrap())
}

/// CString from an absolute path (preserves leading `/`).
fn path_to_cstring_abs(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes())
        .unwrap_or_else(|_| CString::new("/dev/null").unwrap())
}

fn last_errno() -> libc::c_int {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}
