//! Config from environment variables only (`.env.example` documents every key). No default ever
//! widens scope (a missing bucket or cluster name is a startup error, not an empty string that
//! would make every call target "nothing" silently).

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub oidc_issuer_url: String,
    pub oidc_client_id: String,
    pub oidc_client_secret: String,
    pub oidc_redirect_url: String,
    pub r2_account_id: String,
    pub r2_bucket: String,
    pub r2_access_key_id: String,
    pub r2_secret_access_key: String,
    pub cloudflare_api_token: String,
    pub cloudflare_pages_project: String,
    pub aws_region: String,
    pub ecs_cluster_arn: String,
    pub ecs_task_definition_arn: String,
    pub ecs_subnets: Vec<String>,
    pub ecs_security_group: String,
    pub fargate_scratch_bucket: String,
    pub listen_addr: String,
}

#[derive(Debug, thiserror::Error)]
#[error("missing or empty required environment variable: {0}")]
pub struct ConfigError(pub &'static str);

fn require(name: &'static str) -> Result<String, ConfigError> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError(name))
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            database_url: require("DATABASE_URL")?,
            oidc_issuer_url: require("OIDC_ISSUER_URL")?,
            oidc_client_id: require("OIDC_CLIENT_ID")?,
            oidc_client_secret: require("OIDC_CLIENT_SECRET")?,
            oidc_redirect_url: require("OIDC_REDIRECT_URL")?,
            r2_account_id: require("R2_ACCOUNT_ID")?,
            r2_bucket: require("R2_BUCKET")?,
            r2_access_key_id: require("R2_ACCESS_KEY_ID")?,
            r2_secret_access_key: require("R2_SECRET_ACCESS_KEY")?,
            cloudflare_api_token: require("CLOUDFLARE_API_TOKEN")?,
            cloudflare_pages_project: require("CLOUDFLARE_PAGES_PROJECT")?,
            aws_region: require("AWS_REGION")?,
            ecs_cluster_arn: require("ECS_CLUSTER_ARN")?,
            ecs_task_definition_arn: require("ECS_TASK_DEFINITION_ARN")?,
            ecs_subnets: require("ECS_SUBNETS")?
                .split(',')
                .map(str::to_string)
                .collect(),
            ecs_security_group: require("ECS_SECURITY_GROUP")?,
            fargate_scratch_bucket: require("FARGATE_SCRATCH_BUCKET")?,
            listen_addr: std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into()),
        })
    }

    #[must_use]
    pub fn r2_endpoint(&self) -> String {
        format!("https://{}.r2.cloudflarestorage.com", self.r2_account_id)
    }
}
