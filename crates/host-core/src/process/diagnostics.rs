//! Bounded stderr diagnostics for supervised services that fail during startup.
use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const LIMIT: usize = 8 * 1024;

#[derive(Default)]
struct Tail {
    bytes: VecDeque<u8>,
    closed: bool,
}

#[derive(Clone)]
pub(crate) struct StderrCapture(Arc<(Mutex<Tail>, Condvar)>);

impl StderrCapture {
    pub(crate) fn start(mut reader: impl Read + Send + 'static) -> Self {
        let state = Arc::new((Mutex::new(Tail::default()), Condvar::new()));
        let capture = Self(Arc::clone(&state));
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        let mut tail = state
                            .0
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        tail.bytes.extend(&buffer[..count]);
                        let excess = tail.bytes.len().saturating_sub(LIMIT);
                        tail.bytes.drain(..excess);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            state
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closed = true;
            state.1.notify_all();
        });
        capture
    }

    pub(crate) fn with_context(&self, reason: &str) -> String {
        let guard = self
            .0
             .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // stdout EOF may arrive just before the stderr reader consumes the final error.
        // Only failure reporting waits, and a still-running process cannot stall it.
        let (tail, _) = self
            .0
             .1
            .wait_timeout_while(guard, Duration::from_millis(50), |tail| !tail.closed)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes: Vec<_> = tail.bytes.iter().copied().collect();
        let detail = String::from_utf8_lossy(&bytes);
        if detail.trim().is_empty() {
            reason.to_owned()
        } else {
            format!("{reason}: {}", detail.trim())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_errors_keep_the_tail_and_cannot_grow_without_bound() {
        let input = format!("{}permission denied", "x".repeat(LIMIT * 4));
        let capture = StderrCapture::start(std::io::Cursor::new(input));
        let message = capture.with_context("service exited");
        assert!(message.starts_with("service exited: "));
        assert!(message.ends_with("permission denied"));
        assert!(message.len() <= LIMIT + "service exited: ".len());
    }

    #[test]
    fn empty_stderr_keeps_the_original_error() {
        let capture = StderrCapture::start(std::io::Cursor::new(Vec::new()));
        assert_eq!(capture.with_context("disconnected"), "disconnected");
    }
}
