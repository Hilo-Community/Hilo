//! macOS engine: Seatbelt, through `/usr/bin/sandbox-exec`.
//!
//! The profile denies **everything** by default and allows the least a build tool needs:
//!
//! * read the system (`/usr`, `/bin`, `/System`, `/Library`, `/opt`, ...), the task worktree, and
//!   the read-only paths of the request (a shared `node_modules`, a toolchain); nothing else:
//!   the user's home, `/Users`, `/Volumes`, `/tmp` are not readable at all;
//! * write the worktree (its private `TMPDIR` is inside it) and a few `/dev` files;
//! * no network, except `localhost:<port>` of the local proxy when the request asks for it;
//! * no signal to another process, no pasteboard, no Keychain service, no other Mach service.
//!
//! The last rules deny the secrets of the user (`~/.ssh`, `~/.aws`, `~/.gnupg`, the Keychain
//! folders, registry and cloud tokens) and the container sockets. They come last because the last
//! matching rule wins: even a read-only path that contains them by mistake cannot expose them.
//!
//! Every path reaches the profile as a `-D` parameter (`(param "WT")`), never as text, so a path
//! cannot alter the profile. Apple marks `sandbox-exec` deprecated; it still ships with macOS and
//! is what the tools of the major agents use. Its start-up costs about 9 ms, above the 5 ms
//! target of the spec (see the performance test).
//!
//! Limits: the CPU budget is enforced (`ulimit -t`); memory and process-count limits are not
//! (macOS has no per-sandbox quota without private APIs), so a request for them is refused.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use crate::audit::{AuditSink, SecurityAuditEvent, SecurityEventKind};
use crate::common::{check_program_is_readable, cpu_seconds, CONTAINER_SOCKETS, HOME_SECRETS};
use crate::engine::{EngineCapabilities, OsSandboxEngine, RunInfo};
use crate::error::SandboxError;
use crate::probe;
use crate::spec::{NetworkPolicy, ValidatedSpec};
use crate::OsSandboxKind;

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const LOG: &str = "/usr/bin/log";

/// What every process may read, besides its worktree and its read-only paths.
const SYSTEM_READ_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/opt",
    "/private/etc",
    "/private/var/select",
    "/dev",
    "/Applications/Xcode.app/Contents/Developer",
];
/// The only files outside the worktree a process may write.
const WRITABLE_DEVICES: &[&str] = &["/dev/null", "/dev/dtracehelper", "/dev/tty"];
/// Denied everywhere: the system Keychain (and, from `common`, the container sockets).
const SYSTEM_SECRETS: &[&str] = &["/Library/Keychains"];
/// How long the unified log needs to publish what the kernel just reported.
const LOG_SETTLE: Duration = Duration::from_millis(1200);
/// A run reports at most this many distinct denials.
const MAX_DENIALS_PER_RUN: usize = 64;

#[derive(Debug, Clone)]
pub struct SeatbeltEngine {
    /// Canonical home directory whose secrets are denied.
    home: PathBuf,
    sandbox_exec: PathBuf,
}

impl SeatbeltEngine {
    /// `home` is the directory of the user whose secrets must stay out of reach.
    pub fn new(home: &Path) -> Result<Self, SandboxError> {
        let home = home.canonicalize().map_err(|error| {
            SandboxError::unavailable(format!("the home directory {home:?} is unusable: {error}"))
        })?;
        if !home.is_dir() || home.parent().is_none() {
            return Err(SandboxError::unavailable(
                "the home directory must be an existing directory other than /",
            ));
        }
        Ok(Self {
            home,
            sandbox_exec: PathBuf::from(SANDBOX_EXEC),
        })
    }

    /// The home directory of the current user, from `$HOME`. Without it the secrets to protect
    /// are unknown, so there is no engine.
    pub fn for_current_user() -> Result<Self, SandboxError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .ok_or_else(|| SandboxError::unavailable("HOME is not set to an absolute path"))?;
        Self::new(&home)
    }

    /// Uses another `sandbox-exec` binary (a missing one makes the self-test fail, closed).
    #[must_use]
    pub fn with_sandbox_exec_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.sandbox_exec = path.into();
        self
    }
}

/// A rendered profile and the `-D` parameters it refers to.
#[derive(Debug)]
pub(crate) struct Profile {
    pub text: String,
    pub params: Vec<(String, PathBuf)>,
}

impl Profile {
    /// Registers `path` and returns `(subpath (param "NAME"))`.
    fn subpath(&mut self, prefix: &str, path: &Path) -> String {
        let name = format!("{prefix}{}", self.params.len());
        self.params.push((name.clone(), path.to_path_buf()));
        format!("(subpath (param \"{name}\"))")
    }

    fn literal(&mut self, prefix: &str, path: &Path) -> String {
        let name = format!("{prefix}{}", self.params.len());
        self.params.push((name.clone(), path.to_path_buf()));
        format!("(literal (param \"{name}\"))")
    }
}

/// Every path the process may read: the system, the worktree, the read-only paths.
fn readable_roots(spec: &ValidatedSpec) -> Vec<PathBuf> {
    SYSTEM_READ_ROOTS
        .iter()
        .map(PathBuf::from)
        .chain(std::iter::once(spec.worktree_root.clone()))
        .chain(spec.read_only_paths.iter().cloned())
        .collect()
}

pub(crate) fn render_profile(spec: &ValidatedSpec, home: &Path) -> Result<Profile, SandboxError> {
    // The tag is the only run-dependent text in the profile: keep it inert.
    if spec.run_id.is_empty()
        || !spec
            .run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(SandboxError::InvalidSpec(
            "the run id must be letters, digits and '-'".to_string(),
        ));
    }
    let mut profile = Profile {
        text: String::new(),
        params: Vec::new(),
    };
    let system: Vec<String> = SYSTEM_READ_ROOTS
        .iter()
        .map(|root| format!("(subpath \"{root}\")"))
        .collect();
    let system = system.join(" ");
    let worktree = profile.subpath("WT", &spec.worktree_root);
    let read_only: Vec<String> = spec
        .read_only_paths
        .iter()
        .map(|path| profile.subpath("RO", path))
        .collect();

    // Ancestors of everything allowed: `stat` of each directory on the way, nothing more (so
    // that `cd` and path resolution work, without listing or reading the siblings).
    let mut ancestors = BTreeSet::new();
    for path in std::iter::once(&spec.worktree_root).chain(&spec.read_only_paths) {
        ancestors.extend(
            path.ancestors()
                .skip(1)
                .filter(|ancestor| ancestor.parent().is_some())
                .map(Path::to_path_buf),
        );
    }
    let ancestors: Vec<String> = ancestors
        .iter()
        .map(|ancestor| profile.literal("ANC", ancestor))
        .collect();

    let devices: Vec<String> = WRITABLE_DEVICES
        .iter()
        .map(|device| format!("(literal \"{device}\")"))
        .collect();
    let mut secrets: Vec<String> = HOME_SECRETS
        .iter()
        .map(|secret| profile.subpath("SEC", &home.join(secret)))
        .collect();
    secrets.extend(
        SYSTEM_SECRETS
            .iter()
            .chain(CONTAINER_SOCKETS)
            .map(|secret| profile.subpath("SEC", Path::new(secret))),
    );

    let text = &mut profile.text;
    text.push_str("(version 1)\n");
    text.push_str(&format!(
        "(deny default (with message \"coloride-run-{}\"))\n",
        spec.run_id
    ));
    text.push_str("(allow process-exec)\n(allow process-fork)\n");
    text.push_str("(allow signal (target self))\n(allow process-info* (target self))\n");
    text.push_str("(allow sysctl-read)\n");
    text.push_str(&format!(
        "(allow file-read-metadata (literal \"/\") {system} {})\n",
        ancestors.join(" ")
    ));
    text.push_str(&format!(
        "(allow file-read* (literal \"/\") {system} {worktree} {})\n",
        read_only.join(" ")
    ));
    text.push_str(&format!(
        "(allow file-write* {worktree} {})\n",
        devices.join(" ")
    ));
    text.push_str("(allow file-ioctl (literal \"/dev/dtracehelper\"))\n");
    // No `network` rule means no network. The proxy is the one exception.
    if let NetworkPolicy::LocalProxyOnly(addr) = spec.network {
        text.push_str(&format!(
            "(allow network-outbound (remote ip \"localhost:{}\"))\n",
            addr.port()
        ));
    }
    // Last, so that they win over everything above.
    // Tagged like the default rule: the log query filters on the tag.
    text.push_str(&format!(
        "(deny file-read* file-write* {} (with message \"coloride-run-{}\"))\n",
        secrets.join(" "),
        spec.run_id
    ));
    Ok(profile)
}

impl OsSandboxEngine for SeatbeltEngine {
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            name: "macos-seatbelt",
            kind: OsSandboxKind::MacOsSeatbelt,
            enforces_memory_limit: false,
            enforces_process_limit: false,
            enforces_cpu_limit: true,
            supports_local_proxy: true,
        }
    }

    fn self_test(&self) -> Result<(), SandboxError> {
        if !cfg!(target_os = "macos") {
            return Err(SandboxError::unavailable("Seatbelt exists only on macOS"));
        }
        if !self.sandbox_exec.is_file() {
            return Err(SandboxError::unavailable(format!(
                "{} is not available",
                self.sandbox_exec.display()
            )));
        }
        let sandbox_exec = self.sandbox_exec.clone();
        probe::run(
            "Seatbelt",
            |home| SeatbeltEngine { home, sandbox_exec },
            r#"/usr/bin/nc -z -w 1 127.0.0.1 "$PORT" > /dev/null 2>&1 && exit 26"#,
            |_| None,
        )
    }

    fn prepare(&self, spec: &ValidatedSpec) -> Result<Command, SandboxError> {
        check_program_is_readable(spec, &readable_roots(spec))?;

        let profile = render_profile(spec, &self.home)?;
        let mut command = Command::new(&self.sandbox_exec);
        for (name, path) in &profile.params {
            let mut parameter = OsString::from(name);
            parameter.push("=");
            parameter.push(path);
            command.arg("-D").arg(parameter);
        }
        command.arg("-p").arg(&profile.text);
        match cpu_seconds(&spec.limits) {
            None => {
                command.arg(&spec.program);
            }
            Some(seconds) => {
                // `RLIMIT_CPU`: the kernel sends SIGXCPU, then SIGKILL, at the budget.
                command
                    .args(["/bin/sh", "-c", r#"ulimit -t "$0" && exec "$@""#])
                    .arg(seconds.to_string())
                    .arg(&spec.program);
            }
        }
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.environment());
        Ok(command)
    }

    fn collect_denials(&self, run: &RunInfo, audit: Arc<dyn AuditSink>) {
        if !cfg!(target_os = "macos") {
            return;
        }
        let tag = format!("coloride-run-{}", run.run_id);
        let started = run.started;
        thread::spawn(move || {
            thread::sleep(LOG_SETTLE);
            let age = SystemTime::now()
                .duration_since(started)
                .map_or(10, |elapsed| elapsed.as_secs())
                + 5;
            let output = Command::new(LOG)
                .args(["show", "--last", &format!("{age}s"), "--style", "compact"])
                .args(["--predicate", &format!("eventMessage CONTAINS \"{tag}\"")])
                .output();
            match output {
                Ok(output) => {
                    let text = String::from_utf8_lossy(&output.stdout);
                    for event in parse_denials(&text, MAX_DENIALS_PER_RUN) {
                        audit.record(event);
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "could not read the Seatbelt denials from the log");
                }
            }
        });
    }
}

/// Turns the kernel's lines (`... Sandbox: cat(123) deny(1) file-read-data /path`) into audit
/// events: distinct (operation, target) pairs, at most `max`.
pub(crate) fn parse_denials(log: &str, max: usize) -> Vec<SecurityAuditEvent> {
    let mut seen = BTreeSet::new();
    let mut events = Vec::new();
    for line in log.lines() {
        let Some((_, rest)) = line.split_once("Sandbox: ") else {
            continue;
        };
        let Some((who, rest)) = rest.split_once(" deny(") else {
            continue;
        };
        let Some((_, rest)) = rest.split_once(") ") else {
            continue;
        };
        let (operation, target) = rest.split_once(' ').unwrap_or((rest, ""));
        if !seen.insert((operation.to_string(), target.to_string())) {
            continue;
        }
        let kind = if operation.starts_with("file-") {
            SecurityEventKind::FileAccessDenied
        } else if operation.starts_with("network") {
            SecurityEventKind::NetworkAccessDenied
        } else {
            SecurityEventKind::SystemAccessDenied
        };
        let detail: String = format!("{who} {operation} {target}")
            .chars()
            .take(300)
            .collect();
        events.push(SecurityAuditEvent::new(kind, detail));
        if events.len() == max {
            break;
        }
    }
    events
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::spec::ResourceLimits;
    use std::collections::BTreeMap;

    fn spec() -> ValidatedSpec {
        ValidatedSpec {
            worktree_root: "/Users/u/Library/Caches/Coloride/wt".into(),
            program: "/usr/bin/true".into(),
            args: vec![],
            cwd: "/Users/u/Library/Caches/Coloride/wt".into(),
            env: BTreeMap::new(),
            read_only_paths: vec!["/Users/u/project/node_modules".into()],
            network: NetworkPolicy::Denied,
            limits: ResourceLimits::default(),
            tmp_dir: "/Users/u/Library/Caches/Coloride/wt/.coloride-tmp/x".into(),
            run_id: "123-1-abc".into(),
        }
    }

    #[test]
    fn the_profile_denies_by_default_never_allows_everything_and_has_no_network() {
        let profile = render_profile(&spec(), Path::new("/Users/u")).unwrap();
        assert!(profile
            .text
            .contains("(deny default (with message \"coloride-run-123-1-abc\"))"));
        assert!(!profile.text.contains("(allow default"));
        assert!(!profile.text.contains("network"), "{}", profile.text);
        assert!(
            !profile.text.contains("/Users/u"),
            "paths must be parameters"
        );
    }

    #[test]
    fn secrets_are_denied_last_so_that_they_win_over_every_allowance() {
        let profile = render_profile(&spec(), Path::new("/Users/u")).unwrap();
        let last_deny = profile.text.rfind("(deny file-read* file-write*").unwrap();
        for allowance in [
            "(allow file-read*",
            "(allow file-write*",
            "(allow file-read-metadata",
        ] {
            assert!(
                profile.text.rfind(allowance).unwrap() < last_deny,
                "{allowance}"
            );
        }
        let denied: Vec<&PathBuf> = profile
            .params
            .iter()
            .filter(|(name, _)| name.starts_with("SEC"))
            .map(|(_, path)| path)
            .collect();
        for secret in [
            "/Users/u/.ssh",
            "/Users/u/.aws",
            "/Users/u/.gnupg",
            "/Users/u/Library/Keychains",
            "/Library/Keychains",
            "/private/var/run/docker.sock",
        ] {
            assert!(
                denied.contains(&&PathBuf::from(secret)),
                "{secret} not denied"
            );
        }
    }

    #[test]
    fn the_worktree_is_the_only_writable_directory() {
        let profile = render_profile(&spec(), Path::new("/Users/u")).unwrap();
        let write_rule = profile
            .text
            .lines()
            .find(|line| line.starts_with("(allow file-write*"))
            .unwrap();
        assert_eq!(write_rule.matches("(subpath").count(), 1, "{write_rule}");
        let worktree = &profile
            .params
            .iter()
            .find(|(name, _)| name.starts_with("WT"))
            .unwrap();
        assert_eq!(
            worktree.1,
            PathBuf::from("/Users/u/Library/Caches/Coloride/wt")
        );
        // The read-only path is readable but appears in no write rule.
        assert!(profile
            .params
            .iter()
            .any(|(name, _)| name.starts_with("RO")));
        assert!(!write_rule.contains("RO"));
    }

    #[test]
    fn only_the_proxy_port_is_ever_allowed_on_the_network() {
        let mut spec = spec();
        spec.network = NetworkPolicy::LocalProxyOnly("127.0.0.1:8123".parse().unwrap());
        let profile = render_profile(&spec, Path::new("/Users/u")).unwrap();
        assert_eq!(
            profile.text.matches("network").count(),
            1,
            "{}",
            profile.text
        );
        assert!(profile
            .text
            .contains("(allow network-outbound (remote ip \"localhost:8123\"))"));
    }

    #[test]
    fn a_run_id_cannot_alter_the_profile() {
        let mut spec = spec();
        for bad in ["", "x\"))(allow default)", "a b", "a)"] {
            spec.run_id = bad.to_string();
            assert!(
                render_profile(&spec, Path::new("/Users/u")).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn kernel_denials_become_distinct_audit_events() {
        let log = "\
2026-11-15 00:48:29.128 E  kernel[0:23b972] (Sandbox) Sandbox: cat(87225) deny(1) file-read-data /Users/u/.ssh/id_rsa
coloride-run-1
2026-11-15 00:48:29.129 E  kernel[0:23bc08] (Sandbox) Sandbox: cat(87225) deny(1) file-read-data /Users/u/.ssh/id_rsa
coloride-run-1
2026-11-15 00:48:29.130 E  kernel[0:23bccc] (Sandbox) Sandbox: nc(87226) deny(1) network-outbound 127.0.0.1:9
coloride-run-1
2026-11-15 00:48:29.131 E  kernel[0:23bccc] (Sandbox) Sandbox: kill(87227) deny(1) signal target:87000
coloride-run-1
Filtering the log data using \"eventMessage CONTAINS x\"
";
        let events = parse_denials(log, 10);
        let kinds: Vec<_> = events.iter().map(|event| event.kind).collect();
        assert_eq!(
            kinds,
            vec![
                SecurityEventKind::FileAccessDenied,
                SecurityEventKind::NetworkAccessDenied,
                SecurityEventKind::SystemAccessDenied
            ]
        );
        assert_eq!(
            events[0].detail,
            "cat(87225) file-read-data /Users/u/.ssh/id_rsa"
        );
        assert_eq!(parse_denials(log, 1).len(), 1);
    }

    #[test]
    fn the_cpu_budget_rounds_up() {
        let mut limits = ResourceLimits::default();
        assert_eq!(cpu_seconds(&limits), None);
        limits.max_cpu_time = Some(Duration::from_millis(1));
        assert_eq!(cpu_seconds(&limits), Some(1));
        limits.max_cpu_time = Some(Duration::from_secs(2));
        assert_eq!(cpu_seconds(&limits), Some(2));
    }
}
