use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use fuser::Errno;

pub(crate) fn rel_to_cstring(rel: &Path) -> CString {
    let bytes = rel.as_os_str().as_bytes();
    let bytes = bytes.strip_prefix(b"/").unwrap_or(bytes);
    CString::new(bytes).unwrap_or_else(|_| CString::new(".").unwrap())
}

pub(crate) fn abs_to_cstring(path: &Path) -> CString {
    CString::new(path.as_os_str().as_bytes()).unwrap_or_else(|_| CString::new("/dev/null").unwrap())
}

pub(crate) fn io_to_errno(e: io::Error) -> Errno {
    Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))
}

pub(crate) fn last_errno() -> libc::c_int {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}
