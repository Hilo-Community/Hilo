//! What a caller asks the sandbox to run, and its validated form.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::SandboxError;
use crate::path_guard::PathGuard;

/// Name of the private temporary directory created inside the worktree for each run.
pub const TMP_DIR_NAME: &str = ".hilo-tmp";

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
    /// Hide source file data from an agent: only its executable and private scratch are readable.
    pub mediate_worktree_reads: bool,
    /// ACP agents can propose changes but cannot write the task worktree themselves.
    pub worktree_read_only: bool,
    /// The task worktree; read-only when `worktree_read_only` is set.
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
    /// Host-owned private directories. Must be disjoint from the worktree and read-only
    /// shares. Never use the user's real home here.
    pub writable_paths: Vec<PathBuf>,
    /// Worktree paths whose data remains forbidden even when direct agent reads are allowed.
    pub denied_read_paths: Vec<PathBuf>,
    /// macOS only: exact preference domains the process may read through `cfprefsd` (some
    /// programs refuse to start when they cannot read their managed preferences). Reading,
    /// never writing, and no other domain. Other engines have no such service and ignore it.
    pub preference_domains: Vec<String>,
    /// macOS only: loopback TCP ports the process may listen on and be reached on, for a
    /// sign-in that returns to `http://localhost:<port>/...` in the user's browser. Only these
    /// ports, only on loopback; outbound traffic is not widened. Other engines ignore it.
    pub loopback_listen_ports: Vec<u16>,
    pub network: NetworkPolicy,
    pub limits: ResourceLimits,
}

impl SandboxSpec {
    /// A request with no network, default limits and `cwd` at the worktree root.
    pub fn new(worktree_root: impl Into<PathBuf>, program: impl Into<PathBuf>) -> Self {
        let worktree_root = worktree_root.into();
        Self {
            mediate_worktree_reads: false,
            worktree_read_only: false,
            cwd: worktree_root.clone(),
            worktree_root,
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            read_only_paths: Vec::new(),
            writable_paths: Vec::new(),
            denied_read_paths: Vec::new(),
            preference_domains: Vec::new(),
            loopback_listen_ports: Vec::new(),
            network: NetworkPolicy::Denied,
            limits: ResourceLimits::default(),
        }
    }
}

/// A request whose paths were resolved and checked. Engines receive only this.
#[derive(Debug, Clone)]
pub struct ValidatedSpec {
    /// Allocated by the runner, never accepted from a caller. Only this PTY can be controlled.
    pub terminal_device: Option<PathBuf>,
    pub mediate_worktree_reads: bool,
    pub worktree_read_only: bool,
    /// Canonical.
    pub worktree_root: PathBuf,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Canonical, inside `worktree_root`, an existing directory.
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
    /// Canonical, outside the worktree.
    pub read_only_paths: Vec<PathBuf>,
    /// Canonical existing directories, disjoint from the worktree and read-only shares.
    pub writable_paths: Vec<PathBuf>,
    pub denied_read_paths: Vec<PathBuf>,
    /// Checked: exact reverse-DNS names.
    pub preference_domains: Vec<String>,
    /// Checked: unprivileged ports, few.
    pub loopback_listen_ports: Vec<u16>,
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
        if self.mediate_worktree_reads && !self.worktree_read_only {
            return Err(invalid("Host-mediated reads require a read-only worktree"));
        }
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
        let mut writable_paths = Vec::new();
        for path in &self.writable_paths {
            let real = path
                .canonicalize()
                .map_err(|_| invalid("a writable path does not exist"))?;
            if !real.is_dir() || real.parent().is_none() {
                return Err(invalid(
                    "a writable path must be a directory other than the filesystem root",
                ));
            }
            let overlaps = |other: &PathBuf| real.starts_with(other) || other.starts_with(&real);
            if overlaps(&worktree_root) {
                return Err(invalid("a writable path overlaps the worktree"));
            }
            if read_only_paths.iter().any(overlaps) {
                return Err(invalid("a writable path overlaps a read-only share"));
            }
            if writable_paths.iter().any(overlaps) {
                return Err(invalid("writable paths overlap"));
            }
            writable_paths.push(real);
        }
        let denied_read_paths = self
            .denied_read_paths
            .iter()
            .map(|path| {
                let real = PathGuard::verify_within_worktree(&worktree_root, path)?;
                if real == worktree_root {
                    return Err(invalid("the whole worktree cannot be a denied read path"));
                }
                Ok(real)
            })
            .collect::<Result<Vec<_>, SandboxError>>()?;
        if !self
            .preference_domains
            .iter()
            .all(|domain| valid_preference_domain(domain))
        {
            return Err(invalid(
                "a preference domain must be an exact reverse-DNS name",
            ));
        }
        if self.loopback_listen_ports.len() > 4
            || self.loopback_listen_ports.iter().any(|port| *port < 1024)
        {
            return Err(invalid(
                "at most four loopback ports, none below 1024, may be listened on",
            ));
        }
        Ok(ValidatedSpec {
            terminal_device: None,
            mediate_worktree_reads: self.mediate_worktree_reads,
            worktree_read_only: self.worktree_read_only,
            worktree_root,
            program: self.program.clone(),
            args: self.args.clone(),
            cwd,
            env: self.env.clone(),
            read_only_paths,
            writable_paths,
            denied_read_paths,
            preference_domains: self.preference_domains.clone(),
            loopback_listen_ports: self.loopback_listen_ports.clone(),
            network: self.network,
            limits: self.limits,
            tmp_dir: PathBuf::new(),
            run_id: String::new(),
        })
    }
}

/// `com.example.app`: two or more labels of ASCII letters, digits and `-`, no wildcard, no
/// leading or trailing dot. Exact domains only; they are written verbatim into the profile.
fn valid_preference_domain(domain: &str) -> bool {
    domain.len() <= 255
        && domain.contains('.')
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}
