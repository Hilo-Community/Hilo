//! Opt-in, offline inspection of an installed agent's actual ACP authentication surface.
//! Usage: cargo run -p agent-registry --example probe_acp -- <registry-root> <agent-id>
//! Uses a disposable private home and the native fail-closed sandbox. No host credentials.
use agent_registry::AgentRegistry;
use sandbox_runner::{native_sandbox, MemoryAuditSink, SandboxSpec};
use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    sync::Arc,
    time::Duration,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 || args.len() > 4 {
        return Err("expected <registry-root> <agent-id> [--inspect-terminal-auth]".into());
    }
    let launch = AgentRegistry::open(&args[1])?.launch_command(&args[2])?;
    let temp = tempfile::tempdir()?;
    let work = temp.path().join("work");
    fs::create_dir(&work)?;
    let work = work.canonicalize()?;
    let profile = agent_registry::profiles::agent_profile(&args[2]);
    let home =
        agent_registry::private_home::PrivateAgentHome::open(temp.path(), "probe", &profile)?;
    let mut spec = SandboxSpec::new(&work, launch.program.canonicalize()?);
    spec.args = launch.args.iter().map(Into::into).collect();
    spec.read_only_paths = launch.read_only_paths;
    spec.writable_paths = vec![home.path().to_path_buf()];
    spec.worktree_read_only = true;
    spec.limits.wall_timeout = Duration::from_secs(30);
    spec.env = home.environment().clone();
    spec.env.insert(
        "PATH".into(),
        "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin".into(),
    );
    let runner = native_sandbox(Arc::new(MemoryAuditSink::default()))?;
    let mut child = runner.spawn(&spec)?;
    let mut input = child.take_stdin().ok_or("missing stdin")?;
    let output = child.take_stdout().ok_or("missing stdout")?;
    let errors = child.take_stderr().ok_or("missing stderr")?;
    let stderr = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = errors.take(16384).read_to_string(&mut text);
        text
    });
    let mut capabilities = json!({"auth":{"_meta":{"gateway":true}},"elicitation":{"url":{}},"fs":{"readTextFile":true,"writeTextFile":true},"terminal":true});
    // Inspection only: the production client must not advertise this until it implements
    // the interactive, sandboxed invocation described by the returned method.
    if args
        .get(3)
        .is_some_and(|arg| arg == "--inspect-terminal-auth")
    {
        capabilities["auth"]["terminal"] = json!(true);
    }
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"Coloride probe","version":"0.1.0"},"clientCapabilities":capabilities}})
    )?;
    input.flush()?;
    let mut output = BufReader::new(output);
    let mut line = String::new();
    let result = loop {
        line.clear();
        if output.by_ref().take(1024 * 1024).read_line(&mut line)? == 0 {
            break Err("agent exited before initialize response");
        }
        let value: Value = serde_json::from_str(&line)?;
        println!("{value}");
        if value["id"] == 1 {
            break if value.get("error").is_some() {
                Err("initialize failed")
            } else {
                Ok(())
            };
        }
    };
    child.kill();
    eprintln!("{}", stderr.join().unwrap_or_default());
    result.map_err(Into::into)
}
