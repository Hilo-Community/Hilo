//! `GET /v1/auth/login`, `GET /v1/auth/callback`: OIDC Authorization Code + PKCE (`oidc.rs`).
//! A successful callback issues an opaque bearer session token; only its BLAKE3 hash is stored
//! (`sessions.token_blake3`), matching the convention `sessions` migrations document.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Redirect;
use axum::Json;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::oidc::OidcClient;

#[derive(Clone)]
pub struct AuthState {
    pub pool: PgPool,
    pub oidc: Arc<OidcClient>,
}

pub async fn login(State(auth): State<AuthState>) -> Redirect {
    Redirect::temporary(&auth.oidc.authorize_url())
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub state: String,
    pub code: String,
}

#[derive(Serialize)]
pub struct CallbackResponse {
    pub bearer_token: String,
}

pub async fn callback(
    State(auth): State<AuthState>,
    Query(query): Query<CallbackQuery>,
) -> Result<Json<CallbackResponse>, StatusCode> {
    let subject = auth
        .oidc
        .complete_callback(&query.state, &query.code)
        .await
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    // One tenant per OIDC subject on first login, for this skeleton; a managed deployment with
    // invitations would look up an existing tenant membership instead of creating one here.
    let issuer = "configured-issuer"; // Set from `OidcClient` config in a full wiring pass.
    let user_id: Uuid = sqlx::query_scalar(
        "SELECT id FROM users WHERE oidc_issuer = $1 AND oidc_subject = $2",
    )
    .bind(issuer)
    .bind(&subject)
    .fetch_optional(&auth.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .unwrap_or_else(Uuid::new_v4);

    let tenant_id: Option<Uuid> = sqlx::query_scalar("SELECT tenant_id FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&auth.pool)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let tenant_id = match tenant_id {
        Some(id) => id,
        None => {
            let tenant_id = Uuid::new_v4();
            sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, $2)")
                .bind(tenant_id)
                .bind(format!("tenant-{subject}"))
                .execute(&auth.pool)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            sqlx::query(
                "INSERT INTO users (id, tenant_id, oidc_subject, oidc_issuer) VALUES ($1, $2, $3, $4)",
            )
            .bind(user_id)
            .bind(tenant_id)
            .bind(&subject)
            .bind(issuer)
            .execute(&auth.pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            tenant_id
        }
    };

    let token = Uuid::new_v4().to_string();
    let token_blake3 = blake3::hash(token.as_bytes()).to_hex().to_string();
    sqlx::query(
        "INSERT INTO sessions (id, user_id, tenant_id, token_blake3, expires_at)
         VALUES ($1, $2, $3, $4, now() + interval '24 hours')",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(tenant_id)
    .bind(&token_blake3)
    .execute(&auth.pool)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(CallbackResponse { bearer_token: token }))
}
