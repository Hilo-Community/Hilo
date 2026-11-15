//! The only command executor delegates process creation to the fail-closed native sandbox.

use super::{ExecutionError, ExecutionReport, ToolExecutor};
use crate::{budget::BudgetUsage, IntentOutcome};
use host_core::{TaskSpec, ToolArgValue, ToolCall, ToolCapability};
use sandbox_runner::{
    NetworkPolicy, PathGuard, ResourceLimits, SandboxError, SandboxRunner, SandboxSpec,
};
use std::path::PathBuf;

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;

pub struct SandboxToolExecutor {
    task_id: String,
    root: PathBuf,
    runner: Box<dyn SandboxRunner>,
    limits: ResourceLimits,
    proxy: Option<std::net::SocketAddr>,
}

impl SandboxToolExecutor {
    /// Root and resource ceilings come from the Host, never tool arguments.
    pub fn new(
        task_id: String,
        root: PathBuf,
        runner: Box<dyn SandboxRunner>,
        limits: ResourceLimits,
    ) -> Self {
        Self {
            task_id,
            root,
            runner,
            limits,
            proxy: None,
        }
    }

    pub fn with_proxy(mut self, proxy: std::net::SocketAddr) -> Self {
        self.proxy = Some(proxy);
        self
    }

    fn request(&self, spec: &TaskSpec, call: &ToolCall) -> Result<SandboxSpec, ExecutionError> {
        let invalid = |message: &str| ExecutionError::Invalid(message.into());
        if spec.task_id != self.task_id
            || call.tool_name != "run_sandbox_command"
            || !call
                .required_capabilities
                .contains(&ToolCapability::RunSandboxCommand)
        {
            return Err(invalid(
                "executor is bound to one task and sandbox commands",
            ));
        }
        let text = |key| match call.validated_args.get(key) {
            Some(ToolArgValue::Text(value)) => Ok(value.as_str()),
            _ => Err(invalid("missing text argument")),
        };
        let program = PathBuf::from(text("program")?);
        if !program.is_absolute() {
            return Err(invalid("program must be absolute"));
        }
        let cwd = text("cwd")?;
        if crate::policy::paths::normalize(cwd).is_none() {
            return Err(invalid("cwd must be a relative worktree path"));
        }
        let cwd = PathGuard::verify_within_worktree(&self.root, &self.root.join(cwd))
            .map_err(sandbox_runner::SandboxError::from)?;
        let mut request = SandboxSpec::new(self.root.clone(), program);
        request.cwd = cwd;
        request.args = match call.validated_args.get("args") {
            Some(ToolArgValue::StringList(args)) => args.iter().map(Into::into).collect(),
            _ => return Err(invalid("missing argument list")),
        };
        request.limits = self.limits;
        if spec.policy.network_allowed
            && call
                .required_capabilities
                .contains(&ToolCapability::NetworkEgress)
        {
            if let Some(proxy) = self.proxy {
                request.network = NetworkPolicy::LocalProxyOnly(proxy);
            }
        }
        Ok(request)
    }
}

/// Errors the runner reports before any process exists are `NotStarted`: the broker can close
/// the action without a human. A failed spawn also qualifies (on Windows the process is created
/// suspended and killed before it ever runs). Anything else (I/O while running, the watchdog)
/// may come after the program started, so it stays an unknown outcome.
fn classify(error: SandboxError) -> ExecutionError {
    match error {
        SandboxError::SandboxUnavailable { .. }
        | SandboxError::InvalidSpec(_)
        | SandboxError::Path(_)
        | SandboxError::LimitNotEnforceable { .. }
        | SandboxError::NetworkPolicyNotEnforceable { .. }
        | SandboxError::Spawn(_) => ExecutionError::NotStarted(error.to_string()),
        other => ExecutionError::Sandbox(other),
    }
}

impl ToolExecutor for SandboxToolExecutor {
    fn estimate(&self, spec: &TaskSpec, call: &ToolCall) -> Result<BudgetUsage, ExecutionError> {
        let request = self.request(spec, call)?;
        let cpu = request
            .limits
            .max_cpu_time
            .ok_or_else(|| ExecutionError::Invalid("Host must set a CPU ceiling".into()))?;
        // Native rlimits round up to seconds; reserve that enforced allowance too.
        let cpu_ms = cpu
            .as_secs()
            .checked_add(u64::from(cpu.subsec_nanos() != 0))
            .and_then(|seconds| seconds.checked_mul(1_000))
            .filter(|ms| *ms > 0)
            .ok_or_else(|| ExecutionError::Invalid("CPU ceiling out of range".into()))?;
        Ok(BudgetUsage {
            cpu_ms,
            tool_calls: 1,
            ..BudgetUsage::default()
        })
    }

    fn execute(
        &self,
        spec: &TaskSpec,
        call: &ToolCall,
        reserved: BudgetUsage,
    ) -> Result<ExecutionReport, ExecutionError> {
        let request = self.request(spec, call)?;
        let estimate = self.estimate(spec, call)?;
        if !estimate.fits(reserved) {
            return Err(ExecutionError::Invalid(
                "reservation below configured limits".into(),
            ));
        }
        let output = self.runner.run(&request).map_err(classify)?;
        let succeeded = output.termination.success() && !output.incomplete;
        // The runner exposes elapsed wall time, not CPU telemetry: conservatively charge the
        // full configured CPU allowance rather than pretending wall time is CPU consumption.
        Ok(ExecutionReport {
            outcome: IntentOutcome {
                succeeded,
                summary: format!(
                    "{:?}\n{}{}",
                    output.termination,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
            },
            actual: estimate,
            failure_signature: (!succeeded).then(|| format!("sandbox:{:?}", output.termination)),
        })
    }
}
