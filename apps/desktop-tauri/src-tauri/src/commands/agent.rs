//! ACP Host entry points. Native dialogs provide the human decision; signing stays in Rust.
use crate::{bridge::DesktopBinaryEditorBridge, state::DesktopAppState};
use host_core::*;
use judge_orchestrator::{
    acp::*,
    policy::{HostAuthority, SignedTaskSpec},
    Lease, TaskStateRepository,
};
use sandbox_runner::{PathGuard, ResourceLimits, TracingAuditSink};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{ipc::Channel, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

/// How long one session may run: its lease, and the lifetime of the agent process.
const SESSION_LIFETIME: Duration = Duration::from_secs(3600);
/// CPU allowance of the agent process and of each command it runs.
const CPU_ALLOWANCE: Duration = Duration::from_secs(120);
const MAX_LAUNCH_ARGS: usize = 64;
const MAX_LAUNCH_BYTES: usize = 24 * 1024;
/// Never readable by the agent, whatever the task.
const FORBIDDEN_PATHS: [&str; 5] = [".git", ".env", ".ssh", ".aws", ".coloride-tmp"];

/// The workspace text the agent reads: the live Rope of an open document (unsaved edits
/// included), otherwise the file on disk. Reading never opens a document in the editor.
struct HostText {
    root: PathBuf,
    bridge: Arc<Mutex<DesktopBinaryEditorBridge>>,
    mcp: Option<Mutex<context_engine::LocalMcpServer>>,
    // Dropped last: the MCP server above closes its database before the task files are removed.
    _task: Option<TaskFiles>,
}

/// The task worktree and context database of one session, removed when the session ends.
struct TaskFiles {
    manager: CheckpointManager,
    task_id: String,
    context_db: PathBuf,
}

impl Drop for TaskFiles {
    fn drop(&mut self) {
        if let Err(error) = self.manager.release_worktree(&self.task_id) {
            tracing::warn!(%error, "ACP task worktree cleanup failed");
        }
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.context_db.clone().into_os_string();
            path.push(suffix);
            match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    tracing::warn!(%error, "ACP context database cleanup failed");
                }
                _ => {}
            }
        }
    }
}

impl HostText {
    fn open_document_text(&self, path: &Path) -> Result<Option<String>, String> {
        let bridge = self.bridge.lock().map_err(|_| "document lock poisoned")?;
        let Some(session) = bridge
            .document_service()
            .sessions()
            .filter(|s| s.disk_path() == Some(path))
            .max_by_key(|s| s.session_generation())
        else {
            return Ok(None);
        };
        if session.rope().len_bytes() > MAX_FILE_BYTES {
            return Err("file size limit".into());
        }
        Ok(Some(session.rope().to_string()))
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
        if let Some(text) = self.open_document_text(&path)? {
            return Ok(text);
        }
        let mut text = String::new();
        std::fs::File::open(&path)
            .map_err(|e| e.to_string())?
            .take(MAX_FILE_BYTES as u64 + 1)
            .read_to_string(&mut text)
            .map_err(|e| e.to_string())?;
        if text.len() > MAX_FILE_BYTES {
            return Err("file size limit".into());
        }
        Ok(text)
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

fn check_launch(program: &str, args: &[String]) -> Result<(), String> {
    let bytes = program.len() + args.iter().map(String::len).sum::<usize>();
    if !Path::new(program).is_absolute() || args.len() > MAX_LAUNCH_ARGS || bytes > MAX_LAUNCH_BYTES
    {
        return Err("Invalid agent launch arguments".into());
    }
    Ok(())
}

fn resolve_launch(
    state: &DesktopAppState,
    installed_agent_id: &str,
) -> Result<agent_registry::LaunchCommand, String> {
    gate(state)?;
    let launch = super::agent_registry::registry(state)?
        .launch_command(installed_agent_id)
        .map_err(|e| e.to_string())?;
    check_launch(&launch.program.to_string_lossy(), &launch.args)?;
    Ok(launch)
}

fn task_spec(task_id: &str, workspace: &Path, base_snapshot_id: String) -> TaskSpec {
    TaskSpec {
        schema_version: CONTRACT_SCHEMA_VERSION,
        task_id: task_id.into(),
        objective: "ACP agent proposal session".into(),
        workspace_id: workspace.to_string_lossy().into_owned(),
        base_snapshot_id,
        target_env: TargetEnvironment::LocalWorktree,
        policy: ExecutionPolicy {
            autonomy_level: AutonomyLevel::BoundedAutoCorrect,
            network_allowed: false,
            allowed_paths: vec![".".into()],
            forbidden_paths: FORBIDDEN_PATHS.map(Into::into).to_vec(),
            max_auto_retries: 2,
            workspace_trusted: true,
        },
        budget_limits: BudgetLimits {
            max_tokens: 0,
            max_cost_usd_micros: 0,
            max_cpu_ms: 3_600_000,
            max_tool_calls: 512,
        },
        acceptance_criteria: vec![],
        state: TaskState::Created,
    }
}

/// A checkpoint of the workspace (unsaved buffers included) and the CoW task worktree made
/// from it, where the agent and its commands run.
struct Provisioned {
    task_id: String,
    checkpoint: String,
    worktree: PathBuf,
    files: TaskFiles,
}

fn provision(state: &DesktopAppState, root: &Path, id: u32) -> Result<Provisioned, String> {
    let task_id = format!("acp-{}-{id}", now());
    let manager = CheckpointManager::open(root)
        .map_err(|e| e.to_string())?
        .with_worktrees_dir(&state.app_data_dir.join("agent-worktrees"))
        .map_err(|e| e.to_string())?;
    let buffers: Vec<_> = state
        .bridge
        .lock()
        .map_err(|_| "document lock poisoned")?
        .document_service()
        .sessions()
        .filter_map(|s| {
            let path = s.disk_path().filter(|p| p.starts_with(root))?;
            Some((path.to_path_buf(), s.rope().to_string()))
        })
        .collect();
    let buffers: Vec<_> = buffers
        .iter()
        .map(|(path, text)| UnsavedBuffer { path, text })
        .collect();
    let checkpoint = manager
        .create_checkpoint(&task_id, CheckpointKind::BeforeAgent, &buffers)
        .map_err(|e| e.to_string())?;
    let task = manager
        .provision_worktree(&task_id)
        .map_err(|e| e.to_string())?;
    let context_db = state.app_data_dir.join(format!("{task_id}-context.sqlite"));
    Ok(Provisioned {
        files: TaskFiles {
            manager,
            task_id: task_id.clone(),
            context_db,
        },
        task_id,
        checkpoint: checkpoint.commit,
        worktree: task.workspace,
    })
}

/// A Running task signed by a fresh Host authority, under a lease held for the session.
fn start_task(
    state: &DesktopAppState,
    spec: TaskSpec,
) -> Result<
    (
        Arc<TaskStateRepository>,
        Lease,
        SignedTaskSpec,
        Arc<HostAuthority>,
    ),
    String,
> {
    let repository = TaskStateRepository::open(state.app_data_dir.join("agent-tasks"))
        .map_err(|e| e.to_string())?;
    let authority = Arc::new(HostAuthority::new().map_err(|e| e.to_string())?);
    let signed = authority
        .sign_task(spec.clone())
        .map_err(|e| e.to_string())?;
    let task_id = spec.task_id.clone();
    repository
        .create_task(spec, now())
        .map_err(|e| e.to_string())?;
    let lease = repository
        .acquire_lease(&task_id, "acp-host", SESSION_LIFETIME, now())
        .map_err(|e| e.to_string())?;
    for next in [
        TaskState::ContextReady,
        TaskState::Planned,
        TaskState::Running,
    ] {
        repository
            .transition(&lease, next, "Host started ACP session", now())
            .map_err(|e| e.to_string())?;
    }
    Ok((Arc::new(repository), lease, signed, authority))
}

fn start_session(
    app: &tauri::AppHandle,
    installed_agent_id: String,
    events: Channel<AcpEvent>,
) -> Result<u32, String> {
    let state = app.state::<DesktopAppState>();
    let gate = gate(&state)?;
    // The program comes from the registry by id, never from the webview, and the user already
    // confirmed it when installing; it still runs only in the read-only sandbox.
    let launch = resolve_launch(&state, &installed_agent_id)?;
    gate.authorize(GatedCapability::WorkspaceLocalBinary)
        .map_err(|e| e.to_string())?;
    let id = state.next_agent_id.fetch_add(1, Ordering::SeqCst);
    let root = gate.workspace_root().to_path_buf();
    let task = provision(&state, &root, id)?;
    // A program inside the workspace runs from the task snapshot, not from the live workspace.
    let program = launch.program;
    let program = program
        .strip_prefix(&root)
        .map_or_else(|_| program.clone(), |p| task.worktree.join(p));
    let (repository, lease, signed, authority) =
        start_task(&state, task_spec(&task.task_id, &root, task.checkpoint))?;
    let runner =
        sandbox_runner::native_sandbox(Arc::new(TracingAuditSink)).map_err(|e| e.to_string())?;
    let engine = context_engine::SqliteWalContextEngine::open_or_rebuild_from_workspace(
        &task.files.context_db,
        &task.worktree,
        "acp-session",
    )
    .map_err(|e| e.to_string())?;
    let mcp = context_engine::LocalMcpServer::new(
        engine,
        context_engine::ContextBundleAssembler::new(vec![AuthorizedDestination::LocalOnly]),
    );
    let text = HostText {
        root,
        bridge: state.bridge.clone(),
        mcp: Some(Mutex::new(mcp)),
        _task: Some(task.files),
    };
    let host = AcpHost {
        gate: gate.clone(),
        worktree_root: task.worktree,
        repository,
        lease,
        signed,
        authority,
        runner: Arc::from(runner),
        text: Arc::new(text),
        clock: Arc::new(now),
        events: Arc::new(move |event| {
            let _ = events.send(event);
        }),
    };
    let launch = AcpLaunch {
        program,
        args: launch.args,
        read_only_paths: launch.read_only_paths,
        credentials: AgentCredentials::Unauthenticated,
        network: AgentNetwork::Offline,
        limits: ResourceLimits {
            wall_timeout: SESSION_LIFETIME,
            max_cpu_time: Some(CPU_ALLOWANCE),
            ..ResourceLimits::default()
        },
    };
    let supervisor = AcpSessionSupervisor::start(host, launch).map_err(|e| e.to_string())?;
    // Workspace switches/revocations close this same gate. Recheck while holding the map,
    // so cleanup either sees this session or this insertion observes the closed gate.
    let mut agents = state.agents.lock().map_err(|_| "agents lock poisoned")?;
    gate.authorize(GatedCapability::WorkspaceLocalBinary)
        .map_err(|e| e.to_string())?;
    agents.insert(id, Arc::new(Mutex::new(supervisor)));
    Ok(id)
}

#[tauri::command]
pub async fn agent_start_command(
    app: tauri::AppHandle,
    installed_agent_id: String,
    events: Channel<AcpEvent>,
) -> Result<u32, String> {
    tauri::async_runtime::spawn_blocking(move || start_session(&app, installed_agent_id, events))
        .await
        .map_err(|e| e.to_string())?
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

/// Cancels the current turn; the session stays open for the next prompt.
#[tauri::command]
pub fn agent_cancel_command(
    state: tauri::State<'_, DesktopAppState>,
    id: u32,
) -> Result<(), String> {
    agent(&state, id)?
        .lock()
        .map_err(|_| "agent lock poisoned")?
        .cancel()
        .map_err(|e| e.to_string())
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

    fn host_text(root: &Path, bridge: DesktopBinaryEditorBridge) -> HostText {
        HostText {
            root: root.canonicalize().unwrap(),
            bridge: Arc::new(Mutex::new(bridge)),
            mcp: None,
            _task: None,
        }
    }

    #[test]
    fn untrusted_host_cannot_access_agents() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let state = crate::state::test_state(data.path());
        crate::commands::workspace::load_workspace(&state, root.path().to_str().unwrap()).unwrap();
        assert!(gate(&state).is_err());
        assert!(agent(&state, 1).is_err());
        assert!(resolve_launch(&state, "codex").is_err());
    }

    #[test]
    fn installed_ids_are_resolved_by_the_host_after_the_trust_gate() {
        let data = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let state = crate::state::test_state(data.path());
        crate::commands::workspace::load_workspace(&state, root.path().to_str().unwrap()).unwrap();
        state
            .trust_gate
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .grant()
            .unwrap();
        assert!(resolve_launch(&state, "missing")
            .unwrap_err()
            .contains("no installation"));
        assert!(resolve_launch(&state, "goose")
            .unwrap_err()
            .contains("blocked"));
        let location = data.path().join("agent-registry/packages/codex/1.0");
        std::fs::create_dir_all(&location).unwrap();
        let installation = agent_registry::Installation {
            id: "codex@1.0".into(),
            agent_id: "codex".into(),
            name: "Codex".into(),
            version: "1.0".into(),
            distribution: "binary".into(),
            installed_at_ms: 0,
            location: location.clone(),
            launch: agent_registry::Launch::Executable {
                path: "agent".into(),
                args: vec!["--acp".into()],
            },
        };
        std::fs::write(
            data.path().join("agent-registry/installations.json"),
            serde_json::to_vec(&vec![installation]).unwrap(),
        )
        .unwrap();
        let launch = resolve_launch(&state, "codex").unwrap();
        assert_eq!(launch.program, location.join("agent"));
        assert_eq!(launch.args, ["--acp"]);
        assert_eq!(launch.read_only_paths, [location]);
        assert_eq!(
            (launch.name.as_str(), launch.version.as_str()),
            ("Codex", "1.0")
        );
        state
            .trust_gate
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .revoke()
            .unwrap();
        assert!(resolve_launch(&state, "codex").is_err());
    }

    #[test]
    fn proposal_base_reads_unsaved_rope() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.rs");
        std::fs::write(&path, "disk").unwrap();
        let mut bridge = DesktopBinaryEditorBridge::new_in_memory();
        bridge
            .documents_mut()
            .open_document_with_editor_text(
                "file.rs",
                1,
                path.canonicalize().unwrap(),
                "unsaved 🦀",
            )
            .unwrap();
        assert_eq!(
            host_text(root.path(), bridge).text("file.rs").unwrap(),
            "unsaved 🦀"
        );
    }

    #[test]
    fn reading_a_closed_file_takes_the_disk_and_opens_no_document() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("closed.rs"), "on disk").unwrap();
        std::fs::write(root.path().join("big.rs"), "x".repeat(MAX_FILE_BYTES + 1)).unwrap();
        let text = host_text(root.path(), DesktopBinaryEditorBridge::new_in_memory());
        assert_eq!(text.text("closed.rs").unwrap(), "on disk");
        assert!(text.text("big.rs").unwrap_err().contains("size"));
        let bridge = text.bridge.lock().unwrap();
        assert_eq!(bridge.document_service().sessions().count(), 0);
    }

    #[test]
    fn launch_arguments_are_bounded() {
        // Absolute on every OS: `/bin/agent` has no drive on Windows.
        let program = if cfg!(windows) {
            r"C:\agent.exe"
        } else {
            "/bin/agent"
        };
        assert!(check_launch(program, &[]).is_ok());
        assert!(check_launch("agent", &[]).is_err());
        assert!(check_launch(program, &vec![String::new(); MAX_LAUNCH_ARGS + 1]).is_err());
        assert!(check_launch(program, &["x".repeat(MAX_LAUNCH_BYTES)]).is_err());
    }
}
