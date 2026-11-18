//! The isolated sandbox runner: every command an agent asks for runs through `SandboxRunner`, in
//! the native sandbox of the OS (Seatbelt on macOS, bubblewrap on Linux, AppContainer + Job
//! Object on Windows), or not at all (`AGENTS.md` rule 5).
//!
//! `healthcheck` still reports `execution_available: false`: no application command uses the
//! runner yet (wired with the task runner, LOT-013). Its targets are configuration, not measured
//! guarantees.

pub mod audit;
pub mod bubblewrap;
mod common;
pub mod engine;
pub mod error;
pub mod path_guard;
mod probe;
pub mod runner;
pub mod seatbelt;
pub mod spec;
pub mod supervisor;
#[cfg(windows)]
pub mod windows;

pub use audit::{
    AuditSink, MemoryAuditSink, SecurityAuditEvent, SecurityEventKind, TracingAuditSink,
};
pub use bubblewrap::BubblewrapEngine;
pub use engine::{EngineCapabilities, Launch, OsSandboxEngine, RunInfo};
pub use error::SandboxError;
pub use path_guard::{PathGuard, PathGuardError};
pub use runner::{native_sandbox, OsNativeSandbox, SandboxRunner};
pub use seatbelt::SeatbeltEngine;
pub use spec::{NetworkPolicy, ResourceLimits, SandboxSpec, ValidatedSpec, TMP_DIR_NAME};
pub use supervisor::SandboxedTerminal;
pub use supervisor::{ChildInput, ChildOutput, RunOutput, SandboxedChild, Termination};
#[cfg(windows)]
pub use windows::AppContainerEngine;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OsSandboxKind {
    MacOsSeatbelt,
    LinuxLandlockBubblewrap,
    WindowsAppContainerJobObject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    pub subsystem: String,
    pub native_os_kind: OsSandboxKind,
    pub fail_closed_enforced: bool,
    pub target_startup_ms: u32,
    /// False until the native runner can actually create and verify the requested isolation.
    pub execution_available: bool,
}

pub fn detect_native_os_sandbox() -> OsSandboxKind {
    if cfg!(target_os = "macos") {
        OsSandboxKind::MacOsSeatbelt
    } else if cfg!(target_os = "windows") {
        OsSandboxKind::WindowsAppContainerJobObject
    } else {
        OsSandboxKind::LinuxLandlockBubblewrap
    }
}

pub fn healthcheck() -> SandboxCapabilities {
    SandboxCapabilities {
        subsystem: "sandbox-runner".to_string(),
        native_os_kind: detect_native_os_sandbox(),
        fail_closed_enforced: true,
        target_startup_ms: 5,
        execution_available: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_runner_enforces_fail_closed_invariant() {
        let caps = healthcheck();
        assert_eq!(caps.subsystem, "sandbox-runner");
        assert!(caps.fail_closed_enforced);
        assert!(!caps.execution_available);
        assert!(caps.target_startup_ms <= 5);
    }
}
