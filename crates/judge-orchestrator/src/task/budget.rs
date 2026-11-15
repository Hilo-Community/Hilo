//! Atomically reserve against all workers and flush committed usage before acknowledging it.

use super::record::Record;
use super::{Lease, Millis, TaskStateRepository};
use crate::budget::{BudgetError, BudgetUsage};
use host_core::TaskState;

impl TaskStateRepository {
    pub(crate) fn reserve_budget(
        &self,
        lease: &Lease,
        estimate: BudgetUsage,
        now: Millis,
    ) -> Result<u64, BudgetError> {
        self.with_task(lease.task_id(), |task| {
            task.check_lease(lease, now)?;
            task.check_live()?;
            if !matches!(
                task.snapshot.state,
                TaskState::Running | TaskState::Verifying
            ) {
                return Err(super::RepositoryError::NotRunning(task.snapshot.state).into());
            }
            let total = task
                .reservations
                .values()
                .fold(task.snapshot.budget_spent, |sum, usage| sum.add(*usage))
                .add(estimate);
            if !total.fits(task.snapshot.spec.budget_limits.into()) {
                if task.snapshot.pending_intents.is_empty() {
                    task.commit(Record::BudgetSpent {
                        generation: lease.generation(),
                        usage: BudgetUsage::default(),
                        paused: true,
                        at: now,
                    })?;
                }
                return Err(BudgetError::Exhausted);
            }
            let id = task.next_reservation;
            task.next_reservation = id.checked_add(1).ok_or(BudgetError::Exhausted)?;
            task.reservations.insert(id, estimate);
            Ok(id)
        })
    }

    pub(crate) fn release_budget(&self, task_id: &str, id: u64) {
        let _: Result<(), super::RepositoryError> = self.with_task(task_id, |task| {
            task.reservations.remove(&id);
            Ok(())
        });
    }

    pub(crate) fn commit_budget(
        &self,
        lease: &Lease,
        id: u64,
        actual: BudgetUsage,
        now: Millis,
    ) -> Result<(), BudgetError> {
        self.with_task(lease.task_id(), |task| {
            task.check_lease(lease, now)?;
            task.check_live()?;
            let estimate = *task
                .reservations
                .get(&id)
                .ok_or(BudgetError::MissingReservation)?;
            let exceeded = !actual.fits(estimate);
            // A pending external action must remain reconcilable, rather than become paused
            // while its result is unknown. The broker settles its intent before committing.
            if exceeded && !task.snapshot.pending_intents.is_empty() {
                return Err(super::RepositoryError::PendingIntents(
                    task.snapshot.pending_intents.keys().cloned().collect(),
                )
                .into());
            }
            task.commit(Record::BudgetSpent {
                generation: lease.generation(),
                usage: actual,
                paused: exceeded,
                at: now,
            })?;
            task.reservations.remove(&id);
            if exceeded {
                return Err(BudgetError::ActualExceedsReservation);
            }
            Ok(())
        })
    }

    pub(crate) fn correction_cycle(
        &self,
        lease: &Lease,
        signature: &str,
        now: Millis,
    ) -> Result<(), BudgetError> {
        self.with_task(lease.task_id(), |task| {
            task.check_lease(lease, now)?;
            task.check_live()?;
            if !matches!(
                task.snapshot.state,
                TaskState::Running | TaskState::Verifying
            ) {
                return Err(super::RepositoryError::NotRunning(task.snapshot.state).into());
            }
            if !task.snapshot.pending_intents.is_empty() {
                return Err(super::RepositoryError::PendingIntents(
                    task.snapshot.pending_intents.keys().cloned().collect(),
                )
                .into());
            }
            let repeated = task
                .snapshot
                .correction_failures
                .iter()
                .any(|s| s == signature);
            let capped = task.snapshot.correction_failures.len()
                >= usize::from(task.snapshot.spec.policy.max_auto_retries);
            let stopped = if repeated {
                Some(TaskState::Failed)
            } else if capped {
                Some(TaskState::Paused)
            } else {
                None
            };
            task.commit(Record::CorrectionCycle {
                generation: lease.generation(),
                signature: signature.into(),
                stopped,
                at: now,
            })?;
            if repeated {
                return Err(BudgetError::RepeatedFailure);
            }
            if capped {
                return Err(BudgetError::RetryLimit);
            }
            Ok(())
        })
    }
}
