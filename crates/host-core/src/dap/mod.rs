//! JavaScript/TypeScript debugging through the Debug Adapter Protocol (`LOT-010`), with
//! vscode-js-debug's DAP server (`scripts/fetch-js-debug.mjs`).

mod connection;
mod session;

use thiserror::Error;

pub use connection::{DapConnection, DapHandler, DapIncoming};
pub use session::{
    DebugCommand, DebugEvent, DebugEventSink, DebugLaunch, DebugSession, DebugSessionRole,
    DEBUG_EVENT_FRAME,
};

use crate::lsp::LspError;
use crate::process::SupervisionError;
use crate::trust::TrustError;

#[derive(Debug, Error)]
pub enum DebugError {
    #[error(transparent)]
    Trust(#[from] TrustError),
    #[error(transparent)]
    Supervision(#[from] SupervisionError),
    #[error("the program to debug must be a file inside the workspace")]
    ProgramOutsideWorkspace,
    #[error("debug adapter I/O error: {0}")]
    Io(String),
    #[error("the debug adapter did not start: {0}")]
    AdapterStart(String),
    #[error("debug adapter process exited: {detail}")]
    ProcessExited { detail: String },
    #[error("debug adapter disconnected")]
    Disconnected,
    #[error("debug adapter did not answer `{command}` in time")]
    Timeout { command: String },
    #[error("debug adapter refused `{command}`: {message}")]
    Adapter { command: String, message: String },
    #[error("no program is being debugged yet")]
    NoDebuggee,
}

impl DebugError {
    pub(crate) fn io(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<LspError> for DebugError {
    fn from(error: LspError) -> Self {
        match error {
            LspError::Trust(error) => Self::Trust(error),
            LspError::ProcessExited { detail } => Self::ProcessExited { detail },
            other => Self::AdapterStart(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_exit_keeps_its_diagnostic_and_error_kind() {
        let error = DebugError::from(LspError::ProcessExited {
            detail: "exit 1: missing runtime".into(),
        });
        assert!(
            matches!(error, DebugError::ProcessExited { ref detail } if detail == "exit 1: missing runtime")
        );
    }
}
