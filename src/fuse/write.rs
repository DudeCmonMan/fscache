use std::ffi::OsStr;
use std::os::unix::io::RawFd;
use std::path::Path;
use std::time::SystemTime;

use fuser::{
    BsdFileFlags, Errno, FileHandle, FopenFlags, Generation, INodeNo, LockOwner, OpenFlags,
    ReplyAttr, ReplyCreate, ReplyEmpty, ReplyEntry, ReplyWrite, RenameFlags, Request, TimeOrNow,
    WriteFlags,
};

use super::fusefs::{apply_umask, time_or_now_to_timespec, FsCache, TTL};
use super::util::{io_to_errno, last_errno};
use super::credentials::RequestCredentials;

impl FsCache {
    pub(crate) fn open_write_handle(
        &self,
        req: &Request,
        path: &Path,
        flags: i32,
        mode: u32,
        pid: u32,
    ) -> Result<RawFd, Errno> {
        let _credentials =
            super::credentials::RequestCredentials::enter(req).map_err(io_to_errno)?;

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

    pub(crate) fn do_setattr(
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

    pub(crate) fn do_mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let _credentials = match RequestCredentials::enter(req) {
            Ok(credentials) => credentials,
            Err(e) => {
                reply.error(io_to_errno(e));
                return;
            }
        };
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

    pub(crate) fn do_mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let _credentials = match RequestCredentials::enter(req) {
            Ok(credentials) => credentials,
            Err(e) => {
                reply.error(io_to_errno(e));
                return;
            }
        };
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

    pub(crate) fn do_unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let path = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let _credentials = match RequestCredentials::enter(req) {
            Ok(credentials) => credentials,
            Err(e) => {
                reply.error(io_to_errno(e));
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

    pub(crate) fn do_rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let path = match self.child_path(parent, name) {
            Ok(path) => path,
            Err(e) => {
                reply.error(e);
                return;
            }
        };
        let _credentials = match RequestCredentials::enter(req) {
            Ok(credentials) => credentials,
            Err(e) => {
                reply.error(io_to_errno(e));
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

    pub(crate) fn do_symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let _credentials = match RequestCredentials::enter(req) {
            Ok(credentials) => credentials,
            Err(e) => {
                reply.error(io_to_errno(e));
                return;
            }
        };
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

    pub(crate) fn do_rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let _credentials = match RequestCredentials::enter(req) {
            Ok(credentials) => credentials,
            Err(e) => {
                reply.error(io_to_errno(e));
                return;
            }
        };
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

    pub(crate) fn do_link(
        &self,
        req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let _credentials = match RequestCredentials::enter(req) {
            Ok(credentials) => credentials,
            Err(e) => {
                reply.error(io_to_errno(e));
                return;
            }
        };
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

    pub(crate) fn do_write(
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

    pub(crate) fn do_fsync(
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

    pub(crate) fn do_create(
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
            req,
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
                if let Err(unlink_err) =
                    self.backing_store.unlink_file(&path).map_err(io_to_errno)
                {
                    tracing::debug!(path = %path.display(), error = ?unlink_err, "failed to clean up create after attr lookup failure");
                }
                reply.error(e);
            }
        }
    }

    pub(crate) fn do_setxattr(
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

    pub(crate) fn do_removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
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
}
