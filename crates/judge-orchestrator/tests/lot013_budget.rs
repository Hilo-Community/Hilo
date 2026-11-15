mod common;
use common::*;
use host_core::TaskState;
use judge_orchestrator::{budget::*, TaskStateRepository};
use std::{sync::Arc, time::Duration};

/// Host time of every ledger call: after every lease grant of these tests, before expiry.
const NOW: u64 = 4;

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
        let ledger = BudgetLedger::new(repo.clone(), lease);
        assert!(matches!(
            ledger.reserve_before_call(usage, NOW),
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
    let ledger = BudgetLedger::new(repo.clone(), lease);
    drop(ledger.reserve_before_call(tokens(100), NOW).unwrap());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = ledger.reserve_before_call(tokens(100), NOW).unwrap();
        panic!("cancelled model stream");
    }));
    assert!(result.is_err());
    ledger
        .reserve_before_call(tokens(100), NOW)
        .unwrap()
        .commit(tokens(20), NOW)
        .unwrap();
    ledger
        .reserve_before_call(tokens(80), NOW)
        .unwrap()
        .commit(tokens(30), NOW)
        .unwrap();
    assert_eq!(repo.snapshot("task").unwrap().budget_spent.tokens, 50);
    ledger
        .reserve_before_call(tokens(50), NOW)
        .unwrap()
        .commit(tokens(50), NOW)
        .unwrap();
    assert!(matches!(
        ledger.reserve_before_call(tokens(1), NOW),
        Err(BudgetError::Exhausted)
    ));
}

#[test]
fn ledgers_share_reservations_and_spending_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path(), spec());
    let repo = Arc::new(repo);
    let a = BudgetLedger::new(repo.clone(), lease.clone());
    let b = BudgetLedger::new(repo.clone(), lease);
    let held = a.reserve_before_call(tokens(60), NOW).unwrap();
    assert!(matches!(
        b.reserve_before_call(tokens(41), NOW),
        Err(BudgetError::Exhausted)
    ));
    held.commit(tokens(40), NOW).unwrap();
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
    let ledger = BudgetLedger::new(repo.clone(), lease);
    assert!(matches!(
        ledger
            .reserve_before_call(tokens(10), NOW)
            .unwrap()
            .commit(tokens(11), NOW),
        Err(BudgetError::ActualExceedsReservation)
    ));
    assert_eq!(repo.snapshot("task").unwrap().state, TaskState::Paused);
    assert_eq!(repo.snapshot("task").unwrap().budget_spent.tokens, 11);
    assert!(ledger.reserve_before_call(tokens(1), NOW).is_err());

    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path(), spec());
    let repo = Arc::new(repo);
    BudgetLedger::new(repo.clone(), lease)
        .record_failure("tsc:E100", NOW)
        .unwrap();
    drop(repo);
    let repo = Arc::new(TaskStateRepository::open(dir.path()).unwrap());
    let lease = repo
        .acquire_lease("task", "W2", Duration::from_secs(30), 3)
        .unwrap();
    let ledger = BudgetLedger::new(repo.clone(), lease);
    assert!(matches!(
        ledger.record_failure("tsc:E100", NOW),
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
        let ledger = BudgetLedger::new(repo.clone(), lease);
        for cycle in 0..limit {
            ledger
                .record_failure(&format!("failure-{cycle}"), NOW)
                .unwrap();
        }
        assert!(matches!(
            ledger.record_failure("next", NOW),
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
    let ledger = BudgetLedger::new(repo.clone(), lease.clone());
    let guard = ledger.reserve_before_call(tokens(100), NOW).unwrap();
    let next = repo
        .acquire_lease("task", "W1", Duration::from_secs(30), 3)
        .unwrap();
    assert!(guard.commit(tokens(10), NOW).is_err());
    assert!(ledger.record_failure("late", NOW).is_err());
    BudgetLedger::new(repo.clone(), next)
        .reserve_before_call(tokens(100), NOW)
        .unwrap()
        .commit(tokens(100), NOW)
        .unwrap();
    assert_eq!(repo.snapshot("task").unwrap().budget_spent.tokens, 100);
}

#[test]
fn a_ledger_kept_past_its_lease_cannot_reserve_or_record() {
    let dir = tempfile::tempdir().unwrap();
    let (repo, lease) = running(dir.path(), spec());
    let repo = Arc::new(repo);
    let ledger = BudgetLedger::new(repo.clone(), lease.clone());
    ledger.reserve_before_call(tokens(1), NOW).unwrap();
    // Nobody took over, but the lease ran out: the worker holding this ledger is fenced off.
    let late = lease.expires_at();
    assert!(ledger.reserve_before_call(tokens(1), late).is_err());
    assert!(ledger.record_failure("late", late).is_err());
}
