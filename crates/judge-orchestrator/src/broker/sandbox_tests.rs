//! Pure request construction proves the network gate independently of the OS.

use super::*;
use host_core::*;
use sandbox_runner::*;
use std::{collections::BTreeMap, time::Duration};

struct NeverRun;
impl SandboxRunner for NeverRun {
    fn capabilities(&self) -> EngineCapabilities {
        panic!("not used in request validation")
    }
    fn run(&self, _: &SandboxSpec) -> Result<RunOutput, SandboxError> {
        panic!("must not execute")
    }
    fn spawn(&self, _: &SandboxSpec) -> Result<SandboxedChild, SandboxError> {
        panic!("must not spawn")
    }
}

fn spec() -> TaskSpec {
    TaskSpec {
        schema_version: CONTRACT_SCHEMA_VERSION,
        task_id: "task".into(),
        objective: String::new(),
        workspace_id: "ws".into(),
        base_snapshot_id: "snap".into(),
        target_env: TargetEnvironment::LocalWorktree,
        policy: ExecutionPolicy {
            autonomy_level: AutonomyLevel::SupervisedSandbox,
            network_allowed: false,
            allowed_paths: vec![".".into()],
            forbidden_paths: vec![],
            max_auto_retries: 2,
            workspace_trusted: true,
        },
        budget_limits: BudgetLimits {
            max_tokens: 1,
            max_cpu_ms: 1000,
            max_cost_usd_micros: 1,
            max_tool_calls: 1,
        },
        acceptance_criteria: vec![],
        state: TaskState::Created,
    }
}

fn call() -> ToolCall {
    ToolCall {
        schema_version: CONTRACT_SCHEMA_VERSION,
        task_id: "task".into(),
        call_id: "c".into(),
        tool_name: "run_sandbox_command".into(),
        idempotency_key: "key".into(),
        preconditions: vec![],
        required_capabilities: vec![ToolCapability::RunSandboxCommand],
        validated_args: BTreeMap::from([
            (
                "program".into(),
                ToolArgValue::Text(
                    if cfg!(windows) {
                        "C:/Windows/System32/cmd.exe"
                    } else {
                        "/bin/echo"
                    }
                    .into(),
                ),
            ),
            ("cwd".into(), ToolArgValue::Text(".".into())),
            ("args".into(), ToolArgValue::StringList(vec![])),
        ]),
    }
}

#[test]
fn network_needs_both_policy_and_capability_and_a_host_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let proxy = "127.0.0.1:8123".parse().unwrap();
    let executor = SandboxToolExecutor::new(
        "task".into(),
        dir.path().to_path_buf(),
        Box::new(NeverRun),
        ResourceLimits::default(),
    )
    .with_proxy(proxy);
    for allowed in [false, true] {
        for capability in [false, true] {
            let mut spec = spec();
            spec.policy.network_allowed = allowed;
            let mut call = call();
            if capability {
                call.required_capabilities
                    .push(ToolCapability::NetworkEgress);
            }
            assert_eq!(
                executor.request(&spec, &call).unwrap().network,
                if allowed && capability {
                    NetworkPolicy::LocalProxyOnly(proxy)
                } else {
                    NetworkPolicy::Denied
                }
            );
        }
    }
}

#[test]
fn cwd_program_and_cpu_estimates_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let executor = SandboxToolExecutor::new(
        "task".into(),
        dir.path().to_path_buf(),
        Box::new(NeverRun),
        ResourceLimits {
            max_cpu_time: Some(Duration::from_millis(1)),
            ..Default::default()
        },
    );
    assert_eq!(executor.estimate(&spec(), &call()).unwrap().cpu_ms, 1_000);
    for cwd in ["../outside", "/tmp", "C:/outside"] {
        let mut c = call();
        c.validated_args
            .insert("cwd".into(), ToolArgValue::Text(cwd.into()));
        assert!(executor.request(&spec(), &c).is_err());
    }
    let mut relative = call();
    relative
        .validated_args
        .insert("program".into(), ToolArgValue::Text("echo".into()));
    assert!(executor.request(&spec(), &relative).is_err());
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        let mut escaped = call();
        escaped
            .validated_args
            .insert("cwd".into(), ToolArgValue::Text("escape".into()));
        assert!(executor.request(&spec(), &escaped).is_err());
    }
}
