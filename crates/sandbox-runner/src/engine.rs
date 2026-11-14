//! The per-OS part of the sandbox: how a request is turned into an isolated process.

use std::process::Command;

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

    /// The command that runs `spec` inside the sandbox, with `spec.environment()` as its whole
    /// environment and `spec.cwd` as its working directory. The runner sets stdio and the
    /// process group.
    fn prepare(&self, spec: &ValidatedSpec) -> Result<Command, SandboxError>;
}
