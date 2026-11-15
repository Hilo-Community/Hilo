//! The persistent task state machine (`LOT-013`).
//!
//! Every task has its own append-only journal. A change is written and flushed before it takes
//! effect, so the state in memory is never ahead of the disk.
//!
//! Workers act through a [`Lease`]. Taking a lease starts a new *generation*. Every write
//! carries the writer's generation and is refused unless it is the current one and the lease
//! has not expired: a worker that hung or was replaced cannot publish anything afterwards.
//!
//! An external action is recorded as an *intent* before it starts and resolved after it ends.
//! A worker that takes over a task with an unresolved intent cannot tell whether the action
//! happened, so the task moves to `NeedsReconciliation` and nothing retries it: a human settles
//! each such intent first ([`TaskStateRepository::reconcile`]).

mod budget;
mod error;
mod journal;
mod lease;
mod record;
mod repository;
mod settlement;
mod state;
pub mod transitions;

pub use error::RepositoryError;
pub use lease::Lease;
pub use record::{IntentOutcome, TaskSnapshot};
pub use repository::TaskStateRepository;

/// Milliseconds since the Unix epoch. Passed in, never read from the clock here, so that the
/// behaviour is deterministic and testable.
pub type Millis = u64;
