//! Non-secret preferences and the last observed ACP auth surface. Keys belong in the OS store.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AccountMode {
    #[default]
    Auto,
    ApiKey,
    Account,
    Offline,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountPreferences {
    pub mode: AccountMode,
    pub provider: Option<String>,
    pub method_id: Option<String>,
    pub extra_allowed_hosts: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentAccount {
    #[serde(default)]
    pub terminal_authenticated: bool,
    pub preferences: AccountPreferences,
    /// Untrusted display/negotiation data, never an authority to grant network or execute args.
    pub auth_methods: Vec<Value>,
}
