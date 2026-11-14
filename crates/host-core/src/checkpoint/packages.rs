//! Installed packages (`node_modules`) shared with task worktrees.
//!
//! Package stores are ignored by Git, so a checkpoint does not hold them; without them the
//! agent could not build or test. Copying them would take seconds and cloning them tens of
//! milliseconds per thousand files, so each worktree gets a **real** `node_modules` directory
//! whose entries point back to the user's installation:
//!
//! * a relative link that stays inside the workspace (pnpm's links to `.pnpm/...` and to the
//!   workspace's own packages) is reproduced as is: it resolves **inside the worktree**, so a
//!   package of the workspace the agent edits is the worktree's copy, not the user's;
//! * any other link is reproduced pointing at the same absolute destination;
//! * a scope directory (`@types`, ...) is a real directory treated the same way;
//! * a tool cache (`.vite`, `.cache`) is a new, empty, private directory, so tools write
//!   their cache in the worktree, never in the user's;
//! * everything else (packages, `.pnpm`, `.bin`, metadata files) is a link to the original.
//!
//! **Read-only** is a property of the sandbox, not of these links: the shared originals are
//! listed in [`SharedPackageStore`] and the OS sandbox (`LOT-012`) must expose them read-only
//! to the agent. Their permissions are never changed (they are the user's files). No agent
//! runs before `LOT-012`.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use git2::{ObjectType, Repository, TreeWalkMode, TreeWalkResult};
use serde::{Deserialize, Serialize};

use super::paths::from_git_path;
use super::CheckpointError;

/// Names of the package-store directories shared with task worktrees.
pub const PACKAGE_STORE_DIRS: &[&str] = &["node_modules"];
/// Tool caches inside a package store, private (and empty) in each worktree.
const PRIVATE_CACHE_DIRS: &[&str] = &[".vite", ".cache"];

/// A package store of the workspace made available in a task worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedPackageStore {
    /// The store inside the worktree.
    pub path: PathBuf,
    /// The user's store it points to. The sandbox must grant it read-only, never writable.
    pub source: PathBuf,
}

pub(super) struct StoreScope<'a> {
    pub repo: &'a Repository,
    /// Canonical working directory of the repository.
    pub workdir: &'a Path,
    /// Repository-relative path of the workspace.
    pub prefix: &'a Path,
    /// Root of the worktree being provisioned.
    pub root: &'a Path,
}

/// Shares every package store found in a directory of the checkpoint, inside the workspace.
pub(super) fn share_package_stores(
    scope: &StoreScope<'_>,
    tree: git2::Oid,
) -> Result<Vec<SharedPackageStore>, CheckpointError> {
    let tree = scope.repo.find_tree(tree)?;
    let mut dirs = vec![Vec::new()];
    tree.walk(TreeWalkMode::PreOrder, |parent, entry| {
        if entry.kind() == Some(ObjectType::Tree) {
            let mut path = parent.as_bytes().to_vec();
            path.extend_from_slice(entry.name_bytes());
            dirs.push(path);
        }
        TreeWalkResult::Ok
    })?;

    let workspace = scope.workdir.join(scope.prefix);
    let mut shared = Vec::new();
    for dir in dirs {
        let dir = if dir.is_empty() {
            PathBuf::new()
        } else {
            from_git_path(&dir)?
        };
        if !dir.starts_with(scope.prefix) {
            continue;
        }
        for name in PACKAGE_STORE_DIRS {
            let relative = dir.join(name);
            let source = scope.workdir.join(&relative);
            let target = scope.root.join(&relative);
            let Ok(meta) = fs::symlink_metadata(&source) else {
                continue;
            };
            if fs::symlink_metadata(&target).is_ok() {
                continue; // Tracked by Git: already materialized from the checkpoint.
            }
            if meta.file_type().is_symlink() {
                let destination = resolve_link(&source)?;
                make_link(&destination, &target)?;
            } else if meta.is_dir() {
                fs::create_dir(&target).map_err(|error| io_error(&target, error))?;
                mirror(&workspace, &source, &target, true)?;
            } else {
                continue;
            }
            shared.push(SharedPackageStore {
                path: target,
                source,
            });
        }
    }
    Ok(shared)
}

/// Reproduces the entries of `source` in `target` (see the module documentation).
fn mirror(
    workspace: &Path,
    source: &Path,
    target: &Path,
    top_level: bool,
) -> Result<(), CheckpointError> {
    let entries = fs::read_dir(source).map_err(|error| io_error(source, error))?;
    for entry in entries {
        let entry = entry.map_err(|error| io_error(source, error))?;
        let name = entry.file_name();
        let from = source.join(&name);
        let to = target.join(&name);
        let file_type = entry.file_type().map_err(|error| io_error(&from, error))?;
        if file_type.is_symlink() {
            let link = fs::read_link(&from).map_err(|error| io_error(&from, error))?;
            let resolved = lexical_join(source, &link);
            if link.is_relative() && resolved.starts_with(workspace) {
                make_link(&link, &to)?;
            } else {
                make_link(&resolved, &to)?;
            }
        } else if file_type.is_dir() && top_level && is_private_cache(&name) {
            fs::create_dir(&to).map_err(|error| io_error(&to, error))?;
        } else if file_type.is_dir() && top_level && name.to_string_lossy().starts_with('@') {
            fs::create_dir(&to).map_err(|error| io_error(&to, error))?;
            mirror(workspace, &from, &to, false)?;
        } else {
            make_link(&from, &to)?;
        }
    }
    Ok(())
}

fn is_private_cache(name: &OsStr) -> bool {
    PRIVATE_CACHE_DIRS.iter().any(|cache| name == *cache)
}

/// Where the link at `path` points, as an absolute path (the link itself may be relative).
fn resolve_link(path: &Path) -> Result<PathBuf, CheckpointError> {
    let link = fs::read_link(path).map_err(|error| io_error(path, error))?;
    Ok(lexical_join(path.parent().unwrap_or(path), &link))
}

/// `base.join(link)` with `.` and `..` folded without touching the file system.
fn lexical_join(base: &Path, link: &Path) -> PathBuf {
    let mut out = if link.is_absolute() {
        PathBuf::new()
    } else {
        base.to_path_buf()
    };
    for component in link.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(unix)]
fn make_link(destination: &Path, at: &Path) -> Result<(), CheckpointError> {
    std::os::unix::fs::symlink(destination, at).map_err(|error| io_error(at, error))
}

/// Windows needs to know whether the destination is a directory; creating links requires
/// Developer Mode or the privilege, otherwise this is an explicit error.
#[cfg(windows)]
fn make_link(destination: &Path, at: &Path) -> Result<(), CheckpointError> {
    let resolved = lexical_join(at.parent().unwrap_or(at), destination);
    let result = if resolved.is_dir() {
        std::os::windows::fs::symlink_dir(destination, at)
    } else {
        std::os::windows::fs::symlink_file(destination, at)
    };
    result.map_err(|error| io_error(at, error))
}

fn io_error(path: &Path, source: io::Error) -> CheckpointError {
    CheckpointError::Io {
        path: path.to_path_buf(),
        source,
    }
}
