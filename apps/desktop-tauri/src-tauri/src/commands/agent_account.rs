//! Host-side account configuration. The webview never receives provider keys or session tokens.
use crate::state::DesktopAppState;
use agent_gateway::{AgentGateway, GatewayMode};
use agent_registry::{
    accounts::{AccountMode, AccountPreferences},
    private_home::PrivateAgentHome,
    profiles::{agent_profile, provider_profile},
    LaunchCommand,
};
use judge_orchestrator::acp::{AgentCredentials, AgentNetwork};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};
use tauri::{Emitter, Manager};
use zeroize::Zeroizing;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAccountStatus {
    id: String,
    name: String,
    key_saved: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccountStatus {
    preferences: AccountPreferences,
    providers: Vec<ProviderAccountStatus>,
    auth_methods: Vec<serde_json::Value>,
    known_profile: bool,
    runtime_notes: Vec<String>,
}

#[tauri::command]
pub async fn agent_account_status_command(
    app: tauri::AppHandle,
    agent_id: String,
) -> Result<AgentAccountStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<DesktopAppState>();
        let registry = super::agent_registry::registry(&state)?;
        let launch = registry
            .launch_command(&agent_id)
            .map_err(|e| e.to_string())?;
        let account = registry
            .account(&launch.installation_id)
            .map_err(|e| e.to_string())?;
        let profile = agent_profile(&agent_id);
        let runtime_notes = profile
            .runtimes
            .iter()
            .filter(|runtime| {
                runtime.adapter_version == launch.version
                    && Some(runtime.target.as_str()) == agent_registry::platform_target().as_deref()
            })
            .map(|runtime| runtime.reason.clone())
            .collect();
        let providers = profile
            .providers
            .iter()
            .map(|id| {
                let provider = provider_profile(id).ok_or("Invalid provider profile")?;
                Ok(ProviderAccountStatus {
                    id: id.clone(),
                    name: provider.name.clone(),
                    key_saved: agent_gateway::credentials::read_key(id)?.is_some(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(AgentAccountStatus {
            preferences: account.preferences,
            providers,
            auth_methods: account.auth_methods,
            known_profile: !profile.providers.is_empty(),
            runtime_notes,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn agent_provider_key_command(
    app: tauri::AppHandle,
    provider: String,
    key: Option<String>,
) -> Result<(), String> {
    // Wrap immediately: no logs, state snapshots, return payloads, or file persistence.
    let key = key.map(Zeroizing::new);
    tauri::async_runtime::spawn_blocking(move || {
        match key {
            Some(key) => agent_gateway::credentials::store_key(&provider, key)?,
            None => agent_gateway::credentials::remove_key(&provider)?,
        }
        let _ = app.emit("agent-registry-changed", ());
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn agent_account_save_command(
    app: tauri::AppHandle,
    agent_id: String,
    preferences: AccountPreferences,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<DesktopAppState>();
        let _lock = state
            .agent_account_lock
            .lock()
            .map_err(|_| "account lock poisoned")?;
        let registry = super::agent_registry::registry(&state)?;
        let launch = registry
            .launch_command(&agent_id)
            .map_err(|e| e.to_string())?;
        if preferences
            .extra_allowed_hosts
            .iter()
            .any(|host| !agent_gateway::network::valid_host(host))
        {
            return Err(
                "Allowed hosts must be exact public DNS names, without URLs, ports or wildcards"
                    .into(),
            );
        }
        if !agent_profile(&agent_id).providers.is_empty()
            && !preferences.extra_allowed_hosts.is_empty()
        {
            return Err("Known agents use their provider's reviewed host list".into());
        }
        registry
            .save_account_preferences(&launch.installation_id, preferences)
            .map_err(|e| e.to_string())?;
        let _ = app.emit("agent-registry-changed", ());
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn agent_account_sign_out_command(
    app: tauri::AppHandle,
    agent_id: String,
) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<DesktopAppState>();
        let _lock = state
            .agent_account_lock
            .lock()
            .map_err(|_| "account lock poisoned")?;
        let registry = super::agent_registry::registry(&state)?;
        let launch = registry
            .launch_command(&agent_id)
            .map_err(|e| e.to_string())?;
        // Stop and join every session using this home before removing account tokens.
        let mut agents = state.agents.lock().map_err(|_| "agents lock poisoned")?;
        let ids = agents
            .iter()
            .filter_map(|(id, agent)| {
                agent
                    .lock()
                    .ok()
                    .filter(|agent| agent.identity() == launch.installation_id)
                    .map(|_| *id)
            })
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(agent) = agents.remove(&id) {
                agent.lock().map_err(|_| "agent lock poisoned")?.stop();
            }
        }
        drop(agents);
        PrivateAgentHome::clear(&state.app_data_dir, &launch.installation_id)
            .map_err(|e| e.to_string())?;
        let mut preferences = registry
            .account(&launch.installation_id)
            .map_err(|e| e.to_string())?
            .preferences;
        // Back to automatic: the next connection asks to sign in again, instead of staying offline.
        preferences.mode = AccountMode::Auto;
        preferences.method_id = None;
        registry
            .save_account_preferences(&launch.installation_id, preferences)
            .map_err(|e| e.to_string())?;
        let _ = app.emit("agent-registry-changed", ());
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The system certificate bundle, readable inside the macOS sandbox.
#[cfg(target_os = "macos")]
const MACOS_CA_BUNDLE: &str = "/private/etc/ssl/cert.pem";

pub(super) struct AgentRuntime {
    pub home: PathBuf,
    pub preference_domains: Vec<String>,
    pub environment: BTreeMap<OsString, OsString>,
    pub credentials: AgentCredentials,
    pub network: AgentNetwork,
    pub gateway: Option<AgentGateway>,
}

pub(super) fn runtime(
    app_data: &Path,
    agent_id: &str,
    launch: &LaunchCommand,
    preferences: &AccountPreferences,
    account_authenticated: bool,
) -> Result<AgentRuntime, String> {
    runtime_with_key_reader(
        app_data,
        agent_id,
        launch,
        preferences,
        account_authenticated,
        agent_gateway::credentials::read_key,
    )
}

pub(super) fn runtime_with_key_reader(
    app_data: &Path,
    agent_id: &str,
    launch: &LaunchCommand,
    preferences: &AccountPreferences,
    account_authenticated: bool,
    read_key: impl Fn(&str) -> Result<Option<Zeroizing<String>>, String>,
) -> Result<AgentRuntime, String> {
    let profile = agent_profile(agent_id);
    let home = PrivateAgentHome::open(app_data, &launch.installation_id, &profile)
        .map_err(|e| e.to_string())?;
    let mut environment = home.environment().clone();
    let mut paths: Vec<PathBuf> = launch
        .program
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .collect();
    #[cfg(unix)]
    paths.extend(
        [
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
            "/usr/local/bin",
            "/opt/homebrew/bin",
        ]
        .map(PathBuf::from),
    );
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        paths.push(PathBuf::from(root).join("System32"));
    }
    environment.insert(
        "PATH".into(),
        std::env::join_paths(paths).map_err(|_| "Invalid toolchain path")?,
    );
    let provider_id = preferences
        .provider
        .as_ref()
        .or_else(|| profile.providers.first());
    let provider = provider_id.and_then(|id| provider_profile(id));
    let key = if matches!(preferences.mode, AccountMode::Auto | AccountMode::ApiKey) {
        match provider {
            Some(provider) => read_key(&provider.id)?,
            None => None,
        }
    } else {
        None
    };
    let (credentials, gateway) = match preferences.mode {
        AccountMode::Offline => (AgentCredentials::Unauthenticated, None),
        AccountMode::ApiKey | AccountMode::Auto if key.is_some() => {
            let provider = provider.ok_or("Choose an approved provider in Settings")?;
            let gateway = AgentGateway::start(GatewayMode::Key {
                provider: provider.clone(),
                key: key.ok_or("Provider key is missing")?,
            })
            .map_err(|e| e.to_string())?;
            let endpoint = gateway.endpoint();
            let fallback = match (
                profile.base_url_variables.get(&provider.id),
                profile.token_variables.get(&provider.id),
            ) {
                (Some(base), Some(token)) => {
                    for variable in base {
                        environment.insert(variable.into(), endpoint.base_url().into());
                    }
                    for variable in token {
                        environment.insert(variable.into(), endpoint.token.clone().into());
                    }
                    true
                }
                _ => false,
            };
            let credentials = AgentCredentials::Gateway {
                protocol: provider.protocol.clone(),
                base_url: endpoint.base_url(),
                headers: BTreeMap::from([("x-coloride-session".into(), endpoint.token.clone())]),
                provider_name: provider.name.clone(),
                environment_fallback: fallback,
            };
            (credentials, Some(gateway))
        }
        AccountMode::ApiKey => {
            return Err("Save a provider API key in Settings before starting this agent".into())
        }
        AccountMode::Auto | AccountMode::Account => {
            let mut hosts = provider
                .map(|provider| provider.account_hosts.clone())
                .unwrap_or_default();
            hosts.extend(preferences.extra_allowed_hosts.iter().cloned());
            hosts.sort();
            hosts.dedup();
            if hosts.is_empty() {
                return Err("Network is denied for this unknown agent: its sign-in hosts have not been reviewed".into());
            }
            let gateway = AgentGateway::start(GatewayMode::Account {
                allowed_hosts: hosts.clone(),
            })
            .map_err(|e| e.to_string())?;
            for key in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "ALL_PROXY",
                "http_proxy",
                "https_proxy",
                "all_proxy",
            ] {
                environment.insert(key.into(), gateway.endpoint().proxy_url().into());
            }
            // Explicitly clear proxy bypass lists. The OS still denies direct network access.
            environment.insert("NO_PROXY".into(), "".into());
            environment.insert("no_proxy".into(), "".into());
            // macOS: TLS clients that use the platform verifier need trustd and securityd (the
            // keychain daemon), which the sandbox does not expose. Point them at the system's
            // certificate bundle instead (readable in the sandbox): no keychain access needed.
            #[cfg(target_os = "macos")]
            for key in ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE"] {
                environment.insert(key.into(), MACOS_CA_BUNDLE.into());
            }
            (
                AgentCredentials::Account {
                    use_stored_credentials: account_authenticated,
                    method_id: preferences.method_id.clone(),
                    preferred_methods: profile.account_methods.clone(),
                    browser_hosts: hosts,
                },
                Some(gateway),
            )
        }
    };
    let network = gateway
        .as_ref()
        .map_or(AgentNetwork::Offline, |gateway| AgentNetwork::Proxy {
            address: gateway.endpoint().address,
        });
    Ok(AgentRuntime {
        home: home.path().into(),
        preference_domains: profile.preference_domains,
        environment,
        credentials,
        network,
        gateway,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn launch() -> LaunchCommand {
        LaunchCommand {
            installation_id: "agent@1".into(),
            name: "Agent".into(),
            version: "1".into(),
            program: std::env::current_exe().unwrap(),
            args: vec![],
            read_only_paths: vec![],
        }
    }
    fn assert_home_has_no_key(root: &Path, key: &str) {
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                assert_home_has_no_key(&path, key);
            } else {
                assert!(!String::from_utf8_lossy(&std::fs::read(path).unwrap()).contains(key));
            }
        }
    }
    #[test]
    fn real_provider_key_is_absent_from_both_agent_environment_and_private_home() {
        let app = tempfile::tempdir().unwrap();
        let preferences = AccountPreferences {
            mode: AccountMode::ApiKey,
            ..Default::default()
        };
        for agent in ["codex-acp", "claude-acp"] {
            let runtime =
                runtime_with_key_reader(app.path(), agent, &launch(), &preferences, false, |_| {
                    Ok(Some(Zeroizing::new("fake-real-provider-key".into())))
                })
                .unwrap();
            assert!(runtime
                .environment
                .values()
                .all(|value| !value.to_string_lossy().contains("fake-real-provider-key")));
            assert_home_has_no_key(&runtime.home, "fake-real-provider-key");
            assert!(matches!(runtime.network, AgentNetwork::Proxy { .. }));
            if agent == "claude-acp" {
                assert_ne!(
                    runtime
                        .environment
                        .get(std::ffi::OsStr::new("ANTHROPIC_AUTH_TOKEN"))
                        .unwrap(),
                    "fake-real-provider-key"
                );
            }
            drop(runtime);
            // Each installation has its own setup in production; isolate the two fixture homes.
            PrivateAgentHome::clear(app.path(), "agent@1").unwrap();
        }
    }
    #[test]
    fn unknown_agents_never_receive_network_without_explicit_host_permissions() {
        let app = tempfile::tempdir().unwrap();
        let result = runtime_with_key_reader(
            app.path(),
            "unknown",
            &launch(),
            &AccountPreferences::default(),
            false,
            |_| panic!("unknown agents have no provider key"),
        );
        assert!(result.err().unwrap().contains("Network is denied"));
        let runtime = runtime_with_key_reader(
            app.path(),
            "unknown",
            &launch(),
            &AccountPreferences {
                mode: AccountMode::Offline,
                ..Default::default()
            },
            false,
            |_| panic!("offline mode must not read keys"),
        )
        .unwrap();
        assert!(matches!(runtime.network, AgentNetwork::Offline));
        assert!(runtime.gateway.is_none());
    }
}
