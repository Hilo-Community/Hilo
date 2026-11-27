//! Tenant isolation middleware (`LOT-017`): the authenticated tenant is extracted once, here,
//! from the session's bearer token, and attached to the request. Every handler receives it as an
//! extractor argument and threads it explicitly into every `db.rs` call — there is no "current
//! tenant" global a handler could forget to apply (`tests/lot017_tenant_isolation.rs`).

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

use crate::state::AppState;

#[derive(Debug, Clone, Copy)]
pub struct AuthenticatedTenant {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
}

impl<S> FromRequestParts<S> for AuthenticatedTenant
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<AuthenticatedTenant>()
            .copied()
            .ok_or((StatusCode::UNAUTHORIZED, "missing or invalid session"))
    }
}

/// Looks up the bearer token's session, attaches the tenant/user it belongs to, or rejects with
/// 401 before any handler (and therefore any tenant-scoped query) runs.
pub async fn require_session(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let pool = state.pool;
    let token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let token_blake3 = blake3::hash(token.as_bytes()).to_hex().to_string();
    let row = sqlx::query_as::<_, (Uuid, Uuid)>(
        "SELECT tenant_id, user_id FROM sessions WHERE token_blake3 = $1 AND expires_at > now()",
    )
    .bind(&token_blake3)
    .fetch_optional(&pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let (tenant_id, user_id) = row.ok_or(StatusCode::UNAUTHORIZED)?;
    request
        .extensions_mut()
        .insert(AuthenticatedTenant { tenant_id, user_id });
    Ok(next.run(request).await)
}
