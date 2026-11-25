//! A local, loopback-only HTTP endpoint that a CLI agent's own hooks call into, so a terminal
//! session can read the agent's busy/idle state from the agent itself instead of guessing it from
//! the screen. Claude Code is the one agent wired up here ([`install_claude_hooks`]); Codex needs
//! no hook, since it reports its state in the terminal title (see `terminal.rs`).
//!
//! One [`AgentHookServer`] is shared by every terminal session in the app, each with its own bearer
//! token from [`AgentHookServer::register`]. The server only records the last state each token
//! reported; the session's poller in `terminal.rs` reads it through
//! [`AgentHookRegistration::reported_busy`] and stays the one place that decides and publishes.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

const MAX_HEADER_LINES: usize = 32;
const MAX_LINE_BYTES: usize = 4096;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// What a registration's agent last reported, stored in an `AtomicU8`.
const REPORTED_NOTHING: u8 = 0;
const REPORTED_IDLE: u8 = 1;
const REPORTED_BUSY: u8 = 2;

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| io::Error::other("token generation failed"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

struct Registration {
    agent: &'static str,
    reported: Arc<AtomicU8>,
}

#[derive(Default)]
struct Registry(Mutex<HashMap<String, Registration>>);

/// Shared loopback server. Binds on construction; one per app, not one per terminal.
pub struct AgentHookServer {
    address: SocketAddr,
    registry: Arc<Registry>,
    stopped: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl AgentHookServer {
    pub fn start() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let registry = Arc::new(Registry::default());
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_registry = Arc::clone(&registry);
        let thread_stopped = Arc::clone(&stopped);
        let thread = thread::Builder::new()
            .name("agent-hook-listener".into())
            .spawn(move || accept_loop(listener, thread_registry, thread_stopped))?;
        Ok(Self {
            address,
            registry,
            stopped,
            thread: Some(thread),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Registers a fresh, random bearer token for one terminal session's hook calls about
    /// `agent`. Dropping the returned [`AgentHookRegistration`] (when the session ends) removes
    /// the token; a hook call arriving after that is rejected like any other unknown token.
    pub fn register(&self, agent: &'static str) -> io::Result<AgentHookRegistration> {
        let token = random_token()?;
        let reported = Arc::new(AtomicU8::new(REPORTED_NOTHING));
        lock(&self.registry.0).insert(
            token.clone(),
            Registration {
                agent,
                reported: Arc::clone(&reported),
            },
        );
        Ok(AgentHookRegistration {
            registry: Arc::clone(&self.registry),
            token,
            reported,
        })
    }
}

impl Drop for AgentHookServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// One terminal session's hook registration. Its token is valid for exactly as long as this is
/// held.
pub struct AgentHookRegistration {
    registry: Arc<Registry>,
    token: String,
    reported: Arc<AtomicU8>,
}

impl AgentHookRegistration {
    pub fn token(&self) -> &str {
        &self.token
    }

    /// What the agent's own hook last said: `Some(true)` once a turn started, `Some(false)` once
    /// it ended, `None` before any hook call (not installed, or not fired yet).
    pub fn reported_busy(&self) -> Option<bool> {
        match self.reported.load(Ordering::SeqCst) {
            REPORTED_BUSY => Some(true),
            REPORTED_IDLE => Some(false),
            _ => None,
        }
    }

    /// Forgets what the previous agent process reported, e.g. once it has exited.
    pub fn forget(&self) {
        self.reported.store(REPORTED_NOTHING, Ordering::SeqCst);
    }
}

impl Drop for AgentHookRegistration {
    fn drop(&mut self) {
        lock(&self.registry.0).remove(&self.token);
    }
}

fn accept_loop(listener: TcpListener, registry: Arc<Registry>, stopped: Arc<AtomicBool>) {
    while !stopped.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let registry = Arc::clone(&registry);
                let _ = thread::Builder::new()
                    .name("agent-hook-conn".into())
                    .spawn(move || handle_connection(stream, &registry));
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

struct ParsedRequest {
    path: String,
    bearer_token: String,
}

fn read_request(stream: &TcpStream) -> Option<ParsedRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 || request_line.len() > MAX_LINE_BYTES {
        return None;
    }
    let path = request_line.split_whitespace().nth(1)?.to_string();
    let mut bearer_token = String::new();
    for _ in 0..MAX_HEADER_LINES {
        let mut line = String::new();
        let read = reader.read_line(&mut line).ok()?;
        if read == 0 || line.len() > MAX_LINE_BYTES {
            return None;
        }
        if line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("authorization") {
                bearer_token = value
                    .trim()
                    .strip_prefix("Bearer ")
                    .unwrap_or("")
                    .trim()
                    .to_string();
            }
        }
    }
    Some(ParsedRequest { path, bearer_token })
}

fn handle_connection(stream: TcpStream, registry: &Registry) {
    let _ = stream.set_read_timeout(Some(CONNECTION_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CONNECTION_TIMEOUT));
    let status = respond(&stream, registry);
    let body = b"{}";
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut stream = stream;
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(body);
}

/// `/hook/<agent>/<event>`, authenticated by the `Authorization: Bearer <token>` header.
fn respond(stream: &TcpStream, registry: &Registry) -> &'static str {
    let Some(request) = read_request(stream) else {
        return "400 Bad Request";
    };
    let Some(rest) = request.path.strip_prefix("/hook/") else {
        return "404 Not Found";
    };
    let Some((agent, event)) = rest.split_once('/') else {
        return "404 Not Found";
    };
    if request.bearer_token.is_empty() {
        return "401 Unauthorized";
    }
    // The token is the registry's key (a random 32-byte value, never guessable in practice), so
    // a direct map lookup is the auth check — no separate comparison step needed.
    let table = lock(&registry.0);
    let Some(registration) = table.get(&request.bearer_token) else {
        return "401 Unauthorized";
    };
    if registration.agent != agent {
        return "404 Not Found";
    }
    if let Some(busy) = event_busy(agent, event) {
        let state = if busy { REPORTED_BUSY } else { REPORTED_IDLE };
        registration.reported.store(state, Ordering::SeqCst);
    }
    "204 No Content"
}

/// The turn-boundary events Claude Code's hooks report: `UserPromptSubmit` starts a turn, `Stop`
/// ends it.
const TURN_BOUNDARY_HOOK_EVENTS: &[&str] = &["UserPromptSubmit", "Stop"];

const MANAGED_HOOK_MARKER: &str = "coloride-managed-hook";

fn turn_boundary_event_path_segment(hook_event_name: &str) -> &'static str {
    match hook_event_name {
        "UserPromptSubmit" => "user-prompt-submit",
        "Stop" => "stop",
        _ => "unknown",
    }
}

/// Whether `agent`'s hook event means a turn just started (`true`) or ended (`false`). `None`
/// for an event or agent this server does not map.
fn event_busy(agent: &str, event_path_segment: &str) -> Option<bool> {
    match (agent, event_path_segment) {
        ("claude", "user-prompt-submit") => Some(true),
        ("claude", "stop") => Some(false),
        _ => None,
    }
}

/// `-o /dev/null`, discarded stdout and a forced `|| true`: whatever happens to this request
/// (server not running, timeout, wrong response), the hook's own exit code stays 0 and it prints
/// nothing — the agent CLI must never see this as a reason to block the prompt or the stop it is
/// trying to report.
fn curl_hook_command(
    server_address: SocketAddr,
    token: &str,
    agent: &str,
    event_path_segment: &str,
) -> String {
    format!(
        "curl -s -m 2 -o /dev/null -X POST -H \"Authorization: Bearer {token}\" \
         \"http://{server_address}/hook/{agent}/{event_path_segment}\" >/dev/null 2>&1 || true \
         # {MANAGED_HOOK_MARKER}"
    )
}

fn settings_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".claude").join("settings.local.json")
}

fn read_json_object(path: &Path) -> io::Result<serde_json::Map<String, Value>> {
    if !path.exists() {
        return Ok(serde_json::Map::new());
    }
    let text = std::fs::read_to_string(path)?;
    if text.trim().is_empty() {
        return Ok(serde_json::Map::new());
    }
    let value: Value = serde_json::from_str(&text).map_err(|error| {
        io::Error::other(format!("{} is not valid JSON: {error}", path.display()))
    })?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| io::Error::other(format!("{} is not a JSON object", path.display())))
}

fn write_json_atomically(path: &Path, root: &serde_json::Map<String, Value>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(root).map_err(io::Error::other)?;
    let tmp_path = path.with_extension("json.coloride-tmp");
    std::fs::write(&tmp_path, text)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

fn is_managed_entry(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(MANAGED_HOOK_MARKER))
            })
        })
}

fn managed_entry(command: String) -> Value {
    json!({ "hooks": [ { "type": "command", "command": command } ] })
}

/// Merges Claude Code's `UserPromptSubmit`/`Stop` hooks into the *project-local*
/// `.claude/settings.local.json` (never the user's real `~/.claude/settings.json`: this app never
/// writes outside a trusted workspace for its own infrastructure). Only entries carrying
/// [`MANAGED_HOOK_MARKER`] are touched — a user's own hooks for the same event are left alone, and
/// re-installing (a fresh token after this app restarts) replaces only this app's previous entry.
pub fn install_claude_hooks(
    workspace_root: &Path,
    server_address: SocketAddr,
    token: &str,
) -> io::Result<()> {
    let path = settings_path(workspace_root);
    let mut root = read_json_object(&path)?;
    let hooks_value = root.entry("hooks".to_string()).or_insert_with(|| json!({}));
    let hooks = hooks_value
        .as_object_mut()
        .ok_or_else(|| io::Error::other(format!("{}: 'hooks' is not an object", path.display())))?;
    for &event in TURN_BOUNDARY_HOOK_EVENTS {
        let entries_value = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        let entries = entries_value.as_array_mut().ok_or_else(|| {
            io::Error::other(format!(
                "{}: 'hooks.{event}' is not an array",
                path.display()
            ))
        })?;
        entries.retain(|entry| !is_managed_entry(entry));
        entries.push(managed_entry(curl_hook_command(
            server_address,
            token,
            "claude",
            turn_boundary_event_path_segment(event),
        )));
    }
    write_json_atomically(&path, &root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn event_busy_maps_only_claudes_two_turn_boundary_events() {
        assert_eq!(event_busy("claude", "user-prompt-submit"), Some(true));
        assert_eq!(event_busy("claude", "stop"), Some(false));
        assert_eq!(event_busy("claude", "pre-tool-use"), None);
        assert_eq!(event_busy("codex", "stop"), None);
    }

    #[test]
    fn install_adds_managed_hooks_without_touching_unrelated_settings() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings_path(dir.path());
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, r#"{"permissions": {"allow": ["Bash"]}}"#).unwrap();

        let address: SocketAddr = "127.0.0.1:4000".parse().unwrap();
        install_claude_hooks(dir.path(), address, "tok-1").unwrap();

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(written["permissions"]["allow"][0], "Bash");
        let user_prompt_submit = written["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(user_prompt_submit.len(), 1);
        let command = user_prompt_submit[0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("tok-1"));
        assert!(command.contains("/hook/claude/user-prompt-submit"));
        assert!(command.contains("|| true"));
    }

    #[test]
    fn reinstalling_replaces_only_the_previous_managed_entry() {
        let dir = tempfile::tempdir().unwrap();
        let address: SocketAddr = "127.0.0.1:4000".parse().unwrap();
        install_claude_hooks(dir.path(), address, "tok-1").unwrap();
        install_claude_hooks(dir.path(), address, "tok-2").unwrap();

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path(dir.path())).unwrap())
                .unwrap();
        let stop = written["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stop.len(),
            1,
            "stale token entry must be replaced, not duplicated"
        );
        assert!(stop[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("tok-2"));
    }

    #[test]
    fn install_preserves_a_users_own_hook_on_the_same_event() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings_path(dir.path());
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(
            &settings,
            r#"{"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "echo mine"}]}]}}"#,
        )
        .unwrap();

        let address: SocketAddr = "127.0.0.1:4000".parse().unwrap();
        install_claude_hooks(dir.path(), address, "tok-1").unwrap();

        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        let stop = written["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert!(stop
            .iter()
            .any(|entry| entry["hooks"][0]["command"] == "echo mine"));
    }

    fn post(address: SocketAddr, path: &str, token: &str) -> String {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn recognized_events_with_the_right_token_record_the_reported_state() {
        let server = AgentHookServer::start().unwrap();
        let registration = server.register("claude").unwrap();
        assert_eq!(registration.reported_busy(), None);

        let response = post(
            server.address(),
            "/hook/claude/user-prompt-submit",
            registration.token(),
        );
        assert!(response.starts_with("HTTP/1.1 204"), "{response}");
        assert_eq!(registration.reported_busy(), Some(true));

        post(server.address(), "/hook/claude/stop", registration.token());
        assert_eq!(registration.reported_busy(), Some(false));

        registration.forget();
        assert_eq!(registration.reported_busy(), None);
    }

    #[test]
    fn a_wrong_token_or_agent_is_rejected_and_records_nothing() {
        let server = AgentHookServer::start().unwrap();
        let registration = server.register("claude").unwrap();

        let wrong_token = post(
            server.address(),
            "/hook/claude/user-prompt-submit",
            "not-the-token",
        );
        assert!(wrong_token.starts_with("HTTP/1.1 401"), "{wrong_token}");

        let wrong_agent = post(
            server.address(),
            "/hook/codex/user-prompt-submit",
            registration.token(),
        );
        assert!(wrong_agent.starts_with("HTTP/1.1 404"), "{wrong_agent}");

        assert_eq!(registration.reported_busy(), None);
    }

    #[test]
    fn dropping_the_registration_revokes_its_token() {
        let server = AgentHookServer::start().unwrap();
        let registration = server.register("claude").unwrap();
        let token = registration.token().to_string();
        drop(registration);

        let response = post(server.address(), "/hook/claude/user-prompt-submit", &token);
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    }
}
