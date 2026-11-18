#![cfg(unix)]

use sandbox_runner::{SandboxError, SandboxRunner, SandboxSpec};
use std::{fs, sync::Arc};

mod common;

#[test]
fn writable_directories_cannot_overlap_the_worktree_or_read_only_shares() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let private = dir.path().join("home");
    fs::create_dir(&work).unwrap();
    fs::create_dir(work.join("nested")).unwrap();
    fs::create_dir(&private).unwrap();
    fs::create_dir(private.join("nested")).unwrap();
    let work = work.canonicalize().unwrap();
    let runner = sandbox_runner::OsNativeSandbox::new(
        common::PassthroughEngine,
        Arc::new(sandbox_runner::MemoryAuditSink::default()),
    )
    .unwrap();
    for path in [&work, &work.join("nested"), &dir.path().to_path_buf()] {
        let mut spec = SandboxSpec::new(&work, "/bin/sh");
        spec.writable_paths = vec![path.clone()];
        assert!(matches!(
            runner.run(&spec),
            Err(SandboxError::InvalidSpec(_))
        ));
    }
    let mut spec = SandboxSpec::new(&work, "/bin/sh");
    spec.writable_paths = vec![private.clone()];
    spec.read_only_paths = vec![private.join("nested")];
    assert!(matches!(
        runner.run(&spec),
        Err(SandboxError::InvalidSpec(_))
    ));
}

#[test]
fn preference_domains_must_be_exact_reverse_dns_names() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().canonicalize().unwrap();
    let runner = sandbox_runner::OsNativeSandbox::new(
        common::PassthroughEngine,
        Arc::new(sandbox_runner::MemoryAuditSink::default()),
    )
    .unwrap();
    for domain in [
        "",
        "codex",
        "com.openai.*",
        "com..openai",
        ".com.openai",
        "com.openai.codex\") (allow default",
        "com.openai codex",
    ] {
        let mut spec = SandboxSpec::new(&work, "/bin/sh");
        spec.preference_domains = vec![domain.into()];
        assert!(
            matches!(runner.run(&spec), Err(SandboxError::InvalidSpec(_))),
            "{domain:?}"
        );
    }
}
