//! What the registry keeps on disk, under one folder:
//! `installations.json`, `registry-cache.json` and `packages/<agent>/<version>/`.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::error::RegistryError;
use crate::model::Installation;

const INSTALLATIONS: &str = "installations.json";
const CACHE: &str = "registry-cache.json";
const PACKAGES: &str = "packages";
const STAGING: &str = ".staging";

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

pub(crate) struct Store {
    root: PathBuf,
}

/// A folder where an install is prepared. It is removed unless it was committed.
pub(crate) struct Staging {
    path: PathBuf,
    keep: bool,
}

impl Staging {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

impl Store {
    pub(crate) fn open(root: impl Into<PathBuf>) -> Result<Self, RegistryError> {
        let store = Self { root: root.into() };
        fs::create_dir_all(store.packages())
            .map_err(RegistryError::io("Creating the agents folder"))?;
        // What a crashed install left behind. Installs are serialized and start after this.
        match fs::remove_dir_all(store.packages().join(STAGING)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                tracing::warn!(%error, "could not clean unfinished agent installs");
            }
            _ => {}
        }
        Ok(store)
    }

    fn packages(&self) -> PathBuf {
        self.root.join(PACKAGES)
    }

    /// Writes `bytes` to `name` so that a crash leaves the old file or the new one, never half.
    fn write_atomically(&self, name: &str, bytes: &[u8]) -> Result<(), RegistryError> {
        let temporary = self.root.join(format!("{name}.tmp"));
        let mut file =
            File::create(&temporary).map_err(RegistryError::io(format!("Writing {name}")))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(RegistryError::io(format!("Writing {name}")))?;
        drop(file);
        fs::rename(&temporary, self.root.join(name))
            .map_err(RegistryError::io(format!("Saving {name}")))
    }

    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, RegistryError> {
        match fs::read(self.root.join(name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(RegistryError::Io {
                context: format!("Reading {name}"),
                source: error,
            }),
        }
    }

    /// The installed agents. A file that cannot be read is set aside, not overwritten, and the
    /// list starts empty: the agents' folders stay where they are.
    pub(crate) fn installations(&self) -> Result<Vec<Installation>, RegistryError> {
        let Some(bytes) = self.read(INSTALLATIONS)? else {
            return Ok(Vec::new());
        };
        match serde_json::from_slice(&bytes) {
            Ok(list) => Ok(list),
            Err(error) => {
                let aside = format!("{INSTALLATIONS}.corrupt-{}", now_ms());
                tracing::warn!(%error, %aside, "the list of installed agents is unreadable; setting it aside");
                fs::rename(self.root.join(INSTALLATIONS), self.root.join(aside)).map_err(
                    RegistryError::io("Setting aside the unreadable list of agents"),
                )?;
                Ok(Vec::new())
            }
        }
    }

    pub(crate) fn save_installations(&self, list: &[Installation]) -> Result<(), RegistryError> {
        let bytes = serde_json::to_vec_pretty(list)
            .map_err(|error| RegistryError::Install(error.to_string()))?;
        self.write_atomically(INSTALLATIONS, &bytes)
    }

    pub(crate) fn cached_catalog(&self) -> Option<Value> {
        serde_json::from_slice(&self.read(CACHE).ok()??).ok()
    }

    pub(crate) fn cache_catalog(&self, payload: &Value) {
        let saved = serde_json::to_vec(payload)
            .map_err(|error| RegistryError::Install(error.to_string()))
            .and_then(|bytes| self.write_atomically(CACHE, &bytes));
        if let Err(error) = saved {
            tracing::warn!(%error, "could not save the agent catalog");
        }
    }

    pub(crate) fn new_staging(&self, agent_id: &str) -> Result<Staging, RegistryError> {
        let name = format!(
            "{agent_id}-{}-{}",
            now_ms(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = self.packages().join(STAGING).join(name);
        fs::create_dir_all(&path)
            .map_err(RegistryError::io("Creating a folder to install into"))?;
        Ok(Staging { path, keep: false })
    }

    /// Moves a finished install into `packages/<agent>/<version>`, replacing that folder.
    pub(crate) fn commit(
        &self,
        mut staging: Staging,
        agent_id: &str,
        version: &str,
    ) -> Result<PathBuf, RegistryError> {
        let destination = self.packages().join(agent_id).join(version);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(RegistryError::io("Creating the agent folder"))?;
        }
        if fs::symlink_metadata(&destination).is_ok() {
            fs::remove_dir_all(&destination)
                .map_err(RegistryError::io("Replacing the installed version"))?;
        }
        fs::rename(&staging.path, &destination)
            .map_err(RegistryError::io("Moving the agent into place"))?;
        staging.keep = true;
        Ok(destination)
    }

    /// Deletes the folder of an installation, and only a folder inside `packages/`: the list on
    /// disk is data, and a path in it is not trusted to point anywhere.
    pub(crate) fn remove_location(&self, location: &Path) -> Result<(), RegistryError> {
        let refuse =
            |reason: &str| RegistryError::Install(format!("{reason}: {}", location.display()));
        match fs::symlink_metadata(location) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(RegistryError::Io {
                    context: "Looking for the installed agent".into(),
                    source: error,
                })
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(refuse("Refusing to remove a link"))
            }
            Ok(_) => {}
        }
        let packages = fs::canonicalize(self.packages())
            .map_err(RegistryError::io("Reading the agents folder"))?;
        let canonical =
            fs::canonicalize(location).map_err(RegistryError::io("Reading the agent folder"))?;
        if canonical == packages || !canonical.starts_with(&packages) {
            return Err(refuse(
                "Refusing to remove a folder outside the agents folder",
            ));
        }
        fs::remove_dir_all(&canonical).map_err(RegistryError::io("Removing the agent"))?;
        // `packages/<agent>` stays only while it holds another version.
        if let Some(parent) = canonical.parent() {
            let _ = fs::remove_dir(parent);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::model::Launch;

    fn installation(id: &str, location: PathBuf) -> Installation {
        Installation {
            id: id.into(),
            agent_id: id.into(),
            name: "Agent".into(),
            version: "1.0.0".into(),
            distribution: "binary".into(),
            installed_at_ms: 1,
            location,
            launch: Launch::Executable {
                path: "extract/agent".into(),
                args: vec![],
            },
        }
    }

    #[test]
    fn installations_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let list = vec![installation("a", dir.path().join("packages/a/1.0.0"))];
        Store::open(dir.path())
            .unwrap()
            .save_installations(&list)
            .unwrap();
        assert_eq!(
            Store::open(dir.path()).unwrap().installations().unwrap(),
            list
        );
        assert!(!dir.path().join("installations.json.tmp").exists());
    }

    #[test]
    fn an_unreadable_list_is_set_aside_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        fs::write(dir.path().join(INSTALLATIONS), "{ not json").unwrap();
        assert!(store.installations().unwrap().is_empty());
        let aside: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.starts_with("installations.json.corrupt-"))
            .collect();
        assert_eq!(aside.len(), 1);
        assert_eq!(fs::read(dir.path().join(&aside[0])).unwrap(), b"{ not json");
    }

    #[test]
    fn an_unfinished_install_is_cleaned_at_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let staging = store.new_staging("a").unwrap();
        fs::write(staging.path().join("half"), "x").unwrap();
        let leftover = staging.path().to_path_buf();
        std::mem::forget(staging); // a crash: the guard never runs
        assert!(leftover.exists());
        Store::open(dir.path()).unwrap();
        assert!(!leftover.exists());
    }

    #[test]
    fn a_staging_folder_that_is_not_committed_disappears() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let staging = store.new_staging("a").unwrap();
        let path = staging.path().to_path_buf();
        assert!(path.exists());
        drop(staging);
        assert!(!path.exists());
    }

    #[test]
    fn commit_replaces_the_same_version_and_removal_stays_inside_packages() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for content in ["old", "new"] {
            let staging = store.new_staging("a").unwrap();
            fs::write(staging.path().join("file"), content).unwrap();
            let location = store.commit(staging, "a", "1.0.0").unwrap();
            assert_eq!(fs::read_to_string(location.join("file")).unwrap(), content);
        }
        let location = dir.path().join("packages/a/1.0.0");
        store.remove_location(&location).unwrap();
        assert!(!location.exists());
        assert!(
            !dir.path().join("packages/a").exists(),
            "the empty agent folder goes too"
        );
        store.remove_location(&location).unwrap(); // already gone: nothing to do
    }

    #[test]
    fn nothing_outside_packages_is_ever_removed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("agents")).unwrap();
        let victim = dir.path().join("precious");
        fs::create_dir(&victim).unwrap();
        fs::write(victim.join("file"), "keep").unwrap();
        for location in [
            victim.clone(),
            dir.path().join("agents/packages"),
            dir.path().join("agents/packages/../../precious"),
            dir.path().to_path_buf(),
        ] {
            assert!(
                store.remove_location(&location).is_err(),
                "{}",
                location.display()
            );
        }
        assert_eq!(fs::read(victim.join("file")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn a_link_inside_packages_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("agents")).unwrap();
        let victim = dir.path().join("precious");
        fs::create_dir(&victim).unwrap();
        let link = dir.path().join("agents/packages/link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(store.remove_location(&link).is_err());
        assert!(victim.exists());
    }

    #[test]
    fn the_saved_catalog_comes_back_or_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.cached_catalog(), None);
        store.cache_catalog(&json!({"agents": []}));
        assert_eq!(store.cached_catalog(), Some(json!({"agents": []})));
        fs::write(dir.path().join(CACHE), "garbage").unwrap();
        assert_eq!(store.cached_catalog(), None);
    }
}
