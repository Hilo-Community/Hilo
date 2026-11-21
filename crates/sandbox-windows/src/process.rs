//! Creating the process inside the AppContainer and the Job Object, and watching it.

use std::ffi::{c_void, OsStr, OsString};
use std::fs::File;
use std::io;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    SetHandleInformation, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use crate::acl::Grant;
use crate::command_line::{build_command_line, build_environment_block};
use crate::container::Container;
use crate::job::{Job, KILLED_EXIT_CODE};
use crate::journal::JournalEntry;
use crate::win::{check_bool, owned_handle, wide_nul};

/// What to launch, and how tightly.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    /// Absolute path; never looked up in `PATH`. Batch files (`.bat`, `.cmd`) are refused: the
    /// command interpreter parses their arguments again and would undo the quoting.
    pub program: PathBuf,
    /// Arguments, quoted for the C runtime's parser (see `command_line`).
    pub args: Vec<OsString>,
    /// An existing directory inside one of `writable_dirs`.
    pub cwd: PathBuf,
    /// The whole environment of the process; nothing is inherited from the Host.
    pub env: Vec<(OsString, OsString)>,
    /// Directories the process may read and write. Their ACL gets an entry for this run only,
    /// removed when the [`Process`] is dropped. A directory containing a junction, a symbolic
    /// link or a hard link is refused: the entry would follow it outside.
    pub writable_dirs: Vec<PathBuf>,
    /// Read/execute shares. All ACL changes are journaled and scoped to the unique run SID.
    pub read_only_dirs: Vec<PathBuf>,
    /// Worktree paths whose contents this run must not read.
    pub denied_read_paths: Vec<PathBuf>,
    /// Where the run is recorded until it has cleaned up (see [`crate::recover_stale_runs`]).
    pub journal_dir: PathBuf,
    /// Limits of the whole process tree.
    pub limits: JobLimits,
    /// Name of the per-run AppContainer profile: unique, 1 to 64 of `[A-Za-z0-9._-]`.
    pub container_name: String,
    /// Whether the Host writes to the process; otherwise its stdin is at end of file.
    pub stdin_piped: bool,
}

/// Limits the Job Object enforces on the whole process tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobLimits {
    /// Processes alive at the same time, the first one included.
    pub max_processes: Option<u32>,
    /// Committed memory of all the processes together.
    pub max_memory_bytes: Option<u64>,
    /// User-mode CPU time of all the processes together.
    pub max_cpu_time: Option<Duration>,
}

/// A process started by [`spawn`], with the Host's ends of its pipes.
#[derive(Debug)]
pub struct Spawned {
    /// The process and its tree.
    pub process: Process,
    /// `None` unless [`LaunchConfig::stdin_piped`].
    pub stdin: Option<File>,
    /// Its standard output.
    pub stdout: File,
    /// Its standard error.
    pub stderr: File,
}

/// A sandboxed process and its tree. Dropping it kills the whole tree, removes the access it was
/// granted and deletes its AppContainer profile, in that order.
#[derive(Debug)]
pub struct Process {
    handle: OwnedHandle,
    pid: u32,
    job: Arc<Job>,
    /// Released once `kill_tree` has terminated and waited on the tree.
    changes: Mutex<Option<Changes>>,
}

/// What a run changed outside its process, undone when dropped: the access granted to
/// directories, then the profile. The journal entry is removed only if all of it was undone;
/// otherwise the next recovery tries again.
#[derive(Debug, Default)]
struct Changes {
    journal: Option<JournalEntry>,
    container: Option<Container>,
    grants: Vec<Grant>,
}

impl Drop for Changes {
    fn drop(&mut self) {
        let mut undone = true;
        for grant in self.grants.drain(..) {
            undone &= grant.revoke().is_ok();
        }
        if let Some(container) = self.container.take() {
            undone &= container.delete().is_ok();
        }
        if let Some(journal) = self.journal.take() {
            if undone {
                journal.complete();
            }
        }
    }
}

/// Kills a process tree from another thread (a lifetime timer).
#[derive(Debug, Clone)]
pub struct TreeKiller(Arc<Job>);

impl TreeKiller {
    /// Kills every process of the tree. Returns false if the kernel refused.
    pub fn kill(&self) -> bool {
        self.0.terminate()
    }
}

fn invalid_input(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, error)
}

fn utf16(text: &OsStr) -> Vec<u16> {
    text.encode_wide().collect()
}

/// An anonymous pipe; returns `(read end, write end)`. Both ends are inheritable: the caller
/// makes its own end private with [`keep_private`].
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut read, mut write) = (ptr::null_mut(), ptr::null_mut());
    // SAFETY: both out-pointers are valid and `attributes` outlives the call.
    check_bool(unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) })?;
    Ok((owned_handle(read)?, owned_handle(write)?))
}

/// Makes the Host's end of a pipe non-inheritable.
fn keep_private(handle: &OwnedHandle) -> io::Result<()> {
    // SAFETY: the handle is live for the whole call.
    check_bool(unsafe { SetHandleInformation(handle.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) })
}

/// The attribute list of `CreateProcessW`, deleted when dropped.
struct AttributeList {
    /// `u64` units, so that the buffer is aligned for the pointers the list stores.
    buffer: Vec<u64>,
}

impl AttributeList {
    fn new(count: u32) -> io::Result<Self> {
        let mut size = 0usize;
        // SAFETY: the documented way to ask for the size: a null list. The call reports an
        // insufficient buffer, which is expected, and fills `size`.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &mut size) };
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u64; size.div_ceil(mem::size_of::<u64>())];
        // SAFETY: `buffer` holds at least `size` bytes, is 8-aligned and lives as long as the
        // list (deleted in `Drop` before the buffer is freed).
        check_bool(unsafe {
            InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &mut size)
        })?;
        Ok(Self { buffer })
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast()
    }

    /// Adds one attribute.
    ///
    /// # Safety
    /// `value` must point to a `T` of the type `attribute` expects, and stay valid and unmoved
    /// until `CreateProcessW` has returned: the list stores the pointer, not a copy.
    unsafe fn set<T>(&mut self, attribute: u32, value: *const T) -> io::Result<()> {
        // SAFETY: the list was initialised by `new`; the caller guarantees `value`.
        check_bool(unsafe {
            UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                attribute as usize,
                value.cast::<c_void>(),
                mem::size_of::<T>(),
                ptr::null_mut(),
                ptr::null(),
            )
        })
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: the list was initialised by `new` and is deleted exactly once.
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

fn check_config(config: &LaunchConfig) -> io::Result<()> {
    if !config.program.is_absolute() {
        return Err(invalid_input("the program must be an absolute path"));
    }
    let is_batch = config
        .program
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
        });
    if is_batch {
        return Err(invalid_input("batch files cannot be run in the sandbox"));
    }
    if config.writable_dirs.is_empty() {
        return Err(invalid_input("at least one writable directory is required"));
    }
    Ok(())
}

/// AppContainer access is grant-gated: a deny on the package SID cannot carve a hole
/// in a recursive allow. Split shares around exclusions, granting their ancestors
/// only on the directory itself. Every resulting ACL mutation is journaled.
fn read_grants(config: &LaunchConfig) -> io::Result<Vec<(PathBuf, bool)>> {
    let denied = config
        .denied_read_paths
        .iter()
        .map(std::fs::canonicalize)
        .collect::<io::Result<Vec<_>>>()?;
    for writable in &config.writable_dirs {
        let writable = std::fs::canonicalize(writable)?;
        if denied
            .iter()
            .any(|path| path.starts_with(&writable) || writable.starts_with(path))
        {
            return Err(invalid_input(
                "a writable share overlaps a forbidden read path",
            ));
        }
    }
    let mut pending = config
        .read_only_dirs
        .iter()
        .map(std::fs::canonicalize)
        .collect::<io::Result<Vec<_>>>()?;
    let mut grants = Vec::new();
    while let Some(path) = pending.pop() {
        if denied.iter().any(|denied| path.starts_with(denied)) {
            continue;
        }
        let shallow = denied.iter().any(|denied| denied.starts_with(&path));
        if shallow {
            // Check before walking: reparse points must never widen the share.
            crate::acl::ensure_self_contained(&path)?;
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
        grants.push((path, shallow));
    }
    Ok(grants)
}

/// The child's ends of its three standard streams, and the Host's ends.
struct Pipes {
    child: [OwnedHandle; 3],
    stdin: OwnedHandle,
    stdout: OwnedHandle,
    stderr: OwnedHandle,
}

fn pipes() -> io::Result<Pipes> {
    let (child_stdin, stdin) = pipe()?;
    let (stdout, child_stdout) = pipe()?;
    let (stderr, child_stderr) = pipe()?;
    for host_end in [&stdin, &stdout, &stderr] {
        keep_private(host_end)?;
    }
    Ok(Pipes {
        child: [child_stdin, child_stdout, child_stderr],
        stdin,
        stdout,
        stderr,
    })
}

/// Starts `config.program` inside a fresh AppContainer and Job Object.
///
/// The process is created suspended, placed in the job, then resumed: it never runs outside the
/// job. It inherits nothing from the Host but its three pipes.
pub fn spawn(config: &LaunchConfig) -> io::Result<Spawned> {
    check_config(config)?;
    let read_grants = read_grants(config)?;
    let program = wide_nul(config.program.as_os_str())?;
    let cwd = wide_nul(config.cwd.as_os_str())?;
    let args: Vec<Vec<u16>> = config.args.iter().map(|arg| utf16(arg)).collect();
    let mut command_line =
        build_command_line(&program[..program.len() - 1], &args).map_err(invalid_input)?;
    command_line.push(0);
    let env: Vec<(Vec<u16>, Vec<u16>)> = config
        .env
        .iter()
        .map(|(name, value)| (utf16(name), utf16(value)))
        .collect();
    let environment = build_environment_block(&env).map_err(invalid_input)?;

    // Recorded first: if anything below fails, or the Host dies, it is undone.
    let mut changes = Changes::default();
    let changed_paths: Vec<_> = config
        .writable_dirs
        .iter()
        .chain(read_grants.iter().map(|(path, _)| path))
        .chain(&config.denied_read_paths)
        .cloned()
        .collect();
    changes.journal = Some(JournalEntry::create(
        &config.journal_dir,
        &config.container_name,
        &changed_paths,
    )?);
    let container = changes
        .container
        .insert(Container::create(&config.container_name)?);
    let sid = Arc::clone(container.sid());
    for dir in &config.writable_dirs {
        let index = changes.grants.len();
        changes.grants.push(Grant::prepare(dir, &sid)?);
        changes.grants[index].full_access()?;
    }
    for (dir, shallow) in &read_grants {
        let index = changes.grants.len();
        changes.grants.push(Grant::prepare(dir, &sid)?);
        if *shallow {
            changes.grants[index].read_only_shallow()?;
        } else {
            changes.grants[index].read_only()?;
        }
    }
    for path in &config.denied_read_paths {
        let index = changes.grants.len();
        changes.grants.push(Grant::prepare(path, &sid)?);
        changes.grants[index].deny_read()?;
    }
    let job = Arc::new(Job::create(&config.limits)?);
    let pipes = pipes()?;

    let inherited = pipes.child.each_ref().map(AsRawHandle::as_raw_handle);
    let capabilities = SECURITY_CAPABILITIES {
        AppContainerSid: sid.as_ptr(),
        Capabilities: ptr::null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };
    let mut attributes = AttributeList::new(2)?;
    // SAFETY: `capabilities` outlives CreateProcessW and refers to the live container SID.
    unsafe { attributes.set(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &capabilities)? };
    // SAFETY: `inherited` is a local array of the three live child handles that outlives
    // `CreateProcessW`; only these handles are inherited.
    unsafe { attributes.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &inherited)? };

    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: mem::size_of::<STARTUPINFOEXW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: inherited[0],
            hStdOutput: inherited[1],
            hStdError: inherited[2],
            ..STARTUPINFOW::default()
        },
        lpAttributeList: attributes.as_mut_ptr(),
    };
    let mut info = PROCESS_INFORMATION::default();
    // SAFETY: the application name, working directory and command line are NUL-terminated, and
    // the command line is writable (the API may modify it in place); `environment` is a
    // complete, double-NUL-terminated UTF-16 block, as CREATE_UNICODE_ENVIRONMENT says;
    // `startup` is a STARTUPINFOEXW with a valid attribute list, as EXTENDED_STARTUPINFO_PRESENT
    // says; every buffer outlives the call. CREATE_SUSPENDED: nothing runs before the process
    // is in the job.
    check_bool(unsafe {
        CreateProcessW(
            program.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            EXTENDED_STARTUPINFO_PRESENT
                | CREATE_SUSPENDED
                | CREATE_UNICODE_ENVIRONMENT
                | CREATE_NO_WINDOW,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            ptr::from_ref(&startup).cast::<STARTUPINFOW>(),
            &mut info,
        )
    })?;
    // Take ownership of both handles before anything else can fail.
    let handle = owned_handle(info.hProcess);
    let thread = owned_handle(info.hThread);
    let process = Process {
        handle: handle?,
        pid: info.dwProcessId,
        job,
        changes: Mutex::new(Some(changes)),
    };
    // From here, an error drops `process`, which kills the still-suspended process.
    let thread = thread?;
    process.job.assign(&process.handle)?;
    // SAFETY: `thread` is the live primary thread of the suspended process.
    if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    // The child's ends must close in the Host, or reading its output would never see the end.
    drop(pipes.child);

    Ok(Spawned {
        process,
        stdin: config.stdin_piped.then(|| File::from(pipes.stdin)),
        stdout: File::from(pipes.stdout),
        stderr: File::from(pipes.stderr),
    })
}

impl Process {
    /// The process id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// A handle that kills the tree from another thread.
    #[must_use]
    pub fn killer(&self) -> TreeKiller {
        TreeKiller(Arc::clone(&self.job))
    }

    /// The exit code if the process has ended, without waiting.
    pub fn try_wait(&self) -> io::Result<Option<u32>> {
        self.wait_for(0)
    }

    /// Waits for the process to end and returns its exit code.
    pub fn wait(&self) -> io::Result<u32> {
        self.wait_for(INFINITE)?
            .ok_or_else(|| io::Error::other("the wait ended before the process did"))
    }

    fn wait_for(&self, milliseconds: u32) -> io::Result<Option<u32>> {
        let handle = self.handle.as_raw_handle();
        // SAFETY: the handle is a live process handle owned by `self`.
        match unsafe { WaitForSingleObject(handle, milliseconds) } {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: the handle is live and `code` is a valid out-pointer.
                check_bool(unsafe { GetExitCodeProcess(handle, &mut code) })?;
                Ok(Some(code))
            }
            WAIT_TIMEOUT => Ok(None),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// Kills the process and everything it started, then releases file locks and permission
    /// changes once the root process has exited.
    pub fn kill_tree(&self) {
        self.job.terminate();
        // Also the process itself, in case it never made it into the job (a failure between
        // its creation and its assignment).
        // SAFETY: the handle is a live process handle owned by `self`; failure only means the
        // process has already ended.
        unsafe { TerminateProcess(self.handle.as_raw_handle(), KILLED_EXIT_CODE) };
        let _ = self.wait_for(5_000);
        if let Ok(mut guard) = self.changes.lock() {
            guard.take();
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.kill_tree();
    }
}
