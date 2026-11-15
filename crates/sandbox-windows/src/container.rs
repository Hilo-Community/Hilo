//! The AppContainer profile of one run: created before it, deleted after it.

use std::ffi::{c_void, OsStr};
use std::fmt;
use std::io;
use std::ptr;
use std::sync::Arc;

use windows_sys::Win32::Security::FreeSid;
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile,
};

use crate::win::wide_nul;

/// An AppContainer SID, freed with `FreeSid`.
pub(crate) struct Sid(*mut c_void);

// SAFETY: the SID is never modified after `CreateAppContainerProfile` returned it, and `FreeSid`
// may be called from any thread, so sending the owning pointer to another thread is sound.
unsafe impl Send for Sid {}
// SAFETY: shared access only reads the immutable SID.
unsafe impl Sync for Sid {}

impl Sid {
    pub(crate) fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `CreateAppContainerProfile`, whose documentation names
        // `FreeSid` as the way to release it, and it is released exactly once, here.
        unsafe { FreeSid(self.0) };
    }
}

/// A per-run AppContainer profile with no capability. Dropping it deletes the profile.
pub(crate) struct Container {
    /// NUL-terminated.
    name: Vec<u16>,
    sid: Arc<Sid>,
}

impl fmt::Debug for Container {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Container")
            .field(
                "name",
                &String::from_utf16_lossy(&self.name[..self.name.len() - 1]),
            )
            .finish_non_exhaustive()
    }
}

/// A profile name is 1 to 64 characters among letters, digits, `.`, `-` and `_`.
fn is_valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

impl Container {
    /// Creates a profile holding no capability: no network, no access but what is granted to
    /// its SID. Fails if a profile of that name already exists.
    pub(crate) fn create(name: &str) -> io::Result<Self> {
        if !is_valid_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid AppContainer profile name",
            ));
        }
        let name = wide_nul(OsStr::new(name))?;
        let label = wide_nul(OsStr::new("Coloride sandbox"))?;
        let mut sid: *mut c_void = ptr::null_mut();
        // SAFETY: the name and the label (used as display name and description) are
        // NUL-terminated and outlive the call; no capability is requested (null list, count 0);
        // `sid` is a valid out-pointer.
        let result = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                label.as_ptr(),
                label.as_ptr(),
                ptr::null(),
                0,
                &mut sid,
            )
        };
        if result < 0 {
            // An `HRESULT`, which `from_raw_os_error` displays as such.
            return Err(io::Error::from_raw_os_error(result));
        }
        if sid.is_null() {
            return Err(io::Error::other("the AppContainer profile has no SID"));
        }
        Ok(Self {
            name,
            sid: Arc::new(Sid(sid)),
        })
    }

    pub(crate) fn sid(&self) -> &Arc<Sid> {
        &self.sid
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        // SAFETY: `self.name` is the NUL-terminated name the profile was created with. A failure
        // (the profile is already gone) is not actionable in a destructor.
        let _ = unsafe { DeleteAppContainerProfile(self.name.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::is_valid_name;

    #[test]
    fn profile_names_are_restricted() {
        assert!(is_valid_name("coloride-123-4-abc"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("has space"));
        assert!(!is_valid_name(&"a".repeat(65)));
    }
}
