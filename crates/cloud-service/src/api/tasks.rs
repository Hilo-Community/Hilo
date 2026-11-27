//! Minimal task creation/event-append endpoints: enough surface for
//! `tests/lot017_sse_cursor.rs` to drive a real HTTP round trip. Dispatch to
//! `FargateSandboxRunner` (`sandbox_runner::FargateEngine`, wired the same way the desktop wires
//! `OsNativeSandbox<SeatbeltEngine>` into `judge_orchestrator::broker::SandboxToolExecutor`) is a
//! follow-up once real AWS credentials are supplied; see `docs/en/LOTS/LOT-017.md`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db;
use crate::state::AppState;
use crate::tenant_mw::AuthenticatedTenant;

#[derive(Serialize)]
pub struct CreateTaskResponse {
    pub task_id: Uuid,
}

pub async fn create_task(
    State(state): State<AppState>,
    tenant: AuthenticatedTenant,
) -> Result<Json<CreateTaskResponse>, StatusCode> {
    let task_id = db::create_task(&state.pool, tenant.tenant_id, tenant.user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(CreateTaskResponse { task_id }))
}

#[derive(Deserialize)]
pub struct AppendEventRequest {
    pub event_type: String,
    pub payload: serde_json::Value,
}

#[derive(Serialize)]
pub struct AppendEventResponse {
    pub cursor: i64,
}

pub async fn append_event(
    State(state): State<AppState>,
    tenant: AuthenticatedTenant,
    axum::extract::Path(task_id): axum::extract::Path<Uuid>,
    Json(request): Json<AppendEventRequest>,
) -> Result<Json<AppendEventResponse>, StatusCode> {
    if !db::task_belongs_to_tenant(&state.pool, tenant.tenant_id, task_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        return Err(StatusCode::NOT_FOUND);
    }
    let cursor = db::append_task_event(
        &state.pool,
        tenant.tenant_id,
        task_id,
        &request.event_type,
        &request.payload,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(AppendEventResponse { cursor }))
}
