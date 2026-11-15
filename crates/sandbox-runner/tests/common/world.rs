//! The fake world of the OS sandbox scenarios: a FAKE home holding fake secrets (the real
//! `~/.ssh` and friends are never touched), a worktree inside it as in production, a directory
//! next to the home, and helpers to run shell probes.

use std::fs;
use std::path::{Path, PathBuf};

use sandbox_runner::{RunOutput, SandboxSpec};

pub struct World {
    _dir: tempfile::TempDir,
    pub home: PathBuf,
    /// Inside the home, where the app keeps task worktrees.
    pub worktree: PathBuf,
    /// A worktree next to the home, not inside it: the home can be shared with it.
    pub beside: PathBuf,
    /// Another project of the user, in the home.
    pub outside: PathBuf,
}

/// A fake home with fake secrets; the worktree is `worktree_in_home` under it (the app's cache
/// directory on this OS).
pub fn world(worktree_in_home: &str) -> World {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let home = base.join("home");
    for (path, secret) in [
        (".ssh/id_rsa", "SSH-SECRET"),
        (".aws/credentials", "AWS-SECRET"),
        (".gnupg/private.key", "GPG-SECRET"),
        ("Library/Keychains/login.keychain-db", "KEYCHAIN-SECRET"),
        (".netrc", "NETRC-SECRET"),
        (".config/gh/hosts.yml", "GH-SECRET"),
        ("work/other/other.txt", "OTHER-PROJECT"),
    ] {
        let path = home.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, secret).unwrap();
    }
    let worktree = home.join(worktree_in_home);
    fs::create_dir_all(&worktree).unwrap();
    fs::write(worktree.join("a.txt"), "a").unwrap();
    let beside = base.join("wt");
    fs::create_dir_all(&beside).unwrap();
    let outside = home.join("work");
    World {
        _dir: dir,
        home,
        worktree,
        beside,
        outside,
    }
}

/// `/bin/sh -c script` in the world's worktree, with `args` as `$1`, `$2`...
pub fn sh(world: &World, script: &str, args: &[&Path]) -> SandboxSpec {
    sh_in(&world.worktree, script, args)
}

/// `/bin/sh -c script` in the worktree `root`.
pub fn sh_in(root: &Path, script: &str, args: &[&Path]) -> SandboxSpec {
    let mut spec = SandboxSpec::new(root, "/bin/sh");
    spec.args = vec!["-c".into(), script.into(), "script".into()];
    spec.args.extend(args.iter().map(|arg| arg.into()));
    spec
}

/// `try NAME cmd...` prints DENIED:NAME or LEAKED:NAME. Absolute commands only: the environment
/// of a sandboxed process is empty.
pub const TRY: &str = r#"try() { n="$1"; shift; if "$@" >/dev/null 2>&1; then echo "LEAKED:$n"; else echo "DENIED:$n"; fi; }
"#;

/// The lines a run printed.
pub fn lines(output: &RunOutput) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}
