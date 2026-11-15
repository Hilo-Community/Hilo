//! The private temporary directory of a run, and run identifiers.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::SandboxError;
use crate::spec::TMP_DIR_NAME;

static COUNTER: AtomicU64 = AtomicU64::new(1);

/// A fresh identifier, unique among the runs of this Host and, with the process id and the
/// clock, across Hosts. Letters, digits and `-` only.
pub(crate) fn new_run_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "{}-{}-{nanos:x}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// The private temporary directory of one run, inside the worktree. Removed when dropped.
#[derive(Debug)]
pub(crate) struct TmpDir {
    path: PathBuf,
    parent: PathBuf,
}

impl TmpDir {
    pub(crate) fn create(worktree_root: &Path) -> Result<Self, SandboxError> {
        let parent = worktree_root.join(TMP_DIR_NAME);
        match fs::symlink_metadata(&parent) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(SandboxError::InvalidSpec(format!(
                    "{TMP_DIR_NAME} exists in the worktree and is not a plain directory"
                )))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => create_private_dir(&parent)?,
            Err(error) => return Err(error.into()),
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let name = format!(
            "{}-{}-{nanos}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = parent.join(name);
        create_private_dir(&path)?;
        Ok(Self { path, parent })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        // Only succeeds when empty: the worktree is left as it was.
        let _ = fs::remove_dir(&self.parent);
    }
}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}
