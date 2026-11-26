//! Which known agent CLI (Codex, Claude Code) is a terminal's foreground job, and whether it is
//! busy. Unix only: the foreground job comes from `tcgetpgrp`, which has no Windows equivalent.
//!
//! A terminal hooks in at three points (see [`AgentStatusSetup`]): before its shell starts (the
//! Claude Code hook endpoint goes into the shell's environment), on every output frame and every
//! write ([`ObservedOutput`]), and once the shell runs (the poller thread). The poller is the only
//! writer of the published status. The strongest available signal wins: the title the agent sets
//! ([`busy_from_title`], Codex), else its own hook and the screen ([`busy_from_hook_or_screen`],
//! Claude Code).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;

use super::osc_title;
use super::{
    foreground_pgid_of, lock, SharedChild, SharedMaster, TerminalAgentStatus, TerminalStatusSink,
};
use crate::agent_hooks::{AgentHookRegistration, AgentHookServer};

const AGENT_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How far back the on-screen marker check looks. A time window, not just a byte count, so a
/// marker from a finished turn ages out.
const AGENT_STATUS_WINDOW: Duration = Duration::from_secs(6);
/// Safety net on top of the time window, in case of a pathological burst within it.
const RECENT_OUTPUT_BYTE_CAP: usize = 256 * 1024;

/// A terminal's agent-status watch, from before its shell starts until the poller runs.
pub(super) struct AgentStatusSetup {
    observed: ObservedOutput,
    status_sink: TerminalStatusSink,
    claude_hooks: ClaudeHooks,
}

impl AgentStatusSetup {
    /// `None` without a `status_sink`: nothing is observed at all. Otherwise registers the
    /// terminal with the hook server (if any) and hands its shell the endpoint and token.
    pub(super) fn prepare(
        status_sink: Option<TerminalStatusSink>,
        hook_server: Option<&AgentHookServer>,
        workspace_root: &Path,
        command: &mut CommandBuilder,
    ) -> Option<Self> {
        let status_sink = status_sink?;
        let registration =
            hook_server.and_then(|server| register_claude_hooks(server, workspace_root, command));
        Some(Self {
            observed: ObservedOutput::new(),
            status_sink,
            claude_hooks: ClaudeHooks {
                workspace_root: workspace_root.to_path_buf(),
                registration,
            },
        })
    }

    /// What the output pump and [`super::TerminalSession::write`] feed.
    pub(super) fn observer(&self) -> ObservedOutput {
        self.observed.clone()
    }

    /// Starts the poller for the shell `shell_pid`; it ends once that shell has exited.
    pub(super) fn start(self, master: SharedMaster, child: SharedChild, shell_pid: u32) {
        spawn_agent_status_poller(
            master,
            child,
            shell_pid,
            self.observed,
            self.status_sink,
            self.claude_hooks,
        );
    }
}

/// What the agent-status poller observes of a terminal: the output chunks from roughly the last
/// [`AGENT_STATUS_WINDOW`] for on-screen markers, the last window title set (see `osc_title`), and
/// what has been sent to the current agent.
#[derive(Clone)]
pub(super) struct ObservedOutput {
    recent: Arc<Mutex<RecentFrames>>,
    title: Arc<Mutex<osc_title::TitleTracker>>,
    /// What has been sent to the current foreground agent; cleared when it changes (the Enter that
    /// launched it was typed to the shell). An agent only has a turn to finish once it has been
    /// sent something.
    submitted: Arc<Submitted>,
}

/// Output frames with the time each arrived, oldest first.
type RecentFrames = VecDeque<(Instant, Vec<u8>)>;

/// Lines (Enter, or a pasted line) written to the terminal since the current agent appeared.
#[derive(Default)]
struct Submitted {
    /// Any line.
    line: AtomicBool,
    /// A line sent while the agent's own title showed it idle. A title spinner counts as a turn
    /// only after one: Codex also spins while it boots its MCP servers, and the Enter that
    /// answers a startup dialog (trusting the folder, for one) comes before it has set any title.
    at_idle_title: AtomicBool,
}

impl Submitted {
    fn clear(&self) {
        self.line.store(false, Ordering::SeqCst);
        self.at_idle_title.store(false, Ordering::SeqCst);
    }

    fn line(&self) -> bool {
        self.line.load(Ordering::SeqCst)
    }

    fn at_idle_title(&self) -> bool {
        self.at_idle_title.load(Ordering::SeqCst)
    }
}

impl ObservedOutput {
    fn new() -> Self {
        Self {
            recent: Arc::new(Mutex::new(VecDeque::new())),
            title: Arc::new(Mutex::new(osc_title::TitleTracker::default())),
            submitted: Arc::default(),
        }
    }

    /// Notes keystrokes written to the terminal: a carriage return or newline submits a line.
    pub(super) fn record_input(&self, bytes: &[u8]) {
        if !bytes.iter().any(|byte| matches!(byte, b'\r' | b'\n')) {
            return;
        }
        self.submitted.line.store(true, Ordering::SeqCst);
        let idle_title = lock(&self.title).latest().is_some_and(|title| {
            osc_title::title_activity(title) == osc_title::TitleActivity::Other
        });
        if idle_title {
            self.submitted.at_idle_title.store(true, Ordering::SeqCst);
        }
    }

    /// Notes one output frame, as the terminal shows it.
    pub(super) fn record(&self, frame: &[u8]) {
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
fn known_agent_binary(name: &str) -> Option<&'static str> {
    match name {
        "codex" => Some("codex"),
        "claude" => Some("claude"),
        _ => None,
    }
}

/// What Claude Code's footer in the recent, ANSI-stripped output says: it reads "esc to interrupt"
/// while a turn runs and "for agents" between turns, and Claude redraws it as a whole. No other
/// agent is read from the screen: Codex is read from its title instead ([`busy_from_title`]), since
/// it only re-sends the screen cells that change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusySignal {
    Busy,
    Idle,
    /// Neither marker is in the window — a partial redraw, a dialog, or an agent with no markers.
    /// Not evidence either way.
    Ambiguous,
}

fn claude_footer(recent_output_text: &str) -> BusySignal {
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

/// The executable name of `pid`, or `None` if it has none or already exited. Via `sysinfo`
/// (whose own `unsafe` stays inside that crate, not ours — the workspace forbids `unsafe_code`
/// outside `sandbox-windows`), targeting only this one pid rather than scanning every process.
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

/// This terminal's Claude Code status hooks: its registration with the hook server (`None` without
/// one) and the workspace whose project settings carry the hooks.
struct ClaudeHooks {
    workspace_root: PathBuf,
    registration: Option<AgentHookRegistration>,
}

/// Registers this terminal with the hook server and hands its shell the endpoint and token
/// (inherited by Claude Code, then by the hook commands it runs). In a workspace that already uses
/// Claude Code, the hooks are installed right away, before Claude can start and read them;
/// elsewhere the poller installs them when Claude first shows up. `None` if registering failed:
/// the poller then just reads the screen.
fn register_claude_hooks(
    server: &AgentHookServer,
    workspace_root: &Path,
    command: &mut CommandBuilder,
) -> Option<AgentHookRegistration> {
    let registration = match server.register("claude") {
        Ok(registration) => registration,
        Err(error) => {
            tracing::warn!(%error, "failed to register Claude Code status hook listener");
            return None;
        }
    };
    command.env(crate::agent_hooks::HOOK_URL_ENV, server.hook_base_url());
    command.env(crate::agent_hooks::HOOK_TOKEN_ENV, registration.token());
    if crate::agent_hooks::workspace_uses_claude(workspace_root) {
        install_claude_hooks(workspace_root);
    }
    Some(registration)
}

fn install_claude_hooks(workspace_root: &Path) {
    if let Err(error) = crate::agent_hooks::install_claude_hooks(workspace_root) {
        tracing::warn!(%error, "failed to install Claude Code status hooks; keeping screen-based detection");
    }
}

/// Polls, twice a second, whether a known agent CLI is the terminal's foreground job and whether
/// it is busy, and publishes each change to `status_sink`.
fn spawn_agent_status_poller(
    master: SharedMaster,
    child: SharedChild,
    shell_pid: u32,
    observed: ObservedOutput,
    status_sink: TerminalStatusSink,
    claude_hooks: ClaudeHooks,
) {
    std::thread::spawn(move || {
        let mut last = TerminalAgentStatus::default();
        // Checked on Claude Code's first sighting, at most once per session.
        let mut claude_hooks_checked = false;
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
                observed.submitted.clear();
                if let Some(registration) = &claude_hooks.registration {
                    registration.forget();
                }
            }
            let current = match detected {
                None => TerminalAgentStatus::default(),
                Some(agent) => {
                    if agent == "claude"
                        && !claude_hooks_checked
                        && claude_hooks.registration.is_some()
                    {
                        // A no-op if they were installed at spawn. Otherwise (no Claude settings in
                        // the workspace yet) this Claude has read its settings already, but the
                        // next one started anywhere in the workspace will have them.
                        claude_hooks_checked = true;
                        install_claude_hooks(&claude_hooks.workspace_root);
                    }
                    let previously_busy = last.agent_id.as_deref() == Some(agent) && last.busy;
                    let title_busy = busy_from_title(&lock(&observed.title));
                    let busy = match title_busy {
                        Some(busy) => busy && observed.submitted.at_idle_title(),
                        None if agent == "claude" => {
                            observed.submitted.line() && {
                                let hook = claude_hooks
                                    .registration
                                    .as_ref()
                                    .and_then(AgentHookRegistration::reported_busy);
                                let signal = claude_footer(&observed.recent_text());
                                busy_from_hook_or_screen(hook, signal, previously_busy)
                            }
                        }
                        // No title activity and no other signal for this agent.
                        None => false,
                    };
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

#[cfg(test)]
mod tests {
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
        assert_eq!(claude_footer(busy), BusySignal::Busy);
        assert_eq!(claude_footer(idle), BusySignal::Idle);
        assert_eq!(
            claude_footer("4s elapsed, footer not redrawn"),
            BusySignal::Ambiguous
        );
        // A new turn's footer wins over the previous idle one still in the window.
        assert_eq!(claude_footer(&format!("{idle}\n{busy}")), BusySignal::Busy);
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
        assert!(!observed.submitted.line());
        observed.record_input(b"\r");
        assert!(observed.submitted.line());
        observed.submitted.clear();
        assert!(!observed.submitted.line());
    }

    #[test]
    fn a_line_starts_a_titled_turn_only_when_sent_at_the_agents_idle_title() {
        let observed = ObservedOutput::new();
        // Answering a startup dialog: the agent has set no title yet.
        observed.record_input(b"\r");
        assert!(!observed.submitted.at_idle_title());
        // Codex booting its MCP servers: a spinner title, still no turn.
        observed.record("\x1b]0;\u{280b} proj\x07".as_bytes());
        observed.record_input(b"\r");
        assert!(!observed.submitted.at_idle_title());
        // Ready, then a prompt: that is a turn.
        observed.record(b"\x1b]0;proj\x07");
        observed.record_input(b"fix it\r");
        assert!(observed.submitted.at_idle_title());
    }

    #[test]
    fn observed_output_keeps_the_title_and_the_recent_text() {
        let observed = ObservedOutput::new();
        observed.record("\x1b]0;\u{280b} proj\x07".as_bytes());
        observed.record(b"\x1b[1mesc to interrupt\x1b[0m");
        assert_eq!(busy_from_title(&lock(&observed.title)), Some(true));
        assert_eq!(claude_footer(&observed.recent_text()), BusySignal::Busy);
    }
}
