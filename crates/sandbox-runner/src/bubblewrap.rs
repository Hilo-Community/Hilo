//! Linux engine: bubblewrap (`bwrap`).
//!
//! The process gets its own user, PID, IPC, UTS and network namespaces and a root file system
//! made of nothing but what is bound into it:
//!
//! * the system (`/usr`, `/lib*`, `/opt`, and a minimal `/etc`: the loader cache, certificates,
//!   `alternatives`, the time zone) read-only; no `/home`, `/root`, `/var`, `/run`, `/mnt`, no
//!   `/etc/passwd`;
//! * the task worktree, writable, and the read-only paths of the request, read-only; the root,
//!   `/etc` and `/dev` are remounted read-only, so a write anywhere else is refused, not just
//!   discarded;
//! * a private `/tmp` (memory) unless a shared path is under `/tmp`, `/proc` of the new PID
//!   namespace, a minimal `/dev`;
//! * no network: the namespace has a loopback that is down.
//!
//! A PID namespace brings what a process group cannot: when `bwrap` dies (`--die-with-parent`)
//! the kernel kills every process of the namespace, including those that called `setsid`.
//!
//! The user's secrets (`~/.ssh`, `~/.aws`, ...) simply do not exist inside. If a shared path
//! contains one anyway (the whole home shared by mistake), it is hidden under an empty `tmpfs`
//! (directories) or `/dev/null` (files), after the binds, so it cannot show through.
//!
//! **Requires unprivileged user namespaces.** Ubuntu 24.04 restricts them with AppArmor
//! (`kernel.apparmor_restrict_unprivileged_userns=1`): without a profile that gives `bwrap` the
//! `userns` permission, `bwrap` fails and so does the self-test, with that hint: the sandbox is
//! unavailable, never silently weaker.
//!
//! Limits: the CPU budget is enforced (`prlimit --cpu`); memory and process-count limits are not
//! (that takes cgroups, and `RLIMIT_NPROC` does not count per sandbox), so a request for them is
//! refused. A request for the local proxy is refused too: the sandbox has no route to the Host's
//! loopback by design (`LOT-014` will hand it a Unix socket instead). Landlock is not used yet.
//! The kernel does not report what it refused here, so there are no denial audit events.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::common::{check_program_is_readable, cpu_seconds, CONTAINER_SOCKETS, HOME_SECRETS};
use crate::engine::{EngineCapabilities, Launch, OsSandboxEngine};
use crate::error::SandboxError;
use crate::probe;
use crate::spec::{NetworkPolicy, ValidatedSpec};
use crate::OsSandboxKind;

const BWRAP_CANDIDATES: &[&str] = &["/usr/bin/bwrap", "/usr/local/bin/bwrap", "/bin/bwrap"];
const PRLIMIT_CANDIDATES: &[&str] = &["/usr/bin/prlimit", "/bin/prlimit"];
/// Read-only inside the sandbox (symbolic links, as on merged-`/usr` systems, are recreated).
const SYSTEM_ROOTS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/opt",
];
/// The only parts of `/etc` that exist inside; each is skipped if the system does not have it.
const ETC_ENTRIES: &[&str] = &[
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/alternatives",
    "/etc/ssl",
    "/etc/ca-certificates",
    "/etc/pki",
    "/etc/localtime",
];

#[derive(Debug, Clone)]
pub struct BubblewrapEngine {
    bwrap: PathBuf,
    prlimit: Option<PathBuf>,
    /// Canonical home directory whose secrets are hidden if a shared path contains them.
    home: PathBuf,
}

impl BubblewrapEngine {
    /// `home` is the directory of the user whose secrets must stay out of reach. `bwrap` is
    /// looked up in fixed system locations only, never through `PATH` (a binary planted in the
    /// workspace must not become the sandbox).
    pub fn new(home: &Path) -> Result<Self, SandboxError> {
        let home = home.canonicalize().map_err(|error| {
            SandboxError::unavailable(format!("the home directory {home:?} is unusable: {error}"))
        })?;
        if !home.is_dir() || home.parent().is_none() {
            return Err(SandboxError::unavailable(
                "the home directory must be an existing directory other than /",
            ));
        }
        let first_file = |candidates: &[&str]| {
            candidates
                .iter()
                .map(PathBuf::from)
                .find(|path| path.is_file())
        };
        Ok(Self {
            home,
            // Missing: the self-test says so, with the fix.
            bwrap: first_file(BWRAP_CANDIDATES).unwrap_or_else(|| BWRAP_CANDIDATES[0].into()),
            prlimit: first_file(PRLIMIT_CANDIDATES),
        })
    }

    /// The home directory of the current user, from `$HOME`.
    pub fn for_current_user() -> Result<Self, SandboxError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .ok_or_else(|| SandboxError::unavailable("HOME is not set to an absolute path"))?;
        Self::new(&home)
    }

    /// Uses another `bwrap` binary (a missing one makes the self-test fail, closed).
    #[must_use]
    pub fn with_bwrap_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.bwrap = path.into();
        self
    }
}

/// The arguments of `bwrap` (and of the wrapper that sets the CPU budget).
#[derive(Default)]
struct Args(Vec<OsString>);

impl Args {
    fn flags(&mut self, flags: &[&str]) {
        self.0.extend(flags.iter().map(OsString::from));
    }

    fn with(&mut self, flag: &str, value: impl AsRef<OsStr>) {
        self.0.push(flag.into());
        self.0.push(value.as_ref().to_os_string());
    }

    fn bind(&mut self, flag: &str, source: impl AsRef<OsStr>, destination: impl AsRef<OsStr>) {
        self.0.push(flag.into());
        self.0.push(source.as_ref().to_os_string());
        self.0.push(destination.as_ref().to_os_string());
    }
}

/// Every path the process may read: the system, the worktree, the read-only paths.
fn readable_roots(spec: &ValidatedSpec) -> Vec<PathBuf> {
    SYSTEM_ROOTS
        .iter()
        .map(PathBuf::from)
        .chain(std::iter::once(spec.worktree_root.clone()))
        .chain(spec.read_only_paths.iter().cloned())
        .chain(spec.writable_paths.iter().cloned())
        .collect()
}

pub(crate) fn render_args(
    spec: &ValidatedSpec,
    home: &Path,
    prlimit: Option<&Path>,
) -> Result<Vec<OsString>, SandboxError> {
    let mut args = Args::default();
    args.flags(&[
        "--die-with-parent",
        "--new-session",
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup-try",
        "--cap-drop",
        "ALL",
    ]);
    // No network at all: the namespace has a loopback that is down. The only other policy, the
    // local proxy, cannot be offered (see the module documentation).
    match spec.network {
        NetworkPolicy::Denied => args.flags(&["--unshare-net"]),
        NetworkPolicy::LocalProxyOnly(_) => {
            return Err(SandboxError::NetworkPolicyNotEnforceable {
                engine: "linux-bubblewrap",
            })
        }
    }

    for root in SYSTEM_ROOTS {
        match fs::symlink_metadata(root) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(root)?;
                args.bind("--symlink", target, root);
            }
            Ok(meta) if meta.is_dir() => args.bind("--ro-bind", root, root),
            _ => {}
        }
    }
    args.with("--tmpfs", "/etc");
    for entry in ETC_ENTRIES {
        args.bind("--ro-bind-try", entry, entry);
    }
    args.with("--proc", "/proc");
    args.with("--dev", "/dev");
    // A private `/tmp` (memory, gone with the run) for tools that hard-code it, unless a shared
    // path lives under `/tmp`: its parent directories would then sit in that writable tmpfs and
    // the process could write beside the worktree. Without it the root, read-only, covers them.
    let shared_under_tmp = std::iter::once(&spec.worktree_root)
        .chain(&spec.read_only_paths)
        .chain(&spec.writable_paths)
        .any(|path| path.starts_with("/tmp"));
    if !shared_under_tmp {
        args.with("--tmpfs", "/tmp");
    }

    for path in &spec.read_only_paths {
        args.bind("--ro-bind", path, path);
    }
    for path in &spec.writable_paths {
        args.bind("--bind", path, path);
    }
    if spec.mediate_worktree_reads {
        args.with("--dir", &spec.worktree_root);
        if spec.program.starts_with(&spec.worktree_root) {
            args.bind("--ro-bind", &spec.program, &spec.program);
        }
    } else {
        args.bind(
            if spec.worktree_read_only {
                "--ro-bind"
            } else {
                "--bind"
            },
            &spec.worktree_root,
            &spec.worktree_root,
        );
    }
    for path in &spec.denied_read_paths {
        if path.is_dir() {
            args.with("--tmpfs", path);
            args.with("--remount-ro", path);
        } else if path.exists() {
            args.bind("--ro-bind", "/dev/null", path);
        }
    }
    if spec.worktree_read_only {
        args.bind("--bind", &spec.tmp_dir, &spec.tmp_dir);
    }

    // Last of the mounts, so that nothing shows through: a secret that sits inside a shared path.
    let shared: Vec<&PathBuf> = std::iter::once(&spec.worktree_root)
        .chain(&spec.read_only_paths)
        .chain(&spec.writable_paths)
        .collect();
    let secrets = HOME_SECRETS
        .iter()
        .map(|secret| home.join(secret))
        .chain(CONTAINER_SOCKETS.iter().map(PathBuf::from));
    for secret in secrets {
        if !shared.iter().any(|root| secret.starts_with(root)) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&secret) else {
            continue;
        };
        if meta.is_dir() {
            args.with("--tmpfs", &secret);
        } else {
            args.bind("--ro-bind", "/dev/null", &secret);
        }
    }
    // The root and the tmpfs mounts of `/etc` and `/dev` (separate mounts, which a remount of `/`
    // does not reach) are now read-only. What stays writable: the worktree, and a private `/tmp`.
    for mount in ["/etc", "/dev", "/"] {
        args.with("--remount-ro", mount);
    }
    args.with("--chdir", &spec.cwd);

    if let Some(seconds) = cpu_seconds(&spec.limits) {
        let prlimit = prlimit.ok_or(SandboxError::LimitNotEnforceable {
            engine: "linux-bubblewrap",
            limit: "CPU time",
        })?;
        args.0.push(prlimit.into());
        args.0.push(format!("--cpu={seconds}").into());
        args.0.push("--".into());
    }
    args.0.push(spec.program.clone().into());
    args.0.extend(spec.args.iter().cloned());
    Ok(args.0)
}

/// Why `bwrap` may have failed to start, when the cause is the restriction of user namespaces.
fn userns_hint(stderr: &str) -> Option<&'static str> {
    const SYMPTOMS: &[&str] = &[
        "RTM_NEWADDR",
        "uid map",
        "No permissions to create new namespace",
        "Operation not permitted",
    ];
    SYMPTOMS
        .iter()
        .any(|symptom| stderr.contains(symptom))
        .then_some(
            "unprivileged user namespaces look restricted (Ubuntu 24.04 sets \
         kernel.apparmor_restrict_unprivileged_userns=1): install an AppArmor profile that gives \
         /usr/bin/bwrap the `userns` permission, or set that sysctl to 0",
        )
}

impl OsSandboxEngine for BubblewrapEngine {
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            name: "linux-bubblewrap",
            kind: OsSandboxKind::LinuxLandlockBubblewrap,
            enforces_memory_limit: false,
            enforces_process_limit: false,
            enforces_cpu_limit: self.prlimit.is_some(),
            supports_local_proxy: false,
        }
    }

    fn self_test(&self) -> Result<(), SandboxError> {
        if !cfg!(target_os = "linux") {
            return Err(SandboxError::unavailable(
                "bubblewrap is used on Linux only",
            ));
        }
        if !self.bwrap.is_file() {
            return Err(SandboxError::unavailable(format!(
                "{} is not installed; install the `bubblewrap` package",
                self.bwrap.display()
            )));
        }
        let (bwrap, prlimit) = (self.bwrap.clone(), self.prlimit.clone());
        probe::run(
            "bubblewrap",
            |home| BubblewrapEngine {
                bwrap,
                prlimit,
                home,
            },
            // `bash` opens TCP connections itself; if it is not there the probe says so.
            "[ -x /usr/bin/bash ] || exit 13\n\
             /usr/bin/bash -c 'exec 3<>/dev/tcp/127.0.0.1/$0' \"$PORT\" >/dev/null 2>&1 && exit 26",
            userns_hint,
        )
    }

    fn prepare(&self, spec: &ValidatedSpec) -> Result<Launch, SandboxError> {
        check_program_is_readable(spec, &readable_roots(spec))?;
        let args = render_args(spec, &self.home, self.prlimit.as_deref())?;
        let mut command = Command::new(&self.bwrap);
        command
            .args(args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.environment());
        Ok(Launch::Command(command))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::spec::ResourceLimits;

    struct Fixture {
        _dir: tempfile::TempDir,
        home: PathBuf,
        spec: ValidatedSpec,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let home = base.join("home");
        let worktree = home.join(".cache/coloride/wt");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::write(home.join(".ssh/id_rsa"), "secret").unwrap();
        fs::write(home.join(".netrc"), "secret").unwrap();
        fs::create_dir_all(&worktree).unwrap();
        let spec = ValidatedSpec {
            terminal_device: None,
            mediate_worktree_reads: false,
            worktree_read_only: false,
            worktree_root: worktree.clone(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "true".into()],
            cwd: worktree.clone(),
            env: BTreeMap::new(),
            read_only_paths: vec![home.clone()],
            writable_paths: Vec::new(),
            denied_read_paths: Vec::new(),
            preference_domains: Vec::new(),
            loopback_listen_ports: Vec::new(),
            network: NetworkPolicy::Denied,
            limits: ResourceLimits::default(),
            tmp_dir: worktree.join(".coloride-tmp/x"),
            run_id: "1-1-a".into(),
        };
        Fixture {
            _dir: dir,
            home,
            spec,
        }
    }

    fn position(args: &[OsString], flag: &str, value: &Path) -> Option<usize> {
        args.windows(2)
            .position(|pair| pair[0] == flag && pair[1] == value.as_os_str())
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn every_namespace_is_unshared_nothing_is_inherited_and_the_root_is_read_only() {
        let f = fixture();
        let args = strings(&render_args(&f.spec, &f.home, None).unwrap());
        for flag in [
            "--die-with-parent",
            "--new-session",
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-net",
        ] {
            assert!(
                args.iter().any(|arg| arg == flag),
                "{flag} missing: {args:?}"
            );
        }
        assert!(!args.iter().any(|arg| arg == "--share-net"), "{args:?}");
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--bind-try" || arg == "--dev-bind"),
            "{args:?}"
        );
        // /home is not part of the system roots: the home of the user does not exist inside
        // except for what is shared on purpose.
        assert!(
            !args.iter().any(|arg| arg == "/home" || arg == "/root"),
            "{args:?}"
        );
        let remount = args.iter().rposition(|arg| arg == "--remount-ro").unwrap();
        let chdir = args.iter().position(|arg| arg == "--chdir").unwrap();
        let program = args.iter().position(|arg| arg == "/bin/sh").unwrap();
        assert!(remount < chdir && chdir < program);
        assert_eq!(&args[program + 1..], ["-c", "true"]);
    }

    #[test]
    fn a_private_tmp_is_mounted_unless_a_shared_path_is_under_tmp() {
        let f = fixture(); // its paths are under the temporary directory, i.e. /tmp on Linux
        let with_tmp = |args: &[OsString]| {
            args.windows(2)
                .any(|pair| pair[0] == "--tmpfs" && pair[1] == "/tmp")
        };
        let under_tmp = render_args(&f.spec, &f.home, None).unwrap();
        assert_eq!(
            with_tmp(&under_tmp),
            !f.spec.worktree_root.starts_with("/tmp")
        );
        // A worktree in a cache directory gets one.
        let mut elsewhere = fixture();
        elsewhere.spec.worktree_root = "/home/u/.cache/coloride/wt".into();
        elsewhere.spec.cwd = elsewhere.spec.worktree_root.clone();
        elsewhere.spec.read_only_paths = vec!["/home/u/project/node_modules".into()];
        assert!(with_tmp(
            &render_args(&elsewhere.spec, &elsewhere.home, None).unwrap()
        ));
        // A shared path under /tmp removes it.
        elsewhere.spec.read_only_paths = vec!["/tmp/shared-packages".into()];
        assert!(!with_tmp(
            &render_args(&elsewhere.spec, &elsewhere.home, None).unwrap()
        ));
    }

    #[test]
    fn the_worktree_is_the_only_writable_bind() {
        let f = fixture();
        let args = render_args(&f.spec, &f.home, None).unwrap();
        let writable: Vec<&OsString> = args
            .windows(3)
            .filter(|triple| triple[0] == "--bind")
            .map(|triple| &triple[1])
            .collect();
        assert_eq!(writable, vec![&OsString::from(&f.spec.worktree_root)]);
        assert!(position(&args, "--ro-bind", &f.home).is_some());
    }

    #[test]
    fn secrets_inside_a_shared_path_are_hidden_after_the_binds() {
        let f = fixture();
        let args = render_args(&f.spec, &f.home, None).unwrap();
        let share = position(&args, "--ro-bind", &f.home).unwrap();
        let ssh = position(&args, "--tmpfs", &f.home.join(".ssh")).expect(".ssh not hidden");
        let netrc = args
            .windows(3)
            .position(|triple| {
                triple[0] == "--ro-bind"
                    && triple[1] == "/dev/null"
                    && triple[2] == f.home.join(".netrc").as_os_str()
            })
            .expect(".netrc not hidden");
        let remount = args.iter().position(|arg| arg == "--remount-ro").unwrap();
        assert!(share < ssh && ssh < remount && share < netrc && netrc < remount);
        // A secret that does not exist is not mentioned (bubblewrap would refuse the mount).
        assert!(position(&args, "--tmpfs", &f.home.join(".aws")).is_none());
        // And one outside every shared path is not touched: it does not exist inside anyway.
        let mut unshared = fixture();
        unshared.spec.read_only_paths.clear();
        let args = render_args(&unshared.spec, &unshared.home, None).unwrap();
        assert!(position(&args, "--tmpfs", &unshared.home.join(".ssh")).is_none());
    }

    #[test]
    fn the_cpu_budget_goes_through_prlimit_and_is_refused_without_it() {
        let mut f = fixture();
        f.spec.limits.max_cpu_time = Some(std::time::Duration::from_millis(1500));
        let args =
            strings(&render_args(&f.spec, &f.home, Some(Path::new("/usr/bin/prlimit"))).unwrap());
        let wrapper = args
            .iter()
            .position(|arg| arg == "/usr/bin/prlimit")
            .unwrap();
        assert_eq!(&args[wrapper + 1..wrapper + 3], ["--cpu=2", "--"]);
        assert_eq!(args[wrapper + 3], "/bin/sh");
        assert!(matches!(
            render_args(&f.spec, &f.home, None),
            Err(SandboxError::LimitNotEnforceable { .. })
        ));
    }

    #[test]
    fn the_local_proxy_is_refused() {
        let mut f = fixture();
        f.spec.network = NetworkPolicy::LocalProxyOnly("127.0.0.1:8080".parse().unwrap());
        assert!(matches!(
            render_args(&f.spec, &f.home, None),
            Err(SandboxError::NetworkPolicyNotEnforceable { .. })
        ));
    }

    #[test]
    fn a_restricted_user_namespace_is_explained() {
        assert!(
            userns_hint("bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted")
                .unwrap()
                .contains("AppArmor")
        );
        assert!(
            userns_hint("bwrap: setting up uid map: Permission denied").is_some()
                || userns_hint("uid map").is_some()
        );
        assert!(userns_hint("some unrelated failure").is_none());
    }
}
