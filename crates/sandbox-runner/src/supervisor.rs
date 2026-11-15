//! Running a prepared launch under supervision: the process tree is killed as a whole; wall-clock
//! time and output are bounded; a private temporary directory is purged at the end; and the tree
//! dies with the Host even if the Host dies abruptly.
//!
//! * **Unix:** the process leads its own group (`CommandExt::process_group`), signalled through
//!   `rustix`'s safe API, and an orphan watchdog kills the group if the Host disappears. A
//!   process that leaves the group (`setsid`) is not reached by this layer; the Linux engine
//!   adds a PID namespace for that.
//! * **Windows:** the process lives in a Job Object (`sandbox-windows`), which the kernel
//!   empties when the Host's handle to it closes, however the Host ends.
//!
//! No `unsafe` here.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, ChildStdin};
use std::process::{Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::engine::Launch;
use crate::error::SandboxError;
use crate::spec::{ResourceLimits, TMP_DIR_NAME};

/// How often `run` looks at its process.
const POLL_INTERVAL: Duration = Duration::from_millis(2);
/// After the process is gone, how long to wait for the output readers (a process that left the
/// group may still hold a pipe open).
const DRAIN_GRACE: Duration = Duration::from_secs(2);

static TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// A fresh identifier, unique among the runs of this Host and, with the process id and the
/// clock, across Hosts. Letters, digits and `-` only.
pub(crate) fn new_run_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "{}-{}-{nanos:x}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// The private temporary directory of one run, inside the worktree. Removed when dropped.
#[derive(Debug)]
pub(crate) struct TmpDir {
    path: PathBuf,
    parent: PathBuf,
}

impl TmpDir {
    pub(crate) fn create(worktree_root: &Path) -> Result<Self, SandboxError> {
        let parent = worktree_root.join(TMP_DIR_NAME);
        match fs::symlink_metadata(&parent) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(SandboxError::InvalidSpec(format!(
                    "{TMP_DIR_NAME} exists in the worktree and is not a plain directory"
                )))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_dir(&parent)?,
            Err(error) => return Err(error.into()),
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let name = format!(
            "{}-{}-{nanos}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = parent.join(name);
        create_private_dir(&path)?;
        Ok(Self { path, parent })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        // Only succeeds when empty: the worktree is left as it was.
        let _ = fs::remove_dir(&self.parent);
    }
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

/// Sends `SIGKILL` to process group `group`. Returns false if it could not be signalled.
#[cfg(unix)]
pub(crate) fn kill_process_group(group: u32) -> bool {
    use rustix::process::{kill_process_group, Pid, Signal};
    i32::try_from(group)
        .ok()
        .and_then(Pid::from_raw)
        .is_some_and(|group| kill_process_group(group, Signal::KILL).is_ok())
}

/// Kills a process group if the Host dies abruptly, when no destructor runs: a tiny `sh` blocks
/// reading a pipe whose only write end the Host holds. The kernel closes that end when the Host
/// dies, whatever the cause; the watchdog then kills the group.
#[cfg(unix)]
#[derive(Debug)]
struct OrphanWatchdog {
    process: Child,
    _trigger: Option<ChildStdin>,
}

#[cfg(unix)]
impl OrphanWatchdog {
    fn start(group: u32) -> Result<Self, SandboxError> {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        // `-s KILL --` is the form dash (the `/bin/sh` of Debian and Ubuntu) accepts for a
        // group id.
        let mut process = Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"read _; kill -s KILL -- "-$1" 2>/dev/null"#)
            .arg("coloride-sandbox-watchdog")
            .arg(group.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Its own group: signals aimed at the Host's group or the child's do not stop it.
            .process_group(0)
            .spawn()
            .map_err(SandboxError::Watchdog)?;
        let trigger = process.stdin.take();
        Ok(Self {
            process,
            _trigger: trigger,
        })
    }

    /// The group is gone: stop watching before its id can be reused.
    fn stop(mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Runs `kill` at a deadline (the lifetime budget of a long-lived process).
#[derive(Debug)]
struct LifetimeTimer {
    cancel: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl LifetimeTimer {
    fn start(
        kill: impl FnOnce() + Send + 'static,
        lifetime: Duration,
        timed_out: Arc<AtomicBool>,
    ) -> Self {
        let (cancel, receiver) = mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            if let Err(RecvTimeoutError::Timeout) = receiver.recv_timeout(lifetime) {
                timed_out.store(true, Ordering::SeqCst);
                kill();
            }
        });
        Self {
            cancel: Some(cancel),
            handle: Some(handle),
        }
    }

    fn stop(mut self) {
        drop(self.cancel.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// What the Host writes to a sandboxed process.
pub type ChildInput = Box<dyn Write + Send>;
/// What the Host reads from a sandboxed process.
pub type ChildOutput = Box<dyn Read + Send>;

/// The OS-specific half of a running sandboxed process: how it is waited for and how its whole
/// tree is killed.
enum Process {
    /// Unix: a process group and an orphan watchdog.
    #[cfg(unix)]
    Unix {
        child: Child,
        watchdog: Option<OrphanWatchdog>,
    },
    /// Windows: the process lives in a Job Object, which the kernel empties if the Host dies.
    #[cfg(windows)]
    Windows(sandbox_windows::Process),
}

impl std::fmt::Debug for Process {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Process")
    }
}

impl Process {
    fn id(&self) -> u32 {
        match self {
            #[cfg(unix)]
            Process::Unix { child, .. } => child.id(),
            #[cfg(windows)]
            Process::Windows(process) => process.id(),
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            #[cfg(unix)]
            Process::Unix { child, .. } => child.try_wait(),
            #[cfg(windows)]
            Process::Windows(process) => Ok(process.try_wait()?.map(exit_status)),
        }
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        match self {
            #[cfg(unix)]
            Process::Unix { child, .. } => child.wait(),
            #[cfg(windows)]
            Process::Windows(process) => process.wait().map(exit_status),
        }
    }

    /// Kills the whole tree and reaps the process.
    fn kill_and_reap(&mut self) {
        match self {
            #[cfg(unix)]
            Process::Unix { child, .. } => {
                // Not reaped yet, so the pid (and group id) still belongs to our child.
                if !kill_process_group(child.id()) {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
            #[cfg(windows)]
            Process::Windows(process) => {
                process.kill_tree();
                let _ = process.wait();
            }
        }
    }

    /// The process is gone: kills whatever it left running in its tree, and stops watching.
    fn clean_up(&mut self) {
        match self {
            #[cfg(unix)]
            Process::Unix { child, watchdog } => {
                kill_process_group(child.id());
                if let Some(watchdog) = watchdog.take() {
                    watchdog.stop();
                }
            }
            #[cfg(windows)]
            Process::Windows(process) => process.kill_tree(),
        }
    }

    /// What kills the tree from the lifetime timer thread.
    fn killer(&self) -> Box<dyn FnOnce() + Send> {
        match self {
            #[cfg(unix)]
            Process::Unix { child, .. } => {
                let group = child.id();
                Box::new(move || {
                    kill_process_group(group);
                })
            }
            #[cfg(windows)]
            Process::Windows(process) => {
                let killer = process.killer();
                Box::new(move || {
                    killer.kill();
                })
            }
        }
    }
}

#[cfg(windows)]
fn exit_status(code: u32) -> ExitStatus {
    std::os::windows::process::ExitStatusExt::from_raw(code)
}

/// A sandboxed process and everything it started. Dropping it kills the whole tree.
pub struct SandboxedChild {
    process: Process,
    stdin: Option<ChildInput>,
    stdout: Option<ChildOutput>,
    stderr: Option<ChildOutput>,
    timer: Option<LifetimeTimer>,
    timed_out: Arc<AtomicBool>,
    reaped: bool,
    /// Runs once, when the tree is gone (after it was killed or reaped).
    on_finish: Option<FinishHook>,
    /// Declared last: purged after the process tree is gone.
    _tmp: TmpDir,
}

impl std::fmt::Debug for SandboxedChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxedChild")
            .field("id", &self.process.id())
            .field("reaped", &self.reaped)
            .finish_non_exhaustive()
    }
}

/// What to do once a run is over.
pub(crate) struct FinishHook(Box<dyn FnOnce() + Send>);

impl FinishHook {
    pub(crate) fn new(hook: impl FnOnce() + Send + 'static) -> Self {
        Self(Box::new(hook))
    }
}

impl std::fmt::Debug for FinishHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FinishHook")
    }
}

impl SandboxedChild {
    /// Starts an already-isolated launch under supervision.
    pub(crate) fn spawn_prepared(
        launch: Launch,
        tmp: TmpDir,
        lifetime: Option<Duration>,
        stdin_piped: bool,
        on_finish: Option<FinishHook>,
    ) -> Result<Self, SandboxError> {
        let Started {
            process,
            stdin,
            stdout,
            stderr,
        } = match launch {
            Launch::Command(command) => start_command(command, stdin_piped)?,
            #[cfg(windows)]
            Launch::AppContainer(config) => start_app_container(config, stdin_piped)?,
        };
        let mut supervised = Self {
            process,
            stdin,
            stdout,
            stderr,
            timer: None,
            timed_out: Arc::new(AtomicBool::new(false)),
            reaped: false,
            on_finish,
            _tmp: tmp,
        };
        // If supervision cannot be completed, `supervised` is dropped, which kills the child:
        // never run unsupervised.
        supervised.supervise(lifetime)?;
        Ok(supervised)
    }

    /// Completes the supervision of a process that has just started. On Windows the Job Object
    /// already ties the tree to the Host; on Unix an orphan watchdog does it.
    fn supervise(&mut self, lifetime: Option<Duration>) -> Result<(), SandboxError> {
        #[cfg(unix)]
        {
            let Process::Unix { child, watchdog } = &mut self.process;
            *watchdog = Some(OrphanWatchdog::start(child.id())?);
        }
        if let Some(lifetime) = lifetime {
            self.timer = Some(LifetimeTimer::start(
                self.process.killer(),
                lifetime,
                Arc::clone(&self.timed_out),
            ));
        }
        Ok(())
    }

    /// OS process id of the child (on Unix also its process group id).
    #[must_use]
    pub fn id(&self) -> u32 {
        self.process.id()
    }

    pub fn take_stdin(&mut self) -> Option<ChildInput> {
        self.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildOutput> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildOutput> {
        self.stderr.take()
    }

    /// Whether the lifetime budget ran out and the tree was killed (`spawn` only).
    #[must_use]
    pub fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
    }

    /// Exit status if the child has already exited.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.process.try_wait()?;
        if status.is_some() {
            self.finish();
        }
        Ok(status)
    }

    /// Waits for the child to exit by itself.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.process.wait()?;
        self.finish();
        Ok(status)
    }

    /// Kills the child and everything it started, then reaps it.
    pub fn kill(&mut self) {
        if self.reaped {
            return;
        }
        self.process.kill_and_reap();
        self.finish();
    }

    fn finish(&mut self) {
        if self.reaped {
            return;
        }
        self.reaped = true;
        // The timer first, so that it can never signal a recycled group id.
        if let Some(timer) = self.timer.take() {
            timer.stop();
        }
        // The child is gone; whatever it left running in its tree goes too.
        self.process.clean_up();
        if let Some(hook) = self.on_finish.take() {
            (hook.0)();
        }
    }
}

impl Drop for SandboxedChild {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A process that has just started, and the Host's ends of its pipes.
struct Started {
    process: Process,
    stdin: Option<ChildInput>,
    stdout: Option<ChildOutput>,
    stderr: Option<ChildOutput>,
}

#[cfg(unix)]
fn start_command(mut command: Command, stdin_piped: bool) -> Result<Started, SandboxError> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    command
        .process_group(0)
        .stdin(if stdin_piped {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(SandboxError::Spawn)?;
    Ok(Started {
        stdin: child.stdin.take().map(|pipe| Box::new(pipe) as ChildInput),
        stdout: child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as ChildOutput),
        stderr: child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as ChildOutput),
        process: Process::Unix {
            child,
            watchdog: None,
        },
    })
}

/// Windows isolates through an AppContainer, never through a wrapped command: a plain command
/// reaching this point would run unisolated, so it is refused.
#[cfg(windows)]
fn start_command(_command: Command, _stdin_piped: bool) -> Result<Started, SandboxError> {
    Err(SandboxError::unavailable(
        "a wrapped command cannot be isolated on Windows; the engine must use an AppContainer",
    ))
}

#[cfg(windows)]
fn start_app_container(
    mut config: Box<sandbox_windows::LaunchConfig>,
    stdin_piped: bool,
) -> Result<Started, SandboxError> {
    config.stdin_piped = stdin_piped;
    let spawned = sandbox_windows::spawn(&config).map_err(SandboxError::Spawn)?;
    Ok(Started {
        process: Process::Windows(spawned.process),
        stdin: spawned.stdin.map(|pipe| Box::new(pipe) as ChildInput),
        stdout: Some(Box::new(spawned.stdout)),
        stderr: Some(Box::new(spawned.stderr)),
    })
}

/// How a `run` ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
    /// The wall-clock budget ran out; the whole tree was killed.
    WallTimeout,
    /// An output stream exceeded its budget; the whole tree was killed.
    OutputLimit,
}

impl Termination {
    fn from_status(status: ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
        #[cfg(not(unix))]
        let signal = None;
        Termination::Exited {
            code: status.code(),
            signal,
        }
    }

    #[must_use]
    pub fn success(&self) -> bool {
        matches!(self, Termination::Exited { code: Some(0), .. })
    }
}

/// The result of `run`. Output beyond the budget is cut, never buffered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    pub termination: Termination,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Bytes the process wrote (at least what was read before it was stopped).
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// A stream was still open after the grace period (a process left the group holding it).
    pub incomplete: bool,
    pub elapsed: Duration,
}

#[derive(Debug, Default)]
struct Capture {
    data: Vec<u8>,
    total: u64,
    truncated: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_reader<R: Read + Send + 'static>(
    mut reader: R,
    cap: u64,
    overflow: Arc<AtomicBool>,
    done: Sender<()>,
) -> Arc<Mutex<Capture>> {
    let capture = Arc::new(Mutex::new(Capture::default()));
    let shared = Arc::clone(&capture);
    thread::spawn(move || {
        let mut buffer = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let mut capture = lock(&shared);
                    capture.total += read as u64;
                    let room = cap.saturating_sub(capture.data.len() as u64) as usize;
                    let keep = read.min(room);
                    capture.data.extend_from_slice(&buffer[..keep]);
                    if keep < read {
                        capture.truncated = true;
                        overflow.store(true, Ordering::SeqCst);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = done.send(());
    });
    capture
}

/// Waits for `child` under `limits`, reading its output up to the budget. The process tree is
/// killed on timeout or on output overflow.
pub(crate) fn run_to_completion(
    child: &mut SandboxedChild,
    limits: &ResourceLimits,
) -> Result<RunOutput, SandboxError> {
    let started = Instant::now();
    let overflow = Arc::new(AtomicBool::new(false));
    let (done, finished) = mpsc::channel();
    let stdout = child.take_stdout().map(|pipe| {
        spawn_reader(
            pipe,
            limits.max_output_bytes,
            Arc::clone(&overflow),
            done.clone(),
        )
    });
    let stderr = child.take_stderr().map(|pipe| {
        spawn_reader(
            pipe,
            limits.max_output_bytes,
            Arc::clone(&overflow),
            done.clone(),
        )
    });
    drop(done);

    let deadline = started + limits.wall_timeout;
    let mut termination = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Termination::from_status(status),
            Ok(None) => {}
            Err(error) => {
                child.kill();
                return Err(error.into());
            }
        }
        if overflow.load(Ordering::SeqCst) {
            child.kill();
            break Termination::OutputLimit;
        }
        if Instant::now() >= deadline {
            child.kill();
            break Termination::WallTimeout;
        }
        thread::sleep(POLL_INTERVAL);
    };

    let readers = usize::from(stdout.is_some()) + usize::from(stderr.is_some());
    let grace = Instant::now() + DRAIN_GRACE;
    let mut incomplete = false;
    for _ in 0..readers {
        let left = grace.saturating_duration_since(Instant::now());
        if finished.recv_timeout(left).is_err() {
            incomplete = true;
            break;
        }
    }
    // The budget decides, not the race: a process that wrote more than its budget and then exited
    // on its own before the poll noticed is reported the same as one that was stopped.
    if matches!(termination, Termination::Exited { .. }) && overflow.load(Ordering::SeqCst) {
        termination = Termination::OutputLimit;
    }
    let take = |capture: Option<Arc<Mutex<Capture>>>| {
        capture.map_or_else(Capture::default, |capture| {
            std::mem::take(&mut *lock(&capture))
        })
    };
    let (stdout, stderr) = (take(stdout), take(stderr));
    Ok(RunOutput {
        termination,
        stdout_bytes: stdout.total,
        stderr_bytes: stderr.total,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        stdout: stdout.data,
        stderr: stderr.data,
        incomplete,
        elapsed: started.elapsed(),
    })
}
