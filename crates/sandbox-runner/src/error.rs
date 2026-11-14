//! Errors of the sandbox runner. A request that cannot be isolated is an error, never a
//! silent run on the host (`AGENTS.md` rule 5).

use std::io;

use thiserror::Error;

use crate::path_guard::PathGuardError;

#[derive(Debug, Error)]
pub enum SandboxError {
    /// The requested isolation cannot be provided. Nothing was executed.
    #[error("sandbox unavailable: {reason}")]
    SandboxUnavailable { reason: String },
    #[error("invalid sandbox request: {0}")]
    InvalidSpec(String),
    #[error("path rejected: {0}")]
    Path(#[from] PathGuardError),
    /// A limit the caller asked for that this sandbox cannot enforce: running anyway would be
    /// running with less protection than requested.
    #[error("the {engine} sandbox cannot enforce the requested limit: {limit}")]
    LimitNotEnforceable {
        engine: &'static str,
        limit: &'static str,
    },
    #[error("the {engine} sandbox cannot enforce the requested network policy")]
    NetworkPolicyNotEnforceable { engine: &'static str },
    #[error("failed to launch the sandboxed process: {0}")]
    Spawn(#[source] io::Error),
    #[error("failed to start the orphan watchdog: {0}")]
    Watchdog(#[source] io::Error),
    #[error("process supervision is not available on this platform; refusing to run unsupervised")]
    SupervisionUnsupported,
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

impl SandboxError {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        SandboxError::SandboxUnavailable {
            reason: reason.into(),
        }
    }
}
