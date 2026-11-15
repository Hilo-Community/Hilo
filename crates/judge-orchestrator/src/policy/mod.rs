//! Authorization depends exclusively on authenticated contracts and typed call fields.

mod authority;
pub(crate) mod paths;
mod schema;

pub use authority::{AuthorityError, HostAuthority, SignedApproval, SignedTaskSpec};
use host_core::{Approval, ApprovalActionKind, AutonomyLevel, TaskState, ToolCall, ToolCapability};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(String),
    RequiresApproval(ApprovalActionKind, String),
}

/// The Host supplies authority; callers cannot replace policy by embedding instructions in text.
pub struct PolicyEngine {
    authority: Arc<HostAuthority>,
}

impl PolicyEngine {
    pub fn new(authority: Arc<HostAuthority>) -> Self {
        Self { authority }
    }

    pub fn evaluate(
        &self,
        signed: &SignedTaskSpec,
        call: &ToolCall,
        approvals: &[SignedApproval],
        task_state: TaskState,
        now: u64,
    ) -> Decision {
        let deny = |reason: &str| Decision::Deny(reason.into());
        if !self.authority.verify_task(signed) {
            return deny("unauthenticated task");
        }
        let spec = &signed.spec;
        if !matches!(task_state, TaskState::Running | TaskState::Verifying)
            || call.task_id != spec.task_id
            || spec.schema_version != host_core::CONTRACT_SCHEMA_VERSION
            || call.schema_version != host_core::CONTRACT_SCHEMA_VERSION
        {
            return deny("task identity, schema or state mismatch");
        }
        if !schema::valid(spec, call) {
            return deny("unsupported tool, capabilities, arguments or preconditions");
        }
        let mut required = Vec::new();
        for capability in &call.required_capabilities {
            use ToolCapability::*;
            match capability {
                WriteWorktree | RunSandboxCommand
                    if spec.policy.autonomy_level == AutonomyLevel::ProposalOnly =>
                {
                    return deny("proposal-only task");
                }
                NetworkEgress if !spec.policy.network_allowed => return deny("network denied"),
                RunSandboxCommand | QueryLocalMcp if !spec.policy.workspace_trusted => {
                    return deny("workspace untrusted");
                }
                DatabaseMigrate => required.push(ApprovalActionKind::RunDatabaseMigration),
                DeployPreview | DeployProduction => {
                    required.push(ApprovalActionKind::DeployInfrastructure)
                }
                ReadWorkspace | WriteWorktree | RunSandboxCommand | QueryLocalMcp
                | NetworkEgress | DatabaseRead => {}
            }
        }
        if call.preconditions.iter().any(|p| p.requires_human_approval) {
            required.push(ApprovalActionKind::ExecuteToolCall);
        }
        if approvals.iter().any(|a| !self.authority.verify_approval(a)) {
            return deny("unauthenticated approval");
        }
        for kind in required {
            let Ok((call_hash, plan_hash)) = approval_inputs(signed, call) else {
                return deny("cannot encode action");
            };
            let digest =
                Approval::compute_action_digest(&call_hash, &plan_hash, &spec.target_env, &kind);
            if !approvals.iter().any(|a| {
                a.approval.is_valid_for_plan_and_patch(
                    &spec.target_env,
                    &kind,
                    &call_hash,
                    &plan_hash,
                    now,
                )
            }) {
                return Decision::RequiresApproval(kind, digest);
            }
        }
        Decision::Allow
    }
}

/// Bind approvals to the entire call and signed plan, including budgets and policy.
pub fn approval_inputs(
    signed: &SignedTaskSpec,
    call: &ToolCall,
) -> Result<(String, String), bincode::Error> {
    Ok((
        call_digest(call)?,
        blake3::hash(&bincode::serialize(&signed.spec)?)
            .to_hex()
            .to_string(),
    ))
}

pub fn call_digest(call: &ToolCall) -> Result<String, bincode::Error> {
    Ok(blake3::hash(&bincode::serialize(call)?)
        .to_hex()
        .to_string())
}
