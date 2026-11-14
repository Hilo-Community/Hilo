//! LOT-011 — invisible Git checkpoints and rollback, against real repositories: the `git` CLI
//! sets the repositories up and checks that `git log`, `git status` and `.git/index` stay as
//! they were.
#![allow(clippy::disallowed_methods)] // the `git` CLI inspects the repositories under test

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use host_core::{
    CheckpointError, CheckpointKind, CheckpointManager, RollbackConflict, RollbackOptions,
    UnsavedBuffer,
};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git must be installed to run the LOT-011 tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// A repository with one commit holding `files`.
fn repo_with(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Test User"]);
    git(
        dir.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    for (path, contents) in files {
        write(dir.path(), path, contents);
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "initial"]);
    dir
}

fn write(root: &Path, path: &str, contents: &[u8]) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn read(root: &Path, path: &str) -> Vec<u8> {
    fs::read(root.join(path)).unwrap()
}

fn canonical(root: &Path, path: &str) -> PathBuf {
    root.canonicalize().unwrap().join(path)
}

const BEFORE: CheckpointKind = CheckpointKind::BeforeAgent;
const AFTER: CheckpointKind = CheckpointKind::AfterAgent;

/// Blocking (spec 4.5, test 1): checkpoints leave `git log`, `git status` and `.git/index`
/// untouched, and a 1-click rollback restores 6 modified files exactly and deletes 2 new ones.
#[test]
fn blocking_checkpoints_do_not_pollute_git_and_rollback_restores_the_workspace() {
    let originals: Vec<(&str, &[u8])> = vec![
        ("src/a.ts", b"export const a = 1;\n"),
        ("src/b.ts", b"export const b = 2;\n"),
        ("src/nested/c.ts", b"export const c = 3;\n"),
        ("windows.txt", b"line one\r\nline two\r\n"),
        ("README.md", b"# Project\n"),
        ("package.json", b"{ \"name\": \"demo\" }\n"),
        (".gitignore", b".env\n"),
    ];
    let dir = repo_with(&originals);
    let root = dir.path();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        write(root, "run.sh", b"#!/bin/sh\necho hi\n");
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        git(root, &["add", "run.sh"]);
        git(root, &["commit", "-q", "-m", "script"]);
    }
    // The user's own state that must survive: a staged change, an untracked note, a secret.
    write(root, "staged.ts", b"export const staged = true;\n");
    git(root, &["add", "staged.ts"]);
    write(root, "notes.txt", b"untracked notes\n");
    write(root, ".env", b"SECRET=1\n");

    let log_before = git(root, &["log", "-n", "5", "--format=%H %s"]);
    let status_before = git(root, &["status", "--porcelain=v1", "-uall"]);
    let head_before = git(root, &["rev-parse", "HEAD"]);
    let index_before = fs::read(root.join(".git/index")).unwrap();

    let manager = CheckpointManager::open(root).unwrap();
    let c1 = manager.create_checkpoint("task-001", BEFORE, &[]).unwrap();
    assert_eq!(c1.seq, 1);
    assert_eq!(c1.reference, "refs/coloride/checkpoints/task-001/1");
    assert!(c1.skipped.is_empty(), "{:?}", c1.skipped);

    // The simulated task modifies 6 files and creates 2.
    let modified = [
        "src/a.ts",
        "src/b.ts",
        "src/nested/c.ts",
        "windows.txt",
        "README.md",
        "notes.txt",
    ];
    for path in modified {
        write(
            root,
            path,
            format!("// rewritten by the agent: {path}\n").as_bytes(),
        );
    }
    write(root, "src/new_module.ts", b"export const created = true;\n");
    write(root, "generated/output.json", b"{}\n");
    let a1 = manager.create_checkpoint("task-001", AFTER, &[]).unwrap();
    assert_eq!(a1.seq, 2);

    // Neither checkpoint shows up in the branch, HEAD, or the user's index.
    assert_eq!(git(root, &["log", "-n", "5", "--format=%H %s"]), log_before);
    assert!(!git(root, &["log", "--format=%s"]).contains("Coloride checkpoint"));
    assert_eq!(git(root, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(
        fs::read(root.join(".git/index")).unwrap(),
        index_before,
        ".git/index changed"
    );
    let refs = git(
        root,
        &["for-each-ref", "--format=%(refname)", "refs/coloride"],
    );
    assert_eq!(
        refs,
        "refs/coloride/checkpoints/task-001/1\nrefs/coloride/checkpoints/task-001/2\n"
    );
    // The checkpoint holds the untracked note and the staged file, never the ignored secret.
    let captured = git(root, &["ls-tree", "-r", "--name-only", &c1.reference]);
    assert!(captured.lines().any(|line| line == "notes.txt"));
    assert!(captured.lines().any(|line| line == "staged.ts"));
    assert!(!captured.lines().any(|line| line == ".env"));

    let report = manager
        .rollback_to_checkpoint("task-001", 1, &[], &RollbackOptions::default())
        .unwrap();
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(report.restored.len(), 6, "{report:?}");
    let removed: BTreeSet<_> = report.removed.iter().cloned().collect();
    assert_eq!(
        removed,
        BTreeSet::from([
            canonical(root, "src/new_module.ts"),
            canonical(root, "generated/output.json")
        ])
    );

    for (path, contents) in &originals {
        assert_eq!(read(root, path), *contents, "{path} not restored exactly");
    }
    assert_eq!(read(root, "notes.txt"), b"untracked notes\n");
    assert!(!root.join("src/new_module.ts").exists());
    assert!(!root.join("generated/output.json").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(root.join("run.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
    assert_eq!(git(root, &["log", "-n", "5", "--format=%H %s"]), log_before);
    assert_eq!(
        git(root, &["status", "--porcelain=v1", "-uall"]),
        status_before
    );

    // A second rollback finds nothing left to undo.
    let again = manager
        .rollback_to_checkpoint("task-001", 1, &[], &RollbackOptions::default())
        .unwrap();
    assert!(
        again.is_complete() && again.restored.is_empty() && again.removed.is_empty(),
        "{again:?}"
    );
}

/// Blocking (spec 4.5, test 2 — Section 5 blocking scenario): after the agent changed `a.ts`
/// and `b.ts`, the user edits `b.ts`; "Cancel task" restores `a.ts` and reports `b.ts` as a
/// conflict, keeping every one of the user's edits.
#[test]
fn blocking_rollback_never_overwrites_the_users_own_edits() {
    let dir = repo_with(&[
        ("a.ts", b"export const a = 'original';\n"),
        ("b.ts", b"export const b = 'original';\n"),
    ]);
    let root = dir.path();
    let manager = CheckpointManager::open(root).unwrap();

    manager.create_checkpoint("task-ab", BEFORE, &[]).unwrap();
    write(root, "a.ts", b"export const a = 'agent';\n");
    write(root, "b.ts", b"export const b = 'agent';\n");
    manager.create_checkpoint("task-ab", AFTER, &[]).unwrap();

    let user_edit = b"export const b = 'agent';\n// the user's own line\n";
    write(root, "b.ts", user_edit);

    let report = manager
        .rollback_to_checkpoint("task-ab", 1, &[], &RollbackOptions::default())
        .unwrap();
    assert_eq!(report.restored, vec![canonical(root, "a.ts")]);
    assert_eq!(
        report.conflicts,
        vec![RollbackConflict {
            path: canonical(root, "b.ts")
        }]
    );
    assert!(!report.is_complete());
    assert_eq!(read(root, "a.ts"), b"export const a = 'original';\n");
    assert_eq!(
        read(root, "b.ts"),
        user_edit,
        "the user's edit on b.ts was overwritten"
    );

    // Only an explicit confirmation of that path overwrites it.
    let options = RollbackOptions {
        overwrite_confirmed: BTreeSet::from([canonical(root, "b.ts")]),
    };
    let confirmed = manager
        .rollback_to_checkpoint("task-ab", 1, &[], &options)
        .unwrap();
    assert!(confirmed.is_complete(), "{confirmed:?}");
    assert_eq!(confirmed.restored, vec![canonical(root, "b.ts")]);
    assert_eq!(read(root, "b.ts"), b"export const b = 'original';\n");
}

/// An edit the user has not saved yet counts as theirs too.
#[test]
fn an_unsaved_user_edit_is_a_conflict_and_buffers_are_part_of_checkpoints() {
    let dir = repo_with(&[("a.ts", b"original a\n"), ("b.ts", b"original b\n")]);
    let root = dir.path();
    let manager = CheckpointManager::open(root).unwrap();

    // The checkpoint captures the open buffer of a.ts, not its stale disk content.
    let a_path = root.join("a.ts");
    let unsaved = [UnsavedBuffer {
        path: &a_path,
        text: "unsaved a\n",
    }];
    let c1 = manager
        .create_checkpoint("task-buf", BEFORE, &unsaved)
        .unwrap();
    assert_eq!(
        git(root, &["show", &format!("{}:a.ts", c1.reference)]),
        "unsaved a\n"
    );

    write(root, "a.ts", b"agent a\n");
    write(root, "b.ts", b"agent b\n");
    manager.create_checkpoint("task-buf", AFTER, &[]).unwrap();

    let b_path = root.join("b.ts");
    let open_now = [UnsavedBuffer {
        path: &b_path,
        text: "agent b\nuser typing\n",
    }];
    let report = manager
        .rollback_to_checkpoint("task-buf", 1, &open_now, &RollbackOptions::default())
        .unwrap();
    assert_eq!(
        report.conflicts,
        vec![RollbackConflict {
            path: canonical(root, "b.ts")
        }]
    );
    assert_eq!(report.restored, vec![canonical(root, "a.ts")]);
    assert_eq!(read(root, "a.ts"), b"unsaved a\n");
    assert_eq!(read(root, "b.ts"), b"agent b\n");
}

/// Edits the user made between two agent actions are theirs, even on a file the agent edits
/// later; files only the agent touched are restored from the first checkpoint.
#[test]
fn rollback_over_several_agent_actions_tells_agent_and_user_changes_apart() {
    let dir = repo_with(&[("x.ts", b"x0\n"), ("y.ts", b"y0\n"), ("z.ts", b"z0\n")]);
    let root = dir.path();
    let manager = CheckpointManager::open(root).unwrap();

    manager
        .create_checkpoint("task-multi", BEFORE, &[])
        .unwrap();
    write(root, "x.ts", b"x1 agent\n");
    manager.create_checkpoint("task-multi", AFTER, &[]).unwrap();
    write(root, "y.ts", b"y1 user\n");
    manager
        .create_checkpoint("task-multi", BEFORE, &[])
        .unwrap();
    write(root, "y.ts", b"y2 agent on top of the user\n");
    write(root, "z.ts", b"z2 agent\n");
    manager.create_checkpoint("task-multi", AFTER, &[]).unwrap();

    let report = manager
        .rollback_to_checkpoint("task-multi", 1, &[], &RollbackOptions::default())
        .unwrap();
    assert_eq!(
        report.restored,
        vec![canonical(root, "x.ts"), canonical(root, "z.ts")]
    );
    assert_eq!(
        report.conflicts,
        vec![RollbackConflict {
            path: canonical(root, "y.ts")
        }]
    );
    assert_eq!(read(root, "x.ts"), b"x0\n");
    assert_eq!(read(root, "y.ts"), b"y2 agent on top of the user\n");
    assert_eq!(read(root, "z.ts"), b"z0\n");

    // Rolling back only the second action restores y.ts to the user's version.
    let second = manager
        .rollback_to_checkpoint("task-multi", 3, &[], &RollbackOptions::default())
        .unwrap();
    assert_eq!(second.restored, vec![canonical(root, "y.ts")]);
    assert_eq!(read(root, "y.ts"), b"y1 user\n");
}

/// AGENTS.md rule 6: creating checkpoints runs no Git hook, even one the `git` CLI would run
/// on every reference update.
#[cfg(unix)]
#[test]
fn checkpoints_run_no_git_hook() {
    use std::os::unix::fs::PermissionsExt;
    let dir = repo_with(&[("a.ts", b"a\n")]);
    let root = dir.path();
    let marker = root.join("hook-ran");
    for hook in [
        "pre-commit",
        "post-commit",
        "reference-transaction",
        "post-checkout",
    ] {
        let path = root.join(".git/hooks").join(hook);
        fs::write(
            &path,
            format!("#!/bin/sh\necho {hook} >> '{}'\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let manager = CheckpointManager::open(root).unwrap();
    manager
        .create_checkpoint("task-hooks", BEFORE, &[])
        .unwrap();
    write(root, "a.ts", b"changed\n");
    manager.create_checkpoint("task-hooks", AFTER, &[]).unwrap();
    manager
        .rollback_to_checkpoint("task-hooks", 1, &[], &RollbackOptions::default())
        .unwrap();
    manager.discard_task("task-hooks").unwrap();
    assert!(
        !marker.exists(),
        "a hook ran: {}",
        fs::read_to_string(&marker).unwrap_or_default()
    );
}

#[test]
fn a_workspace_inside_a_larger_repository_only_covers_itself() {
    let dir = repo_with(&[("outside.ts", b"outside\n"), ("app/inside.ts", b"inside\n")]);
    let root = dir.path();
    let manager = CheckpointManager::open(&root.join("app")).unwrap();
    assert_eq!(manager.workspace_root(), canonical(root, "app"));

    let c1 = manager.create_checkpoint("task-sub", BEFORE, &[]).unwrap();
    let captured = git(root, &["ls-tree", "-r", "--name-only", &c1.reference]);
    assert_eq!(captured, "app/inside.ts\n");

    write(root, "outside.ts", b"changed by someone else\n");
    write(root, "app/inside.ts", b"agent\n");
    manager.create_checkpoint("task-sub", AFTER, &[]).unwrap();
    let report = manager
        .rollback_to_checkpoint("task-sub", 1, &[], &RollbackOptions::default())
        .unwrap();
    assert_eq!(report.restored, vec![canonical(root, "app/inside.ts")]);
    assert_eq!(read(root, "outside.ts"), b"changed by someone else\n");

    let outside = root.join("outside.ts");
    let error = manager
        .create_checkpoint(
            "task-sub",
            BEFORE,
            &[UnsavedBuffer {
                path: &outside,
                text: "x",
            }],
        )
        .unwrap_err();
    assert!(
        matches!(error, CheckpointError::OutsideWorkspace(_)),
        "{error}"
    );
}

#[test]
fn checkpoint_order_task_ids_and_missing_results_are_explicit_errors() {
    let dir = repo_with(&[("a.ts", b"a\n")]);
    let root = dir.path();
    let manager = CheckpointManager::open(root).unwrap();

    for bad in [
        "",
        "../escape",
        "a/b",
        ".hidden",
        "-flag",
        "x.lock",
        "a..b",
        "spaces here",
    ] {
        assert!(
            matches!(
                manager.create_checkpoint(bad, BEFORE, &[]),
                Err(CheckpointError::InvalidTaskId(_))
            ),
            "{bad:?} accepted"
        );
    }
    assert!(matches!(
        manager.create_checkpoint("task-order", AFTER, &[]),
        Err(CheckpointError::OutOfOrder { previous: None, .. })
    ));
    manager
        .create_checkpoint("task-order", BEFORE, &[])
        .unwrap();
    assert!(matches!(
        manager.create_checkpoint("task-order", BEFORE, &[]),
        Err(CheckpointError::OutOfOrder {
            previous: Some(CheckpointKind::BeforeAgent),
            ..
        })
    ));
    // The agent is still running: its changes cannot be told from the user's yet.
    write(root, "a.ts", b"half-written\n");
    assert!(matches!(
        manager.rollback_to_checkpoint("task-order", 1, &[], &RollbackOptions::default()),
        Err(CheckpointError::AgentResultMissing { .. })
    ));
    manager.create_checkpoint("task-order", AFTER, &[]).unwrap();
    assert!(matches!(
        manager.rollback_to_checkpoint("task-order", 2, &[], &RollbackOptions::default()),
        Err(CheckpointError::NotABeforeCheckpoint { .. })
    ));
    assert!(matches!(
        manager.rollback_to_checkpoint("task-order", 9, &[], &RollbackOptions::default()),
        Err(CheckpointError::UnknownCheckpoint { .. })
    ));
    let kinds: Vec<_> = manager
        .list_checkpoints("task-order")
        .unwrap()
        .iter()
        .map(|c| c.kind)
        .collect();
    assert_eq!(kinds, vec![BEFORE, AFTER]);
    assert_eq!(manager.discard_task("task-order").unwrap(), 2);
    assert!(manager.list_checkpoints("task-order").unwrap().is_empty());
    assert_eq!(git(root, &["for-each-ref", "refs/coloride"]), "");

    let not_a_repo = tempfile::tempdir().unwrap();
    // A temporary directory may itself sit inside a repository on some machines; only check
    // the error when it does not.
    if Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(not_a_repo.path())
        .output()
        .is_ok_and(|output| !output.status.success())
    {
        assert!(matches!(
            CheckpointManager::open(not_a_repo.path()),
            Err(CheckpointError::NotARepository(_))
        ));
    }
}

/// A symbolic link planted by the agent cannot redirect a restore outside the workspace.
#[cfg(unix)]
#[test]
fn a_restore_never_writes_through_a_symbolic_link() {
    let outside = tempfile::tempdir().unwrap();
    let dir = repo_with(&[("lib/a.ts", b"original\n")]);
    let root = dir.path();
    let manager = CheckpointManager::open(root).unwrap();
    manager.create_checkpoint("task-link", BEFORE, &[]).unwrap();

    // The agent replaces the `lib` directory by a link to a folder outside the workspace.
    fs::remove_dir_all(root.join("lib")).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("lib")).unwrap();
    manager.create_checkpoint("task-link", AFTER, &[]).unwrap();

    let report = manager
        .rollback_to_checkpoint("task-link", 1, &[], &RollbackOptions::default())
        .unwrap();
    assert!(
        !outside.path().join("a.ts").exists(),
        "wrote outside the workspace"
    );
    // The link itself is removed (the agent created it), and lib/a.ts is restored or reported.
    let restored_inside = root.join("lib").is_dir() && read(root, "lib/a.ts") == b"original\n";
    assert!(restored_inside || !report.failed.is_empty(), "{report:?}");
}
