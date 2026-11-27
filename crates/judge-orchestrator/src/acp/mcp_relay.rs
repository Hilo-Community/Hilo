//! Standard MCP relay: the Host's local MCP server offered to the agent as an ordinary
//! Streamable HTTP MCP server in `session/new`.
//!
//! The transport (an HTTP endpoint the sandboxed agent can reach) is owned by the Host. This
//! module only builds the handler behind it, and that handler sends every MCP request through
//! the session's [`ToolBroker`] as one `query_local_mcp` call: policy, budget, durable intent
//! and idempotency apply exactly as for `_hilo/query_local_mcp`. Nothing here executes anything
//! by itself.
use super::{
    executor::EventSink,
    protocol::{AcpEvent, MAX_MESSAGE_BYTES},
};
use crate::{
    broker::{BrokerError, ToolBroker},
    policy::{Decision, SignedTaskSpec},
    Lease, Millis,
};
use host_core::{ToolArgValue, ToolCall, ToolCapability, CONTRACT_SCHEMA_VERSION};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

/// Answers one MCP JSON-RPC message: `Some(response)` for a request, `None` for a notification.
/// An `Err` means the message itself was refused (not valid single JSON-RPC).
pub type McpHandler = Arc<dyn Fn(&str) -> Result<Option<String>, String> + Send + Sync>;

/// JSON-RPC error code for a call the Host did not authorize.
const NOT_AUTHORIZED: i64 = -32001;
/// Upper bound on relayed requests per session, so the journal cannot grow without limit.
const MAX_RELAYED_REQUESTS: u64 = 4096;

/// An HTTP endpoint the Host serves to this session's agent, and how to plug the handler in.
pub struct McpRelay {
    /// Absolute `http://` URL the agent can reach from inside its sandbox.
    pub url: String,
    /// Value of the `Authorization` header the agent must send.
    pub authorization: String,
    /// Installs the handler once the session's broker exists. Until then the endpoint refuses.
    pub install: Box<dyn FnOnce(McpHandler) + Send>,
}

impl McpRelay {
    /// The `mcpServers` entry ACP v1 defines for an HTTP MCP server.
    pub(crate) fn server_entry(&self) -> Value {
        json!({
            "type": "http",
            "name": "hilo",
            "url": self.url,
            "headers": [{"name": "Authorization", "value": self.authorization}],
        })
    }
}

pub(crate) struct RelayContext {
    pub broker: Arc<ToolBroker>,
    pub lease: Lease,
    pub signed: SignedTaskSpec,
    pub clock: Arc<dyn Fn() -> Millis + Send + Sync>,
    pub events: EventSink,
}

pub(crate) fn handler(context: RelayContext) -> McpHandler {
    let next = AtomicU64::new(0);
    Arc::new(move |line: &str| relay(&context, &next, line))
}

fn relay(context: &RelayContext, next: &AtomicU64, line: &str) -> Result<Option<String>, String> {
    if line.len() > MAX_MESSAGE_BYTES {
        return Err("MCP message size limit".into());
    }
    let message: Value = serde_json::from_str(line).map_err(|_| "MCP message is not JSON")?;
    let Some(object) = message.as_object() else {
        return Err("batched or non-object MCP messages are not supported".into());
    };
    if object.get("jsonrpc") != Some(&json!("2.0")) {
        return Err("MCP message is not JSON-RPC 2.0".into());
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Err("the Host MCP server sends no requests, so it expects no responses".into());
    };
    let Some(id) = object.get("id") else {
        // A notification (`notifications/initialized`, cancellations) carries no data and
        // reaches no tool: there is nothing for the broker to authorize or to run.
        return Ok(None);
    };
    if !(id.is_string() || id.is_number()) {
        return Err("MCP request id must be a string or a number".into());
    }
    let n = next.fetch_add(1, Ordering::SeqCst);
    if n >= MAX_RELAYED_REQUESTS {
        return Ok(Some(error(id, "MCP request count limit".into())));
    }
    let call_id = format!("mcp-{n}");
    let request = serde_json::to_string(&message).map_err(|e| e.to_string())?;
    let call = ToolCall {
        schema_version: CONTRACT_SCHEMA_VERSION,
        call_id: call_id.clone(),
        task_id: context.lease.task_id().into(),
        tool_name: "query_local_mcp".into(),
        validated_args: BTreeMap::from([("request".into(), ToolArgValue::Text(request))]),
        required_capabilities: vec![ToolCapability::QueryLocalMcp],
        preconditions: vec![],
        idempotency_key: format!("mcp:{}:{n}", context.lease.generation()),
    };
    // Only tool calls are shown in the panel: the handshake and listings are plumbing.
    let shown = method == "tools/call";
    let tool = |status: &str, reason: Option<String>| {
        if shown {
            (context.events)(AcpEvent::Tool {
                id: call_id.clone(),
                kind: "query_local_mcp".into(),
                status: status.into(),
                locations: json!([]),
                reason,
            });
        }
    };
    tool("running", None);
    let answer = match context.broker.execute(
        &context.lease,
        &context.signed,
        &call,
        &[],
        (context.clock)(),
    ) {
        Ok(outcome) if outcome.succeeded => Ok(outcome.summary),
        Ok(outcome) => Err(outcome.summary),
        // The relay has no way to ask the user mid-request: a call that needs consent is refused.
        Err(BrokerError::Policy(Decision::RequiresApproval(_, reason))) => {
            Err(format!("requires Host approval: {reason}"))
        }
        Err(other) => Err(other.to_string()),
    };
    match answer {
        Ok(response) => {
            tool("done", None);
            Ok(Some(response))
        }
        Err(reason) => {
            tool("denied", Some(reason.clone()));
            Ok(Some(error(id, reason)))
        }
    }
}

fn error(id: &Value, message: String) -> String {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":NOT_AUTHORIZED,"message":message}}).to_string()
}
