// The agent-launch tests need the read-only sandbox of macOS and Linux: Windows refuses that mode
// until its AppContainer backend supports it (see `src/acp/README.md`). There only the fail-closed
// test runs, and the helpers of the others would be dead code.
#![cfg_attr(
    not(any(target_os = "macos", target_os = "linux")),
    allow(dead_code, unused_imports)
)]

mod common;
use host_core::*;
use judge_orchestrator::{acp::*, policy::HostAuthority, TaskStateRepository};
use sandbox_runner::{ResourceLimits, SandboxRunner, TracingAuditSink};
use std::{
    sync::{
        mpsc::{self, Receiver},
        Arc,
    },
    time::Duration,
};

struct Text;
impl WorkspaceText for Text {
    fn text(&self, _path: &str) -> Result<String, String> {
        Ok("unsaved 🦀\r\n".into())
    }
}
struct Fixture {
    _data: tempfile::TempDir,
    root: tempfile::TempDir,
    repository: Arc<TaskStateRepository>,
    gate: WorkspaceTrustGate,
    lease: judge_orchestrator::Lease,
    signed: judge_orchestrator::policy::SignedTaskSpec,
    authority: Arc<HostAuthority>,
}
impl Fixture {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/file.rs"), "disk text").unwrap();
        let binary = root.path().join("fake-agent");
        std::fs::copy(env!("CARGO_BIN_EXE_coloride-acp-test-agent"), &binary).unwrap();
        let mut spec = common::spec();
        spec.policy.allowed_paths = vec![".".into()];
        spec.budget_limits.max_cpu_ms = 1_000_000;
        spec.budget_limits.max_tool_calls = 100;
        let authority = Arc::new(HostAuthority::new().unwrap());
        let signed = authority.sign_task(spec.clone()).unwrap();
        let (repository, lease) = common::running(data.path(), spec);
        let gate = WorkspaceTrustGate::open(data.path().join("trust"), root.path()).unwrap();
        gate.grant().unwrap();
        Self {
            _data: data,
            root,
            repository: Arc::new(repository),
            gate,
            lease,
            signed,
            authority,
        }
    }
    fn host(&self, runner: Arc<dyn SandboxRunner>) -> (AcpHost, Receiver<AcpEvent>) {
        let (tx, rx) = mpsc::channel();
        (
            AcpHost {
                gate: self.gate.clone(),
                worktree_root: self.root.path().canonicalize().unwrap(),
                repository: self.repository.clone(),
                lease: self.lease.clone(),
                signed: self.signed.clone(),
                authority: self.authority.clone(),
                runner,
                text: Arc::new(Text),
                clock: Arc::new(|| 2),
                events: Arc::new(move |event| {
                    let _ = tx.send(event);
                }),
            },
            rx,
        )
    }
    fn launch(&self, mode: &str) -> AcpLaunch {
        AcpLaunch {
            read_only_paths: vec![],
            program: self.root.path().join("fake-agent"),
            args: vec![mode.into()],
            credentials: AgentCredentials::Unauthenticated,
            network: AgentNetwork::Offline,
            limits: ResourceLimits {
                wall_timeout: Duration::from_secs(10),
                max_cpu_time: Some(Duration::from_secs(2)),
                ..ResourceLimits::default()
            },
        }
    }
    fn start(&self, mode: &str) -> (AcpSessionSupervisor, Receiver<AcpEvent>) {
        let runner = sandbox_runner::native_sandbox(Arc::new(TracingAuditSink))
            .expect("native sandbox is a blocking requirement");
        let (host, rx) = self.host(Arc::from(runner));
        let session = AcpSessionSupervisor::start(host, self.launch(mode)).unwrap();
        match rx.recv_timeout(Duration::from_secs(12)).unwrap() {
            AcpEvent::Ready { .. } => {}
            other => panic!("unexpected startup event: {other:?}"),
        }
        session.prompt("test".into()).unwrap();
        (session, rx)
    }
    fn disk(&self) -> String {
        std::fs::read_to_string(self.root.path().join("src/file.rs")).unwrap()
    }
}
fn next(rx: &Receiver<AcpEvent>) -> AcpEvent {
    rx.recv_timeout(Duration::from_secs(5))
        .expect("ACP must not hang")
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn installed_agent_modules_are_shared_read_only_through_the_launch_broker() {
    let f = Fixture::new();
    let installation = tempfile::tempdir().unwrap();
    let binary = installation.path().join("agent");
    std::fs::copy(env!("CARGO_BIN_EXE_coloride-acp-test-agent"), &binary).unwrap();
    let module = installation.path().join("module.txt");
    std::fs::write(&module, "installed code").unwrap();
    let runner = sandbox_runner::native_sandbox(Arc::new(TracingAuditSink)).unwrap();
    let (host, rx) = f.host(Arc::from(runner));
    let mut launch = f.launch("installation");
    launch.program = binary;
    launch.read_only_paths = vec![installation.path().to_path_buf()];
    let mut session = AcpSessionSupervisor::start(host, launch).unwrap();
    assert!(matches!(next(&rx), AcpEvent::Ready { .. }));
    session.prompt("test".into()).unwrap();
    loop {
        if let AcpEvent::Update { update } = next(&rx) {
            assert_eq!(
                update["content"]["text"],
                "module=installed code; write=false"
            );
            break;
        }
    }
    session.stop();
    assert_eq!(std::fs::read_to_string(module).unwrap(), "installed code");
    assert_eq!(
        f.repository
            .snapshot("task")
            .unwrap()
            .resolved_intents
            .len(),
        1
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn agent_write_is_one_proposal_with_live_rope_base_and_never_changes_disk() {
    let f = Fixture::new();
    let (mut session, rx) = f.start("write");
    let mut approved = false;
    let edit = loop {
        match next(&rx) {
            AcpEvent::Permission { id, .. } => {
                assert!(session.proposed_edits().is_empty());
                session.answer_permission(id, true).unwrap();
                approved = true;
            }
            AcpEvent::ProposedEdit { edit } => break edit,
            _ => {}
        }
    };
    assert_eq!(edit.old_text, "unsaved 🦀\r\n");
    assert!(approved, "a write requires Host consent");
    assert_eq!(edit.new_text, "new 🦀\n");
    assert_eq!(
        edit.base_content_hash,
        host_core::compute_rope_blake3_hex(&ropey::Rope::from_str(&edit.old_text))
    );
    session.stop();
    assert_eq!(session.proposed_edits().len(), 1);
    assert_eq!(f.disk(), "disk text");
    assert!(f
        .repository
        .snapshot("task")
        .unwrap()
        .pending_intents
        .is_empty());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn denied_policy_and_host_rejection_execute_nothing() {
    let mut f = Fixture::new();
    f.signed = f
        .authority
        .sign_task({
            let mut spec = f.signed.spec.clone();
            spec.policy.autonomy_level = AutonomyLevel::ProposalOnly;
            spec
        })
        .unwrap();
    // This signed task cannot even authorize process launch, before any fake agent exists.
    let runner = sandbox_runner::native_sandbox(Arc::new(TracingAuditSink)).unwrap();
    let (host, _) = f.host(Arc::from(runner));
    assert!(AcpSessionSupervisor::start(host, f.launch("write")).is_err());
    assert!(f
        .repository
        .snapshot("task")
        .unwrap()
        .resolved_intents
        .is_empty());
    let f = Fixture::new();
    let (mut session, rx) = f.start("write");
    loop {
        match next(&rx) {
            AcpEvent::Permission { id, .. } => session.answer_permission(id, false).unwrap(),
            AcpEvent::Update { update } if update.to_string().contains("Rejected by Host user") => {
                break
            }
            _ => {}
        }
    }
    session.stop();
    assert!(session.proposed_edits().is_empty());
    assert_eq!(f.disk(), "disk text");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn traversal_absolute_and_forbidden_paths_in_any_case_are_denied_to_agent() {
    let f = Fixture::new();
    let (mut session, rx) = f.start("paths");
    let mut refused = 0;
    while refused < 7 {
        if let AcpEvent::Update { update } = next(&rx) {
            if update["content"]["text"]
                .as_str()
                .is_some_and(|s| s.contains("\"error\""))
            {
                refused += 1;
            }
        }
    }
    session.stop();
    assert_eq!(
        f.repository
            .snapshot("task")
            .unwrap()
            .resolved_intents
            .len(),
        1,
        "only launch ran"
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn malformed_oversized_unknown_and_pending_overflow_do_not_hang() {
    for mode in ["malformed", "oversized", "unknown", "flood"] {
        let f = Fixture::new();
        let (mut session, rx) = f.start(mode);
        loop {
            match next(&rx) {
                AcpEvent::Stopped { .. } => break,
                AcpEvent::Update { update }
                    if mode == "unknown" && update.to_string().contains("Unknown ACP method") =>
                {
                    break
                }
                _ => {}
            }
        }
        session.stop();
        assert!(f
            .repository
            .snapshot("task")
            .unwrap()
            .pending_intents
            .is_empty());
        if mode != "unknown" {
            assert_eq!(
                f.repository.snapshot("task").unwrap().state,
                TaskState::Failed
            );
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn terminal_lifecycle_replays_brokered_output_without_executing_again() {
    let f = Fixture::new();
    let (mut session, rx) = f.start("terminal-complete");
    let mut output = false;
    loop {
        match next(&rx) {
            AcpEvent::Permission { id, .. } => session.answer_permission(id, true).unwrap(),
            AcpEvent::Update { update } => {
                let text = update["content"]["text"].as_str().unwrap_or("");
                if text.contains("terminal-output") {
                    output = true;
                }
                if text.contains("Unknown terminal") {
                    break;
                }
            }
            _ => {}
        }
    }
    session.stop();
    assert!(output);
    assert_eq!(
        f.repository
            .snapshot("task")
            .unwrap()
            .resolved_intents
            .len(),
        2
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn real_environment_is_empty_of_configured_secrets_and_direct_writes_are_blocked() {
    std::env::set_var("COLORIDE_TEST_BYOK_SECRET", "never-pass-this");
    for mode in ["environment", "direct-write", "direct-read"] {
        let f = Fixture::new();
        let (mut session, rx) = f.start(mode);
        loop {
            if let AcpEvent::Update { update } = next(&rx) {
                let text = update["content"]["text"].as_str().unwrap();
                if mode == "environment" {
                    assert!(!text.contains("COLORIDE_TEST_BYOK_SECRET"));
                    assert!(!text.contains("HOME"));
                } else {
                    assert_eq!(text, format!("{mode}=false"));
                }
                break;
            }
        }
        session.stop();
        assert_eq!(f.disk(), "disk text");
    }
    std::env::remove_var("COLORIDE_TEST_BYOK_SECRET");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn cancellation_kills_sandbox_tree_and_records_every_intent() {
    let f = Fixture::new();
    let (mut session, rx) = f.start("terminal");
    loop {
        if let AcpEvent::Permission { id, .. } = next(&rx) {
            session.answer_permission(id, true).unwrap();
            break;
        }
    }
    // Wait for a durable terminal intent, rather than racing process registration.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while session.process_ids().len() < 2 {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let started = std::time::Instant::now();
    let pids = session.process_ids();
    session.stop();
    for pid in pids {
        assert!(
            !std::process::Command::new("/bin/ps")
                .args(["-p", &pid.to_string()])
                .output()
                .unwrap()
                .status
                .success(),
            "child {pid} survived cancellation"
        );
    }
    assert!(started.elapsed() < Duration::from_secs(3));
    let snapshot = f.repository.snapshot("task").unwrap();
    assert!(
        snapshot.pending_intents.is_empty(),
        "{:?}",
        snapshot.pending_intents
    );
    assert_eq!(snapshot.state, TaskState::Cancelled);
    assert_eq!(snapshot.resolved_intents.len(), 2);
}

struct Unavailable;
impl SandboxRunner for Unavailable {
    fn capabilities(&self) -> sandbox_runner::EngineCapabilities {
        panic!("not needed")
    }
    fn run(
        &self,
        _: &sandbox_runner::SandboxSpec,
    ) -> Result<sandbox_runner::RunOutput, sandbox_runner::SandboxError> {
        panic!("never run")
    }
    fn spawn(
        &self,
        _: &sandbox_runner::SandboxSpec,
    ) -> Result<sandbox_runner::SandboxedChild, sandbox_runner::SandboxError> {
        Err(sandbox_runner::SandboxError::SandboxUnavailable {
            reason: "test unavailable".into(),
        })
    }
}
#[test]
fn unavailable_sandbox_untrusted_workspace_network_and_credentials_fail_closed() {
    let f = Fixture::new();
    let (host, _) = f.host(Arc::new(Unavailable));
    let error = AcpSessionSupervisor::start(host, f.launch("write"))
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("unavailable"), "{error}");
    let snapshot = f.repository.snapshot("task").unwrap();
    assert!(snapshot.pending_intents.is_empty());
    assert!(snapshot
        .resolved_intents
        .values()
        .all(|outcome| !outcome.succeeded));
    f.gate.revoke().unwrap();
    let (host, _) = f.host(Arc::new(Unavailable));
    assert!(AcpSessionSupervisor::start(host, f.launch("write"))
        .err()
        .unwrap()
        .to_string()
        .contains("trusted"));
    f.gate.grant().unwrap();
    for network in [false, true] {
        let (host, _) = f.host(Arc::new(Unavailable));
        let mut launch = f.launch("write");
        if network {
            launch.network = AgentNetwork::ProviderOnly {
                destination: "https://provider.example".into(),
            };
        } else {
            launch.credentials = AgentCredentials::HostMediated {
                description: "requires login".into(),
            };
        }
        let error = AcpSessionSupervisor::start(host, launch)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains(if network { "egress" } else { "authentication" }),
            "{error}"
        );
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn absolute_paths_inside_the_worktree_work_as_acp_specifies() {
    let f = Fixture::new();
    let (mut session, rx) = f.start("absolute");
    let (mut read, mut location) = (false, false);
    while !(read && location) {
        if let AcpEvent::Update { update } = next(&rx) {
            if update["sessionUpdate"] == "tool_call" {
                // The UI receives the worktree-relative path it can open.
                assert_eq!(update["locations"][0]["path"], "src/file.rs");
                location = true;
            } else if update["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("unsaved"))
            {
                read = true;
            }
        }
    }
    session.stop();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn a_verbose_command_keeps_the_end_of_its_output_and_the_session_alive() {
    let f = Fixture::new();
    let (mut session, rx) = f.start("terminal-verbose");
    let output = loop {
        match next(&rx) {
            AcpEvent::Permission { id, .. } => session.answer_permission(id, true).unwrap(),
            AcpEvent::Stopped { reason } => panic!("the session ended: {reason}"),
            AcpEvent::Update { update } => {
                let text = update["content"]["text"].as_str().unwrap_or("");
                let echoed: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
                if echoed["id"] == 11 {
                    break echoed["result"].clone();
                }
            }
            _ => {}
        }
    };
    session.stop();
    assert_eq!(output["truncated"], true);
    assert_eq!(output["exitStatus"]["exitCode"], 0);
    let text = output["output"].as_str().unwrap();
    assert!(text.len() <= MAX_TERMINAL_OUTPUT_BYTES);
    assert!(text.ends_with("line 3999\n"), "the end is kept");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn cancelling_a_turn_stops_its_command_but_not_the_agent() {
    let f = Fixture::new();
    let (mut session, rx) = f.start("terminal");
    loop {
        if let AcpEvent::Permission { id, .. } = next(&rx) {
            session.answer_permission(id, true).unwrap();
            break;
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while session.process_ids().len() < 2 {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let started = std::time::Instant::now();
    session.cancel().unwrap();
    // The agent, still alive, echoes the answer to its `sleep 60` command: it ended at once.
    loop {
        if let AcpEvent::Update { update } = next(&rx) {
            if update["content"]["text"]
                .as_str()
                .is_some_and(|text| text.contains("terminalId"))
            {
                break;
            }
        }
    }
    assert!(started.elapsed() < Duration::from_secs(3));
    session.stop();
    assert!(f
        .repository
        .snapshot("task")
        .unwrap()
        .pending_intents
        .is_empty());
}
