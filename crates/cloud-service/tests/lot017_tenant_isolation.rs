//! LOT-017's tenant isolation invariant (`tenant_mw.rs`): every tenant-scoped table carries
//! `tenant_id`, and every query filters by it explicitly. Two tenants, cross-tenant access must
//! be denied without ever confirming the resource exists under another tenant. Needs a real
//! Postgres; see `tests/common`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn one_tenants_bearer_token_cannot_read_another_tenants_task_events() {
    let Some(database_url) = common::require_database() else {
        return;
    };
    let pool = common::connect(&database_url).await;

    let (tenant_a, user_a, token_a) = common::seed_session(&pool).await;
    let (_tenant_b, _user_b, token_b) = common::seed_session(&pool).await;

    let task_id = cloud_service::db::create_task(&pool, tenant_a, user_a)
        .await
        .unwrap();
    cloud_service::db::append_task_event(
        &pool,
        tenant_a,
        task_id,
        "progress",
        &serde_json::json!({ "secret": "tenant-a-only" }),
    )
    .await
    .unwrap();

    let app = cloud_service::api::protected_router(common::fake_app_state(pool.clone()));

    // Tenant A reads its own task's events: succeeds.
    let own_request = Request::builder()
        .method("GET")
        .uri(format!("/v1/tasks/{task_id}/events"))
        .header("Authorization", format!("Bearer {token_a}"))
        .body(Body::empty())
        .unwrap();
    let own_response = app.clone().oneshot(own_request).await.unwrap();
    assert_eq!(own_response.status(), StatusCode::OK);

    // Tenant B, with a perfectly valid session of its own, reads tenant A's task: 404, not 403 —
    // it must look exactly like the task does not exist, never confirm it belongs to someone
    // else.
    let cross_tenant_request = Request::builder()
        .method("GET")
        .uri(format!("/v1/tasks/{task_id}/events"))
        .header("Authorization", format!("Bearer {token_b}"))
        .body(Body::empty())
        .unwrap();
    let cross_tenant_response = app.clone().oneshot(cross_tenant_request).await.unwrap();
    assert_eq!(cross_tenant_response.status(), StatusCode::NOT_FOUND);

    // No session at all: 401, distinct from the "valid session, wrong tenant" 404 above.
    let unauthenticated_request = Request::builder()
        .method("GET")
        .uri(format!("/v1/tasks/{task_id}/events"))
        .body(Body::empty())
        .unwrap();
    let unauthenticated_response = app.oneshot(unauthenticated_request).await.unwrap();
    assert_eq!(unauthenticated_response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn posting_an_event_to_another_tenants_task_is_refused() {
    let Some(database_url) = common::require_database() else {
        return;
    };
    let pool = common::connect(&database_url).await;

    let (tenant_a, user_a, _token_a) = common::seed_session(&pool).await;
    let (_tenant_b, _user_b, token_b) = common::seed_session(&pool).await;
    let task_id = cloud_service::db::create_task(&pool, tenant_a, user_a)
        .await
        .unwrap();

    let app = cloud_service::api::protected_router(common::fake_app_state(pool.clone()));
    let request = Request::builder()
        .method("POST")
        .uri(format!("/v1/tasks/{task_id}/events"))
        .header("Authorization", format!("Bearer {token_b}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({ "event_type": "progress", "payload": {} }).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let remaining = cloud_service::db::task_events_after(&pool, tenant_a, task_id, 0)
        .await
        .unwrap();
    assert!(
        remaining.is_empty(),
        "tenant B's event must never have been appended to tenant A's task"
    );
}
