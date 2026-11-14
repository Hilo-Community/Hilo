//! Engines that exist only for these tests. `PassthroughEngine` isolates NOTHING: it lets the
//! supervision (process groups, budgets, cleanup) be tested with real processes before the OS
//! engines exist. It is not part of the crate and cannot be reached from production code.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, Instant};

use sandbox_runner::{
    detect_native_os_sandbox, EngineCapabilities, OsSandboxEngine, SandboxError, ValidatedSpec,
};

pub fn capabilities(name: &'static str) -> EngineCapabilities {
    EngineCapabilities {
        name,
        kind: detect_native_os_sandbox(),
        enforces_memory_limit: false,
        enforces_process_limit: false,
        enforces_cpu_limit: false,
        supports_local_proxy: false,
    }
}

/// Runs the program as is (no isolation) with the environment the request asks for.
pub struct PassthroughEngine;

impl OsSandboxEngine for PassthroughEngine {
    fn capabilities(&self) -> EngineCapabilities {
        capabilities("passthrough-test")
    }
    fn self_test(&self) -> Result<(), SandboxError> {
        Ok(())
    }
    fn prepare(&self, spec: &ValidatedSpec) -> Result<Command, SandboxError> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.environment());
        Ok(command)
    }
}

/// Its self-test fails; `prepare` would run a canary if it were ever reached.
pub struct BrokenSelfTestEngine {
    pub canary: PathBuf,
}

impl OsSandboxEngine for BrokenSelfTestEngine {
    fn capabilities(&self) -> EngineCapabilities {
        capabilities("broken-self-test")
    }
    fn self_test(&self) -> Result<(), SandboxError> {
        Err(SandboxError::unavailable("injected sandbox failure"))
    }
    fn prepare(&self, _spec: &ValidatedSpec) -> Result<Command, SandboxError> {
        let mut command = Command::new("/usr/bin/touch");
        command.arg(&self.canary);
        Ok(command)
    }
}

/// Passes its self-test, then fails to isolate the request.
pub struct FailingPrepareEngine;

impl OsSandboxEngine for FailingPrepareEngine {
    fn capabilities(&self) -> EngineCapabilities {
        capabilities("failing-prepare")
    }
    fn self_test(&self) -> Result<(), SandboxError> {
        Ok(())
    }
    fn prepare(&self, _spec: &ValidatedSpec) -> Result<Command, SandboxError> {
        Err(SandboxError::unavailable(
            "the isolation could not be set up",
        ))
    }
}

/// Reports every optional capability, to test the paths that need them.
pub struct FullCapabilitiesEngine;

impl OsSandboxEngine for FullCapabilitiesEngine {
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            enforces_memory_limit: true,
            enforces_process_limit: true,
            enforces_cpu_limit: true,
            supports_local_proxy: true,
            ..capabilities("full-test")
        }
    }
    fn self_test(&self) -> Result<(), SandboxError> {
        Ok(())
    }
    fn prepare(&self, spec: &ValidatedSpec) -> Result<Command, SandboxError> {
        PassthroughEngine.prepare(spec)
    }
}

#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    // `ps -p` exits 0 only while the pid exists (a zombie has `Z` state: treat as dead).
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    output.status.success()
        && !String::from_utf8_lossy(&output.stdout)
            .trim()
            .starts_with('Z')
}

#[cfg(unix)]
pub fn wait_until_dead(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// A worktree with one file, canonical (macOS `/var` is `/private/var`).
pub fn worktree() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    (dir, root)
}

pub fn sh(root: &Path, script: &str) -> sandbox_runner::SandboxSpec {
    let mut spec = sandbox_runner::SandboxSpec::new(root, "/bin/sh");
    spec.args = vec!["-c".into(), script.into()];
    spec
}

/// Pids printed one per line by a script (`echo $!`).
pub fn pids(output: &[u8]) -> Vec<u32> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}
