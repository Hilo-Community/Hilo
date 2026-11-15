use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("The ACP registry could not be reached and no saved catalog exists ({0})")]
    CatalogUnavailable(String),
    #[error("The registry catalog is not valid: {0}")]
    BadCatalog(String),
    #[error("The registry has no agent {0}")]
    UnknownAgent(String),
    #[error("{name} is version {current} in the catalog now, not {requested}. Refresh the list and try again")]
    VersionChanged {
        name: String,
        requested: String,
        current: String,
    },
    #[error("{name} cannot be installed: {reason}")]
    NotInstallable { name: String, reason: String },
    #[error("Download failed: {0}")]
    Download(String),
    #[error(
        "The download does not match the SHA-256 published by the registry; nothing was installed"
    )]
    ChecksumMismatch,
    #[error("The archive is not safe to extract: {0}")]
    UnsafeArchive(String),
    #[error("Installing failed: {0}")]
    Install(String),
    #[error("There is no installation {0}")]
    UnknownInstallation(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
}

impl RegistryError {
    pub(crate) fn io(context: impl Into<String>) -> impl FnOnce(io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }
}
