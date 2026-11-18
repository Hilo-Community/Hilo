//! Running a prepared launch under supervision: the process tree is killed as a whole; wall-clock
//! time and output are bounded; a private temporary directory is purged at the end; and the tree
//! dies with the Host even if the Host dies abruptly.
//!
//! The common life cycle lives here; each OS provides a `Backend` with the same methods: `unix`
//! (process group and orphan watchdog) and `windows` (Job Object). No `unsafe` here.

mod capture;
pub(crate) mod terminal;
mod timer;
mod tmp;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::io::{self, Read, Write};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub(crate) use capture::run_to_completion;
pub use capture::{RunOutput, Termination};
pub use terminal::SandboxedTerminal;
pub(crate) use tmp::{new_run_id, TmpDir};

use crate::engine::Launch;
use crate::error::SandboxError;
use timer::LifetimeTimer;
#[cfg(unix)]
use unix::{start_command, Backend};
#[cfg(windows)]
use windows::{start_app_container, start_command, Backend};

/// What the Host writes to a sandboxed process.
pub type ChildInput = Box<dyn Write + Send>;
/// What the Host reads from a sandboxed process.
pub type ChildOutput = Box<dyn Read + Send>;

/// A process that has just started, and the Host's ends of its pipes.
struct Started {
    process: Backend,
    stdin: Option<ChildInput>,
    stdout: Option<ChildOutput>,
    stderr: Option<ChildOutput>,
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

/// A sandboxed process and everything it started. Dropping it kills the whole tree.
pub struct SandboxedChild {
    process: Backend,
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
        supervised.process.supervise()?;
        if let Some(lifetime) = lifetime {
            supervised.timer = Some(LifetimeTimer::start(
                supervised.process.killer(),
                lifetime,
                Arc::clone(&supervised.timed_out),
            ));
        }
        Ok(supervised)
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
