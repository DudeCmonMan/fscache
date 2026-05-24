use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, OsStr};
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::backing_store::BackingStore;
use crate::discovery::{DiscoveryController, OpKind};
use crate::preset::{CachePreset, ProcessInfo};
use crate::telemetry;

use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, KernelConfig, LockOwner, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, ReplyXattr, Request, TimeOrNow,
    WriteFlags,
};

use super::inode::InodeTable;
use super::util::{abs_to_cstring, io_to_errno, last_errno, rel_to_cstring};
use crate::cache::db::SourceMetadata;
use crate::cache::manager::CacheManager;
use crate::engine::action::AccessEvent;

/// Short TTL so the kernel re-checks after a cache file appears.
const TTL: Duration = Duration::from_secs(1);

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
    inodes: Arc<Mutex<InodeTable>>,
    pub passthrough_mode: bool,
    pub cache: Option<Arc<CacheManager>>,
    pub access_tx: Option<tokio::sync::mpsc::UnboundedSender<AccessEvent>>,
    pub repeat_log_window: Duration,
    /// Optional preset that controls open-time filtering and future caching actions.
    pub preset: Option<Arc<dyn CachePreset>>,
    pub discovery: Option<Arc<DiscoveryController>>,
    recent_logs: Mutex<HashMap<PathBuf, std::time::Instant>>,
    /// Bytes read per open file handle — emitted as telemetry on release.
    open_bytes: Mutex<HashMap<u64, u64>>,
    /// Path for each open file handle — used to send on_close events.
    open_paths: Mutex<HashMap<u64, PathBuf>>,
    /// Write passthrough handles are kept separate from read telemetry.
    write_paths: Mutex<HashMap<u64, PathBuf>>,
    append_handles: Mutex<HashSet<u64>>,
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

    fn stat_backing(&self, rel_path: &Path) -> Option<libc::stat> {
        self.backing_store.stat(rel_path)
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

    fn source_metadata_to_attr(&self, ino: u64, m: &SourceMetadata) -> FileAttr {
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

    fn cached_regular_attr(&self, rel_path: &Path, ino: u64) -> Option<FileAttr> {
        let cache = self.cache.as_ref()?;
        if !cache.is_cached(rel_path) {
            return None;
        }
        cache
            .source_metadata(rel_path)
            .map(|m| self.source_metadata_to_attr(ino, &m))
    }

    fn list_dir_entries(
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

    fn is_write_intent(flags: i32) -> bool {
        let access_mode = flags & libc::O_ACCMODE;
        access_mode == libc::O_WRONLY
            || access_mode == libc::O_RDWR
            || flags & (libc::O_TRUNC | libc::O_APPEND) != 0
    }

    fn child_path(&self, parent: INodeNo, name: &OsStr) -> Result<PathBuf, Errno> {
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

    fn path_for_ino(&self, ino: INodeNo) -> Result<PathBuf, Errno> {
        self.inodes
            .lock()
            .unwrap()
            .get_path(ino.0)
            .map(Path::to_path_buf)
            .ok_or(Errno::ENOENT)
    }

    fn fresh_attr_for_path(&self, path: &Path) -> Result<(u64, FileAttr), Errno> {
        let stat = self.stat_backing(path).ok_or(Errno::ENOENT)?;
        let ino = self.inodes.lock().unwrap().get_or_create(path);
        Ok((ino, self.stat_to_attr(ino, &stat)))
    }

    fn open_write_handle(
        &self,
        path: &Path,
        flags: i32,
        mode: u32,
        pid: u32,
    ) -> Result<RawFd, Errno> {
        let fd = self
            .backing_store
            .open_file_with_flags(path, flags, mode)
            .map_err(io_to_errno)?;
        let fh = fd as u64;
        self.write_paths
            .lock()
            .unwrap()
            .insert(fh, path.to_path_buf());
        if flags & libc::O_APPEND != 0 {
            self.append_handles.lock().unwrap().insert(fh);
        }
        tracing::debug!(path = %path.display(), flags, pid, "write passthrough open");
        Ok(fd)
    }

    ///
    /// `filtered` is passed through only to determine the return outcome (Hit vs
    /// Miss vs Filtered) — it does not affect which store is attempted.
    fn try_open(&self, path: &Path, filtered: bool) -> Result<(i32, OpenOutcome), Errno> {
        if !self.passthrough_mode
            && let Some(ref cache) = self.cache
            && cache.is_cached(path)
        {
            let cache_path = cache.cache_path(path);
            let c = abs_to_cstring(&cache_path);
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY) };
            if fd >= 0 {
                cache.mark_hit(path);
                return Ok((fd, OpenOutcome::Hit));
            }
            tracing::debug!(
                path = %path.display(),
                cache_path = %cache_path.display(),
                error = %std::io::Error::last_os_error(),
                "cache hit race, falling back to backing store"
            );
        }

        let fd = self
            .backing_store
            .open_file(path)
            .map_err(|_| Errno::from_i32(last_errno()))?;
        Ok((
            fd,
            if filtered {
                OpenOutcome::Filtered
            } else {
                OpenOutcome::Miss
            },
        ))
    }

    /// Emit the appropriate tracing line for an open() outcome.
    ///
    /// HIT is always INFO (confirms cache is working; never suppressed).
    /// Filtered is always INFO ("ignored process access").
    /// Miss respects the repeat-log suppress window: DEBUG when suppressed, INFO otherwise.
    fn log_open_outcome(
        &self,
        path: &Path,
        opener: Option<&str>,
        pid: u32,
        outcome: OpenOutcome,
        suppress: bool,
    ) {
        let opener = opener.unwrap_or("?");
        match outcome {
            OpenOutcome::Hit => {
                tracing::info!(
                    event = telemetry::EVENT_CACHE_HIT,
                    path = %path.display(),
                    "cache HIT: {:?} (serving from SSD)", path,
                );
            }
            OpenOutcome::Filtered => {
                tracing::info!(
                    "ignored process access: {:?} (filtered by preset, not caching) [opener: {} pid={}]",
                    path,
                    opener,
                    pid,
                );
            }
            OpenOutcome::Miss if suppress => {
                tracing::debug!(
                    event = telemetry::EVENT_CACHE_MISS,
                    path = %path.display(),
                    "cache MISS: {:?} (serving from backing store) [opener: {} pid={}]",
                    path, opener, pid,
                );
            }
            OpenOutcome::Miss => {
                tracing::info!(
                    event = telemetry::EVENT_CACHE_MISS,
                    path = %path.display(),
                    "cache MISS: {:?} (serving from backing store) [opener: {} pid={}]",
                    path, opener, pid,
                );
            }
        }
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
        let parent_path = match self.inodes.lock().unwrap().get_path(parent.0) {
            Some(p) => p.to_path_buf(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let child_path = if parent_path == Path::new("") {
            PathBuf::from(name)
        } else {
            parent_path.join(name)
        };

        let ino = self.inodes.lock().unwrap().get_or_create(&child_path);

        if let Some(attr) = self.cached_regular_attr(&child_path, ino) {
            if let Some(ref d) = self.discovery {
                d.log_touch(req.pid(), OpKind::Meta);
            }
            reply.entry(&TTL, &attr, Generation(0));
            return;
        }

        let Some(stat) = self.stat_backing(&child_path) else {
            reply.error(Errno::ENOENT);
            return;
        };

        if let Some(ref d) = self.discovery {
            d.log_touch(req.pid(), OpKind::Meta);
        }
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR
            && let Some(ref bw) = self.backing_watch
        {
            bw.touch(child_path.clone(), ino);
        }

        let attr = self.stat_to_attr(ino, &stat);
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.inodes.lock().unwrap().forget(ino.0, nlookup);
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let path = match self.path_for_ino(ino) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };

        if let Some(mode) = mode {
            if let Err(e) = self.backing_store.chmod(&path, mode).map_err(io_to_errno) {
                reply.error(e);
                return;
            }
        }
        if uid.is_some() || gid.is_some() {
            if let Err(e) = self
                .backing_store
                .chown(&path, uid, gid)
                .map_err(io_to_errno)
            {
                reply.error(e);
                return;
            }
        }
        if let Some(size) = size {
            let result = if let Some(fh) = fh {
                let rc = unsafe { libc::ftruncate(fh.0 as RawFd, size as libc::off_t) };
                if rc == 0 {
                    Ok(())
                } else {
                    Err(io_to_errno(std::io::Error::last_os_error()))
                }
            } else {
                self.backing_store
                    .truncate(&path, size)
                    .map_err(io_to_errno)
            };
            if let Err(e) = result {
                reply.error(e);
                return;
            }
        }
        if atime.is_some() || mtime.is_some() {
            let current = match self.stat_backing(&path) {
                Some(stat) => stat,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            };
            let atime = time_or_now_to_timespec(atime, current.st_atime, current.st_atime_nsec);
            let mtime = time_or_now_to_timespec(mtime, current.st_mtime, current.st_mtime_nsec);
            if let Err(e) = self
                .backing_store
                .utimens(&path, atime, mtime)
                .map_err(io_to_errno)
            {
                reply.error(e);
                return;
            }
        }

        match self.stat_backing(&path) {
            Some(stat) => reply.attr(&TTL, &self.stat_to_attr(ino.0, &stat)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let path = match self.inodes.lock().unwrap().get_path(ino.0) {
            Some(p) => p.to_path_buf(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if let Some(attr) = self.cached_regular_attr(&path, ino.0) {
            if let Some(ref d) = self.discovery {
                d.log_touch(req.pid(), OpKind::Meta);
            }
            reply.attr(&TTL, &attr);
            return;
        }

        let Some(stat) = self.stat_backing(&path) else {
            reply.error(Errno::ENOENT);
            return;
        };
        if let Some(ref d) = self.discovery {
            d.log_touch(req.pid(), OpKind::Meta);
        }
        reply.attr(&TTL, &self.stat_to_attr(ino.0, &stat));
    }

    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let path = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        if let Err(e) = self
            .backing_store
            .mknod(&path, apply_umask(mode, umask), rdev)
            .map_err(io_to_errno)
        {
            reply.error(e);
            return;
        }
        match self.fresh_attr_for_path(&path) {
            Ok((_ino, attr)) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let path = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        if let Err(e) = self
            .backing_store
            .mkdir(&path, apply_umask(mode, umask))
            .map_err(io_to_errno)
        {
            reply.error(e);
            return;
        }
        match self.fresh_attr_for_path(&path) {
            Ok((_ino, attr)) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let path = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        if let Err(e) = self.backing_store.unlink_file(&path).map_err(io_to_errno) {
            reply.error(e);
            return;
        }
        self.inodes.lock().unwrap().remove_path(&path);
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let path = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        if let Err(e) = self.backing_store.rmdir(&path).map_err(io_to_errno) {
            reply.error(e);
            return;
        }
        self.inodes.lock().unwrap().remove_subtree(&path);
        reply.ok();
    }

    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let path = match self.child_path(parent, link_name) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        if let Err(e) = self
            .backing_store
            .symlink(target.as_os_str(), &path)
            .map_err(io_to_errno)
        {
            reply.error(e);
            return;
        }
        match self.fresh_attr_for_path(&path) {
            Ok((_ino, attr)) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let old = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let new = match self.child_path(newparent, newname) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        if let Err(e) = self
            .backing_store
            .rename(&old, &new, flags.bits())
            .map_err(io_to_errno)
        {
            reply.error(e);
            return;
        }
        self.inodes.lock().unwrap().rename_path(&old, &new);
        reply.ok();
    }

    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let old = match self.path_for_ino(ino) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let new = match self.child_path(newparent, newname) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        if let Err(e) = self.backing_store.link(&old, &new).map_err(io_to_errno) {
            reply.error(e);
            return;
        }
        match self.fresh_attr_for_path(&new) {
            Ok((_ino, attr)) => reply.entry(&TTL, &attr, Generation(0)),
            Err(e) => reply.error(e),
        }
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
            match self.open_write_handle(&path, raw_flags, 0, req.pid()) {
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

        let (fd, outcome) = match self.try_open(&path, filtered) {
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
            return;
        }
        buf.truncate(n as usize);

        // Accumulate bytes read for telemetry emitted on release.
        if let Some(bytes) = self.open_bytes.lock().unwrap().get_mut(&fh.0) {
            *bytes += n as u64;
        }

        reply.data(&buf);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let append =
            self.append_handles.lock().unwrap().contains(&fh.0) || flags.0 & libc::O_APPEND != 0;
        let n = if append {
            unsafe { libc::write(fh.0 as RawFd, data.as_ptr().cast(), data.len()) }
        } else {
            unsafe {
                libc::pwrite(
                    fh.0 as RawFd,
                    data.as_ptr().cast(),
                    data.len(),
                    offset as libc::off_t,
                )
            }
        };
        if n < 0 {
            reply.error(Errno::from_i32(last_errno()));
        } else {
            reply.written(n as u32);
        }
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
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let rc = if datasync {
            unsafe { libc::fdatasync(fh.0 as RawFd) }
        } else {
            unsafe { libc::fsync(fh.0 as RawFd) }
        };
        if rc == 0 {
            reply.ok();
        } else {
            reply.error(Errno::from_i32(last_errno()));
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
        let path = match self.inodes.lock().unwrap().get_path(ino.0) {
            Some(p) => p.to_path_buf(),
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        if let Some(ref d) = self.discovery {
            d.log_touch(req.pid(), OpKind::Meta);
        }

        // O_PATH fds can't be used with fdopendir, so always open via openat with real flags.
        let c_path = if path == Path::new("") {
            rel_to_cstring(Path::new("."))
        } else {
            rel_to_cstring(&path)
        };
        let fd = unsafe {
            libc::openat(
                self.backing_store.fd(),
                c_path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY,
            )
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
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
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
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let c_path = rel_to_cstring(&path);
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        let len = unsafe {
            libc::readlinkat(
                self.backing_store.fd(),
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
        let path = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let fd = match self.open_write_handle(
            &path,
            flags | libc::O_CREAT,
            apply_umask(mode, umask),
            req.pid(),
        ) {
            Ok(fd) => fd,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        match self.fresh_attr_for_path(&path) {
            Ok((_ino, attr)) => reply.created(
                &TTL,
                &attr,
                Generation(0),
                FileHandle(fd as u64),
                FopenFlags::empty(),
            ),
            Err(e) => {
                unsafe { libc::close(fd) };
                self.write_paths.lock().unwrap().remove(&(fd as u64));
                self.append_handles.lock().unwrap().remove(&(fd as u64));
                if let Err(unlink_err) = self.backing_store.unlink_file(&path).map_err(io_to_errno)
                {
                    tracing::debug!(path = %path.display(), error = ?unlink_err, "failed to clean up create after attr lookup failure");
                }
                reply.error(e);
            }
        }
    }

    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        if position != 0 {
            reply.error(Errno::ENOSYS);
            return;
        }
        let path = match self.path_for_ino(ino) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        match self
            .backing_store
            .setxattr(&path, name, value, flags)
            .map_err(io_to_errno)
        {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let path = match self.path_for_ino(ino) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        match self
            .backing_store
            .getxattr(&path, name, size)
            .map_err(io_to_errno)
        {
            Ok(data) if size == 0 => reply.size(data.len() as u32),
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }

    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let path = match self.path_for_ino(ino) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        match self
            .backing_store
            .listxattr(&path, size)
            .map_err(io_to_errno)
        {
            Ok(data) if size == 0 => reply.size(data.len() as u32),
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }

    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let path = match self.path_for_ino(ino) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        match self
            .backing_store
            .removexattr(&path, name)
            .map_err(io_to_errno)
        {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: fuser::ReplyStatfs) {
        // backing_fd is O_PATH and cannot be used with fstatvfs directly.
        let dot = rel_to_cstring(Path::new("."));
        let real_fd = unsafe {
            libc::openat(
                self.backing_store.fd(),
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY,
            )
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

fn time_or_now_to_timespec(
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

fn apply_umask(mode: u32, umask: u32) -> u32 {
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
