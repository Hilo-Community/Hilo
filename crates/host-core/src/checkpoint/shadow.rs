//! Shadow repositories: checkpoints for a workspace that is in no Git repository.
//!
//! Such a workspace gets a bare repository of its own in the application's data directory,
//! `<shadow repos dir>/<id>`, where `id` comes from the workspace's canonical path. Each time
//! it is opened, the workspace is attached as its working directory **in memory only**
//! (`set_workdir(.., false)`): no `.git` file or directory is ever written in the workspace,
//! and the `git` CLI run there still says it is not a repository.
//!
//! Everything else works as with a real repository: the snapshot reads the workspace's own
//! `.gitignore` files, skips untracked files above `MAX_UNTRACKED_FILE_BYTES` (every file is
//! untracked here), and the repository's `info/exclude` leaves package stores out, as a
//! project's `.gitignore` usually does. Only libgit2 touches the repository: no hook runs.
//!
//! If the user later runs `git init` in the workspace, the real repository is used from then
//! on; the shadow repository is simply no longer opened.

use std::fs;
use std::path::{Path, PathBuf};

use git2::{ErrorCode, Repository, RepositoryInitOptions};

use super::packages::PACKAGE_STORE_DIRS;
use super::CheckpointError;

/// Paths a shadow repository never records (its `info/exclude`).
const SHADOW_EXCLUDES: &[&str] = &[".DS_Store", "Thumbs.db"];

/// Opens (creating it on first use) the shadow repository of the canonical workspace `root`,
/// with `root` attached as its working directory.
pub(super) fn open_shadow_repository(
    root: &Path,
    shadow_repos_dir: &Path,
) -> Result<Repository, CheckpointError> {
    let invalid = |reason: &str| CheckpointError::InvalidShadowRepository {
        path: root.to_path_buf(),
        reason: reason.to_string(),
    };
    if root.parent().is_none() {
        return Err(invalid("the root of a file system is not a workspace"));
    }
    if !shadow_repos_dir.is_absolute() {
        return Err(invalid(
            "the shadow repositories directory must be absolute",
        ));
    }
    // Checked before creating anything, on the canonical form of the part that exists
    // (`/var` and `/private/var` on macOS are the same directory); again after creating it.
    if shadow_repos_dir.starts_with(root) || canonical_prefix(shadow_repos_dir).starts_with(root) {
        return Err(invalid(
            "the shadow repositories directory must be outside the workspace",
        ));
    }
    fs::create_dir_all(shadow_repos_dir).map_err(|source| io_error(shadow_repos_dir, source))?;
    let dir = shadow_repos_dir
        .canonicalize()
        .map_err(|source| io_error(shadow_repos_dir, source))?;
    if dir.starts_with(root) {
        return Err(invalid(
            "the shadow repositories directory must be outside the workspace",
        ));
    }
    if root.starts_with(&dir) {
        return Err(invalid(
            "the workspace must be outside the shadow repositories directory",
        ));
    }

    let id = blake3::hash(root.as_os_str().as_encoded_bytes()).to_hex()[..16].to_string();
    let path = dir.join(id);
    let repo = match Repository::open_bare(&path) {
        Ok(repo) => repo,
        Err(error) if error.code() == ErrorCode::NotFound => create(&path)?,
        Err(error) => return Err(error.into()),
    };
    if !repo.is_bare() {
        return Err(invalid("the shadow repository is not a bare repository"));
    }
    // `false`: never write a `.git` link file into the workspace.
    repo.set_workdir(root, false)?;
    Ok(repo)
}

/// `path` with its longest existing ancestor canonicalized and the rest appended as is.
fn canonical_prefix(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            return missing
                .iter()
                .rev()
                .fold(canonical, |acc, part| acc.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn create(path: &Path) -> Result<Repository, CheckpointError> {
    let mut options = RepositoryInitOptions::new();
    options
        .bare(true)
        .mkpath(true)
        // No template: nothing from the user's Git installation (hooks, excludes) is copied.
        .external_template(false);
    let repo = Repository::init_opts(path, &options)?;
    let info = path.join("info");
    fs::create_dir_all(&info).map_err(|source| io_error(&info, source))?;
    let excludes: String = PACKAGE_STORE_DIRS
        .iter()
        .map(|dir| format!("{dir}/\n"))
        .chain(SHADOW_EXCLUDES.iter().map(|name| format!("{name}\n")))
        .collect();
    let exclude = info.join("exclude");
    fs::write(&exclude, excludes).map_err(|source| io_error(&exclude, source))?;
    Ok(repo)
}

fn io_error(path: &Path, source: std::io::Error) -> CheckpointError {
    CheckpointError::Io {
        path: PathBuf::from(path),
        source,
    }
}
