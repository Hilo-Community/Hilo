//! What a task journal holds, and how it rebuilds the task's state.

use std::collections::BTreeMap;

use host_core::{TaskSpec, TaskState};
use serde::{Deserialize, Serialize};

use super::Millis;

/// How an external action ended, as recorded after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentOutcome {
    pub succeeded: bool,
    /// What the action reported (or what the human who reconciled it found).
    pub summary: String,
}

/// One entry of a task journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Record {
    Created {
        spec: TaskSpec,
        at: Millis,
    },
    LeaseGranted {
        generation: u64,
        worker_id: String,
        expires_at: Millis,
        at: Millis,
    },
    StateChanged {
        generation: u64,
        from: TaskState,
        to: TaskState,
        reason: String,
        at: Millis,
    },
    IntentRecorded {
        generation: u64,
        key: String,
        call_digest: String,
        at: Millis,
    },
    IntentResolved {
        generation: u64,
        key: String,
        outcome: IntentOutcome,
        reconciled_by_human: bool,
        at: Millis,
    },
}

/// What is known about a task, as of its last journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub spec: TaskSpec,
    pub state: TaskState,
    /// The current generation (0 until a first lease).
    pub generation: u64,
    /// The worker holding the current lease and when it ends; `None` once the Host restarted,
    /// since a lease never outlives the process that granted it.
    pub lease: Option<(String, Millis)>,
    /// Unresolved external actions: idempotency key to digest of the call.
    pub pending_intents: BTreeMap<String, String>,
    /// Resolved external actions, by idempotency key.
    pub resolved_intents: BTreeMap<String, IntentOutcome>,
    /// State before the last `Paused`, where a resume must go back to.
    pub paused_from: Option<TaskState>,
}

pub(super) fn new_snapshot(spec: TaskSpec) -> TaskSnapshot {
    TaskSnapshot {
        state: spec.state,
        spec,
        generation: 0,
        lease: None,
        pending_intents: BTreeMap::new(),
        resolved_intents: BTreeMap::new(),
        paused_from: None,
    }
}

/// Applies one journal record to the state it follows.
pub(super) fn apply(snapshot: &mut TaskSnapshot, record: Record) {
    match record {
        Record::Created { .. } => {}
        Record::LeaseGranted {
            generation,
            worker_id,
            expires_at,
            ..
        } => {
            snapshot.generation = generation;
            snapshot.lease = Some((worker_id, expires_at));
        }
        Record::StateChanged { from, to, .. } => {
            if to == TaskState::Paused {
                snapshot.paused_from = Some(from);
            } else if from == TaskState::Paused {
                snapshot.paused_from = None;
            }
            snapshot.state = to;
        }
        Record::IntentRecorded {
            key, call_digest, ..
        } => {
            snapshot.pending_intents.insert(key, call_digest);
        }
        Record::IntentResolved { key, outcome, .. } => {
            snapshot.pending_intents.remove(&key);
            snapshot.resolved_intents.insert(key, outcome);
        }
    }
}
