//! Real `ArtifactStore`/`PagesPublisher` (`LOT-017`): Cloudflare R2 (S3-compatible) for bundle
//! bytes, the Cloudflare Pages API for the live deployment pointer. Not exercised against a real
//! Cloudflare account in this pass — see `docs/en/LOTS/LOT-017.md`. `publish_service.rs`'s tests
//! use `publish_service::fakes` instead; these are wired only from `main.rs`.

use std::collections::BTreeMap;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client as S3Client;
use host_core::publish::PublishError;
use tokio::runtime::Handle;

use crate::egress::EgressAllowlist;
use crate::publish_service::{ArtifactStore, PagesPublisher};

pub struct R2ArtifactStore {
    client: S3Client,
    bucket: String,
    handle: Handle,
}

impl R2ArtifactStore {
    #[must_use]
    pub fn new(client: S3Client, bucket: String, handle: Handle) -> Self {
        Self {
            client,
            bucket,
            handle,
        }
    }
}

/// `put_bundle` is sync (the `ArtifactStore` trait boundary, shared with `publish_service.rs`'s
/// sync tests); bridges to the async S3 SDK via `block_in_place`, same convention as
/// `fargate_runs.rs`.
impl ArtifactStore for R2ArtifactStore {
    fn put_bundle(
        &self,
        digest: &str,
        files: &BTreeMap<String, Vec<u8>>,
    ) -> Result<String, PublishError> {
        let tar = tar_bytes(files).map_err(|error| PublishError::Remote(error.to_string()))?;
        let key = format!("bundles/{digest}.tar");
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        let key_clone = key.clone();
        tokio::task::block_in_place(move || {
            self.handle.block_on(async move {
                client
                    .put_object()
                    .bucket(bucket)
                    .key(key_clone)
                    .body(ByteStream::from(tar))
                    .send()
                    .await
                    .map_err(|error| PublishError::Remote(error.to_string()))
            })
        })?;
        Ok(key)
    }
}

fn tar_bytes(files: &BTreeMap<String, Vec<u8>>) -> std::io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, bytes.as_slice())?;
    }
    builder.into_inner()
}

/// Flips the Cloudflare Pages project's production deployment to serve `object_key`, through the
/// Cloudflare API, over the egress allowlist (`egress.rs`) — the only non-AWS network call this
/// service makes.
pub struct CloudflarePagesPublisher {
    http: reqwest::blocking::Client,
    api_token: String,
    account_id: String,
    project_name: String,
    allowlist: EgressAllowlist,
    handle: Handle,
}

const CLOUDFLARE_API_HOST: &str = "api.cloudflare.com";

impl CloudflarePagesPublisher {
    pub fn new(
        api_token: String,
        account_id: String,
        project_name: String,
        handle: Handle,
    ) -> Result<Self, PublishError> {
        let allowlist = EgressAllowlist::new(vec![CLOUDFLARE_API_HOST.to_string()])
            .map_err(|error| PublishError::Remote(error.to_string()))?;
        Ok(Self {
            http: reqwest::blocking::Client::new(),
            api_token,
            account_id,
            project_name,
            allowlist,
            handle,
        })
    }
}

impl PagesPublisher for CloudflarePagesPublisher {
    fn set_active(&self, object_key: &str) -> Result<String, PublishError> {
        let allowlist = self.allowlist.clone();
        tokio::task::block_in_place(|| self.handle.block_on(allowlist.verify(CLOUDFLARE_API_HOST)))
            .map_err(|error| PublishError::Remote(error.to_string()))?;

        let url = format!(
            "https://{CLOUDFLARE_API_HOST}/client/v4/accounts/{}/pages/projects/{}/deployments",
            self.account_id, self.project_name
        );
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.api_token)
            .json(&serde_json::json!({ "source_object_key": object_key }))
            .send()
            .map_err(|error| PublishError::Remote(error.to_string()))?;
        if !response.status().is_success() {
            return Err(PublishError::Remote(format!(
                "Cloudflare Pages deployment failed: {}",
                response.status()
            )));
        }
        Ok(format!("https://{}.pages.dev/", self.project_name))
    }
}
