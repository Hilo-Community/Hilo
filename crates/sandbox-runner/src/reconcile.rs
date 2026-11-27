//! Reconciliation watchdog for `LOT-017`'s documented risk: a Fargate task whose local
//! orchestrator died (Host crash, forced kill) before it could call `StopTask` itself, left
//! running and accumulating cost. This is a second, independent backstop on top of the
//! orchestrator's own `--stop-timeout-secs` (`fargate.rs`): it watches *recorded* runs, not
//! process trees, so it still works after the local process that started a run is gone.
//!
//! Storage-agnostic: `sandbox-runner` has no database of its own (`crates/cloud-service` owns
//! the `fargate_runs` table); callers implement `RunDeadlineStore` over whatever they persist
//! runs in.

use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

/// One run this watchdog may need to stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverdueRun {
    /// Opaque to this crate: an ECS task ARN, or any id `StopTaskClient` can act on.
    pub run_id: String,
}

/// Where overdue runs are read from and marked once stopped. Implementations must not block for
/// long (called on every poll tick) and must be safe to call from a background task.
pub trait RunDeadlineStore: Send + Sync {
    /// Runs whose recorded deadline has already passed and are not yet marked stopped.
    fn overdue(&self) -> Vec<OverdueRun>;
    /// Records that `run_id` was stopped by the watchdog (so it is not reported again).
    fn mark_stopped(&self, run_id: &str);
}

/// Issues the actual stop call. Kept separate from `RunDeadlineStore` so tests can fake the AWS
/// side without faking storage, and vice versa.
pub trait StopTaskClient: Send + Sync {
    /// Must not silently succeed when the task cannot be confirmed stopped: an error here keeps
    /// `mark_stopped` from being called, so the run is retried on the next tick.
    fn stop_task(&self, run_id: &str) -> Result<(), String>;
}

/// Polls `RunDeadlineStore` on an interval and stops every overdue run through `StopTaskClient`.
pub struct ReconciliationWatchdog {
    store: Arc<dyn RunDeadlineStore>,
    client: Arc<dyn StopTaskClient>,
    interval: Duration,
}

impl ReconciliationWatchdog {
    #[must_use]
    pub fn new(
        store: Arc<dyn RunDeadlineStore>,
        client: Arc<dyn StopTaskClient>,
        interval: Duration,
    ) -> Self {
        Self {
            store,
            client,
            interval,
        }
    }

    /// Runs one reconciliation pass now; returns the run ids it stopped.
    pub fn tick(&self) -> Vec<String> {
        let mut stopped = Vec::new();
        for run in self.store.overdue() {
            match self.client.stop_task(&run.run_id) {
                Ok(()) => {
                    self.store.mark_stopped(&run.run_id);
                    stopped.push(run.run_id);
                }
                Err(error) => {
                    warn!(run_id = %run.run_id, %error, "reconciliation watchdog could not stop an overdue run");
                }
            }
        }
        stopped
    }

    /// Runs `tick` forever on `interval`. Intended to be spawned as its own Tokio task by the
    /// Cloud Service; never returns.
    pub async fn run_forever(self) {
        loop {
            self.tick();
            tokio::time::sleep(self.interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeStore {
        overdue: Mutex<Vec<OverdueRun>>,
        stopped: Mutex<Vec<String>>,
    }

    impl RunDeadlineStore for FakeStore {
        fn overdue(&self) -> Vec<OverdueRun> {
            self.overdue.lock().unwrap().clone()
        }

        fn mark_stopped(&self, run_id: &str) {
            self.overdue.lock().unwrap().retain(|r| r.run_id != run_id);
            self.stopped.lock().unwrap().push(run_id.to_string());
        }
    }

    struct AlwaysStops;
    impl StopTaskClient for AlwaysStops {
        fn stop_task(&self, _run_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    struct AlwaysFails;
    impl StopTaskClient for AlwaysFails {
        fn stop_task(&self, _run_id: &str) -> Result<(), String> {
            Err("api unavailable".into())
        }
    }

    #[test]
    fn an_overdue_run_is_stopped_and_marked() {
        let store = Arc::new(FakeStore::default());
        store.overdue.lock().unwrap().push(OverdueRun {
            run_id: "arn:aws:ecs:...:task/1".into(),
        });
        let watchdog = ReconciliationWatchdog::new(
            store.clone(),
            Arc::new(AlwaysStops),
            Duration::from_secs(1),
        );
        let stopped = watchdog.tick();
        assert_eq!(stopped, vec!["arn:aws:ecs:...:task/1".to_string()]);
        assert!(store.overdue().is_empty());
        assert_eq!(
            store.stopped.lock().unwrap().as_slice(),
            ["arn:aws:ecs:...:task/1"]
        );
    }

    #[test]
    fn a_run_that_fails_to_stop_is_retried_next_tick() {
        let store = Arc::new(FakeStore::default());
        store.overdue.lock().unwrap().push(OverdueRun {
            run_id: "arn:aws:ecs:...:task/2".into(),
        });
        let watchdog = ReconciliationWatchdog::new(
            store.clone(),
            Arc::new(AlwaysFails),
            Duration::from_secs(1),
        );
        let stopped = watchdog.tick();
        assert!(stopped.is_empty());
        assert_eq!(
            store.overdue().len(),
            1,
            "an unconfirmed stop must stay overdue"
        );
    }
}
