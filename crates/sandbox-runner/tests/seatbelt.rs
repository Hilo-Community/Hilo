//! LOT-012, macOS engine: the blocking security scenarios against the real kernel sandbox.
//!
//! Everything runs with a FAKE home directory holding fake secrets; the real `~/.ssh` and
//! friends are never touched. Process, file and network attempts are made by real programs
//! inside `sandbox-exec`; the Host side checks what actually happened on the disk and on the
//! listeners it opened.
#![cfg(target_os = "macos")]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sandbox_runner::{
    native_sandbox, MemoryAuditSink, NetworkPolicy, OsNativeSandbox, SandboxError, SandboxRunner,
    SandboxSpec, SeatbeltEngine, SecurityEventKind, Termination,
};

mod common;
use common::world::{lines, sh, World, TRY};

fn world() -> World {
    common::world::world("Library/Caches/Coloride/worktrees/task")
}

fn runner(world: &World, sink: &Arc<MemoryAuditSink>) -> OsNativeSandbox<SeatbeltEngine> {
    let engine = SeatbeltEngine::new(&world.home).unwrap();
    OsNativeSandbox::new(engine, sink.clone()).expect("the Seatbelt self-test must pass")
}

/// The kernel reports denials to the log with a delay: wait for the audit event.
fn wait_for_event(
    sink: &MemoryAuditSink,
    kind: SecurityEventKind,
    detail_contains: &str,
) -> sandbox_runner::SecurityAuditEvent {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Some(event) = sink
            .events()
            .into_iter()
            .find(|event| event.kind == kind && event.detail.contains(detail_contains))
        {
            return event;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!(
        "no {kind:?} event mentioning {detail_contains:?}; got {:#?}",
        sink.events()
    );
}

/// Scenario 1: secrets of the host, `..`, symbolic links out of the worktree.
#[test]
fn blocking_scenario_1_secrets_path_traversal_and_symlinks_are_refused_by_the_kernel() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    symlink(
        world.home.join(".ssh/id_rsa"),
        world.worktree.join("escape-ssh"),
    )
    .unwrap();
    symlink("/etc/passwd", world.worktree.join("escape-passwd")).unwrap();

    let script = format!(
        r#"{TRY}H="$1"; O="$2"
echo "ok" > inside.txt && echo "ALLOWED:inside"
try ssh /bin/cat "$H/.ssh/id_rsa"
try aws /bin/cat "$H/.aws/credentials"
try gnupg /bin/cat "$H/.gnupg/private.key"
try keychain /bin/cat "$H/Library/Keychains/login.keychain-db"
try netrc /bin/cat "$H/.netrc"
try gh-token /bin/cat "$H/.config/gh/hosts.yml"
try other-project /bin/cat "$O/other/other.txt"
try parent-write /bin/sh -c 'echo x > ../outside.txt'
try absolute-write /bin/sh -c "echo x > $O/new.txt"
try home-write /bin/sh -c "echo x > $H/evil.txt"
try tmp-write /bin/sh -c 'echo x > /tmp/coloride-evil-test'
try link-to-ssh /bin/cat escape-ssh
try link-to-passwd /bin/cat escape-passwd
try list-home /bin/ls "$H"
try list-users /bin/ls /Users
try list-ssh-dir /bin/ls "$H/.ssh"
"#
    );
    let output = runner(&world, &sink)
        .run(&sh(&world, &script, &[&world.home, &world.outside]))
        .unwrap();
    assert!(output.termination.success(), "{output:?}");
    let lines = lines(&output);

    assert!(lines.contains(&"ALLOWED:inside".to_string()), "{lines:?}");
    assert_eq!(
        fs::read_to_string(world.worktree.join("inside.txt")).unwrap(),
        "ok\n"
    );
    let leaked: Vec<_> = lines
        .iter()
        .filter(|line| line.starts_with("LEAKED"))
        .collect();
    assert!(
        leaked.is_empty(),
        "the sandbox let these through: {leaked:?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.starts_with("DENIED"))
            .count(),
        16,
        "{lines:?}"
    );
    for secret in [
        "SSH-SECRET",
        "AWS-SECRET",
        "GPG-SECRET",
        "KEYCHAIN-SECRET",
        "NETRC-SECRET",
        "GH-SECRET",
    ] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
    }
    // And nothing was written outside, whatever the process reported.
    for path in [
        world.worktree.parent().unwrap().join("outside.txt"),
        world.outside.join("new.txt"),
        world.home.join("evil.txt"),
        PathBuf::from("/tmp/coloride-evil-test"),
    ] {
        assert!(!path.exists(), "{path:?} was written");
    }
    assert_eq!(
        fs::read_to_string(world.home.join(".ssh/id_rsa")).unwrap(),
        "SSH-SECRET"
    );

    // The kernel's refusals reach the audit log, tagged with this run.
    let event = wait_for_event(&sink, SecurityEventKind::FileAccessDenied, "id_rsa");
    assert!(event.detail.contains("file-read"), "{event:?}");
    wait_for_event(&sink, SecurityEventKind::FileAccessDenied, "other.txt");
}

/// Scenario 2: no outbound network, no cloud metadata, no local sockets.
#[test]
fn blocking_scenario_2_network_cloud_metadata_and_sockets_are_refused() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    tcp.set_nonblocking(true).unwrap();
    let port = tcp.local_addr().unwrap().port();
    // Unix socket paths are limited to about 100 bytes: a short directory of its own.
    let socket_dir = tempfile::Builder::new()
        .prefix("cs")
        .tempdir_in("/tmp")
        .unwrap();
    let plain_socket = socket_dir.path().join("plain.sock");
    let docker_socket = socket_dir.path().join("docker.sock");
    let unix = [
        UnixListener::bind(&plain_socket).unwrap(),
        UnixListener::bind(&docker_socket).unwrap(),
    ];
    for socket in &unix {
        socket.set_nonblocking(true).unwrap();
    }

    let script = format!(
        r#"{TRY}PORT="$1"; S1="$2"; S2="$3"
try tcp-loopback /usr/bin/nc -z -w 1 127.0.0.1 "$PORT"
try dns-8.8.8.8 /usr/bin/nc -z -w 1 8.8.8.8 53
try http-example /usr/bin/curl -s -m 2 http://example.com
try https-example /usr/bin/curl -s -m 2 https://example.com
try cloud-metadata /usr/bin/curl -s -m 2 http://169.254.169.254/latest/meta-data/
try unix-socket /usr/bin/nc -U "$S1"
try docker-socket /usr/bin/nc -U "$S2"
"#
    );
    let port_text = port.to_string();
    let spec = sh(
        &world,
        &script,
        &[Path::new(&port_text), &plain_socket, &docker_socket],
    );
    let output = runner(&world, &sink).run(&spec).unwrap();
    assert!(output.termination.success(), "{output:?}");
    let lines = lines(&output);
    let leaked: Vec<_> = lines
        .iter()
        .filter(|line| line.starts_with("LEAKED"))
        .collect();
    assert!(
        leaked.is_empty(),
        "the sandbox let these through: {leaked:?}"
    );
    assert_eq!(lines.len(), 7, "{lines:?}");
    // The Host's own sockets never saw a connection.
    assert!(
        tcp.accept().is_err(),
        "the TCP listener received a connection"
    );
    for socket in &unix {
        assert!(
            socket.accept().is_err(),
            "a Unix socket received a connection"
        );
    }
    wait_for_event(&sink, SecurityEventKind::NetworkAccessDenied, "");
}

/// The one exception: the local proxy of the Model Gateway (LOT-014), and nothing else.
#[test]
fn the_local_proxy_port_is_reachable_and_no_other_port_or_host_is() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let other = TcpListener::bind("127.0.0.1:0").unwrap();
    let (proxy_port, other_port) = (
        proxy.local_addr().unwrap().port().to_string(),
        other.local_addr().unwrap().port().to_string(),
    );
    proxy.set_nonblocking(true).unwrap();
    other.set_nonblocking(true).unwrap();

    let script = format!(
        r#"{TRY}
try proxy /usr/bin/nc -z -w 1 127.0.0.1 "$1"
try other-port /usr/bin/nc -z -w 1 127.0.0.1 "$2"
try internet /usr/bin/nc -z -w 1 8.8.8.8 53
"#
    );
    let mut spec = sh(
        &world,
        &script,
        &[Path::new(&proxy_port), Path::new(&other_port)],
    );
    spec.network = NetworkPolicy::LocalProxyOnly(proxy.local_addr().unwrap());
    let output = runner(&world, &sink).run(&spec).unwrap();
    assert!(output.termination.success(), "{output:?}");
    let lines = lines(&output);
    assert_eq!(
        lines,
        vec!["LEAKED:proxy", "DENIED:other-port", "DENIED:internet"],
        "(for this test LEAKED:proxy is the expected, allowed connection)"
    );
    assert!(proxy.accept().is_ok(), "the proxy never got the connection");
    assert!(other.accept().is_err(), "another local port was reachable");
}

/// Scenario 3, macOS part: the CPU budget is enforced by the kernel; memory and process-count
/// budgets cannot be, so they are refused rather than silently ignored.
#[test]
fn the_cpu_budget_is_enforced_and_unenforceable_limits_are_refused() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = runner(&world, &sink);

    let mut spin = sh(&world, "while :; do :; done", &[]);
    spin.limits.max_cpu_time = Some(Duration::from_secs(1));
    spin.limits.wall_timeout = Duration::from_secs(30);
    let started = Instant::now();
    let output = runner.run(&spin).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(
        matches!(
            output.termination,
            Termination::Exited {
                signal: Some(24 | 9),
                ..
            }
        ),
        "{:?}",
        output.termination
    );

    for (field, memory, processes) in [
        ("memory", Some(1u64 << 30), None),
        ("processes", None, Some(64)),
    ] {
        let mut spec = sh(&world, "true", &[]);
        spec.limits.max_memory_bytes = memory;
        spec.limits.max_processes = processes;
        assert!(
            matches!(
                runner.run(&spec),
                Err(SandboxError::LimitNotEnforceable { .. })
            ),
            "{field} limit accepted"
        );
    }
}

/// Scenario 4: a sandbox that does not isolate is detected by the self-test, whatever the cause.
#[test]
fn fail_closed_a_missing_broken_or_non_isolating_sandbox_exec_builds_no_runner() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let engine = || SeatbeltEngine::new(&world.home).unwrap();

    // Missing.
    let missing = engine().with_sandbox_exec_path("/nonexistent/sandbox-exec");
    assert!(matches!(
        OsNativeSandbox::new(missing, sink.clone()),
        Err(SandboxError::SandboxUnavailable { .. })
    ));
    // Present but failing.
    let broken = engine().with_sandbox_exec_path("/usr/bin/false");
    assert!(matches!(
        OsNativeSandbox::new(broken, sink.clone()),
        Err(SandboxError::SandboxUnavailable { .. })
    ));
    // Present, "works", and isolates nothing: it drops the profile and runs the command.
    let fake = world.worktree.parent().unwrap().join("fake-sandbox-exec");
    fs::write(
        &fake,
        "#!/bin/sh\nwhile [ \"$1\" = -D ]; do shift 2; done\nshift 2\nexec \"$@\"\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let passthrough = engine().with_sandbox_exec_path(&fake);
    let error = OsNativeSandbox::new(passthrough, sink.clone())
        .err()
        .expect("a non-isolating sandbox was accepted");
    assert!(
        matches!(error, SandboxError::SandboxUnavailable { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("self-test failed"), "{error}");
    assert_eq!(sink.kinds(), vec![SecurityEventKind::SandboxUnavailable; 3]);
}

/// The sandbox of the current user, built the way the Host builds it.
#[test]
fn the_native_sandbox_of_this_machine_passes_its_self_test_and_runs_programs() {
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = native_sandbox(sink.clone()).expect("macOS must provide a native sandbox");
    assert_eq!(runner.capabilities().name, "macos-seatbelt");
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let spec = SandboxSpec::new(&root, "/usr/bin/true");
    assert!(runner.run(&spec).unwrap().termination.success());
    assert!(sink.events().is_empty(), "{:?}", sink.events());
}

#[test]
fn the_private_tmpdir_is_writable_in_the_sandbox_and_purged_afterwards() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let output = runner(&world, &sink)
        .run(&sh(
            &world,
            r#"echo x > "$TMPDIR/scratch" && /bin/cat "$TMPDIR/scratch" && echo "$TMPDIR""#,
            &[],
        ))
        .unwrap();
    assert!(output.termination.success(), "{output:?}");
    let lines = lines(&output);
    assert_eq!(lines[0], "x");
    assert!(Path::new(&lines[1]).starts_with(world.worktree.join(".coloride-tmp")));
    assert!(!Path::new(&lines[1]).exists());
    assert!(!world.worktree.join(".coloride-tmp").exists());
}

/// A program the profile would not let the process read is refused up front, with the fix.
#[test]
fn a_program_outside_the_readable_paths_is_refused_unless_its_directory_is_shared() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = runner(&world, &sink);
    let tools = world.home.join("tools");
    fs::create_dir_all(&tools).unwrap();
    let tool = tools.join("tool.sh");
    fs::write(&tool, "#!/bin/sh\necho tool-ran\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut spec = SandboxSpec::new(&world.worktree, &tool);
    let error = runner.run(&spec).unwrap_err();
    assert!(
        matches!(&error, SandboxError::InvalidSpec(message) if message.contains("read-only paths")),
        "{error}"
    );
    spec.read_only_paths = vec![tools];
    let output = runner.run(&spec).unwrap();
    assert_eq!(output.stdout, b"tool-ran\n");
}

/// A long-lived process (an agent) through the real sandbox: piped stdio, killed with its handle.
#[test]
fn a_long_lived_process_runs_in_the_sandbox_over_pipes() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let mut child = runner(&world, &sink)
        .spawn(&sh(&world, r#"read line; echo "got $line"; /bin/cat "$1" 2>/dev/null || echo "no-secret"; sleep 600"#, &[&world.home.join(".ssh/id_rsa")]))
        .unwrap();
    writeln!(child.take_stdin().unwrap(), "hello").unwrap();
    let mut stdout = BufReader::new(child.take_stdout().unwrap());
    let (mut first, mut second) = (String::new(), String::new());
    stdout.read_line(&mut first).unwrap();
    stdout.read_line(&mut second).unwrap();
    assert_eq!(
        (first.as_str(), second.as_str()),
        ("got hello\n", "no-secret\n")
    );
    let pid = child.id();
    drop(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline
        && std::process::Command::new("/bin/ps")
            .args(["-p", &pid.to_string()])
            .output()
            .unwrap()
            .status
            .success()
    {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!std::process::Command::new("/bin/ps")
        .args(["-p", &pid.to_string()])
        .output()
        .unwrap()
        .status
        .success());
}

/// A real build toolchain works in the sandbox: the TypeScript compiler (Node) type-checks a
/// project and writes its output in the worktree, reading `node_modules` shared read-only.
#[test]
fn a_real_toolchain_builds_in_the_sandbox_with_shared_read_only_packages() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let tsc = workspace.join("node_modules/typescript/bin/tsc");
    let node = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join("node"))
        .find(|candidate| candidate.is_file());
    let (Some(node), true) = (node, tsc.is_file()) else {
        eprintln!("SKIPPED: node or node_modules/typescript is not installed here");
        return;
    };
    let node = node.canonicalize().unwrap();
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = runner(&world, &sink);
    fs::write(world.worktree.join("tsconfig.json"), r#"{"compilerOptions":{"strict":true,"outDir":"dist","target":"es2020","module":"commonjs"},"files":["index.ts"]}"#).unwrap();
    fs::write(
        world.worktree.join("index.ts"),
        "export const n: number = 1;\n",
    )
    .unwrap();

    let toolchain = vec![
        node.parent().and_then(Path::parent).unwrap().to_path_buf(),
        workspace.join("node_modules").canonicalize().unwrap(),
    ];
    let compile = |source: &str| {
        fs::write(world.worktree.join("index.ts"), source).unwrap();
        let mut spec = SandboxSpec::new(&world.worktree, &node);
        spec.args = vec![
            tsc.canonicalize().unwrap().into_os_string(),
            "-p".into(),
            ".".into(),
        ];
        spec.read_only_paths = toolchain.clone();
        runner.run(&spec).unwrap()
    };
    let good = compile("export const n: number = 1;\n");
    assert!(
        good.termination.success(),
        "{}{}",
        String::from_utf8_lossy(&good.stdout),
        String::from_utf8_lossy(&good.stderr)
    );
    assert!(
        world.worktree.join("dist/index.js").is_file(),
        "the build output was not written"
    );

    let bad = compile("export const n: number = 'not a number';\n");
    assert!(!bad.termination.success());
    assert!(
        String::from_utf8_lossy(&bad.stdout).contains("error TS2322"),
        "{}",
        String::from_utf8_lossy(&bad.stdout)
    );
    // The shared packages were not modified.
    assert!(!workspace.join("node_modules/dist").exists());
}

/// LOT-012 budget: sandbox instantiation p95 < 5 ms. Measured as the cost of running
/// `/usr/bin/true` through the sandbox over running it directly, then through `sandbox-exec`
/// alone (the part no Host code can reduce). Reported always; enforced in optimized builds only.
///
/// KNOWN FAILURE in optimized builds: `sandbox-exec` alone adds about 9 ms on the development
/// machine (a second process and a profile compiled at each start). The budget is kept as an
/// objective, not raised.
#[test]
fn sandbox_instantiation_overhead_is_measured_against_the_5_ms_budget() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = runner(&world, &sink);
    let spec = SandboxSpec::new(&world.worktree, "/usr/bin/true");
    let direct = || {
        let started = Instant::now();
        assert!(std::process::Command::new("/usr/bin/true")
            .status()
            .unwrap()
            .success());
        started.elapsed()
    };
    let sandboxed = || {
        let started = Instant::now();
        assert!(runner.run(&spec).unwrap().termination.success());
        started.elapsed()
    };
    let p95 = |sample: &dyn Fn() -> Duration| {
        let mut times: Vec<Duration> = (0..60).map(|_| sample()).collect();
        times.sort();
        (times[times.len() / 2], times[times.len() * 95 / 100])
    };
    let (direct_median, direct_p95) = p95(&direct);
    let (sandbox_median, sandbox_p95) = p95(&sandboxed);
    let overhead = sandbox_p95.saturating_sub(direct_p95);
    eprintln!(
        "true direct: median {direct_median:?} p95 {direct_p95:?} | in the sandbox: median {sandbox_median:?} p95 {sandbox_p95:?} | overhead p95 {overhead:?}"
    );
    if !cfg!(debug_assertions) {
        assert!(
            overhead < Duration::from_millis(5),
            "overhead p95 {overhead:?} >= 5 ms"
        );
    }
}

#[test]
fn proxy_address_must_be_loopback() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let mut spec = sh(&world, "true", &[]);
    spec.network = NetworkPolicy::LocalProxyOnly(SocketAddr::from(([10, 0, 0, 1], 8080)));
    assert!(matches!(
        runner(&world, &sink).run(&spec),
        Err(SandboxError::InvalidSpec(_))
    ));
}
