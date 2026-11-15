//! The lifetime budget of a long-lived process.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Runs `kill` at a deadline, unless stopped first.
#[derive(Debug)]
pub(super) struct LifetimeTimer {
    cancel: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl LifetimeTimer {
    pub(super) fn start(
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

    /// Returns once the timer thread is gone: it can no longer kill anything.
    pub(super) fn stop(mut self) {
        drop(self.cancel.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
