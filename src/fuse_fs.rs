use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TriggerStrategy {
    /// Fire an AccessEvent only on a cache miss. Hits mean lookahead is working.
    CacheMissOnly,
    /// Fire an AccessEvent on every access, keeping the lookahead window topped up.
    RollingBuffer,
}

pub struct PlexHotCacheFs {
    /// O_PATH fd opened to target_directory *before* the FUSE overmount.
    pub backing_fd: RawFd,
    inodes: Arc<Mutex<InodeTable>>,
    pub passthrough_mode: bool,
    pub cache: Option<Arc<CacheManager>>,
    pub access_tx: Option<tokio::sync::mpsc::UnboundedSender<AccessEvent>>,
    pub repeat_log_window: Duration,
    pub trigger_strategy: TriggerStrategy,
    recent_logs: Mutex<HashMap<PathBuf, Instant>>,
}

impl PlexHotCacheFs {
    /// MUST be called before mounting FUSE over `backing_path`.
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
            repeat_log_window: Duration::from_secs(60),
            trigger_strategy: TriggerStrategy::CacheMissOnly,
            recent_logs: Mutex::new(HashMap::new()),
        })
    }

    /// Returns true if this path was already logged at INFO within the repeat window.
    /// On first call (or after the window expires), records the timestamp and returns false.
    pub fn should_suppress_log(&self, path: &Path) -> bool {
        if self.repeat_log_window.is_zero() {
            return false;
        }
        let now = Instant::now();
        let mut recent = self.recent_logs.lock().unwrap();
        if recent.len() > 1000 {
            recent.retain(|_, last| now.duration_since(*last) < self.repeat_log_window);
        }
        match recent.get(path) {
            Some(&last) if now.duration_since(last) < self.repeat_log_window => true,
            _ => {
                recent.insert(path.to_path_buf(), now);
                false
            }
        }
    }

    // ---- backing store helpers ----

    /// Directories are never cached, so this naturally falls through to stat_backing for them.
    fn stat_cached(&self, rel_path: &Path) -> Option<libc::stat> {
        if self.passthrough_mode {
            return None;
        }
        let cache = self.cache.as_ref()?;
        if !cache.is_cached(rel_path) {
            return None;
        }
        let cache_path = cache.cache_path(rel_path);
        let c = path_to_cstring_abs(&cache_path);
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::lstat(c.as_ptr(), &mut stat) };
        if rc == 0 { Some(stat) } else { None }
    }

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
        let kind = mode_to_filetype(s.st_mode);
        FileAttr {
            ino: INodeNo(ino),
            size: s.st_size as u64,
            blocks: s.st_blocks as u64,
            atime: UNIX_EPOCH + Duration::new(s.st_atime as u64, s.st_atime_nsec as u32),
            mtime: UNIX_EPOCH + Duration::new(s.st_mtime as u64, s.st_mtime_nsec as u32),
            ctime: UNIX_EPOCH + Duration::new(s.st_ctime as u64, s.st_ctime_nsec as u32),
            crtime: UNIX_EPOCH, // Linux doesn't expose birth time via stat(2); macOS-only field
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

    fn list_dir_entries(
        &self,
        dir_fd: RawFd,
        parent_path: &Path,
    ) -> Vec<(std::ffi::OsString, u64, FileType)> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut entries: Vec<(OsString, u64, FileType)> = Vec::new();

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
                libc::DT_DIR  => FileType::Directory,
                libc::DT_LNK  => FileType::Symlink,
                libc::DT_BLK  => FileType::BlockDevice,
                libc::DT_CHR  => FileType::CharDevice,
                libc::DT_FIFO => FileType::NamedPipe,
                libc::DT_SOCK => FileType::Socket,
                libc::DT_UNKNOWN | _ => match self.stat_backing(&child_path) {
                    Some(s) => mode_to_filetype(s.st_mode),
                    None    => FileType::RegularFile,
                },
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

        let Some(stat) = self.stat_cached(&child_path).or_else(|| self.stat_backing(&child_path)) else {
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
        // Avoids waking backing drives (spinning rust, NFS) for files already on SSD.
        let Some(stat) = self.stat_cached(&path).or_else(|| self.stat_backing(&path)) else {
            reply.error(Errno::ENOENT);
            return;
        };
        reply.attr(&TTL, &self.stat_to_attr(ino.0, &stat));
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != fuser::OpenAccMode::O_RDONLY {
            reply.error(Errno::EACCES);
            return;
        }

        let path = match self.inodes.lock().unwrap().get_path(ino.0) {
            Some(p) => p.to_path_buf(),
            None => { reply.error(Errno::ENOENT); return; }
        };

        let suppress = self.should_suppress_log(&path);

        // RollingBuffer: notify predictor on every access before the cache check.
        if self.trigger_strategy == TriggerStrategy::RollingBuffer {
            if let Some(ref tx) = self.access_tx {
                if suppress {
                    tracing::debug!("plex access: {:?}", path);
                } else {
                    tracing::info!("plex access: {:?}", path);
                }
                let _ = tx.send(AccessEvent { relative_path: path.clone() });
            }
        }

        if !self.passthrough_mode {
            if let Some(ref cache) = self.cache {
                if cache.is_cached(&path) {
                    let cache_path = cache.cache_path(&path);
                    let c = path_to_cstring_abs(&cache_path);
                    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY) };
                    if fd >= 0 {
                        if suppress {
                            tracing::debug!("cache HIT: {:?} (serving from SSD)", path);
                        } else {
                            tracing::info!("cache HIT: {:?} (serving from SSD)", path);
                        }
                        reply.opened(FileHandle(fd as u64), FopenFlags::empty());
                        return;
                    }
                    // Cache file vanished between check and open — fall through to backing store.
                    tracing::warn!("cache hit race for {:?}, falling back to backing store", path);
                }
            }
        }

        let c_path = path_to_cstring(&path);
        let fd = unsafe { libc::openat(self.backing_fd, c_path.as_ptr(), libc::O_RDONLY) };

        if fd < 0 {
            reply.error(Errno::from_i32(last_errno()));
            return;
        }

        if suppress {
            tracing::debug!("cache MISS: {:?} (serving from backing store)", path);
        } else {
            tracing::info!("cache MISS: {:?} (serving from backing store)", path);
        }

        // CacheMissOnly: notify predictor only on miss — hits mean lookahead is working.
        if self.trigger_strategy == TriggerStrategy::CacheMissOnly {
            if let Some(ref tx) = self.access_tx {
                let _ = tx.send(AccessEvent { relative_path: path.clone() });
            }
        }

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
                continue;
            }
            if reply.add(INodeNo(*entry_ino), next_offset, *kind, name) {
                break;
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

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let path = match self.inodes.lock().unwrap().get_path(ino.0) {
            Some(p) => p.to_path_buf(),
            None => { reply.error(Errno::ENOENT); return; }
        };

        let c_path = path_to_cstring(&path);
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        let len = unsafe {
            libc::readlinkat(
                self.backing_fd,
                c_path.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };
        if len < 0 {
            reply.error(Errno::from_i32(last_errno()));
        } else {
            buf.truncate(len as usize);
            reply.data(&buf);
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
        // backing_fd is O_PATH and cannot be used with fstatvfs directly.
        // Open a real fd to "." relative to the backing dir for the call.
        let dot = CString::new(".").unwrap();
        let real_fd = unsafe {
            libc::openat(self.backing_fd, dot.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY)
        };
        if real_fd < 0 {
            reply.error(Errno::from_i32(last_errno()));
            return;
        }
        let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstatvfs(real_fd, &mut buf) };
        unsafe { libc::close(real_fd) };
        if rc != 0 {
            reply.error(Errno::from_i32(last_errno()));
            return;
        }
        reply.statfs(
            buf.f_blocks,
            buf.f_bfree,
            buf.f_bavail,
            buf.f_files,
            buf.f_ffree,
            buf.f_bsize as u32,
            buf.f_namemax as u32,
            buf.f_frsize as u32,
        );
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

fn mode_to_filetype(mode: libc::mode_t) -> FileType {
    match mode & libc::S_IFMT {
        libc::S_IFDIR  => FileType::Directory,
        libc::S_IFLNK  => FileType::Symlink,
        libc::S_IFBLK  => FileType::BlockDevice,
        libc::S_IFCHR  => FileType::CharDevice,
        libc::S_IFIFO  => FileType::NamedPipe,
        libc::S_IFSOCK => FileType::Socket,
        _              => FileType::RegularFile,
    }
}
