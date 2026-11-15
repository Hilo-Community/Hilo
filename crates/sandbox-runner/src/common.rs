//! Pieces every OS engine shares: what counts as a secret, what a process may read, the CPU
//! budget, and the check that the program can be run at all.

use std::path::{Path, PathBuf};

use crate::error::SandboxError;
use crate::spec::{ResourceLimits, ValidatedSpec};

/// Denied (or hidden) below the home directory of the user, whatever else is shared.
pub(crate) const HOME_SECRETS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".azure",
    ".kube",
    ".docker",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".config/gcloud",
    ".config/gh",
    ".config/op",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".terraform.d/credentials.tfrc.json",
    ".orbstack",
    ".colima",
    "Library/Keychains",
    "Library/Cookies",
];

/// Sockets of container runtimes: a process that reaches one controls the host.
pub(crate) const CONTAINER_SOCKETS: &[&str] = &[
    "/private/var/run/docker.sock",
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/run/podman",
    "/private/var/run/podman",
];

pub(crate) fn within(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// CPU seconds for an `RLIMIT_CPU`, rounded up so the budget is never shorter than asked.
pub(crate) fn cpu_seconds(limits: &ResourceLimits) -> Option<u64> {
    limits
        .max_cpu_time
        .map(|time| time.as_secs() + u64::from(time.subsec_nanos() > 0))
}

/// The program must be something the sandbox lets the process read and run, otherwise it would
/// fail at start-up with an unreadable error. Both the path as given (its directory resolved) and
/// its real location must be inside `readable`.
pub(crate) fn check_program_is_readable(
    spec: &ValidatedSpec,
    readable: &[PathBuf],
) -> Result<(), SandboxError> {
    let real = std::fs::canonicalize(&spec.program).map_err(|error| {
        SandboxError::InvalidSpec(format!(
            "the program {:?} is unusable: {error}",
            spec.program
        ))
    })?;
    let named = spec
        .program
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .zip(spec.program.file_name())
        .map(|(parent, name)| parent.join(name))
        .unwrap_or_else(|| spec.program.clone());
    if !within(&real, readable) || !within(&named, readable) {
        return Err(SandboxError::InvalidSpec(format!(
            "the program {:?} is outside the paths the sandbox lets it read; add its toolchain \
             directory to the read-only paths",
            spec.program
        )));
    }
    Ok(())
}
