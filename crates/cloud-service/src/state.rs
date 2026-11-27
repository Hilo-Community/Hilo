//! Shared Axum state: the Postgres pool and the per-tenant `CloudStaticPublishService` factory.
//! R2/Cloudflare credentials live only inside the closures this module builds, never in a
//! handler or in a type a handler could accidentally log.

use std::collections::BTreeMap;
use std::sync::Arc;

use host_core::publish::PublishError;
use sqlx::PgPool;

use crate::publish_service::{ArtifactStore, CloudStaticPublishService, DeploymentLedger, PagesPublisher};

/// Lets a shared `Arc<dyn ArtifactStore>` (one upload destination for every tenant) be handed
/// into `CloudStaticPublishService::new`, which wants its own `Box<dyn ArtifactStore>` per call.
impl ArtifactStore for Arc<dyn ArtifactStore> {
    fn put_bundle(&self, digest: &str, files: &BTreeMap<String, Vec<u8>>) -> Result<String, PublishError> {
        (**self).put_bundle(digest, files)
    }
}

impl PagesPublisher for Arc<dyn PagesPublisher> {
    fn set_active(&self, object_key: &str) -> Result<String, PublishError> {
        (**self).set_active(object_key)
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub publish_factory: Arc<dyn Fn(String) -> CloudStaticPublishService + Send + Sync>,
}

impl AppState {
    #[must_use]
    pub fn new(
        pool: PgPool,
        artifacts: Arc<dyn ArtifactStore>,
        pages: Arc<dyn PagesPublisher>,
        ledger_factory: Arc<dyn Fn(String) -> Box<dyn DeploymentLedger> + Send + Sync>,
    ) -> Self {
        let publish_factory = Arc::new(move |tenant_id: String| {
            CloudStaticPublishService::new(
                tenant_id.clone(),
                Box::new(Arc::clone(&artifacts)),
                Box::new(Arc::clone(&pages)),
                ledger_factory(tenant_id),
            )
        });
        Self { pool, publish_factory }
    }
}
