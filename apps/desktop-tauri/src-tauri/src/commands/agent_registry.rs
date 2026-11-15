//! Settings: the ACP agent registry. The webview asks; the Host downloads and installs, and a
//! native dialog (which the webview cannot answer) confirms before anything is fetched.
use std::sync::Arc;

use agent_registry::{AgentRegistry, InstallPlan, InstalledAgent, Listing};
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

use crate::state::DesktopAppState;

/// Where the registry keeps its catalog cache, installation list and agent folders.
const REGISTRY_DIR: &str = "agent-registry";

/// The registry, opened the first time Settings needs it.
fn registry(state: &DesktopAppState) -> Result<Arc<AgentRegistry>, String> {
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

fn confirmation_text(plan: &InstallPlan) -> String {
    format!(
        "Install {} {}?\n\n{}\n\nThe agent is kept in Coloride's own folder. It does not run now: \
         when started it runs in the sandbox, offline and without your credentials.",
        plan.name, plan.version, plan.summary
    )
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

/// Installs `agent_id` at exactly `version`, the one the user saw. `None` when the user
/// declined the native confirmation.
#[tauri::command]
pub async fn agent_registry_install_command(
    app: tauri::AppHandle,
    agent_id: String,
    version: String,
) -> Result<Option<InstalledAgent>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let registry = registry(&app.state::<DesktopAppState>())?;
        // Refused here, before any dialog, if the agent is hidden, blocked or has changed.
        let plan = registry
            .plan(&agent_id, &version)
            .map_err(|e| e.to_string())?;
        let confirmed = app
            .dialog()
            .message(confirmation_text(&plan))
            .title("Install ACP agent")
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Install".into(),
                "Cancel".into(),
            ))
            .blocking_show();
        if !confirmed {
            return Ok(None);
        }
        registry
            .install(&agent_id, &version)
            .map(Some)
            .map_err(|e| e.to_string())
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
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
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

    #[test]
    fn the_confirmation_names_the_agent_its_version_and_where_the_code_comes_from() {
        let text = confirmation_text(&InstallPlan {
            name: "Codex".into(),
            version: "2.6.0".into(),
            summary: "Installs the npm package codex-acp@2.6.0.".into(),
        });
        for expected in ["Codex 2.6.0", "codex-acp@2.6.0", "sandbox", "offline"] {
            assert!(text.contains(expected), "{expected} in {text}");
        }
    }
}
