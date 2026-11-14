//! `PathGuard`: decides whether a path chosen by an agent stays inside the task worktree.
//!
//! Every segment is resolved, one at a time, so a symbolic link anywhere on the way is followed
//! and its real destination checked. `..` is refused outright: resolving it after a link would
//! not mean what its text says. The check is for the Host's own decisions (a tool call naming a
//! file); the sandbox of the OS is what stops the *process* itself, whatever it does at run time
//! (a link created after the check, a hard link, a race).

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathGuardError {
    #[error("worktree root {path:?} is not usable: {source}")]
    BadRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("path {candidate:?} leaves the worktree (it resolves to {resolved:?})")]
    Escapes {
        candidate: PathBuf,
        resolved: PathBuf,
    },
    #[error("path {0:?} contains '..'")]
    ParentDirectory(PathBuf),
    #[error("path {0:?} goes through a symbolic link that points nowhere")]
    DanglingSymlink(PathBuf),
    #[error("cannot inspect {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub struct PathGuard;

impl PathGuard {
    /// Returns the real, absolute location of `candidate` (relative to `worktree_root`, or
    /// absolute), which may not exist yet; fails if it is, or goes through, anything outside the
    /// worktree.
    pub fn verify_within_worktree(
        worktree_root: &Path,
        candidate: &Path,
    ) -> Result<PathBuf, PathGuardError> {
        let root = worktree_root
            .canonicalize()
            .map_err(|source| PathGuardError::BadRoot {
                path: worktree_root.to_path_buf(),
                source,
            })?;
        if !root.is_dir() {
            return Err(PathGuardError::BadRoot {
                path: worktree_root.to_path_buf(),
                source: io::Error::other("not a directory"),
            });
        }
        let relative = if candidate.is_absolute() {
            // The root may be spelled through an alias (`/var` is `/private/var` on macOS).
            candidate
                .strip_prefix(&root)
                .or_else(|_| candidate.strip_prefix(worktree_root))
                .map_err(|_| PathGuardError::Escapes {
                    candidate: candidate.to_path_buf(),
                    resolved: candidate.to_path_buf(),
                })?
                .to_path_buf()
        } else {
            candidate.to_path_buf()
        };

        let io_error = |path: &Path, source| PathGuardError::Io {
            path: path.to_path_buf(),
            source,
        };
        let mut current = root.clone();
        let mut components = relative.components();
        while let Some(component) = components.next() {
            let name = match component {
                Component::CurDir => continue,
                Component::Normal(name) => name,
                Component::ParentDir => {
                    return Err(PathGuardError::ParentDirectory(candidate.to_path_buf()))
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(PathGuardError::Escapes {
                        candidate: candidate.to_path_buf(),
                        resolved: candidate.to_path_buf(),
                    })
                }
            };
            let next = current.join(name);
            match fs::symlink_metadata(&next) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    let resolved = fs::canonicalize(&next).map_err(|source| {
                        if source.kind() == io::ErrorKind::NotFound {
                            PathGuardError::DanglingSymlink(candidate.to_path_buf())
                        } else {
                            io_error(&next, source)
                        }
                    })?;
                    if !resolved.starts_with(&root) {
                        return Err(PathGuardError::Escapes {
                            candidate: candidate.to_path_buf(),
                            resolved,
                        });
                    }
                    current = resolved;
                }
                Ok(_) => current = next,
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    // Nothing exists from here on: the rest can only be plain new names.
                    current = next;
                    for rest in components {
                        match rest {
                            Component::Normal(name) => current.push(name),
                            Component::CurDir => {}
                            _ => {
                                return Err(PathGuardError::ParentDirectory(
                                    candidate.to_path_buf(),
                                ))
                            }
                        }
                    }
                    return Ok(current);
                }
                Err(source) => return Err(io_error(&next, source)),
            }
        }
        Ok(current)
    }
}
