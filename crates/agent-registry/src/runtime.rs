//! Verified compatibility runtimes declared in bundled profiles, never agent-name branches.
use crate::{archive, installer, profiles::RuntimeArtifact, AgentRegistry, RegistryError};
use std::{ffi::OsString, fs, path::PathBuf};

pub struct PreparedRuntime {
    pub variable: OsString,
    pub program: PathBuf,
    pub read_only_path: PathBuf,
}

impl AgentRegistry {
    /// Materialize an optional pinned runtime before starting the sandbox. No downloaded code
    /// is executed by this method. A failed download, digest or extraction aborts the launch.
    pub fn prepare_runtime(
        &self,
        agent_id: &str,
    ) -> Result<Option<PreparedRuntime>, RegistryError> {
        let _lock = self.busy.lock().expect("registry lock");
        let launch = self.launch_command(agent_id)?;
        let profile = crate::profiles::agent_profile(agent_id);
        let Some(artifact) = profile.runtimes.iter().find(|artifact| {
            artifact.adapter_version == launch.version
                && Some(&artifact.target) == self.parts.target.as_ref()
        }) else {
            return Ok(None);
        };
        self.materialize_runtime(artifact).map(Some)
    }

    fn materialize_runtime(
        &self,
        artifact: &RuntimeArtifact,
    ) -> Result<PreparedRuntime, RegistryError> {
        let id = format!("_runtime-{}", artifact.id);
        let version = format!("{}-{}", artifact.version, artifact.target);
        let location = self.store.runtime_location(&id, &version)?;
        let relative = archive::safe_relative(&artifact.program)
            .ok_or_else(|| RegistryError::Install("Unsafe runtime program path".into()))?;
        let program = location.join("extract").join(&relative);
        if !location.exists() {
            let staging = self.store.new_staging(&id)?;
            let download = staging.path().join("download");
            installer::download_sha512(
                self.parts.fetch.as_ref(),
                &artifact.url,
                &artifact.sha512,
                &download,
            )?;
            let format = archive::format_of(&artifact.url)
                .ok_or_else(|| RegistryError::Install("Unsupported runtime archive".into()))?;
            let extraction = staging.path().join("extract");
            fs::create_dir(&extraction)
                .map_err(RegistryError::io("Preparing runtime extraction"))?;
            archive::extract(&download, format, &extraction, archive::Limits::default())?;
            let source = extraction.join(relative);
            if !fs::symlink_metadata(&source).is_ok_and(|metadata| metadata.file_type().is_file()) {
                return Err(RegistryError::Install(
                    "Runtime archive has no declared program".into(),
                ));
            }
            fs::remove_file(download).map_err(RegistryError::io("Removing runtime download"))?;
            installer::make_executable(&source)?;
            self.store.commit(staging, &id, &version)?;
        }
        if !fs::symlink_metadata(&program).is_ok_and(|metadata| metadata.file_type().is_file()) {
            return Err(RegistryError::Install(
                "Cached runtime is not a regular file".into(),
            ));
        }
        Ok(PreparedRuntime {
            variable: artifact.variable.clone().into(),
            program,
            read_only_path: location,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Fetch, Parts, Redirects};
    use sha2::{Digest, Sha512};
    use std::{
        io::{Cursor, Read},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    struct FixtureFetch {
        archive: Vec<u8>,
        calls: AtomicUsize,
    }
    impl Fetch for FixtureFetch {
        fn open(&self, _: &str, _: Redirects) -> Result<Box<dyn Read + Send>, RegistryError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Cursor::new(self.archive.clone())))
        }
    }
    fn fixture() -> (Arc<FixtureFetch>, RuntimeArtifact) {
        let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_size(7);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "package/bin/runtime", &b"fixture"[..])
            .unwrap();
        let archive = tar.into_inner().unwrap().finish().unwrap();
        let digest = format!("{:x}", Sha512::digest(&archive));
        (
            Arc::new(FixtureFetch {
                archive,
                calls: AtomicUsize::new(0),
            }),
            RuntimeArtifact {
                id: "fixture".into(),
                adapter_version: "1".into(),
                version: "2".into(),
                target: "test".into(),
                url: "https://example.org/runtime.tgz".into(),
                sha512: digest,
                program: "package/bin/runtime".into(),
                variable: "RUNTIME_PATH".into(),
                reason: "fixture".into(),
            },
        )
    }
    #[test]
    fn a_runtime_is_verified_and_cached_without_running_it() {
        let root = tempfile::tempdir().unwrap();
        let (fetch, artifact) = fixture();
        let registry = AgentRegistry::with_parts(
            root.path(),
            Parts {
                fetch: fetch.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        let prepared = registry.materialize_runtime(&artifact).unwrap();
        assert_eq!(fs::read(&prepared.program).unwrap(), b"fixture");
        assert!(prepared.program.starts_with(&prepared.read_only_path));
        registry.materialize_runtime(&artifact).unwrap();
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn an_invalid_runtime_digest_leaves_no_committed_cache() {
        let root = tempfile::tempdir().unwrap();
        let (fetch, mut artifact) = fixture();
        artifact.sha512 = "0".repeat(128);
        let registry = AgentRegistry::with_parts(
            root.path(),
            Parts {
                fetch,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(
            registry.materialize_runtime(&artifact),
            Err(RegistryError::ChecksumMismatch)
        ));
        assert!(!root
            .path()
            .join("packages/_runtime-fixture/2-test")
            .exists());
    }
    #[cfg(unix)]
    #[test]
    fn a_runtime_cache_link_is_refused_before_downloading() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let (fetch, artifact) = fixture();
        let registry = AgentRegistry::with_parts(
            root.path(),
            Parts {
                fetch: fetch.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path(),
            root.path().join("packages/_runtime-fixture"),
        )
        .unwrap();
        assert!(registry.materialize_runtime(&artifact).is_err());
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 0);
    }
}
