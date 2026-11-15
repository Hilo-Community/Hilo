//! ACP Host entry points. Native dialogs provide the human decision; signing stays in Rust.
use crate::{bridge::DesktopBinaryEditorBridge, state::DesktopAppState};
use host_core::*;
use judge_orchestrator::{acp::*, policy::HostAuthority, TaskStateRepository};
use sandbox_runner::{PathGuard, ResourceLimits, TracingAuditSink};
use std::{
    io::Read,
    path::PathBuf,
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{ipc::Channel, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

struct HostText {
    root: PathBuf,
    bridge: Arc<Mutex<DesktopBinaryEditorBridge>>,
    mcp: Option<Mutex<context_engine::LocalMcpServer>>,
    _worktree: Option<TaskSnapshotGuard>,
}
struct TaskSnapshotGuard {
    manager: CheckpointManager,
    task_id: String,
}
impl Drop for TaskSnapshotGuard {
    fn drop(&mut self) {
        if let Err(error) = self.manager.release_worktree(&self.task_id) {
            tracing::warn!(%error, "ACP task worktree cleanup failed");
        }
    }
}
impl WorkspaceText for HostText {
    fn query_local_mcp(&self, request: &str) -> Result<String, String> {
        let mut mcp = self
            .mcp
            .as_ref()
            .ok_or("Local MCP is unavailable")?
            .lock()
            .map_err(|_| "MCP lock poisoned")?;
        let bridge = self.bridge.lock().map_err(|_| "document lock poisoned")?;
        mcp.engine
            .overlay_mut()
            .sync_from_document_service(Some(&self.root), bridge.document_service());
        mcp.handle_line(request)
            .map(|value| value.to_string())
            .ok_or_else(|| "MCP notification has no response".into())
    }
    fn text(&self, relative: &str) -> Result<String, String> {
        let path = PathGuard::verify_within_worktree(&self.root, &self.root.join(relative))
            .map_err(|e| e.to_string())?;
        let bridge = self.bridge.lock().map_err(|_| "document lock poisoned")?;
        if let Some(session) = bridge
            .document_service()
            .sessions()
            .filter(|s| s.disk_path() == Some(path.as_path()))
            .max_by_key(|s| s.session_generation())
        {
            if session.rope().len_bytes() > 24 * 1024 {
                return Err("file size limit".into());
            }
            return Ok(session.rope().to_string());
        }
        // Unopened files enter host-core before their text becomes a proposal's base.
        drop(bridge);
        let mut text = String::new();
        std::fs::File::open(&path)
            .map_err(|e| e.to_string())?
            .take(24 * 1024 + 1)
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
        if text.len() > 24 * 1024 {
            return Err("file size limit".into());
        }
        let mut bridge = self.bridge.lock().map_err(|_| "document lock poisoned")?;
        // Recheck under the lock: an editor could have opened this file while disk was read.
        if let Some(session) = bridge
            .document_service()
            .sessions()
            .filter(|s| s.disk_path() == Some(path.as_path()))
            .max_by_key(|s| s.session_generation())
        {
            return Ok(session.rope().to_string());
        }
        bridge
            .documents_mut()
            .open_document_with_editor_text(relative, 0, &path, &text)
            .map_err(|e| e.to_string())?;
        Ok(bridge
            .document_service()
            .session(relative, 0)
            .map_err(|e| e.to_string())?
            .rope()
            .to_string())
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn gate(state: &DesktopAppState) -> Result<WorkspaceTrustGate, String> {
    let gate = state
        .trust_gate
        .lock()
        .map_err(|_| "trust lock poisoned")?
        .clone()
        .ok_or("No workspace is open")?;
    gate.authorize(GatedCapability::WorkspaceLocalBinary)
        .map_err(|e| e.to_string())?;
    Ok(gate)
}
type Agent = Arc<Mutex<AcpSessionSupervisor>>;
fn agent(state: &DesktopAppState, id: u32) -> Result<Agent, String> {
    gate(state)?;
    state
        .agents
        .lock()
        .map_err(|_| "agents lock poisoned")?
        .get(&id)
        .cloned()
        .ok_or_else(|| "Agent session is closed".into())
}
pub(crate) fn close_all_agents(state: &DesktopAppState) {
    let agents: Vec<_> = state
        .agents
        .lock()
        .map(|mut s| s.drain().map(|(_, a)| a).collect())
        .unwrap_or_default();
    for agent in agents {
        if let Ok(mut agent) = agent.lock() {
            agent.stop();
        }
    }
}

#[tauri::command]
pub async fn agent_start_command(
    app: tauri::AppHandle,
    program: String,
    args: Vec<String>,
    provider: Option<String>,
    events: Channel<AcpEvent>,
) -> Result<u32, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<DesktopAppState>();
        let gate = gate(&state)?;
        if !std::path::Path::new(&program).is_absolute() || args.len() > 64 || program.len() + args.iter().map(String::len).sum::<usize>() > 24 * 1024 { return Err("Invalid agent launch arguments".into()); }
        if provider.is_some() { return Err("Provider-only egress requires a destination-filtering Host proxy; no agent was started".into()); }
        if !app.dialog().message(format!("Start this ACP program in a read-only sandbox?\n\nProgram: {program}\nArguments: {args:?}\n\nNetwork and credentials are unavailable in this task. File writes become proposals for review.")).title("Start ACP agent").buttons(MessageDialogButtons::OkCancelCustom("Start agent".into(), "Cancel".into())).blocking_show() { return Err("Agent launch cancelled".into()); }
        gate.authorize(GatedCapability::WorkspaceLocalBinary).map_err(|e| e.to_string())?;
        let id = state.next_agent_id.fetch_add(1, Ordering::SeqCst);
        let root = gate.workspace_root().to_path_buf();
        let repository = Arc::new(TaskStateRepository::open(state.app_data_dir.join("agent-tasks")).map_err(|e| e.to_string())?);
        let task_id = format!("acp-{}-{id}", now());
        let manager = CheckpointManager::open(&root).map_err(|e| e.to_string())?
            .with_worktrees_dir(&state.app_data_dir.join("agent-worktrees")).map_err(|e| e.to_string())?;
        let buffers: Vec<_> = state.bridge.lock().map_err(|_| "document lock poisoned")?
            .document_service().sessions().filter_map(|s| s.disk_path().filter(|p| p.starts_with(&root)).map(|p| (p.to_path_buf(), s.rope().to_string()))).collect();
        let buffers: Vec<_> = buffers.iter().map(|(path, text)| UnsavedBuffer { path, text }).collect();
        let checkpoint = manager.create_checkpoint(&task_id, CheckpointKind::BeforeAgent, &buffers).map_err(|e| e.to_string())?;
        let task = manager.provision_worktree(&task_id).map_err(|e| e.to_string())?;
        let worktree = TaskSnapshotGuard { manager, task_id: task_id.clone() };
        let program = PathBuf::from(&program);
        let program = program.strip_prefix(&root).map_or_else(|_| program.clone(), |p| task.workspace.join(p));
        let spec = TaskSpec { schema_version: CONTRACT_SCHEMA_VERSION, task_id: task_id.clone(), objective: "ACP agent proposal session".into(), workspace_id: root.to_string_lossy().into_owned(), base_snapshot_id: format!("session-{task_id}"), target_env: TargetEnvironment::LocalWorktree, policy: ExecutionPolicy { autonomy_level: AutonomyLevel::BoundedAutoCorrect, network_allowed: false, allowed_paths: vec![".".into()], forbidden_paths: vec![".git".into(), ".env".into(), ".ssh".into(), ".aws".into(), ".coloride-tmp".into()], max_auto_retries: 2, workspace_trusted: true }, budget_limits: BudgetLimits { max_tokens: 0, max_cost_usd_micros: 0, max_cpu_ms: 3_600_000, max_tool_calls: 512 }, acceptance_criteria: vec![], state: TaskState::Created };
        let authority = Arc::new(HostAuthority::new().map_err(|e| e.to_string())?);
        let mut spec = spec;
        spec.base_snapshot_id = checkpoint.commit;
        let signed = authority.sign_task(spec.clone()).map_err(|e| e.to_string())?;
        repository.create_task(spec, now()).map_err(|e| e.to_string())?;
        let lease = repository.acquire_lease(&task_id, "acp-host", Duration::from_secs(3600), now()).map_err(|e| e.to_string())?;
        for state in [TaskState::ContextReady, TaskState::Planned, TaskState::Running] { repository.transition(&lease, state, "Host started ACP session", now()).map_err(|e| e.to_string())?; }
        let runner = sandbox_runner::native_sandbox(Arc::new(TracingAuditSink)).map_err(|e| e.to_string())?;
        let engine = context_engine::SqliteWalContextEngine::open_or_rebuild_from_workspace(state.app_data_dir.join(format!("{task_id}-context.sqlite")), &task.workspace, "acp-session").map_err(|e| e.to_string())?;
        let mcp = context_engine::LocalMcpServer::new(engine, context_engine::ContextBundleAssembler::new(vec![AuthorizedDestination::LocalOnly]));
        let supervisor = AcpSessionSupervisor::start(AcpHost { gate: gate.clone(), worktree_root: task.workspace, repository, lease, signed, authority, runner: Arc::from(runner), text: Arc::new(HostText { root, bridge: state.bridge.clone(), mcp: Some(Mutex::new(mcp)), _worktree: Some(worktree) }), clock: Arc::new(now), events: Arc::new(move |event| { let _ = events.send(event); }) }, AcpLaunch { program, args, credentials: AgentCredentials::Unauthenticated, network: AgentNetwork::Offline, limits: ResourceLimits { wall_timeout: Duration::from_secs(3600), max_cpu_time: Some(Duration::from_secs(120)), ..ResourceLimits::default() } }).map_err(|e| e.to_string())?;
        // Workspace switches/revocations close this same gate. Recheck while holding the map,
        // so cleanup either sees this session or this insertion observes the closed gate.
        let mut agents = state.agents.lock().map_err(|_| "agents lock poisoned")?;
        gate.authorize(GatedCapability::WorkspaceLocalBinary).map_err(|e| e.to_string())?;
        agents.insert(id, Arc::new(Mutex::new(supervisor)));
        Ok(id)
    }).await.map_err(|e| e.to_string())?
}
#[tauri::command]
pub fn agent_prompt_command(
    state: tauri::State<'_, DesktopAppState>,
    id: u32,
    text: String,
) -> Result<(), String> {
    agent(&state, id)?
        .lock()
        .map_err(|_| "agent lock poisoned")?
        .prompt(text)
        .map_err(|e| e.to_string())
}
#[tauri::command]
pub fn agent_cancel_command(
    state: tauri::State<'_, DesktopAppState>,
    id: u32,
) -> Result<(), String> {
    agent(&state, id)?
        .lock()
        .map_err(|_| "agent lock poisoned")?
        .cancel();
    Ok(())
}
#[tauri::command]
pub async fn agent_stop_command(app: tauri::AppHandle, id: u32) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let agent = app
            .state::<DesktopAppState>()
            .agents
            .lock()
            .map_err(|_| "agents lock poisoned")?
            .remove(&id);
        if let Some(agent) = agent {
            agent.lock().map_err(|_| "agent lock poisoned")?.stop();
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
pub async fn agent_permission_command(
    app: tauri::AppHandle,
    id: u32,
    request_id: String,
    allow: bool,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let agent = agent(&app.state::<DesktopAppState>(), id)?;
        let description = agent
            .lock()
            .map_err(|_| "agent lock poisoned")?
            .permission_description(&request_id)
            .ok_or("Permission request is no longer pending")?;
        // The webview requests a decision; only this native dialog confirms consent.
        let approved = allow
            && app
                .dialog()
                .message(format!(
                    "Allow this exact ACP operation once?\n\n{description}"
                ))
                .title("ACP permission")
                .buttons(MessageDialogButtons::OkCancelCustom(
                    "Allow once".into(),
                    "Reject".into(),
                ))
                .blocking_show();
        let result = agent
            .lock()
            .map_err(|_| "agent lock poisoned")?
            .answer_permission(request_id, approved)
            .map_err(|e| e.to_string());
        result
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn untrusted_host_cannot_access_agents() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let state = crate::state::test_state(data.path());
        crate::commands::workspace::load_workspace(&state, root.path().to_str().unwrap()).unwrap();
        assert!(gate(&state).is_err());
        assert!(agent(&state, 1).is_err());
    }
    #[test]
    fn proposal_base_reads_unsaved_rope() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.rs");
        std::fs::write(&path, "disk").unwrap();
        let bridge = Arc::new(Mutex::new(DesktopBinaryEditorBridge::new_in_memory()));
        bridge
            .lock()
            .unwrap()
            .documents_mut()
            .open_document_with_editor_text(
                "file.rs",
                1,
                path.canonicalize().unwrap(),
                "unsaved 🦀",
            )
            .unwrap();
        let text = HostText {
            root: root.path().canonicalize().unwrap(),
            bridge,
            mcp: None,
            _worktree: None,
        };
        assert_eq!(text.text("file.rs").unwrap(), "unsaved 🦀");
    }
}
