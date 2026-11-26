//! The window title a program in the terminal last set through `OSC 0` / `OSC 2`
//! (`ESC ] 0 ; <title> BEL`, or terminated by `ESC \`).
//!
//! A title is state, not scrollback: the last one written *is* the current one, however long ago
//! it was written. That makes it a far better busy/idle signal than text on screen for a TUI that
//! only re-sends the cells that change — Codex, for instance, sets its title from its own
//! "task running" flag (a braille spinner frame while it works, a bare project name once it's
//! done), independently of what its status line happens to be painting at that moment.

/// Titles are short; anything longer is treated as garbage and ignored rather than buffered.
const MAX_TITLE_BYTES: usize = 1024;

/// What a title says about the program that set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TitleActivity {
    /// Starts with a braille-pattern spinner frame (U+2800–U+28FF): Codex's title "activity" item
    /// while a task runs.
    Working,
    /// Codex's blinking "[ ! ] Action Required" / "[ . ] Action Required" prefix while a turn is
    /// blocked on the user (an approval, a question).
    AwaitingUser,
    /// Any other title, including an empty one.
    Other,
}

pub(super) fn title_activity(title: &str) -> TitleActivity {
    let title = title.trim_start();
    if title.starts_with("[ ! ] Action Required") || title.starts_with("[ . ] Action Required") {
        return TitleActivity::AwaitingUser;
    }
    match title.chars().next() {
        Some(first) if ('\u{2800}'..='\u{28FF}').contains(&first) => TitleActivity::Working,
        _ => TitleActivity::Other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    /// Inside `ESC ]`, reading the numeric command before `;`.
    Command {
        number: u32,
        has_digits: bool,
    },
    Payload {
        is_title: bool,
    },
    /// Saw `ESC` inside a payload: `\` completes it (ST), anything else cancels it.
    PayloadEscape {
        is_title: bool,
    },
}

/// Fed every byte the terminal receives, in order, possibly split at any point across calls.
#[derive(Debug)]
pub(super) struct TitleTracker {
    state: State,
    payload: Vec<u8>,
    overflowed: bool,
    latest: Option<String>,
    activity_seen: bool,
}

impl Default for TitleTracker {
    fn default() -> Self {
        Self {
            state: State::Ground,
            payload: Vec::new(),
            overflowed: false,
            latest: None,
            activity_seen: false,
        }
    }
}

impl TitleTracker {
    pub(super) fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.step(byte);
        }
    }

    /// The last title set since the last [`reset`](Self::reset), if any.
    pub(super) fn latest(&self) -> Option<&str> {
        self.latest.as_deref()
    }

    /// Whether any title since the last reset showed activity ([`TitleActivity::Working`] or
    /// [`TitleActivity::AwaitingUser`]) — proof that the program really reports its state there,
    /// so a plain title afterwards can be read as "idle" rather than "doesn't use the title".
    pub(super) fn activity_seen(&self) -> bool {
        self.activity_seen
    }

    /// Forgets the previous program's title, e.g. once it has exited, so the next one starts
    /// from nothing.
    pub(super) fn reset(&mut self) {
        self.latest = None;
        self.activity_seen = false;
    }

    fn step(&mut self, byte: u8) {
        const ESC: u8 = 0x1b;
        const BEL: u8 = 0x07;
        self.state = match self.state {
            State::Ground => {
                if byte == ESC {
                    State::Escape
                } else {
                    State::Ground
                }
            }
            State::Escape => match byte {
                b']' => State::Command {
                    number: 0,
                    has_digits: false,
                },
                ESC => State::Escape,
                _ => State::Ground,
            },
            State::Command { number, has_digits } => match byte {
                b'0'..=b'9' => State::Command {
                    number: number
                        .saturating_mul(10)
                        .saturating_add(u32::from(byte - b'0')),
                    has_digits: true,
                },
                b';' => {
                    self.payload.clear();
                    self.overflowed = false;
                    State::Payload {
                        is_title: has_digits && (number == 0 || number == 2),
                    }
                }
                BEL => State::Ground,
                ESC => State::Escape,
                _ => State::Payload { is_title: false },
            },
            State::Payload { is_title } => match byte {
                BEL => {
                    self.finish(is_title);
                    State::Ground
                }
                ESC => State::PayloadEscape { is_title },
                _ => {
                    if is_title {
                        if self.payload.len() < MAX_TITLE_BYTES {
                            self.payload.push(byte);
                        } else {
                            self.overflowed = true;
                        }
                    }
                    State::Payload { is_title }
                }
            },
            State::PayloadEscape { is_title } => {
                if byte == b'\\' {
                    self.finish(is_title);
                    State::Ground
                } else {
                    // An ESC that is not ST cancels the OSC and starts a new escape sequence.
                    self.state = State::Escape;
                    self.step(byte);
                    return;
                }
            }
        };
    }

    fn finish(&mut self, is_title: bool) {
        if !is_title || self.overflowed {
            return;
        }
        let title = String::from_utf8_lossy(&self.payload).into_owned();
        if title_activity(&title) != TitleActivity::Other {
            self.activity_seen = true;
        }
        self.latest = Some(title);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_bel_terminated_osc_0_title() {
        let mut tracker = TitleTracker::default();
        tracker.feed(b"hello \x1b]0;agente_personal\x07 world");
        assert_eq!(tracker.latest(), Some("agente_personal"));
    }

    #[test]
    fn reads_an_st_terminated_osc_2_title() {
        let mut tracker = TitleTracker::default();
        tracker.feed(b"\x1b]2;my title\x1b\\");
        assert_eq!(tracker.latest(), Some("my title"));
    }

    #[test]
    fn a_title_split_across_reads_is_still_read_whole() {
        let mut tracker = TitleTracker::default();
        let bytes = "\x1b]0;\u{280b} agente_personal\x07".as_bytes();
        for chunk in bytes.chunks(1) {
            tracker.feed(chunk);
        }
        assert_eq!(tracker.latest(), Some("\u{280b} agente_personal"));
    }

    #[test]
    fn the_last_title_wins() {
        let mut tracker = TitleTracker::default();
        tracker.feed("\x1b]0;\u{2819} proj\x07".as_bytes());
        tracker.feed(b"\x1b]0;proj\x07");
        assert_eq!(tracker.latest(), Some("proj"));
    }

    #[test]
    fn other_osc_commands_and_csi_sequences_are_not_titles() {
        let mut tracker = TitleTracker::default();
        tracker.feed(b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07\x1b[2J\x1b]1;icon\x07");
        assert_eq!(tracker.latest(), None);
    }

    #[test]
    fn an_escape_that_is_not_st_cancels_the_title() {
        let mut tracker = TitleTracker::default();
        tracker.feed(b"\x1b]0;half\x1b[0mrest\x07");
        assert_eq!(tracker.latest(), None);
    }

    #[test]
    fn an_oversized_title_is_ignored() {
        let mut tracker = TitleTracker::default();
        let mut bytes = b"\x1b]0;".to_vec();
        bytes.extend(vec![b'x'; MAX_TITLE_BYTES + 1]);
        bytes.push(0x07);
        tracker.feed(&bytes);
        assert_eq!(tracker.latest(), None);
    }

    #[test]
    fn classifies_codexs_working_awaiting_and_idle_titles() {
        assert_eq!(
            title_activity("\u{280b} agente_personal"),
            TitleActivity::Working
        );
        assert_eq!(
            title_activity("\u{28cf} agente_personal"),
            TitleActivity::Working
        );
        assert_eq!(
            title_activity("[ ! ] Action Required | agente_personal"),
            TitleActivity::AwaitingUser
        );
        assert_eq!(
            title_activity("[ . ] Action Required | agente_personal"),
            TitleActivity::AwaitingUser
        );
        assert_eq!(title_activity("agente_personal"), TitleActivity::Other);
        assert_eq!(title_activity(""), TitleActivity::Other);
        assert_eq!(title_activity("✳ Claude Code"), TitleActivity::Other);
    }

    #[test]
    fn activity_is_remembered_until_reset() {
        let mut tracker = TitleTracker::default();
        tracker.feed(b"\x1b]0;agente_personal\x07");
        assert!(
            !tracker.activity_seen(),
            "a plain title alone proves nothing"
        );
        tracker.feed("\x1b]0;\u{280b} agente_personal\x07".as_bytes());
        tracker.feed(b"\x1b]0;agente_personal\x07");
        assert!(tracker.activity_seen());
        assert_eq!(tracker.latest(), Some("agente_personal"));
        tracker.reset();
        assert!(!tracker.activity_seen());
        assert_eq!(tracker.latest(), None);
    }
}
