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

#[cfg(windows)]
mod windows_pty {
    use super::super::{timer::LifetimeTimer, FinishHook, TmpDir};
    use super::*;
    use crate::{engine::Launch, error::SandboxError};
    use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtyPair, PtySize};
    use sandbox_windows::SupervisedJob;
    use std::{
        sync::{atomic::AtomicBool, Arc},
        time::Duration,
    };

    pub(crate) struct PreparedPty {
        pair: PtyPair,
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
            Ok(Self { pair })
        }

        pub(crate) fn spawn(
            self,
            launch: Launch,
            tmp: TmpDir,
            lifetime: Duration,
            on_finish: FinishHook,
        ) -> Result<Box<dyn SandboxedTerminal>, SandboxError> {
            let config = match launch {
                Launch::AppContainer(config) => config,
                Launch::Command(_) => {
                    return Err(SandboxError::unavailable(
                        "a wrapped command cannot be isolated on Windows",
                    ))
                }
            };
            let mut builder = CommandBuilder::new(&config.program);
            builder.args(&config.args);
            builder.env_clear();
            for (key, value) in &config.env {
                builder.env(key, value);
            }
            builder.cwd(&config.cwd);
            let stdout = self.pair.master.try_clone_reader().map_err(pty_error)?;
            let mut stdin = self.pair.master.take_writer().map_err(pty_error)?;
            let mut child = self.pair.slave.spawn_command(builder).map_err(pty_error)?;
            drop(self.pair.slave);
            // `portable-pty` creates ConPTY with `PSEUDOCONSOLE_INHERIT_CURSOR`, which makes
            // `conhost.exe` emit `ESC [ 6 n` and block all output until the initial cursor
            // position (`ESC [ 1 ; 1 R`) is reported on the input pipe.
            use std::io::Write;
            stdin.write_all(b"\x1b[1;1R").map_err(pty_error)?;
            stdin.flush().map_err(pty_error)?;
            let Some(pid) = child.process_id() else {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SandboxError::unavailable(
                    "terminal process has no id for supervision",
                ));
            };
            let job = match child.as_raw_handle() {
                Some(raw) => match SupervisedJob::assign_raw(raw) {
                    Ok(job) => Arc::new(job),
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(SandboxError::Io(error));
                    }
                },
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SandboxError::unavailable(
                        "terminal process has no OS handle for Job Object supervision",
                    ));
                }
            };
            let timer_job = Arc::clone(&job);
            let timer = Some(LifetimeTimer::start(
                move || {
                    timer_job.terminate();
                },
                lifetime,
                Arc::new(AtomicBool::new(false)),
            ));
            Ok(Box::new(PtyTerminal {
                child,
                pid,
                master: Some(self.pair.master),
                stdin: Some(stdin),
                stdout: Some(stdout),
                job,
                timer,
                finished: false,
                on_finish: Some(on_finish),
                _tmp: tmp,
            }))
        }
    }

    fn pty_error(error: impl std::fmt::Display) -> SandboxError {
        SandboxError::Io(io::Error::other(error.to_string()))
    }

    struct PtyTerminal {
        child: Box<dyn Child + Send + Sync>,
        pid: u32,
        master: Option<Box<dyn MasterPty + Send>>,
        stdin: Option<ChildInput>,
        stdout: Option<ChildOutput>,
        job: Arc<SupervisedJob>,
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
            self.job.terminate();
            // Closing the ConPTY pseudo-console flushes conhost.exe and closes its output pipe.
            let _ = self.master.take();
            if let Some(hook) = self.on_finish.take() {
                (hook.0)();
            }
        }
    }

    impl SandboxedTerminal for PtyTerminal {
        fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
            let Some(master) = self.master.as_ref() else {
                return Err(io::Error::other("terminal has exited"));
            };
            master
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
            self.job.terminate();
            let _ = self.child.kill();
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
#[cfg(windows)]
pub(crate) use windows_pty::PreparedPty;
