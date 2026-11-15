//! The right to change one task.

use super::Millis;

/// The right to change one task, for one generation. Only the repository creates leases, so a
/// worker cannot claim a generation it was not given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    task_id: String,
    worker_id: String,
    generation: u64,
    expires_at: Millis,
}

impl Lease {
    #[must_use]
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    #[must_use]
    pub fn worker_id(&self) -> &str {
        &self.worker_id
    }

    /// The `generation_token`.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn expires_at(&self) -> Millis {
        self.expires_at
    }
}

impl Lease {
    pub(super) fn new(task_id: &str, worker_id: &str, generation: u64, expires_at: Millis) -> Self {
        Self {
            task_id: task_id.to_string(),
            worker_id: worker_id.to_string(),
            generation,
            expires_at,
        }
    }

    /// The same lease, ending at `expires_at`.
    pub(super) fn extended_to(&self, expires_at: Millis) -> Self {
        Self {
            expires_at,
            ..self.clone()
        }
    }
}
