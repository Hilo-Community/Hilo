//! The public ACP catalog: what it offers, how each agent is distributed, and why one cannot be
//! installed here.

use serde::Serialize;
use serde_json::Value;

use crate::archive;
use crate::error::RegistryError;
use crate::node::NodeStatus;

/// The only place the catalog is read from.
pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// Agents Hilo does not offer, whatever the catalog says.
pub const HIDDEN_AGENT_IDS: &[&str] = &[
    "agoragentic-acp",
    "amp-acp",
    "auggie",
    "autohand",
    "codebuddy-code",
    "cortex-code",
    "corust-agent",
    "crow-cli",
    "deepagents",
    "devin",
    "dimcode",
    "dirac",
    "fast-agent",
    "gemini",
    "glm-acp-agent",
    "goose",
    "harn",
    "junie",
    "kimchi",
    "minion-code",
    "nova",
    "opencode-v2",
    "pi-acp",
    "poolside",
    "qoder",
    "sigit",
    "stakpak",
    "vtcode",
];

pub fn is_offered(agent_id: &str) -> bool {
    !HIDDEN_AGENT_IDS.contains(&agent_id)
}

/// `darwin-aarch64`, `linux-x86_64`, `windows-x86_64`...: the key of a binary build in the catalog.
pub fn platform_target() -> Option<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        "windows" => "windows",
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => return None,
    };
    Some(format!("{os}-{arch}"))
}

/// How an agent is obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// An archive of a ready-to-run program, verified against the catalog's SHA-256.
    Binary {
        archive: String,
        sha256: Option<String>,
        command: String,
        args: Vec<String>,
    },
    /// An exact npm package, run with the Node.js of this computer.
    Npm { package: String, args: Vec<String> },
    /// A Python package, run with `uv`. Not supported.
    Uv { package: String },
}

impl Source {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Binary { .. } => "binary",
            Self::Npm { .. } => "npm",
            Self::Uv { .. } => "uv",
        }
    }

    /// What makes this source unusable whatever the machine: a download that cannot be verified
    /// is refused, not installed with a warning.
    fn problem(&self) -> Option<String> {
        match self {
            Self::Binary {
                archive: url,
                sha256,
                command,
                ..
            } => {
                if !url.starts_with("https://") {
                    return Some("The download is not served over HTTPS.".into());
                }
                if !sha256.as_deref().is_some_and(is_sha256) {
                    return Some(
                        "The registry publishes no SHA-256 for this download, so it cannot be verified."
                            .into(),
                    );
                }
                if archive::format_of(url).is_none() {
                    return Some(
                        "The download format is not supported (only .zip and .tar.gz).".into(),
                    );
                }
                if archive::safe_relative(command).is_none() {
                    return Some("The registry gives an unsafe command path.".into());
                }
                None
            }
            Self::Npm { package, .. } => parse_exact_package(package)
                .is_none()
                .then(|| "The npm package is not pinned to an exact version.".into()),
            Self::Uv { .. } => Some("Agents distributed through uv are not supported yet.".into()),
        }
    }
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `@scope/name@1.2.3` or `name@1.2.3-beta.1`: the name and the exact version. Ranges, tags
/// (`latest`) and URLs are not exact and give `None`.
pub(crate) fn parse_exact_package(spec: &str) -> Option<(&str, &str)> {
    let at = spec.rfind('@').filter(|at| *at > 0)?;
    let (name, version) = (&spec[..at], &spec[at + 1..]);
    let bare = match name.strip_prefix('@') {
        Some(scoped) => {
            let (scope, rest) = scoped.split_once('/')?;
            if !npm_name_part(scope) {
                return None;
            }
            rest
        }
        None => name,
    };
    if !npm_name_part(bare) {
        return None;
    }
    let (core, rest) = match version.find(['-', '+']) {
        Some(index) => (&version[..index], &version[index..]),
        None => (version, ""),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    let numeric = numbers.len() == 3
        && numbers
            .iter()
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let suffix_ok = rest
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'));
    (numeric && suffix_ok).then_some((name, version))
}

fn npm_name_part(part: &str) -> bool {
    let mut bytes = part.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn identifier(text: &str, extra: &[u8]) -> bool {
    let mut bytes = text.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
}

/// One agent of the catalog, as this computer would install it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub repository: Option<String>,
    pub website: Option<String>,
    pub license: Option<String>,
    pub license_url: Option<String>,
    pub source: Source,
}

impl CatalogEntry {
    /// Why this agent cannot be installed on this computer right now, if it cannot.
    pub fn problem(&self, node: &NodeStatus) -> Option<String> {
        if let Some(problem) = self.source.problem() {
            return Some(problem);
        }
        match (&self.source, node) {
            (Source::Npm { .. }, NodeStatus::Missing(reason)) => Some(reason.clone()),
            _ => None,
        }
    }

    pub fn card(&self, node: &NodeStatus) -> AgentCard {
        AgentCard {
            id: self.id.clone(),
            name: self.name.clone(),
            version: self.version.clone(),
            description: self.description.clone(),
            repository: self.repository.clone(),
            website: self.website.clone(),
            license: self.license.clone(),
            license_url: self.license_url.clone(),
            distribution: self.source.label(),
            blocked: self.problem(node),
        }
    }
}

/// What the Settings screen shows for one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub repository: Option<String>,
    pub website: Option<String>,
    pub license: Option<String>,
    pub license_url: Option<String>,
    pub distribution: &'static str,
    /// Set when the agent cannot be installed here, with the reason.
    pub blocked: Option<String>,
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The sources an entry offers for `target`, best first: a verified binary needs nothing else on
/// the computer, npm needs Node.js, uv is not supported.
fn sources(distribution: &Value, target: Option<&str>) -> Vec<Source> {
    let mut found = Vec::new();
    if let Some(build) = target.and_then(|target| distribution["binary"].get(target)) {
        if let (Some(archive), Some(command)) = (text(&build["archive"]), text(&build["cmd"])) {
            found.push(Source::Binary {
                archive,
                sha256: text(&build["sha256"]).map(|digest| digest.to_ascii_lowercase()),
                command,
                args: strings(&build["args"]),
            });
        }
    }
    if let Some(package) = text(&distribution["npx"]["package"]) {
        found.push(Source::Npm {
            package,
            args: strings(&distribution["npx"]["args"]),
        });
    }
    if let Some(package) = text(&distribution["uvx"]["package"]) {
        found.push(Source::Uv { package });
    }
    found
}

/// The agents of a catalog payload that this computer could be offered. Malformed entries and
/// builds for other platforms are left out; hidden agents are not filtered here.
pub fn parse_catalog(
    payload: &Value,
    target: Option<&str>,
) -> Result<Vec<CatalogEntry>, RegistryError> {
    let agents = payload["agents"]
        .as_array()
        .ok_or_else(|| RegistryError::BadCatalog("there is no list of agents".into()))?;
    let mut entries = Vec::new();
    for raw in agents {
        let (Some(id), Some(version)) = (text(&raw["id"]), text(&raw["version"])) else {
            continue;
        };
        if !identifier(&id, b"._-") || !identifier(&version, b"._+-") {
            continue;
        }
        let candidates = sources(&raw["distribution"], target);
        let source = candidates
            .iter()
            .find(|source| source.problem().is_none())
            .or_else(|| candidates.first())
            .cloned();
        let Some(source) = source else { continue };
        entries.push(CatalogEntry {
            name: text(&raw["name"]).unwrap_or_else(|| id.clone()),
            id,
            version,
            description: text(&raw["description"]).unwrap_or_default(),
            repository: text(&raw["repository"]),
            website: text(&raw["website"]),
            license: text(&raw["license"]),
            license_url: text(&raw["license_url"]),
            source,
        });
    }
    Ok(entries)
}

/// What the user is asked to confirm before an install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub name: String,
    pub version: String,
    /// Where the code comes from and how it is checked, in one sentence.
    pub summary: String,
}

impl CatalogEntry {
    pub fn plan(&self) -> InstallPlan {
        let summary = match &self.source {
            Source::Binary { archive, .. } => {
                format!("Downloads {archive} and checks its SHA-256 against the registry before using it.")
            }
            Source::Npm { package, .. } => format!(
                "Installs the npm package {package} with this computer's Node.js. Its install scripts are not run."
            ),
            Source::Uv { package } => format!("Installs the Python package {package}."),
        };
        InstallPlan {
            name: self.name.clone(),
            version: self.version.clone(),
            summary,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const SHA: &str = "ba106984d2d89b4e260ac0648e354c924a1a1545392929b1e27700615d7ba9b3";

    fn binary(extra: Value) -> Value {
        let mut build = json!({
            "archive": "https://example.org/agent-darwin.zip",
            "cmd": "./agent",
            "sha256": SHA,
        });
        for (key, value) in extra.as_object().cloned().unwrap_or_default() {
            build[key] = value;
        }
        json!({"binary": {"darwin-aarch64": build}})
    }

    fn catalog(distribution: Value) -> Vec<CatalogEntry> {
        let payload = json!({"agents": [{
            "id": "agent", "name": "Agent", "version": "1.2.3", "distribution": distribution,
        }]});
        parse_catalog(&payload, Some("darwin-aarch64")).unwrap()
    }

    #[test]
    fn exact_npm_versions_are_told_from_ranges_and_tags() {
        assert_eq!(
            parse_exact_package("@agentclientprotocol/codex-acp@2.6.0"),
            Some(("@agentclientprotocol/codex-acp", "2.6.0"))
        );
        assert_eq!(
            parse_exact_package("tool@1.0.0-beta.1+build.5"),
            Some(("tool", "1.0.0-beta.1+build.5"))
        );
        for spec in [
            "tool",
            "tool@latest",
            "tool@^1.2.3",
            "tool@1.2",
            "tool@1.2.x",
            "@scope@1.2.3",
            "@/x@1.2.3",
            "../evil@1.2.3",
            "tool@1.2.3 --registry=http://evil",
            "git+https://example.org/x.git@1.2.3",
            "@1.2.3",
        ] {
            assert_eq!(parse_exact_package(spec), None, "{spec}");
        }
    }

    #[test]
    fn a_verified_binary_is_offered_for_this_platform_only() {
        let entries = catalog(binary(json!({})));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].problem(&NodeStatus::Ready), None);
        let payload =
            json!({"agents": [{"id": "a", "version": "1.0.0", "distribution": binary(json!({}))}]});
        assert!(parse_catalog(&payload, Some("linux-x86_64"))
            .unwrap()
            .is_empty());
        assert!(parse_catalog(&payload, None).unwrap().is_empty());
    }

    #[test]
    fn a_download_without_a_checksum_is_refused_not_installed_unverified() {
        for build in [
            json!({"sha256": null}),
            json!({"sha256": "abc"}),
            json!({"sha256": "z".repeat(64)}),
        ] {
            let entry = &catalog(binary(build))[0];
            assert!(entry
                .problem(&NodeStatus::Ready)
                .unwrap()
                .contains("SHA-256"));
        }
    }

    #[test]
    fn unusable_downloads_say_why() {
        let http = catalog(binary(json!({"archive": "http://example.org/a.zip"})));
        assert!(http[0]
            .problem(&NodeStatus::Ready)
            .unwrap()
            .contains("HTTPS"));
        let bz2 = catalog(binary(json!({"archive": "https://example.org/a.tar.bz2"})));
        assert!(bz2[0]
            .problem(&NodeStatus::Ready)
            .unwrap()
            .contains("format"));
        for command in ["../escape", "/bin/sh", "a/../../b"] {
            let unsafe_command = catalog(binary(json!({"cmd": command})));
            assert!(
                unsafe_command[0]
                    .problem(&NodeStatus::Ready)
                    .unwrap()
                    .contains("unsafe"),
                "{command}"
            );
        }
    }

    #[test]
    fn npm_agents_need_node_and_uv_agents_are_not_supported() {
        let npm = &catalog(json!({"npx": {"package": "tool@1.2.3", "args": ["--acp"]}}))[0];
        assert_eq!(npm.problem(&NodeStatus::Ready), None);
        let missing = NodeStatus::Missing("Node.js 22 or newer was not found.".into());
        assert_eq!(
            npm.problem(&missing).as_deref(),
            Some("Node.js 22 or newer was not found.")
        );
        let ranged = &catalog(json!({"npx": {"package": "tool@^1.2.3"}}))[0];
        assert!(ranged
            .problem(&NodeStatus::Ready)
            .unwrap()
            .contains("exact"));
        let uv = &catalog(json!({"uvx": {"package": "tool@1.2.3"}}))[0];
        assert!(uv.problem(&NodeStatus::Ready).unwrap().contains("uv"));
    }

    #[test]
    fn a_verified_binary_is_preferred_to_npm_and_npm_stands_in_for_a_bad_one() {
        let both = |build: Value| {
            let mut distribution = binary(build);
            distribution["npx"] = json!({"package": "tool@1.2.3"});
            catalog(distribution)
        };
        assert!(matches!(both(json!({}))[0].source, Source::Binary { .. }));
        assert!(matches!(
            both(json!({"sha256": null}))[0].source,
            Source::Npm { .. }
        ));
    }

    #[test]
    fn malformed_entries_are_skipped_and_a_bad_catalog_is_an_error() {
        let payload = json!({"agents": [
            {"id": "../evil", "version": "1.0.0", "distribution": {"npx": {"package": "x@1.0.0"}}},
            {"id": "ok", "version": "1.0.0 ", "distribution": {"npx": {"package": "x@1.0.0"}}},
            {"id": "no-version", "distribution": {"npx": {"package": "x@1.0.0"}}},
            {"id": "no-source", "version": "1.0.0", "distribution": {}},
            "nonsense",
        ]});
        let entries = parse_catalog(&payload, None).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "ok");
        assert!(parse_catalog(&json!({"agents": "none"}), None).is_err());
        assert!(parse_catalog(&json!([]), None).is_err());
    }

    #[test]
    fn hidden_agents_are_not_offered() {
        assert!(!is_offered("gemini"));
        assert!(is_offered("codex-acp"));
    }
}
