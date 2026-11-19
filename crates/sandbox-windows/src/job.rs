//! The Job Object of one run: the process tree, its limits, and how it is killed.

use std::ffi::c_void;
use std::fmt;
use std::io;
use std::mem;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
    JOBOBJECTINFOCLASS, JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_JOB_TIME, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
};

use crate::process::JobLimits;
use crate::win::{check_bool, owned_handle};

/// Exit code given to the processes the job kills.
pub(crate) const KILLED_EXIT_CODE: u32 = 1;

/// Everything a sandboxed process must not do to the user's desktop session.
const UI_RESTRICTIONS: u32 = JOB_OBJECT_UILIMIT_DESKTOP
    | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
    | JOB_OBJECT_UILIMIT_EXITWINDOWS
    | JOB_OBJECT_UILIMIT_GLOBALATOMS
    | JOB_OBJECT_UILIMIT_HANDLES
    | JOB_OBJECT_UILIMIT_READCLIPBOARD
    | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS
    | JOB_OBJECT_UILIMIT_WRITECLIPBOARD;

pub(crate) struct Job {
    handle: OwnedHandle,
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job").finish_non_exhaustive()
    }
}

/// The extended limits of a job: always kill-on-close, plus what `limits` asks for.
fn extended_limits(limits: &JobLimits) -> JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
    // SAFETY: a plain C struct of integers, for which all-zero is a valid value.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
    let basic = &mut info.BasicLimitInformation;
    basic.LimitFlags =
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
    if let Some(count) = limits.max_processes {
        basic.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        basic.ActiveProcessLimit = count;
    }
    if let Some(time) = limits.max_cpu_time {
        basic.LimitFlags |= JOB_OBJECT_LIMIT_JOB_TIME;
        // In units of 100 ns.
        basic.PerJobUserTimeLimit = i64::try_from(time.as_nanos() / 100).unwrap_or(i64::MAX);
    }
    if let Some(bytes) = limits.max_memory_bytes {
        basic.LimitFlags |= JOB_OBJECT_LIMIT_JOB_MEMORY;
        info.JobMemoryLimit = usize::try_from(bytes).unwrap_or(usize::MAX);
    }
    info
}

impl Job {
    /// A job that kills its whole tree when its last handle closes, optionally forbids the UI
    /// escapes (clipboard, desktop, ...) and applies `limits`.
    pub(crate) fn create(limits: &JobLimits, restrict_ui: bool) -> io::Result<Self> {
        // SAFETY: default security and no name (both null); no other precondition.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        let job = Self {
            handle: owned_handle(raw)?,
        };
        job.set(JobObjectExtendedLimitInformation, &extended_limits(limits))?;
        if restrict_ui {
            job.set(
                JobObjectBasicUIRestrictions,
                &JOBOBJECT_BASIC_UI_RESTRICTIONS {
                    UIRestrictionsClass: UI_RESTRICTIONS,
                },
            )?;
        }
        Ok(job)
    }

    /// `info` must be the struct that `class` designates.
    fn set<T>(&self, class: JOBOBJECTINFOCLASS, info: &T) -> io::Result<()> {
        let size = u32::try_from(mem::size_of::<T>()).expect("job information structs are small");
        // SAFETY: the handle is a live job; `info` points to `size` readable bytes of the struct
        // `class` expects (guaranteed by the two private call sites) and outlives the call.
        check_bool(unsafe {
            SetInformationJobObject(
                self.handle.as_raw_handle(),
                class,
                ptr::from_ref(info).cast::<c_void>(),
                size,
            )
        })
    }

    /// Puts a process (created suspended) in the job: from then on, it and its descendants
    /// cannot leave it.
    pub(crate) fn assign(&self, process: &OwnedHandle) -> io::Result<()> {
        // SAFETY: both handles are live for the whole call.
        check_bool(unsafe {
            AssignProcessToJobObject(self.handle.as_raw_handle(), process.as_raw_handle())
        })
    }

    /// Kills every process of the job. Returns false if the kernel refused.
    pub(crate) fn terminate(&self) -> bool {
        // SAFETY: the handle is a live job.
        unsafe { TerminateJobObject(self.handle.as_raw_handle(), KILLED_EXIT_CODE) != 0 }
    }
}

/// A Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` that ties a Host-supervised process
/// tree (`host-core` terminal, language server or debug adapter) to the Host's lifetime.
pub struct SupervisedJob {
    handle: OwnedHandle,
}

impl fmt::Debug for SupervisedJob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupervisedJob").finish_non_exhaustive()
    }
}

impl SupervisedJob {
    /// Places `process` in a fresh Job Object whose processes are killed when the handle closes
    /// (including on an abrupt Host exit) or when [`Self::terminate`] is called.
    // A Windows `RawHandle` (`HANDLE`) is an opaque kernel handle-table index, never dereferenced
    // as a memory pointer; the kernel validates it and returns `ERROR_INVALID_HANDLE` if invalid.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn assign_raw(process: std::os::windows::io::RawHandle) -> io::Result<Self> {
        if process.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing process handle",
            ));
        }
        // SAFETY: default security and no name (both null); no other precondition.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        let job = Self {
            handle: owned_handle(raw)?,
        };
        let info = extended_limits(&JobLimits::default());
        let size = u32::try_from(mem::size_of_val(&info)).expect("job information struct is small");
        // SAFETY: the handle is a live job; `info` points to `size` readable bytes of
        // `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` and outlives the call.
        check_bool(unsafe {
            SetInformationJobObject(
                job.handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                ptr::from_ref(&info).cast::<c_void>(),
                size,
            )
        })?;
        // SAFETY: `job.handle` is a live job; `process` is a non-null OS handle validated by the
        // kernel (`AssignProcessToJobObject` fails with `ERROR_INVALID_HANDLE` if invalid).
        check_bool(unsafe { AssignProcessToJobObject(job.handle.as_raw_handle(), process) })?;
        Ok(job)
    }

    /// Kills every process in the job. Returns false if the kernel refused.
    pub fn terminate(&self) -> bool {
        // SAFETY: the handle is a live job.
        unsafe { TerminateJobObject(self.handle.as_raw_handle(), KILLED_EXIT_CODE) != 0 }
    }
}

impl Drop for SupervisedJob {
    fn drop(&mut self) {
        self.terminate();
    }
}
