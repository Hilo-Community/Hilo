//! Shared test setup for the LOT-017 integration tests. These need a real Postgres
//! (`DATABASE_URL`) — this sandbox has no Docker daemon reachable, so they could not be run here;
//! every test calls `require_database` first and skips itself (prints why, returns early) when
//! the variable is unset, so `cargo test -p cloud-service` still passes without one. Run them for
//! real with e.g. `docker run -e POSTGRES_PASSWORD=hilo -p 5432:5432 postgres:16` and
//! `DATABASE_URL=postgres://postgres:hilo@localhost:5432/postgres`.

use std::sync::Arc;

use cloud_service::publish_service::fakes::{
    FakeArtifactStore, FakeDeploymentLedger, FakePagesPublisher,
};
use cloud_service::publish_service::{ArtifactStore, DeploymentLedger, PagesPublisher};
use cloud_service::state::AppState;
use sqlx::PgPool;
use uuid::Uuid;

/// `Some(DATABASE_URL)` if set and non-empty; printing a skip notice and returning `None`
/// otherwise, so a caller can `let Some(url) = common::require_database() else { return };`.
pub fn require_database() -> Option<String> {
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ => {
            eprintln!(
                "skipping: DATABASE_URL is not set (see crates/cloud-service/.env.example) — \
                 this test needs a real Postgres, not exercised in this environment"
            );
            None
        }
    }
}

pub async fn connect(database_url: &str) -> PgPool {
    cloud_service::db::connect(database_url)
        .await
        .expect("connect + migrate")
}

/// Inserts a tenant, user and session directly (bypassing the OIDC flow, which needs a real
/// issuer) and returns `(tenant_id, user_id, bearer_token)`.
pub async fn seed_session(pool: &PgPool) -> (Uuid, Uuid, String) {
    let tenant_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, $2)")
        .bind(tenant_id)
        .bind("test-tenant")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, oidc_subject, oidc_issuer) VALUES ($1, $2, $3, $4)",
    )
    .bind(user_id)
    .bind(tenant_id)
    .bind(user_id.to_string())
    .bind("test-issuer")
    .execute(pool)
    .await
    .unwrap();
    let token = Uuid::new_v4().to_string();
    let token_blake3 = blake3::hash(token.as_bytes()).to_hex().to_string();
    sqlx::query(
        "INSERT INTO sessions (id, user_id, tenant_id, token_blake3, expires_at)
         VALUES ($1, $2, $3, $4, now() + interval '1 hour')",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(tenant_id)
    .bind(&token_blake3)
    .execute(pool)
    .await
    .unwrap();
    (tenant_id, user_id, token)
}

/// An `AppState` whose publish pipeline is entirely in-memory fakes — enough for the SSE/tenant
/// tests, which don't exercise `publish_service.rs` itself (that has its own thorough unit tests
/// in `src/publish_service.rs`).
pub fn fake_app_state(pool: PgPool) -> AppState {
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(FakeArtifactStore::default());
    let pages: Arc<dyn PagesPublisher> = Arc::new(FakePagesPublisher::default());
    let ledger_factory: Arc<dyn Fn(String) -> Box<dyn DeploymentLedger> + Send + Sync> =
        Arc::new(|_tenant_id: String| {
            Box::new(FakeDeploymentLedger::default()) as Box<dyn DeploymentLedger>
        });
    AppState::new(pool, artifacts, pages, ledger_factory)
}
