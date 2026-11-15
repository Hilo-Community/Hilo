//! LOT-012, Windows engine: the blocking security scenarios against a real AppContainer and Job
//! Object. Only the Windows CI runs these (there is no Windows machine at hand when writing them).
//!
//! The worktree is `<base>\work`; `<base>\outside` is a sibling holding a fake secret. Paths used
//! inside `cmd.exe` are relative, so that no quoting of `cmd`'s own syntax is needed.
#![cfg(windows)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sandbox_runner::{
    native_sandbox, AppContainerEngine, MemoryAuditSink, NetworkPolicy, OsNativeSandbox,
    SandboxError, SandboxRunner, SandboxSpec, SecurityEventKind, Termination,
};

fn system32() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot")).join("System32")
}

struct Layout {
    _base: tempfile::TempDir,
    work: PathBuf,
    outside: PathBuf,
}

fn layout() -> Layout {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().canonicalize().unwrap();
    let (work, outside) = (root.join("work"), root.join("outside"));
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "TOP-SECRET").unwrap();
    fs::write(work.join("input.txt"), "readable").unwrap();
    Layout {
        _base: base,
        work,
        outside,
    }
}

fn spec(work: &Path, program: &str, args: &[&str]) -> SandboxSpec {
    let mut spec = SandboxSpec::new(work, system32().join(program));
    spec.args = args.iter().map(Into::into).collect();
    spec.env
        .insert("SystemRoot".into(), std::env::var_os("SystemRoot").unwrap());
    spec.limits.wall_timeout = Duration::from_secs(60);
    spec
}

/// `cmd /s /c <script>`: with `/s`, `cmd` always removes the quotes around the script that the
/// command-line quoting adds when it contains spaces, and runs the rest as written.
fn cmd(work: &Path, script: &str) -> SandboxSpec {
    spec(work, "cmd.exe", &["/d", "/s", "/c", script])
}

/// Runs until killed, with no network (`ping` cannot even reach loopback in an AppContainer).
const FOREVER: &str = "for /l %i in (0,0,1) do @rem";

fn runner() -> (Box<dyn SandboxRunner>, Arc<MemoryAuditSink>) {
    let audit = Arc::new(MemoryAuditSink::default());
    let runner = native_sandbox(audit.clone())
        .unwrap_or_else(|error| panic!("the AppContainer self-test must pass: {error}"));
    (runner, audit)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_program_runs_and_its_output_is_captured() {
    let l = layout();
    let (runner, _) = runner();
    let out = runner.run(&cmd(&l.work, "echo hello")).unwrap();
    assert!(out.termination.success(), "{out:?}");
    assert!(text(&out.stdout).contains("hello"));
}

#[test]
fn the_worktree_is_readable_and_writable() {
    let l = layout();
    let (runner, _) = runner();
    let out = runner
        .run(&cmd(&l.work, "type input.txt && echo made>made.txt"))
        .unwrap();
    assert!(out.termination.success(), "{out:?}");
    assert!(text(&out.stdout).contains("readable"));
    assert_eq!(
        fs::read_to_string(l.work.join("made.txt")).unwrap().trim(),
        "made"
    );
}

#[test]
fn a_file_outside_the_worktree_cannot_be_read() {
    let l = layout();
    let (runner, _) = runner();
    let out = runner
        .run(&cmd(&l.work, r"type ..\outside\secret.txt"))
        .unwrap();
    assert!(!out.termination.success(), "{out:?}");
    assert!(!text(&out.stdout).contains("TOP-SECRET"));
}

#[test]
fn nothing_can_be_written_outside_the_worktree() {
    let l = layout();
    let (runner, _) = runner();
    let _ = runner
        .run(&cmd(&l.work, r"echo x>..\outside\escape.txt"))
        .unwrap();
    assert!(!l.outside.join("escape.txt").exists());
}

#[test]
fn the_users_profile_is_out_of_reach() {
    let l = layout();
    let (runner, _) = runner();
    let profile = std::env::var("USERPROFILE").unwrap();
    // The listing of the profile directory must be refused.
    let out = runner
        .run(&spec(&l.work, "cmd.exe", &["/d", "/c", "dir", &profile]))
        .unwrap();
    assert!(!out.termination.success(), "{out:?}");
}

#[test]
fn the_network_is_unreachable_even_on_loopback() {
    let l = layout();
    let (runner, _) = runner();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!(
        "http://127.0.0.1:{}/",
        listener.local_addr().unwrap().port()
    );
    let out = runner
        .run(&spec(
            &l.work,
            "curl.exe",
            &["--silent", "--max-time", "5", &url],
        ))
        .unwrap();
    assert!(!out.termination.success(), "{out:?}");
    assert!(listener.accept().is_err(), "the sandbox reached the host");
}

#[test]
fn a_local_proxy_is_refused_rather_than_approximated() {
    let l = layout();
    let (runner, audit) = runner();
    let mut request = cmd(&l.work, "echo hi");
    request.network = NetworkPolicy::LocalProxyOnly("127.0.0.1:9".parse().unwrap());
    assert!(matches!(
        runner.run(&request),
        Err(SandboxError::NetworkPolicyNotEnforceable { .. })
    ));
    assert!(audit
        .events()
        .iter()
        .any(|event| event.kind == SecurityEventKind::PolicyNotEnforceable));
}

#[test]
fn shared_read_only_directories_are_refused_rather_than_exposed() {
    let l = layout();
    let (runner, _) = runner();
    let mut request = cmd(&l.work, "echo hi");
    request.read_only_paths = vec![l.outside.clone()];
    assert!(matches!(
        runner.run(&request),
        Err(SandboxError::InvalidSpec(_))
    ));
}

#[test]
fn the_wall_clock_budget_kills_the_whole_tree() {
    let l = layout();
    let (runner, audit) = runner();
    let mut request = cmd(&l.work, FOREVER);
    request.limits.wall_timeout = Duration::from_secs(2);
    let started = Instant::now();
    let out = runner.run(&request).unwrap();
    assert_eq!(out.termination, Termination::WallTimeout);
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(audit
        .events()
        .iter()
        .any(|event| event.kind == SecurityEventKind::WallTimeoutKilled));
}

#[test]
fn the_output_budget_stops_a_flood() {
    let l = layout();
    let (runner, _) = runner();
    let mut request = spec(
        &l.work,
        "cmd.exe",
        &[
            "/d",
            "/s",
            "/c",
            "for /l %i in (1,1,500000) do @echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        ],
    );
    request.limits.max_output_bytes = 10_000;
    let out = runner.run(&request).unwrap();
    assert_eq!(out.termination, Termination::OutputLimit, "{out:?}");
    assert!(out.stdout.len() <= 10_000);
}

#[test]
fn the_process_count_limit_is_enforced() {
    let l = layout();
    let (runner, _) = runner();
    // `findstr` is a second process started by `cmd`.
    let script = "findstr readable input.txt";
    let free = runner.run(&cmd(&l.work, script)).unwrap();
    assert!(text(&free.stdout).contains("readable"), "{free:?}");
    let mut limited = cmd(&l.work, script);
    limited.limits.max_processes = Some(1);
    let out = runner.run(&limited).unwrap();
    assert!(
        !text(&out.stdout).contains("readable"),
        "the second process ran despite the limit of one: {out:?}"
    );
}

#[test]
fn the_private_temporary_directory_is_purged() {
    let l = layout();
    let (runner, _) = runner();
    let out = runner
        .run(&cmd(&l.work, "echo scratch>%TEMP%\\scratch.txt"))
        .unwrap();
    assert!(out.termination.success(), "{out:?}");
    assert!(!l.work.join(".coloride-tmp").exists());
}

/// The security descriptor of `path` in SDDL, where every SID that has no alias is written out
/// (`S-1-15-2-...` for an AppContainer). `icacls /save` writes it as UTF-16 to a file.
fn acl_sddl(path: &Path) -> String {
    let dir = tempfile::tempdir().unwrap();
    let saved = dir.path().join("acl.txt");
    let out = Command::new("icacls")
        .arg(path)
        .arg("/save")
        .arg(&saved)
        .output()
        .unwrap();
    assert!(out.status.success(), "icacls failed: {}", text(&out.stdout));
    let bytes = fs::read(&saved).unwrap();
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

/// The prefix of every AppContainer SID.
const APP_CONTAINER_SID: &str = "S-1-15-2-";

#[test]
fn the_access_granted_for_the_run_is_taken_back() {
    let l = layout();
    let (runner, _) = runner();
    let out = runner.run(&cmd(&l.work, "echo hi")).unwrap();
    assert!(out.termination.success(), "{out:?}");
    let sids = acl_sddl(&l.work);
    assert!(
        !sids.contains(APP_CONTAINER_SID),
        "an AppContainer entry was left on the worktree:\n{sids}"
    );
}

#[test]
fn a_worktree_holding_a_hard_link_is_refused_before_any_permission_changes() {
    let l = layout();
    let (runner, _) = runner();
    let secret = l.outside.join("secret.txt");
    fs::hard_link(&secret, l.work.join("innocent.txt")).unwrap();
    assert!(runner.run(&cmd(&l.work, "echo hi")).is_err());
    assert!(!acl_sddl(&secret).contains(APP_CONTAINER_SID));
}

#[test]
fn a_worktree_holding_a_junction_is_refused_before_any_permission_changes() {
    let l = layout();
    let (runner, _) = runner();
    let made = Command::new("cmd")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(l.work.join("junction"))
        .arg(&l.outside)
        .output()
        .unwrap();
    assert!(made.status.success(), "{}", text(&made.stderr));
    assert!(runner.run(&cmd(&l.work, "echo hi")).is_err());
    assert!(!acl_sddl(&l.outside).contains(APP_CONTAINER_SID));
    assert!(!acl_sddl(&l.outside.join("secret.txt")).contains(APP_CONTAINER_SID));
}

const HELPER_ENV: &str = "COLORIDE_WINDOWS_HOST_HELPER";
const HELPER_TEST: &str = "a_run_whose_host_died_is_undone_at_the_next_start";

fn journal_entries(journal: &Path) -> usize {
    fs::read_dir(journal).map_or(0, |entries| entries.count())
}

/// The Host of the abrupt-exit scenario is this test binary run again in a helper mode: it starts
/// a long run, reports it, and is then killed without running any destructor.
fn host_helper(dirs: &str) {
    let (work, journal) = dirs.split_once('|').unwrap();
    let engine = AppContainerEngine::with_journal_dir(journal);
    let runner = OsNativeSandbox::new(engine, Arc::new(MemoryAuditSink::default())).unwrap();
    let child = runner.spawn(&cmd(Path::new(work), FOREVER)).unwrap();
    println!("SPAWNED {}", child.id());
    std::io::stdout().flush().unwrap();
    std::thread::sleep(Duration::from_secs(120));
}

#[test]
fn a_run_whose_host_died_is_undone_at_the_next_start() {
    if let Ok(dirs) = std::env::var(HELPER_ENV) {
        host_helper(&dirs);
        return;
    }
    let l = layout();
    let journal = l.outside.parent().unwrap().join("journal");
    let mut host = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", HELPER_TEST, "--nocapture", "--test-threads=1"])
        .env(
            HELPER_ENV,
            format!("{}|{}", l.work.display(), journal.display()),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let (lines_tx, lines_rx) = std::sync::mpsc::channel();
    let stdout = host.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(60);
    let pid = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(line) = lines_rx.recv_timeout(left) else {
            let _ = host.kill();
            panic!("the helper Host did not start its run");
        };
        if let Some(at) = line.find("SPAWNED ") {
            break line[at + 8..].trim().parse::<u32>().unwrap();
        }
    };
    // TerminateProcess: no destructor runs in the helper.
    host.kill().unwrap();
    host.wait().unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while !gone(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(gone(pid), "the sandboxed process outlived its Host");
    assert_eq!(journal_entries(&journal), 1, "the run should be on record");
    assert!(acl_sddl(&l.work).contains(APP_CONTAINER_SID));

    // The next start cleans up before accepting anything.
    let engine = AppContainerEngine::with_journal_dir(&journal);
    OsNativeSandbox::new(engine, Arc::new(MemoryAuditSink::default())).unwrap();
    assert_eq!(journal_entries(&journal), 0);
    let sids = acl_sddl(&l.work);
    assert!(
        !sids.contains(APP_CONTAINER_SID),
        "the dead run's access is still on the worktree:\n{sids}"
    );
}

#[test]
fn a_long_lived_process_talks_through_its_pipes() {
    let l = layout();
    let (runner, _) = runner();
    let mut child = runner.spawn(&spec(&l.work, "findstr.exe", &["."])).unwrap();
    writeln!(child.take_stdin().unwrap(), "hello").unwrap();
    let mut stdout = BufReader::new(child.take_stdout().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "hello");
}

fn gone(pid: u32) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .unwrap();
    !text(&out.stdout).contains(&pid.to_string())
}

#[test]
fn dropping_the_child_kills_its_whole_tree() {
    let l = layout();
    let (runner, _) = runner();
    let child = runner.spawn(&cmd(&l.work, FOREVER)).unwrap();
    let pid = child.id();
    assert!(!gone(pid));
    drop(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !gone(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(gone(pid), "the process survived its handle");
}

#[test]
fn a_program_that_cannot_run_at_all_is_an_error_not_a_fallback() {
    let l = layout();
    let (runner, _) = runner();
    let request = SandboxSpec::new(&l.work, system32().join("does-not-exist.exe"));
    assert!(runner.run(&request).is_err());
}

#[test]
fn batch_files_are_refused() {
    let l = layout();
    let (runner, _) = runner();
    fs::write(l.work.join("run.cmd"), "@echo hi").unwrap();
    let request = SandboxSpec::new(&l.work, l.work.join("run.cmd"));
    assert!(runner.run(&request).is_err());
}

#[test]
fn the_engine_passes_its_self_test_on_this_machine() {
    let engine = AppContainerEngine::for_current_user().unwrap();
    if let Err(error) = OsNativeSandbox::new(engine, Arc::new(MemoryAuditSink::default())) {
        panic!("{error}");
    }
}
