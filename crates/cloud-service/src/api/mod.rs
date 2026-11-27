pub mod auth;
pub mod events;
pub mod publish;
pub mod tasks;

use axum::middleware;
use axum::routing::{get, post};
use axum::Router;

use crate::state::AppState;

/// Every endpoint that requires a valid bearer session (`tenant_mw::require_session`, which is
/// also what attaches the tenant every handler scopes its queries by). Exposed separately from
/// [`router`] so tests can drive it directly, bypassing the OIDC login flow (which needs a real
/// issuer) by seeding a session row straight into Postgres instead.
pub fn protected_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/tasks", post(tasks::create_task))
        .route(
            "/v1/tasks/{task_id}/events",
            get(events::task_events).post(tasks::append_event),
        )
        .route("/v1/publish", post(publish::publish))
        .route("/v1/rollback", post(publish::rollback))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state,
            crate::tenant_mw::require_session,
        ))
}

/// `/v1/auth/*`: public, since it is how a session is obtained at all.
pub fn public_router(auth_state: auth::AuthState) -> Router {
    Router::new()
        .route("/v1/auth/login", get(auth::login))
        .route("/v1/auth/callback", get(auth::callback))
        .with_state(auth_state)
}

/// The full app: [`public_router`] merged with [`protected_router`].
pub fn router(state: AppState, auth_state: auth::AuthState) -> Router {
    public_router(auth_state).merge(protected_router(state))
}
