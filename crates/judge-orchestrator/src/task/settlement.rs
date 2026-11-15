//! Closing an external action and charging its budget is one atomic journal event.

use super::record::Record;
use super::{IntentOutcome, Lease, Millis, RepositoryError, TaskStateRepository};
use crate::budget::{BudgetError, BudgetUsage};
use host_core::TaskState;

impl TaskStateRepository {
    pub(crate) fn settle_budget(
        &self,
        lease: &Lease,
        id: u64,
        key: &str,
        outcome: IntentOutcome,
        actual: BudgetUsage,
        now: Millis,
    ) -> Result<(), BudgetError> {
        self.with_task(lease.task_id(), |task| {
            task.check_lease(lease, now)?;
            task.check_live()?;
            if task.snapshot.state == TaskState::NeedsReconciliation {
                return Err(RepositoryError::NotAwaitingReconciliation.into());
            }
            if !task.snapshot.pending_intents.contains_key(key) {
                return Err(RepositoryError::UnknownIntent(key.into()).into());
            }
            let estimate = *task
                .reservations
                .get(&id)
                .ok_or(BudgetError::MissingReservation)?;
            let exceeded = !actual.fits(estimate);
            task.commit(Record::IntentSettled {
                generation: lease.generation(),
                key: key.into(),
                outcome,
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
}
