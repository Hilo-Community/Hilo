//! Invisible Git time-travel checkpoints (`LOT-011`).
//!
//! Before an agent acts, the Host records the workspace exactly as the user sees it (tracked
//! files, untracked files that `.gitignore` does not exclude, unsaved `ropey` buffers) as a
//! commit under `refs/hilo/checkpoints/<task_id>/<seq>`. When the agent is done, the Host
//! records its result the same way. Nothing else of the repository changes: not `HEAD`, not
//! the branch, not `.git/index` (the tree is built in Hilo's own index file), not
//! `git log` of any branch.
//!
//! [`CheckpointManager::rollback_to_checkpoint`] undoes what the agent did since a checkpoint
//! and **never** overwrites a change the user made by hand: such a file is reported as a
//! conflict and left as it is, unless the user confirmed that path explicitly.
//!
//! [`CheckpointManager::provision_worktree`] (the `CowWorktreeProvisioner` of the spec)
//! materializes the latest checkpoint of a task in its own directory by Copy-on-Write, for the
//! agent to work in; see `worktree.rs`.
//!
//! Only libgit2 is used: it runs no hook, no filter driver and no other program, so creating a
//! checkpoint is allowed on an untrusted workspace (`AGENTS.md` rule 6).

mod packages;
mod paths;
mod rollback;
mod shadow;
mod snapshot;
mod worktree;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use git2::{Oid, Repository, Signature, Time};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use packages::{SharedPackageStore, PACKAGE_STORE_DIRS};
pub use rollback::{RollbackConflict, RollbackFailure, RollbackOptions, RollbackReport};
pub use worktree::TaskWorktree;

/// Namespace of every checkpoint reference.
pub const CHECKPOINT_REF_PREFIX: &str = "refs/hilo/checkpoints";
/// Hilo's own index file, inside the Git directory, next to (never instead of) `index`.
pub const CHECKPOINT_INDEX_FILENAME: &str = "hilo_tmp_index";
/// Untracked files larger than this are left out of checkpoints (and reported as skipped).
pub const MAX_UNTRACKED_FILE_BYTES: u64 = 32 * 1024 * 1024;

const KIND_TRAILER: &str = "Hilo-Checkpoint-Kind";
const AUTHOR_NAME: &str = "Hilo";
const AUTHOR_EMAIL: &str = "checkpoints@hilo.invalid";
const MAX_TASK_ID_LEN: usize = 64;

/// When a checkpoint was taken, relative to an agent action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CheckpointKind {
    /// The workspace just before an agent action: what a rollback returns to.
    BeforeAgent,
    /// The workspace just after it: what tells the agent's changes from the user's.
    AfterAgent,
}

impl CheckpointKind {
    fn trailer_value(self) -> &'static str {
        match self {
            CheckpointKind::BeforeAgent => "before-agent",
            CheckpointKind::AfterAgent => "after-agent",
        }
    }

    fn from_trailer_value(value: &str) -> Option<Self> {
        match value {
            "before-agent" => Some(CheckpointKind::BeforeAgent),
            "after-agent" => Some(CheckpointKind::AfterAgent),
            _ => None,
        }
    }
}

/// The current text of an open document, which may differ from the file on disk.
#[derive(Debug, Clone, Copy)]
pub struct UnsavedBuffer<'a> {
    /// Absolute, or relative to the repository's working directory.
    pub path: &'a Path,
    pub text: &'a str,
}

/// Why a path is not part of a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    /// An untracked file above [`MAX_UNTRACKED_FILE_BYTES`].
    TooLarge { bytes: u64 },
    /// The file or directory could not be read.
    Unreadable,
    /// A submodule: its content belongs to another repository.
    Submodule,
    /// An untracked directory that is itself a Git repository.
    NestedRepository,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedPath {
    pub path: PathBuf,
    pub reason: SkipReason,
}

/// A recorded checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub task_id: String,
    pub seq: u32,
    pub kind: CheckpointKind,
    /// `refs/hilo/checkpoints/<task_id>/<seq>`.
    pub reference: String,
    /// Hex id of the checkpoint commit.
    pub commit: String,
    /// What the snapshot left out; empty for listed checkpoints (only known when taken).
    pub skipped: Vec<SkippedPath>,
}

#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("{0} is not inside a Git repository with a working tree")]
    NotARepository(PathBuf),
    #[error("invalid task id {0:?}: 1 to 64 characters among A-Z a-z 0-9 . _ -, not starting with '.' or '-'")]
    InvalidTaskId(String),
    #[error("invalid workspace path {0:?}")]
    InvalidPath(PathBuf),
    #[error("path {0:?} is outside the workspace")]
    OutsideWorkspace(PathBuf),
    #[error("task {task_id}: a {requested:?} checkpoint cannot follow {previous:?}")]
    OutOfOrder {
        task_id: String,
        requested: CheckpointKind,
        previous: Option<CheckpointKind>,
    },
    #[error("task {task_id} has no checkpoint {seq}")]
    UnknownCheckpoint { task_id: String, seq: u32 },
    #[error("checkpoint {seq} of task {task_id} was not taken before an agent action")]
    NotABeforeCheckpoint { task_id: String, seq: u32 },
    #[error(
        "task {task_id}: the agent's result after the last checkpoint is not recorded, so its \
         changes cannot be told from the user's; record an AfterAgent checkpoint first"
    )]
    AgentResultMissing { task_id: String },
    #[error("checkpoint reference {0} is malformed")]
    Corrupt(String),
    #[error("task {task_id} has no checkpoint to build a worktree from")]
    NoCheckpoint { task_id: String },
    #[error("no worktrees directory is configured (CheckpointManager::with_worktrees_dir)")]
    WorktreesDirNotConfigured,
    #[error("invalid worktrees directory {path:?}: {reason}")]
    InvalidWorktreesDir { path: PathBuf, reason: String },
    #[error("cannot keep a shadow repository for {path:?}: {reason}")]
    InvalidShadowRepository { path: PathBuf, reason: String },
    #[error("task {task_id} already has a worktree; release it first")]
    WorktreeExists { task_id: String },
    #[error(
        "Copy-on-Write cloning is not available for {path:?} ({source}); task worktrees need a \
         file system that supports it (APFS, btrfs, XFS, ReFS) on the same volume as the Git \
         directory, and are never created by a full copy"
    )]
    CowUnavailable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("git: {0}")]
    Git(#[from] git2::Error),
    #[error("I/O error on {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// One checkpoint as stored in Git.
#[derive(Debug, Clone, Copy)]
struct Record {
    seq: u32,
    kind: CheckpointKind,
    commit: Oid,
    tree: Oid,
}

/// Creates, lists, discards and rolls back the checkpoints of one workspace.
pub struct CheckpointManager {
    repo: Mutex<Repository>,
    /// Canonical working directory of the repository.
    workdir: PathBuf,
    /// Repository-relative path of the workspace (empty when it is the whole work tree).
    prefix: PathBuf,
    cache_path: PathBuf,
    /// Where task worktrees go (see [`CheckpointManager::with_worktrees_dir`]).
    worktrees_dir: Option<PathBuf>,
    /// Stable id of the repository, naming its folder in the worktrees directory.
    repository_id: String,
}

impl std::fmt::Debug for CheckpointManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheckpointManager")
            .field("workdir", &self.workdir)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl CheckpointManager {
    /// Opens the repository that contains `workspace_root`. Checkpoints cover `workspace_root`
    /// only, even when the repository is larger.
    pub fn open(workspace_root: &Path) -> Result<Self, CheckpointError> {
        let not_a_repo = || CheckpointError::NotARepository(workspace_root.to_path_buf());
        let root = workspace_root.canonicalize().map_err(|_| not_a_repo())?;
        let repo = Repository::discover(&root).map_err(|_| not_a_repo())?;
        Self::with_repository(repo, &root, workspace_root)
    }

    /// Like [`CheckpointManager::open`], but a workspace that is in no Git repository gets a
    /// **shadow repository** in `shadow_repos_dir` (an application data directory, created on
    /// first use): see `shadow.rs`. The workspace itself is never modified; no `.git` is
    /// created in it.
    pub fn open_or_shadow(
        workspace_root: &Path,
        shadow_repos_dir: &Path,
    ) -> Result<Self, CheckpointError> {
        let not_a_repo = || CheckpointError::NotARepository(workspace_root.to_path_buf());
        let root = workspace_root.canonicalize().map_err(|_| not_a_repo())?;
        match Repository::discover(&root) {
            Ok(repo) => Self::with_repository(repo, &root, workspace_root),
            Err(error) if error.code() == git2::ErrorCode::NotFound => {
                let repo = shadow::open_shadow_repository(&root, shadow_repos_dir)?;
                Self::with_repository(repo, &root, workspace_root)
            }
            Err(_) => Err(not_a_repo()),
        }
    }

    fn with_repository(
        repo: Repository,
        root: &Path,
        workspace_root: &Path,
    ) -> Result<Self, CheckpointError> {
        let not_a_repo = || CheckpointError::NotARepository(workspace_root.to_path_buf());
        let workdir = repo
            .workdir()
            .ok_or_else(not_a_repo)?
            .canonicalize()
            .map_err(|_| not_a_repo())?;
        let prefix = root
            .strip_prefix(&workdir)
            .map_err(|_| not_a_repo())?
            .to_path_buf();
        let cache_path = repo.path().join(CHECKPOINT_INDEX_FILENAME);
        let git_dir = repo
            .path()
            .canonicalize()
            .unwrap_or_else(|_| repo.path().to_path_buf());
        let repository_id =
            blake3::hash(git_dir.as_os_str().as_encoded_bytes()).to_hex()[..16].to_string();
        Ok(CheckpointManager {
            repo: Mutex::new(repo),
            workdir,
            prefix,
            cache_path,
            worktrees_dir: None,
            repository_id,
        })
    }

    /// Sets the directory that holds task worktrees (created if missing): an application
    /// cache directory, outside the repository, without a `.git` component, on the same
    /// volume as the workspace.
    pub fn with_worktrees_dir(mut self, dir: &Path) -> Result<Self, CheckpointError> {
        let invalid = |reason: &str| CheckpointError::InvalidWorktreesDir {
            path: dir.to_path_buf(),
            reason: reason.to_string(),
        };
        if !dir.is_absolute() {
            return Err(invalid("it must be absolute"));
        }
        if dir
            .components()
            .any(|part| part.as_os_str().eq_ignore_ascii_case(".git"))
        {
            return Err(invalid("tools ignore files below a .git directory"));
        }
        // Checked before creating anything; again after, on the canonical path (`/var` and
        // `/private/var` on macOS are the same directory).
        if dir.starts_with(&self.workdir) {
            return Err(invalid("it must be outside the repository"));
        }
        let existed = dir.exists();
        std::fs::create_dir_all(dir).map_err(|source| CheckpointError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let canonical = dir.canonicalize().map_err(|source| CheckpointError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        if canonical.starts_with(&self.workdir) {
            if !existed {
                let _ = std::fs::remove_dir(dir);
            }
            return Err(invalid("it must be outside the repository"));
        }
        self.worktrees_dir = Some(canonical);
        Ok(self)
    }

    /// The workspace the checkpoints cover (canonical).
    pub fn workspace_root(&self) -> PathBuf {
        self.workdir.join(&self.prefix)
    }

    /// Records the workspace for `task_id`. Kinds must alternate, starting with
    /// [`CheckpointKind::BeforeAgent`].
    pub fn create_checkpoint(
        &self,
        task_id: &str,
        kind: CheckpointKind,
        buffers: &[UnsavedBuffer<'_>],
    ) -> Result<Checkpoint, CheckpointError> {
        validate_task_id(task_id)?;
        let repo = self.lock();
        let history = load_history(&repo, task_id)?;
        let previous = history.last().map(|record| record.kind);
        let in_order = matches!(
            (previous, kind),
            (
                None | Some(CheckpointKind::AfterAgent),
                CheckpointKind::BeforeAgent
            ) | (
                Some(CheckpointKind::BeforeAgent),
                CheckpointKind::AfterAgent
            )
        );
        if !in_order {
            return Err(CheckpointError::OutOfOrder {
                task_id: task_id.to_string(),
                requested: kind,
                previous,
            });
        }

        let snapshot = snapshot::snapshot_workspace(&self.scope(&repo), buffers)?;
        let tree = repo.find_tree(snapshot.tree)?;
        let seq = history.last().map_or(1, |record| record.seq + 1);
        // The first checkpoint hangs off `HEAD` for context; it is a child, never an ancestor,
        // of the branch, so `git log` of the branch does not show it.
        let parent = match history.last() {
            Some(record) => Some(repo.find_commit(record.commit)?),
            None => repo.head().ok().and_then(|head| head.peel_to_commit().ok()),
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|time| time.as_secs())
            .unwrap_or(0);
        let signature = Signature::new(AUTHOR_NAME, AUTHOR_EMAIL, &Time::new(now as i64, 0))?;
        let message = format!(
            "Hilo checkpoint {task_id}/{seq}\n\n{KIND_TRAILER}: {}\n",
            kind.trailer_value()
        );
        let parents: Vec<_> = parent.iter().collect();
        let commit = repo.commit(None, &signature, &signature, &message, &tree, &parents)?;
        let reference = checkpoint_ref(task_id, seq);
        // `force = false`: an existing reference (another process won the race) is an error.
        repo.reference(&reference, commit, false, "hilo: checkpoint")?;
        Ok(Checkpoint {
            task_id: task_id.to_string(),
            seq,
            kind,
            reference,
            commit: commit.to_string(),
            skipped: snapshot.skipped,
        })
    }

    /// Checkpoints of `task_id`, oldest first.
    pub fn list_checkpoints(&self, task_id: &str) -> Result<Vec<Checkpoint>, CheckpointError> {
        validate_task_id(task_id)?;
        let repo = self.lock();
        Ok(load_history(&repo, task_id)?
            .into_iter()
            .map(|record| Checkpoint {
                task_id: task_id.to_string(),
                seq: record.seq,
                kind: record.kind,
                reference: checkpoint_ref(task_id, record.seq),
                commit: record.commit.to_string(),
                skipped: Vec::new(),
            })
            .collect())
    }

    /// Deletes every checkpoint reference of `task_id` and its worktree (the task is closed).
    /// Returns how many references were deleted. Git reclaims the objects later, at its next
    /// garbage collection.
    pub fn discard_task(&self, task_id: &str) -> Result<usize, CheckpointError> {
        validate_task_id(task_id)?;
        if self.worktrees_dir.is_some() {
            worktree::remove_worktree(&self.worktree_path(task_id)?)?;
        }
        let repo = self.lock();
        let names: Vec<String> = repo
            .references_glob(&format!("{CHECKPOINT_REF_PREFIX}/{task_id}/*"))?
            .filter_map(|reference| reference.ok()?.name().ok().map(str::to_string))
            .collect();
        for name in &names {
            repo.find_reference(name)?.delete()?;
        }
        Ok(names.len())
    }

    /// Undoes what the agent changed since checkpoint `seq` of `task_id` (see
    /// [`RollbackReport`]). `buffers` is the current text of the open documents.
    pub fn rollback_to_checkpoint(
        &self,
        task_id: &str,
        seq: u32,
        buffers: &[UnsavedBuffer<'_>],
        options: &RollbackOptions,
    ) -> Result<RollbackReport, CheckpointError> {
        validate_task_id(task_id)?;
        let repo = self.lock();
        let history = load_history(&repo, task_id)?;
        let start = history
            .iter()
            .position(|record| record.seq == seq)
            .ok_or_else(|| CheckpointError::UnknownCheckpoint {
                task_id: task_id.to_string(),
                seq,
            })?;
        if history[start].kind != CheckpointKind::BeforeAgent {
            return Err(CheckpointError::NotABeforeCheckpoint {
                task_id: task_id.to_string(),
                seq,
            });
        }
        if history.last().map(|record| record.kind) != Some(CheckpointKind::AfterAgent) {
            return Err(CheckpointError::AgentResultMissing {
                task_id: task_id.to_string(),
            });
        }
        // Agent and user changes are told apart by position: the kinds must alternate.
        let alternates = history[start..].iter().enumerate().all(|(step, record)| {
            record.kind
                == if step % 2 == 0 {
                    CheckpointKind::BeforeAgent
                } else {
                    CheckpointKind::AfterAgent
                }
        });
        if !alternates {
            return Err(CheckpointError::Corrupt(format!(
                "{CHECKPOINT_REF_PREFIX}/{task_id}"
            )));
        }
        let now = snapshot::snapshot_workspace(&self.scope(&repo), buffers)?;
        let trees: Vec<Oid> = history[start..].iter().map(|record| record.tree).collect();
        rollback::rollback(&repo, &self.workdir, &trees, now.tree, options)
    }

    fn scope<'a>(&'a self, repo: &'a Repository) -> snapshot::SnapshotScope<'a> {
        snapshot::SnapshotScope {
            repo,
            workdir: &self.workdir,
            prefix: &self.prefix,
            cache_path: &self.cache_path,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Repository> {
        self.repo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn checkpoint_ref(task_id: &str, seq: u32) -> String {
    format!("{CHECKPOINT_REF_PREFIX}/{task_id}/{seq}")
}

/// Task ids become one reference-name component: keep them to a conservative alphabet.
fn validate_task_id(task_id: &str) -> Result<(), CheckpointError> {
    let valid = !task_id.is_empty()
        && task_id.len() <= MAX_TASK_ID_LEN
        && !task_id.starts_with(['.', '-'])
        && !task_id.ends_with(".lock")
        && !task_id.contains("..")
        && task_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(CheckpointError::InvalidTaskId(task_id.to_string()))
    }
}

fn load_history(repo: &Repository, task_id: &str) -> Result<Vec<Record>, CheckpointError> {
    let prefix = format!("{CHECKPOINT_REF_PREFIX}/{task_id}/");
    let mut records = Vec::new();
    for reference in repo.references_glob(&format!("{prefix}*"))? {
        let reference = reference?;
        let name = reference.name().unwrap_or_default().to_string();
        let corrupt = || CheckpointError::Corrupt(name.clone());
        let seq: u32 = name
            .strip_prefix(&prefix)
            .and_then(|seq| seq.parse().ok())
            .ok_or_else(corrupt)?;
        let commit = reference.peel_to_commit().map_err(|_| corrupt())?;
        let kind = commit
            .message()
            .unwrap_or_default()
            .lines()
            .find_map(|line| line.strip_prefix(KIND_TRAILER)?.strip_prefix(": "))
            .and_then(CheckpointKind::from_trailer_value)
            .ok_or_else(corrupt)?;
        records.push(Record {
            seq,
            kind,
            commit: commit.id(),
            tree: commit.tree_id(),
        });
    }
    records.sort_by_key(|record| record.seq);
    Ok(records)
}
