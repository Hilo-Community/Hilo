//! `TaskStateRepository`: the only way to create and change tasks.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use host_core::{TaskSpec, TaskState};

use super::journal::Journal;
use super::record::{apply, new_snapshot, IntentOutcome, Record, TaskSnapshot};
use super::transitions;
use super::{Lease, Millis, RepositoryError};

struct Task {
    journal: Journal,
    snapshot: TaskSnapshot,
}

impl Task {
    fn apply(&mut self, record: Record) {
        apply(&mut self.snapshot, record);
    }

    /// Flushes `record` to disk, then applies it.
    fn commit(&mut self, record: Record) -> Result<(), RepositoryError> {
        self.journal.append(&record)?;
        self.apply(record);
        Ok(())
    }

    fn check_lease(&self, lease: &Lease, now: Millis) -> Result<(), RepositoryError> {
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

    fn check_live(&self) -> Result<(), RepositoryError> {
        if self.snapshot.state.is_terminal() {
            return Err(RepositoryError::Terminal {
                task_id: self.snapshot.spec.task_id.clone(),
                state: self.snapshot.state,
            });
        }
        Ok(())
    }
}

fn valid_task_id(task_id: &str) -> bool {
    (1..=128).contains(&task_id.len())
        && task_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !task_id.starts_with('.')
}

fn millis(duration: Duration) -> Millis {
    Millis::try_from(duration.as_millis()).unwrap_or(Millis::MAX)
}

/// Stores every task of a workspace under one directory and is the only way to change them.
pub struct TaskStateRepository {
    dir: PathBuf,
    tasks: Mutex<HashMap<String, Task>>,
}

impl std::fmt::Debug for TaskStateRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskStateRepository")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl TaskStateRepository {
    /// Opens (or creates) the repository in `dir` and replays every task journal. Leases of the
    /// previous process are void: their workers are gone with it.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, RepositoryError> {
        let dir = dir.into();
        fs::create_dir_all(&dir).map_err(|source| RepositoryError::Io {
            path: dir.clone(),
            source,
        })?;
        let mut tasks = HashMap::new();
        let entries = fs::read_dir(&dir).map_err(|source| RepositoryError::Io {
            path: dir.clone(),
            source,
        })?;
        for entry in entries {
            let path = entry
                .map_err(|source| RepositoryError::Io {
                    path: dir.clone(),
                    source,
                })?
                .path();
            let Some(task_id) = journal_task_id(&path) else {
                continue;
            };
            let (journal, records) = Journal::open::<Record>(&path)?;
            let mut records = records.into_iter();
            let Some(Record::Created { spec, .. }) = records.next() else {
                return Err(RepositoryError::MissingCreation { task_id });
            };
            let mut snapshot = new_snapshot(spec);
            for record in records {
                apply(&mut snapshot, record);
            }
            snapshot.lease = None;
            tasks.insert(task_id, Task { journal, snapshot });
        }
        Ok(Self {
            dir,
            tasks: Mutex::new(tasks),
        })
    }

    fn tasks(&self) -> MutexGuard<'_, HashMap<String, Task>> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records a new task, in `Created`.
    pub fn create_task(&self, spec: TaskSpec, now: Millis) -> Result<(), RepositoryError> {
        if !valid_task_id(&spec.task_id) {
            return Err(RepositoryError::InvalidTaskId(spec.task_id));
        }
        if spec.state != TaskState::Created {
            return Err(RepositoryError::NotCreated(spec.state));
        }
        let mut tasks = self.tasks();
        if tasks.contains_key(&spec.task_id) {
            return Err(RepositoryError::TaskExists(spec.task_id));
        }
        let path = self
            .dir
            .join(format!("{}.{JOURNAL_EXTENSION}", spec.task_id));
        let mut journal = Journal::create(&path).map_err(|error| match error {
            RepositoryError::Io { source, .. }
                if source.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                RepositoryError::TaskExists(spec.task_id.clone())
            }
            other => other,
        })?;
        let record = Record::Created {
            spec: spec.clone(),
            at: now,
        };
        journal.append(&record)?;
        let task_id = spec.task_id.clone();
        tasks.insert(
            task_id,
            Task {
                journal,
                snapshot: new_snapshot(spec),
            },
        );
        Ok(())
    }

    /// The current state of a task.
    #[must_use]
    pub fn snapshot(&self, task_id: &str) -> Option<TaskSnapshot> {
        self.tasks().get(task_id).map(|task| task.snapshot.clone())
    }

    /// Ids of every known task.
    #[must_use]
    pub fn task_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.tasks().keys().cloned().collect();
        ids.sort();
        ids
    }

    fn with_task<T>(
        &self,
        task_id: &str,
        action: impl FnOnce(&mut Task) -> Result<T, RepositoryError>,
    ) -> Result<T, RepositoryError> {
        let mut tasks = self.tasks();
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| RepositoryError::UnknownTask(task_id.to_string()))?;
        action(task)
    }

    /// Gives `worker_id` the task for `ttl`, as a new generation: every lease handed out before
    /// is stale from now on. Refused while another worker holds a lease that has not expired.
    ///
    /// If the task has an external action that was started and never resolved, nobody can tell
    /// whether it happened: the task moves to `NeedsReconciliation` (recorded under the new
    /// generation) and the action is not retried.
    pub fn acquire_lease(
        &self,
        task_id: &str,
        worker_id: &str,
        ttl: Duration,
        now: Millis,
    ) -> Result<Lease, RepositoryError> {
        self.with_task(task_id, |task| {
            task.check_live()?;
            if let Some((holder, until)) = &task.snapshot.lease {
                if holder != worker_id && now < *until {
                    return Err(RepositoryError::LeaseHeld {
                        task_id: task_id.to_string(),
                        holder: holder.clone(),
                        until: *until,
                    });
                }
            }
            let generation = task.snapshot.generation + 1;
            let expires_at = now.saturating_add(millis(ttl));
            task.commit(Record::LeaseGranted {
                generation,
                worker_id: worker_id.to_string(),
                expires_at,
                at: now,
            })?;
            let state = task.snapshot.state;
            if !task.snapshot.pending_intents.is_empty() && state != TaskState::NeedsReconciliation
            {
                let keys: Vec<&str> = task
                    .snapshot
                    .pending_intents
                    .keys()
                    .map(String::as_str)
                    .collect();
                task.commit(Record::StateChanged {
                    generation,
                    from: state,
                    to: TaskState::NeedsReconciliation,
                    reason: format!("interrupted external actions: {}", keys.join(", ")),
                    at: now,
                })?;
            }
            Ok(Lease::new(task_id, worker_id, generation, expires_at))
        })
    }

    /// Extends a live lease, same generation.
    pub fn renew_lease(
        &self,
        lease: &Lease,
        ttl: Duration,
        now: Millis,
    ) -> Result<Lease, RepositoryError> {
        self.with_task(lease.task_id(), |task| {
            task.check_lease(lease, now)?;
            let expires_at = now.saturating_add(millis(ttl));
            task.commit(Record::LeaseGranted {
                generation: lease.generation(),
                worker_id: lease.worker_id().to_string(),
                expires_at,
                at: now,
            })?;
            Ok(lease.extended_to(expires_at))
        })
    }

    /// Moves the task to `to`, if the transition table allows it. While an external action is
    /// unresolved, the only move is to `NeedsReconciliation`.
    pub fn transition(
        &self,
        lease: &Lease,
        to: TaskState,
        reason: &str,
        now: Millis,
    ) -> Result<TaskState, RepositoryError> {
        self.with_task(lease.task_id(), |task| {
            task.check_live()?;
            task.check_lease(lease, now)?;
            let from = task.snapshot.state;
            let pending = &task.snapshot.pending_intents;
            if !pending.is_empty() && to != TaskState::NeedsReconciliation {
                return Err(RepositoryError::PendingIntents(
                    pending.keys().cloned().collect(),
                ));
            }
            let resumes_elsewhere = from == TaskState::Paused
                && !matches!(to, TaskState::Cancelled | TaskState::Failed)
                && task.snapshot.paused_from != Some(to);
            if resumes_elsewhere || !transitions::is_allowed(from, to) {
                return Err(RepositoryError::IllegalTransition { from, to });
            }
            task.commit(Record::StateChanged {
                generation: lease.generation(),
                from,
                to,
                reason: reason.to_string(),
                at: now,
            })?;
            Ok(to)
        })
    }

    /// Records, before it starts, that an external action is about to happen.
    pub fn record_intent(
        &self,
        lease: &Lease,
        idempotency_key: &str,
        call_digest: &str,
        now: Millis,
    ) -> Result<(), RepositoryError> {
        self.with_task(lease.task_id(), |task| {
            task.check_live()?;
            task.check_lease(lease, now)?;
            let state = task.snapshot.state;
            if !matches!(state, TaskState::Running | TaskState::Verifying) {
                return Err(RepositoryError::NotRunning(state));
            }
            if task.snapshot.pending_intents.contains_key(idempotency_key) {
                return Err(RepositoryError::IntentPending(idempotency_key.to_string()));
            }
            if task.snapshot.resolved_intents.contains_key(idempotency_key) {
                return Err(RepositoryError::IntentResolved(idempotency_key.to_string()));
            }
            task.commit(Record::IntentRecorded {
                generation: lease.generation(),
                key: idempotency_key.to_string(),
                call_digest: call_digest.to_string(),
                at: now,
            })
        })
    }

    /// Records how an external action ended, by the worker that started it.
    pub fn resolve_intent(
        &self,
        lease: &Lease,
        idempotency_key: &str,
        outcome: IntentOutcome,
        now: Millis,
    ) -> Result<(), RepositoryError> {
        self.with_task(lease.task_id(), |task| {
            task.check_live()?;
            task.check_lease(lease, now)?;
            if task.snapshot.state == TaskState::NeedsReconciliation {
                return Err(RepositoryError::StaleLease {
                    lease: lease.generation(),
                    current: task.snapshot.generation,
                });
            }
            if !task.snapshot.pending_intents.contains_key(idempotency_key) {
                return Err(RepositoryError::UnknownIntent(idempotency_key.to_string()));
            }
            task.commit(Record::IntentResolved {
                generation: lease.generation(),
                key: idempotency_key.to_string(),
                outcome,
                reconciled_by_human: false,
                at: now,
            })
        })
    }

    /// A human states how an interrupted external action actually ended (after checking the
    /// world). Only in `NeedsReconciliation`; work resumes once none is left unresolved.
    pub fn reconcile(
        &self,
        lease: &Lease,
        idempotency_key: &str,
        outcome: IntentOutcome,
        now: Millis,
    ) -> Result<(), RepositoryError> {
        self.with_task(lease.task_id(), |task| {
            task.check_live()?;
            task.check_lease(lease, now)?;
            if task.snapshot.state != TaskState::NeedsReconciliation {
                return Err(RepositoryError::NotAwaitingReconciliation);
            }
            if !task.snapshot.pending_intents.contains_key(idempotency_key) {
                return Err(RepositoryError::UnknownIntent(idempotency_key.to_string()));
            }
            task.commit(Record::IntentResolved {
                generation: lease.generation(),
                key: idempotency_key.to_string(),
                outcome,
                reconciled_by_human: true,
                at: now,
            })
        })
    }
}

const JOURNAL_EXTENSION: &str = "journal";

fn journal_task_id(path: &Path) -> Option<String> {
    if path.extension()?.to_str()? != JOURNAL_EXTENSION {
        return None;
    }
    let id = path.file_stem()?.to_str()?;
    valid_task_id(id).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::valid_task_id;

    #[test]
    fn task_ids_cannot_name_other_files() {
        assert!(valid_task_id("task-1.a_b"));
        assert!(!valid_task_id(""));
        assert!(!valid_task_id("../escape"));
        assert!(!valid_task_id("a/b"));
        assert!(!valid_task_id(".hidden"));
        assert!(!valid_task_id(&"a".repeat(129)));
    }
}
