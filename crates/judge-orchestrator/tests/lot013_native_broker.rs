mod common;
use common::*;
use host_core::*;
use judge_orchestrator::{broker::*, policy::*};
use sandbox_runner::{native_sandbox, ResourceLimits, TracingAuditSink};
use std::{sync::Arc, time::Duration};

#[test]
fn real_native_sandbox_runs_only_after_policy_budget_and_durable_intent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let mut spec = spec();
    spec.budget_limits.max_cpu_ms = 2_000;
    let (repo, lease) = running(&dir.path().join("journal"), spec.clone());
    let repo = Arc::new(repo);
    let host = Arc::new(HostAuthority::new().unwrap());
    let signed = host.sign_task(spec).unwrap();
    // No skip/fallback: an unavailable native sandbox makes this integration test fail.
    let runner = native_sandbox(Arc::new(TracingAuditSink)).unwrap();
    let limits = ResourceLimits {
        wall_timeout: Duration::from_secs(2),
        max_cpu_time: Some(Duration::from_secs(1)),
        ..Default::default()
    };
    let executor = Arc::new(SandboxToolExecutor::new(
        "task".into(),
        dir.path().to_path_buf(),
        runner,
        limits,
    ));
    let broker = ToolBroker::new(
        repo.clone(),
        PolicyEngine::new(host),
        executor,
        Arc::new(|| 3),
    );
    #[cfg(unix)]
    let (program, args) = ("/bin/echo".to_string(), vec!["LOT-013".into()]);
    #[cfg(windows)]
    let (program, args) = (
        format!(
            "{}\\System32\\cmd.exe",
            std::env::var("SystemRoot").unwrap()
        ),
        vec!["/d".into(), "/c".into(), "echo LOT-013".into()],
    );
    let c = call(
        "run_sandbox_command",
        ToolCapability::RunSandboxCommand,
        &[
            ("program", text(&program)),
            ("cwd", text("src")),
            ("args", ToolArgValue::StringList(args)),
        ],
    );
    let result = broker.execute(&lease, &signed, &c, &[], 2).unwrap();
    assert!(result.succeeded, "{}", result.summary);
    assert!(result.summary.contains("LOT-013"));
    assert_eq!(broker.execute(&lease, &signed, &c, &[], 3).unwrap(), result);
    let snapshot = repo.snapshot("task").unwrap();
    assert_eq!(snapshot.budget_spent.tool_calls, 1);
    assert_eq!(snapshot.budget_spent.cpu_ms, 1_000);
    assert!(snapshot.pending_intents.is_empty());
}
