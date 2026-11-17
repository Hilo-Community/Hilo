//! Settings: the ACP agent registry. The webview asks; the Host checks the request against the
//! registry (offered, installable here, exact version), downloads, verifies and installs.
use std::sync::Arc;

use agent_registry::{AgentRegistry, InstalledAgent, Listing};
use tauri::{Emitter, Manager};

use crate::state::DesktopAppState;

/// Emitted when an install starts; the payload is the agent id.
const INSTALL_CONFIRMED_EVENT: &str = "agent-registry-install-confirmed";

/// Where the registry keeps its catalog cache, installation list and agent folders.
const REGISTRY_DIR: &str = "agent-registry";

/// The registry, opened the first time Settings needs it.
pub(super) fn registry(state: &DesktopAppState) -> Result<Arc<AgentRegistry>, String> {
    let mut slot = state
        .agent_registry
        .lock()
        .map_err(|_| "registry lock poisoned")?;
    if let Some(registry) = slot.as_ref() {
        return Ok(Arc::clone(registry));
    }
    let opened = Arc::new(
        AgentRegistry::open(state.app_data_dir.join(REGISTRY_DIR)).map_err(|e| e.to_string())?,
    );
    *slot = Some(Arc::clone(&opened));
    Ok(opened)
}

#[tauri::command]
pub async fn agent_registry_list_command(
    app: tauri::AppHandle,
    refresh: bool,
) -> Result<Listing, String> {
    tauri::async_runtime::spawn_blocking(move || {
        registry(&app.state::<DesktopAppState>())?
            .listing(refresh)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Installs `agent_id` at exactly `version`, the one the user saw. Always `Some` now that there
/// is no confirmation dialog; the `Option` stays so the webview contract does not change.
#[tauri::command]
pub async fn agent_registry_install_command(
    app: tauri::AppHandle,
    agent_id: String,
    version: String,
) -> Result<Option<InstalledAgent>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let registry = registry(&app.state::<DesktopAppState>())?;
        // Refused here if the agent is hidden, blocked on this computer or its version changed.
        registry
            .plan(&agent_id, &version)
            .map_err(|e| e.to_string())?;
        // Tells Settings the download has started.
        let _ = app.emit(INSTALL_CONFIRMED_EVENT, &agent_id);
        let installed = registry
            .install(&agent_id, &version)
            .map_err(|e| e.to_string())?;
        let _ = app.emit("agent-registry-changed", ());
        Ok(Some(installed))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn agent_registry_uninstall_command(
    app: tauri::AppHandle,
    installation_id: String,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        registry(&app.state::<DesktopAppState>())?
            .uninstall(&installation_id)
            .map_err(|e| e.to_string())?;
        let _ = app.emit("agent-registry-changed", ());
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Settings requests a panel; only the main window owns the chat stores and editor.
#[tauri::command]
pub fn agent_registry_open_command(app: tauri::AppHandle, agent_id: String) -> Result<(), String> {
    let installed = registry(&app.state::<DesktopAppState>())?
        .listing(false)
        .map_err(|e| e.to_string())?
        .installed
        .into_iter()
        .find(|agent| agent.agent_id == agent_id)
        .ok_or("The agent is no longer installed")?;
    let main = app
        .get_webview_window("main")
        .ok_or("Main window is unavailable")?;
    main.show().map_err(|e| e.to_string())?;
    main.unminimize().map_err(|e| e.to_string())?;
    main.set_focus().map_err(|e| e.to_string())?;
    app.emit_to("main", "agent-open-requested", installed)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_opens_once_inside_the_app_data_folder() {
        let data = tempfile::tempdir().unwrap();
        let state = crate::state::test_state(data.path());
        let first = registry(&state).unwrap();
        let second = registry(&state).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(data.path().join(REGISTRY_DIR).join("packages").is_dir());
    }
}
