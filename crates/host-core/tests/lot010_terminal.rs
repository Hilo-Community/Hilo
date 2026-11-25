//! LOT-010 — integrated terminal on a real pseudo-terminal with real shells.
#![cfg(any(unix, windows))]
#![allow(clippy::disallowed_methods)] // `ps` / `tasklist` and the helper Host in tests

use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use host_core::{
    GatedCapability, TerminalEvent, TerminalLaunch, TerminalSession, TerminalSize, TrustError,
    WorkspaceTrustGate, TERMINAL_FRAME,
};

const SIZE: TerminalSize = TerminalSize {
    cols: 100,
    rows: 30,
};
const HELPER_ENV: &str = "COLORIDE_LOT010_TERMINAL_HOST_DIR";

fn gate(dir: &std::path::Path, trusted: bool) -> WorkspaceTrustGate {
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let gate = WorkspaceTrustGate::open(dir.join("trust.json"), &workspace).unwrap();
    if trusted {
        gate.grant().unwrap();
    }
    gate
}

#[cfg(unix)]
fn sh(script: &str) -> TerminalLaunch {
    TerminalLaunch {
        program: Some("/bin/sh".into()),
        args: vec!["-c".into(), script.into()],
    }
}

#[cfg(windows)]
fn cmd_launch(args: &[&str]) -> TerminalLaunch {
    TerminalLaunch {
        program: Some("cmd.exe".into()),
        args: args.iter().map(|s| (*s).to_string()).collect(),
    }
}

#[cfg(windows)]
fn stubborn_job_launch() -> TerminalLaunch {
    TerminalLaunch {
        program: Some("powershell.exe".into()),
        args: vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "$p = Start-Process -FilePath 'ping.exe' -ArgumentList '-n','600','127.0.0.1' -PassThru -WindowStyle Hidden; [Console]::Out.WriteLine('job=' + $p.Id + '='); Wait-Process -Id $p.Id".into(),
        ],
    }
}

fn open(
    gate: &WorkspaceTrustGate,
    launch: &TerminalLaunch,
) -> (TerminalSession, Receiver<(Instant, TerminalEvent)>) {
    let (sender, events) = mpsc::channel();
    let sender = std::sync::Mutex::new(sender);
    let session = TerminalSession::spawn(
        gate,
        launch,
        SIZE,
        Arc::new(move |event| {
            let _ = sender.lock().unwrap().send((Instant::now(), event));
        }),
        None,
        None,
    )
    .unwrap();
    (session, events)
}

/// Collects output until `needle` appears (or the shell exits).
fn read_until(events: &Receiver<(Instant, TerminalEvent)>, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut output = Vec::new();
    while !String::from_utf8_lossy(&output).contains(needle) {
        let left = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        let Ok((_, event)) = events.recv_timeout(left) else {
            panic!(
                "no {needle:?} in terminal output: {:?}",
                String::from_utf8_lossy(&output)
            );
        };
        match event {
            TerminalEvent::Output(bytes) => output.extend(bytes),
            TerminalEvent::Exited => break,
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

#[cfg(unix)]
#[path = "../../../test-support/process.rs"]
mod process_checks;
#[cfg(unix)]
use process_checks::is_alive;

#[cfg(windows)]
fn is_alive(pid: u32) -> bool {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
}

fn wait_until_dead(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

#[test]
fn an_untrusted_workspace_gets_no_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let gate = gate(dir.path(), false);
    let refused = TerminalSession::spawn(
        &gate,
        &TerminalLaunch::default(),
        SIZE,
        Arc::new(|_| {}),
        None,
        None,
    );
    assert!(matches!(
        refused,
        Err(host_core::TerminalError::Trust(TrustError::Untrusted {
            capability: GatedCapability::Terminal,
            ..
        }))
    ));
    assert_eq!(gate.spawned_process_count(), 0);
}

#[cfg(unix)]
#[test]
fn keystrokes_reach_the_shell_in_the_workspace_and_its_output_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let gate = gate(dir.path(), true);
    let (session, events) = open(&gate, &sh("exec /bin/sh -i"));
    session.write(b"echo \"ok-$((40+2))\"; pwd\n").unwrap();
    // One frame may hold both answers: read until the later one, then check both.
    let output = read_until(&events, "/workspace");
    assert!(output.contains("ok-42"), "{output}");
    assert_eq!(gate.spawned_process_count(), 1);

    // Resizing reaches the pty (`stty size` prints "rows cols").
    session
        .resize(TerminalSize {
            cols: 132,
            rows: 41,
        })
        .unwrap();
    session.write(b"stty size\n").unwrap();
    assert!(read_until(&events, "41 132").contains("41 132"));
}

/// The terminal agent-status poller's foundation: knowing when something other than the shell
/// itself is in the pty's foreground.
#[cfg(unix)]
#[test]
fn foreground_pgid_differs_from_the_shell_while_a_job_runs_in_it() {
    let dir = tempfile::tempdir().unwrap();
    let gate = gate(dir.path(), true);
    let (session, events) = open(&gate, &sh("exec /bin/sh -i"));
    session.write(b"echo ready\n").unwrap();
    let _ = read_until(&events, "ready");
    // Idle at its own prompt: the foreground process group is the shell's.
    assert_eq!(session.foreground_pgid(), Some(session.process_id() as i32));

    session.write(b"sleep 5\n").unwrap();
    let saw_foreground_job = (0..60).any(|_| {
        std::thread::sleep(Duration::from_millis(50));
        session
            .foreground_pgid()
            .is_some_and(|pgid| pgid != session.process_id() as i32)
    });
    assert!(
        saw_foreground_job,
        "expected `sleep` to become the foreground job"
    );
}

#[cfg(windows)]
#[test]
fn keystrokes_reach_the_shell_in_the_workspace_and_its_output_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let gate = gate(dir.path(), true);
    let (session, events) = open(&gate, &cmd_launch(&["/Q"]));
    session
        .write(b"set /a 40+2 & echo. & echo CWD=%cd%\r\n")
        .unwrap();
    let output = read_until(&events, "CWD=");
    assert!(
        output.contains("42") && output.contains("\\workspace"),
        "{output}"
    );
    assert_eq!(gate.spawned_process_count(), 1);

    session
        .resize(TerminalSize {
            cols: 132,
            rows: 41,
        })
        .unwrap();
    session.write(b"mode con\r\n").unwrap();
    let resized = read_until(&events, "132");
    assert!(
        resized.contains("41") && resized.contains("132"),
        "{resized}"
    );
}

#[test]
fn output_is_delivered_in_frames_at_most_one_per_16_ms() {
    let dir = tempfile::tempdir().unwrap();
    let gate = gate(dir.path(), true);
    #[cfg(unix)]
    let launch = sh("i=0; while [ $i -lt 3000 ]; do echo line$i; i=$((i+1)); done");
    #[cfg(windows)]
    let launch = cmd_launch(&["/d", "/c", "for /L %i in (0,1,2999) do @echo line%i"]);
    let (_session, events) = open(&gate, &launch);
    let mut frames = Vec::new();
    let mut output = Vec::new();
    loop {
        let (at, event) = events.recv_timeout(Duration::from_secs(20)).unwrap();
        match event {
            TerminalEvent::Output(bytes) => {
                frames.push(at);
                output.extend(bytes);
            }
            TerminalEvent::Exited => break,
        }
    }
    let text = String::from_utf8_lossy(&output);
    assert!(text.contains("line0") && text.contains("line2999"));
    assert!(
        frames.len() < 3000 / 10,
        "{} frames for 3000 lines: not batched",
        frames.len()
    );
    let tolerance = Duration::from_millis(1);
    // The last frame is flushed as soon as the shell exits.
    for pair in frames[..frames.len() - 1].windows(2) {
        assert!(
            pair[1] - pair[0] + tolerance >= TERMINAL_FRAME,
            "frames {:?} apart",
            pair[1] - pair[0]
        );
    }
}

fn parse_job_pid(output: &str) -> u32 {
    let rest = output.split("job=").nth(1).expect("job= prefix");
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().expect("job pid")
}

#[test]
fn closing_the_terminal_kills_the_shell_and_its_background_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let gate = gate(dir.path(), true);
    #[cfg(unix)]
    let launch = sh("sleep 600 & echo job=$!; wait");
    #[cfg(windows)]
    let launch = stubborn_job_launch();
    let (session, events) = open(&gate, &launch);
    #[cfg(unix)]
    let output = read_until(&events, "\n");
    #[cfg(windows)]
    let output = read_until(&events, "=");
    #[cfg(windows)]
    let output = if output.matches('=').count() < 2 {
        format!("{output}{}", read_until(&events, "\n"))
    } else {
        output
    };
    let job = parse_job_pid(&output);
    let shell = session.process_id();
    assert!(is_alive(shell) && is_alive(job));
    drop(session);
    assert!(wait_until_dead(shell));
    assert!(wait_until_dead(job), "background job survived the terminal");
}

/// Not a test of its own: the Host of the scenario below (does nothing unless asked).
#[test]
fn helper_host_with_a_terminal() {
    let Ok(dir) = std::env::var(HELPER_ENV) else {
        return;
    };
    let gate = gate(std::path::Path::new(&dir), true);
    #[cfg(unix)]
    let launch = sh("sleep 600 & echo job=$!; wait");
    #[cfg(windows)]
    let launch = stubborn_job_launch();
    let (session, events) = open(&gate, &launch);
    #[cfg(unix)]
    let output = read_until(&events, "\n");
    #[cfg(windows)]
    let output = read_until(&events, "\n");
    let job = parse_job_pid(&output);
    println!("\nPIDS {} {}", session.process_id(), job);
    std::thread::sleep(Duration::from_secs(600));
}

#[test]
fn a_host_killed_abruptly_leaves_no_terminal_behind() {
    use std::io::BufRead;
    let dir = tempfile::tempdir().unwrap();
    let mut host = Command::new(std::env::current_exe().unwrap())
        .args([
            "helper_host_with_a_terminal",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_ENV, dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut pids = None;
    for line in std::io::BufReader::new(host.stdout.take().unwrap()).lines() {
        let line = line.unwrap();
        if let Some(rest) = line.strip_prefix("PIDS ") {
            let mut parts = rest
                .split_whitespace()
                .map(|part| part.parse::<u32>().unwrap());
            pids = Some((parts.next().unwrap(), parts.next().unwrap()));
            break;
        }
    }
    let (shell, job) = pids.expect("helper host reported its terminal");
    assert!(is_alive(shell) && is_alive(job));
    host.kill().unwrap(); // SIGKILL
    host.wait().unwrap();
    assert!(
        wait_until_dead(shell),
        "shell orphaned by an abrupt Host exit"
    );
    assert!(wait_until_dead(job), "job orphaned by an abrupt Host exit");
}
