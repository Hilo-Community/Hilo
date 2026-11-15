mod common;
use common::*;
use host_core::TaskState;
use judge_orchestrator::{budget::*, TaskStateRepository};
use std::{sync::Arc, time::Duration};

fn tokens(tokens: u64) -> BudgetUsage {
    BudgetUsage {
        tokens,
        ..BudgetUsage::default()
    }
}

#[test]
fn reservation_is_upfront_and_all_four_limits_are_enforced() {
    for usage in [
        tokens(101),
        BudgetUsage {
            cost_usd_micros: 101,
            ..Default::default()
        },
        BudgetUsage {
            cpu_ms: 101,
            ..Default::default()
        },
        BudgetUsage {
            tool_calls: 11,
            ..Default::default()
        },
        tokens(u64::MAX),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (repo, lease) = running(dir.path(), spec());
        let repo = Arc::new(repo);
        let ledger = BudgetLedger::new(repo.clone(), lease, 2);
        assert!(matches!(
            ledger.reserve_before_call(usage),
            Err(BudgetError::Exhausted)
        ));
        assert_eq!(repo.snapshot("task").unwrap().state, TaskState::Paused);
        assert_eq!(
            repo.snapshot("task").unwrap().budget_spent,
            BudgetUsage::default()
        );
    }
}

#[test]
fn cancellation_error_and_panic_release_reservations_and_commit_releases_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path(), spec());
    let repo = Arc::new(repo);
    let ledger = BudgetLedger::new(repo.clone(), lease, 2);
    drop(ledger.reserve_before_call(tokens(100)).unwrap());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = ledger.reserve_before_call(tokens(100)).unwrap();
        panic!("cancelled model stream");
    }));
    assert!(result.is_err());
    ledger
        .reserve_before_call(tokens(100))
        .unwrap()
        .commit(tokens(20))
        .unwrap();
    ledger
        .reserve_before_call(tokens(80))
        .unwrap()
        .commit(tokens(30))
        .unwrap();
    assert_eq!(repo.snapshot("task").unwrap().budget_spent.tokens, 50);
    ledger
        .reserve_before_call(tokens(50))
        .unwrap()
        .commit(tokens(50))
        .unwrap();
    assert!(matches!(
        ledger.reserve_before_call(tokens(1)),
        Err(BudgetError::Exhausted)
    ));
}

#[test]
fn ledgers_share_reservations_and_spending_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path(), spec());
    let repo = Arc::new(repo);
    let a = BudgetLedger::new(repo.clone(), lease.clone(), 2);
    let b = BudgetLedger::new(repo.clone(), lease, 2);
    let held = a.reserve_before_call(tokens(60)).unwrap();
    assert!(matches!(
        b.reserve_before_call(tokens(41)),
        Err(BudgetError::Exhausted)
    ));
    held.commit(tokens(40)).unwrap();
    drop(a);
    drop(b);
    drop(repo);
    let repo = TaskStateRepository::open(dir.path()).unwrap();
    assert_eq!(repo.snapshot("task").unwrap().budget_spent.tokens, 40);
    assert_eq!(repo.snapshot("task").unwrap().state, TaskState::Paused);
}

#[test]
fn budget_and_loop_circuit_breaker_halts_on_actual_overrun_or_second_identical_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path(), spec());
    let repo = Arc::new(repo);
    let ledger = BudgetLedger::new(repo.clone(), lease, 2);
    assert!(matches!(
        ledger
            .reserve_before_call(tokens(10))
            .unwrap()
            .commit(tokens(11)),
        Err(BudgetError::ActualExceedsReservation)
    ));
    assert_eq!(repo.snapshot("task").unwrap().state, TaskState::Paused);
    assert_eq!(repo.snapshot("task").unwrap().budget_spent.tokens, 11);
    assert!(ledger.reserve_before_call(tokens(1)).is_err());

    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path(), spec());
    let repo = Arc::new(repo);
    BudgetLedger::new(repo.clone(), lease, 2)
        .record_failure("tsc:E100")
        .unwrap();
    drop(repo);
    let repo = Arc::new(TaskStateRepository::open(dir.path()).unwrap());
    let lease = repo
        .acquire_lease("task", "W2", Duration::from_secs(30), 3)
        .unwrap();
    let ledger = BudgetLedger::new(repo.clone(), lease, 4);
    assert!(matches!(
        ledger.record_failure("tsc:E100"),
        Err(BudgetError::RepeatedFailure)
    ));
    assert_eq!(repo.snapshot("task").unwrap().state, TaskState::Failed);
}

#[test]
fn automatic_retries_default_to_two_and_only_host_policy_can_raise_limit() {
    for limit in [0, 2, 3] {
        let dir = tempfile::tempdir().unwrap();
        let mut spec = spec();
        spec.policy.max_auto_retries = limit;
        let (repo, lease) = running(dir.path(), spec);
        let repo = Arc::new(repo);
        let ledger = BudgetLedger::new(repo.clone(), lease, 2);
        for cycle in 0..limit {
            ledger.record_failure(&format!("failure-{cycle}")).unwrap();
        }
        assert!(matches!(
            ledger.record_failure("next"),
            Err(BudgetError::RetryLimit)
        ));
        assert_eq!(repo.snapshot("task").unwrap().state, TaskState::Paused);
    }
}

#[test]
fn expired_generation_cannot_commit_or_start_a_correction_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path(), spec());
    let repo = Arc::new(repo);
    let ledger = BudgetLedger::new(repo.clone(), lease.clone(), 2);
    let guard = ledger.reserve_before_call(tokens(100)).unwrap();
    let next = repo
        .acquire_lease("task", "W1", Duration::from_secs(30), 3)
        .unwrap();
    assert!(guard.commit(tokens(10)).is_err());
    assert!(ledger.record_failure("late").is_err());
    BudgetLedger::new(repo.clone(), next, 4)
        .reserve_before_call(tokens(100))
        .unwrap()
        .commit(tokens(100))
        .unwrap();
    assert_eq!(repo.snapshot("task").unwrap().budget_spent.tokens, 100);
}
