//! The start-up self-test shared by the OS engines: a probe runs in a throw-away sandbox. Positive
//! controls make sure it does not pass merely because nothing runs at all; negative controls are
//! the isolation itself. A sandbox that fails any of them is not used.

use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::engine::OsSandboxEngine;
use crate::error::SandboxError;
use crate::spec::SandboxSpec;
use crate::supervisor::{new_run_id, run_to_completion, SandboxedChild, Termination, TmpDir};

/// Removes the self-test's files when dropped.
struct ProbeDir(PathBuf);

impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Shell script of the probe. `@NET@` is replaced by the engine's own way to open a TCP
/// connection to `127.0.0.1:$PORT` (exit 13 if the tool is missing, 26 if it connects).
const SCRIPT: &str = r#"
W="$1"; O="$2"; H="$3"; PORT="$4"; VICTIM="$5"
echo ok > "$W/probe.txt" || exit 10
[ "$(cat "$W/probe.txt")" = ok ] || exit 11
[ "$(cat "$H/ok.txt")" = ok ] || exit 12
( echo x > "$O/escape.txt" ) 2>/dev/null && exit 20
( echo x > "$W/../escape2.txt" ) 2>/dev/null && exit 21
cat "$O/secret.txt" > /dev/null 2>&1 && exit 22
cat "$H/.ssh/id_rsa" > /dev/null 2>&1 && exit 23
( echo x > "$H/ok.txt" ) 2>/dev/null && exit 24
kill -9 "$VICTIM" 2>/dev/null && exit 25
@NET@
exit 0
"#;

fn failure(code: i32) -> &'static str {
    match code {
        10 | 11 => "a process cannot use its own worktree",
        12 => "a read-only path is not readable",
        13 => "the network probe tool is missing inside the sandbox",
        20 | 21 => "a process can write outside its worktree",
        22 => "a process can read a file outside what it was given",
        23 => "a process can read ~/.ssh even when its parent directory is shared",
        24 => "a read-only path is writable",
        25 => "a process can signal other processes",
        26 => "a process can open a network connection",
        _ => "the sandbox could not run the probe",
    }
}

/// `build_engine` receives a fake home directory (holding a fake `.ssh`), so the real secrets of
/// the user are never involved. `net_probe` is a shell fragment that runs `exit 26` when the
/// process manages to connect to `127.0.0.1:$PORT`. `hint` may explain a failure to the user.
pub(crate) fn run<E: OsSandboxEngine>(
    name: &str,
    build_engine: impl FnOnce(PathBuf) -> E,
    net_probe: &str,
    hint: fn(&str) -> Option<&'static str>,
) -> Result<(), SandboxError> {
    let unavailable =
        |what: &str| SandboxError::unavailable(format!("the {name} self-test failed: {what}"));
    let io = |error: std::io::Error| unavailable(&error.to_string());

    let base = std::env::temp_dir()
        .canonicalize()
        .map_err(io)?
        .join(format!("coloride-selftest-{}", new_run_id()));
    let (worktree, outside, home) = (base.join("wt"), base.join("outside"), base.join("home"));
    fs::create_dir_all(home.join(".ssh")).map_err(io)?;
    fs::create_dir_all(&worktree).map_err(io)?;
    fs::create_dir_all(&outside).map_err(io)?;
    let _cleanup = ProbeDir(base.clone());
    fs::write(outside.join("secret.txt"), "secret").map_err(io)?;
    fs::write(home.join(".ssh/id_rsa"), "secret").map_err(io)?;
    fs::write(home.join("ok.txt"), "ok").map_err(io)?;

    // What the isolated process will try to reach from outside.
    let listener = TcpListener::bind("127.0.0.1:0").map_err(io)?;
    listener.set_nonblocking(true).map_err(io)?;
    let port = listener.local_addr().map_err(io)?.port();
    let mut victim = Command::new("/bin/sleep").arg("600").spawn().map_err(io)?;

    let mut spec = SandboxSpec::new(&worktree, "/bin/sh");
    spec.args = vec![
        "-c".into(),
        SCRIPT.replace("@NET@", net_probe).into(),
        "probe".into(),
        worktree.clone().into(),
        outside.clone().into(),
        home.clone().into(),
        port.to_string().into(),
        victim.id().to_string().into(),
    ];
    // The home is shared read-only on purpose: only the explicit secret rules stand between the
    // process and `~/.ssh` here.
    spec.read_only_paths = vec![home.clone()];
    spec.limits.wall_timeout = Duration::from_secs(20);
    spec.limits.max_output_bytes = 64 * 1024;

    let outcome = (|| {
        let mut validated = spec.validate()?;
        let tmp = TmpDir::create(&validated.worktree_root)?;
        validated.tmp_dir = tmp.path().to_path_buf();
        validated.run_id = new_run_id();
        let launch = build_engine(home.clone()).prepare(&validated)?;
        let mut child = SandboxedChild::spawn_prepared(launch, tmp, None, false, None)?;
        run_to_completion(&mut child, &spec.limits)
    })();

    let victim_alive = matches!(victim.try_wait(), Ok(None));
    let _ = victim.kill();
    let _ = victim.wait();
    let connected = listener.accept().is_ok();

    let output = outcome.map_err(|error| unavailable(&error.to_string()))?;
    match output.termination {
        Termination::Exited { code: Some(0), .. } => {}
        Termination::Exited {
            code: Some(code), ..
        } => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let hint = hint(&stderr)
                .map(|hint| format!(" Hint: {hint}"))
                .unwrap_or_default();
            return Err(unavailable(&format!(
                "{} (check {code}) {}{hint}",
                failure(code),
                stderr.trim()
            )));
        }
        other => return Err(unavailable(&format!("the probe did not finish: {other:?}"))),
    }
    if connected {
        return Err(unavailable("a process reached a local network port"));
    }
    if !victim_alive {
        return Err(unavailable("a process killed another process"));
    }
    if outside.join("escape.txt").exists() || base.join("escape2.txt").exists() {
        return Err(unavailable("a file was written outside the worktree"));
    }
    Ok(())
}
