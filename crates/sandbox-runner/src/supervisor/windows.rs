//! Windows supervision: the process lives in a Job Object (`sandbox-windows`), which the kernel
//! empties when the Host's handle to it closes, however the Host ends. No watchdog is needed.

use std::io;
use std::os::windows::process::ExitStatusExt;
use std::process::{Command, ExitStatus};

use super::{ChildInput, ChildOutput, Started};
use crate::error::SandboxError;

/// A running process tree in its Job Object.
#[derive(Debug)]
pub(super) struct Backend(sandbox_windows::Process);

impl Backend {
    pub(super) fn id(&self) -> u32 {
        self.0.id()
    }

    /// The Job Object already ties the tree to the Host's life.
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    pub(super) fn supervise(&mut self) -> Result<(), SandboxError> {
        Ok(())
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        Ok(self.0.try_wait()?.map(ExitStatus::from_raw))
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.0.wait().map(ExitStatus::from_raw)
    }

    /// Kills the whole tree and waits for the process.
    pub(super) fn kill_and_reap(&mut self) {
        self.0.kill_tree();
        let _ = self.0.wait();
    }

    /// The process is gone: kills whatever it left running in its tree.
    pub(super) fn clean_up(&mut self) {
        self.0.kill_tree();
    }

    /// What kills the tree from another thread.
    pub(super) fn killer(&self) -> Box<dyn FnOnce() + Send> {
        let killer = self.0.killer();
        Box::new(move || {
            killer.kill();
        })
    }
}

/// Windows isolates through an AppContainer, never through a wrapped command: a plain command
/// reaching this point would run unisolated, so it is refused.
pub(super) fn start_command(
    _command: Command,
    _stdin_piped: bool,
) -> Result<Started, SandboxError> {
    Err(SandboxError::unavailable(
        "a wrapped command cannot be isolated on Windows; the engine must use an AppContainer",
    ))
}

/// Starts a process inside a fresh AppContainer and Job Object.
pub(super) fn start_app_container(
    mut config: Box<sandbox_windows::LaunchConfig>,
    stdin_piped: bool,
) -> Result<Started, SandboxError> {
    config.stdin_piped = stdin_piped;
    let spawned = sandbox_windows::spawn(&config).map_err(SandboxError::Spawn)?;
    Ok(Started {
        process: Backend(spawned.process),
        stdin: spawned.stdin.map(|pipe| Box::new(pipe) as ChildInput),
        stdout: Some(Box::new(spawned.stdout) as ChildOutput),
        stderr: Some(Box::new(spawned.stderr) as ChildOutput),
    })
}
