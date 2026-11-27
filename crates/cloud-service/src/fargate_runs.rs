//! Wires `sandbox_runner::reconcile`'s storage-agnostic watchdog to the `fargate_runs` Postgres
//! table and to the real `aws ecs stop-task` CLI call. `RunDeadlineStore`/`StopTaskClient` are
//! sync traits (the watchdog itself is driven from an async Tokio task, but the trait boundary
//! predates this crate and is shared with `sandbox-runner`'s own tests); both impls here bridge
//! through a captured `tokio::runtime::Handle::block_on`, the same pattern `FargateEngine` uses
//! to drive the AWS CLI from a synchronous `OsSandboxEngine::prepare`.

use std::process::Command;
use std::sync::Arc;

use sqlx::PgPool;
use tokio::runtime::Handle;

use sandbox_runner::{OverdueRun, RunDeadlineStore};

pub struct PostgresRunDeadlineStore {
    pool: PgPool,
    handle: Handle,
}

impl PostgresRunDeadlineStore {
    #[must_use]
    pub fn new(pool: PgPool, handle: Handle) -> Self {
        Self { pool, handle }
    }
}

impl RunDeadlineStore for PostgresRunDeadlineStore {
    fn overdue(&self) -> Vec<OverdueRun> {
        let pool = self.pool.clone();
        let handle = self.handle.clone();
        // `tick()` runs inside `run_forever`'s own async task on this same multi-thread runtime,
        // so a bare `Handle::block_on` here would panic ("Cannot block the current thread from
        // within a runtime"); `block_in_place` moves the wait off the worker thread instead.
        tokio::task::block_in_place(move || {
            handle.block_on(async move {
                sqlx::query_as::<_, (String,)>(
                    "SELECT ecs_task_arn FROM fargate_runs WHERE status = 'running' AND stop_deadline < now()",
                )
                .fetch_all(&pool)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(run_id,)| OverdueRun { run_id })
                .collect()
            })
        })
    }

    fn mark_stopped(&self, run_id: &str) {
        let pool = self.pool.clone();
        let handle = self.handle.clone();
        let run_id = run_id.to_string();
        tokio::task::block_in_place(move || {
            handle.block_on(async move {
                let _ =
                    sqlx::query("UPDATE fargate_runs SET status = 'stopped' WHERE ecs_task_arn = $1")
                        .bind(&run_id)
                        .execute(&pool)
                        .await;
            });
        });
    }
}

/// Real `StopTaskClient`: shells out to `aws ecs stop-task`, same control-plane convention as
/// `fargate.rs`'s orchestrator (the `aws` CLI, not the Rust SDK, so this crate's only AWS SDK
/// dependency stays the R2/S3 client used by `publish_service.rs`).
pub struct AwsCliStopTaskClient {
    pub aws_cli: String,
    pub region: String,
    pub cluster_arn: String,
}

impl sandbox_runner::reconcile::StopTaskClient for AwsCliStopTaskClient {
    fn stop_task(&self, run_id: &str) -> Result<(), String> {
        let output = Command::new(&self.aws_cli)
            .args(["ecs", "stop-task"])
            .args(["--region", &self.region])
            .args(["--cluster", &self.cluster_arn])
            .args(["--task", run_id])
            .args(["--reason", "reconciliation watchdog: orphaned task"])
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        Ok(())
    }
}

/// Call once at startup, after the Postgres pool and the Tokio runtime both exist, to spawn the
/// background reconciliation loop (`LOT-017`'s "orphaned Fargate tasks" mitigation).
pub fn spawn_watchdog(
    pool: PgPool,
    aws_cli: String,
    region: String,
    cluster_arn: String,
    interval: std::time::Duration,
) {
    let handle = Handle::current();
    let store = Arc::new(PostgresRunDeadlineStore::new(pool, handle));
    let client = Arc::new(AwsCliStopTaskClient {
        aws_cli,
        region,
        cluster_arn,
    });
    let watchdog = sandbox_runner::ReconciliationWatchdog::new(store, client, interval);
    tokio::spawn(watchdog.run_forever());
}
