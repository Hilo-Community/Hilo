//! The Windows engine: an AppContainer for isolation, a Job Object for supervision.
//!
//! All the `unsafe` lives in the `sandbox-windows` crate; this module only describes the launch
//! and proves, at start-up, that the isolation works on this machine.
//!
//! * **Files:** the AppContainer has no access to the user's files. The worktree is the only
//!   directory it is granted, for the run only: an entry is added to its ACL (inherited by its
//!   content, so the cost grows with the number of files) and removed afterwards. System
//!   directories are readable by every AppContainer, so the OS and the programs installed there
//!   run; a toolchain installed under the user's profile is not readable and fails to start.
//! * **Shared read-only directories** (`read_only_paths`) are refused: sharing one would mean
//!   changing the permissions of the user's own files, and nothing here could restore them if
//!   the Host died in between. This engine says so instead of weakening the isolation.
//! * **Network:** no capability is granted, so there is no network at all, loopback included.
//!   `NetworkPolicy::LocalProxyOnly` is therefore refused (as on Linux): LOT-014 hands the
//!   agent another channel.
//! * **Limits:** memory, process count and CPU time come from the Job Object and are enforced.

use std::ffi::OsString;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sandbox_windows::{JobLimits, LaunchConfig};

use crate::engine::{EngineCapabilities, Launch, OsSandboxEngine};
use crate::error::SandboxError;
use crate::spec::{ResourceLimits, ValidatedSpec};
use crate::supervisor::{new_run_id, run_to_completion, RunOutput, SandboxedChild, TmpDir};
use crate::OsSandboxKind;

/// Variables that point programs at a temporary directory.
const TMP_VARS: [&str; 3] = ["TMPDIR", "TMP", "TEMP"];

/// The Windows engine. Stateless: each run gets its own AppContainer profile and Job Object.
#[derive(Debug, Clone, Default)]
pub struct AppContainerEngine;

impl AppContainerEngine {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// `\\?\C:\dir` to `C:\dir`: the verbatim form that `canonicalize` returns is not accepted as a
/// working directory by every program. Other forms (UNC, device paths) are kept as they are.
fn plain(path: &Path) -> PathBuf {
    if let Some(text) = path.to_str() {
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            let bytes = rest.as_bytes();
            if bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && bytes[2] == b'\\'
            {
                return PathBuf::from(rest);
            }
        }
    }
    path.to_path_buf()
}

fn launch_config(spec: &ValidatedSpec) -> Result<LaunchConfig, SandboxError> {
    if !spec.read_only_paths.is_empty() {
        return Err(SandboxError::InvalidSpec(
            "the Windows sandbox cannot share read-only directories: that would change the \
             permissions of files outside the worktree"
                .to_string(),
        ));
    }
    // The private temporary directory replaces any caller value, whatever its case: names are
    // case-insensitive on Windows and a duplicate would be refused.
    let tmp = plain(&spec.tmp_dir).into_os_string();
    let is_tmp_var = |key: &OsString| TMP_VARS.iter().any(|var| key.eq_ignore_ascii_case(var));
    let mut env: Vec<(OsString, OsString)> = spec
        .env
        .iter()
        .filter(|(key, _)| !is_tmp_var(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    env.extend(
        TMP_VARS
            .iter()
            .map(|var| (OsString::from(var), tmp.clone())),
    );
    Ok(LaunchConfig {
        program: plain(&spec.program),
        args: spec.args.clone(),
        cwd: plain(&spec.cwd),
        env,
        writable_dirs: vec![plain(&spec.worktree_root)],
        limits: JobLimits {
            max_processes: spec.limits.max_processes,
            max_memory_bytes: spec.limits.max_memory_bytes,
            max_cpu_time: spec.limits.max_cpu_time,
        },
        container_name: format!("coloride-{}", spec.run_id),
        stdin_piped: false,
    })
}

impl OsSandboxEngine for AppContainerEngine {
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            name: "appcontainer",
            kind: OsSandboxKind::WindowsAppContainerJobObject,
            enforces_memory_limit: true,
            enforces_process_limit: true,
            enforces_cpu_limit: true,
            supports_local_proxy: false,
        }
    }

    fn self_test(&self) -> Result<(), SandboxError> {
        self_test()
    }

    fn prepare(&self, _spec: &ValidatedSpec) -> Result<Command, SandboxError> {
        // An AppContainer cannot be expressed as a wrapped command, and a plain command would
        // run unisolated: this engine only ever launches through `prepare_launch`.
        Err(SandboxError::unavailable(
            "the AppContainer engine launches through `prepare_launch`",
        ))
    }

    fn prepare_launch(&self, spec: &ValidatedSpec) -> Result<Launch, SandboxError> {
        Ok(Launch::AppContainer(Box::new(launch_config(spec)?)))
    }
}

/// Removes the self-test's files when dropped.
struct ProbeDir(PathBuf);

impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn probe_failure(what: &str, detail: impl std::fmt::Display) -> SandboxError {
    SandboxError::unavailable(format!("AppContainer self-test: {what} ({detail})"))
}

/// The self-test's throw-away layout: `work` is the probe's worktree, `outside` a sibling it
/// must not reach.
struct Probe {
    _cleanup: ProbeDir,
    system_root: OsString,
    system32: PathBuf,
    work: PathBuf,
    outside: PathBuf,
}

impl Probe {
    fn new() -> Result<Self, SandboxError> {
        let system_root = std::env::var_os("SystemRoot")
            .ok_or_else(|| SandboxError::unavailable("the SystemRoot variable is not set"))?;
        let base = std::env::temp_dir().join(format!("coloride-selftest-{}", new_run_id()));
        let cleanup = ProbeDir(base.clone());
        let (work, outside) = (base.join("work"), base.join("outside"));
        fs::create_dir_all(&work)?;
        fs::create_dir_all(&outside)?;
        fs::write(outside.join(SECRET_FILE), SECRET)?;
        fs::write(work.join("input.txt"), "readable")?;
        Ok(Self {
            _cleanup: cleanup,
            system32: Path::new(&system_root).join("System32"),
            system_root,
            work,
            outside,
        })
    }

    /// Runs a program of `System32` in a fresh AppContainer, under the regular supervision.
    fn run(&self, program: &str, args: &[&str]) -> Result<RunOutput, SandboxError> {
        let tmp = TmpDir::create(&self.work)?;
        let tmp_path = tmp.path().as_os_str().to_os_string();
        let config = LaunchConfig {
            program: self.system32.join(program),
            args: args.iter().map(OsString::from).collect(),
            cwd: self.work.clone(),
            env: vec![
                ("SystemRoot".into(), self.system_root.clone()),
                ("TEMP".into(), tmp_path.clone()),
                ("TMP".into(), tmp_path),
            ],
            writable_dirs: vec![self.work.clone()],
            limits: JobLimits::default(),
            container_name: format!("coloride-selftest-{}", new_run_id()),
            stdin_piped: false,
        };
        let mut child = SandboxedChild::spawn_prepared(
            Launch::AppContainer(Box::new(config)),
            tmp,
            None,
            false,
            None,
        )
        .map_err(|error| probe_failure(&format!("{program} could not start"), error))?;
        run_to_completion(&mut child, &PROBE_LIMITS)
    }
}

const SECRET_FILE: &str = "secret.txt";
const SECRET: &str = "COLORIDE-SELF-TEST-SECRET";

const PROBE_LIMITS: ResourceLimits = ResourceLimits {
    wall_timeout: Duration::from_secs(30),
    max_output_bytes: 64 * 1024,
    max_processes: None,
    max_memory_bytes: None,
    max_cpu_time: None,
};

fn stdout(output: &RunOutput) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Proves on this machine that the AppContainer isolates. Positive controls first (the probe
/// programs run and can use their worktree), so that nothing passes merely because nothing runs;
/// then the negative controls, which are the isolation itself.
fn self_test() -> Result<(), SandboxError> {
    let probe = Probe::new()?;

    // Positive controls.
    let made = probe.run("cmd.exe", &["/d", "/c", "echo ok>probe.txt"])?;
    let written = fs::read_to_string(probe.work.join("probe.txt")).unwrap_or_default();
    if !made.termination.success() || written.trim() != "ok" {
        return Err(probe_failure(
            "a process cannot write its worktree",
            format!("{:?}", made.termination),
        ));
    }
    let read = probe.run("findstr.exe", &["readable", "input.txt"])?;
    if !read.termination.success() || !stdout(&read).contains("readable") {
        return Err(probe_failure(
            "a process cannot read its worktree",
            format!("{:?}", read.termination),
        ));
    }
    let curl = probe.run("curl.exe", &["--version"])?;
    if !curl.termination.success() {
        return Err(probe_failure(
            "the network probe tool does not run in the sandbox",
            format!("{:?}", curl.termination),
        ));
    }

    // Negative controls: files. Paths are relative so that `cmd` needs no quoting.
    let leaked = probe.run("cmd.exe", &["/d", "/c", r"type ..\outside\secret.txt"])?;
    if stdout(&leaked).contains(SECRET) {
        return Err(probe_failure(
            "a process can read outside its worktree",
            SECRET_FILE,
        ));
    }
    probe.run("cmd.exe", &["/d", "/c", r"echo x>..\outside\escape.txt"])?;
    if probe.outside.join("escape.txt").exists() {
        return Err(probe_failure(
            "a process can write outside its worktree",
            "escape.txt",
        ));
    }

    // Negative control: network, loopback included.
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.set_nonblocking(true).map(|()| listener))
        .map_err(|error| probe_failure("could not open a probe listener", error))?;
    let port = listener
        .local_addr()
        .map_err(|error| probe_failure("could not read the probe listener address", error))?
        .port();
    let url = format!("http://127.0.0.1:{port}/");
    probe.run("curl.exe", &["--silent", "--max-time", "5", &url])?;
    if listener.accept().is_ok() {
        return Err(probe_failure(
            "a process can open a network connection",
            url,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::plain;
    use std::path::Path;

    #[test]
    fn verbatim_drive_paths_become_plain_and_others_are_kept() {
        assert_eq!(plain(Path::new(r"\\?\C:\work")), Path::new(r"C:\work"));
        assert_eq!(
            plain(Path::new(r"\\?\UNC\server\share")),
            Path::new(r"\\?\UNC\server\share")
        );
        assert_eq!(plain(Path::new(r"C:\work")), Path::new(r"C:\work"));
    }
}
