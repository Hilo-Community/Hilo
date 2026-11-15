//! LOT-013: the persistent task state machine, leases and generation tokens, and the
//! "crash after an external action" blocking scenario.

use std::path::Path;
use std::time::Duration;

use host_core::{
    AutonomyLevel, BudgetLimits, ExecutionPolicy, TargetEnvironment, TaskSpec, TaskState,
    CONTRACT_SCHEMA_VERSION,
};
use judge_orchestrator::{IntentOutcome, Lease, RepositoryError, TaskStateRepository};

const TTL: Duration = Duration::from_secs(30);
const TASK: &str = "task-1";

fn spec(task_id: &str) -> TaskSpec {
    TaskSpec {
        schema_version: CONTRACT_SCHEMA_VERSION,
        task_id: task_id.to_string(),
        objective: "fix the failing test".to_string(),
        workspace_id: "ws".to_string(),
        base_snapshot_id: "snap".to_string(),
        target_env: TargetEnvironment::LocalWorktree,
        policy: ExecutionPolicy {
            autonomy_level: AutonomyLevel::SupervisedSandbox,
            network_allowed: false,
            allowed_paths: vec!["src/".to_string()],
            forbidden_paths: vec![".env".to_string()],
            max_auto_retries: 2,
            workspace_trusted: true,
        },
        budget_limits: BudgetLimits {
            max_cost_usd_micros: 1_000_000,
            max_tokens: 100_000,
            max_cpu_ms: 60_000,
            max_tool_calls: 50,
        },
        acceptance_criteria: vec!["tests pass".to_string()],
        state: TaskState::Created,
    }
}

fn outcome(summary: &str) -> IntentOutcome {
    IntentOutcome {
        succeeded: true,
        summary: summary.to_string(),
    }
}

/// A repository with one task, leased to W1 and driven to `Running`.
fn running(dir: &Path) -> (TaskStateRepository, Lease) {
    let repo = TaskStateRepository::open(dir).unwrap();
    repo.create_task(spec(TASK), 0).unwrap();
    let lease = repo.acquire_lease(TASK, "W1", TTL, 0).unwrap();
    for state in [
        TaskState::ContextReady,
        TaskState::Planned,
        TaskState::Running,
    ] {
        repo.transition(&lease, state, "next step", 1).unwrap();
    }
    (repo, lease)
}

fn state(repo: &TaskStateRepository) -> TaskState {
    repo.snapshot(TASK).unwrap().state
}

fn is_stale(result: Result<impl std::fmt::Debug, RepositoryError>) -> bool {
    matches!(result, Err(RepositoryError::StaleLease { .. }))
}

/// Blocking scenario, process crash: W1 (generation 1) dies after recording an external action
/// and before resolving it; the Host restarts; W2 takes over on generation 2.
#[test]
fn a_crash_mid_action_needs_reconciliation_and_the_old_worker_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, w1) = running(dir.path());
    assert_eq!(w1.generation(), 1);
    repo.record_intent(&w1, "call-1", "digest-1", 2).unwrap();
    drop(repo); // the process dies here: the action's outcome was never recorded

    let repo = TaskStateRepository::open(dir.path()).unwrap();
    let snapshot = repo.snapshot(TASK).unwrap();
    assert_eq!(snapshot.lease, None, "leases do not survive their process");
    let w2 = repo.acquire_lease(TASK, "W2", TTL, 3).unwrap();
    assert_eq!(w2.generation(), 2);
    assert_eq!(state(&repo), TaskState::NeedsReconciliation);
    assert!(repo
        .snapshot(TASK)
        .unwrap()
        .pending_intents
        .contains_key("call-1"));

    // Late writes from W1 are rejected, whatever they try.
    assert!(is_stale(repo.resolve_intent(
        &w1,
        "call-1",
        outcome("done"),
        4
    )));
    assert!(is_stale(repo.transition(
        &w1,
        TaskState::Verifying,
        "late",
        4
    )));
    assert!(is_stale(repo.record_intent(&w1, "call-2", "d", 4)));
    assert!(is_stale(repo.renew_lease(&w1, TTL, 4)));

    // Zero blind retries: no new action and no resuming until a human settles it.
    assert!(matches!(
        repo.record_intent(&w2, "call-1", "digest-1", 5),
        Err(RepositoryError::NotRunning(TaskState::NeedsReconciliation))
    ));
    assert!(matches!(
        repo.transition(&w2, TaskState::Running, "resume", 5),
        Err(RepositoryError::PendingIntents(keys)) if keys == ["call-1"]
    ));
    // The worker that took over cannot claim the outcome either: only reconciliation can.
    assert!(repo
        .resolve_intent(&w2, "call-1", outcome("guess"), 5)
        .is_err());

    repo.reconcile(&w2, "call-1", outcome("checked by hand: applied"), 6)
        .unwrap();
    repo.transition(&w2, TaskState::Running, "reconciled", 7)
        .unwrap();
    // The settled action is never run again under the same key.
    assert!(matches!(
        repo.record_intent(&w2, "call-1", "digest-1", 8),
        Err(RepositoryError::IntentResolved(_))
    ));

    // All of it is durable.
    drop(repo);
    let repo = TaskStateRepository::open(dir.path()).unwrap();
    let snapshot = repo.snapshot(TASK).unwrap();
    assert_eq!(snapshot.state, TaskState::Running);
    assert_eq!(snapshot.generation, 2);
    assert!(snapshot.pending_intents.is_empty());
    assert_eq!(
        snapshot.resolved_intents["call-1"].summary,
        "checked by hand: applied"
    );
}

/// Blocking scenario, hung worker: W1 stops answering in the same process; its lease expires and
/// W2 takes over. W1 waking up later cannot publish anything.
#[test]
fn a_hung_worker_is_replaced_after_its_lease_and_cannot_publish_late() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, w1) = running(dir.path());
    repo.record_intent(&w1, "call-1", "digest-1", 2).unwrap();

    // While W1's lease runs, nobody else gets the task.
    assert!(matches!(
        repo.acquire_lease(TASK, "W2", TTL, 10_000),
        Err(RepositoryError::LeaseHeld { .. })
    ));
    let after_expiry = w1.expires_at();
    let w2 = repo.acquire_lease(TASK, "W2", TTL, after_expiry).unwrap();
    assert_eq!(w2.generation(), 2);
    assert_eq!(state(&repo), TaskState::NeedsReconciliation);

    // W1 comes back with its result: refused, and nothing changed.
    assert!(is_stale(repo.resolve_intent(
        &w1,
        "call-1",
        outcome("late"),
        after_expiry + 1
    )));
    let snapshot = repo.snapshot(TASK).unwrap();
    assert!(snapshot.pending_intents.contains_key("call-1"));
    assert!(snapshot.resolved_intents.is_empty());
}

#[test]
fn an_expired_lease_cannot_write_even_without_a_successor() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, w1) = running(dir.path());
    assert!(matches!(
        repo.transition(&w1, TaskState::Verifying, "too late", w1.expires_at()),
        Err(RepositoryError::LeaseExpired { generation: 1, .. })
    ));
    let renewed = repo.renew_lease(&w1, TTL, w1.expires_at() - 1).unwrap();
    assert_eq!(renewed.generation(), 1);
    repo.transition(&renewed, TaskState::Verifying, "in time", w1.expires_at())
        .unwrap();
}

#[test]
fn a_new_lease_makes_every_earlier_one_stale_even_for_the_same_worker() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, first) = running(dir.path());
    let second = repo.acquire_lease(TASK, "W1", TTL, 5).unwrap();
    assert_eq!(second.generation(), 2);
    assert!(is_stale(repo.transition(
        &first,
        TaskState::Verifying,
        "x",
        6
    )));
    repo.transition(&second, TaskState::Verifying, "x", 6)
        .unwrap();
}

#[test]
fn illegal_transitions_are_refused_and_not_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let repo = TaskStateRepository::open(dir.path()).unwrap();
    repo.create_task(spec(TASK), 0).unwrap();
    let lease = repo.acquire_lease(TASK, "W1", TTL, 0).unwrap();
    assert!(matches!(
        repo.transition(&lease, TaskState::Applied, "skip everything", 1),
        Err(RepositoryError::IllegalTransition { .. })
    ));
    drop(repo);
    let repo = TaskStateRepository::open(dir.path()).unwrap();
    assert_eq!(state(&repo), TaskState::Created);
}

#[test]
fn a_paused_task_resumes_only_where_it_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path());
    repo.transition(&lease, TaskState::Paused, "budget", 2)
        .unwrap();
    assert!(matches!(
        repo.transition(&lease, TaskState::Verifying, "jump", 3),
        Err(RepositoryError::IllegalTransition { .. })
    ));
    // Even across a restart.
    drop(repo);
    let repo = TaskStateRepository::open(dir.path()).unwrap();
    let lease = repo.acquire_lease(TASK, "W1", TTL, 4).unwrap();
    assert_eq!(
        repo.snapshot(TASK).unwrap().paused_from,
        Some(TaskState::Running)
    );
    repo.transition(&lease, TaskState::Running, "resume", 5)
        .unwrap();
}

#[test]
fn nothing_but_reconciliation_while_an_action_is_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path());
    repo.record_intent(&lease, "call-1", "d", 2).unwrap();
    for to in [
        TaskState::Paused,
        TaskState::Verifying,
        TaskState::Cancelled,
    ] {
        assert!(matches!(
            repo.transition(&lease, to, "x", 3),
            Err(RepositoryError::PendingIntents(_))
        ));
    }
    assert!(matches!(
        repo.record_intent(&lease, "call-1", "d", 3),
        Err(RepositoryError::IntentPending(_))
    ));
    repo.resolve_intent(&lease, "call-1", outcome("ok"), 4)
        .unwrap();
    repo.transition(&lease, TaskState::Verifying, "next", 5)
        .unwrap();
}

#[test]
fn a_finished_task_accepts_nothing_more() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path());
    repo.transition(&lease, TaskState::Cancelled, "user", 2)
        .unwrap();
    assert!(matches!(
        repo.transition(&lease, TaskState::Running, "x", 3),
        Err(RepositoryError::Terminal { .. })
    ));
    assert!(matches!(
        repo.acquire_lease(TASK, "W2", TTL, 3),
        Err(RepositoryError::Terminal { .. })
    ));
}

#[test]
fn a_write_cut_short_by_a_crash_is_dropped_and_the_task_keeps_its_last_complete_state() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path());
    repo.transition(&lease, TaskState::Verifying, "x", 2)
        .unwrap();
    drop(repo);
    let journal = dir.path().join(format!("{TASK}.journal"));
    let bytes = std::fs::read(&journal).unwrap();
    std::fs::write(&journal, &bytes[..bytes.len() - 3]).unwrap();
    let repo = TaskStateRepository::open(dir.path()).unwrap();
    assert_eq!(state(&repo), TaskState::Running);
}

#[test]
fn tasks_are_unique_valid_and_start_in_created() {
    let dir = tempfile::tempdir().unwrap();
    let repo = TaskStateRepository::open(dir.path()).unwrap();
    repo.create_task(spec(TASK), 0).unwrap();
    assert!(matches!(
        repo.create_task(spec(TASK), 0),
        Err(RepositoryError::TaskExists(_))
    ));
    assert!(matches!(
        repo.create_task(spec("../escape"), 0),
        Err(RepositoryError::InvalidTaskId(_))
    ));
    let mut late = spec("task-2");
    late.state = TaskState::Running;
    assert!(matches!(
        repo.create_task(late, 0),
        Err(RepositoryError::NotCreated(TaskState::Running))
    ));
    assert_eq!(repo.task_ids(), vec![TASK.to_string()]);
}

#[test]
fn concurrent_workers_never_both_win_a_lease() {
    let dir = tempfile::tempdir().unwrap();
    let repo = std::sync::Arc::new(TaskStateRepository::open(dir.path()).unwrap());
    repo.create_task(spec(TASK), 0).unwrap();
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let repo = std::sync::Arc::clone(&repo);
            std::thread::spawn(move || repo.acquire_lease(TASK, &format!("W{i}"), TTL, 1).is_ok())
        })
        .collect();
    let winners = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(winners, 1);
}
