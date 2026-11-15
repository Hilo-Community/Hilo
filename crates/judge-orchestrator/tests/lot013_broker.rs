mod common;
use common::*;
use host_core::*;
use judge_orchestrator::{
    broker::*, budget::*, policy::*, IntentOutcome, RepositoryError, TaskStateRepository,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

struct Fake {
    calls: AtomicUsize,
    repo: Arc<TaskStateRepository>,
    panic: bool,
    failed: bool,
    tokens: u64,
}

impl ToolExecutor for Fake {
    fn estimate(&self, _: &TaskSpec, _: &ToolCall) -> Result<BudgetUsage, ExecutionError> {
        Ok(BudgetUsage {
            tokens: 10,
            tool_calls: 1,
            ..Default::default()
        })
    }
    fn execute(
        &self,
        _: &TaskSpec,
        call: &ToolCall,
        _: BudgetUsage,
    ) -> Result<ExecutionReport, ExecutionError> {
        assert!(self
            .repo
            .snapshot("task")
            .unwrap()
            .pending_intents
            .contains_key(&call.idempotency_key));
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "crashed after external action");
        Ok(ExecutionReport {
            outcome: IntentOutcome {
                succeeded: !self.failed,
                summary: "external result".into(),
            },
            actual: BudgetUsage {
                tokens: self.tokens,
                tool_calls: 1,
                ..Default::default()
            },
            failure_signature: self.failed.then(|| "compiler:E100".into()),
        })
    }
}

fn fake(repo: Arc<TaskStateRepository>, panic: bool, failed: bool, tokens: u64) -> Arc<Fake> {
    Arc::new(Fake {
        calls: AtomicUsize::new(0),
        repo,
        panic,
        failed,
        tokens,
    })
}

fn broker(
    repo: Arc<TaskStateRepository>,
    host: Arc<HostAuthority>,
    executor: Arc<Fake>,
    now: u64,
) -> ToolBroker {
    ToolBroker::new(
        repo,
        PolicyEngine::new(host),
        executor,
        Arc::new(move || now),
    )
}

#[test]
fn denied_calls_have_zero_execution_budget_or_journal_side_effects() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path(), spec());
    let repo = Arc::new(repo);
    let host = Arc::new(HostAuthority::new().unwrap());
    let executor = fake(repo.clone(), false, false, 5);
    let broker = broker(repo.clone(), host.clone(), executor.clone(), 2);
    let mut signed = host.sign_task(spec()).unwrap();
    signed.mac = [0; 32];
    let before = std::fs::read(dir.path().join("task.journal")).unwrap();
    assert!(matches!(
        broker.execute(&lease, &signed, &read(), &[], 2),
        Err(BrokerError::Policy(_))
    ));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read(dir.path().join("task.journal")).unwrap(),
        before
    );
    let mut changed_spec = spec();
    changed_spec.budget_limits.max_tokens += 100;
    assert!(matches!(
        broker.execute(
            &lease,
            &host.sign_task(changed_spec).unwrap(),
            &read(),
            &[],
            2
        ),
        Err(BrokerError::Policy(_))
    ));
}

#[test]
fn resolved_call_is_replayed_without_execution_or_spending_and_key_binds_digest() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path(), spec());
    let repo = Arc::new(repo);
    let host = Arc::new(HostAuthority::new().unwrap());
    let signed = host.sign_task(spec()).unwrap();
    let executor = fake(repo.clone(), false, false, 5);
    let broker = broker(repo.clone(), host, executor.clone(), 2);
    let first = broker.execute(&lease, &signed, &read(), &[], 2).unwrap();
    assert_eq!(
        broker.execute(&lease, &signed, &read(), &[], 3).unwrap(),
        first
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(repo.snapshot("task").unwrap().budget_spent.tokens, 5);
    let mut changed = read();
    changed
        .validated_args
        .insert("path".into(), text("src/other"));
    assert!(matches!(
        broker.execute(&lease, &signed, &changed, &[], 3),
        Err(BrokerError::IdempotencyConflict)
    ));
    drop(broker);
    drop(executor);
    drop(repo);
    let repo = Arc::new(TaskStateRepository::open(dir.path()).unwrap());
    let lease = repo
        .acquire_lease("task", "W2", Duration::from_secs(30), 4)
        .unwrap();
    let executor = fake(repo.clone(), false, false, 5);
    let host = Arc::new(HostAuthority::new().unwrap());
    let signed = host.sign_task(spec()).unwrap();
    let resumed = ToolBroker::new(
        repo.clone(),
        PolicyEngine::new(host),
        executor.clone(),
        Arc::new(|| 5),
    );
    assert_eq!(
        resumed.execute(&lease, &signed, &read(), &[], 5).unwrap(),
        first
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert_eq!(repo.snapshot("task").unwrap().budget_spent.tokens, 5);
}

#[test]
fn crash_after_external_action_and_expired_worker_rejection_has_zero_blind_retries() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, w1) = running(dir.path(), spec());
    let repo = Arc::new(repo);
    let host = Arc::new(HostAuthority::new().unwrap());
    let signed = host.sign_task(spec()).unwrap();
    let executor = fake(repo.clone(), true, false, 5);
    let broker = broker(repo.clone(), host.clone(), executor.clone(), 2);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| broker.execute(
            &w1,
            &signed,
            &read(),
            &[],
            2
        )))
        .is_err()
    );
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        broker.execute(&w1, &signed, &read(), &[], 3),
        Err(BrokerError::IdempotencyConflict)
    ));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    drop(broker);
    drop(executor);
    drop(repo);

    let repo = Arc::new(TaskStateRepository::open(dir.path()).unwrap());
    let w2 = repo
        .acquire_lease("task", "W2", Duration::from_secs(30), 4)
        .unwrap();
    assert_eq!(w2.generation(), 2);
    let executor = fake(repo.clone(), false, false, 5);
    let recovered = ToolBroker::new(
        repo.clone(),
        PolicyEngine::new(host),
        executor.clone(),
        Arc::new(|| 5),
    );
    assert_eq!(
        recovered.state("task"),
        Some(TaskState::NeedsReconciliation)
    );
    assert!(recovered.execute(&w2, &signed, &read(), &[], 5).is_err());
    assert!(recovered.execute(&w1, &signed, &read(), &[], 5).is_err());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        repo.resolve_intent(
            &w1,
            "key",
            IntentOutcome {
                succeeded: true,
                summary: "late".into()
            },
            5
        ),
        Err(RepositoryError::StaleLease { .. })
    ));
}

#[test]
fn broker_budget_overrun_repeated_failure_and_retry_limit_stop_execution() {
    for tokens in [5, 11] {
        let dir = tempfile::tempdir().unwrap();
        let (repo, lease) = running(dir.path(), spec());
        let repo = Arc::new(repo);
        let host = Arc::new(HostAuthority::new().unwrap());
        let signed = host.sign_task(spec()).unwrap();
        let executor = fake(repo.clone(), false, tokens == 5, tokens);
        let broker = broker(repo.clone(), host, executor.clone(), 2);
        let first = broker.execute(&lease, &signed, &read(), &[], 2);
        if tokens == 11 {
            assert!(matches!(
                first,
                Err(BrokerError::Budget(BudgetError::ActualExceedsReservation))
            ));
            assert_eq!(repo.snapshot("task").unwrap().state, TaskState::Paused);
            assert_eq!(repo.snapshot("task").unwrap().budget_spent.tokens, 11);
        } else {
            assert!(!first.unwrap().succeeded);
            let mut second = read();
            second.idempotency_key = "second".into();
            assert!(matches!(
                broker.execute(&lease, &signed, &second, &[], 3),
                Err(BrokerError::Budget(BudgetError::RepeatedFailure))
            ));
            assert_eq!(repo.snapshot("task").unwrap().state, TaskState::Failed);
        }
        let count = executor.calls.load(Ordering::SeqCst);
        assert!(broker.execute(&lease, &signed, &read(), &[], 4).is_err());
        assert_eq!(executor.calls.load(Ordering::SeqCst), count);
        let spent = repo.snapshot("task").unwrap().budget_spent;
        drop(broker);
        drop(executor);
        drop(repo);
        assert_eq!(
            TaskStateRepository::open(dir.path())
                .unwrap()
                .snapshot("task")
                .unwrap()
                .budget_spent,
            spent
        );
    }
}

#[test]
fn completion_after_expiry_cannot_publish_or_charge_as_a_live_worker() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path(), spec());
    let repo = Arc::new(repo);
    let host = Arc::new(HostAuthority::new().unwrap());
    let signed = host.sign_task(spec()).unwrap();
    let executor = fake(repo.clone(), false, false, 5);
    let broker = broker(repo.clone(), host, executor.clone(), lease.expires_at());
    assert!(matches!(
        broker.execute(&lease, &signed, &read(), &[], 2),
        Err(BrokerError::Budget(BudgetError::Repository(
            RepositoryError::LeaseExpired { .. }
        )))
    ));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert!(repo
        .snapshot("task")
        .unwrap()
        .pending_intents
        .contains_key("key"));
    let next = repo
        .acquire_lease("task", "W2", Duration::from_secs(30), lease.expires_at())
        .unwrap();
    assert_eq!(broker.state("task"), Some(TaskState::NeedsReconciliation));
    assert!(broker
        .execute(&next, &signed, &read(), &[], lease.expires_at() + 1)
        .is_err());
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn exhausted_upfront_budget_executes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = spec();
    spec.budget_limits.max_tokens = 5;
    let (repo, lease) = running(dir.path(), spec.clone());
    let repo = Arc::new(repo);
    let host = Arc::new(HostAuthority::new().unwrap());
    let signed = host.sign_task(spec).unwrap();
    let executor = fake(repo.clone(), false, false, 5);
    let broker = broker(repo.clone(), host, executor.clone(), 2);
    assert!(matches!(
        broker.execute(&lease, &signed, &read(), &[], 2),
        Err(BrokerError::Budget(BudgetError::Exhausted))
    ));
    assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    assert!(repo.snapshot("task").unwrap().pending_intents.is_empty());
    assert_eq!(broker.state("task"), Some(TaskState::Paused));
}
