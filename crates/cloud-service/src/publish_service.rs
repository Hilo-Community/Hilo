//! `CloudStaticPublishService` (`LOT-017`): the server-side implementation of
//! `host_core::StaticPublishService`. Gates on `Evidence`, scans `dist/` with
//! `AclAndSecretScanner` before anything is uploaded, then promotes a content-addressed bundle
//! to Cloudflare R2/Pages with 1-click digest rollback.
//!
//! `ArtifactStore` (the R2 upload) and `PagesPublisher` (the Cloudflare Pages pointer flip) are
//! traits so tests exercise the full gate → scan → digest → upload → flip → record pipeline
//! against in-memory fakes, with no real network call — see `tests/lot017_publish_rollback.rs`.

use std::collections::BTreeMap;
use std::path::Path;

use context_engine::AclAndSecretScanner;
use host_core::contracts::Evidence;
use host_core::publish::{require_validated_green, PublishError, PublishRequest, PublishResult, RollbackRequest};

/// Where a bundle's bytes go once it passes the gate. Implemented against Cloudflare R2
/// (S3-compatible) in production; an in-memory fake in tests.
pub trait ArtifactStore: Send + Sync {
    /// Uploads under a content-addressed key; must be idempotent (publishing the same digest
    /// twice is not an error).
    fn put_bundle(&self, digest: &str, files: &BTreeMap<String, Vec<u8>>) -> Result<String, PublishError>;
}

/// Flips the live Cloudflare Pages deployment pointer. Implemented against the Cloudflare API
/// (through `egress::EgressAllowlist`) in production; an in-memory fake in tests.
pub trait PagesPublisher: Send + Sync {
    /// Returns the public URL now serving `object_key`.
    fn set_active(&self, object_key: &str) -> Result<String, PublishError>;
}

/// What `CloudStaticPublishService` persists per deployment. The real implementation keeps this
/// in the `publish_deployments` table (`src/db.rs`); tests use an in-memory version.
pub trait DeploymentLedger: Send + Sync {
    /// Records a newly promoted digest as `active`, superseding whichever one was active before.
    fn record_active(&self, tenant_id: &str, task_id: &str, digest: &str, object_key: &str);
    /// The object key stored for a digest, if this tenant ever published it.
    fn object_key_for(&self, tenant_id: &str, digest: &str) -> Option<String>;
    /// Marks `digest` active again (a rollback), returning its object key.
    fn reactivate(&self, tenant_id: &str, digest: &str) -> Option<String>;
}

pub struct CloudStaticPublishService {
    tenant_id: String,
    artifacts: Box<dyn ArtifactStore>,
    pages: Box<dyn PagesPublisher>,
    ledger: Box<dyn DeploymentLedger>,
}

impl CloudStaticPublishService {
    #[must_use]
    pub fn new(
        tenant_id: impl Into<String>,
        artifacts: Box<dyn ArtifactStore>,
        pages: Box<dyn PagesPublisher>,
        ledger: Box<dyn DeploymentLedger>,
    ) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            artifacts,
            pages,
            ledger,
        }
    }

    /// Reads every file under `dist_root`, refusing (without touching the network) if any fails
    /// `AclAndSecretScanner::is_clean_of_secrets` — the LOT's "block a bundle containing a
    /// secret" blocking scenario.
    fn read_and_scan_bundle(dist_root: &Path) -> Result<BTreeMap<String, Vec<u8>>, PublishError> {
        let scanner = AclAndSecretScanner::from_workspace_root(dist_root);
        let mut files = BTreeMap::new();
        let mut secret_paths = Vec::new();
        for entry in walk_files(dist_root).map_err(|e| PublishError::Remote(e.to_string()))? {
            let relative = entry
                .strip_prefix(dist_root)
                .unwrap_or(&entry)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&entry).map_err(|e| PublishError::Remote(e.to_string()))?;
            if let Ok(text) = String::from_utf8(bytes.clone()) {
                if !scanner.is_clean_of_secrets(&relative, &text) {
                    secret_paths.push(relative.clone());
                }
            }
            files.insert(relative, bytes);
        }
        if !secret_paths.is_empty() {
            return Err(PublishError::SecretsDetected(secret_paths));
        }
        Ok(files)
    }
}

fn walk_files(root: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// BLAKE3 digest of a bundle, computed deterministically over its sorted `(path, bytes)` pairs so
/// the exact same content always yields the exact same digest regardless of read order.
#[must_use]
pub fn bundle_digest(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut hasher = blake3::Hasher::new();
    for (path, bytes) in files {
        hasher.update(path.as_bytes());
        hasher.update(&[0]);
        hasher.update(bytes);
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

impl host_core::publish::StaticPublishService for CloudStaticPublishService {
    fn publish(
        &self,
        request: &PublishRequest,
        evidence: &Evidence,
    ) -> Result<PublishResult, PublishError> {
        require_validated_green(request, evidence)?;
        let files = Self::read_and_scan_bundle(&request.dist_root)?;
        let digest = bundle_digest(&files);
        let object_key = self.artifacts.put_bundle(&digest, &files)?;
        let pointer_url = self.pages.set_active(&object_key)?;
        self.ledger
            .record_active(&self.tenant_id, &request.task_id, &digest, &object_key);
        Ok(PublishResult {
            digest_blake3: digest,
            deployed_at_unix_ms: now_unix_ms(),
            pointer_url,
        })
    }

    fn rollback(&self, request: &RollbackRequest) -> Result<PublishResult, PublishError> {
        let object_key = self
            .ledger
            .reactivate(&self.tenant_id, &request.target_digest)
            .ok_or_else(|| PublishError::UnknownDigest(request.target_digest.clone()))?;
        let pointer_url = self.pages.set_active(&object_key)?;
        Ok(PublishResult {
            digest_blake3: request.target_digest.clone(),
            deployed_at_unix_ms: now_unix_ms(),
            pointer_url,
        })
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// In-memory fakes for tests (`tests/lot017_publish_rollback.rs`): no real R2/Cloudflare call.
pub mod fakes {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct FakeArtifactStore {
        pub uploaded: Mutex<BTreeMap<String, BTreeMap<String, Vec<u8>>>>,
    }

    impl ArtifactStore for FakeArtifactStore {
        fn put_bundle(
            &self,
            digest: &str,
            files: &BTreeMap<String, Vec<u8>>,
        ) -> Result<String, PublishError> {
            self.uploaded
                .lock()
                .unwrap()
                .insert(digest.to_string(), files.clone());
            Ok(format!("bundles/{digest}.tar"))
        }
    }

    #[derive(Default)]
    pub struct FakePagesPublisher {
        pub active_object_key: Mutex<Option<String>>,
    }

    impl PagesPublisher for FakePagesPublisher {
        fn set_active(&self, object_key: &str) -> Result<String, PublishError> {
            *self.active_object_key.lock().unwrap() = Some(object_key.to_string());
            Ok(format!("https://fake-pages.example/{object_key}"))
        }
    }

    #[derive(Default)]
    pub struct FakeDeploymentLedger {
        // digest -> object_key, insertion order preserved for test assertions.
        pub deployments: Mutex<Vec<(String, String)>>,
    }

    impl DeploymentLedger for FakeDeploymentLedger {
        fn record_active(&self, _tenant_id: &str, _task_id: &str, digest: &str, object_key: &str) {
            self.deployments
                .lock()
                .unwrap()
                .push((digest.to_string(), object_key.to_string()));
        }

        fn object_key_for(&self, _tenant_id: &str, digest: &str) -> Option<String> {
            self.deployments
                .lock()
                .unwrap()
                .iter()
                .find(|(d, _)| d == digest)
                .map(|(_, key)| key.clone())
        }

        fn reactivate(&self, tenant_id: &str, digest: &str) -> Option<String> {
            self.object_key_for(tenant_id, digest)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fakes::*;
    use super::*;
    use host_core::contracts::{
        ExecutedCommandRecord, HiloValidationState, PreviewRuntimeReport, QualityDiagnosticsSummary,
    };
    use host_core::publish::StaticPublishService;
    use std::sync::Arc;

    fn write_dist(root: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }
    }

    fn validated_evidence(task_id: &str, hash: &str) -> Evidence {
        Evidence {
            schema_version: 1,
            evidence_id: "ev".into(),
            task_id: task_id.into(),
            tested_snapshot_hash: hash.into(),
            checkpoint_ref: "chk".into(),
            commands: vec![ExecutedCommandRecord {
                command: "pnpm".into(),
                args: vec!["build".into()],
                exit_code: 0,
                duration_ms: 1,
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
            verdict: HiloValidationState::ValidatedGreen,
        }
    }

    fn service(
        artifacts: Arc<FakeArtifactStore>,
        pages: Arc<FakePagesPublisher>,
        ledger: Arc<FakeDeploymentLedger>,
    ) -> CloudStaticPublishService {
        struct ArcStore<T>(Arc<T>);
        impl ArtifactStore for ArcStore<FakeArtifactStore> {
            fn put_bundle(&self, d: &str, f: &BTreeMap<String, Vec<u8>>) -> Result<String, PublishError> {
                self.0.put_bundle(d, f)
            }
        }
        impl PagesPublisher for ArcStore<FakePagesPublisher> {
            fn set_active(&self, k: &str) -> Result<String, PublishError> {
                self.0.set_active(k)
            }
        }
        impl DeploymentLedger for ArcStore<FakeDeploymentLedger> {
            fn record_active(&self, t: &str, task: &str, d: &str, k: &str) {
                self.0.record_active(t, task, d, k)
            }
            fn object_key_for(&self, t: &str, d: &str) -> Option<String> {
                self.0.object_key_for(t, d)
            }
            fn reactivate(&self, t: &str, d: &str) -> Option<String> {
                self.0.reactivate(t, d)
            }
        }
        CloudStaticPublishService::new(
            "tenant-1",
            Box::new(ArcStore(artifacts)),
            Box::new(ArcStore(pages)),
            Box::new(ArcStore(ledger)),
        )
    }

    #[test]
    fn publish_then_publish_then_blocked_secret_then_rollback() {
        let dir1 = tempfile::tempdir().unwrap();
        write_dist(dir1.path(), &[("index.html", "<h1>v1</h1>")]);
        let hash1 = blake3::hash(b"v1-task-content").to_hex().to_string();

        let artifacts = Arc::new(FakeArtifactStore::default());
        let pages = Arc::new(FakePagesPublisher::default());
        let ledger = Arc::new(FakeDeploymentLedger::default());
        let svc = service(artifacts.clone(), pages.clone(), ledger.clone());

        let req1 = PublishRequest {
            task_id: "task-1".into(),
            checkpoint_ref: "chk-1".into(),
            dist_root: dir1.path().to_path_buf(),
            tested_snapshot_hash: hash1.clone(),
        };
        let evidence1 = validated_evidence("task-1", &hash1);
        let result1 = svc.publish(&req1, &evidence1).expect("v1 publishes");
        let digest1 = result1.digest_blake3.clone();
        assert!(artifacts.uploaded.lock().unwrap().contains_key(&digest1));

        // V2: different content.
        let dir2 = tempfile::tempdir().unwrap();
        write_dist(dir2.path(), &[("index.html", "<h1>v2</h1>")]);
        let hash2 = blake3::hash(b"v2-task-content").to_hex().to_string();
        let req2 = PublishRequest {
            task_id: "task-1".into(),
            checkpoint_ref: "chk-2".into(),
            dist_root: dir2.path().to_path_buf(),
            tested_snapshot_hash: hash2.clone(),
        };
        let evidence2 = validated_evidence("task-1", &hash2);
        let result2 = svc.publish(&req2, &evidence2).expect("v2 publishes");
        assert_ne!(result1.digest_blake3, result2.digest_blake3);
        assert_eq!(
            *pages.active_object_key.lock().unwrap(),
            Some(format!("bundles/{}.tar", result2.digest_blake3))
        );

        // V3 contains a planted secret: blocked, nothing uploaded for it.
        let dir3 = tempfile::tempdir().unwrap();
        write_dist(
            dir3.path(),
            &[("config.js", "const KEY='AKIAABCDEFGHIJKLMNOP';")],
        );
        let hash3 = blake3::hash(b"v3-task-content").to_hex().to_string();
        let req3 = PublishRequest {
            task_id: "task-1".into(),
            checkpoint_ref: "chk-3".into(),
            dist_root: dir3.path().to_path_buf(),
            tested_snapshot_hash: hash3.clone(),
        };
        let evidence3 = validated_evidence("task-1", &hash3);
        let before_count = artifacts.uploaded.lock().unwrap().len();
        let blocked = svc.publish(&req3, &evidence3);
        assert!(matches!(blocked, Err(PublishError::SecretsDetected(_))));
        assert_eq!(artifacts.uploaded.lock().unwrap().len(), before_count, "nothing new was uploaded");

        // 1-click rollback to V1.
        let rollback = svc
            .rollback(&RollbackRequest {
                target_digest: digest1.clone(),
            })
            .expect("rollback to v1 succeeds");
        assert_eq!(rollback.digest_blake3, digest1);
        assert_eq!(
            *pages.active_object_key.lock().unwrap(),
            Some(format!("bundles/{digest1}.tar"))
        );
    }

    #[test]
    fn rollback_to_an_unknown_digest_is_refused() {
        let svc = service(
            Arc::new(FakeArtifactStore::default()),
            Arc::new(FakePagesPublisher::default()),
            Arc::new(FakeDeploymentLedger::default()),
        );
        let result = svc.rollback(&RollbackRequest {
            target_digest: "never-published".into(),
        });
        assert!(matches!(result, Err(PublishError::UnknownDigest(_))));
    }
}
