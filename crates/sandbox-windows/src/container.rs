//! The AppContainer profile of one run: created before it, deleted after it.

use std::ffi::{c_void, OsStr};
use std::fmt;
use std::io;
use std::ptr;
use std::sync::Arc;

use windows_sys::Win32::Security::FreeSid;
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};

use crate::win::wide_nul;

/// An AppContainer SID, freed with `FreeSid`.
pub(crate) struct Sid(*mut c_void);

// SAFETY: the SID is never modified after the API returned it, and `FreeSid` may be called from
// any thread, so sending the owning pointer to another thread is sound.
unsafe impl Send for Sid {}
// SAFETY: shared access only reads the immutable SID.
unsafe impl Sync for Sid {}

impl Sid {
    /// The SID of the profile `name`, computed from the name alone: it also works for a profile
    /// that was already deleted (to clean up after it).
    pub(crate) fn derive(name: &str) -> io::Result<Self> {
        let name = profile_name(name)?;
        let mut sid: *mut c_void = ptr::null_mut();
        // SAFETY: `name` is NUL-terminated and outlives the call; `sid` is a valid out-pointer.
        let result = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
        Self::from_result(result, sid)
    }

    fn from_result(result: i32, sid: *mut c_void) -> io::Result<Self> {
        if result < 0 {
            // An `HRESULT`, which `from_raw_os_error` displays as such.
            return Err(io::Error::from_raw_os_error(result));
        }
        if sid.is_null() {
            return Err(io::Error::other("the AppContainer API returned no SID"));
        }
        Ok(Self(sid))
    }

    pub(crate) fn as_ptr(&self) -> *mut c_void {
        self.0
    }
}

impl Drop for Sid {
    fn drop(&mut self) {
        // SAFETY: `self.0` came from `CreateAppContainerProfile` or
        // `DeriveAppContainerSidFromAppContainerName`, whose documentation names `FreeSid` as the
        // way to release it, and it is released exactly once, here.
        unsafe { FreeSid(self.0) };
    }
}

/// A profile name is 1 to 64 characters among letters, digits, `.`, `-` and `_`.
pub(crate) fn is_valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn profile_name(name: &str) -> io::Result<Vec<u16>> {
    if !is_valid_name(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid AppContainer profile name",
        ));
    }
    wide_nul(OsStr::new(name))
}

/// Deletes the profile `name`. Deleting a profile that does not exist is not an error.
pub(crate) fn delete_profile(name: &str) -> io::Result<()> {
    let name = profile_name(name)?;
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let result = unsafe { DeleteAppContainerProfile(name.as_ptr()) };
    if result < 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    Ok(())
}

/// A per-run AppContainer profile with no capability. It is deleted by [`Container::delete`]
/// (the owner reports whether that worked); it is not deleted on drop.
pub(crate) struct Container {
    name: String,
    sid: Arc<Sid>,
}

impl fmt::Debug for Container {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Container")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Container {
    /// Creates a profile holding no capability: no network, no access but what is granted to
    /// its SID. Fails if a profile of that name already exists.
    pub(crate) fn create(name: &str) -> io::Result<Self> {
        let wide = profile_name(name)?;
        let label = wide_nul(OsStr::new("Coloride sandbox"))?;
        let mut sid: *mut c_void = ptr::null_mut();
        // SAFETY: the name and the label (used as display name and description) are
        // NUL-terminated and outlive the call; no capability is requested (null list, count 0);
        // `sid` is a valid out-pointer.
        let result = unsafe {
            CreateAppContainerProfile(
                wide.as_ptr(),
                label.as_ptr(),
                label.as_ptr(),
                ptr::null(),
                0,
                &mut sid,
            )
        };
        Ok(Self {
            name: name.to_string(),
            sid: Arc::new(Sid::from_result(result, sid)?),
        })
    }

    pub(crate) fn sid(&self) -> &Arc<Sid> {
        &self.sid
    }

    pub(crate) fn delete(self) -> io::Result<()> {
        delete_profile(&self.name)
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
        assert!(!is_valid_name(r"..\\escape"));
        assert!(!is_valid_name(&"a".repeat(65)));
    }
}
