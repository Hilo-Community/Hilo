//! LOT-017 4.5 "Publish & 1-Click Digest Rollback Test" (blocking), driven through the real HTTP
//! surface (`/v1/publish`, `/v1/rollback`) rather than calling `CloudStaticPublishService`
//! directly — `src/publish_service.rs` already has a thorough direct unit test
//! (`publish_then_publish_then_blocked_secret_then_rollback`); this one additionally proves the
//! gate, the scan and the digest pipeline survive going through Axum's JSON extraction and the
//! tenant-session middleware. Needs a real Postgres only for the session lookup; the publish
//! pipeline itself runs against in-memory fakes (`common::fake_app_state`).

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use host_core::contracts::{
    ExecutedCommandRecord, HiloValidationState, PreviewRuntimeReport, QualityDiagnosticsSummary,
};
use host_core::publish::PublishResult;
use tower::ServiceExt;

fn evidence_json(task_id: &str, hash: &str) -> serde_json::Value {
    let evidence = host_core::contracts::Evidence {
        schema_version: 1,
        evidence_id: "ev-http".into(),
        task_id: task_id.into(),
        tested_snapshot_hash: hash.into(),
        checkpoint_ref: "chk-http".into(),
        commands: vec![ExecutedCommandRecord {
            command: "pnpm".into(),
            args: vec!["build".into()],
            exit_code: 0,
            duration_ms: 1,
            impacted_tests_only: false,
        }],
        diagnostics: QualityDiagnosticsSummary::default(),
        preview_report: PreviewRuntimeReport {
            inspected_url: "about:blank".into(),
            console_error_count: 0,
            uncaught_exception_count: 0,
            failed_network_request_count: 0,
            crashed: false,
            dom_snapshot_hash: None,
        },
        artifacts: Vec::new(),
        verdict: HiloValidationState::ValidatedGreen,
    };
    serde_json::to_value(evidence).unwrap()
}

async fn publish(
    app: &axum::Router,
    token: &str,
    dist_root: &std::path::Path,
    task_id: &str,
    hash: &str,
) -> (StatusCode, Option<PublishResult>) {
    let body = serde_json::json!({
        "task_id": task_id,
        "checkpoint_ref": "chk",
        "tested_snapshot_hash": hash,
        "evidence": evidence_json(task_id, hash),
        "dist_root": dist_root,
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/publish")
        .header("Authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let result = serde_json::from_slice(&bytes).ok();
    (status, result)
}

#[tokio::test]
async fn publish_then_block_a_secret_then_rollback_over_http() {
    let Some(database_url) = common::require_database() else {
        return;
    };
    let pool = common::connect(&database_url).await;
    let (_tenant_id, _user_id, token) = common::seed_session(&pool).await;
    let app = cloud_service::api::protected_router(common::fake_app_state(pool.clone()));

    let dir1 = tempfile::tempdir().unwrap();
    std::fs::write(dir1.path().join("index.html"), "<h1>v1</h1>").unwrap();
    let (status1, result1) = publish(&app, &token, dir1.path(), "task-http-1", "hash-v1").await;
    assert_eq!(status1, StatusCode::OK);
    let digest1 = result1.unwrap().digest_blake3;

    let dir2 = tempfile::tempdir().unwrap();
    std::fs::write(
        dir2.path().join("config.js"),
        "const KEY='AKIAABCDEFGHIJKLMNOP';",
    )
    .unwrap();
    let (status2, result2) = publish(&app, &token, dir2.path(), "task-http-1", "hash-v2").await;
    assert_eq!(
        status2,
        StatusCode::FORBIDDEN,
        "a bundle containing a secret must be blocked, never published"
    );
    assert!(result2.is_none());

    let rollback_request = Request::builder()
        .method("POST")
        .uri("/v1/rollback")
        .header("Authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "target_digest": digest1 }).to_string(),
        ))
        .unwrap();
    let rollback_response = app.oneshot(rollback_request).await.unwrap();
    assert_eq!(rollback_response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(rollback_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let rollback_result: PublishResult = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(rollback_result.digest_blake3, digest1);
}
