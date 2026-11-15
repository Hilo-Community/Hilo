//! Security audit events. The runner reports every refusal and every forced stop, so that the
//! `PolicyEngine` (`LOT-013`) and the user can see what a sandboxed process tried to do.
//! Kernel-level denials (a blocked `open`, a blocked `connect`) are reported by the OS engines.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SecurityEventKind {
    /// A path left the worktree (`..`, a symbolic link pointing outside, a foreign absolute path).
    PathEscapeBlocked,
    /// The sandbox cannot start, or failed its self-test: nothing ran.
    SandboxUnavailable,
    /// The request asked for a limit or a network policy the sandbox cannot enforce.
    PolicyNotEnforceable,
    /// The process outlived its wall-clock budget and its whole tree was killed.
    WallTimeoutKilled,
    /// The process exceeded its output budget and its whole tree was killed.
    OutputLimitKilled,
    /// The OS refused a file access of the sandboxed process.
    FileAccessDenied,
    /// The OS refused a network connection of the sandboxed process.
    NetworkAccessDenied,
    /// The OS refused something else (a service lookup, a signal to another process).
    SystemAccessDenied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityAuditEvent {
    pub kind: SecurityEventKind,
    pub detail: String,
    /// Milliseconds since the Unix epoch.
    pub unix_ms: u64,
}

impl SecurityAuditEvent {
    pub fn new(kind: SecurityEventKind, detail: impl Into<String>) -> Self {
        let unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64);
        Self {
            kind,
            detail: detail.into(),
            unix_ms,
        }
    }
}

/// Where audit events go. Implementations must not block.
pub trait AuditSink: Send + Sync {
    fn record(&self, event: SecurityAuditEvent);
}

/// Keeps events in memory (tests, and hosts that forward them in batches).
#[derive(Debug, Default)]
pub struct MemoryAuditSink {
    events: Mutex<Vec<SecurityAuditEvent>>,
}

impl MemoryAuditSink {
    pub fn events(&self) -> Vec<SecurityAuditEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn kinds(&self) -> Vec<SecurityEventKind> {
        self.events().iter().map(|event| event.kind).collect()
    }
}

impl AuditSink for MemoryAuditSink {
    fn record(&self, event: SecurityAuditEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}

/// Writes events to the `tracing` log at `warn` level.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingAuditSink;

impl AuditSink for TracingAuditSink {
    fn record(&self, event: SecurityAuditEvent) {
        tracing::warn!(kind = ?event.kind, detail = %event.detail, "sandbox security event");
    }
}
