use std::sync::Arc;
use std::time::Duration;

use cloud_service::api::auth::AuthState;
use cloud_service::config::Config;
use cloud_service::oidc::OidcClient;
use cloud_service::publish_service::{ArtifactStore, DeploymentLedger, PagesPublisher};
use cloud_service::r2_store::{CloudflarePagesPublisher, R2ArtifactStore};
use cloud_service::state::AppState;
use cloud_service::{api, db, fargate_runs};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let config = Config::from_env()?;

    let pool = db::connect(&config.database_url).await?;

    let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(config.aws_region.clone()))
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            &config.r2_access_key_id,
            &config.r2_secret_access_key,
            None,
            None,
            "r2",
        ))
        .endpoint_url(config.r2_endpoint())
        .load()
        .await;
    let s3_client = aws_sdk_s3::Client::new(&aws_config);

    let handle = tokio::runtime::Handle::current();
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(R2ArtifactStore::new(
        s3_client,
        config.r2_bucket.clone(),
        handle.clone(),
    ));
    let pages: Arc<dyn PagesPublisher> = Arc::new(CloudflarePagesPublisher::new(
        config.cloudflare_api_token.clone(),
        config.r2_account_id.clone(),
        config.cloudflare_pages_project.clone(),
        handle.clone(),
    )?);

    // `publish_deployments` ledger, scoped per tenant at call time by `AppState::new`'s factory.
    let ledger_pool = pool.clone();
    let ledger_factory: Arc<dyn Fn(String) -> Box<dyn DeploymentLedger> + Send + Sync> =
        Arc::new(move |tenant_id: String| {
            Box::new(cloud_service::db::PostgresDeploymentLedger::new(
                ledger_pool.clone(),
                handle.clone(),
                tenant_id,
            )) as Box<dyn DeploymentLedger>
        });

    let app_state = AppState::new(pool.clone(), artifacts, pages, ledger_factory);

    let oidc = OidcClient::discover(
        &config.oidc_issuer_url,
        &config.oidc_client_id,
        &config.oidc_client_secret,
        &config.oidc_redirect_url,
    )
    .await?;
    let auth_state = AuthState {
        pool: pool.clone(),
        oidc: Arc::new(oidc),
    };

    fargate_runs::spawn_watchdog(
        pool.clone(),
        "aws".to_string(),
        config.aws_region.clone(),
        config.ecs_cluster_arn.clone(),
        Duration::from_secs(60),
    );

    let app = api::router(app_state, auth_state);
    let listener = tokio::net::TcpListener::bind(&config.listen_addr).await?;
    tracing::info!(addr = %config.listen_addr, "hilo-cloud-service listening");
    axum::serve(listener, app).await?;
    Ok(())
}
