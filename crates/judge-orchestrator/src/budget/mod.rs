//! Reservations belong to the repository, not an individual worker or model stream.

use crate::{Lease, Millis, RepositoryError, TaskStateRepository};
use host_core::BudgetLimits;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetUsage {
    pub tokens: u64,
    pub cost_usd_micros: u64,
    pub cpu_ms: u64,
    pub tool_calls: u64,
}

impl BudgetUsage {
    pub(crate) fn add(self, other: Self) -> Self {
        Self {
            tokens: self.tokens.saturating_add(other.tokens),
            cost_usd_micros: self.cost_usd_micros.saturating_add(other.cost_usd_micros),
            cpu_ms: self.cpu_ms.saturating_add(other.cpu_ms),
            tool_calls: self.tool_calls.saturating_add(other.tool_calls),
        }
    }

    pub(crate) fn fits(self, limit: Self) -> bool {
        self.tokens <= limit.tokens
            && self.cost_usd_micros <= limit.cost_usd_micros
            && self.cpu_ms <= limit.cpu_ms
            && self.tool_calls <= limit.tool_calls
    }
}

impl From<BudgetLimits> for BudgetUsage {
    fn from(limits: BudgetLimits) -> Self {
        Self {
            tokens: limits.max_tokens.into(),
            cost_usd_micros: limits.max_cost_usd_micros.into(),
            cpu_ms: limits.max_cpu_ms.into(),
            tool_calls: limits.max_tool_calls.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum BudgetError {
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error("budget exhausted; task paused")]
    Exhausted,
    #[error("actual usage exceeds reservation; spending recorded and task paused")]
    ActualExceedsReservation,
    #[error("same failure repeated; task failed")]
    RepeatedFailure,
    #[error("automatic correction limit reached; task paused")]
    RetryLimit,
    #[error("reservation is no longer available")]
    MissingReservation,
}

/// Reserves and charges a task's budget on behalf of one lease. It holds no clock: every call
/// takes the Host time, so an expired lease is refused however long the ledger was kept.
pub struct BudgetLedger {
    repository: Arc<TaskStateRepository>,
    lease: Lease,
}

impl BudgetLedger {
    pub fn new(repository: Arc<TaskStateRepository>, lease: Lease) -> Self {
        Self { repository, lease }
    }

    /// Reserves the maximum `estimate` before a model or tool call. Refused (and the task paused)
    /// when it would exceed the task's limits together with what is spent and reserved.
    pub fn reserve_before_call(
        &self,
        estimate: BudgetUsage,
        now: Millis,
    ) -> Result<BudgetReservationGuard, BudgetError> {
        let id = self.repository.reserve_budget(&self.lease, estimate, now)?;
        Ok(BudgetReservationGuard {
            repository: self.repository.clone(),
            lease: self.lease.clone(),
            id,
        })
    }

    /// Call before starting each automatic correction, including after a Host restart.
    pub fn record_failure(&self, signature: &str, now: Millis) -> Result<(), BudgetError> {
        self.repository
            .correction_cycle(&self.lease, signature, now)
    }
}

/// Cancellation, unwinding, and ordinary errors all release the unused reservation.
pub struct BudgetReservationGuard {
    repository: Arc<TaskStateRepository>,
    lease: Lease,
    id: u64,
}

impl BudgetReservationGuard {
    /// One durable frame closes the outbox and charges its spending, avoiding a crash gap.
    pub(crate) fn settle(
        self,
        key: &str,
        outcome: crate::IntentOutcome,
        actual: BudgetUsage,
        now: Millis,
    ) -> Result<(), BudgetError> {
        self.repository
            .settle_budget(&self.lease, self.id, key, outcome, actual, now)
    }

    /// Charges what the call actually used and releases the rest of the reservation.
    pub fn commit(self, actual: BudgetUsage, now: Millis) -> Result<(), BudgetError> {
        self.repository
            .commit_budget(&self.lease, self.id, actual, now)
    }
}

impl Drop for BudgetReservationGuard {
    fn drop(&mut self) {
        self.repository
            .release_budget(self.lease.task_id(), self.id);
    }
}
