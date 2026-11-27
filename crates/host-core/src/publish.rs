//! `StaticPublishService` contract (`LOT-017`): promoting a `🟢 Validated` build to the managed
//! Cloud Service's Cloudflare R2/Pages publishing pipeline, with 1-click rollback by digest.
//!
//! Only the **contract** and a thin HTTP client live here. The concrete implementation — the
//! `AclAndSecretScanner` pass over `dist/`, the R2 upload, the Cloudflare Pages pointer flip, and
//! the KMS-backed credentials that make those calls — lives server-side in `crates/cloud-service`,
//! which can depend on both `host-core` and `context-engine` (this crate cannot depend on
//! `context-engine`: it already depends on `host-core`, and a cycle is not an option). The
//! desktop app only ever holds a [`PublishClient`], so R2/Cloudflare/KMS credentials never reach
//! the desktop binary or a sandboxed build task, matching the LOT's "zero LLM secrets or cloud
//! deployment roles accessible inside ECS Fargate build tasks" acceptance criterion.
//!
//! Every call is gated on [`Evidence::satisfies_validated_green`] here, not on a stored flag the
//! server could be asked to trust blindly: the exact content hash of `dist/` must match what the
//! Judge actually tested (`LOT-015`'s Invariant #4).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::contracts::Evidence;

/// What is being promoted. `tested_snapshot_hash` must equal the BLAKE3 of the exact `dist_root`
/// bundle the caller is asking to publish — not trusted from the caller, re-derived by whichever
/// `StaticPublishService` implementation receives this request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishRequest {
    pub task_id: String,
    pub checkpoint_ref: String,
    pub dist_root: PathBuf,
    pub tested_snapshot_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishResult {
    pub digest_blake3: String,
    /// Milliseconds since the Unix epoch.
    pub deployed_at_unix_ms: u64,
    pub pointer_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackRequest {
    pub target_digest: String,
}

#[derive(Debug, Error)]
pub enum PublishError {
    /// `Evidence` does not satisfy `ValidatedGreen` for the exact bundle being published.
    #[error("the bundle is not 🟢 Validated for its exact content")]
    NotValidated,
    /// `AclAndSecretScanner` found a secret in the bundle: nothing was uploaded.
    #[error("secret(s) detected in the bundle; nothing was uploaded: {0:?}")]
    SecretsDetected(Vec<String>),
    /// No deployment exists for the requested rollback digest.
    #[error("no deployment exists for digest {0}")]
    UnknownDigest(String),
    /// The remote Cloud Service, R2 or Cloudflare Pages call failed.
    #[error("publish service error: {0}")]
    Remote(String),
}

/// Promotes a validated build, or rolls back to a previously promoted one. Implemented
/// server-side by `cloud-service::CloudStaticPublishService`, and client-side by
/// [`PublishClient`] (what the desktop actually holds).
pub trait StaticPublishService: Send + Sync {
    fn publish(
        &self,
        request: &PublishRequest,
        evidence: &Evidence,
    ) -> Result<PublishResult, PublishError>;
    fn rollback(&self, request: &RollbackRequest) -> Result<PublishResult, PublishError>;
}

/// Gate shared by every implementation: refuses before any network call or filesystem scan if
/// `evidence` was not computed for exactly this content.
pub fn require_validated_green(
    request: &PublishRequest,
    evidence: &Evidence,
) -> Result<(), PublishError> {
    if evidence.task_id != request.task_id
        || !evidence.satisfies_validated_green(&request.tested_snapshot_hash)
    {
        return Err(PublishError::NotValidated);
    }
    Ok(())
}

/// What the desktop app uses: a plain HTTP client for the Cloud Service's publish endpoints.
/// Holds no R2/Cloudflare/KMS credential — those stay server-side (`AGENTS.md` rule 1: the model
/// proposes, the system authorizes and executes; here, the Cloud Service is the only thing that
/// executes a real deployment).
pub struct PublishClient {
    base_url: String,
    http: reqwest::blocking::Client,
    bearer_token: Option<String>,
}

impl PublishClient {
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::blocking::Client::new(),
            bearer_token: None,
        }
    }

    #[must_use]
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    fn request(&self, builder: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
        match &self.bearer_token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }
}

#[derive(Serialize)]
struct PublishBody<'a> {
    task_id: &'a str,
    checkpoint_ref: &'a str,
    tested_snapshot_hash: &'a str,
    evidence: &'a Evidence,
}

impl StaticPublishService for PublishClient {
    fn publish(
        &self,
        request: &PublishRequest,
        evidence: &Evidence,
    ) -> Result<PublishResult, PublishError> {
        require_validated_green(request, evidence)?;
        let url = format!("{}/v1/publish", self.base_url);
        let body = PublishBody {
            task_id: &request.task_id,
            checkpoint_ref: &request.checkpoint_ref,
            tested_snapshot_hash: &request.tested_snapshot_hash,
            evidence,
        };
        let response = self
            .request(self.http.post(&url))
            .json(&body)
            .send()
            .map_err(|error| PublishError::Remote(error.to_string()))?;
        parse_publish_response(response)
    }

    fn rollback(&self, request: &RollbackRequest) -> Result<PublishResult, PublishError> {
        let url = format!("{}/v1/rollback", self.base_url);
        let response = self
            .request(self.http.post(&url))
            .json(request)
            .send()
            .map_err(|error| PublishError::Remote(error.to_string()))?;
        parse_publish_response(response)
    }
}

fn parse_publish_response(
    response: reqwest::blocking::Response,
) -> Result<PublishResult, PublishError> {
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(PublishError::UnknownDigest("unknown".into()));
    }
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().unwrap_or_default();
        return Err(PublishError::Remote(format!("{status}: {text}")));
    }
    response
        .json()
        .map_err(|error| PublishError::Remote(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{
        ExecutedCommandRecord, HiloValidationState, PreviewRuntimeReport, QualityDiagnosticsSummary,
    };

    fn evidence_for(task_id: &str, hash: &str, passed: bool) -> Evidence {
        Evidence {
            schema_version: 1,
            evidence_id: "ev-1".into(),
            task_id: task_id.into(),
            tested_snapshot_hash: hash.into(),
            checkpoint_ref: "chk-1".into(),
            commands: vec![ExecutedCommandRecord {
                command: "pnpm".into(),
                args: vec!["build".into()],
                exit_code: if passed { 0 } else { 1 },
                duration_ms: 10,
                impacted_tests_only: false,
            }],
            diagnostics: QualityDiagnosticsSummary::default(),
            preview_report: PreviewRuntimeReport {
                inspected_url: "about:blank".into(),
                console_error_count: 0,
                uncaught_exception_count: 0,
                failed_network_request_count: 0,
                crashed: false,
                dom_snapshot_hash: None,
            },
            artifacts: Vec::new(),
            verdict: if passed {
                HiloValidationState::ValidatedGreen
            } else {
                HiloValidationState::FailedRed
            },
        }
    }

    #[test]
    fn publish_is_refused_when_evidence_does_not_satisfy_validated_green() {
        let request = PublishRequest {
            task_id: "task-1".into(),
            checkpoint_ref: "chk-1".into(),
            dist_root: PathBuf::from("/tmp/dist"),
            tested_snapshot_hash: "abc123".into(),
        };
        let evidence = evidence_for("task-1", "abc123", false);
        assert!(matches!(
            require_validated_green(&request, &evidence),
            Err(PublishError::NotValidated)
        ));
    }

    #[test]
    fn publish_is_refused_when_the_hash_does_not_match_exactly() {
        let request = PublishRequest {
            task_id: "task-1".into(),
            checkpoint_ref: "chk-1".into(),
            dist_root: PathBuf::from("/tmp/dist"),
            tested_snapshot_hash: "abc123".into(),
        };
        let evidence = evidence_for("task-1", "different-hash", true);
        assert!(matches!(
            require_validated_green(&request, &evidence),
            Err(PublishError::NotValidated)
        ));
    }

    #[test]
    fn publish_is_allowed_when_evidence_matches_exactly() {
        let request = PublishRequest {
            task_id: "task-1".into(),
            checkpoint_ref: "chk-1".into(),
            dist_root: PathBuf::from("/tmp/dist"),
            tested_snapshot_hash: "abc123".into(),
        };
        let evidence = evidence_for("task-1", "abc123", true);
        assert!(require_validated_green(&request, &evidence).is_ok());
    }
}
