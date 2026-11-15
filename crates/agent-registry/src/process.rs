//! Running a program with a deadline and a bounded amount of output.

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// Reads at most `limit` bytes and discards the rest, so a chatty child never blocks on a full
/// pipe nor grows our memory.
fn drain(pipe: impl Read + Send + 'static, limit: usize) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0u8; 4096];
        let mut pipe = pipe;
        while let Ok(count) = pipe.read(&mut buffer) {
            if count == 0 {
                break;
            }
            let room = limit.saturating_sub(kept.len());
            kept.extend_from_slice(&buffer[..count.min(room)]);
        }
        String::from_utf8_lossy(&kept).into_owned()
    })
}

/// Runs `command` to the end, or kills it after `timeout`. `limit` caps what is kept of each of
/// stdout and stderr.
pub(crate) fn run_bounded(
    mut command: Command,
    timeout: Duration,
    limit: usize,
) -> Result<Output, String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flashes when the app has none.
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let stdout = child.stdout.take().map(|pipe| drain(pipe, limit));
    let stderr = child.stderr.take().map(|pipe| drain(pipe, limit));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "it did not finish within {} seconds",
                    timeout.as_secs()
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error.to_string()),
        }
    };
    let collect = |handle: Option<thread::JoinHandle<String>>| {
        handle
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    };
    Ok(Output {
        status,
        stdout: collect(stdout),
        stderr: collect(stderr),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn output_is_captured_and_bounded() {
        let output = run_bounded(
            sh("printf out; printf err >&2; head -c 100000 /dev/zero | tr '\\0' x"),
            Duration::from_secs(10),
            1000,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1000);
        assert_eq!(output.stderr, "err");
    }

    #[test]
    fn a_program_that_hangs_is_killed_at_the_deadline() {
        let started = Instant::now();
        let error = run_bounded(sh("sleep 30"), Duration::from_millis(200), 100).unwrap_err();
        assert!(error.contains("did not finish"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_an_error() {
        assert!(run_bounded(
            Command::new("/nonexistent/program"),
            Duration::from_secs(1),
            100
        )
        .is_err());
    }
}
