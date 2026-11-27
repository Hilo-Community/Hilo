//! Postgres access. Every query takes `tenant_id` explicitly — there is no ambient "current
//! tenant" to forget to set (`tenant_mw.rs` extracts it once per request and every handler
//! threads it through by hand). Runtime-checked queries (`sqlx::query`/`query_as`), not the
//! `query!` macro: this crate must compile without a live database (CI, and this sandbox, have
//! none), so query shape is verified by the tests in `tests/`, run against a real Postgres the
//! operator provides locally (`DATABASE_URL`), not at compile time.

use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tokio::runtime::Handle;
use uuid::Uuid;

use crate::publish_service::DeploymentLedger;

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

#[derive(Debug, Clone)]
pub struct TaskEvent {
    pub cursor: i64,
    pub event_type: String,
    pub payload: serde_json::Value,
}

/// Appends one event and returns its assigned cursor. Allocates the cursor from
/// `tasks.next_event_cursor` inside the same transaction, so two concurrent appends for the same
/// task never collide (`FOR UPDATE` row lock).
pub async fn append_task_event(
    pool: &PgPool,
    tenant_id: Uuid,
    task_id: Uuid,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<i64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let cursor: i64 = sqlx::query_scalar(
        "UPDATE tasks SET next_event_cursor = next_event_cursor + 1
         WHERE id = $1 AND tenant_id = $2
         RETURNING next_event_cursor - 1",
    )
    .bind(task_id)
    .bind(tenant_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO task_events (task_id, tenant_id, cursor, event_type, payload)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(task_id)
    .bind(tenant_id)
    .bind(cursor)
    .bind(event_type)
    .bind(payload)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(cursor)
}

/// Every event for `task_id` strictly after `after_cursor` (0 for "from the start"), scoped to
/// `tenant_id` so a cross-tenant replay returns nothing rather than another tenant's events —
/// this is what `events.rs`'s SSE handler replays on reconnect (`Last-Event-ID`).
pub async fn task_events_after(
    pool: &PgPool,
    tenant_id: Uuid,
    task_id: Uuid,
    after_cursor: i64,
) -> Result<Vec<TaskEvent>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT cursor, event_type, payload FROM task_events
         WHERE task_id = $1 AND tenant_id = $2 AND cursor > $3
         ORDER BY cursor ASC",
    )
    .bind(task_id)
    .bind(tenant_id)
    .bind(after_cursor)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| TaskEvent {
            cursor: row.get("cursor"),
            event_type: row.get("event_type"),
            payload: row.get("payload"),
        })
        .collect())
}

/// `None` when the task does not exist for this tenant: a cross-tenant lookup must look exactly
/// like a not-found, never leak that the row exists under another tenant.
pub async fn task_belongs_to_tenant(
    pool: &PgPool,
    tenant_id: Uuid,
    task_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query("SELECT 1 FROM tasks WHERE id = $1 AND tenant_id = $2")
        .bind(task_id)
        .bind(tenant_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

pub async fn create_task(pool: &PgPool, tenant_id: Uuid, created_by: Uuid) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO tasks (id, tenant_id, created_by) VALUES ($1, $2, $3)")
        .bind(id)
        .bind(tenant_id)
        .bind(created_by)
        .execute(pool)
        .await?;
    Ok(id)
}

/// Postgres-backed `DeploymentLedger` (`publish_service.rs`): the real `publish_deployments`
/// table. Bound to one tenant at construction, mirroring `CloudStaticPublishService` itself.
/// Sync trait, bridged to the async pool via `block_in_place`, same convention as
/// `fargate_runs.rs`/`r2_store.rs`.
pub struct PostgresDeploymentLedger {
    pool: PgPool,
    handle: Handle,
    tenant_id: Uuid,
}

impl PostgresDeploymentLedger {
    #[must_use]
    pub fn new(pool: PgPool, handle: Handle, tenant_id: String) -> Self {
        let tenant_id = Uuid::parse_str(&tenant_id).unwrap_or_else(|_| Uuid::nil());
        Self { pool, handle, tenant_id }
    }
}

impl DeploymentLedger for PostgresDeploymentLedger {
    fn record_active(&self, _tenant_id: &str, task_id: &str, digest: &str, object_key: &str) {
        let pool = self.pool.clone();
        let tenant_id = self.tenant_id;
        let task_id = Uuid::parse_str(task_id).unwrap_or_else(|_| Uuid::nil());
        let digest = digest.to_string();
        let object_key = object_key.to_string();
        tokio::task::block_in_place(move || {
            self.handle.block_on(async move {
                let mut tx = match pool.begin().await {
                    Ok(tx) => tx,
                    Err(_) => return,
                };
                let _ = sqlx::query(
                    "UPDATE publish_deployments SET status = 'superseded'
                     WHERE tenant_id = $1 AND status = 'active'",
                )
                .bind(tenant_id)
                .execute(&mut *tx)
                .await;
                let _ = sqlx::query(
                    "INSERT INTO publish_deployments (id, tenant_id, task_id, digest_blake3, object_key, status)
                     VALUES ($1, $2, $3, $4, $5, 'active')
                     ON CONFLICT (tenant_id, digest_blake3) DO UPDATE SET status = 'active'",
                )
                .bind(Uuid::new_v4())
                .bind(tenant_id)
                .bind(task_id)
                .bind(&digest)
                .bind(&object_key)
                .execute(&mut *tx)
                .await;
                let _ = tx.commit().await;
            });
        });
    }

    fn object_key_for(&self, _tenant_id: &str, digest: &str) -> Option<String> {
        let pool = self.pool.clone();
        let tenant_id = self.tenant_id;
        let digest = digest.to_string();
        tokio::task::block_in_place(move || {
            self.handle.block_on(async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT object_key FROM publish_deployments WHERE tenant_id = $1 AND digest_blake3 = $2",
                )
                .bind(tenant_id)
                .bind(&digest)
                .fetch_optional(&pool)
                .await
                .ok()
                .flatten()
            })
        })
    }

    fn reactivate(&self, tenant_id: &str, digest: &str) -> Option<String> {
        let object_key = self.object_key_for(tenant_id, digest)?;
        let pool = self.pool.clone();
        let tenant_uuid = self.tenant_id;
        let digest_owned = digest.to_string();
        tokio::task::block_in_place(move || {
            self.handle.block_on(async move {
                let mut tx = pool.begin().await.ok()?;
                sqlx::query(
                    "UPDATE publish_deployments SET status = 'superseded'
                     WHERE tenant_id = $1 AND status = 'active'",
                )
                .bind(tenant_uuid)
                .execute(&mut *tx)
                .await
                .ok()?;
                sqlx::query(
                    "UPDATE publish_deployments SET status = 'active'
                     WHERE tenant_id = $1 AND digest_blake3 = $2",
                )
                .bind(tenant_uuid)
                .bind(&digest_owned)
                .execute(&mut *tx)
                .await
                .ok()?;
                tx.commit().await.ok()?;
                Some(())
            })
        })?;
        Some(object_key)
    }
}
