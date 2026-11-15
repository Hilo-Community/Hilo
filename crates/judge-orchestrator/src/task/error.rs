//! Why a task change was refused.

use std::path::PathBuf;

use host_core::TaskState;
use thiserror::Error;

use super::Millis;

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("task journal I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("task journal {path} is corrupt at byte {offset}: {reason}")]
    Corrupt {
        path: PathBuf,
        offset: u64,
        reason: String,
    },
    /// Nothing was written: the journal is intact. The caller must record less.
    #[error("a task journal record of {bytes} bytes is too large to write to {path}")]
    RecordTooLarge { path: PathBuf, bytes: usize },
    #[error("task journal serialization error: {0}")]
    Serialization(#[from] bincode::Error),
    #[error("invalid task id {0:?}: use 1 to 128 of letters, digits, `.`, `-`, `_`")]
    InvalidTaskId(String),
    #[error("task {0} already exists")]
    TaskExists(String),
    #[error("unknown task {0}")]
    UnknownTask(String),
    #[error("a new task must start in `Created`, not {0:?}")]
    NotCreated(TaskState),
    #[error("task {task_id} journal does not start with its creation")]
    MissingCreation { task_id: String },
    #[error("task {task_id} is leased to {holder} until {until}")]
    LeaseHeld {
        task_id: String,
        holder: String,
        until: Millis,
    },
    #[error("stale lease: generation {lease} is not the current generation {current}")]
    StaleLease { lease: u64, current: u64 },
    #[error("the lease of generation {generation} expired at {expired_at}")]
    LeaseExpired { generation: u64, expired_at: Millis },
    #[error("task {task_id} is {state:?}, which is final")]
    Terminal { task_id: String, state: TaskState },
    #[error("transition {from:?} -> {to:?} is not allowed")]
    IllegalTransition { from: TaskState, to: TaskState },
    #[error("unresolved external actions must be reconciled first: {0:?}")]
    PendingIntents(Vec<String>),
    #[error("external actions are only recorded while running or verifying, not in {0:?}")]
    NotRunning(TaskState),
    #[error("intent {0} is already recorded and unresolved")]
    IntentPending(String),
    #[error("intent {0} is already resolved")]
    IntentResolved(String),
    #[error("no unresolved intent {0}")]
    UnknownIntent(String),
    #[error("the task is not awaiting reconciliation")]
    NotAwaitingReconciliation,
}
