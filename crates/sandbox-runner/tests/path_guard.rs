//! LOT-012 scenario 1 (host side): a path chosen by an agent must stay in the worktree.

use std::fs;
use std::path::Path;

use sandbox_runner::{PathGuard, PathGuardError};

mod common;
use common::worktree;

fn verify(root: &Path, candidate: &str) -> Result<std::path::PathBuf, PathGuardError> {
    PathGuard::verify_within_worktree(root, Path::new(candidate))
}

#[test]
fn paths_inside_the_worktree_resolve_even_when_they_do_not_exist_yet() {
    let (_dir, root) = worktree();
    fs::create_dir_all(root.join("src/deep")).unwrap();
    assert_eq!(verify(&root, "a.txt").unwrap(), root.join("a.txt"));
    assert_eq!(verify(&root, "./src/deep").unwrap(), root.join("src/deep"));
    assert_eq!(
        verify(&root, "src/new/dir/file.ts").unwrap(),
        root.join("src/new/dir/file.ts")
    );
    // An absolute path inside the root, and the root itself.
    assert_eq!(
        PathGuard::verify_within_worktree(&root, &root.join("a.txt")).unwrap(),
        root.join("a.txt")
    );
    assert_eq!(verify(&root, ".").unwrap(), root);
}

#[test]
fn parent_directory_segments_are_refused_wherever_they_are() {
    let (_dir, root) = worktree();
    fs::create_dir_all(root.join("src")).unwrap();
    for bad in [
        "../outside.txt",
        "src/../../outside.txt",
        "src/../a.txt",
        "new/../../x",
    ] {
        assert!(
            matches!(verify(&root, bad), Err(PathGuardError::ParentDirectory(_))),
            "{bad} accepted"
        );
    }
}

#[test]
fn absolute_paths_outside_the_worktree_are_refused() {
    let (_dir, root) = worktree();
    for bad in ["/etc/passwd", "/tmp", "/"] {
        assert!(
            matches!(verify(&root, bad), Err(PathGuardError::Escapes { .. })),
            "{bad} accepted"
        );
    }
    // A sibling whose name merely starts with the root's.
    let sibling = format!("{}-evil/file", root.display());
    assert!(matches!(
        verify(&root, &sibling),
        Err(PathGuardError::Escapes { .. })
    ));
}

#[test]
fn a_missing_root_is_an_error() {
    let (_dir, root) = worktree();
    assert!(matches!(
        PathGuard::verify_within_worktree(&root.join("missing"), Path::new("a")),
        Err(PathGuardError::BadRoot { .. })
    ));
    assert!(matches!(
        PathGuard::verify_within_worktree(&root.join("a.txt"), Path::new("a")),
        Err(PathGuardError::BadRoot { .. })
    ));
}

#[cfg(unix)]
mod symlinks {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn a_link_to_a_file_outside_is_refused_but_a_link_inside_is_fine() {
        let (_dir, root) = worktree();
        let (_other, outside) = worktree();
        symlink("/etc/passwd", root.join("escape")).unwrap();
        symlink(outside.join("a.txt"), root.join("to-outside")).unwrap();
        symlink("a.txt", root.join("inside")).unwrap();
        for bad in ["escape", "to-outside"] {
            assert!(
                matches!(verify(&root, bad), Err(PathGuardError::Escapes { .. })),
                "{bad} accepted"
            );
        }
        assert_eq!(verify(&root, "inside").unwrap(), root.join("a.txt"));
    }

    #[test]
    fn a_link_to_a_directory_outside_blocks_everything_below_it() {
        let (_dir, root) = worktree();
        let (_other, outside) = worktree();
        symlink(&outside, root.join("lib")).unwrap();
        for bad in ["lib", "lib/a.txt", "lib/new/file.ts"] {
            assert!(
                matches!(verify(&root, bad), Err(PathGuardError::Escapes { .. })),
                "{bad} accepted"
            );
        }
    }

    #[test]
    fn a_chain_of_links_is_followed_to_its_end() {
        let (_dir, root) = worktree();
        let (_other, outside) = worktree();
        symlink(&outside, root.join("hop2")).unwrap();
        symlink("hop2", root.join("hop1")).unwrap();
        assert!(matches!(
            verify(&root, "hop1/a.txt"),
            Err(PathGuardError::Escapes { .. })
        ));
    }

    #[test]
    fn a_link_pointing_nowhere_is_refused_because_writing_through_it_would_create_its_target() {
        let (_dir, root) = worktree();
        symlink("/tmp/coloride-never-created", root.join("dangling")).unwrap();
        symlink("missing-inside", root.join("dangling-inside")).unwrap();
        for bad in ["dangling", "dangling/file", "dangling-inside"] {
            assert!(
                matches!(verify(&root, bad), Err(PathGuardError::DanglingSymlink(_))),
                "{bad} accepted"
            );
        }
    }

    #[test]
    fn the_root_may_be_reached_through_an_alias() {
        let (_dir, root) = worktree();
        let alias = root.parent().unwrap().join(format!(
            "alias-{}",
            root.file_name().unwrap().to_string_lossy()
        ));
        symlink(&root, &alias).unwrap();
        let resolved = PathGuard::verify_within_worktree(&alias, &alias.join("a.txt")).unwrap();
        assert_eq!(resolved, root.join("a.txt"));
        fs::remove_file(alias).unwrap();
    }
}
