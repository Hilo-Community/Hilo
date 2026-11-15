//! What a caller asks the sandbox to run, and its validated form.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::SandboxError;
use crate::path_guard::PathGuard;

/// Name of the private temporary directory created inside the worktree for each run.
pub const TMP_DIR_NAME: &str = ".coloride-tmp";

/// Outbound network access of the sandboxed process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// No network at all (the default for every command).
    Denied,
    /// Only this loopback address: the local proxy that the Model Gateway runs outside the
    /// sandbox (`LOT-014`) and that applies the domain allowlist.
    LocalProxyOnly(SocketAddr),
}

/// Budgets of a run. Timeout and output budget are always enforced by the runner; the others
/// only when the engine reports it can (see `EngineCapabilities`), otherwise the request is
/// refused instead of running with less protection than asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceLimits {
    /// For `run`: the longest the command may take. For `spawn`: the longest the process may live.
    pub wall_timeout: Duration,
    /// Per stream (`stdout`, `stderr`). `run` stops the process tree when it is exceeded;
    /// `spawn` hands the pipes to the caller, who owns that budget.
    pub max_output_bytes: u64,
    pub max_processes: Option<u32>,
    pub max_memory_bytes: Option<u64>,
    pub max_cpu_time: Option<Duration>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            wall_timeout: Duration::from_secs(300),
            max_output_bytes: 16 * 1024 * 1024,
            max_processes: None,
            max_memory_bytes: None,
            max_cpu_time: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SandboxSpec {
    /// The task worktree: the only place the process may write.
    pub worktree_root: PathBuf,
    /// Absolute path of the program. It is never looked up in `PATH`.
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Working directory, inside the worktree.
    pub cwd: PathBuf,
    /// The complete environment: nothing is inherited from the Host (no tokens, no `HOME`).
    pub env: BTreeMap<OsString, OsString>,
    /// Directories outside the worktree the process may read, never write (shared
    /// `node_modules`, toolchains).
    pub read_only_paths: Vec<PathBuf>,
    pub network: NetworkPolicy,
    pub limits: ResourceLimits,
}

impl SandboxSpec {
    /// A request with no network, default limits and `cwd` at the worktree root.
    pub fn new(worktree_root: impl Into<PathBuf>, program: impl Into<PathBuf>) -> Self {
        let worktree_root = worktree_root.into();
        Self {
            cwd: worktree_root.clone(),
            worktree_root,
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            read_only_paths: Vec::new(),
            network: NetworkPolicy::Denied,
            limits: ResourceLimits::default(),
        }
    }
}

/// A request whose paths were resolved and checked. Engines receive only this.
#[derive(Debug, Clone)]
pub struct ValidatedSpec {
    /// Canonical.
    pub worktree_root: PathBuf,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Canonical, inside `worktree_root`, an existing directory.
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    /// Canonical, outside the worktree.
    pub read_only_paths: Vec<PathBuf>,
    pub network: NetworkPolicy,
    pub limits: ResourceLimits,
    /// Private, writable temporary directory inside the worktree, purged when the run ends.
    pub tmp_dir: PathBuf,
    /// Unique to this run (letters, digits and `-` only): engines use it to tag what the OS
    /// reports about it.
    pub run_id: String,
}

impl ValidatedSpec {
    /// The environment of the process: `env`, with the temporary directory variables pointing
    /// at the private `tmp_dir` (build tools need a writable one).
    pub fn environment(&self) -> BTreeMap<OsString, OsString> {
        let mut env = self.env.clone();
        for key in ["TMPDIR", "TMP", "TEMP"] {
            env.insert(key.into(), self.tmp_dir.clone().into_os_string());
        }
        env
    }
}

impl SandboxSpec {
    /// Checks the request. `tmp_dir` is filled in by the runner once it has created it.
    pub(crate) fn validate(&self) -> Result<ValidatedSpec, SandboxError> {
        let invalid = |reason: &str| SandboxError::InvalidSpec(reason.to_string());
        if !self.program.is_absolute() {
            return Err(invalid("the program must be an absolute path"));
        }
        if self.limits.wall_timeout.is_zero() || self.limits.max_output_bytes == 0 {
            return Err(invalid(
                "the timeout and the output budget must not be zero",
            ));
        }
        if self.limits.max_processes == Some(0)
            || self.limits.max_memory_bytes == Some(0)
            || self.limits.max_cpu_time.is_some_and(|time| time.is_zero())
        {
            return Err(invalid(
                "a limit of zero would forbid the process from running",
            ));
        }
        if let NetworkPolicy::LocalProxyOnly(addr) = self.network {
            if !addr.ip().is_loopback() {
                return Err(invalid("the network proxy must be on a loopback address"));
            }
        }
        for (key, value) in &self.env {
            let key_text = key.to_string_lossy();
            if key_text.is_empty() || key_text.contains(['=', '\0']) {
                return Err(invalid("an environment variable has an invalid name"));
            }
            if value.to_string_lossy().contains('\0') {
                return Err(invalid("an environment variable has a NUL in its value"));
            }
        }

        let worktree_root = self.worktree_root.canonicalize().map_err(|source| {
            crate::path_guard::PathGuardError::BadRoot {
                path: self.worktree_root.clone(),
                source,
            }
        })?;
        let cwd = PathGuard::verify_within_worktree(&worktree_root, &self.cwd)?;
        if !cwd.is_dir() {
            return Err(invalid(
                "the working directory is not an existing directory",
            ));
        }
        let mut read_only_paths = Vec::new();
        for path in &self.read_only_paths {
            let real = path
                .canonicalize()
                .map_err(|_| invalid("a read-only path does not exist"))?;
            if real.parent().is_none() {
                return Err(invalid("the whole file system cannot be shared read-only"));
            }
            if real.starts_with(&worktree_root) {
                return Err(invalid(
                    "a read-only path is inside the worktree, which is writable",
                ));
            }
            if worktree_root.starts_with(&real) {
                return Err(invalid("a read-only path contains the worktree"));
            }
            read_only_paths.push(real);
        }
        Ok(ValidatedSpec {
            worktree_root,
            program: self.program.clone(),
            args: self.args.clone(),
            cwd,
            env: self.env.clone(),
            read_only_paths,
            network: self.network,
            limits: self.limits,
            tmp_dir: PathBuf::new(),
            run_id: String::new(),
        })
    }
}
