//! Persistent state for one installation, never the user's home or a workspace directory.
use crate::profiles::AgentProfile;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

pub struct PrivateAgentHome {
    path: PathBuf,
    environment: BTreeMap<OsString, OsString>,
}

fn refuse(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason)
}

fn private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(refuse("agent home contains a link or non-directory"))
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                builder
            };
            #[cfg(not(unix))]
            let builder = fs::DirBuilder::new();
            builder.create(path)?;
        }
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            return Err(refuse("agent home must be accessible only by its owner"));
        }
    }
    Ok(())
}

fn beneath(root: &Path, relative: &str, directory: bool) -> io::Result<PathBuf> {
    let parts: Vec<_> = Path::new(relative).components().collect();
    if parts.is_empty()
        || !parts
            .iter()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err(refuse(
            "agent setup paths must be relative without traversal",
        ));
    }
    let mut path = root.to_path_buf();
    for (index, part) in parts.iter().enumerate() {
        path.push(part);
        if directory || index + 1 < parts.len() {
            private_dir(&path)?;
        }
    }
    Ok(path)
}

impl PrivateAgentHome {
    pub fn clear(app_data: &Path, installation_id: &str) -> io::Result<()> {
        if installation_id.is_empty()
            || installation_id.len() > 200
            || !installation_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b))
            || matches!(installation_id, "." | "..")
        {
            return Err(refuse("invalid agent installation identifier"));
        }
        let homes = app_data.canonicalize()?.join("agent-homes");
        private_dir(&homes)?;
        let path = homes.join(installation_id);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
            Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
                return Err(refuse("agent home is not a private directory"))
            }
            Ok(_) => {}
        }
        fs::remove_dir_all(path)
    }
    /// `app_data` is resolved by the Host. The identifier comes from a saved installation.
    pub fn open(
        app_data: &Path,
        installation_id: &str,
        profile: &AgentProfile,
    ) -> io::Result<Self> {
        if installation_id.is_empty()
            || installation_id.len() > 200
            || !installation_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.@".contains(&b))
            || matches!(installation_id, "." | "..")
        {
            return Err(refuse("invalid agent installation identifier"));
        }
        let app_data = app_data.canonicalize()?;
        let homes = app_data.join("agent-homes");
        private_dir(&homes)?;
        let path = homes.join(installation_id);
        private_dir(&path)?;
        for file in &profile.setup {
            let destination = beneath(&path, &file.path, false)?;
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&destination) {
                Ok(mut output) => {
                    output.write_all(file.content.as_bytes())?;
                    output.sync_all()?;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let meta = fs::symlink_metadata(&destination)?;
                    if !meta.is_file()
                        || meta.file_type().is_symlink()
                        || fs::read(&destination)? != file.content.as_bytes()
                    {
                        return Err(refuse(
                            "agent setup file was changed; sign out to reset its private home",
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        let mut environment = BTreeMap::new();
        environment.insert("HOME".into(), path.clone().into_os_string());
        environment.insert("USERPROFILE".into(), path.clone().into_os_string());
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::MetadataExt;
            // CoreFoundation otherwise prefers the account database's home over HOME.
            // Keep its preference files in the same private home. The sandbox continues to
            // deny access to Host preference services and shared-memory caches.
            environment.insert("CFFIXED_USER_HOME".into(), path.clone().into_os_string());
            environment.insert(
                "__CF_USER_TEXT_ENCODING".into(),
                format!("0x{:x}:0:0", fs::metadata(&path)?.uid()).into(),
            );
            beneath(&path, "Library/Preferences", true)?;
        }
        for (variable, relative) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_CACHE_HOME", ".cache"),
            ("XDG_STATE_HOME", ".local/state"),
            ("APPDATA", "AppData/Roaming"),
            ("LOCALAPPDATA", "AppData/Local"),
        ] {
            environment.insert(
                variable.into(),
                beneath(&path, relative, true)?.into_os_string(),
            );
        }
        for (variable, relative) in &profile.home_variables {
            environment.insert(
                variable.into(),
                beneath(&path, relative, true)?.into_os_string(),
            );
        }
        for (key, value) in &profile.environment {
            environment.insert(key.into(), value.into());
        }
        Ok(Self { path, environment })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::agent_profile;

    #[test]
    fn homes_are_persistent_and_contain_only_explicit_environment() {
        let app = tempfile::tempdir().unwrap();
        let profile = agent_profile("codex-acp");
        let home = PrivateAgentHome::open(app.path(), "codex-acp@2.7.0", &profile).unwrap();
        // The browser sign-in is set up by the Host (or switched off there), not by the profile.
        assert!(!home
            .environment()
            .contains_key(std::ffi::OsStr::new("NO_BROWSER")));
        assert_eq!(profile.login_callback_ports, [1455]);
        assert!(!home
            .environment()
            .contains_key(std::ffi::OsStr::new("OPENAI_API_KEY")));
        assert_eq!(
            fs::read_to_string(home.path().join(".codex/config.toml")).unwrap(),
            profile.setup[0].content
        );
        fs::write(home.path().join("login-state"), "synthetic-token").unwrap();
        let second = PrivateAgentHome::open(app.path(), "codex-acp@2.7.0", &profile).unwrap();
        assert_eq!(
            fs::read_to_string(second.path().join("login-state")).unwrap(),
            "synthetic-token"
        );
        for id in ["../escape", ".", "..", "/tmp/escape", "a\\escape"] {
            assert!(PrivateAgentHome::open(app.path(), id, &profile).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn setup_never_follows_an_agent_created_symlink() {
        let app = tempfile::tempdir().unwrap();
        let unknown = agent_profile("unknown");
        let home = PrivateAgentHome::open(app.path(), "agent@1", &unknown).unwrap();
        let outside = app.path().join("outside");
        fs::write(&outside, "unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, home.path().join("config")).unwrap();
        let profile = AgentProfile {
            setup: vec![crate::profiles::SetupFile {
                path: "config".into(),
                content: "replacement".into(),
            }],
            ..unknown
        };
        assert!(PrivateAgentHome::open(app.path(), "agent@1", &profile).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "unchanged");
    }
}
