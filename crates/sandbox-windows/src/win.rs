//! Small helpers shared by the Win32 modules.

use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, WIN32_ERROR};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
};

/// UTF-16 with a terminating NUL, as the `...W` functions take it. A string that contains a NUL
/// would be cut there by Windows and is refused.
pub(crate) fn wide_nul(text: &OsStr) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = text.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a string contains a NUL character",
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// The result of a call that returns a `BOOL` and sets the thread's last error on failure.
pub(crate) fn check_bool(ok: BOOL) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// The result of a call that returns a Win32 error code (`ERROR_SUCCESS` on success).
pub(crate) fn check_status(status: WIN32_ERROR) -> io::Result<()> {
    match status {
        0 => Ok(()),
        // Win32 error codes are below `i32::MAX`.
        code => Err(io::Error::from_raw_os_error(code as i32)),
    }
}

/// Takes ownership of a handle a Win32 call just returned (null or invalid means it failed).
pub(crate) fn owned_handle(raw: *mut c_void) -> io::Result<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a valid, open handle that the caller just received from Win32 and that
    // nothing else owns; `OwnedHandle` closes it exactly once.
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// How many names (hard links) the open file has.
pub(crate) fn link_count(file: &File) -> io::Result<u32> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is an open file for the whole call and `info` is a valid out-pointer.
    check_bool(unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) })?;
    Ok(info.nNumberOfLinks)
}
