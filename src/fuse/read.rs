use std::ffi::OsStr;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

use fuser::{
    Errno, FileHandle, FopenFlags, Generation, INodeNo, LockOwner, OpenFlags, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyXattr, Request,
};

use crate::discovery::OpKind;
use crate::telemetry;

use super::fusefs::{FsCache, OpenOutcome, TTL};
use super::util::{abs_to_cstring, io_to_errno, last_errno, rel_to_cstring};

impl FsCache {
    pub(crate) fn try_open(&self, path: &Path, filtered: bool) -> Result<(i32, OpenOutcome), Errno> {
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

    pub(crate) fn log_open_outcome(
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

    pub(crate) fn do_lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
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

    pub(crate) fn do_getattr(&self, req: &Request, ino: INodeNo, reply: ReplyAttr) {
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

    pub(crate) fn do_read(
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

        if let Some(bytes) = self.open_bytes.lock().unwrap().get_mut(&fh.0) {
            *bytes += n as u64;
        }

        reply.data(&buf);
    }

    pub(crate) fn do_opendir(&self, req: &Request, ino: INodeNo, reply: ReplyOpen) {
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

    pub(crate) fn do_readdir(
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

    pub(crate) fn do_releasedir(&self, fh: FileHandle, reply: fuser::ReplyEmpty) {
        unsafe { libc::close(fh.0 as RawFd) };
        reply.ok();
    }

    pub(crate) fn do_readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
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

    pub(crate) fn do_getxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
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

    pub(crate) fn do_listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
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

    pub(crate) fn do_statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
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
