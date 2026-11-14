//! Workspace-relative paths for checkpoints: validation, Git encoding, and writes that never
//! leave the workspace (`AGENTS.md` rule 1).

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::CheckpointError;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Turns `path` (absolute inside `workdir`, or relative to it) into a clean relative path.
///
/// Refused: anything that is not plainly below `workdir` (`..`, a root, a drive prefix) and any
/// `.git` component, whatever its case (case-insensitive file systems would reach the real one).
pub(super) fn normalize_relative(workdir: &Path, path: &Path) -> Result<PathBuf, CheckpointError> {
    let relative = if path.is_absolute() {
        strip_workdir(workdir, path)?
    } else {
        path.to_path_buf()
    };
    let mut clean = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                if part.eq_ignore_ascii_case(".git") {
                    return Err(CheckpointError::InvalidPath(path.to_path_buf()));
                }
                clean.push(part);
            }
            Component::CurDir => {}
            _ => return Err(CheckpointError::InvalidPath(path.to_path_buf())),
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(CheckpointError::InvalidPath(path.to_path_buf()));
    }
    Ok(clean)
}

/// `workdir` is canonical; `path` may not be (`/var` and `/private/var` on macOS).
fn strip_workdir(workdir: &Path, path: &Path) -> Result<PathBuf, CheckpointError> {
    if let Ok(relative) = path.strip_prefix(workdir) {
        return Ok(relative.to_path_buf());
    }
    // The file itself may not exist yet (an unsaved new buffer): canonicalize its parent.
    let outside = || CheckpointError::OutsideWorkspace(path.to_path_buf());
    let (parent, name) = (
        path.parent().ok_or_else(outside)?,
        path.file_name().ok_or_else(outside)?,
    );
    let parent = parent.canonicalize().map_err(|_| outside())?;
    parent
        .strip_prefix(workdir)
        .map(|relative| relative.join(name))
        .map_err(|_| outside())
}

/// Index/tree path bytes: components joined by `/` on every platform.
pub(super) fn to_git_path(relative: &Path) -> Result<Vec<u8>, CheckpointError> {
    let mut out = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(CheckpointError::InvalidPath(relative.to_path_buf()));
        };
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(os_str_bytes(part, relative)?);
    }
    Ok(out)
}

/// Inverse of [`to_git_path`]; also rejects tree paths that would escape the workspace.
pub(super) fn from_git_path(bytes: &[u8]) -> Result<PathBuf, CheckpointError> {
    let mut path = PathBuf::new();
    for part in bytes.split(|byte| *byte == b'/') {
        let part = bytes_os_str(part)?;
        if part.is_empty() || part == "." || part == ".." || part.eq_ignore_ascii_case(".git") {
            return Err(CheckpointError::InvalidPath(PathBuf::from(
                String::from_utf8_lossy(bytes).into_owned(),
            )));
        }
        path.push(part);
    }
    Ok(path)
}

#[cfg(unix)]
fn os_str_bytes<'a>(part: &'a OsStr, _whole: &Path) -> Result<&'a [u8], CheckpointError> {
    use std::os::unix::ffi::OsStrExt;
    Ok(part.as_bytes())
}

#[cfg(not(unix))]
fn os_str_bytes<'a>(part: &'a OsStr, whole: &Path) -> Result<&'a [u8], CheckpointError> {
    part.to_str()
        .map(str::as_bytes)
        .ok_or_else(|| CheckpointError::InvalidPath(whole.to_path_buf()))
}

#[cfg(unix)]
fn bytes_os_str(bytes: &[u8]) -> Result<&OsStr, CheckpointError> {
    use std::os::unix::ffi::OsStrExt;
    Ok(OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn bytes_os_str(bytes: &[u8]) -> Result<&OsStr, CheckpointError> {
    std::str::from_utf8(bytes).map(OsStr::new).map_err(|_| {
        CheckpointError::InvalidPath(PathBuf::from(String::from_utf8_lossy(bytes).into_owned()))
    })
}

/// Absolute target for writing `relative`, refusing to go through a symbolic link: every
/// existing ancestor below `workdir` must be a real directory, so a link planted in the
/// workspace cannot redirect a restore outside of it.
pub(super) fn resolve_for_write(
    workdir: &Path,
    relative: &Path,
) -> Result<PathBuf, CheckpointError> {
    let mut current = workdir.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(CheckpointError::OutsideWorkspace(workdir.join(relative)));
                }
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    return Err(CheckpointError::Io {
                        path: current,
                        source: io::Error::other("a file stands where a directory is expected"),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(source) => {
                    return Err(CheckpointError::Io {
                        path: current,
                        source,
                    })
                }
            }
        }
    }
    Ok(workdir.join(relative))
}

/// Replaces `target` atomically (temporary file in the same directory, then rename). A symbolic
/// link at `target` is replaced, never followed.
pub(super) fn write_file_atomic(
    target: &Path,
    contents: &[u8],
    executable: bool,
) -> Result<(), CheckpointError> {
    let io_error = |source| CheckpointError::Io {
        path: target.to_path_buf(),
        source,
    };
    let parent = target
        .parent()
        .ok_or_else(|| CheckpointError::InvalidPath(target.to_path_buf()))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    if fs::symlink_metadata(target).is_ok_and(|meta| meta.is_dir()) {
        return Err(io_error(io::Error::other(
            "a directory stands where the file is restored",
        )));
    }
    let tmp = parent.join(format!(
        ".{}.coloride-restore-{}-{}",
        target
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default(),
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(contents)?;
        set_executable(&file, executable)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(io_error)
}

#[cfg(unix)]
fn set_executable(file: &fs::File, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(if executable {
        0o755
    } else {
        0o644
    }))
}

#[cfg(not(unix))]
fn set_executable(_file: &fs::File, _executable: bool) -> io::Result<()> {
    Ok(())
}

/// Replaces `target` with a symbolic link to `destination` (Unix only).
#[cfg(unix)]
pub(super) fn write_symlink(target: &Path, destination: &[u8]) -> Result<(), CheckpointError> {
    use std::os::unix::ffi::OsStrExt;
    let io_error = |source| CheckpointError::Io {
        path: target.to_path_buf(),
        source,
    };
    let parent = target
        .parent()
        .ok_or_else(|| CheckpointError::InvalidPath(target.to_path_buf()))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    remove_file_if_present(target)?;
    std::os::unix::fs::symlink(OsStr::from_bytes(destination), target).map_err(io_error)
}

/// Removes a file or symbolic link; a missing file is not an error. Directories are refused.
pub(super) fn remove_file_if_present(target: &Path) -> Result<(), CheckpointError> {
    match fs::symlink_metadata(target) {
        Ok(meta) if meta.is_dir() => Err(CheckpointError::Io {
            path: target.to_path_buf(),
            source: io::Error::other("a directory stands where a file is removed"),
        }),
        Ok(_) => fs::remove_file(target).map_err(|source| CheckpointError::Io {
            path: target.to_path_buf(),
            source,
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CheckpointError::Io {
            path: target.to_path_buf(),
            source,
        }),
    }
}
