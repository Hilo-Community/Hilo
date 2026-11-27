//! Hilo managed Cloud Service (`LOT-017`): Axum/Postgres/OIDC API, single-tenant Fargate
//! dispatch, and Cloudflare R2/Pages static publishing with 1-click digest rollback.
//!
//! Library surface so `tests/` can build a router against a real `DATABASE_URL` without going
//! through `main`.

pub mod api;
pub mod config;
pub mod db;
pub mod egress;
pub mod fargate_runs;
pub mod oidc;
pub mod publish_service;
pub mod r2_store;
pub mod state;
pub mod tenant_mw;
