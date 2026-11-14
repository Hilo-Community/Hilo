//! Rolling a task back without overwriting the user's own edits.
//!
//! The history of a task alternates `BeforeAgent` and `AfterAgent` checkpoints:
//! `C_s, A_s, C_s+1, A_s+1, ..., A_n`, then the workspace as it is now. A change between `C_k`
//! and `A_k` was made by the agent; a change between `A_k` and `C_k+1`, or between `A_n` and
//! now, was made by somebody else (the user). Rolling back to `C_s`:
//!
//! * a file the agent never touched is left alone;
//! * a file only the agent touched is restored to its `C_s` content (or removed if the agent
//!   created it);
//! * a file both touched is a **conflict**: it is left exactly as it is, unless the user
//!   confirmed that path in [`RollbackOptions::overwrite_confirmed`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use git2::{DiffOptions, Oid, Repository, Tree};
use serde::{Deserialize, Serialize};

use super::paths::{from_git_path, remove_file_if_present, resolve_for_write, write_file_atomic};
use super::snapshot::{MODE_EXECUTABLE, MODE_FILE, MODE_SYMLINK};
use super::CheckpointError;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackOptions {
    /// Absolute paths (as reported in [`RollbackConflict::path`]) the user agreed to lose their
    /// own edits on.
    pub overwrite_confirmed: BTreeSet<PathBuf>,
}

/// A file changed by both the agent and the user: left untouched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackConflict {
    pub path: PathBuf,
}

/// A file that should have been restored but could not be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackFailure {
    pub path: PathBuf,
    pub reason: String,
}

/// What a rollback did. Paths are absolute. Open documents for `restored` and `removed`
/// paths must be reloaded from disk by the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackReport {
    pub restored: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub conflicts: Vec<RollbackConflict>,
    pub failed: Vec<RollbackFailure>,
}

impl RollbackReport {
    /// Every change of the agent was undone.
    pub fn is_complete(&self) -> bool {
        self.conflicts.is_empty() && self.failed.is_empty()
    }
}

enum Restore {
    Written,
    Removed,
}

/// `trees` is `[C_s, A_s, C_s+1, A_s+1, ..., A_n]`; `now` the current workspace.
pub(super) fn rollback(
    repo: &Repository,
    workdir: &Path,
    trees: &[Oid],
    now: Oid,
    options: &RollbackOptions,
) -> Result<RollbackReport, CheckpointError> {
    let mut agent = BTreeSet::new();
    let mut human = BTreeSet::new();
    for (step, pair) in trees.windows(2).enumerate() {
        let touched = if step % 2 == 0 {
            &mut agent
        } else {
            &mut human
        };
        touched.extend(changed_paths(repo, pair[0], pair[1])?);
    }
    if let Some(last) = trees.last() {
        human.extend(changed_paths(repo, *last, now)?);
    }

    let target = repo.find_tree(trees[0])?;
    let current = repo.find_tree(now)?;
    let mut report = RollbackReport::default();
    for git_path in agent {
        let relative = from_git_path(&git_path)?;
        let absolute = workdir.join(&relative);
        let wanted = file_entry(&target, &git_path);
        if wanted == file_entry(&current, &git_path) {
            continue; // Already as it was (the agent reverted it, or a previous rollback did).
        }
        if human.contains(&git_path) && !options.overwrite_confirmed.contains(&absolute) {
            report.conflicts.push(RollbackConflict { path: absolute });
            continue;
        }
        match restore(repo, workdir, &relative, wanted) {
            Ok(Restore::Written) => report.restored.push(absolute),
            Ok(Restore::Removed) => report.removed.push(absolute),
            Err(error) => report.failed.push(RollbackFailure {
                path: absolute,
                reason: error.to_string(),
            }),
        }
    }
    Ok(report)
}

/// Files that differ between two trees (both sides of renames and type changes).
fn changed_paths(
    repo: &Repository,
    old: Oid,
    new: Oid,
) -> Result<BTreeSet<Vec<u8>>, CheckpointError> {
    let mut paths = BTreeSet::new();
    if old == new {
        return Ok(paths);
    }
    let (old, new) = (repo.find_tree(old)?, repo.find_tree(new)?);
    let mut options = DiffOptions::new();
    options
        .include_typechange(true)
        .ignore_submodules(true)
        .skip_binary_check(true);
    let diff = repo.diff_tree_to_tree(Some(&old), Some(&new), Some(&mut options))?;
    for delta in diff.deltas() {
        for file in [delta.old_file(), delta.new_file()] {
            if file.id().is_zero() {
                continue;
            }
            if let Some(path) = file.path_bytes() {
                paths.insert(path.to_vec());
            }
        }
    }
    Ok(paths)
}

/// Blob id and mode of the file at `git_path`, if the tree holds a file (not a directory) there.
fn file_entry(tree: &Tree<'_>, git_path: &[u8]) -> Option<(Oid, u32)> {
    let path = PathBuf::from(String::from_utf8_lossy(git_path).into_owned());
    let entry = tree.get_path(&path).ok()?;
    let mode = entry.filemode() as u32;
    matches!(mode, MODE_FILE | MODE_EXECUTABLE | MODE_SYMLINK).then(|| (entry.id(), mode))
}

fn restore(
    repo: &Repository,
    workdir: &Path,
    relative: &Path,
    wanted: Option<(Oid, u32)>,
) -> Result<Restore, CheckpointError> {
    let target = resolve_for_write(workdir, relative)?;
    let Some((id, mode)) = wanted else {
        remove_file_if_present(&target)?;
        return Ok(Restore::Removed);
    };
    let blob = repo.find_blob(id)?;
    if mode == MODE_SYMLINK {
        return restore_symlink(&target, blob.content());
    }
    write_file_atomic(&target, blob.content(), mode == MODE_EXECUTABLE)?;
    Ok(Restore::Written)
}

#[cfg(unix)]
fn restore_symlink(target: &Path, destination: &[u8]) -> Result<Restore, CheckpointError> {
    super::paths::write_symlink(target, destination)?;
    Ok(Restore::Written)
}

#[cfg(not(unix))]
fn restore_symlink(target: &Path, _destination: &[u8]) -> Result<Restore, CheckpointError> {
    Err(CheckpointError::Io {
        path: target.to_path_buf(),
        source: std::io::Error::other("symbolic links are not restored on this platform"),
    })
}
