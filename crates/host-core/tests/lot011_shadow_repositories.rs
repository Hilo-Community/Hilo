//! Checkpoints for a workspace that is in no Git repository: a shadow repository in the
//! application's data directory, the workspace left untouched (no `.git` written in it).
#![allow(clippy::disallowed_methods)] // the `git` CLI inspects the repositories under test

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use host_core::{
    CheckpointError, CheckpointKind, CheckpointManager, RollbackOptions, UnsavedBuffer,
};

const BEFORE: CheckpointKind = CheckpointKind::BeforeAgent;
const AFTER: CheckpointKind = CheckpointKind::AfterAgent;

fn git_output(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git must be installed to run the LOT-011 tests")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = git_output(dir, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Whether `dir` is in no Git repository. A temporary directory may sit inside one on some
/// machines; the tests that need a plain folder then have nothing to check.
fn outside_any_repository(dir: &Path) -> bool {
    !git_output(dir, &["rev-parse", "--git-dir"])
        .status
        .success()
}

fn write(root: &Path, path: &str, contents: &[u8]) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn read(root: &Path, path: &str) -> Vec<u8> {
    fs::read(root.join(path)).unwrap()
}

/// The single shadow repository in `shadow_dir`.
fn shadow_repository(shadow_dir: &Path) -> PathBuf {
    let repos: Vec<_> = fs::read_dir(shadow_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(repos.len(), 1, "{repos:?}");
    repos.into_iter().next().unwrap()
}

/// A folder without Git gets checkpoints and rollback like a repository, while nothing is
/// written in it: it is still not a Git repository afterwards.
#[test]
fn a_folder_without_git_gets_checkpoints_in_a_shadow_repository() {
    let folder = tempfile::tempdir().unwrap();
    let root = folder.path();
    if !outside_any_repository(root) {
        return;
    }
    let shadow = tempfile::tempdir().unwrap();
    write(root, "notes.md", b"notes\n");
    write(root, "src/a.ts", b"export const a = 1;\n");
    write(root, "node_modules/pkg/index.js", b"module.exports = 1;\n");
    write(root, ".DS_Store", b"finder\n");
    write(root, ".gitignore", b"*.log\n");
    write(root, "debug.log", b"log\n");

    let manager = CheckpointManager::open_or_shadow(root, shadow.path()).unwrap();
    assert_eq!(manager.workspace_root(), root.canonicalize().unwrap());
    let buffer_path = root.join("src/a.ts");
    let unsaved = [UnsavedBuffer {
        path: &buffer_path,
        text: "export const a = 'unsaved';\n",
    }];
    let before = manager
        .create_checkpoint("task-1", BEFORE, &unsaved)
        .unwrap();

    // What the checkpoint holds: the workspace's own files and buffers, not package stores,
    // Finder metadata, or what the workspace's `.gitignore` excludes.
    let repo = shadow_repository(shadow.path());
    let listed = git(
        root,
        &[
            "--git-dir",
            repo.to_str().unwrap(),
            "ls-tree",
            "-r",
            "--name-only",
            &before.commit,
        ],
    );
    assert_eq!(
        listed.lines().collect::<Vec<_>>(),
        [".gitignore", "notes.md", "src/a.ts"]
    );
    let blob = git(
        root,
        &[
            "--git-dir",
            repo.to_str().unwrap(),
            "show",
            &format!("{}:src/a.ts", before.commit),
        ],
    );
    assert_eq!(blob, "export const a = 'unsaved';\n");

    // The agent edits and adds files; a rollback undoes it.
    write(root, "notes.md", b"agent\n");
    write(root, "src/new.ts", b"new\n");
    manager.create_checkpoint("task-1", AFTER, &[]).unwrap();
    let report = manager
        .rollback_to_checkpoint("task-1", before.seq, &[], &RollbackOptions::default())
        .unwrap();
    assert!(report.conflicts.is_empty(), "{report:?}");
    assert_eq!(read(root, "notes.md"), b"notes\n");
    assert!(!root.join("src/new.ts").exists());
    // The buffer's text was the checkpointed state: the rollback writes it to disk.
    assert_eq!(read(root, "src/a.ts"), b"export const a = 'unsaved';\n");

    // The folder is untouched as far as Git is concerned.
    assert!(!root.join(".git").exists());
    assert!(outside_any_repository(root));

    // Opening again finds the same shadow repository and its history.
    let again = CheckpointManager::open_or_shadow(root, shadow.path()).unwrap();
    assert_eq!(again.list_checkpoints("task-1").unwrap().len(), 2);
    assert_eq!(shadow_repository(shadow.path()), repo);
    // The strict constructor still refuses the folder.
    assert!(matches!(
        CheckpointManager::open(root),
        Err(CheckpointError::NotARepository(_))
    ));
}

/// Inside a real repository, the real one is used and no shadow repository is created.
#[test]
fn a_real_repository_is_used_when_there_is_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    write(root, "a.ts", b"a\n");
    let shadow = tempfile::tempdir().unwrap();
    let manager = CheckpointManager::open_or_shadow(root, shadow.path()).unwrap();
    manager.create_checkpoint("task-real", BEFORE, &[]).unwrap();
    assert_ne!(git(root, &["for-each-ref", "refs/hilo"]), "");
    assert_eq!(fs::read_dir(shadow.path()).unwrap().count(), 0);
}

/// The shadow repositories directory is absolute and outside the workspace (a snapshot would
/// otherwise record the repository itself); refusing creates nothing.
#[test]
fn the_shadow_repositories_directory_must_be_absolute_and_outside_the_workspace() {
    let folder = tempfile::tempdir().unwrap();
    let root = folder.path();
    if !outside_any_repository(root) {
        return;
    }
    write(root, "a.txt", b"a\n");
    for bad in [PathBuf::from("relative/shadow"), root.join("shadow")] {
        let error = CheckpointManager::open_or_shadow(root, &bad).unwrap_err();
        assert!(
            matches!(error, CheckpointError::InvalidShadowRepository { .. }),
            "{bad:?}: {error}"
        );
    }
    assert!(
        !root.join("shadow").exists(),
        "a refused directory was created"
    );

    // A workspace inside the shadow repositories directory is refused too.
    let shadow = tempfile::tempdir().unwrap();
    let inner = shadow.path().join("workspace");
    fs::create_dir(&inner).unwrap();
    if outside_any_repository(&inner) {
        assert!(matches!(
            CheckpointManager::open_or_shadow(&inner, shadow.path()),
            Err(CheckpointError::InvalidShadowRepository { .. })
        ));
    }
}

/// Task worktrees work from a shadow repository like from a real one.
#[test]
fn a_task_worktree_is_provisioned_from_a_shadow_repository() {
    let folder = tempfile::tempdir().unwrap();
    let root = folder.path();
    if !outside_any_repository(root) {
        return;
    }
    write(root, "src/a.ts", b"a\n");
    let shadow = tempfile::tempdir().unwrap();
    let worktrees = tempfile::tempdir().unwrap();
    let manager = CheckpointManager::open_or_shadow(root, shadow.path())
        .unwrap()
        .with_worktrees_dir(worktrees.path())
        .unwrap();
    manager.create_checkpoint("task-wt", BEFORE, &[]).unwrap();
    match manager.provision_worktree("task-wt") {
        Ok(worktree) => {
            assert_eq!(read(&worktree.workspace, "src/a.ts"), b"a\n");
            assert!(!worktree.path.join(".git").exists());
        }
        Err(CheckpointError::CowUnavailable { .. }) => {} // fail closed, as with a repository
        Err(error) => panic!("{error}"),
    }
}
