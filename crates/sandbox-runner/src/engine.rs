//! The per-OS part of the sandbox: how a request is turned into an isolated process.

use std::process::Command;
use std::sync::Arc;
use std::time::SystemTime;

use crate::audit::AuditSink;

use crate::error::SandboxError;
use crate::spec::ValidatedSpec;
use crate::OsSandboxKind;

/// What an engine can enforce. Wall-clock and output budgets are enforced by the runner for
/// every engine; the rest depends on the OS mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineCapabilities {
    pub name: &'static str,
    pub kind: OsSandboxKind,
    pub enforces_memory_limit: bool,
    pub enforces_process_limit: bool,
    pub enforces_cpu_limit: bool,
    /// Can allow exactly one loopback address (`NetworkPolicy::LocalProxyOnly`).
    pub supports_local_proxy: bool,
}

/// Implemented once per OS (`Seatbelt`, `Landlock` + `bubblewrap`, `AppContainer`).
///
/// An engine builds the isolated command; it never runs anything itself and never falls back to
/// an unisolated command: when it cannot isolate, it returns an error.
pub trait OsSandboxEngine: Send + Sync + 'static {
    fn capabilities(&self) -> EngineCapabilities;

    /// Proves, on this machine and now, that file-system and network isolation are effective
    /// (for example by running a probe that must be refused). A failure keeps the runner from
    /// being built: no task is accepted.
    fn self_test(&self) -> Result<(), SandboxError>;

    /// How to start `spec` inside the sandbox, with `spec.environment()` as its whole environment
    /// and `spec.cwd` as its working directory: a command that runs the sandbox tool (Seatbelt,
    /// bubblewrap), or an AppContainer launch (Windows). The runner sets stdio and supervision.
    fn prepare(&self, spec: &ValidatedSpec) -> Result<Launch, SandboxError>;

    /// Called once when the process tree of a run has ended. An engine whose OS reports refusals
    /// after the fact (a log, a trace) collects them here and records them in `audit`. It must
    /// not block: start a thread for slow work. The default reports nothing.
    fn collect_denials(&self, _run: &RunInfo, _audit: Arc<dyn AuditSink>) {}
}

/// How a prepared run is started.
#[derive(Debug)]
pub enum Launch {
    /// A command that is already isolated (it runs the sandbox tool itself).
    Command(Command),
    /// A process started inside a Windows AppContainer and Job Object.
    #[cfg(windows)]
    AppContainer(Box<sandbox_windows::LaunchConfig>),
}

/// Identifies one run to `OsSandboxEngine::collect_denials`.
#[derive(Debug, Clone)]
pub struct RunInfo {
    pub run_id: String,
    pub started: SystemTime,
    pub program: std::path::PathBuf,
}
