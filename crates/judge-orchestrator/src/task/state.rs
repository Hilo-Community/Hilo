//! Shared atomic journal commit and worker fencing for every repository operation.

use super::journal::Journal;
use super::record::{apply, Record, TaskSnapshot};
use super::{Lease, Millis, RepositoryError};
use crate::budget::BudgetUsage;
use std::collections::HashMap;

pub(super) struct Task {
    pub(super) journal: Journal,
    pub(super) snapshot: TaskSnapshot,
    pub(super) reservations: HashMap<u64, BudgetUsage>,
    pub(super) next_reservation: u64,
}

impl Task {
    /// Flush before changing memory; reservations of a replaced worker cannot hold a new one.
    pub(super) fn commit(&mut self, record: Record) -> Result<(), RepositoryError> {
        self.journal.append(&record)?;
        if matches!(&record, Record::LeaseGranted { generation, .. } if *generation != self.snapshot.generation)
        {
            self.reservations.clear();
        }
        apply(&mut self.snapshot, record);
        Ok(())
    }

    pub(super) fn check_lease(&self, lease: &Lease, now: Millis) -> Result<(), RepositoryError> {
        let current = self.snapshot.generation;
        let live = self
            .snapshot
            .lease
            .as_ref()
            .is_some_and(|(worker, _)| *worker == lease.worker_id());
        if lease.generation() != current || !live {
            return Err(RepositoryError::StaleLease {
                lease: lease.generation(),
                current,
            });
        }
        let expires_at = self.snapshot.lease.as_ref().map_or(0, |(_, until)| *until);
        if now >= expires_at {
            return Err(RepositoryError::LeaseExpired {
                generation: lease.generation(),
                expired_at: expires_at,
            });
        }
        Ok(())
    }

    pub(super) fn check_live(&self) -> Result<(), RepositoryError> {
        if self.snapshot.state.is_terminal() {
            return Err(RepositoryError::Terminal {
                task_id: self.snapshot.spec.task_id.clone(),
                state: self.snapshot.state,
            });
        }
        Ok(())
    }
}
