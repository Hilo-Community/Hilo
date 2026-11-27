//! LOT-017 4.5 "Cursor SSE Reconnection & Zero Implicit Sync Test" (blocking): disconnect at
//! event #15, emit #16..#20, reconnect with `Last-Event-ID: 15`, and verify exact delivery of
//! #16..#20 — nothing before it, nothing missing. Needs a real Postgres; see `tests/common`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

#[tokio::test]
async fn reconnecting_with_last_event_id_replays_exactly_the_events_after_it() {
    let Some(database_url) = common::require_database() else {
        return;
    };
    let pool = common::connect(&database_url).await;
    let (tenant_id, user_id, token) = common::seed_session(&pool).await;
    let task_id = cloud_service::db::create_task(&pool, tenant_id, user_id)
        .await
        .unwrap();

    // Emit #1..#15, "disconnect", then #16..#20.
    for n in 1..=20 {
        let cursor = cloud_service::db::append_task_event(
            &pool,
            tenant_id,
            task_id,
            "progress",
            &serde_json::json!({ "n": n }),
        )
        .await
        .unwrap();
        assert_eq!(cursor, n);
    }

    let state = common::fake_app_state(pool.clone());
    let app = cloud_service::api::protected_router(state);

    let request = Request::builder()
        .method("GET")
        .uri(format!("/v1/tasks/{task_id}/events"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Last-Event-ID", "15")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();

    for n in 1..=15 {
        assert!(
            !text.contains(&format!("\"n\":{n}}}")),
            "event #{n} must not be replayed; it was already delivered before reconnection"
        );
    }
    for n in 16..=20 {
        assert!(
            text.contains(&format!("\"n\":{n}}}")),
            "event #{n} must be replayed after reconnecting with Last-Event-ID: 15"
        );
    }
}

#[tokio::test]
async fn a_task_belonging_to_another_tenant_looks_not_found() {
    let Some(database_url) = common::require_database() else {
        return;
    };
    let pool = common::connect(&database_url).await;
    let (owner_tenant, owner_user_id, owner_token) = common::seed_session(&pool).await;
    let (_other_tenant, _other_user_id, other_token) = common::seed_session(&pool).await;
    let task_id = cloud_service::db::create_task(&pool, owner_tenant, owner_user_id)
        .await
        .unwrap();
    let _ = owner_token; // only the owning tenant's session would be allowed to read this

    let state = common::fake_app_state(pool.clone());
    let app = cloud_service::api::protected_router(state);
    let request = Request::builder()
        .method("GET")
        .uri(format!("/v1/tasks/{task_id}/events"))
        .header("Authorization", format!("Bearer {other_token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
