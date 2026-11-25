//! Integrated terminal (`LOT-010`): a login shell on a pseudo-terminal, started in a trusted
//! workspace only, whose output is delivered in frames of at most one every 16 ms.
//!
//! The shell leads its own session and process group (`portable-pty` calls `setsid`), so the
//! whole job tree is killed with the session, and an [`OrphanWatchdog`] kills it if the Host
//! dies abruptly.

#[cfg(unix)]
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(any(unix, windows))]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use thiserror::Error;

use crate::process::{kill_process_group, OrphanWatchdog, SupervisionError};
use crate::trust::{GatedCapability, TrustError, WorkspaceTrustGate};

#[cfg(unix)]
mod osc_title;

/// `\\?\C:\dir` to `C:\dir`: `cmd.exe` rejects the verbatim prefix returned by `canonicalize`
/// ("UNC paths are not supported. Defaulting to Windows directory").
fn plain_cwd(path: &Path) -> PathBuf {
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

/// Output is sent at most once per frame (`AGENTS.md`: 16 ms stream batching).
pub const TERMINAL_FRAME: Duration = Duration::from_millis(16);
/// A frame never holds more than this; a flood is sent in several frames.
const MAX_FRAME_BYTES: usize = 256 * 1024;

#[derive(Debug, Error)]
pub enum TerminalError {
    #[error(transparent)]
    Trust(#[from] TrustError),
    #[error(transparent)]
    Supervision(#[from] SupervisionError),
    #[error("terminal error: {0}")]
    Pty(String),
}

fn pty_error(error: impl std::fmt::Display) -> TerminalError {
    TerminalError::Pty(error.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

impl TerminalSize {
    fn pty(self) -> PtySize {
        PtySize {
            rows: self.rows.max(1),
            cols: self.cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    /// Bytes the shell wrote during one frame (raw: may end inside a UTF-8 sequence).
    Output(Vec<u8>),
    /// The shell exited; nothing follows.
    Exited,
}

pub type TerminalSink = Arc<dyn Fn(TerminalEvent) + Send + Sync>;

/// Whether a known agent CLI (not the shell itself) is the terminal's foreground job, and
/// whether it looks busy. Unix only (`tcgetpgrp` has no Windows equivalent); on other platforms
/// this is never reported.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TerminalAgentStatus {
    pub agent_id: Option<String>,
    pub busy: bool,
}

pub type TerminalStatusSink = Arc<dyn Fn(TerminalAgentStatus) + Send + Sync>;

/// What the agent-status poller observes of a terminal, fed by the output pump and by
/// [`TerminalSession::write`]: the output chunks from roughly the last [`AGENT_STATUS_WINDOW`]
/// for on-screen markers, the last window title set (see `osc_title`), and whether a line has
/// been submitted since the current agent appeared. Unix only, like the poller; not kept at all
/// for a terminal without one.
#[cfg(unix)]
#[derive(Clone)]
struct ObservedOutput {
    recent: Arc<Mutex<RecentFrames>>,
    title: Arc<Mutex<osc_title::TitleTracker>>,
    /// Set by Enter (or a pasted line) written to the terminal; cleared when the foreground
    /// agent changes. An agent only has a turn to finish once it has been sent something: Codex
    /// also shows its working title while it boots its MCP servers, which is not a turn.
    line_submitted: Arc<AtomicBool>,
}

/// Output frames with the time each arrived, oldest first.
#[cfg(unix)]
type RecentFrames = VecDeque<(Instant, Vec<u8>)>;

/// How far back the on-screen marker check looks. A time window, not just a byte count, so a
/// marker from a finished turn ages out.
#[cfg(unix)]
const AGENT_STATUS_WINDOW: Duration = Duration::from_secs(6);
/// Safety net on top of the time window, in case of a pathological burst within it.
#[cfg(unix)]
const RECENT_OUTPUT_BYTE_CAP: usize = 256 * 1024;

#[cfg(unix)]
impl ObservedOutput {
    fn new() -> Self {
        Self {
            recent: Arc::new(Mutex::new(VecDeque::new())),
            title: Arc::new(Mutex::new(osc_title::TitleTracker::default())),
            line_submitted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Notes keystrokes written to the terminal: a carriage return or newline submits a line.
    fn record_input(&self, bytes: &[u8]) {
        if bytes.iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
            self.line_submitted.store(true, Ordering::SeqCst);
        }
    }

    fn record(&self, frame: &[u8]) {
        lock(&self.title).feed(frame);
        let now = Instant::now();
        let mut recent = lock(&self.recent);
        recent.push_back((now, frame.to_vec()));
        if let Some(cutoff) = now.checked_sub(AGENT_STATUS_WINDOW) {
            while recent.front().is_some_and(|(at, _)| *at < cutoff) {
                recent.pop_front();
            }
        }
        let mut total: usize = recent.iter().map(|(_, bytes)| bytes.len()).sum();
        while total > RECENT_OUTPUT_BYTE_CAP {
            let Some((_, dropped)) = recent.pop_front() else {
                break;
            };
            total -= dropped.len();
        }
    }

    /// The recent output as plain text, for marker matching.
    fn recent_text(&self) -> String {
        let window: Vec<u8> = lock(&self.recent)
            .iter()
            .flat_map(|(_, bytes)| bytes.iter().copied())
            .collect();
        strip_ansi(&window)
    }
}

/// The agent CLIs this host recognizes by their executable name. Extend this to add one.
#[cfg(unix)]
fn known_agent_binary(name: &str) -> Option<&'static str> {
    match name {
        "codex" => Some("codex"),
        "claude" => Some("claude"),
        _ => None,
    }
}

/// What the recent, ANSI-stripped output says about `agent`. Only Claude Code has markers here:
/// its footer reads "esc to interrupt" while a turn runs and "for agents" between turns, and it
/// redraws that footer as a whole. Codex is read from its title instead ([`busy_from_title`]):
/// it only re-sends the screen cells that change, so its on-screen text is not a reliable signal.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusySignal {
    Busy,
    Idle,
    /// Neither marker is in the window — a partial redraw, a dialog, or an agent with no markers.
    /// Not evidence either way.
    Ambiguous,
}

#[cfg(unix)]
fn classify(agent: &str, recent_output_text: &str) -> BusySignal {
    if agent != "claude" {
        return BusySignal::Ambiguous;
    }
    // Busy first: right after a turn starts, the window can still hold the previous idle footer.
    if recent_output_text.contains("esc to interrupt") {
        BusySignal::Busy
    } else if recent_output_text.contains("for agents") {
        BusySignal::Idle
    } else {
        BusySignal::Ambiguous
    }
}

/// The agent's busy state from the window title it sets, once it has shown it reports activity
/// there.
///
/// By default (`animations = true`, title items `activity` + `project-name`) Codex prefixes its
/// title with a braille spinner frame for exactly as long as its own task-running flag is set,
/// replaces it with "[ ! ] Action Required" while blocked on the user, and drops back to the bare
/// project name when the task ends. That flag is the real task state, not what the status line
/// happens to paint: it stays set while the final answer streams in and across tool calls, and a
/// title is current state rather than something that has to be re-sent to be seen. Waiting on the
/// user counts as busy: the turn isn't over.
///
/// `None` until a spinner or action-required title has been seen since this agent started: one
/// that never animates its title (another CLI, or Codex with animations off) says nothing there.
#[cfg(unix)]
fn busy_from_title(tracker: &osc_title::TitleTracker) -> Option<bool> {
    if !tracker.activity_seen() {
        return None;
    }
    Some(matches!(
        tracker.latest().map(osc_title::title_activity),
        Some(osc_title::TitleActivity::Working | osc_title::TitleActivity::AwaitingUser)
    ))
}

/// The busy state from the agent's own hook (`hook`, see `agent_hooks.rs`) and the on-screen
/// `signal`, for an agent that does not report through its title.
///
/// A hook report is trusted over the screen, with one exception: after "a turn started", the
/// agent's own idle footer on screen wins, since the matching "turn ended" call can be lost (a
/// dropped `curl`, an interrupted turn). Without any hook report, the screen decides, and an
/// [`BusySignal::Ambiguous`] window keeps the previous state rather than guessing.
#[cfg(unix)]
fn busy_from_hook_or_screen(hook: Option<bool>, signal: BusySignal, previously_busy: bool) -> bool {
    match (hook, signal) {
        (Some(true), BusySignal::Idle) | (Some(false), _) => false,
        (Some(true), _) => true,
        (None, BusySignal::Busy) => true,
        (None, BusySignal::Idle) => false,
        (None, BusySignal::Ambiguous) => previously_busy,
    }
}

/// Drops ANSI/VT escape sequences (`ESC [ ... <final byte>`) so marker matching sees plain text.
#[cfg(unix)]
fn strip_ansi(input: &[u8]) -> String {
    let text = String::from_utf8_lossy(input);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.as_str().starts_with('[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() || next == '~' {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// What to run. `program: None` is the user's login shell.
#[derive(Debug, Clone, Default)]
pub struct TerminalLaunch {
    pub program: Option<PathBuf>,
    pub args: Vec<String>,
}

pub struct TerminalSession {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    watchdog: Mutex<Option<OrphanWatchdog>>,
    pid: u32,
    #[cfg(unix)]
    observed: Option<ObservedOutput>,
}

impl std::fmt::Debug for TerminalSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalSession")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl TerminalSession {
    /// Starts a shell in the workspace root. Refused unless the workspace is trusted.
    ///
    /// `status_sink`, when given, is told whenever a known agent CLI becomes (or stops being)
    /// the terminal's foreground job, and whenever its busy/idle guess changes. Unix only; on
    /// other platforms it is accepted but never called.
    pub fn spawn(
        gate: &WorkspaceTrustGate,
        launch: &TerminalLaunch,
        size: TerminalSize,
        sink: TerminalSink,
        #[cfg_attr(not(unix), allow(unused_variables))] status_sink: Option<TerminalStatusSink>,
        #[cfg_attr(not(unix), allow(unused_variables))] hook_server: Option<
            Arc<crate::agent_hooks::AgentHookServer>,
        >,
    ) -> Result<Self, TerminalError> {
        gate.authorize(GatedCapability::Terminal)?;
        let pair = native_pty_system().openpty(size.pty()).map_err(pty_error)?;
        let mut command = match &launch.program {
            Some(program) => {
                let mut command = CommandBuilder::new(program);
                command.args(&launch.args);
                command
            }
            None => CommandBuilder::new_default_prog(),
        };
        command.cwd(plain_cwd(gate.workspace_root()));
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "Coloride");
        // Trust may have been revoked while the pty was being opened.
        gate.authorize(GatedCapability::Terminal)?;
        let mut child = pair.slave.spawn_command(command).map_err(pty_error)?;
        drop(pair.slave);
        gate.note_spawned();
        let pid = child.process_id().unwrap_or_default();
        #[cfg(unix)]
        let watchdog_result = OrphanWatchdog::start(pid);
        #[cfg(windows)]
        let watchdog_result = OrphanWatchdog::for_raw_handle(child.as_raw_handle());
        #[cfg(not(any(unix, windows)))]
        let watchdog_result = OrphanWatchdog::start(pid);
        let watchdog = match watchdog_result {
            Ok(watchdog) => watchdog,
            Err(error) => {
                // Never leave an unsupervised shell running.
                kill_process_group(pid);
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        };
        let reader = pair.master.try_clone_reader().map_err(pty_error)?;
        let writer = pair.master.take_writer().map_err(pty_error)?;
        #[cfg(windows)]
        let mut writer = writer;
        #[cfg(windows)]
        {
            // `portable-pty` creates ConPTY with `PSEUDOCONSOLE_INHERIT_CURSOR`, which makes
            // `conhost.exe` emit `ESC [ 6 n` and block all output until the initial cursor
            // position (`ESC [ 1 ; 1 R`) is reported on the input pipe.
            writer.write_all(b"\x1b[1;1R").map_err(pty_error)?;
            writer.flush().map_err(pty_error)?;
        }
        let master = Arc::new(Mutex::new(Some(pair.master)));
        let child = Arc::new(Mutex::new(child));
        #[cfg(unix)]
        let observed = status_sink.as_ref().map(|_| ObservedOutput::new());
        #[cfg(windows)]
        let active = Arc::new(AtomicBool::new(false));
        spawn_output_pump(
            reader,
            sink,
            #[cfg(unix)]
            observed.clone(),
            #[cfg(windows)]
            Arc::clone(&active),
        );
        #[cfg(unix)]
        if let (Some(status_sink), Some(observed)) = (status_sink, observed.clone()) {
            spawn_agent_status_poller(
                Arc::clone(&master),
                Arc::clone(&child),
                pid,
                observed,
                status_sink,
                AgentHookContext {
                    workspace_root: gate.workspace_root().to_path_buf(),
                    hook_server,
                },
            );
        }
        #[cfg(windows)]
        {
            let child = Arc::clone(&child);
            let master = Arc::clone(&master);
            std::thread::spawn(move || {
                while !matches!(lock(&child).try_wait(), Ok(Some(_))) {
                    if lock(&master).is_none() {
                        return;
                    }
                    std::thread::sleep(TERMINAL_FRAME);
                }
                while active.swap(false, Ordering::SeqCst) {
                    std::thread::sleep(TERMINAL_FRAME);
                }
                // Closing the ConPTY pseudo-console flushes conhost.exe and closes its output
                // pipe, so the reader thread sees EOF once the shell has exited.
                let _ = lock(&master).take();
            });
        }
        Ok(Self {
            writer: Mutex::new(writer),
            master,
            child,
            watchdog: Mutex::new(Some(watchdog)),
            pid,
            #[cfg(unix)]
            observed,
        })
    }

    /// The pid of the pty's current foreground process group, when it differs from the shell's
    /// own (i.e. a foreground job is running). Unix only.
    #[cfg(unix)]
    #[must_use]
    pub fn foreground_pgid(&self) -> Option<i32> {
        foreground_pgid_of(&self.master)
    }

    /// OS process id of the shell, which is also its process group id.
    #[must_use]
    pub fn process_id(&self) -> u32 {
        self.pid
    }

    /// Sends keystrokes (or pasted text) to the shell.
    pub fn write(&self, bytes: &[u8]) -> Result<(), TerminalError> {
        #[cfg(unix)]
        if let Some(observed) = &self.observed {
            observed.record_input(bytes);
        }
        let mut writer = lock(&self.writer);
        writer.write_all(bytes).map_err(pty_error)?;
        writer.flush().map_err(pty_error)
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), TerminalError> {
        let master = lock(&self.master);
        let Some(master) = master.as_ref() else {
            return Err(pty_error("terminal has exited"));
        };
        master.resize(size.pty()).map_err(pty_error)
    }

    /// Whether the shell has exited.
    #[must_use]
    pub fn has_exited(&self) -> bool {
        matches!(lock(&self.child).try_wait(), Ok(Some(_)))
    }

    /// Kills the shell and every job it started, then reaps it.
    pub fn kill(&self) {
        #[cfg(windows)]
        if let Some(watchdog) = lock(&self.watchdog).take() {
            watchdog.stop();
        }
        let mut child = lock(&self.child);
        if !matches!(child.try_wait(), Ok(Some(_))) {
            // Not reaped, so the group id still belongs to this shell.
            kill_process_group(self.pid);
            let _ = child.kill();
            let _ = child.wait();
        }
        drop(child);
        if let Some(watchdog) = lock(&self.watchdog).take() {
            watchdog.stop();
        }
        #[cfg(windows)]
        let _ = lock(&self.master).take();
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Reads the pty on one thread and hands frames to `sink` from another, so a slow consumer
/// never stalls the shell and a fast shell never floods the consumer.
fn spawn_output_pump(
    mut reader: Box<dyn Read + Send>,
    sink: TerminalSink,
    #[cfg(unix)] observed: Option<ObservedOutput>,
    #[cfg(windows)] active: Arc<AtomicBool>,
) {
    let (chunks, received) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break, // EOF / EIO once the shell and its jobs are gone
                Ok(read) => {
                    #[cfg(windows)]
                    active.store(true, Ordering::SeqCst);
                    if chunks.send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    std::thread::spawn(move || {
        let mut last_sent: Option<Instant> = None;
        while let Ok(first) = received.recv() {
            // A frame closes no sooner than one frame after the previous one was sent.
            let started = match last_sent {
                Some(sent) if sent.elapsed() < TERMINAL_FRAME => sent,
                _ => Instant::now(),
            };
            let mut frame = first;
            let mut ended = false;
            while frame.len() < MAX_FRAME_BYTES {
                let Some(left) = TERMINAL_FRAME.checked_sub(started.elapsed()) else {
                    break;
                };
                match received.recv_timeout(left) {
                    Ok(more) => frame.extend_from_slice(&more),
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        ended = true;
                        break;
                    }
                }
            }
            if !ended && frame.len() < MAX_FRAME_BYTES {
                if let Some(left) = TERMINAL_FRAME.checked_sub(started.elapsed()) {
                    std::thread::sleep(left);
                }
            }
            #[cfg(unix)]
            if let Some(observed) = &observed {
                observed.record(&frame);
            }
            sink(TerminalEvent::Output(frame));
            last_sent = Some(Instant::now());
            if ended {
                break;
            }
        }
        sink(TerminalEvent::Exited);
    });
}

#[cfg(unix)]
fn foreground_pgid_of(master: &Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>) -> Option<i32> {
    let guard = lock(master);
    let master = guard.as_ref()?;
    master.process_group_leader()
}

/// The executable name of `pid`, or `None` if it has none or already exited. Via `sysinfo`
/// (whose own `unsafe` stays inside that crate, not ours — the workspace forbids `unsafe_code`
/// outside `sandbox-windows`), targeting only this one pid rather than scanning every process.
#[cfg(unix)]
fn process_name(pid: i32) -> Option<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let pid = Pid::from(usize::try_from(pid).ok()?);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        false,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    system
        .process(pid)
        .map(|process| process.name().to_string_lossy().into_owned())
}

#[cfg(unix)]
const AGENT_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Where [`spawn_agent_status_poller`] installs an agent's own status hooks.
#[cfg(unix)]
struct AgentHookContext {
    workspace_root: PathBuf,
    hook_server: Option<Arc<crate::agent_hooks::AgentHookServer>>,
}

/// Registers this session with the hook server and installs Claude Code's status hooks into the
/// workspace. Returns the registration even if the install failed: the poller then simply never
/// hears from it and keeps reading the screen. `None` when there is no hook server.
#[cfg(unix)]
fn register_claude_hooks(
    hooks: &AgentHookContext,
) -> Option<crate::agent_hooks::AgentHookRegistration> {
    let server = hooks.hook_server.as_ref()?;
    let registration = match server.register("claude") {
        Ok(registration) => registration,
        Err(error) => {
            tracing::warn!(%error, "failed to register Claude Code status hook listener");
            return None;
        }
    };
    if let Err(error) = crate::agent_hooks::install_claude_hooks(
        &hooks.workspace_root,
        server.address(),
        registration.token(),
    ) {
        tracing::warn!(%error, "failed to install Claude Code status hooks; keeping screen-based detection");
    }
    Some(registration)
}

/// Polls, twice a second, whether a known agent CLI is the terminal's foreground job and whether
/// it is busy, and publishes each change to `status_sink` — the only writer of this terminal's
/// agent status. The strongest available signal wins: the title the agent sets
/// ([`busy_from_title`], Codex), else its own hook and the screen ([`busy_from_hook_or_screen`],
/// Claude Code).
#[cfg(unix)]
fn spawn_agent_status_poller(
    master: Arc<Mutex<Option<Box<dyn MasterPty + Send>>>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    shell_pid: u32,
    observed: ObservedOutput,
    status_sink: TerminalStatusSink,
    hooks: AgentHookContext,
) {
    std::thread::spawn(move || {
        let mut last = TerminalAgentStatus::default();
        // Installed on Claude Code's first sighting, at most once per session.
        let mut claude_hooks: Option<crate::agent_hooks::AgentHookRegistration> = None;
        let mut claude_hooks_tried = false;
        loop {
            if matches!(lock(&child).try_wait(), Ok(Some(_))) {
                return;
            }
            let detected = foreground_pgid_of(&master)
                .filter(|pgid| *pgid as u32 != shell_pid)
                .and_then(process_name)
                .and_then(|name| known_agent_binary(&name));
            if detected != last.agent_id.as_deref() {
                // Another agent process (or none): what the previous one reported is stale, and
                // the Enter that launched it was typed to the shell, not to the agent.
                lock(&observed.title).reset();
                observed.line_submitted.store(false, Ordering::SeqCst);
                if let Some(registration) = &claude_hooks {
                    registration.forget();
                }
            }
            let current = match detected {
                None => TerminalAgentStatus::default(),
                Some(agent) => {
                    if agent == "claude" && !claude_hooks_tried {
                        claude_hooks_tried = true;
                        claude_hooks = register_claude_hooks(&hooks);
                    }
                    let previously_busy = last.agent_id.as_deref() == Some(agent) && last.busy;
                    let busy = observed.line_submitted.load(Ordering::SeqCst)
                        && busy_from_title(&lock(&observed.title)).unwrap_or_else(|| {
                            let hook = match (agent, &claude_hooks) {
                                ("claude", Some(registration)) => registration.reported_busy(),
                                _ => None,
                            };
                            let signal = classify(agent, &observed.recent_text());
                            busy_from_hook_or_screen(hook, signal, previously_busy)
                        });
                    TerminalAgentStatus {
                        agent_id: Some(agent.to_string()),
                        busy,
                    }
                }
            };
            if current != last {
                status_sink(current.clone());
                last = current;
            }
            std::thread::sleep(AGENT_STATUS_POLL_INTERVAL);
        }
    });
}

#[cfg(all(test, unix))]
mod agent_status_tests {
    use super::*;

    #[test]
    fn strips_ansi_escapes_before_matching() {
        let raw = b"\x1b[2mthinking \xe2\x80\xa2 \x1b[1mesc to interrupt\x1b[0m)\x1b[0m";
        let text = strip_ansi(raw);
        assert!(text.contains("esc to interrupt"), "{text}");
        assert!(!text.contains('\u{1b}'), "{text}");
    }

    #[test]
    fn known_agent_binary_maps_only_recognized_names() {
        assert_eq!(known_agent_binary("codex"), Some("codex"));
        assert_eq!(known_agent_binary("claude"), Some("claude"));
        assert_eq!(known_agent_binary("bash"), None);
    }

    #[test]
    fn classifies_claude_codes_footer() {
        let busy = "Germinating… (4s · ↓ 345 tokens)\nauto mode on (shift+tab to cycle) · esc to interrupt";
        let idle = "auto mode on (shift+tab to cycle) · ← for agents";
        assert_eq!(classify("claude", busy), BusySignal::Busy);
        assert_eq!(classify("claude", idle), BusySignal::Idle);
        assert_eq!(
            classify("claude", "4s elapsed, footer not redrawn"),
            BusySignal::Ambiguous
        );
        // A new turn's footer wins over the previous idle one still in the window.
        assert_eq!(
            classify("claude", &format!("{idle}\n{busy}")),
            BusySignal::Busy
        );
    }

    #[test]
    fn other_agents_have_no_on_screen_markers() {
        assert_eq!(
            classify(
                "codex",
                "Working (3s • esc to interrupt)\nAsk Codex to do anything"
            ),
            BusySignal::Ambiguous
        );
    }

    #[test]
    fn a_running_turn_reported_by_the_hook_stays_busy_until_the_screen_shows_it_idle() {
        assert!(busy_from_hook_or_screen(
            Some(true),
            BusySignal::Ambiguous,
            false
        ));
        assert!(busy_from_hook_or_screen(
            Some(true),
            BusySignal::Busy,
            false
        ));
        // The "turn ended" call never arrived, but the footer is back to idle.
        assert!(!busy_from_hook_or_screen(
            Some(true),
            BusySignal::Idle,
            true
        ));
    }

    #[test]
    fn a_finished_turn_reported_by_the_hook_is_idle_whatever_the_screen_still_shows() {
        // The last busy footer can linger in the window for a few seconds after the turn ends.
        assert!(!busy_from_hook_or_screen(
            Some(false),
            BusySignal::Busy,
            true
        ));
    }

    #[test]
    fn without_a_hook_the_screen_decides_and_ambiguity_keeps_the_previous_state() {
        assert!(busy_from_hook_or_screen(None, BusySignal::Busy, false));
        assert!(!busy_from_hook_or_screen(None, BusySignal::Idle, true));
        assert!(busy_from_hook_or_screen(None, BusySignal::Ambiguous, true));
        assert!(!busy_from_hook_or_screen(
            None,
            BusySignal::Ambiguous,
            false
        ));
    }

    fn tracker_fed(titles: &[&str]) -> osc_title::TitleTracker {
        let mut tracker = osc_title::TitleTracker::default();
        for title in titles {
            tracker.feed(format!("\x1b]0;{title}\x07").as_bytes());
        }
        tracker
    }

    #[test]
    fn a_spinner_title_means_busy_and_a_plain_one_afterwards_means_idle() {
        assert_eq!(
            busy_from_title(&tracker_fed(&[
                "agente_personal",
                "\u{280b} agente_personal"
            ])),
            Some(true)
        );
        assert_eq!(
            busy_from_title(&tracker_fed(&[
                "\u{280b} agente_personal",
                "\u{2819} agente_personal",
                "agente_personal"
            ])),
            Some(false)
        );
    }

    #[test]
    fn waiting_on_an_approval_counts_as_busy() {
        assert_eq!(
            busy_from_title(&tracker_fed(&[
                "\u{280b} agente_personal",
                "[ ! ] Action Required | agente_personal"
            ])),
            Some(true)
        );
    }

    #[test]
    fn a_title_that_never_animated_says_nothing() {
        // Codex with title animations off only ever sets its bare project name.
        assert_eq!(busy_from_title(&tracker_fed(&["agente_personal"])), None);
        assert_eq!(busy_from_title(&osc_title::TitleTracker::default()), None);
    }

    #[test]
    fn only_a_submitted_line_counts_as_input_to_the_agent() {
        let observed = ObservedOutput::new();
        observed.record_input(b"hello");
        observed.record_input(b"\x1b[A");
        assert!(!observed.line_submitted.load(Ordering::SeqCst));
        observed.record_input(b"\r");
        assert!(observed.line_submitted.load(Ordering::SeqCst));
    }

    #[test]
    fn observed_output_keeps_the_title_and_the_recent_text() {
        let observed = ObservedOutput::new();
        observed.record("\x1b]0;\u{280b} proj\x07".as_bytes());
        observed.record(b"\x1b[1mesc to interrupt\x1b[0m");
        assert_eq!(busy_from_title(&lock(&observed.title)), Some(true));
        assert_eq!(
            classify("claude", &observed.recent_text()),
            BusySignal::Busy
        );
    }
}
