//! Reviewed, bundled agent knowledge. The ACP session layer never branches on agent names.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderProfile {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub upstream: String,
    pub key_header: String,
    pub key_prefix: String,
    /// Exact DNS names, never suffixes or wildcard patterns.
    pub account_hosts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetupFile {
    pub path: String,
    pub content: String,
}

/// A reviewed runtime needed by a particular adapter version on one platform.
/// Downloading and verification belong to the registry; session code sees only an env override.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeArtifact {
    pub id: String,
    pub adapter_version: String,
    pub version: String,
    pub target: String,
    pub url: String,
    pub sha512: String,
    pub program: String,
    pub variable: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentProfile {
    pub id: String,
    pub providers: Vec<String>,
    /// Environment variable -> subdirectory of the private home.
    pub home_variables: BTreeMap<String, String>,
    pub environment: BTreeMap<String, String>,
    pub setup: Vec<SetupFile>,
    /// Provider ID -> variable consumed by an agent without ACP gateway auth.
    pub base_url_variables: BTreeMap<String, Vec<String>>,
    pub token_variables: BTreeMap<String, Vec<String>>,
    /// Preference order only: a method must also be advertised at runtime.
    pub account_methods: Vec<String>,
    /// macOS preference domains the agent must read to start (through `cfprefsd`, read-only,
    /// exact names); see `SandboxSpec::preference_domains`.
    #[serde(default)]
    pub preference_domains: Vec<String>,
    /// Loopback ports the agent's browser sign-in listens on (macOS). Without a Host that can
    /// open the page and allow the port, the agent is told not to use a browser at all.
    #[serde(default)]
    pub login_callback_ports: Vec<u16>,
    #[serde(default)]
    pub runtimes: Vec<RuntimeArtifact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profiles {
    providers: Vec<ProviderProfile>,
    agents: Vec<AgentProfile>,
}

fn profiles() -> &'static Profiles {
    static PROFILES: OnceLock<Profiles> = OnceLock::new();
    PROFILES.get_or_init(|| {
        serde_json::from_str(include_str!("profiles.json"))
            .expect("bundled agent profiles must be valid")
    })
}

pub fn provider_profile(id: &str) -> Option<&'static ProviderProfile> {
    profiles().providers.iter().find(|profile| profile.id == id)
}

/// Unknown agents get an empty profile and therefore no upstream or login hosts.
pub fn agent_profile(id: &str) -> AgentProfile {
    profiles()
        .agents
        .iter()
        .find(|profile| profile.id == id)
        .cloned()
        .unwrap_or_else(|| AgentProfile {
            id: id.into(),
            ..AgentProfile::default()
        })
}

pub fn provider_profiles() -> &'static [ProviderProfile] {
    &profiles().providers
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeSet,
        path::{Component, Path},
    };

    #[test]
    fn unknown_agents_have_no_implicit_network_or_authentication() {
        let profile = agent_profile("unreviewed-agent");
        assert!(profile.providers.is_empty());
        assert!(profile.environment.is_empty());
        assert!(profile.account_methods.is_empty());
        assert!(profile.setup.is_empty());
    }

    #[test]
    fn bundled_profiles_reference_known_providers_and_contain_only_relative_setup() {
        let profiles = profiles();
        let mut ids = BTreeSet::new();
        for provider in &profiles.providers {
            assert!(ids.insert(&provider.id));
            assert!(provider.upstream.starts_with("https://"));
            assert!(!provider.account_hosts.is_empty());
            for host in &provider.account_hosts {
                assert!(host.contains('.'));
                assert!(host.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'.'
                    || b == b'-'));
            }
        }
        ids.clear();
        for agent in &profiles.agents {
            assert!(ids.insert(&agent.id));
            for runtime in &agent.runtimes {
                assert!(runtime.url.starts_with("https://registry.npmjs.org/"));
                assert_eq!(runtime.sha512.len(), 128);
                assert!(runtime.sha512.bytes().all(|b| b.is_ascii_hexdigit()));
                assert!(crate::archive::safe_relative(&runtime.program).is_some());
                assert!(crate::archive::safe_relative(&runtime.id).is_some());
                assert!(runtime
                    .variable
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b == b'_'));
            }
            for provider in &agent.providers {
                assert!(provider_profile(provider).is_some());
            }
            assert!(agent.login_callback_ports.len() <= 4);
            assert!(agent.login_callback_ports.iter().all(|port| *port >= 1024));
            for domain in &agent.preference_domains {
                assert!(domain.contains('.'));
                assert!(domain.split('.').all(|label| !label.is_empty()
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')));
            }
            for path in agent
                .home_variables
                .values()
                .chain(agent.setup.iter().map(|file| &file.path))
            {
                assert!(!path.is_empty());
                assert!(Path::new(path)
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))));
            }
            for provider in agent
                .base_url_variables
                .keys()
                .chain(agent.token_variables.keys())
            {
                assert!(agent.providers.contains(provider));
            }
        }
    }
}
