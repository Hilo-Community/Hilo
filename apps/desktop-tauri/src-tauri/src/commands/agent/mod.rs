//! ACP Host entry points: the Tauri commands of the agent panel. Native dialogs provide the
//! human decision; signing stays in Rust. Starting a session lives in `session_start`, the
//! sign-in pages the Host opens in `login_pages`.
mod login_pages;
mod session_start;
#[cfg(all(test, target_os = "macos"))]
mod smoke_test;

use crate::state::DesktopAppState;
use host_core::*;
use judge_orchestrator::acp::*;
use session_start::start_session;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
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
        let state = app.state::<DesktopAppState>();
        if let Ok(mut urls) = state.login_urls.lock() {
            urls.remove(&id);
        }
        let agent = state
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

/// Opens only a URL retained from this agent's pending, allowlisted elicitation. The request
/// carries an opaque request ID, never a URL supplied by the webview. `accept` opens the page
/// and answers the agent; `open` only opens it again while the agent still waits.
#[tauri::command]
pub async fn agent_elicitation_command(
    app: tauri::AppHandle,
    id: u32,
    request_id: String,
    action: String,
) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let agent = agent(&app.state::<DesktopAppState>(), id)?;
    let agent = agent.lock().map_err(|_| "agent lock poisoned")?;
    if action == "accept" || action == "open" {
        let url = agent
            .elicitation_url(&request_id)
            .ok_or("Authentication request is no longer pending")?;
        app.opener()
            .open_url(url, None::<&str>)
            .map_err(|_| "Could not open the authentication URL")?;
    }
    if action == "open" {
        return Ok(());
    }
    agent
        .answer_elicitation(request_id, action)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn agent_terminal_auth_start_command(
    state: tauri::State<'_, DesktopAppState>,
    id: u32,
) -> Result<(), String> {
    agent(&state, id)?
        .lock()
        .map_err(|_| "agent lock poisoned")?
        .start_terminal_auth()
        .map_err(|e| e.to_string())
}
/// The user's pick among the sign-in methods the agent offered.
#[tauri::command]
pub fn agent_auth_choose_command(
    state: tauri::State<'_, DesktopAppState>,
    id: u32,
    method_id: String,
) -> Result<(), String> {
    agent(&state, id)?
        .lock()
        .map_err(|_| "agent lock poisoned")?
        .choose_authentication(method_id)
        .map_err(|e| e.to_string())
}
/// The user's pick for a model, mode or effort the agent offered. The session only sends it
/// if the value is one the agent itself advertised.
#[tauri::command]
pub fn agent_option_command(
    state: tauri::State<'_, DesktopAppState>,
    id: u32,
    option_id: String,
    value: String,
) -> Result<(), String> {
    agent(&state, id)?
        .lock()
        .map_err(|_| "agent lock poisoned")?
        .set_option(option_id, value)
        .map_err(|e| e.to_string())
}
/// Opens again the sign-in page the login printed, when the browser did not open or was
/// closed. Only the address the Host kept is opened, never one the webview supplies.
#[tauri::command]
pub fn agent_terminal_auth_open_command(app: tauri::AppHandle, id: u32) -> Result<(), String> {
    login_pages::open_again(&app, id)
}
#[tauri::command]
pub fn agent_terminal_auth_input_command(
    state: tauri::State<'_, DesktopAppState>,
    id: u32,
    data: String,
) -> Result<(), String> {
    agent(&state, id)?
        .lock()
        .map_err(|_| "agent lock poisoned")?
        .write_terminal_auth(data)
        .map_err(|e| e.to_string())
}
#[tauri::command]
pub fn agent_terminal_auth_resize_command(
    state: tauri::State<'_, DesktopAppState>,
    id: u32,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    agent(&state, id)?
        .lock()
        .map_err(|_| "agent lock poisoned")?
        .resize_terminal_auth(cols, rows)
        .map_err(|e| e.to_string())
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
    use super::session_start::*;
    use super::*;
    use crate::bridge::DesktopBinaryEditorBridge;
    use std::path::Path;

    fn host_text(root: &Path, bridge: DesktopBinaryEditorBridge) -> HostText {
        HostText {
            root: root.canonicalize().unwrap(),
            bridge: Arc::new(Mutex::new(bridge)),
            mcp: Mutex::new(None),
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
