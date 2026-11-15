//! Granting an AppContainer access to a directory for one run, and taking it back.

use std::ffi::c_void;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr;
use std::sync::Arc;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, ACCESS_MODE, EXPLICIT_ACCESS_W,
    GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ALL_ACCESS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

use crate::container::Sid;
use crate::win::{check_status, link_count, wide_nul};

/// Memory allocated by the security API, released with `LocalFree`.
struct LocalMemory(*mut c_void);

impl Drop for LocalMemory {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer was allocated by `GetNamedSecurityInfoW` or `SetEntriesInAclW`,
            // whose documentation names `LocalFree` as the way to release it, and it is
            // released exactly once, here.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// Adds (`GRANT_ACCESS`) or removes (`REVOKE_ACCESS`) the entry of `sid` in the DACL of the
/// directory `path` (NUL-terminated), inherited by everything it contains.
fn update_dacl(path: &mut [u16], sid: &Sid, mode: ACCESS_MODE) -> io::Result<()> {
    let mut current: *mut ACL = ptr::null_mut();
    let mut descriptor: *mut c_void = ptr::null_mut();
    // SAFETY: `path` is NUL-terminated; only the DACL is requested (owner, group and SACL are
    // null); both out-pointers are valid. `current` points into `descriptor`, which stays
    // alive (`_descriptor`) until the end of this function.
    let status = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut current,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    check_status(status)?;
    let _descriptor = LocalMemory(descriptor);

    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: mode,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            // With `TRUSTEE_IS_SID` this field holds a pointer to the SID, not a name.
            ptstrName: sid.as_ptr().cast(),
        },
    };
    let mut updated: *mut ACL = ptr::null_mut();
    // SAFETY: one valid entry whose SID outlives the call; `current` is the DACL read above
    // (null is accepted) and is still valid; `updated` is a valid out-pointer.
    let status = unsafe { SetEntriesInAclW(1, &entry, current, &mut updated) };
    check_status(status)?;
    let _updated = LocalMemory(updated.cast());

    // SAFETY: `path` is NUL-terminated (the API takes a mutable pointer but does not write
    // through it); `updated` is the valid DACL built above and outlives the call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            path.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            updated,
            ptr::null(),
        )
    };
    check_status(status)
}

/// Refuses a directory tree that reaches outside itself. Granting access to a directory is
/// propagated to everything it contains, and a link would carry it to its target: a junction or
/// a symbolic link to another directory, or a hard link (a second name of a file that lives
/// elsewhere). Checking before granting keeps the change of permissions inside `dir`.
pub(crate) fn ensure_self_contained(dir: &Path) -> io::Result<()> {
    let refuse = |path: &Path, what: &str| {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is {what}: granting access to the worktree would reach outside it",
                path.display()
            ),
        ))
    };
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let meta = fs::symlink_metadata(&current)?;
        let kind = meta.file_type();
        // On Windows `is_symlink` is true for every name-surrogate reparse point: symbolic links
        // and junctions alike.
        if kind.is_symlink() {
            return refuse(&current, "a link");
        }
        if kind.is_dir() {
            for entry in fs::read_dir(&current)? {
                pending.push(entry?.path());
            }
        } else {
            // Read-attributes access is enough to count the names; sharing everything leaves the
            // file usable by others meanwhile.
            let file = OpenOptions::new()
                .access_mode(FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(&current)?;
            if link_count(&file)? > 1 {
                return refuse(&current, "a hard link");
            }
        }
    }
    Ok(())
}

/// Access of one AppContainer to one directory, taken back by [`Grant::revoke`].
pub(crate) struct Grant {
    /// NUL-terminated.
    path: Vec<u16>,
    sid: Arc<Sid>,
}

impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grant")
            .field(
                "path",
                &String::from_utf16_lossy(&self.path[..self.path.len() - 1]),
            )
            .finish_non_exhaustive()
    }
}

impl Grant {
    /// Gives the AppContainer `sid` full access to `dir` and everything in it, after checking
    /// that nothing in it leads outside.
    pub(crate) fn full_access(dir: &Path, sid: &Arc<Sid>) -> io::Result<Self> {
        ensure_self_contained(dir)?;
        let mut path = wide_nul(dir.as_os_str())?;
        if let Err(error) = update_dacl(&mut path, sid, GRANT_ACCESS) {
            // Never leave a half-applied grant behind.
            let _ = update_dacl(&mut path, sid, REVOKE_ACCESS);
            return Err(error);
        }
        Ok(Self {
            path,
            sid: Arc::clone(sid),
        })
    }

    /// Removes the access again.
    pub(crate) fn revoke(mut self) -> io::Result<()> {
        update_dacl(&mut self.path, &self.sid, REVOKE_ACCESS)
    }
}

/// Removes every entry of `sid` from the ACL of `dir`, for cleaning up after a run that could
/// not do it itself. A directory that no longer exists has nothing left to clean.
pub(crate) fn revoke_access(dir: &Path, sid: &Sid) -> io::Result<()> {
    let mut path = wide_nul(dir.as_os_str())?;
    match update_dacl(&mut path, sid, REVOKE_ACCESS) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}
