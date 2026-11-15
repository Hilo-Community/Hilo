//! Policy, reservation, idempotency, durable intent, execution, durable settlement. No retries.

mod executor;
mod sandbox;
mod summary;
pub use executor::{ExecutionError, ExecutionReport, ToolExecutor};
pub use sandbox::SandboxToolExecutor;
pub(crate) use summary::MAX_SUMMARY_BYTES;

use crate::{
    budget::{BudgetError, BudgetLedger, BudgetUsage},
    policy::{call_digest, Decision, PolicyEngine, SignedApproval, SignedTaskSpec},
    IntentOutcome, Lease, Millis, RepositoryError, TaskStateRepository,
};
use host_core::{TaskState, ToolCall};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BrokerError {
    #[error("policy did not authorize execution: {0:?}")]
    Policy(Decision),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Execution(#[from] ExecutionError),
    #[error("cannot digest call: {0}")]
    Encoding(#[from] bincode::Error),
    #[error("idempotency key is pending or has a different call digest")]
    IdempotencyConflict,
}

pub struct ToolBroker {
    repository: Arc<TaskStateRepository>,
    policy: PolicyEngine,
    executor: Arc<dyn ToolExecutor>,
    /// Host clock, injected for deterministic tests and rechecked after blocking execution.
    clock: Arc<dyn Fn() -> Millis + Send + Sync>,
}

impl ToolBroker {
    pub fn new(
        repository: Arc<TaskStateRepository>,
        policy: PolicyEngine,
        executor: Arc<dyn ToolExecutor>,
        clock: Arc<dyn Fn() -> Millis + Send + Sync>,
    ) -> Self {
        Self {
            repository,
            policy,
            executor,
            clock,
        }
    }

    /// Runs one tool call for the task, at most once.
    ///
    /// Order: policy; idempotency (a settled call returns its stored outcome without spending);
    /// budget reservation; durable intent; execution; one durable record of outcome and spending.
    /// A failure after the intent leaves it pending for reconciliation, unless the executor
    /// proves nothing ran ([`ExecutionError::NotStarted`]).
    pub fn execute(
        &self,
        lease: &Lease,
        signed: &SignedTaskSpec,
        call: &ToolCall,
        approvals: &[SignedApproval],
        now: Millis,
    ) -> Result<IntentOutcome, BrokerError> {
        let snapshot = self
            .repository
            .snapshot(lease.task_id())
            .ok_or_else(|| RepositoryError::UnknownTask(lease.task_id().into()))?;
        let decision = self
            .policy
            .evaluate(signed, call, approvals, snapshot.state, now);
        if decision != Decision::Allow {
            return Err(BrokerError::Policy(decision));
        }
        if signed.spec != snapshot.spec || lease.task_id() != call.task_id {
            return Err(BrokerError::Policy(Decision::Deny(
                "signed spec differs from persisted task".into(),
            )));
        }
        // Before any spending: replaying a settled call must not reserve budget (and so can
        // never pause the task). A key recorded meanwhile is caught by `record_intent` below.
        let digest = call_digest(call)?;
        if snapshot
            .call_digests
            .get(&call.idempotency_key)
            .is_some_and(|stored| stored != &digest)
            || snapshot.pending_intents.contains_key(&call.idempotency_key)
        {
            return Err(BrokerError::IdempotencyConflict);
        }
        if let Some(outcome) = snapshot.resolved_intents.get(&call.idempotency_key) {
            return Ok(outcome.clone());
        }

        let estimate = self.executor.estimate(&signed.spec, call)?;
        // Every external call costs at least one invocation, even with a zero executor estimate.
        let estimate = BudgetUsage {
            tool_calls: estimate.tool_calls.max(1),
            ..estimate
        };
        let ledger = BudgetLedger::new(self.repository.clone(), lease.clone());
        let guard = ledger.reserve_before_call(estimate, now)?;
        self.repository
            .record_intent(lease, &call.idempotency_key, &digest, now)?;
        let report = match self.executor.execute(&signed.spec, call, estimate) {
            Ok(report) => report,
            Err(ExecutionError::NotStarted(reason)) => {
                // Nothing ran: the outcome is known, so it is closed now and charges nothing.
                let outcome = IntentOutcome {
                    succeeded: false,
                    summary: format!("not started: {reason}"),
                };
                let finished = now.max((self.clock)());
                guard.settle(
                    &call.idempotency_key,
                    outcome,
                    BudgetUsage::default(),
                    finished,
                )?;
                return Err(ExecutionError::NotStarted(reason).into());
            }
            // The action may have happened: its intent stays pending for reconciliation.
            Err(error) => return Err(error.into()),
        };
        let finished = now.max((self.clock)());
        let actual = BudgetUsage {
            tool_calls: report.actual.tool_calls.max(1),
            ..report.actual
        };
        let outcome = summary::bounded(report.outcome);
        guard.settle(&call.idempotency_key, outcome.clone(), actual, finished)?;
        if !outcome.succeeded {
            let signature = report
                .failure_signature
                .unwrap_or_else(|| format!("tool:{}:failed", call.tool_name));
            ledger.record_failure(&signature, finished)?;
        }
        Ok(outcome)
    }

    pub fn begin_correction(
        &self,
        lease: &Lease,
        signature: &str,
        now: Millis,
    ) -> Result<(), BudgetError> {
        BudgetLedger::new(self.repository.clone(), lease.clone()).record_failure(signature, now)
    }

    /// Recovery stays in the repository's human reconciliation flow; this broker never retries.
    pub fn state(&self, task_id: &str) -> Option<TaskState> {
        self.repository.snapshot(task_id).map(|s| s.state)
    }
}
