//! LOT-012 common guard rails, with real processes: budgets, process-tree cleanup, private
//! temporary directory, fail-closed refusals. The engine used here isolates nothing (see
//! `common`); what is tested is the supervision every OS engine sits on.
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sandbox_runner::{
    MemoryAuditSink, NetworkPolicy, OsNativeSandbox, SandboxError, SandboxRunner,
    SecurityEventKind, Termination, TMP_DIR_NAME,
};

mod common;
use common::*;

fn runner(sink: &Arc<MemoryAuditSink>) -> OsNativeSandbox<PassthroughEngine> {
    OsNativeSandbox::new(PassthroughEngine, sink.clone()).unwrap()
}

#[test]
fn run_captures_output_and_the_exit_status() {
    let (_dir, root) = worktree();
    let sink = Arc::new(MemoryAuditSink::default());
    let output = runner(&sink)
        .run(&sh(&root, "echo out; echo err >&2; exit 3"))
        .unwrap();
    assert_eq!(output.stdout, b"out\n");
    assert_eq!(output.stderr, b"err\n");
    assert_eq!(
        output.termination,
        Termination::Exited {
            code: Some(3),
            signal: None
        }
    );
    assert!(!output.termination.success());
    assert!(!output.stdout_truncated && !output.incomplete);
    assert!(sink.events().is_empty());
}

#[test]
fn the_environment_is_exactly_what_the_request_gives_and_tmpdir_is_private_and_purged() {
    let (_dir, root) = worktree();
    let sink = Arc::new(MemoryAuditSink::default());
    let mut spec = sh(
        &root,
        r#"echo "$TMPDIR"; echo "$TMP"; echo "$TEMP"; echo "[$HOME]"; echo "[$CUSTOM]"; touch "$TMPDIR/scratch""#,
    );
    spec.env.insert("CUSTOM".into(), "yes".into());
    let output = runner(&sink).run(&spec).unwrap();
    let lines: Vec<String> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let tmp = std::path::PathBuf::from(&lines[0]);
    assert!(tmp.starts_with(root.join(TMP_DIR_NAME)), "{tmp:?}");
    assert_eq!(lines[1], lines[0]);
    assert_eq!(lines[2], lines[0]);
    assert_eq!(lines[3], "[]", "HOME was inherited from the Host");
    assert_eq!(lines[4], "[yes]");
    // Purged at the end of the run, and the worktree is back to what it was.
    assert!(!tmp.exists());
    assert!(!root.join(TMP_DIR_NAME).exists());
    let entries: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("a.txt")]);
}

/// Scenario 3 (time): the whole tree dies at the budget, however the command spreads.
#[test]
fn a_command_that_outlives_its_budget_is_stopped_with_every_process_it_started() {
    let (_dir, root) = worktree();
    let sink = Arc::new(MemoryAuditSink::default());
    let mut spec = sh(
        &root,
        "sleep 600 & echo $!; sleep 600 & echo $!; (sleep 600 & echo $!; wait) & wait",
    );
    spec.limits.wall_timeout = Duration::from_millis(400);
    let started = Instant::now();
    let output = runner(&sink).run(&spec).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(output.termination, Termination::WallTimeout);
    let children = pids(&output.stdout);
    assert_eq!(children.len(), 3, "{:?}", output.stdout);
    for pid in children {
        assert!(wait_until_dead(pid), "process {pid} survived");
    }
    assert_eq!(sink.kinds(), vec![SecurityEventKind::WallTimeoutKilled]);
}

/// Scenario 3 (output): 500 MB of output is cut at the budget and the producer is stopped.
#[test]
fn a_flood_of_output_is_truncated_at_the_budget_and_the_producer_is_stopped() {
    let (_dir, root) = worktree();
    let sink = Arc::new(MemoryAuditSink::default());
    let mut spec = sh(&root, "head -c 500000000 /dev/zero; echo never");
    spec.limits.max_output_bytes = 1024 * 1024;
    let started = Instant::now();
    let output = runner(&sink).run(&spec).unwrap();
    assert_eq!(output.termination, Termination::OutputLimit);
    assert_eq!(
        output.stdout.len(),
        1024 * 1024,
        "exactly the budget is kept"
    );
    assert!(output.stdout_truncated && !output.stderr_truncated);
    assert!(output.stdout_bytes > 1024 * 1024 && output.stdout_bytes < 500_000_000);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(sink.kinds(), vec![SecurityEventKind::OutputLimitKilled]);

    // Each stream has its own budget: stderr flooding is cut too.
    let mut spec = sh(&root, "head -c 5000000 /dev/zero >&2");
    spec.limits.max_output_bytes = 4096;
    let output = runner(&sink).run(&spec).unwrap();
    assert_eq!(output.termination, Termination::OutputLimit);
    assert_eq!(output.stderr.len(), 4096);
    assert!(output.stderr_truncated && !output.stdout_truncated);
}

/// Scenario 3 (processes): many processes spawned by nested shells are all gone afterwards.
#[test]
fn a_process_explosion_is_cleaned_up_completely() {
    let (_dir, root) = worktree();
    let sink = Arc::new(MemoryAuditSink::default());
    let mut spec = sh(
        &root,
        r#"for i in 1 2 3 4 5 6 7 8 9 10 11 12; do sh -c 'for j in 1 2 3 4 5; do sleep 600 & echo $!; done; wait' & done; wait"#,
    );
    spec.limits.wall_timeout = Duration::from_millis(1500);
    let output = runner(&sink).run(&spec).unwrap();
    assert_eq!(output.termination, Termination::WallTimeout);
    let all = pids(&output.stdout);
    assert!(all.len() >= 30, "only {} processes were started", all.len());
    for pid in all {
        assert!(wait_until_dead(pid), "process {pid} survived");
    }
}

#[test]
fn background_processes_left_by_a_command_that_exits_are_killed_with_it() {
    let (_dir, root) = worktree();
    let sink = Arc::new(MemoryAuditSink::default());
    let started = Instant::now();
    let output = runner(&sink)
        .run(&sh(&root, "sleep 600 & echo $!"))
        .unwrap();
    assert!(output.termination.success());
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!output.incomplete);
    let leftover = pids(&output.stdout);
    assert_eq!(leftover.len(), 1);
    assert!(
        wait_until_dead(leftover[0]),
        "a daemonized process survived"
    );
}

/// Scenario 4: a sandbox that fails its self-test is never built, and nothing runs.
#[test]
fn fail_closed_a_failed_self_test_means_no_runner_and_no_execution() {
    let (_dir, root) = worktree();
    let canary = root.join("canary-ran");
    let sink = Arc::new(MemoryAuditSink::default());
    let result = OsNativeSandbox::new(
        BrokenSelfTestEngine {
            canary: canary.clone(),
        },
        sink.clone(),
    );
    assert!(matches!(
        result,
        Err(SandboxError::SandboxUnavailable { .. })
    ));
    assert!(!canary.exists(), "the command ran on the host");
    assert_eq!(sink.kinds(), vec![SecurityEventKind::SandboxUnavailable]);
}

#[test]
fn fail_closed_an_engine_that_cannot_isolate_a_request_runs_nothing() {
    let (_dir, root) = worktree();
    let canary = root.join("canary-ran");
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = OsNativeSandbox::new(FailingPrepareEngine, sink.clone()).unwrap();
    let script = format!("touch {}", canary.display());
    assert!(matches!(
        runner.run(&sh(&root, &script)),
        Err(SandboxError::SandboxUnavailable { .. })
    ));
    assert!(matches!(
        runner.spawn(&sh(&root, &script)),
        Err(SandboxError::SandboxUnavailable { .. })
    ));
    assert!(!canary.exists(), "the command ran on the host");
    assert_eq!(
        sink.kinds(),
        vec![
            SecurityEventKind::SandboxUnavailable,
            SecurityEventKind::SandboxUnavailable
        ]
    );
    // The private temporary directory of a refused run is not left behind.
    assert!(!root.join(TMP_DIR_NAME).exists());
}

/// Until the OS engines exist there is no native sandbox, and no unisolated fallback.
#[cfg(not(target_os = "macos"))]
#[test]
fn fail_closed_the_native_sandbox_refuses_while_no_engine_is_implemented() {
    let sink = Arc::new(MemoryAuditSink::default());
    assert!(matches!(
        native_sandbox(sink.clone()),
        Err(SandboxError::SandboxUnavailable { .. })
    ));
    assert_eq!(sink.kinds(), vec![SecurityEventKind::SandboxUnavailable]);
}

#[test]
fn limits_and_network_policies_the_engine_cannot_enforce_are_refused_not_ignored() {
    let (_dir, root) = worktree();
    let canary = root.join("canary-ran");
    let script = format!("touch {}", canary.display());
    let sink = Arc::new(MemoryAuditSink::default());
    let weak = runner(&sink);
    let proxy: SocketAddr = "127.0.0.1:8080".parse().unwrap();

    let mut memory = sh(&root, &script);
    memory.limits.max_memory_bytes = Some(1 << 30);
    let mut processes = sh(&root, &script);
    processes.limits.max_processes = Some(64);
    let mut cpu = sh(&root, &script);
    cpu.limits.max_cpu_time = Some(Duration::from_secs(5));
    for spec in [&memory, &processes, &cpu] {
        assert!(matches!(
            weak.run(spec),
            Err(SandboxError::LimitNotEnforceable { .. })
        ));
    }
    let mut network = sh(&root, &script);
    network.network = NetworkPolicy::LocalProxyOnly(proxy);
    assert!(matches!(
        weak.run(&network),
        Err(SandboxError::NetworkPolicyNotEnforceable { .. })
    ));
    assert!(!canary.exists());
    assert_eq!(
        sink.kinds(),
        vec![SecurityEventKind::PolicyNotEnforceable; 4]
    );

    // An engine that reports the capability accepts them.
    let strong = OsNativeSandbox::new(FullCapabilitiesEngine, sink.clone()).unwrap();
    let mut accepted = sh(&root, "echo ok");
    accepted.limits.max_memory_bytes = Some(1 << 30);
    accepted.limits.max_processes = Some(64);
    accepted.limits.max_cpu_time = Some(Duration::from_secs(5));
    accepted.network = NetworkPolicy::LocalProxyOnly(proxy);
    assert!(strong.run(&accepted).unwrap().termination.success());
}

#[test]
fn invalid_requests_are_refused_before_anything_runs() {
    let (_dir, root) = worktree();
    let canary = root.join("canary-ran");
    let script = format!("touch {}", canary.display());
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = runner(&sink);

    let mut relative_program = sh(&root, &script);
    relative_program.program = "sh".into();
    let mut zero_timeout = sh(&root, &script);
    zero_timeout.limits.wall_timeout = Duration::ZERO;
    let mut public_proxy = sh(&root, &script);
    public_proxy.network = NetworkPolicy::LocalProxyOnly("8.8.8.8:53".parse().unwrap());
    let mut bad_env = sh(&root, &script);
    bad_env.env.insert("A=B".into(), "x".into());
    let mut missing_read_only = sh(&root, &script);
    missing_read_only.read_only_paths = vec![root.join("../does-not-exist")];
    let mut whole_disk = sh(&root, &script);
    whole_disk.read_only_paths = vec!["/".into()];
    let mut writable_inside = sh(&root, &script);
    writable_inside.read_only_paths = vec![root.clone()];
    for spec in [
        relative_program,
        zero_timeout,
        public_proxy,
        bad_env,
        missing_read_only,
        whole_disk,
        writable_inside,
    ] {
        assert!(
            matches!(runner.run(&spec), Err(SandboxError::InvalidSpec(_))),
            "{spec:?}"
        );
    }
    assert!(!canary.exists());
}

/// Scenario 1 (host side): a working directory that leaves the worktree is refused and audited.
#[test]
fn a_working_directory_outside_the_worktree_is_refused_and_audited() {
    let (_dir, root) = worktree();
    let (_other, outside) = worktree();
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = runner(&sink);
    for cwd in [
        outside.clone(),
        root.join("escape"),
        root.join("../elsewhere"),
    ] {
        let mut spec = sh(&root, "touch pwned");
        spec.cwd = cwd.clone();
        assert!(
            matches!(runner.run(&spec), Err(SandboxError::Path(_))),
            "{cwd:?} accepted"
        );
    }
    assert!(!outside.join("pwned").exists());
    assert_eq!(sink.kinds(), vec![SecurityEventKind::PathEscapeBlocked; 3]);
    // A cwd that is a file is invalid, not an escape.
    let mut spec = sh(&root, "true");
    spec.cwd = root.join("a.txt");
    assert!(matches!(
        runner.run(&spec),
        Err(SandboxError::InvalidSpec(_))
    ));
}

/// A long-lived process (an agent): piped stdio, killed with its whole tree when dropped.
#[test]
fn a_long_lived_process_talks_over_its_pipes_and_dies_with_its_handle() {
    let (_dir, root) = worktree();
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = runner(&sink);

    let mut child = runner
        .spawn(&sh(
            &root,
            "read line; echo \"got $line\"; sleep 600 & echo $!; wait",
        ))
        .unwrap();
    let mut stdin = child.take_stdin().unwrap();
    let mut stdout = BufReader::new(child.take_stdout().unwrap());
    writeln!(stdin, "hello").unwrap();
    let mut answer = String::new();
    stdout.read_line(&mut answer).unwrap();
    assert_eq!(answer, "got hello\n");
    let mut grandchild = String::new();
    stdout.read_line(&mut grandchild).unwrap();
    let grandchild: u32 = grandchild.trim().parse().unwrap();
    let pid = child.id();
    assert!(is_alive(pid) && is_alive(grandchild));
    assert!(!child.timed_out());
    drop(child);
    assert!(wait_until_dead(pid), "the process survived its handle");
    assert!(wait_until_dead(grandchild), "its child survived its handle");
    // Its private temporary directory went with it.
    assert!(!root.join(TMP_DIR_NAME).exists());
}

#[test]
fn a_long_lived_process_is_stopped_at_the_end_of_its_lifetime_budget() {
    let (_dir, root) = worktree();
    let sink = Arc::new(MemoryAuditSink::default());
    let mut spec = sh(&root, "sleep 600 & echo $!; wait");
    spec.limits.wall_timeout = Duration::from_millis(500);
    let mut child = runner(&sink).spawn(&spec).unwrap();
    let mut line = String::new();
    BufReader::new(child.take_stdout().unwrap())
        .read_line(&mut line)
        .unwrap();
    let grandchild: u32 = line.trim().parse().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert!(child.timed_out());
    assert!(wait_until_dead(grandchild));
}

const HELPER_ENV: &str = "COLORIDE_SANDBOX_HOST_HELPER_DIR";

/// The "Host" of the abrupt-exit scenario is this test binary re-executed in a helper mode, then
/// killed with `SIGKILL` so that none of its destructors run.
#[test]
fn the_sandboxed_tree_does_not_outlive_a_host_that_dies_abruptly() {
    if let Ok(dir) = std::env::var(HELPER_ENV) {
        let root = std::path::PathBuf::from(dir);
        let sink = Arc::new(MemoryAuditSink::default());
        let mut child = runner(&sink)
            .spawn(&sh(&root, "sleep 600 & echo $!; wait"))
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.take_stdout().unwrap())
            .read_line(&mut line)
            .unwrap();
        println!("CHILD {}", child.id());
        println!("GRANDCHILD {}", line.trim());
        std::thread::sleep(Duration::from_secs(600));
        return;
    }
    let (_dir, root) = worktree();
    let mut host = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "the_sandboxed_tree_does_not_outlive_a_host_that_dies_abruptly",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER_ENV, &root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // libtest prints "test <name> ... " without a newline before the helper's own output, so
    // the markers are looked for anywhere in the line. A reader thread and a deadline keep a
    // helper that never prints from hanging this test.
    let (lines_tx, lines_rx) = std::sync::mpsc::channel();
    let stdout = host.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut child = None;
    let mut grandchild = None;
    while child.is_none() || grandchild.is_none() {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(line) = lines_rx.recv_timeout(left) else {
            let _ = host.kill();
            panic!("the helper Host did not report its processes (child {child:?}, grandchild {grandchild:?})");
        };
        // "GRANDCHILD " contains "CHILD ": look for it first.
        let marker = |name: &str| {
            line.find(name)
                .and_then(|at| line[at + name.len()..].trim().parse::<u32>().ok())
        };
        if let Some(pid) = marker("GRANDCHILD ") {
            grandchild = Some(pid);
        } else if let Some(pid) = marker("CHILD ") {
            child = Some(pid);
        }
    }
    let (child, grandchild) = (child.unwrap(), grandchild.unwrap());
    assert!(is_alive(child) && is_alive(grandchild));
    host.kill().unwrap(); // SIGKILL: no destructor runs in the helper
    host.wait().unwrap();
    assert!(
        wait_until_dead(child),
        "the sandboxed process outlived the Host"
    );
    assert!(wait_until_dead(grandchild), "its child outlived the Host");
}

#[test]
fn reading_the_pipes_of_a_killed_tree_does_not_block() {
    // Sanity check of the helper used above: `Read` on a closed pipe returns 0.
    let mut child = Command::new("/bin/sh")
        .args(["-c", "true"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut buffer = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut buffer)
        .unwrap();
    child.wait().unwrap();
    assert!(buffer.is_empty());
}
