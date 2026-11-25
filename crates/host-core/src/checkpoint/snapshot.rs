//! Builds the Git tree of the workspace as the user sees it, without touching `.git/index`.
//!
//! The tree is assembled in Hilo's own index file (`<git dir>/hilo_tmp_index`), which
//! doubles as a stat cache: a file whose size, times and inode did not change since the last
//! snapshot keeps its blob id instead of being hashed again. Blobs are written with
//! `git_blob_create_from_disk` / `git_blob_create_from_buffer`, which apply **no** Git filter
//! (no CRLF conversion, no `.gitattributes` driver): a restore gives back the exact bytes.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use git2::{Index, IndexEntry, IndexTime, Oid, Repository};

use super::paths::{from_git_path, normalize_relative, to_git_path};
use super::{CheckpointError, SkipReason, SkippedPath, UnsavedBuffer, MAX_UNTRACKED_FILE_BYTES};

pub(super) const MODE_FILE: u32 = 0o100_644;
pub(super) const MODE_EXECUTABLE: u32 = 0o100_755;
pub(super) const MODE_SYMLINK: u32 = 0o120_000;
pub(super) const MODE_GITLINK: u32 = 0o160_000;

/// A cached blob id is trusted only for files last modified at least this long before the
/// snapshot started: a write within the same time-stamp granularity could otherwise keep its
/// size and time ("racy Git").
const RACY_WINDOW: Duration = Duration::from_secs(2);

pub(super) struct Snapshot {
    pub tree: Oid,
    pub skipped: Vec<SkippedPath>,
}

pub(super) struct SnapshotScope<'a> {
    pub repo: &'a Repository,
    /// Canonical working directory of the repository.
    pub workdir: &'a Path,
    /// Repository-relative prefix of the workspace (empty when the workspace is the work tree).
    pub prefix: &'a Path,
    pub cache_path: &'a Path,
}

pub(super) fn snapshot_workspace(
    scope: &SnapshotScope<'_>,
    buffers: &[UnsavedBuffer<'_>],
) -> Result<Snapshot, CheckpointError> {
    let started = SystemTime::now();
    let repo = scope.repo;
    let mut skipped = Vec::new();

    let mut index = open_cache(scope.cache_path)?;
    let cache: HashMap<Vec<u8>, IndexEntry> = index
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    index.clear()?;

    // Unsaved buffers win over the disk.
    let mut overlay = BTreeMap::new();
    for buffer in buffers {
        let relative = normalize_relative(scope.workdir, buffer.path)?;
        if !relative.starts_with(scope.prefix) {
            return Err(CheckpointError::OutsideWorkspace(buffer.path.to_path_buf()));
        }
        overlay.insert(relative, buffer.text);
    }

    // Candidates: what Git tracks (read-only view of the user's index), then every file of the
    // workspace that `.gitignore` does not exclude.
    let mut candidates: BTreeMap<PathBuf, Option<u32>> = BTreeMap::new();
    let mut user_index = repo.index()?;
    user_index.read(false)?;
    for entry in user_index.iter() {
        let relative = from_git_path(&entry.path)?;
        if !relative.starts_with(scope.prefix) {
            continue;
        }
        if entry.mode == MODE_GITLINK {
            skipped.push(SkippedPath {
                path: scope.workdir.join(&relative),
                reason: SkipReason::Submodule,
            });
            continue;
        }
        let stage = (entry.flags >> 12) & 0b11;
        let tracked_mode = (stage == 0).then_some(entry.mode);
        candidates.entry(relative).or_insert(tracked_mode);
    }
    walk(scope, scope.prefix, &mut candidates, &mut skipped)?;

    for (relative, tracked_mode) in &candidates {
        let absolute = scope.workdir.join(relative);
        let git_path = to_git_path(relative)?;
        let meta = match fs::symlink_metadata(&absolute) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if let Some(text) = overlay.remove(relative) {
                    index.add(&entry(
                        git_path,
                        tracked_mode.unwrap_or(MODE_FILE),
                        repo.blob(text.as_bytes())?,
                        None,
                    ))?;
                }
                continue;
            }
            Err(_) => {
                skipped.push(SkippedPath {
                    path: absolute,
                    reason: SkipReason::Unreadable,
                });
                continue;
            }
        };
        if meta.is_dir() {
            continue;
        }
        let mode = file_mode(&meta, *tracked_mode);
        if let Some(text) = overlay.remove(relative) {
            index.add(&entry(git_path, mode, repo.blob(text.as_bytes())?, None))?;
            continue;
        }
        if tracked_mode.is_none() && meta.is_file() && meta.len() > MAX_UNTRACKED_FILE_BYTES {
            skipped.push(SkippedPath {
                path: absolute,
                reason: SkipReason::TooLarge { bytes: meta.len() },
            });
            continue;
        }
        let stat = StatKey::of(&meta);
        let cached = cache
            .get(&git_path)
            .filter(|cached| {
                cached.mode == mode && stat.matches(cached) && stat.settled_before(started)
            })
            .map(|cached| cached.id);
        let id = match cached {
            Some(id) => id,
            None => match hash_file(repo, &absolute, &meta) {
                Ok(id) => id,
                Err(_) => {
                    skipped.push(SkippedPath {
                        path: absolute,
                        reason: SkipReason::Unreadable,
                    });
                    continue;
                }
            },
        };
        index.add(&entry(git_path, mode, id, Some(&stat)))?;
    }

    // Buffers of files that exist neither in Git nor on disk (a new, never saved document).
    for (relative, text) in overlay {
        if repo.is_path_ignored(git_path_for_query(&relative)?)? {
            continue;
        }
        index.add(&entry(
            to_git_path(&relative)?,
            MODE_FILE,
            repo.blob(text.as_bytes())?,
            None,
        ))?;
    }

    let tree = index.write_tree_to(repo)?;
    if let Err(error) = index.write() {
        // Only the stat cache is lost: the next snapshot hashes the files again.
        tracing::warn!(%error, path = %scope.cache_path.display(), "could not persist the checkpoint index");
    }
    Ok(Snapshot { tree, skipped })
}

fn open_cache(path: &Path) -> Result<Index, CheckpointError> {
    match Index::open(path) {
        Ok(index) => Ok(index),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "discarding an unreadable checkpoint index");
            let _ = fs::remove_file(path);
            Ok(Index::open(path)?)
        }
    }
}

/// Recursively collects the files below `dir` that are not ignored. Symbolic links are
/// recorded as links, never followed; nested repositories are reported, not entered.
fn walk(
    scope: &SnapshotScope<'_>,
    dir: &Path,
    out: &mut BTreeMap<PathBuf, Option<u32>>,
    skipped: &mut Vec<SkippedPath>,
) -> Result<(), CheckpointError> {
    let absolute_dir = scope.workdir.join(dir);
    let entries = match fs::read_dir(&absolute_dir) {
        Ok(entries) => entries,
        Err(_) => {
            skipped.push(SkippedPath {
                path: absolute_dir,
                reason: SkipReason::Unreadable,
            });
            return Ok(());
        }
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        if name.eq_ignore_ascii_case(".git") {
            continue;
        }
        let relative = dir.join(&name);
        let Ok(file_type) = entry.file_type() else {
            skipped.push(SkippedPath {
                path: scope.workdir.join(&relative),
                reason: SkipReason::Unreadable,
            });
            continue;
        };
        if file_type.is_dir() {
            if scope.repo.is_path_ignored(git_path_for_query(&relative)?)? {
                continue;
            }
            if scope.workdir.join(&relative).join(".git").exists() {
                if !out.keys().any(|tracked| tracked.starts_with(&relative)) {
                    skipped.push(SkippedPath {
                        path: scope.workdir.join(&relative),
                        reason: SkipReason::NestedRepository,
                    });
                }
                continue;
            }
            walk(scope, &relative, out, skipped)?;
        } else if !out.contains_key(&relative)
            && !scope.repo.is_path_ignored(git_path_for_query(&relative)?)?
        {
            out.insert(relative, None);
        }
    }
    Ok(())
}

/// Paths handed to libgit2 queries always use `/`.
fn git_path_for_query(relative: &Path) -> Result<PathBuf, CheckpointError> {
    let bytes = to_git_path(relative)?;
    Ok(PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
}

fn hash_file(repo: &Repository, absolute: &Path, meta: &Metadata) -> Result<Oid, CheckpointError> {
    if meta.file_type().is_symlink() {
        let destination = fs::read_link(absolute).map_err(|source| CheckpointError::Io {
            path: absolute.to_path_buf(),
            source,
        })?;
        return Ok(repo.blob(&to_git_link(&destination))?);
    }
    Ok(repo.blob_path(absolute)?)
}

#[cfg(unix)]
fn to_git_link(destination: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    destination.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn to_git_link(destination: &Path) -> Vec<u8> {
    destination
        .to_string_lossy()
        .replace('\\', "/")
        .into_bytes()
}

#[cfg(unix)]
fn file_mode(meta: &Metadata, _tracked_mode: Option<u32>) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if meta.file_type().is_symlink() {
        MODE_SYMLINK
    } else if meta.permissions().mode() & 0o111 != 0 {
        MODE_EXECUTABLE
    } else {
        MODE_FILE
    }
}

/// Windows has no executable bit: keep the one Git records.
#[cfg(not(unix))]
fn file_mode(meta: &Metadata, tracked_mode: Option<u32>) -> u32 {
    if meta.file_type().is_symlink() {
        MODE_SYMLINK
    } else if tracked_mode == Some(MODE_EXECUTABLE) {
        MODE_EXECUTABLE
    } else {
        MODE_FILE
    }
}

fn entry(path: Vec<u8>, mode: u32, id: Oid, stat: Option<&StatKey>) -> IndexEntry {
    // A buffer entry carries a zero stat, which never matches a real file: it is hashed again.
    let zero = StatKey::default();
    let stat = stat.unwrap_or(&zero);
    IndexEntry {
        ctime: IndexTime::new(stat.ctime.0, stat.ctime.1),
        mtime: IndexTime::new(stat.mtime.0, stat.mtime.1),
        dev: stat.dev,
        ino: stat.ino,
        mode,
        uid: 0,
        gid: 0,
        file_size: stat.size,
        id,
        flags: 0,
        flags_extended: 0,
        path,
    }
}

/// What the stat cache compares, truncated the way the index stores it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct StatKey {
    mtime: (i32, u32),
    ctime: (i32, u32),
    dev: u32,
    ino: u32,
    size: u32,
    modified: Option<SystemTime>,
}

/// The file behind `meta` is the one `cached` was hashed from, unchanged since.
pub(super) fn stat_matches(meta: &Metadata, cached: &IndexEntry) -> bool {
    StatKey::of(meta).matches(cached)
}

impl StatKey {
    fn of(meta: &Metadata) -> Self {
        let modified = meta.modified().ok();
        let mtime = modified.map(index_time).unwrap_or_default();
        #[cfg(unix)]
        let (ctime, dev, ino) = {
            use std::os::unix::fs::MetadataExt;
            (
                (meta.ctime() as i32, meta.ctime_nsec() as u32),
                meta.dev() as u32,
                meta.ino() as u32,
            )
        };
        #[cfg(not(unix))]
        let (ctime, dev, ino) = (
            meta.created().ok().map(index_time).unwrap_or_default(),
            0,
            0,
        );
        StatKey {
            mtime,
            ctime,
            dev,
            ino,
            size: meta.len() as u32,
            modified,
        }
    }

    fn matches(&self, cached: &IndexEntry) -> bool {
        self.mtime != (0, 0)
            && (cached.mtime.seconds(), cached.mtime.nanoseconds()) == self.mtime
            && (cached.ctime.seconds(), cached.ctime.nanoseconds()) == self.ctime
            && cached.dev == self.dev
            && cached.ino == self.ino
            && cached.file_size == self.size
    }

    fn settled_before(&self, started: SystemTime) -> bool {
        self.modified
            .and_then(|modified| modified.checked_add(RACY_WINDOW))
            .is_some_and(|limit| limit <= started)
    }
}

fn index_time(time: SystemTime) -> (i32, u32) {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| (elapsed.as_secs() as i32, elapsed.subsec_nanos()))
        .unwrap_or_default()
}
