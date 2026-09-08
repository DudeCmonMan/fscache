use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, OsStr};
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::backing_store::BackingStore;
use crate::discovery::DiscoveryController;
use crate::preset::{CachePreset, ProcessInfo};
use crate::telemetry;

use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, INodeNo,
    KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, ReplyXattr, Request, TimeOrNow,
    WriteFlags,
};

use super::inode::InodeTable;
use super::util::abs_to_cstring;
use crate::cache::db::SourceMetadata;
use crate::cache::manager::CacheManager;
use crate::engine::action::AccessEvent;

pub(crate) const TTL: Duration = Duration::from_secs(1);

/// Outcome of a FUSE open() call. Drives log emission, engine notification, and
/// future cross-cutting hooks (e.g. process-discoverability).
///
/// Note: `Filtered` means the preset's `should_filter` returned true — a runtime,
/// preset-driven decision. This is distinct from the discovery `BLK` column which
/// reflects only explicit process allow/block policy. They overlap but are not
/// equivalent (e.g. PlexEpisodePrediction also filters Plex Transcoder via cmdline
/// inspection, independent of explicit process policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOutcome {
    /// Served from the SSD cache.
    Hit,
    /// Served from the backing store; eligible for caching by the action engine.
    Miss,
    /// Served from the backing store; preset suppressed engine notification — the
    /// access will not influence cache population or eviction decisions.
    Filtered,
}

pub struct FsCache {
    /// O_PATH fd opened to target_directory *before* the FUSE overmount.
    pub backing_store: Arc<BackingStore>,
    pub(crate) inodes: Arc<Mutex<InodeTable>>,
    pub passthrough_mode: bool,
    pub cache: Option<Arc<CacheManager>>,
    pub access_tx: Option<tokio::sync::mpsc::UnboundedSender<AccessEvent>>,
    pub repeat_log_window: Duration,
    /// Optional preset that controls open-time filtering and future caching actions.
    pub preset: Option<Arc<dyn CachePreset>>,
    pub discovery: Option<Arc<DiscoveryController>>,
    pub(crate) recent_logs: Mutex<HashMap<PathBuf, std::time::Instant>>,
    pub(crate) open_bytes: Mutex<HashMap<u64, u64>>,
    pub(crate) open_paths: Mutex<HashMap<u64, PathBuf>>,
    pub(crate) write_paths: Mutex<HashMap<u64, PathBuf>>,
    pub(crate) append_handles: Mutex<HashSet<u64>>,
    /// Handle to the backing-directory inotify watcher. None when feature is disabled.
    pub backing_watch: Option<crate::backing_watch::BackingWatchHandle>,
}

impl FsCache {
    /// MUST be called before mounting FUSE over `backing_path`.
    pub fn new(backing_path: &Path) -> anyhow::Result<Self> {
        let c_path = abs_to_cstring(backing_path);

        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_DIRECTORY) };
        if fd < 0 {
            return Err(anyhow::anyhow!(
                "failed to open backing path {}: {}",
                backing_path.display(),
                std::io::Error::last_os_error()
            ));
        }

        tracing::debug!(
            "Opened backing store O_PATH fd for {}",
            backing_path.display()
        );
        Ok(Self {
            backing_store: Arc::new(BackingStore::new(fd)),
            inodes: Arc::new(Mutex::new(InodeTable::new())),
            passthrough_mode: false,
            cache: None,
            access_tx: None,
            repeat_log_window: Duration::from_secs(60),
            preset: None,
            discovery: None,
            recent_logs: Mutex::new(HashMap::new()),
            open_bytes: Mutex::new(HashMap::new()),
            open_paths: Mutex::new(HashMap::new()),
            write_paths: Mutex::new(HashMap::new()),
            append_handles: Mutex::new(HashSet::new()),
            backing_watch: None,
        })
    }

    /// On first call (or after the window expires), records the timestamp and returns false.
    pub fn should_suppress_log(&self, path: &Path) -> bool {
        if self.repeat_log_window.is_zero() {
            return false;
        }
        let now = std::time::Instant::now();
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

    pub(crate) fn stat_backing(&self, rel_path: &Path) -> Option<libc::stat> {
        self.backing_store.stat(rel_path)
    }

    pub(crate) fn stat_to_attr(&self, ino: u64, s: &libc::stat) -> FileAttr {
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

    pub(crate) fn source_metadata_to_attr(&self, ino: u64, m: &SourceMetadata) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size: m.size,
            blocks: m.blocks,
            atime: UNIX_EPOCH
                + Duration::new(m.atime_sec.max(0) as u64, m.atime_nsec.max(0) as u32),
            mtime: UNIX_EPOCH
                + Duration::new(m.mtime_sec.max(0) as u64, m.mtime_nsec.max(0) as u32),
            ctime: UNIX_EPOCH
                + Duration::new(m.ctime_sec.max(0) as u64, m.ctime_nsec.max(0) as u32),
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: (m.mode & 0o7777) as u16,
            nlink: m.nlink as u32,
            uid: m.uid,
            gid: m.gid,
            rdev: m.rdev as u32,
            blksize: m.blksize,
            flags: 0,
        }
    }

    pub(crate) fn cached_regular_attr(&self, rel_path: &Path, ino: u64) -> Option<FileAttr> {
        let cache = self.cache.as_ref()?;
        if !cache.is_cached(rel_path) {
            return None;
        }
        cache
            .source_metadata(rel_path)
            .map(|m| self.source_metadata_to_attr(ino, &m))
    }

    pub(crate) fn list_dir_entries(
        &self,
        dir_fd: RawFd,
        parent_path: &Path,
    ) -> Vec<(std::ffi::OsString, u64, FileType)> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut entries: Vec<(OsString, u64, FileType)> = Vec::new();

        let dot_ino = self
            .inodes
            .lock()
            .unwrap()
            .get_path_ino(parent_path)
            .unwrap_or(InodeTable::root_ino().0);
        entries.push((OsString::from("."), dot_ino, FileType::Directory));

        let dotdot_path = parent_path.parent().unwrap_or(Path::new(""));
        let dotdot_ino = self
            .inodes
            .lock()
            .unwrap()
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
                libc::DT_BLK => FileType::BlockDevice,
                libc::DT_CHR => FileType::CharDevice,
                libc::DT_FIFO => FileType::NamedPipe,
                libc::DT_SOCK => FileType::Socket,
                libc::DT_UNKNOWN => match self.stat_backing(&child_path) {
                    Some(s) => mode_to_filetype(s.st_mode),
                    None => FileType::RegularFile,
                },
                _ => match self.stat_backing(&child_path) {
                    Some(s) => mode_to_filetype(s.st_mode),
                    None => FileType::RegularFile,
                },
            };

            let ino = self.inodes.lock().unwrap().get_or_create(&child_path);
            entries.push((name_os, ino, kind));
        }

        unsafe { libc::closedir(dir) };
        entries
    }

    pub(crate) fn is_write_intent(flags: i32) -> bool {
        let access_mode = flags & libc::O_ACCMODE;
        access_mode == libc::O_WRONLY
            || access_mode == libc::O_RDWR
            || flags & (libc::O_TRUNC | libc::O_APPEND) != 0
    }

    pub(crate) fn child_path(&self, parent: INodeNo, name: &OsStr) -> Result<PathBuf, Errno> {
        let parent_path = self
            .inodes
            .lock()
            .unwrap()
            .get_path(parent.0)
            .map(Path::to_path_buf)
            .ok_or(Errno::ENOENT)?;
        Ok(if parent_path == Path::new("") {
            PathBuf::from(name)
        } else {
            parent_path.join(name)
        })
    }

    pub(crate) fn path_for_ino(&self, ino: INodeNo) -> Result<PathBuf, Errno> {
        self.inodes
            .lock()
            .unwrap()
            .get_path(ino.0)
            .map(Path::to_path_buf)
            .ok_or(Errno::ENOENT)
    }

    pub(crate) fn fresh_attr_for_path(&self, path: &Path) -> Result<(u64, FileAttr), Errno> {
        let stat = self.stat_backing(path).ok_or(Errno::ENOENT)?;
        let ino = self.inodes.lock().unwrap().get_or_create(path);
        Ok((ino, self.stat_to_attr(ino, &stat)))
    }
}

impl Filesystem for FsCache {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> std::io::Result<()> {
        tracing::info!("FUSE filesystem initialized");
        Ok(())
    }

    fn destroy(&mut self) {
        tracing::info!("FUSE filesystem destroyed");
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.do_lookup(req, parent, name, reply)
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.inodes.lock().unwrap().forget(ino.0, nlookup);
    }

    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        self.do_setattr(
            req, ino, mode, uid, gid, size, atime, mtime, ctime, fh, crtime, chgtime, bkuptime,
            flags, reply,
        )
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.do_getattr(req, ino, reply)
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        self.do_mknod(req, parent, name, mode, umask, rdev, reply)
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        self.do_mkdir(req, parent, name, mode, umask, reply)
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.do_unlink(req, parent, name, reply)
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.do_rmdir(req, parent, name, reply)
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        self.do_symlink(req, parent, link_name, target, reply)
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        self.do_rename(req, parent, name, newparent, newname, flags, reply)
    }

    fn link(
        &self,
        req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        self.do_link(req, ino, newparent, newname, reply)
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let path = match self.inodes.lock().unwrap().get_path(ino.0) {
            Some(p) => p.to_path_buf(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let raw_flags = flags.0;
        if Self::is_write_intent(raw_flags) {
            match self.open_write_handle(req, &path, raw_flags, 0, req.pid()) {
                Ok(fd) => reply.opened(FileHandle(fd as u64), FopenFlags::empty()),
                Err(e) => reply.error(e),
            }
            return;
        }

        let suppress = self.should_suppress_log(&path);

        let process = if self.preset.is_some() || self.discovery.is_some() {
            Some(ProcessInfo::capture(req.pid()))
        } else {
            None
        };

        let (filtered, opener_name) = if let Some(ref preset) = self.preset {
            let proc = process.as_ref().unwrap();
            let name = proc.name.clone();
            let is_filtered = preset.should_filter(proc);
            if proc.name.as_deref() == Some("Plex Transcoder") {
                let cmdline = proc
                    .cmdline
                    .as_deref()
                    .map(|b| {
                        b.split(|&c| c == 0)
                            .filter(|s| !s.is_empty())
                            .map(|s| String::from_utf8_lossy(s).into_owned())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                tracing::debug!(
                    "Plex Transcoder open: filtered={} pid={} cmdline={:?}",
                    is_filtered,
                    req.pid(),
                    cmdline,
                );
            }
            if is_filtered {
                tracing::debug!(
                    "preset filtered pid {} ({}) on {:?}",
                    req.pid(),
                    name.as_deref().unwrap_or("?"),
                    path
                );
                (true, name)
            } else {
                (false, name)
            }
        } else {
            (false, None)
        };

        tracing::debug!(event = telemetry::EVENT_FUSE_OPEN, path = %path.display(), "fuse open");

        let (fd, outcome) = match self.try_open(req, &path, filtered) {
            Ok(v) => v,
            Err(e) => {
                reply.error(e);
                return;
            }
        };

        self.log_open_outcome(&path, opener_name.as_deref(), req.pid(), outcome, suppress);

        // Engine notification — filtered opens never notify the engine.
        // Passthrough mode also skips engine notification for misses (no caching desired).
        // Hit is only reachable from try_open when !passthrough_mode, so no extra guard needed.
        if !filtered {
            let engine_event = match outcome {
                OpenOutcome::Hit => Some(AccessEvent::hit(path.clone())),
                OpenOutcome::Miss if !self.passthrough_mode => {
                    Some(AccessEvent::miss(path.clone()))
                }
                OpenOutcome::Miss | OpenOutcome::Filtered => None,
            };
            if let Some(event) = engine_event {
                if let Some(ref tx) = self.access_tx {
                    let _ = tx.send(event);
                }
                self.open_paths
                    .lock()
                    .unwrap()
                    .insert(fd as u64, path.clone());
            }
        }

        self.open_bytes.lock().unwrap().insert(fd as u64, 0);

        if let (Some(d), Some(proc)) = (&self.discovery, &process) {
            d.log_open(proc, outcome);
        }

        reply.opened(FileHandle(fd as u64), FopenFlags::empty());
    }

    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        self.do_read(req, ino, fh, offset, size, flags, lock_owner, reply)
    }

    fn write(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        write_flags: WriteFlags,
        flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        self.do_write(req, ino, fh, offset, data, write_flags, flags, lock_owner, reply)
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.do_fsync(req, ino, fh, datasync, reply)
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
        if let Some(write_path) = self.write_paths.lock().unwrap().remove(&fh.0) {
            self.append_handles.lock().unwrap().remove(&fh.0);
            tracing::debug!(path = %write_path.display(), "write passthrough handle closed");
        } else {
            let bytes_read = self.open_bytes.lock().unwrap().remove(&fh.0).unwrap_or(0);
            let path = self.open_paths.lock().unwrap().remove(&fh.0);
            tracing::debug!(
                event = telemetry::EVENT_HANDLE_CLOSED,
                bytes_read,
                "handle closed"
            );
            if let (Some(path), Some(tx)) = (path, &self.access_tx) {
                let _ = tx.send(AccessEvent::close(path, bytes_read));
            }
        }
        unsafe { libc::close(fh.0 as RawFd) };
        reply.ok();
    }

    fn opendir(&self, req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        self.do_opendir(req, ino, reply)
    }

    fn readdir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        reply: ReplyDirectory,
    ) {
        self.do_readdir(req, ino, fh, offset, reply)
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.do_releasedir(fh, reply)
    }

    fn readlink(&self, req: &Request, ino: INodeNo, reply: ReplyData) {
        self.do_readlink(req, ino, reply)
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        self.do_create(req, parent, name, mode, umask, flags, reply)
    }

    fn setxattr(
        &self,
        req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        self.do_setxattr(req, ino, name, value, flags, position, reply)
    }

    fn getxattr(&self, req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        self.do_getxattr(req, ino, name, size, reply)
    }

    fn listxattr(&self, req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        self.do_listxattr(req, ino, size, reply)
    }

    fn removexattr(&self, req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.do_removexattr(req, ino, name, reply)
    }

    fn statfs(&self, req: &Request, ino: INodeNo, reply: fuser::ReplyStatfs) {
        self.do_statfs(req, ino, reply)
    }
}

pub(crate) fn time_or_now_to_timespec(
    value: Option<TimeOrNow>,
    fallback_sec: i64,
    fallback_nsec: i64,
) -> libc::timespec {
    match value {
        Some(TimeOrNow::SpecificTime(t)) => system_time_to_timespec(t),
        Some(TimeOrNow::Now) => system_time_to_timespec(SystemTime::now()),
        None => libc::timespec {
            tv_sec: fallback_sec as libc::time_t,
            tv_nsec: fallback_nsec as libc::c_long,
        },
    }
}

fn system_time_to_timespec(t: SystemTime) -> libc::timespec {
    let duration = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    libc::timespec {
        tv_sec: duration.as_secs() as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    }
}

pub(crate) fn apply_umask(mode: u32, umask: u32) -> u32 {
    mode & !umask
}

fn mode_to_filetype(mode: libc::mode_t) -> FileType {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => FileType::Directory,
        libc::S_IFLNK => FileType::Symlink,
        libc::S_IFBLK => FileType::BlockDevice,
        libc::S_IFCHR => FileType::CharDevice,
        libc::S_IFIFO => FileType::NamedPipe,
        libc::S_IFSOCK => FileType::Socket,
        _ => FileType::RegularFile,
    }
}
