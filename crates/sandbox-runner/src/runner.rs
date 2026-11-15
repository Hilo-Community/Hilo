//! `SandboxRunner`: the one way to run something for an agent.
//!
//! Fail closed (`AGENTS.md` rule 5): a runner exists only if its engine passed the self-test;
//! a request that cannot be isolated as asked is refused; nothing ever runs on the host without
//! the sandbox. The caller (the `PolicyEngine`, `LOT-013`) decides *whether* a command may run,
//! including the workspace trust; this layer decides *how* it is isolated.

use std::sync::Arc;

use crate::audit::{AuditSink, SecurityAuditEvent, SecurityEventKind};
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
use crate::detect_native_os_sandbox;
use crate::engine::{EngineCapabilities, Launch, OsSandboxEngine, RunInfo};
use crate::error::SandboxError;
use crate::path_guard::PathGuardError;
use crate::spec::{NetworkPolicy, ResourceLimits, SandboxSpec};
use crate::supervisor::{
    new_run_id, run_to_completion, FinishHook, RunOutput, SandboxedChild, Termination, TmpDir,
};

/// Runs commands and long-lived processes in isolation.
pub trait SandboxRunner: Send + Sync {
    fn capabilities(&self) -> EngineCapabilities;

    /// Runs a command to completion with its output bounded. The process tree is killed when
    /// the wall-clock or output budget runs out, whatever the command does.
    fn run(&self, spec: &SandboxSpec) -> Result<RunOutput, SandboxError>;

    /// Starts a long-lived process (an agent) with piped stdin, stdout and stderr. The wall-clock
    /// budget is its lifetime; the caller owns the pipes and their volume. Dropping the child
    /// kills its whole tree.
    fn spawn(&self, spec: &SandboxSpec) -> Result<SandboxedChild, SandboxError>;
}

/// A runner over the isolation mechanism of the current OS.
pub struct OsNativeSandbox<E: OsSandboxEngine> {
    engine: Arc<E>,
    audit: Arc<dyn AuditSink>,
}

impl<E: OsSandboxEngine> OsNativeSandbox<E> {
    /// Runs the engine's self-test. If it fails, no runner is built: no task can be accepted.
    pub fn new(engine: E, audit: Arc<dyn AuditSink>) -> Result<Self, SandboxError> {
        if let Err(error) = engine.self_test() {
            let error = match error {
                unavailable @ SandboxError::SandboxUnavailable { .. } => unavailable,
                other => SandboxError::unavailable(format!("the self-test failed: {other}")),
            };
            audit.record(SecurityAuditEvent::new(
                SecurityEventKind::SandboxUnavailable,
                error.to_string(),
            ));
            return Err(error);
        }
        Ok(Self {
            engine: Arc::new(engine),
            audit,
        })
    }

    /// Records the refusal, then hands the error back.
    fn refuse(&self, error: SandboxError) -> SandboxError {
        let kind = match &error {
            SandboxError::Path(
                PathGuardError::Escapes { .. }
                | PathGuardError::ParentDirectory(_)
                | PathGuardError::DanglingSymlink(_),
            ) => Some(SecurityEventKind::PathEscapeBlocked),
            SandboxError::LimitNotEnforceable { .. }
            | SandboxError::NetworkPolicyNotEnforceable { .. } => {
                Some(SecurityEventKind::PolicyNotEnforceable)
            }
            SandboxError::SandboxUnavailable { .. } => Some(SecurityEventKind::SandboxUnavailable),
            _ => None,
        };
        if let Some(kind) = kind {
            self.audit
                .record(SecurityAuditEvent::new(kind, error.to_string()));
        }
        error
    }

    fn check_capabilities(&self, spec: &SandboxSpec) -> Result<(), SandboxError> {
        let caps = self.engine.capabilities();
        let refuse_limit = |limit| SandboxError::LimitNotEnforceable {
            engine: caps.name,
            limit,
        };
        if spec.limits.max_memory_bytes.is_some() && !caps.enforces_memory_limit {
            return Err(refuse_limit("memory"));
        }
        if spec.limits.max_processes.is_some() && !caps.enforces_process_limit {
            return Err(refuse_limit("process count"));
        }
        if spec.limits.max_cpu_time.is_some() && !caps.enforces_cpu_limit {
            return Err(refuse_limit("CPU time"));
        }
        if matches!(spec.network, NetworkPolicy::LocalProxyOnly(_)) && !caps.supports_local_proxy {
            return Err(SandboxError::NetworkPolicyNotEnforceable { engine: caps.name });
        }
        Ok(())
    }

    /// Everything needed to start the run: the isolated command, its private temporary
    /// directory, its limits, and what to do once it is over.
    fn prepare(&self, spec: &SandboxSpec) -> Result<Prepared, SandboxError> {
        // A malformed request is reported as such before asking what the engine can enforce.
        let mut validated = spec.validate()?;
        self.check_capabilities(spec)?;
        let tmp = TmpDir::create(&validated.worktree_root)?;
        validated.tmp_dir = tmp.path().to_path_buf();
        validated.run_id = new_run_id();
        let launch = self.engine.prepare(&validated)?;
        let info = RunInfo {
            run_id: validated.run_id.clone(),
            started: std::time::SystemTime::now(),
            program: validated.program.clone(),
        };
        let (engine, audit) = (Arc::clone(&self.engine), Arc::clone(&self.audit));
        let on_finish = FinishHook::new(move || engine.collect_denials(&info, audit));
        Ok(Prepared {
            launch,
            tmp,
            limits: validated.limits,
            on_finish,
        })
    }
}

struct Prepared {
    launch: Launch,
    tmp: TmpDir,
    limits: ResourceLimits,
    on_finish: FinishHook,
}

impl<E: OsSandboxEngine> SandboxRunner for OsNativeSandbox<E> {
    fn capabilities(&self) -> EngineCapabilities {
        self.engine.capabilities()
    }

    fn run(&self, spec: &SandboxSpec) -> Result<RunOutput, SandboxError> {
        let Prepared {
            launch,
            tmp,
            limits,
            on_finish,
        } = self.prepare(spec).map_err(|error| self.refuse(error))?;
        let mut child = SandboxedChild::spawn_prepared(launch, tmp, None, false, Some(on_finish))?;
        let output = run_to_completion(&mut child, &limits)?;
        let program = spec.program.display();
        match output.termination {
            Termination::WallTimeout => self.audit.record(SecurityAuditEvent::new(
                SecurityEventKind::WallTimeoutKilled,
                format!("{program} exceeded {:?}", limits.wall_timeout),
            )),
            Termination::OutputLimit => self.audit.record(SecurityAuditEvent::new(
                SecurityEventKind::OutputLimitKilled,
                format!(
                    "{program} exceeded {} bytes of output on a stream",
                    limits.max_output_bytes
                ),
            )),
            Termination::Exited { .. } => {}
        }
        Ok(output)
    }

    fn spawn(&self, spec: &SandboxSpec) -> Result<SandboxedChild, SandboxError> {
        let Prepared {
            launch,
            tmp,
            limits,
            on_finish,
        } = self.prepare(spec).map_err(|error| self.refuse(error))?;
        SandboxedChild::spawn_prepared(
            launch,
            tmp,
            Some(limits.wall_timeout),
            true,
            Some(on_finish),
        )
    }
}

/// The runner of the current OS, or `SandboxUnavailable`. On macOS this is Seatbelt (see
/// `SeatbeltEngine`). There is no unisolated fallback on any system.
#[cfg(target_os = "macos")]
pub fn native_sandbox(audit: Arc<dyn AuditSink>) -> Result<Box<dyn SandboxRunner>, SandboxError> {
    let engine = crate::seatbelt::SeatbeltEngine::for_current_user().inspect_err(|error| {
        audit.record(SecurityAuditEvent::new(
            SecurityEventKind::SandboxUnavailable,
            error.to_string(),
        ));
    })?;
    Ok(Box::new(OsNativeSandbox::new(engine, audit)?))
}

/// The runner of the current OS, or `SandboxUnavailable`. On Linux this is bubblewrap (see
/// `BubblewrapEngine` for what the system must provide).
#[cfg(target_os = "linux")]
pub fn native_sandbox(audit: Arc<dyn AuditSink>) -> Result<Box<dyn SandboxRunner>, SandboxError> {
    let engine = crate::bubblewrap::BubblewrapEngine::for_current_user().inspect_err(|error| {
        audit.record(SecurityAuditEvent::new(
            SecurityEventKind::SandboxUnavailable,
            error.to_string(),
        ));
    })?;
    Ok(Box::new(OsNativeSandbox::new(engine, audit)?))
}

/// The runner of the current OS, or `SandboxUnavailable`. On Windows this is an AppContainer
/// inside a Job Object (see `AppContainerEngine`).
#[cfg(target_os = "windows")]
pub fn native_sandbox(audit: Arc<dyn AuditSink>) -> Result<Box<dyn SandboxRunner>, SandboxError> {
    let engine = crate::windows::AppContainerEngine::for_current_user().inspect_err(|error| {
        audit.record(SecurityAuditEvent::new(
            SecurityEventKind::SandboxUnavailable,
            error.to_string(),
        ));
    })?;
    Ok(Box::new(OsNativeSandbox::new(engine, audit)?))
}

/// The runner of the current OS, or `SandboxUnavailable` (no engine exists for this OS).
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn native_sandbox(audit: Arc<dyn AuditSink>) -> Result<Box<dyn SandboxRunner>, SandboxError> {
    let error = SandboxError::unavailable(format!(
        "the {:?} engine is not implemented yet",
        detect_native_os_sandbox()
    ));
    audit.record(SecurityAuditEvent::new(
        SecurityEventKind::SandboxUnavailable,
        error.to_string(),
    ));
    Err(error)
}
