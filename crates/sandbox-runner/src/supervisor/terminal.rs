//! Interactive login processes use the same prepared OS sandbox, with a private PTY.
use super::{ChildInput, ChildOutput};
use std::io;

#[derive(Debug, Clone, Copy)]
pub struct TerminalExit {
    pub code: Option<i32>,
    pub success: bool,
}

pub trait SandboxedTerminal: Send {
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
    fn id(&self) -> u32;
    fn take_stdin(&mut self) -> Option<ChildInput>;
    fn take_stdout(&mut self) -> Option<ChildOutput>;
    fn try_wait(&mut self) -> io::Result<Option<TerminalExit>>;
    fn kill(&mut self);
}

#[cfg(unix)]
mod native {
    use super::super::{
        timer::LifetimeTimer,
        unix::{kill_process_group, OrphanWatchdog},
        FinishHook, TmpDir,
    };
    use super::*;
    use crate::{engine::Launch, error::SandboxError};
    use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtyPair, PtySize};
    use std::{
        path::PathBuf,
        sync::{atomic::AtomicBool, Arc},
        time::Duration,
    };

    pub(crate) struct PreparedPty {
        pair: PtyPair,
        pub device: PathBuf,
    }
    impl PreparedPty {
        pub(crate) fn open() -> Result<Self, SandboxError> {
            let pair = native_pty_system()
                .openpty(PtySize {
                    rows: 30,
                    cols: 100,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(pty_error)?;
            let device = pair.master.tty_name().ok_or_else(|| {
                SandboxError::unavailable("the private terminal has no device path")
            })?;
            Ok(Self { pair, device })
        }
        pub(crate) fn spawn(
            self,
            launch: Launch,
            tmp: TmpDir,
            lifetime: Duration,
            on_finish: FinishHook,
        ) -> Result<Box<dyn SandboxedTerminal>, SandboxError> {
            let Launch::Command(command) = launch;
            let mut builder = CommandBuilder::new(command.get_program());
            builder.args(command.get_args());
            builder.env_clear();
            for (key, value) in command.get_envs() {
                if let Some(value) = value {
                    builder.env(key, value);
                }
            }
            builder.cwd(
                command.get_current_dir().ok_or_else(|| {
                    SandboxError::InvalidSpec("terminal sandbox has no cwd".into())
                })?,
            );
            let stdout = self.pair.master.try_clone_reader().map_err(pty_error)?;
            let stdin = self.pair.master.take_writer().map_err(pty_error)?;
            let mut child = self.pair.slave.spawn_command(builder).map_err(pty_error)?;
            drop(self.pair.slave);
            let Some(pid) = child.process_id() else {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SandboxError::unavailable(
                    "terminal process has no id for supervision",
                ));
            };
            let mut terminal = PtyTerminal {
                child,
                pid,
                _master: self.pair.master,
                stdin: Some(stdin),
                stdout: Some(stdout),
                watchdog: None,
                timer: None,
                finished: false,
                on_finish: Some(on_finish),
                _tmp: tmp,
            };
            // Drop kills the process if either supervision step fails.
            terminal.watchdog = Some(OrphanWatchdog::start(pid)?);
            terminal.timer = Some(LifetimeTimer::start(
                move || {
                    kill_process_group(pid);
                },
                lifetime,
                Arc::new(AtomicBool::new(false)),
            ));
            Ok(Box::new(terminal))
        }
    }
    fn pty_error(error: impl std::fmt::Display) -> SandboxError {
        SandboxError::Io(io::Error::other(error.to_string()))
    }

    struct PtyTerminal {
        child: Box<dyn Child + Send + Sync>,
        pid: u32,
        _master: Box<dyn MasterPty + Send>,
        stdin: Option<ChildInput>,
        stdout: Option<ChildOutput>,
        watchdog: Option<OrphanWatchdog>,
        timer: Option<LifetimeTimer>,
        finished: bool,
        on_finish: Option<FinishHook>,
        _tmp: TmpDir,
    }
    impl PtyTerminal {
        fn finish(&mut self) {
            if self.finished {
                return;
            }
            self.finished = true;
            if let Some(timer) = self.timer.take() {
                timer.stop();
            }
            kill_process_group(self.pid);
            if let Some(watchdog) = self.watchdog.take() {
                watchdog.stop();
            }
            if let Some(hook) = self.on_finish.take() {
                (hook.0)();
            }
        }
    }
    impl SandboxedTerminal for PtyTerminal {
        fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
            self._master
                .resize(PtySize {
                    cols,
                    rows,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| io::Error::other(e.to_string()))
        }
        fn id(&self) -> u32 {
            self.pid
        }
        fn take_stdin(&mut self) -> Option<ChildInput> {
            self.stdin.take()
        }
        fn take_stdout(&mut self) -> Option<ChildOutput> {
            self.stdout.take()
        }
        fn try_wait(&mut self) -> io::Result<Option<TerminalExit>> {
            let status = self.child.try_wait()?;
            if status.is_some() {
                self.finish();
            }
            Ok(status.map(|status| TerminalExit {
                code: if status.signal().is_some() {
                    None
                } else {
                    i32::try_from(status.exit_code()).ok()
                },
                success: status.success(),
            }))
        }
        fn kill(&mut self) {
            if self.finished {
                return;
            }
            if !kill_process_group(self.pid) {
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
            self.finish();
        }
    }
    impl Drop for PtyTerminal {
        fn drop(&mut self) {
            self.kill();
        }
    }
}
#[cfg(unix)]
pub(crate) use native::PreparedPty;
