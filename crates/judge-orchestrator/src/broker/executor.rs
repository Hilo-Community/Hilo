//! Executors are trusted Host adapters, never dynamically supplied by MCP or a model.

use crate::{budget::BudgetUsage, IntentOutcome};
use host_core::{TaskSpec, ToolCall};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("unsupported or invalid execution: {0}")]
    Invalid(String),
    #[error(transparent)]
    Sandbox(#[from] sandbox_runner::SandboxError),
}

pub struct ExecutionReport {
    pub outcome: IntentOutcome,
    pub actual: BudgetUsage,
    /// A stable failure class, not a timestamp or arbitrary model-generated explanation.
    pub failure_signature: Option<String>,
}

pub trait ToolExecutor: Send + Sync {
    /// Pure maximum estimate: this must never invoke the tool or incur external spending.
    fn estimate(&self, spec: &TaskSpec, call: &ToolCall) -> Result<BudgetUsage, ExecutionError>;
    /// Respect the reservation while streaming. An error leaves an unknown outcome pending;
    /// only a complete report proves that the action can be durably resolved.
    fn execute(
        &self,
        spec: &TaskSpec,
        call: &ToolCall,
        reserved: BudgetUsage,
    ) -> Result<ExecutionReport, ExecutionError>;
}
