//! LOT-013: how the broker closes actions: replays never spend, large outputs are recorded
//! bounded, a refusal before anything started needs no human, and forbidden paths hold
//! whatever their case.

mod common;
use common::*;
use host_core::*;
use judge_orchestrator::{
    broker::*, budget::*, policy::*, IntentOutcome, Lease, TaskStateRepository,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Executor {
    runs: AtomicUsize,
    summary_len: usize,
    refuse_before_start: bool,
}

impl ToolExecutor for Executor {
    fn estimate(&self, _: &TaskSpec, _: &ToolCall) -> Result<BudgetUsage, ExecutionError> {
        Ok(BudgetUsage {
            tool_calls: 1,
            ..Default::default()
        })
    }

    fn execute(
        &self,
        _: &TaskSpec,
        _: &ToolCall,
        _: BudgetUsage,
    ) -> Result<ExecutionReport, ExecutionError> {
        if self.refuse_before_start {
            return Err(ExecutionError::NotStarted("limit not enforceable".into()));
        }
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ExecutionReport {
            outcome: IntentOutcome {
                succeeded: true,
                summary: "x".repeat(self.summary_len),
            },
            actual: BudgetUsage {
                tool_calls: 1,
                ..Default::default()
            },
            failure_signature: None,
        })
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    repo: Arc<TaskStateRepository>,
    lease: Lease,
    signed: SignedTaskSpec,
    executor: Arc<Executor>,
    broker: ToolBroker,
}

fn fixture(spec: TaskSpec, summary_len: usize, refuse_before_start: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path(), spec.clone());
    let repo = Arc::new(repo);
    let host = Arc::new(HostAuthority::new().unwrap());
    let signed = host.sign_task(spec).unwrap();
    let executor = Arc::new(Executor {
        runs: AtomicUsize::new(0),
        summary_len,
        refuse_before_start,
    });
    let broker = ToolBroker::new(
        repo.clone(),
        PolicyEngine::new(host),
        executor.clone(),
        Arc::new(|| 2),
    );
    Fixture {
        _dir: dir,
        repo,
        lease,
        signed,
        executor,
        broker,
    }
}

impl Fixture {
    fn state(&self) -> TaskState {
        self.repo.snapshot("task").unwrap().state
    }
}

#[test]
fn replaying_a_settled_call_returns_its_outcome_even_with_no_budget_left() {
    let mut spec = spec();
    spec.budget_limits.max_tool_calls = 1;
    let fx = fixture(spec, 1, false);
    let first = fx
        .broker
        .execute(&fx.lease, &fx.signed, &read(), &[], 2)
        .unwrap();
    // The single tool call of the budget is spent; the same key again is a replay.
    let replay = fx
        .broker
        .execute(&fx.lease, &fx.signed, &read(), &[], 3)
        .unwrap();
    assert_eq!(replay, first);
    assert_eq!(fx.executor.runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        fx.state(),
        TaskState::Running,
        "a replay must not pause the task"
    );
}

#[test]
fn a_huge_output_is_recorded_bounded_and_the_action_is_closed() {
    let fx = fixture(spec(), 17 * 1024 * 1024, false);
    let outcome = fx
        .broker
        .execute(&fx.lease, &fx.signed, &read(), &[], 2)
        .unwrap();
    assert!(outcome.summary.len() <= 64 * 1024);
    assert!(outcome.summary.contains("bytes omitted"));
    let snapshot = fx.repo.snapshot("task").unwrap();
    assert!(snapshot.pending_intents.is_empty());
    assert_eq!(snapshot.resolved_intents["key"], outcome);
}

#[test]
fn a_refusal_before_anything_started_is_closed_without_a_human_or_a_charge() {
    let fx = fixture(spec(), 1, true);
    assert!(matches!(
        fx.broker.execute(&fx.lease, &fx.signed, &read(), &[], 2),
        Err(BrokerError::Execution(ExecutionError::NotStarted(_)))
    ));
    let snapshot = fx.repo.snapshot("task").unwrap();
    assert!(snapshot.pending_intents.is_empty());
    assert!(!snapshot.resolved_intents["key"].succeeded);
    assert_eq!(snapshot.budget_spent, BudgetUsage::default());
    // A worker taking over once W1's lease ends finds nothing to reconcile.
    let next = fx
        .repo
        .acquire_lease(
            "task",
            "W2",
            std::time::Duration::from_secs(30),
            fx.lease.expires_at(),
        )
        .unwrap();
    assert_eq!(next.generation(), 2);
    assert_eq!(fx.state(), TaskState::Running);
}

#[test]
fn a_forbidden_folder_cannot_be_reached_by_changing_its_case() {
    let fx = fixture(spec(), 1, false); // forbidden: src/secrets
    for path in ["src/secrets/key", "src/SECRETS/key", "src/Secrets/key"] {
        let call = call(
            "read_workspace",
            ToolCapability::ReadWorkspace,
            &[("path", text(path))],
        );
        assert!(
            matches!(
                fx.broker.execute(&fx.lease, &fx.signed, &call, &[], 2),
                Err(BrokerError::Policy(Decision::Deny(_)))
            ),
            "{path}"
        );
    }
    assert_eq!(fx.executor.runs.load(Ordering::SeqCst), 0);
}
