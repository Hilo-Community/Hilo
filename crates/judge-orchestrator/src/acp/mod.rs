//! Bounded ACP v1 stdio client. Only trusted Host code constructs configuration and approvals.
mod executor;
mod protocol;
mod session;

pub use executor::{AgentToolExecutor, WorkspaceText};
pub use protocol::{
    AcpError, AcpEvent, ProposedEdit, MAX_FILE_BYTES, MAX_MESSAGE_BYTES, MAX_PENDING_REQUESTS,
    MAX_TERMINAL_OUTPUT_BYTES,
};
pub use session::{AcpHost, AcpLaunch, AcpSessionSupervisor, AgentCredentials, AgentNetwork};
