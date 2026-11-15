//! The ACP agent registry of the Host: reads the public catalog, installs and removes agents.
//!
//! Installing runs on the Host, never in a sandbox, because it needs the network and writes the
//! agent's own folder. What keeps that safe:
//!
//! - the catalog is read over HTTPS from one address, with no redirect;
//! - a binary is installed only if the catalog publishes its SHA-256 and the download matches it;
//! - an npm package must be an exact version and is installed with `--ignore-scripts`: its code is
//!   fetched, none of it is run;
//! - archives are extracted by our own loop: names outside the folder, links, duplicates and
//!   oversized files are refused;
//! - a folder is deleted only if it is inside the registry's own `packages/`.
//!
//! Running an agent afterwards is not done here: it goes through `sandbox-runner` like any other
//! process (see `judge-orchestrator::acp`).

mod archive;
mod catalog;
mod error;
mod http;
mod installer;
mod model;
mod node;
mod process;
mod store;

use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::Value;

pub use catalog::{
    is_offered, parse_catalog, platform_target, AgentCard, CatalogEntry, InstallPlan, Source,
    HIDDEN_AGENT_IDS, REGISTRY_URL,
};
pub use error::RegistryError;
pub use http::{Fetch, HttpFetch, Redirects};
pub use model::{CatalogOrigin, Installation, InstalledAgent, Launch, LaunchCommand, Listing};
pub use node::{NodeLocator, NodeStatus, NodeToolchain, SystemNode, MIN_NODE_MAJOR};

use store::Store;

/// Largest catalog read.
const MAX_CATALOG_BYTES: u64 = 4 * 1024 * 1024;

/// What the registry reaches the outside world with. The default is the real one.
pub struct Parts {
    pub fetch: Arc<dyn Fetch>,
    pub node: Arc<dyn NodeLocator>,
    pub registry_url: String,
    /// The catalog key of binary builds for this computer.
    pub target: Option<String>,
}

impl Default for Parts {
    fn default() -> Self {
        Self {
            fetch: Arc::new(HttpFetch::default()),
            node: Arc::new(SystemNode),
            registry_url: REGISTRY_URL.into(),
            target: platform_target(),
        }
    }
}

pub struct AgentRegistry {
    store: Store,
    parts: Parts,
    /// One install or removal at a time.
    busy: Mutex<()>,
}

impl AgentRegistry {
    /// Keeps its files under `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, RegistryError> {
        Self::with_parts(root, Parts::default())
    }

    pub fn with_parts(root: impl Into<PathBuf>, parts: Parts) -> Result<Self, RegistryError> {
        Ok(Self {
            store: Store::open(root)?,
            parts,
            busy: Mutex::new(()),
        })
    }

    fn download_catalog(&self) -> Result<Value, RegistryError> {
        let reader = self
            .parts
            .fetch
            .open(&self.parts.registry_url, Redirects::Refuse)?;
        let mut bytes = Vec::new();
        reader
            .take(MAX_CATALOG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| RegistryError::Download(error.to_string()))?;
        if bytes.len() as u64 > MAX_CATALOG_BYTES {
            return Err(RegistryError::BadCatalog("it is larger than 4 MiB".into()));
        }
        let payload: Value = serde_json::from_slice(&bytes)
            .map_err(|error| RegistryError::BadCatalog(error.to_string()))?;
        // Saved only once it is known to be a catalog.
        parse_catalog(&payload, self.parts.target.as_deref())?;
        Ok(payload)
    }

    /// The catalog, fresh when `refresh` and the registry answers, otherwise the saved one.
    fn load_catalog(
        &self,
        refresh: bool,
    ) -> Result<(Vec<CatalogEntry>, CatalogOrigin, Option<String>), RegistryError> {
        let target = self.parts.target.as_deref();
        let mut failure = None;
        if refresh {
            match self.download_catalog() {
                Ok(payload) => {
                    self.store.cache_catalog(&payload);
                    return Ok((
                        parse_catalog(&payload, target)?,
                        CatalogOrigin::Network,
                        None,
                    ));
                }
                Err(error) => failure = Some(error.to_string()),
            }
        }
        match self
            .store
            .cached_catalog()
            .map(|payload| parse_catalog(&payload, target))
        {
            Some(Ok(entries)) => Ok((entries, CatalogOrigin::Saved, failure)),
            _ => Err(RegistryError::CatalogUnavailable(
                failure.unwrap_or_else(|| "no catalog has been saved yet".into()),
            )),
        }
    }

    /// The agents on offer and the ones installed.
    pub fn listing(&self, refresh: bool) -> Result<Listing, RegistryError> {
        let (entries, origin, refresh_error) = self.load_catalog(refresh)?;
        let node = self.parts.node.status();
        let mut agents: Vec<AgentCard> = entries
            .iter()
            .filter(|entry| is_offered(&entry.id))
            .map(|entry| entry.card(&node))
            .collect();
        agents.sort_by_key(|card| card.name.to_lowercase());
        let installed = self
            .store
            .installations()?
            .iter()
            .map(InstalledAgent::from)
            .collect();
        Ok(Listing {
            agents,
            installed,
            origin,
            refresh_error,
        })
    }

    /// The catalog entry the user asked for, if it can be installed here as that exact version.
    fn resolve(&self, agent_id: &str, version: &str) -> Result<CatalogEntry, RegistryError> {
        let (entries, _, _) = self.load_catalog(true)?;
        let entry = entries
            .into_iter()
            .find(|entry| entry.id == agent_id && is_offered(&entry.id))
            .ok_or_else(|| RegistryError::UnknownAgent(agent_id.into()))?;
        if entry.version != version {
            return Err(RegistryError::VersionChanged {
                name: entry.name,
                requested: version.into(),
                current: entry.version,
            });
        }
        match entry.problem(&self.parts.node.status()) {
            Some(reason) => Err(RegistryError::NotInstallable {
                name: entry.name,
                reason,
            }),
            None => Ok(entry),
        }
    }

    /// What installing `agent_id` at `version` would do, for the user to confirm.
    pub fn plan(&self, agent_id: &str, version: &str) -> Result<InstallPlan, RegistryError> {
        Ok(self.resolve(agent_id, version)?.plan())
    }

    /// Installs `agent_id` at exactly `version`, replacing another version of the same agent.
    pub fn install(&self, agent_id: &str, version: &str) -> Result<InstalledAgent, RegistryError> {
        let _busy = self
            .busy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = self.resolve(agent_id, version)?;
        let installation =
            installer::install(&self.store, &*self.parts.fetch, &*self.parts.node, &entry)?;

        let mut all = self.store.installations()?;
        let replaced: Vec<Installation> = all
            .iter()
            .filter(|old| old.agent_id == entry.id)
            .cloned()
            .collect();
        all.retain(|old| old.agent_id != entry.id);
        all.push(installation.clone());
        self.store.save_installations(&all)?;
        for old in replaced
            .iter()
            .filter(|old| old.location != installation.location)
        {
            if let Err(error) = self.store.remove_location(&old.location) {
                tracing::warn!(%error, "could not remove the replaced agent version");
            }
        }
        Ok(InstalledAgent::from(&installation))
    }

    pub fn uninstall(&self, installation_id: &str) -> Result<(), RegistryError> {
        let _busy = self
            .busy
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut all = self.store.installations()?;
        let found = all
            .iter()
            .find(|installation| installation.id == installation_id)
            .cloned()
            .ok_or_else(|| RegistryError::UnknownInstallation(installation_id.into()))?;
        self.store.remove_location(&found.location)?;
        all.retain(|installation| installation.id != installation_id);
        self.store.save_installations(&all)
    }

    /// How to start an installed agent. The caller runs it through the sandbox, sharing
    /// `read_only_paths` read-only.
    pub fn launch_command(&self, agent_id: &str) -> Result<LaunchCommand, RegistryError> {
        let installation = self
            .store
            .installations()?
            .into_iter()
            .find(|installation| installation.agent_id == agent_id)
            .ok_or_else(|| RegistryError::UnknownInstallation(agent_id.into()))?;
        let path_of = |relative: &str| {
            relative
                .split('/')
                .fold(installation.location.clone(), |path, part| path.join(part))
        };
        let (program, args) =
            match &installation.launch {
                Launch::Executable { path, args } => (path_of(path), args.clone()),
                Launch::Node { script, args } => {
                    let toolchain = self.parts.node.locate().map_err(|reason| {
                        RegistryError::NotInstallable {
                            name: installation.name.clone(),
                            reason,
                        }
                    })?;
                    let mut all = vec![path_of(script).to_string_lossy().into_owned()];
                    all.extend(args.iter().cloned());
                    (toolchain.node, all)
                }
            };
        Ok(LaunchCommand {
            program,
            args,
            read_only_paths: vec![installation.location],
        })
    }
}
