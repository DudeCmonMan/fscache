use std::io;

use fuser::Request;

pub(crate) struct RequestCredentials {
    old_fsuid: libc::uid_t,
    old_fsgid: libc::gid_t,
}

impl RequestCredentials {
    pub(crate) fn enter(req: &Request) -> io::Result<Self> {
        let uid = req.uid() as libc::uid_t;
        let gid = req.gid() as libc::gid_t;

        let old_fsuid = unsafe { libc::setfsuid(uid) } as libc::uid_t;
        let current_fsuid = unsafe { libc::setfsuid(uid) } as libc::uid_t;

        if current_fsuid != uid {
            unsafe {
                libc::setfsuid(old_fsuid);
            }

            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("failed to set fsuid to {uid}"),
            ));
        }

        let old_fsgid = unsafe { libc::setfsgid(gid) } as libc::gid_t;
        let current_fsgid = unsafe { libc::setfsgid(gid) } as libc::gid_t;

        if current_fsgid != gid {
            unsafe {
                libc::setfsgid(old_fsgid);
                libc::setfsuid(old_fsuid);
            }

            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("failed to set fsgid to {gid}"),
            ));
        }

        Ok(Self {
            old_fsuid,
            old_fsgid,
        })
    }
}

impl Drop for RequestCredentials {
    fn drop(&mut self) {
        unsafe {
            libc::setfsgid(self.old_fsgid);
            libc::setfsuid(self.old_fsuid);
        }
    }
}
