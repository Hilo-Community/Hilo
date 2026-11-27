//! `POST /v1/publish`, `POST /v1/rollback`: what `host_core::publish::PublishClient` (the
//! desktop's thin client) calls. The actual gate/scan/upload/flip pipeline is
//! `CloudStaticPublishService` (`publish_service.rs`); this module only adapts it to HTTP.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use host_core::contracts::Evidence;
use host_core::publish::{PublishError, PublishResult, RollbackRequest, StaticPublishService};
use serde::Deserialize;

use crate::state::AppState;
use crate::tenant_mw::AuthenticatedTenant;

#[derive(Deserialize)]
pub struct PublishHttpRequest {
    pub task_id: String,
    pub checkpoint_ref: String,
    pub tested_snapshot_hash: String,
    pub evidence: Evidence,
    /// The bundle's files, base64-free for this skeleton: a production deployment streams
    /// `dist/` as a multipart upload instead of inlining it in JSON. Kept explicit here so the
    /// gate (`Evidence`) and the scan (`AclAndSecretScanner`) stay visibly wired end to end.
    pub dist_root: std::path::PathBuf,
}

fn status_for(error: &PublishError) -> StatusCode {
    match error {
        PublishError::NotValidated | PublishError::SecretsDetected(_) => StatusCode::FORBIDDEN,
        PublishError::UnknownDigest(_) => StatusCode::NOT_FOUND,
        PublishError::Remote(_) => StatusCode::BAD_GATEWAY,
    }
}

pub async fn publish(
    State(state): State<AppState>,
    tenant: AuthenticatedTenant,
    Json(request): Json<PublishHttpRequest>,
) -> Result<Json<PublishResult>, StatusCode> {
    let service = (state.publish_factory)(tenant.tenant_id.to_string());
    let publish_request = host_core::publish::PublishRequest {
        task_id: request.task_id,
        checkpoint_ref: request.checkpoint_ref,
        dist_root: request.dist_root,
        tested_snapshot_hash: request.tested_snapshot_hash,
    };
    service
        .publish(&publish_request, &request.evidence)
        .map(Json)
        .map_err(|error| status_for(&error))
}

pub async fn rollback(
    State(state): State<AppState>,
    tenant: AuthenticatedTenant,
    Json(request): Json<RollbackRequest>,
) -> Result<Json<PublishResult>, StatusCode> {
    let service = (state.publish_factory)(tenant.tenant_id.to_string());
    service
        .rollback(&request)
        .map(Json)
        .map_err(|error| status_for(&error))
}
