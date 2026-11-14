//! Running a prepared command under supervision: its own process group, killed as a whole;
//! bounded wall-clock time and output; a private temporary directory purged at the end; and a
//! watchdog that kills the group if the Host itself dies abruptly.
//!
//! No `unsafe`: the group is made with `CommandExt::process_group` and signalled through
//! `rustix`'s safe API. A process that leaves the group (`setsid`) is not reached by this layer;
//! the OS engines add what their platform offers (a PID namespace on Linux, a Job Object on
//! Windows). Without supervision support (no Unix process groups) launching fails.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::SandboxError;
use crate::spec::{ResourceLimits, TMP_DIR_NAME};

/// How often `run` looks at its process.
const POLL_INTERVAL: Duration = Duration::from_millis(2);
/// After the process is gone, how long to wait for the output readers (a process that left the
/// group may still hold a pipe open).
const DRAIN_GRACE: Duration = Duration::from_secs(2);

static TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

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

#[cfg(not(unix))]
pub(crate) fn kill_process_group(_group: u32) -> bool {
    false
}

/// Kills a process group if the Host dies abruptly, when no destructor runs: a tiny `sh` blocks
/// reading a pipe whose only write end the Host holds. The kernel closes that end when the Host
/// dies, whatever the cause; the watchdog then kills the group.
#[derive(Debug)]
struct OrphanWatchdog {
    process: Child,
    _trigger: Option<ChildStdin>,
}

// Only started by the Unix supervision; Windows gets a Job Object with its engine (this lot).
#[cfg_attr(not(unix), allow(dead_code))]
impl OrphanWatchdog {
    #[cfg(unix)]
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

    #[cfg(not(unix))]
    fn start(_group: u32) -> Result<Self, SandboxError> {
        Err(SandboxError::SupervisionUnsupported)
    }

    /// The group is gone: stop watching before its id can be reused.
    fn stop(mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// Kills the group at a deadline (the lifetime budget of a long-lived process).
#[derive(Debug)]
struct LifetimeTimer {
    cancel: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

// Only started by the Unix supervision; Windows gets a Job Object with its engine (this lot).
#[cfg_attr(not(unix), allow(dead_code))]
impl LifetimeTimer {
    fn start(group: u32, lifetime: Duration, timed_out: Arc<AtomicBool>) -> Self {
        let (cancel, receiver) = mpsc::channel::<()>();
        let handle = thread::spawn(move || {
            if let Err(RecvTimeoutError::Timeout) = receiver.recv_timeout(lifetime) {
                timed_out.store(true, Ordering::SeqCst);
                kill_process_group(group);
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

/// A sandboxed process and everything it started. Dropping it kills the whole tree.
#[derive(Debug)]
pub struct SandboxedChild {
    child: Child,
    watchdog: Option<OrphanWatchdog>,
    timer: Option<LifetimeTimer>,
    timed_out: Arc<AtomicBool>,
    reaped: bool,
    /// Declared last: purged after the process tree is gone.
    _tmp: TmpDir,
}

impl SandboxedChild {
    #[cfg(unix)]
    pub(crate) fn spawn_prepared(
        mut command: Command,
        tmp: TmpDir,
        lifetime: Option<Duration>,
        stdin_piped: bool,
    ) -> Result<Self, SandboxError> {
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
        let child = command.spawn().map_err(SandboxError::Spawn)?;
        let mut supervised = Self {
            child,
            watchdog: None,
            timer: None,
            timed_out: Arc::new(AtomicBool::new(false)),
            reaped: false,
            _tmp: tmp,
        };
        // If the watchdog cannot start, `supervised` is dropped here, which kills the child:
        // never run unsupervised.
        supervised.watchdog = Some(OrphanWatchdog::start(supervised.child.id())?);
        if let Some(lifetime) = lifetime {
            supervised.timer = Some(LifetimeTimer::start(
                supervised.child.id(),
                lifetime,
                Arc::clone(&supervised.timed_out),
            ));
        }
        Ok(supervised)
    }

    #[cfg(not(unix))]
    pub(crate) fn spawn_prepared(
        _command: Command,
        _tmp: TmpDir,
        _lifetime: Option<Duration>,
        _stdin_piped: bool,
    ) -> Result<Self, SandboxError> {
        Err(SandboxError::SupervisionUnsupported)
    }

    /// OS process id of the child, which is also its process group id.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Whether the lifetime budget ran out and the tree was killed (`spawn` only).
    #[must_use]
    pub fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
    }

    /// Exit status if the child has already exited.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.finish();
        }
        Ok(status)
    }

    /// Waits for the child to exit by itself.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait()?;
        self.finish();
        Ok(status)
    }

    /// Kills the child and everything it started, then reaps it.
    pub fn kill(&mut self) {
        if self.reaped {
            return;
        }
        // Not reaped yet, so the pid (and group id) still belongs to our child.
        if !kill_process_group(self.child.id()) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
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
        // The child is gone; whatever it left running in its group goes too.
        kill_process_group(self.child.id());
        if let Some(watchdog) = self.watchdog.take() {
            watchdog.stop();
        }
    }
}

impl Drop for SandboxedChild {
    fn drop(&mut self) {
        self.kill();
    }
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
    let termination = loop {
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
