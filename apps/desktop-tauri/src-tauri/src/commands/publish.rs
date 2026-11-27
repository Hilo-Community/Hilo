//! Publishing a `🟢 Validated` build to the managed Cloud Service (`LOT-017`).
//!
//! Deliberately thin: this never talks to R2, Cloudflare or AWS. It only calls
//! `host_core::publish::PublishClient`, which calls the Cloud Service's own HTTP endpoints —
//! R2/Cloudflare/KMS credentials stay server-side.
//!
//! **Known gap, left for a follow-up pass:** the frontend passes `evidence` directly rather than
//! this command looking it up from a `SignedTaskSpec`/`SignedApproval`-gated flow through
//! `judge_orchestrator::policy::PolicyEngine` (`ToolCapability::DeployPreview`/`DeployProduction`
//! → `ApprovalActionKind::DeployInfrastructure`, which already exists and already requires human
//! approval for exactly this). Wiring that through needs the same signed-task plumbing
//! `commands::judge` uses for accept/reject, which was out of scope for this pass; see
//! `docs/en/LOTS/LOT-017.md`.

use std::path::PathBuf;
use std::sync::Arc;

use host_core::contracts::Evidence;
use host_core::publish::{PublishClient, PublishRequest, PublishResult, RollbackRequest, StaticPublishService};
use serde::Deserialize;

use crate::state::DesktopAppState;

fn client(state: &DesktopAppState) -> Result<Arc<PublishClient>, String> {
    let mut guard = state
        .publish_client
        .lock()
        .map_err(|_| "publish client lock poisoned".to_string())?;
    if guard.is_none() {
        let base_url = std::env::var("HILO_CLOUD_SERVICE_URL")
            .map_err(|_| "publishing is not configured (HILO_CLOUD_SERVICE_URL is unset)".to_string())?;
        let token = std::env::var("HILO_CLOUD_BEARER_TOKEN").ok();
        let mut built = PublishClient::new(base_url);
        if let Some(token) = token {
            built = built.with_bearer_token(token);
        }
        *guard = Some(Arc::new(built));
    }
    Ok(Arc::clone(guard.as_ref().unwrap()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishBuildArgs {
    pub task_id: String,
    pub checkpoint_ref: String,
    pub dist_root: PathBuf,
    pub tested_snapshot_hash: String,
    pub evidence: Evidence,
}

#[tauri::command]
pub async fn publish_build_command(
    state: tauri::State<'_, DesktopAppState>,
    args: PublishBuildArgs,
) -> Result<PublishResult, String> {
    let client = client(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        client
            .publish(
                &PublishRequest {
                    task_id: args.task_id,
                    checkpoint_ref: args.checkpoint_ref,
                    dist_root: args.dist_root,
                    tested_snapshot_hash: args.tested_snapshot_hash,
                },
                &args.evidence,
            )
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
pub async fn rollback_build_command(
    state: tauri::State<'_, DesktopAppState>,
    target_digest: String,
) -> Result<PublishResult, String> {
    let client = client(&state)?;
    tauri::async_runtime::spawn_blocking(move || {
        client
            .rollback(&RollbackRequest { target_digest })
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}
