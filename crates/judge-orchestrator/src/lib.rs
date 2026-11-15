//! The deterministic governance core (`LOT-013`): the persistent task state machine, and (in
//! progress) the `PolicyEngine`, the `BudgetLedger` and the `ToolBroker`.
//!
//! The model proposes; this crate decides what is authorized and records every step durably
//! (`AGENTS.md` rule 1). Health-check targets are configuration, not measured guarantees.

pub mod budget;
pub mod policy;
pub mod task;

pub use task::{IntentOutcome, Lease, Millis, RepositoryError, TaskSnapshot, TaskStateRepository};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgeHealthStatus {
    pub subsystem: String,
    pub max_auto_retries: u8,
    pub deterministic_evidence_required: bool,
    /// False while the task state machine and authorization engine are not yet implemented.
    pub policy_engine_operational: bool,
}

pub fn healthcheck() -> JudgeHealthStatus {
    JudgeHealthStatus {
        subsystem: "judge-orchestrator".to_string(),
        max_auto_retries: 2,
        deterministic_evidence_required: true,
        policy_engine_operational: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judge_orchestrator_healthcheck_enforces_two_retries_limit() {
        let status = healthcheck();
        assert_eq!(status.subsystem, "judge-orchestrator");
        assert_eq!(status.max_auto_retries, 2);
        assert!(status.deterministic_evidence_required);
        assert!(!status.policy_engine_operational);
    }
}
