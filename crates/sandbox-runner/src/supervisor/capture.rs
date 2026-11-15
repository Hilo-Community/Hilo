//! Running a process to completion with its output bounded: how a `run` ends and what it saw.

use std::io::{self, Read};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::SandboxedChild;
use crate::error::SandboxError;
use crate::spec::ResourceLimits;

/// How often `run` looks at its process.
const POLL_INTERVAL: Duration = Duration::from_millis(2);
/// After the process is gone, how long to wait for the output readers (a process that left the
/// group may still hold a pipe open).
const DRAIN_GRACE: Duration = Duration::from_secs(2);

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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
