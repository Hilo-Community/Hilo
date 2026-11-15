#![allow(dead_code)]

use host_core::*;
use judge_orchestrator::{Lease, TaskStateRepository};
use std::{collections::BTreeMap, path::Path, time::Duration};

pub fn spec() -> TaskSpec {
    TaskSpec {
        schema_version: CONTRACT_SCHEMA_VERSION,
        task_id: "task".into(),
        objective: "repair tests".into(),
        workspace_id: "ws".into(),
        base_snapshot_id: "snapshot".into(),
        target_env: TargetEnvironment::LocalWorktree,
        policy: ExecutionPolicy {
            autonomy_level: AutonomyLevel::BoundedAutoCorrect,
            network_allowed: false,
            allowed_paths: vec!["src".into()],
            forbidden_paths: vec!["src/secrets".into()],
            max_auto_retries: 2,
            workspace_trusted: true,
        },
        budget_limits: BudgetLimits {
            max_tokens: 100,
            max_cost_usd_micros: 100,
            max_cpu_ms: 100,
            max_tool_calls: 10,
        },
        acceptance_criteria: vec![],
        state: TaskState::Created,
    }
}

pub fn call(name: &str, capability: ToolCapability, args: &[(&str, ToolArgValue)]) -> ToolCall {
    ToolCall {
        schema_version: CONTRACT_SCHEMA_VERSION,
        call_id: "call".into(),
        task_id: "task".into(),
        tool_name: name.into(),
        validated_args: args
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
        required_capabilities: vec![capability],
        preconditions: vec![],
        idempotency_key: "key".into(),
    }
}

pub fn text(value: &str) -> ToolArgValue {
    ToolArgValue::Text(value.into())
}

pub fn read() -> ToolCall {
    call(
        "read_workspace",
        ToolCapability::ReadWorkspace,
        &[("path", text("src/file.rs"))],
    )
}

pub fn running(path: &Path, spec: TaskSpec) -> (TaskStateRepository, Lease) {
    let repo = TaskStateRepository::open(path).unwrap();
    repo.create_task(spec, 0).unwrap();
    let lease = repo
        .acquire_lease("task", "W1", Duration::from_secs(30), 0)
        .unwrap();
    for state in [
        TaskState::ContextReady,
        TaskState::Planned,
        TaskState::Running,
    ] {
        repo.transition(&lease, state, "advance", 1).unwrap();
    }
    (repo, lease)
}
