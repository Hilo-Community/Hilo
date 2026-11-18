use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::catalog::AgentCard;

/// How an installed agent is started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Launch {
    /// Node.js runs `script`, a path under the installation's folder.
    Node { script: String, args: Vec<String> },
    /// A program, as a path under the installation's folder.
    Executable { path: String, args: Vec<String> },
}

/// An agent installed on this computer. This is what is saved to disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Installation {
    pub id: String,
    pub agent_id: String,
    pub name: String,
    pub version: String,
    pub distribution: String,
    pub installed_at_ms: u64,
    /// The folder holding the agent. Everything the agent needs is below it.
    pub location: PathBuf,
    pub launch: Launch,
}

/// What the Settings screen is told about an installation (no paths).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledAgent {
    pub id: String,
    pub agent_id: String,
    pub name: String,
    pub version: String,
    pub distribution: String,
}

impl From<&Installation> for InstalledAgent {
    fn from(installation: &Installation) -> Self {
        Self {
            id: installation.id.clone(),
            agent_id: installation.agent_id.clone(),
            name: installation.name.clone(),
            version: installation.version.clone(),
            distribution: installation.distribution.clone(),
        }
    }
}

/// Where the catalog on screen came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CatalogOrigin {
    /// Just downloaded.
    Network,
    /// Saved by an earlier visit: the registry could not be reached.
    Saved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub agents: Vec<AgentCard>,
    pub installed: Vec<InstalledAgent>,
    pub origin: CatalogOrigin,
    /// Why the registry could not be reached, when `origin` is `saved`.
    pub refresh_error: Option<String>,
}

/// A program to start an installed agent, and the folder it needs to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchCommand {
    pub installation_id: String,
    pub name: String,
    pub version: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub read_only_paths: Vec<PathBuf>,
}
