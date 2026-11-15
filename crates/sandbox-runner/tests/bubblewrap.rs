//! LOT-012, Linux engine: the blocking security scenarios against real bubblewrap namespaces.
//!
//! Needs unprivileged user namespaces. On Ubuntu 24.04 (AppArmor restriction) the self-test fails
//! with a hint; the CI installs an AppArmor profile for `bwrap` before running these tests.
//! Everything uses a FAKE home with fake secrets; the real ones are never touched.
#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sandbox_runner::{
    native_sandbox, BubblewrapEngine, MemoryAuditSink, NetworkPolicy, OsNativeSandbox,
    SandboxError, SandboxRunner, SandboxSpec, SecurityEventKind, Termination,
};

mod common;
use common::world::{lines, sh, sh_in, World, TRY};

fn world() -> World {
    common::world::world(".cache/coloride/worktrees/task")
}

fn runner(world: &World, sink: &Arc<MemoryAuditSink>) -> OsNativeSandbox<BubblewrapEngine> {
    let engine = BubblewrapEngine::new(&world.home).unwrap();
    OsNativeSandbox::new(engine, sink.clone())
        .unwrap_or_else(|error| panic!("the bubblewrap self-test must pass: {error}"))
}

fn alive_by_name(pattern: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", pattern])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn wait_until_gone(pattern: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !alive_by_name(pattern) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Scenario 1, in the two situations that matter: the home is not shared at all (the normal case:
/// the secrets do not exist inside), and the whole home is shared read-only with a worktree that
/// sits next to it (the secrets must stay hidden even though their directory is shared).
#[test]
fn blocking_scenario_1_secrets_path_traversal_and_symlinks_are_refused() {
    for home_is_shared in [false, true] {
        let world = world();
        let sink = Arc::new(MemoryAuditSink::default());
        // A worktree inside the home cannot have the home shared (the request is refused, see the
        // test below), so the shared case uses a worktree beside it.
        let worktree = if home_is_shared {
            &world.beside
        } else {
            &world.worktree
        };
        symlink(world.home.join(".ssh/id_rsa"), worktree.join("escape-ssh")).unwrap();
        symlink("/etc/passwd", worktree.join("escape-passwd")).unwrap();

        let script = format!(
            r#"{TRY}H="$1"; O="$2"
echo "ok" > inside.txt && echo "ALLOWED:inside"
try ssh /bin/cat "$H/.ssh/id_rsa"
try aws /bin/cat "$H/.aws/credentials"
try gnupg /bin/cat "$H/.gnupg/private.key"
try keychain /bin/cat "$H/Library/Keychains/login.keychain-db"
try netrc /bin/cat "$H/.netrc"
try gh-token /bin/cat "$H/.config/gh/hosts.yml"
try parent-write /bin/sh -c 'echo x > ../outside.txt'
try home-write /bin/sh -c "echo x > $H/evil.txt"
try etc-write /bin/sh -c 'echo x > /etc/evil'
try root-write /bin/sh -c 'echo x > /newfile'
try dev-write /bin/sh -c 'echo x > /dev/evil'
try usr-write /bin/sh -c 'echo x > /usr/evil'
try link-to-ssh /bin/cat escape-ssh
try link-to-passwd /bin/cat escape-passwd
try etc-passwd /bin/cat /etc/passwd
try etc-shadow /bin/cat /etc/shadow
try list-home-dir /bin/ls /home
try list-root-dir /bin/ls /root
"#
        );
        let mut spec = sh_in(worktree, &script, &[&world.home, &world.outside]);
        if home_is_shared {
            spec.read_only_paths = vec![world.home.clone()];
        }
        let output = runner(&world, &sink).run(&spec).unwrap();
        assert!(
            output.termination.success(),
            "shared={home_is_shared}: {output:?}"
        );
        let lines = lines(&output);
        assert!(lines.contains(&"ALLOWED:inside".to_string()), "{lines:?}");
        assert_eq!(
            fs::read_to_string(worktree.join("inside.txt")).unwrap(),
            "ok\n"
        );
        let leaked: Vec<_> = lines
            .iter()
            .filter(|line| line.starts_with("LEAKED"))
            .collect();
        assert!(
            leaked.is_empty(),
            "shared={home_is_shared}: leaked {leaked:?}"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with("DENIED"))
                .count(),
            18,
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
        for path in [
            worktree.parent().unwrap().join("outside.txt"),
            world.home.join("evil.txt"),
        ] {
            assert!(!path.exists(), "{path:?} was written");
        }
        assert_eq!(
            fs::read_to_string(world.home.join(".ssh/id_rsa")).unwrap(),
            "SSH-SECRET"
        );
        if !home_is_shared {
            // Not shared: the other project is not there at all.
            let probe = sh_in(
                worktree,
                r#"/bin/cat "$1" 2>&1; echo "exit=$?""#,
                &[&world.outside.join("other/other.txt")],
            );
            let output = runner(&world, &sink).run(&probe).unwrap();
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("No such file"),
                "{output:?}"
            );
        }
    }
}

/// Sharing a directory that contains the worktree (the whole home, for a worktree in the cache
/// directory) is refused: it would make the worktree part of a read-only share.
#[test]
fn sharing_a_directory_that_contains_the_worktree_is_refused() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let mut spec = sh(&world, "true", &[]);
    spec.read_only_paths = vec![world.home.clone()];
    assert!(matches!(
        runner(&world, &sink).run(&spec),
        Err(SandboxError::InvalidSpec(_))
    ));
}

/// Scenario 2: no network, no cloud metadata, no local sockets (path or abstract).
#[test]
fn blocking_scenario_2_network_cloud_metadata_and_sockets_are_refused() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    tcp.set_nonblocking(true).unwrap();
    let port = tcp.local_addr().unwrap().port().to_string();
    let socket_dir = tempfile::Builder::new()
        .prefix("cs")
        .tempdir_in("/tmp")
        .unwrap();
    let path_socket = socket_dir.path().join("docker.sock");
    let path_listener = UnixListener::bind(&path_socket).unwrap();
    path_listener.set_nonblocking(true).unwrap();
    let abstract_name = format!("coloride-test-{}", std::process::id());
    let abstract_listener =
        UnixListener::bind_addr(&SocketAddr::from_abstract_name(&abstract_name).unwrap()).unwrap();
    abstract_listener.set_nonblocking(true).unwrap();

    let script = format!(
        r#"{TRY}PORT="$1"; SOCK="$2"; ABS="$3"
for tool in /usr/bin/bash /usr/bin/curl /usr/bin/python3; do [ -x "$tool" ] && echo "HAVE:$tool" || echo "MISSING:$tool"; done
try tcp-loopback /usr/bin/bash -c 'exec 3<>/dev/tcp/127.0.0.1/$0' "$PORT"
try dns-8.8.8.8 /usr/bin/bash -c 'exec 3<>/dev/tcp/8.8.8.8/53'
try http-example /usr/bin/curl -s -m 2 http://example.com
try https-example /usr/bin/curl -s -m 2 https://example.com
try cloud-metadata /usr/bin/curl -s -m 2 http://169.254.169.254/latest/meta-data/
try path-socket /usr/bin/python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])' "$SOCK"
try abstract-socket /usr/bin/python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect("\0"+sys.argv[1])' "$ABS"
"#
    );
    let spec = sh(
        &world,
        &script,
        &[Path::new(&port), &path_socket, Path::new(&abstract_name)],
    );
    let output = runner(&world, &sink).run(&spec).unwrap();
    assert!(output.termination.success(), "{output:?}");
    let lines = lines(&output);
    let missing: Vec<_> = lines
        .iter()
        .filter(|line| line.starts_with("MISSING"))
        .collect();
    assert!(
        missing.is_empty(),
        "the probe tools must exist so that the test is not vacuous: {missing:?}"
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
        7,
        "{lines:?}"
    );
    assert!(
        tcp.accept().is_err(),
        "the TCP listener received a connection"
    );
    assert!(
        path_listener.accept().is_err(),
        "a Unix socket received a connection"
    );
    assert!(
        abstract_listener.accept().is_err(),
        "an abstract Unix socket received a connection"
    );
}

/// The local proxy cannot be offered through a network namespace: refused, not ignored.
#[test]
fn the_local_proxy_is_refused_because_there_is_no_route_to_the_hosts_loopback() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let mut spec = sh(&world, "true", &[]);
    spec.network = NetworkPolicy::LocalProxyOnly("127.0.0.1:8080".parse().unwrap());
    assert!(matches!(
        runner(&world, &sink).run(&spec),
        Err(SandboxError::NetworkPolicyNotEnforceable { .. })
    ));
}

/// Scenario 3, Linux part: processes that leave the process group (`setsid`) die with the
/// namespace; the CPU budget is enforced; memory and process-count budgets are refused.
#[test]
fn a_process_that_escapes_its_group_still_dies_with_the_namespace() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    // Unique durations tell these processes apart from everything else on the machine.
    let mut spec = sh(
        &world,
        "setsid /usr/bin/sleep 61234 & for i in 1 2 3 4 5 6 7 8 9 10; do setsid /bin/sh -c 'sleep 61235 & wait' & done; /usr/bin/sleep 61236",
        &[],
    );
    spec.limits.wall_timeout = Duration::from_millis(1500);
    let output = runner(&world, &sink).run(&spec).unwrap();
    assert_eq!(output.termination, Termination::WallTimeout);
    for pattern in ["sleep 61234", "sleep 61235", "sleep 61236"] {
        assert!(wait_until_gone(pattern), "{pattern} survived the sandbox");
    }
    assert_eq!(sink.kinds(), vec![SecurityEventKind::WallTimeoutKilled]);
}

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
        // `bwrap` is the init of the namespace: it reports a child killed by a signal as 128 + it.
        matches!(
            output.termination,
            Termination::Exited {
                code: Some(137 | 152),
                ..
            } | Termination::Exited {
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
fn fail_closed_a_missing_broken_or_non_isolating_bwrap_builds_no_runner() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let engine = || BubblewrapEngine::new(&world.home).unwrap();

    let missing = engine().with_bwrap_path("/nonexistent/bwrap");
    let error = OsNativeSandbox::new(missing, sink.clone())
        .err()
        .expect("a missing bwrap was accepted");
    assert!(error.to_string().contains("bubblewrap"), "{error}");
    let broken = engine().with_bwrap_path("/usr/bin/false");
    assert!(matches!(
        OsNativeSandbox::new(broken, sink.clone()),
        Err(SandboxError::SandboxUnavailable { .. })
    ));
    // Present, "works", and isolates nothing: it skips to `--chdir` and runs the command.
    let fake = world.worktree.parent().unwrap().join("fake-bwrap");
    fs::write(
        &fake,
        "#!/bin/sh\nwhile [ $# -gt 0 ] && [ \"$1\" != --chdir ]; do shift; done\nshift 2\nexec \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let error = OsNativeSandbox::new(engine().with_bwrap_path(&fake), sink.clone())
        .err()
        .expect("a non-isolating sandbox was accepted");
    assert!(
        matches!(error, SandboxError::SandboxUnavailable { .. }),
        "{error}"
    );
    assert!(error.to_string().contains("self-test failed"), "{error}");
    assert_eq!(sink.kinds(), vec![SecurityEventKind::SandboxUnavailable; 3]);
}

#[test]
fn the_native_sandbox_of_this_machine_passes_its_self_test_and_runs_programs() {
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = native_sandbox(sink.clone())
        .unwrap_or_else(|error| panic!("Linux must provide a native sandbox here: {error}"));
    assert_eq!(runner.capabilities().name, "linux-bubblewrap");
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
            r#"echo x > "$TMPDIR/scratch" && /bin/cat "$TMPDIR/scratch" && echo "$TMPDIR" && (echo y > /tmp/scratch) 2>/dev/null && echo tmp-ok || echo tmp-readonly"#,
            &[],
        ))
        .unwrap();
    assert!(output.termination.success(), "{output:?}");
    let lines = lines(&output);
    assert_eq!(lines[0], "x");
    assert!(Path::new(&lines[1]).starts_with(world.worktree.join(".coloride-tmp")));
    // The worktree is under /tmp here, so there is no private /tmp: the root stays read-only.
    assert_eq!(lines[2], "tmp-readonly");
    assert!(!Path::new(&lines[1]).exists());
    assert!(!world.worktree.join(".coloride-tmp").exists());
    assert!(
        !Path::new("/tmp/scratch").exists(),
        "the sandbox's /tmp is not the host's"
    );
}

#[test]
fn a_program_outside_the_readable_paths_is_refused_unless_its_directory_is_shared() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = runner(&world, &sink);
    let tools = world.home.join("tools");
    fs::create_dir_all(&tools).unwrap();
    let tool = tools.join("tool.sh");
    fs::write(&tool, "#!/bin/sh\necho tool-ran\n").unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
    let mut spec = SandboxSpec::new(&world.worktree, &tool);
    let error = runner.run(&spec).unwrap_err();
    assert!(
        matches!(&error, SandboxError::InvalidSpec(message) if message.contains("read-only paths")),
        "{error}"
    );
    spec.read_only_paths = vec![tools];
    assert_eq!(runner.run(&spec).unwrap().stdout, b"tool-ran\n");
}

#[test]
fn a_long_lived_process_runs_in_the_sandbox_over_pipes_and_dies_with_its_handle() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let mut child = runner(&world, &sink)
        .spawn(&sh(
            &world,
            r#"read line; echo "got $line"; /bin/cat "$1" 2>/dev/null || echo "no-secret"; sleep 61237"#,
            &[&world.home.join(".ssh/id_rsa")],
        ))
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
    assert!(alive_by_name("sleep 61237"));
    drop(child);
    assert!(
        wait_until_gone("sleep 61237"),
        "the process survived its handle"
    );
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
    fs::write(
        world.worktree.join("tsconfig.json"),
        r#"{"compilerOptions":{"strict":true,"outDir":"dist","target":"es2020","module":"commonjs"},"files":["index.ts"]}"#,
    )
    .unwrap();
    // The toolchain: the directory that holds `node` and its libraries, and the packages.
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
    assert!(String::from_utf8_lossy(&bad.stdout).contains("error TS2322"));
}

/// LOT-012 budget: sandbox instantiation p95 < 5 ms. Reported always; enforced in optimized
/// builds only. See the macOS test for the same budget.
#[test]
fn sandbox_instantiation_overhead_is_measured_against_the_5_ms_budget() {
    let world = world();
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = runner(&world, &sink);
    let spec = SandboxSpec::new(&world.worktree, "/usr/bin/true");
    let direct = || {
        let started = Instant::now();
        assert!(Command::new("/usr/bin/true").status().unwrap().success());
        started.elapsed()
    };
    let sandboxed = || {
        let started = Instant::now();
        assert!(runner.run(&spec).unwrap().termination.success());
        started.elapsed()
    };
    let measure = |sample: &dyn Fn() -> Duration| {
        let mut times: Vec<Duration> = (0..60).map(|_| sample()).collect();
        times.sort();
        (times[times.len() / 2], times[times.len() * 95 / 100])
    };
    let (direct_median, direct_p95) = measure(&direct);
    let (sandbox_median, sandbox_p95) = measure(&sandboxed);
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

/// In the production layout (the worktree in the user's cache directory) a private, writable
/// `/tmp` exists for tools that hard-code it, and it is not the host's.
#[test]
fn a_private_tmp_exists_when_the_worktree_is_in_a_cache_directory() {
    let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_dir())
    else {
        eprintln!("SKIPPED: HOME is not usable here");
        return;
    };
    let cache = home.join(".cache");
    fs::create_dir_all(&cache).unwrap();
    let dir = tempfile::Builder::new()
        .prefix("coloride-test-")
        .tempdir_in(&cache)
        .unwrap();
    let worktree = dir.path().canonicalize().unwrap().join("wt");
    fs::create_dir_all(&worktree).unwrap();
    let engine = BubblewrapEngine::new(&dir.path().canonicalize().unwrap()).unwrap();
    let sink = Arc::new(MemoryAuditSink::default());
    let runner = OsNativeSandbox::new(engine, sink).unwrap_or_else(|error| panic!("{error}"));
    let mut spec = SandboxSpec::new(&worktree, "/bin/sh");
    spec.args = vec![
        "-c".into(),
        "echo y > /tmp/private && /bin/cat /tmp/private && /bin/ls /tmp".into(),
    ];
    let output = runner.run(&spec).unwrap();
    assert!(output.termination.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "y\nprivate\n");
    assert!(!Path::new("/tmp/private").exists());
}
