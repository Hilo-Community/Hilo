//! Integrated terminal (`LOT-010`): a login shell on a pseudo-terminal, started in a trusted
//! workspace only, whose output is delivered in frames of at most one every 16 ms.
//!
//! The shell leads its own session and process group (`portable-pty` calls `setsid`), so the
//! whole job tree is killed with the session, and an [`OrphanWatchdog`] kills it if the Host
//! dies abruptly. On Unix, a known agent CLI running in it is reported too (`agent_status`).

use std::io::{Read, Write};
use std::path::PathBuf;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use path_util::plain_path as plain_cwd;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use thiserror::Error;

use crate::process::{kill_process_group, OrphanWatchdog, SupervisionError};
use crate::trust::{GatedCapability, TrustError, WorkspaceTrustGate};

#[cfg(unix)]
mod agent_status;
#[cfg(unix)]
mod osc_title;

/// Output is sent at most once per frame (`AGENTS.md`: 16 ms stream batching).
pub const TERMINAL_FRAME: Duration = Duration::from_millis(16);
/// A frame never holds more than this; a flood is sent in several frames.
const MAX_FRAME_BYTES: usize = 256 * 1024;

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error(transparent)]
    Trust(#[from] TrustError),
    #[error(transparent)]
    Supervision(#[from] SupervisionError),
    #[error("terminal error: {0}")]
    Pty(String),
}

fn pty_error(error: impl std::fmt::Display) -> TerminalError {
    TerminalError::Pty(error.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl TerminalSize {
    fn pty(self) -> PtySize {
        PtySize {
            rows: self.rows.max(1),
            cols: self.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Bytes the shell wrote during one frame (raw: may end inside a UTF-8 sequence).
    Output(Vec<u8>),
    /// The shell exited; nothing follows.
    Exited,
}

pub type TerminalSink = Arc<dyn Fn(TerminalEvent) + Send + Sync>;

/// Whether a known agent CLI (not the shell itself) is the terminal's foreground job, and
/// whether it looks busy. Unix only (`tcgetpgrp` has no Windows equivalent); on other platforms
/// this is never reported.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalAgentStatus {
    pub agent_id: Option<String>,
    pub busy: bool,
}

pub type TerminalStatusSink = Arc<dyn Fn(TerminalAgentStatus) + Send + Sync>;

/// The pty, shared with the threads that outlive a call (`None` once closed, on Windows).
type SharedMaster = Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>;
/// The shell process, shared with the threads that watch it.
type SharedChild = Arc<Mutex<Box<dyn Child + Send + Sync>>>;

/// What to run. `program: None` is the user's login shell.
#[derive(Debug, Clone, Default)]
pub struct TerminalLaunch {
    pub program: Option<PathBuf>,
    pub args: Vec<String>,
}

pub struct TerminalSession {
    writer: Mutex<Box<dyn Write + Send>>,
    master: SharedMaster,
    child: SharedChild,
    watchdog: Mutex<Option<OrphanWatchdog>>,
    pid: u32,
    #[cfg(unix)]
    observed: Option<agent_status::ObservedOutput>,
}

impl std::fmt::Debug for TerminalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalSession")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl TerminalSession {
    /// Starts a shell in the workspace root. Refused unless the workspace is trusted.
    ///
    /// `status_sink`, when given, is told whenever a known agent CLI becomes (or stops being)
    /// the terminal's foreground job, and whenever its busy/idle guess changes. Unix only; on
    /// other platforms it is accepted but never called.
    pub fn spawn(
        gate: &WorkspaceTrustGate,
        launch: &TerminalLaunch,
        size: TerminalSize,
        sink: TerminalSink,
        #[cfg_attr(not(unix), allow(unused_variables))] status_sink: Option<TerminalStatusSink>,
        #[cfg_attr(not(unix), allow(unused_variables))] hook_server: Option<
            Arc<crate::agent_hooks::AgentHookServer>,
        >,
    ) -> Result<Self, TerminalError> {
        gate.authorize(GatedCapability::Terminal)?;
        let pair = native_pty_system().openpty(size.pty()).map_err(pty_error)?;
        let mut command = match &launch.program {
            Some(program) => {
                let mut command = CommandBuilder::new(program);
                command.args(&launch.args);
                command
            }
            None => CommandBuilder::new_default_prog(),
        };
        command.cwd(plain_cwd(gate.workspace_root()));
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "Hilo");
        #[cfg(unix)]
        let agent_status = agent_status::AgentStatusSetup::prepare(
            status_sink,
            hook_server.as_deref(),
            gate.workspace_root(),
            &mut command,
        );
        // Trust may have been revoked while the pty was being opened.
        gate.authorize(GatedCapability::Terminal)?;
        let mut child = pair.slave.spawn_command(command).map_err(pty_error)?;
        drop(pair.slave);
        gate.note_spawned();
        let pid = child.process_id().unwrap_or_default();
        #[cfg(unix)]
        let watchdog_result = OrphanWatchdog::start(pid);
        #[cfg(windows)]
        let watchdog_result = OrphanWatchdog::for_raw_handle(child.as_raw_handle());
        #[cfg(not(any(unix, windows)))]
        let watchdog_result = OrphanWatchdog::start(pid);
        let watchdog = match watchdog_result {
            Ok(watchdog) => watchdog,
            Err(error) => {
                // Never leave an unsupervised shell running.
                kill_process_group(pid);
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        };
        let reader = pair.master.try_clone_reader().map_err(pty_error)?;
        let writer = pair.master.take_writer().map_err(pty_error)?;
        #[cfg(windows)]
        let mut writer = writer;
        #[cfg(windows)]
        {
            // `portable-pty` creates ConPTY with `PSEUDOCONSOLE_INHERIT_CURSOR`, which makes
            // `conhost.exe` emit `ESC [ 6 n` and block all output until the initial cursor
            // position (`ESC [ 1 ; 1 R`) is reported on the input pipe.
            writer.write_all(b"\x1b[1;1R").map_err(pty_error)?;
            writer.flush().map_err(pty_error)?;
        }
        let master = Arc::new(Mutex::new(Some(pair.master)));
        let child = Arc::new(Mutex::new(child));
        #[cfg(unix)]
        let observed = agent_status
            .as_ref()
            .map(agent_status::AgentStatusSetup::observer);
        #[cfg(windows)]
        let active = Arc::new(AtomicBool::new(false));
        spawn_output_pump(
            reader,
            sink,
            #[cfg(unix)]
            observed.clone(),
            #[cfg(windows)]
            Arc::clone(&active),
        );
        #[cfg(unix)]
        if let Some(agent_status) = agent_status {
            agent_status.start(Arc::clone(&master), Arc::clone(&child), pid);
        }
        #[cfg(windows)]
        {
            let child = Arc::clone(&child);
            let master = Arc::clone(&master);
            std::thread::spawn(move || {
                while !matches!(lock(&child).try_wait(), Ok(Some(_))) {
                    if lock(&master).is_none() {
                        return;
                    }
                    std::thread::sleep(TERMINAL_FRAME);
                }
                while active.swap(false, Ordering::SeqCst) {
                    std::thread::sleep(TERMINAL_FRAME);
                }
                // Closing the ConPTY pseudo-console flushes conhost.exe and closes its output
                // pipe, so the reader thread sees EOF once the shell has exited.
                let _ = lock(&master).take();
            });
        }
        Ok(Self {
            writer: Mutex::new(writer),
            master,
            child,
            watchdog: Mutex::new(Some(watchdog)),
            pid,
            #[cfg(unix)]
            observed,
        })
    }

    /// The pid of the pty's current foreground process group, when it differs from the shell's
    /// own (i.e. a foreground job is running). Unix only.
    #[cfg(unix)]
    #[must_use]
    pub fn foreground_pgid(&self) -> Option<i32> {
        foreground_pgid_of(&self.master)
    }

    /// OS process id of the shell, which is also its process group id.
    #[must_use]
    pub fn process_id(&self) -> u32 {
        self.pid
    }

    /// Sends keystrokes (or pasted text) to the shell.
    pub fn write(&self, bytes: &[u8]) -> Result<(), TerminalError> {
        #[cfg(unix)]
        if let Some(observed) = &self.observed {
            observed.record_input(bytes);
        }
        let mut writer = lock(&self.writer);
        writer.write_all(bytes).map_err(pty_error)?;
        writer.flush().map_err(pty_error)
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), TerminalError> {
        let master = lock(&self.master);
        let Some(master) = master.as_ref() else {
            return Err(pty_error("terminal has exited"));
        };
        master.resize(size.pty()).map_err(pty_error)
    }

    /// Whether the shell has exited.
    #[must_use]
    pub fn has_exited(&self) -> bool {
        matches!(lock(&self.child).try_wait(), Ok(Some(_)))
    }

    /// Kills the shell and every job it started, then reaps it.
    pub fn kill(&self) {
        #[cfg(windows)]
        if let Some(watchdog) = lock(&self.watchdog).take() {
            watchdog.stop();
        }
        let mut child = lock(&self.child);
        if !matches!(child.try_wait(), Ok(Some(_))) {
            // Not reaped, so the group id still belongs to this shell.
            kill_process_group(self.pid);
            let _ = child.kill();
            let _ = child.wait();
        }
        drop(child);
        if let Some(watchdog) = lock(&self.watchdog).take() {
            watchdog.stop();
        }
        #[cfg(windows)]
        let _ = lock(&self.master).take();
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Reads the pty on one thread and hands frames to `sink` from another, so a slow consumer
/// never stalls the shell and a fast shell never floods the consumer.
fn spawn_output_pump(
    mut reader: Box<dyn Read + Send>,
    sink: TerminalSink,
    #[cfg(unix)] observed: Option<agent_status::ObservedOutput>,
    #[cfg(windows)] active: Arc<AtomicBool>,
) {
    let (chunks, received) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break, // EOF / EIO once the shell and its jobs are gone
                Ok(read) => {
                    #[cfg(windows)]
                    active.store(true, Ordering::SeqCst);
                    if chunks.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    std::thread::spawn(move || {
        let mut last_sent: Option<Instant> = None;
        while let Ok(first) = received.recv() {
            // A frame closes no sooner than one frame after the previous one was sent.
            let started = match last_sent {
                Some(sent) if sent.elapsed() < TERMINAL_FRAME => sent,
                _ => Instant::now(),
            };
            let mut frame = first;
            let mut ended = false;
            while frame.len() < MAX_FRAME_BYTES {
                let Some(left) = TERMINAL_FRAME.checked_sub(started.elapsed()) else {
                    break;
                };
                match received.recv_timeout(left) {
                    Ok(more) => frame.extend_from_slice(&more),
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        ended = true;
                        break;
                    }
                }
            }
            if !ended && frame.len() < MAX_FRAME_BYTES {
                if let Some(left) = TERMINAL_FRAME.checked_sub(started.elapsed()) {
                    std::thread::sleep(left);
                }
            }
            #[cfg(unix)]
            if let Some(observed) = &observed {
                observed.record(&frame);
            }
            sink(TerminalEvent::Output(frame));
            last_sent = Some(Instant::now());
            if ended {
                break;
            }
        }
        sink(TerminalEvent::Exited);
    });
}

#[cfg(unix)]
fn foreground_pgid_of(master: &SharedMaster) -> Option<i32> {
    let guard = lock(master);
    let master = guard.as_ref()?;
    master.process_group_leader()
}
