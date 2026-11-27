//! Cloud engine: single-tenant AWS ECS Fargate (`LOT-017`).
//!
//! Unlike the OS-native engines, isolation here is not a local kernel mechanism: it is the
//! Fargate task itself (its own network namespace, no privileged mode, no Docker-in-Docker, no
//! LLM secret or deployment IAM role in its environment). What runs *locally*, under the existing
//! supervision machinery (`supervisor::run_to_completion` / `SandboxedChild`), is a thin
//! orchestrator process (`scripts/fargate_run.sh`) that uploads the worktree, starts the task,
//! waits on it with a hard `stopTimeout`, retrieves its result, and exits with its exit code.
//! Reusing `Launch::Command` this way gets wall-clock/output budgets, process-tree teardown and
//! `TmpDir` cleanup for free, without duplicating that logic for a remote execution model.
//!
//! **Deviation, undocumented until real credentials are wired up (`AGENTS.md` still applies: no
//! silent fallback):** the orchestrator shells out to the `aws` CLI rather than the Rust AWS SDKs,
//! so this engine adds no new dependency to a crate linked into every build of the desktop app.
//! `self_test` fails closed (no engine is built) when the `aws` CLI is missing or cannot reach
//! ECS with the configured identity. The task definition's own container image must run the
//! companion `scripts/fargate_agent.sh` (or an equivalent entrypoint) that downloads the uploaded
//! worktree, execs the requested program with exactly the forwarded environment, and uploads the
//! result and exit code back — that contract is documented in `scripts/fargate_agent.sh`'s header.
//! This has not been exercised against a real AWS account; see `docs/en/LOTS/LOT-017.md`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::engine::{EngineCapabilities, Launch, OsSandboxEngine, RunInfo};
use crate::error::SandboxError;
use crate::spec::{NetworkPolicy, ValidatedSpec};
use crate::OsSandboxKind;

/// Where the bundled orchestrator script lives relative to this crate, so it ships with it.
pub const DEFAULT_FARGATE_RUN_SCRIPT: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/fargate_run.sh");

/// What a single-tenant task run needs. Every field is required: a default that silently widened
/// which cluster or bucket a task lands in would be exactly the kind of implicit behavior
/// `AGENTS.md` forbids.
#[derive(Debug, Clone)]
pub struct FargateEngineConfig {
    pub region: String,
    pub cluster_arn: String,
    pub task_definition_arn: String,
    /// At least one private subnet the task's `awsvpcConfiguration` runs in.
    pub subnets: Vec<String>,
    pub security_group: String,
    /// Bucket (or R2 equivalent reached through the `aws` CLI's S3-compatible endpoint config)
    /// used only as scratch space for one run's input/output bundles, never as the publish
    /// artifact store (`StaticPublishService` owns that, server-side).
    pub scratch_bucket: String,
    /// Hard ceiling on top of the task's own budget: the orchestrator calls `StopTask` itself if
    /// the task outlives it, before the local wall-clock supervisor would kill the orchestrator.
    pub stop_timeout: Duration,
    /// Path to the orchestrator script. Overridable for tests; defaults to the bundled one.
    pub run_script: PathBuf,
    /// Path to the `aws` CLI. Overridable for tests.
    pub aws_cli: PathBuf,
}

impl FargateEngineConfig {
    #[must_use]
    pub fn new(
        region: impl Into<String>,
        cluster_arn: impl Into<String>,
        task_definition_arn: impl Into<String>,
        subnets: Vec<String>,
        security_group: impl Into<String>,
        scratch_bucket: impl Into<String>,
    ) -> Self {
        Self {
            region: region.into(),
            cluster_arn: cluster_arn.into(),
            task_definition_arn: task_definition_arn.into(),
            subnets,
            security_group: security_group.into(),
            scratch_bucket: scratch_bucket.into(),
            stop_timeout: Duration::from_secs(900),
            run_script: PathBuf::from(DEFAULT_FARGATE_RUN_SCRIPT),
            aws_cli: PathBuf::from("aws"),
        }
    }

    fn validate(&self) -> Result<(), SandboxError> {
        let blank = |field: &str| SandboxError::unavailable(format!("{field} must not be empty"));
        if self.region.is_empty() {
            return Err(blank("region"));
        }
        if self.cluster_arn.is_empty() {
            return Err(blank("cluster_arn"));
        }
        if self.task_definition_arn.is_empty() {
            return Err(blank("task_definition_arn"));
        }
        if self.subnets.is_empty() || self.subnets.iter().any(String::is_empty) {
            return Err(blank("subnets"));
        }
        if self.security_group.is_empty() {
            return Err(blank("security_group"));
        }
        if self.scratch_bucket.is_empty() {
            return Err(blank("scratch_bucket"));
        }
        if self.stop_timeout.is_zero() {
            return Err(blank("stop_timeout"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FargateEngine {
    config: FargateEngineConfig,
}

impl FargateEngine {
    /// Builds the engine without probing AWS. Prefer `self_test` (run once by `OsNativeSandbox::new`)
    /// before trusting it to accept tasks.
    pub fn new(config: FargateEngineConfig) -> Result<Self, SandboxError> {
        config.validate()?;
        if !config.run_script.is_file() {
            return Err(SandboxError::unavailable(format!(
                "the Fargate orchestrator script {} is missing",
                config.run_script.display()
            )));
        }
        Ok(Self { config })
    }
}

impl OsSandboxEngine for FargateEngine {
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            name: "aws-ecs-fargate",
            kind: OsSandboxKind::CloudAwsFargate,
            // Enforced by the task definition's own resource requirements, set from these limits
            // by the orchestrator (see `scripts/fargate_run.sh`).
            enforces_memory_limit: true,
            // ECS does not cap process count inside a container.
            enforces_process_limit: false,
            enforces_cpu_limit: true,
            // The remote task is not reachable at any loopback address of this host.
            supports_local_proxy: false,
        }
    }

    /// Fails closed when the `aws` CLI is missing or cannot describe the configured task
    /// definition with the ambient identity: nothing is accepted without a working control plane.
    fn self_test(&self) -> Result<(), SandboxError> {
        let output = Command::new(&self.config.aws_cli)
            .args(["ecs", "describe-task-definition"])
            .arg("--task-definition")
            .arg(&self.config.task_definition_arn)
            .args(["--region", &self.config.region])
            .output()
            .map_err(|error| {
                SandboxError::unavailable(format!(
                    "the aws CLI ({}) could not be run: {error}",
                    self.config.aws_cli.display()
                ))
            })?;
        if !output.status.success() {
            return Err(SandboxError::unavailable(format!(
                "aws ecs describe-task-definition failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(())
    }

    fn prepare(&self, spec: &ValidatedSpec) -> Result<Launch, SandboxError> {
        if matches!(spec.network, NetworkPolicy::LocalProxyOnly(_)) {
            return Err(SandboxError::NetworkPolicyNotEnforceable {
                engine: "aws-ecs-fargate",
            });
        }
        // The *remote container's* environment, never this local orchestrator's: written to a
        // file inside the run's private tmp dir (purged with it) rather than passed as CLI
        // arguments, which would otherwise leak into this host's process listing.
        let env_file = spec.tmp_dir.join("container.env");
        write_env_file(&env_file, spec)?;
        let args_file = spec.tmp_dir.join("container.args");
        fs::write(
            &args_file,
            spec.args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .map_err(SandboxError::Io)?;

        let mut command = Command::new(&self.config.run_script);
        command
            .arg("--aws-cli")
            .arg(&self.config.aws_cli)
            .arg("--region")
            .arg(&self.config.region)
            .arg("--cluster")
            .arg(&self.config.cluster_arn)
            .arg("--task-definition")
            .arg(&self.config.task_definition_arn)
            .arg("--subnets")
            .arg(self.config.subnets.join(","))
            .arg("--security-group")
            .arg(&self.config.security_group)
            .arg("--bucket")
            .arg(&self.config.scratch_bucket)
            .arg("--run-id")
            .arg(&spec.run_id)
            .arg("--worktree")
            .arg(&spec.worktree_root)
            .arg("--cwd")
            .arg(
                spec.cwd
                    .strip_prefix(&spec.worktree_root)
                    .unwrap_or(Path::new(".")),
            )
            .arg("--program")
            .arg(&spec.program)
            .arg("--args-file")
            .arg(&args_file)
            .arg("--env-file")
            .arg(&env_file)
            .arg("--stop-timeout-secs")
            .arg(self.config.stop_timeout.as_secs().to_string());
        if let Some(memory) = spec.limits.max_memory_bytes {
            command.arg("--memory-bytes").arg(memory.to_string());
        }
        if let Some(cpu) = spec.limits.max_cpu_time {
            command.arg("--cpu-seconds").arg(cpu.as_secs().to_string());
        }
        // Deliberately not `env_clear()`: this process is the control plane (it needs the
        // ambient AWS credential chain), not the sandboxed program. The sandboxed program's
        // environment travels only through `--env-file`, read by `fargate_agent.sh` inside the
        // task, never inherited here.
        Ok(Launch::Command(command))
    }

    fn collect_denials(&self, _run: &RunInfo, _audit: std::sync::Arc<dyn crate::audit::AuditSink>) {
        // ECS does not report kernel-style denials after the fact; the orchestrator's own stderr
        // (captured as ordinary command output) carries any AWS-side refusal.
    }
}

fn write_env_file(path: &Path, spec: &ValidatedSpec) -> Result<(), SandboxError> {
    let mut text = String::new();
    for (key, value) in spec.environment() {
        let key = key.to_string_lossy();
        let value = value.to_string_lossy();
        if key.contains('\n') || value.contains('\n') {
            return Err(SandboxError::InvalidSpec(
                "an environment entry contains a newline".into(),
            ));
        }
        text.push_str(&key);
        text.push('=');
        text.push_str(&value);
        text.push('\n');
    }
    fs::write(path, text).map_err(SandboxError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config(script: &Path, aws_cli: &Path) -> FargateEngineConfig {
        let mut config = FargateEngineConfig::new(
            "eu-west-1",
            "arn:aws:ecs:eu-west-1:000000000000:cluster/hilo",
            "arn:aws:ecs:eu-west-1:000000000000:task-definition/hilo-build:1",
            vec!["subnet-aaaa".into()],
            "sg-aaaa",
            "hilo-fargate-scratch",
        );
        config.run_script = script.to_path_buf();
        config.aws_cli = aws_cli.to_path_buf();
        config
    }

    #[test]
    fn construction_is_refused_when_the_run_script_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let config = base_config(&dir.path().join("missing.sh"), Path::new("aws"));
        assert!(FargateEngine::new(config).is_err());
    }

    #[test]
    fn construction_is_refused_on_a_blank_required_field() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("run.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let mut config = base_config(&script, Path::new("aws"));
        config.cluster_arn.clear();
        assert!(FargateEngine::new(config).is_err());
    }

    #[test]
    fn self_test_fails_closed_when_the_aws_cli_is_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("run.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        // A path that cannot be executed: the self-test must fail, not fall back to anything.
        let config = base_config(&script, &dir.path().join("no-such-aws-cli"));
        let engine = FargateEngine::new(config).expect("config shape is valid");
        assert!(engine.self_test().is_err());
    }

    #[test]
    fn local_proxy_network_policy_is_refused() {
        use crate::spec::SandboxSpec;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("run.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let config = base_config(&script, Path::new("aws"));
        let engine = FargateEngine::new(config).unwrap();
        let worktree = tempfile::tempdir().unwrap();
        let root = worktree.path().canonicalize().unwrap();
        let mut spec = SandboxSpec::new(&root, "/usr/bin/true");
        spec.network = NetworkPolicy::LocalProxyOnly("127.0.0.1:9".parse().unwrap());
        let validated = spec.validate().expect("spec is otherwise valid");
        assert!(matches!(
            engine.prepare(&validated),
            Err(SandboxError::NetworkPolicyNotEnforceable { .. })
        ));
    }
}
