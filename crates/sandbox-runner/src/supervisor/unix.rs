//! Unix supervision: the process leads its own group, signalled through `rustix`'s safe API, and
//! an orphan watchdog kills the group if the Host disappears. A process that leaves the group
//! (`setsid`) is not reached by this layer; the Linux engine adds a PID namespace for that.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};

use super::{ChildInput, ChildOutput, Started};
use crate::error::SandboxError;

/// Sends `SIGKILL` to process group `group`. Returns false if it could not be signalled.
pub(super) fn kill_process_group(group: u32) -> bool {
    use rustix::process::{kill_process_group, Pid, Signal};
    i32::try_from(group)
        .ok()
        .and_then(Pid::from_raw)
        .is_some_and(|group| kill_process_group(group, Signal::KILL).is_ok())
}

/// Kills a process group if the Host dies abruptly, when no destructor runs: a tiny `sh` blocks
/// reading a pipe whose only write end the Host holds. The kernel closes that end when the Host
/// dies, whatever the cause; the watchdog then kills the group.
#[derive(Debug)]
pub(super) struct OrphanWatchdog {
    process: Child,
    _trigger: Option<ChildStdin>,
}

impl OrphanWatchdog {
    pub(super) fn start(group: u32) -> Result<Self, SandboxError> {
        // `-s KILL --` is the form dash (the `/bin/sh` of Debian and Ubuntu) accepts for a
        // group id.
        let mut process = Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"read _; kill -s KILL -- "-$1" 2>/dev/null"#)
            .arg("hilo-sandbox-watchdog")
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
    pub(super) fn stop(mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

/// A running process group.
#[derive(Debug)]
pub(super) struct Backend {
    child: Child,
    watchdog: Option<OrphanWatchdog>,
}

impl Backend {
    pub(super) fn id(&self) -> u32 {
        self.child.id()
    }

    /// Ties the group to the Host's life.
    pub(super) fn supervise(&mut self) -> Result<(), SandboxError> {
        self.watchdog = Some(OrphanWatchdog::start(self.child.id())?);
        Ok(())
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Kills the whole group and reaps the process.
    pub(super) fn kill_and_reap(&mut self) {
        // Not reaped yet, so the pid (and group id) still belongs to our child.
        if !kill_process_group(self.child.id()) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }

    /// The process is gone: kills whatever it left running in its group, and stops watching.
    pub(super) fn clean_up(&mut self) {
        kill_process_group(self.child.id());
        if let Some(watchdog) = self.watchdog.take() {
            watchdog.stop();
        }
    }

    /// What kills the group from another thread.
    pub(super) fn killer(&self) -> Box<dyn FnOnce() + Send> {
        let group = self.child.id();
        Box::new(move || {
            kill_process_group(group);
        })
    }
}

/// Starts an isolating command (the sandbox tool) as the leader of a new process group.
pub(super) fn start_command(
    mut command: Command,
    stdin_piped: bool,
) -> Result<Started, SandboxError> {
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
        process: Backend {
            child,
            watchdog: None,
        },
    })
}
