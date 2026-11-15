//! An allowlist ties tool names to capabilities so a deploy cannot masquerade as a read.

use super::paths;
use host_core::{TargetEnvironment, TaskSpec, ToolArgValue, ToolCall, ToolCapability};

pub(crate) fn text<'a>(call: &'a ToolCall, key: &str) -> Option<&'a str> {
    match call.validated_args.get(key) {
        Some(ToolArgValue::Text(value)) => Some(value),
        _ => None,
    }
}

pub(super) fn valid(spec: &TaskSpec, call: &ToolCall) -> bool {
    use ToolCapability::*;
    let (primary, keys): (_, &[&str]) = match call.tool_name.as_str() {
        "read_workspace" => (ReadWorkspace, &["path"]),
        "write_worktree" => (WriteWorktree, &["path", "content"]),
        "run_sandbox_command" => (RunSandboxCommand, &["program", "cwd", "args"]),
        "query_local_mcp" => (QueryLocalMcp, &["request"]),
        "network_egress" => (NetworkEgress, &["request"]),
        "database_read" => (DatabaseRead, &["request"]),
        "database_migrate" => (DatabaseMigrate, &["target_env", "request"]),
        "deploy_preview" => (DeployPreview, &["target_env", "request"]),
        "deploy_production" => (DeployProduction, &["target_env", "request"]),
        _ => return false,
    };
    let caps = &call.required_capabilities;
    if caps.first() != Some(&primary)
        || !(caps.len() == 1
            || (primary == RunSandboxCommand
                && caps.as_slice() == [RunSandboxCommand, NetworkEgress]))
        || call.call_id.is_empty()
        || call.idempotency_key.is_empty()
        || call.validated_args.len() != keys.len()
        || keys
            .iter()
            .any(|key| !call.validated_args.contains_key(*key))
    {
        return false;
    }
    if call.preconditions.iter().any(|p| {
        p.required_snapshot_hash
            .as_ref()
            .is_some_and(|h| h != &spec.base_snapshot_id)
    }) {
        return false;
    }
    for key in keys {
        if *key == "args" {
            if !matches!(call.validated_args.get(*key), Some(ToolArgValue::StringList(v)) if v.iter().all(|s| !s.contains('\0')))
            {
                return false;
            }
            continue;
        }
        let Some(value) = text(call, key) else {
            return false;
        };
        if value.contains('\0') {
            return false;
        }
        if matches!(*key, "path" | "cwd")
            && !paths::permitted(
                value,
                &spec.policy.allowed_paths,
                &spec.policy.forbidden_paths,
            )
        {
            return false;
        }
        if *key == "program"
            && (!std::path::Path::new(value).is_absolute()
                || value.split(['/', '\\']).any(|p| p == ".."))
        {
            return false;
        }
    }
    if keys.contains(&"target_env") {
        let expected = match spec.target_env {
            TargetEnvironment::LocalWorktree => "local_worktree",
            TargetEnvironment::LocalOciSandbox => "local_oci_sandbox",
            TargetEnvironment::Staging => "staging",
            TargetEnvironment::Production => "production",
            TargetEnvironment::SanitizedDatabaseClone => "sanitized_database_clone",
        };
        if text(call, "target_env") != Some(expected)
            || (primary == DeployProduction && spec.target_env != TargetEnvironment::Production)
            || (primary == DeployPreview && spec.target_env != TargetEnvironment::Staging)
        {
            return false;
        }
    }
    true
}
