//! The sign-in pages the Host opens for a sandboxed agent, which cannot open a browser itself.
//!
//! An agent reports its page in two ways: its login terminal prints it (Claude), or its `open`
//! shim writes it to a file in the agent's private home (Codex). Either way the Host opens only an
//! `https` address with no credentials and no port, on a reviewed sign-in host of that agent, and
//! keeps it for "Open page again". The webview never supplies the address that gets opened.
use crate::state::DesktopAppState;
use std::{
    io::Read,
    path::PathBuf,
    time::{Duration, Instant},
};
use tauri::Manager;
use tauri_plugin_opener::OpenerExt;

/// At most this much of a login terminal's output is kept while looking for the page.
const WATCH_LIMIT: usize = 64 * 1024;
/// A page the agent's shim reports is short; never read more than this.
const REPORT_LIMIT: u64 = 8192;

/// The reviewed sign-in hosts of an agent: the account hosts of its providers.
pub(super) fn hosts(agent_id: &str) -> Vec<String> {
    ::agent_registry::profiles::agent_profile(agent_id)
        .providers
        .iter()
        .filter_map(|provider| ::agent_registry::profiles::provider_profile(provider))
        .flat_map(|provider| provider.account_hosts.clone())
        .collect()
}

/// Opens `url` for agent session `id` and keeps it for "Open page again".
pub(super) fn open(app: &tauri::AppHandle, id: u32, url: String) {
    if let Ok(mut urls) = app.state::<DesktopAppState>().login_urls.lock() {
        urls.insert(id, url.clone());
    }
    if app.opener().open_url(url, None::<&str>).is_err() {
        tracing::warn!("could not open the agent sign-in page");
    }
}

/// Opens again the page kept for agent session `id`, when the browser did not open or was
/// closed.
pub(super) fn open_again(app: &tauri::AppHandle, id: u32) -> Result<(), String> {
    let url = app
        .state::<DesktopAppState>()
        .login_urls
        .lock()
        .map_err(|_| "login lock poisoned")?
        .get(&id)
        .cloned()
        .ok_or("The sign-in page is not ready yet")?;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|_| "Could not open the sign-in page".into())
}

/// Text printed by an agent's login terminal, kept only until its sign-in page is found.
#[derive(Default)]
pub(super) struct TerminalWatch {
    text: String,
    found: bool,
}

impl TerminalWatch {
    /// Adds terminal output; returns the sign-in page the first time a complete one appears.
    pub(super) fn feed(&mut self, data: &[u8], hosts: &[String]) -> Option<String> {
        if self.found {
            return None;
        }
        self.text.push_str(&String::from_utf8_lossy(data));
        if self.text.len() > WATCH_LIMIT {
            let mut cut = self.text.len() - WATCH_LIMIT;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
        }
        let url = login_url(&self.text, hosts)?;
        self.found = true;
        self.text.clear();
        Some(url)
    }
}

/// Opens each page the agent's `open` shim reports in `file`. Ends when agent session `id` is
/// gone or after `lifetime`.
pub(super) fn watch_shim_reports(
    app: tauri::AppHandle,
    id: u32,
    file: PathBuf,
    hosts: Vec<String>,
    lifetime: Duration,
) {
    // A page left over from an earlier session is never opened.
    let _ = std::fs::remove_file(&file);
    std::thread::spawn(move || {
        let deadline = Instant::now() + lifetime;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
            let alive = app
                .state::<DesktopAppState>()
                .agents
                .lock()
                .map(|agents| agents.contains_key(&id))
                .unwrap_or(false);
            if !alive {
                return;
            }
            let Ok(report) = std::fs::File::open(&file) else {
                continue;
            };
            let mut text = String::new();
            let _ = report.take(REPORT_LIMIT).read_to_string(&mut text);
            let _ = std::fs::remove_file(&file);
            if let Some(url) = login_url(&text, &hosts) {
                open(&app, id, url);
            }
        }
    });
}

/// `text` without terminal escape sequences (colors, cursor moves, hyperlinks).
fn strip_terminal_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters, then one final byte. Cursor-positioning and erase sequences
            // separate screen cells in TUIs and Windows ConPTY without emitting `\r\n`.
            Some('[') => {
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        if next != 'm' && !out.ends_with(char::is_whitespace) {
                            out.push(' ');
                        }
                        break;
                    }
                }
            }
            // OSC: up to BEL or ESC \.
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The first complete `https://` address in `text` whose host is exactly one of `hosts`, with no
/// credentials and no port.
pub(super) fn login_url(text: &str, hosts: &[String]) -> Option<String> {
    let clean = strip_terminal_escapes(text);
    let mut rest = clean.as_str();
    while let Some(start) = rest.find("https://") {
        let tail = &rest[start..];
        // An address still being written has no terminator yet; wait for the rest.
        let end = tail.find(|c: char| c.is_whitespace() || c.is_control())?;
        let candidate = tail[..end].trim_end_matches(['.', ',', ')', '>', '"', '\'']);
        if let Ok(url) = tauri::Url::parse(candidate) {
            if url.scheme() == "https"
                && url.username().is_empty()
                && url.password().is_none()
                && url.port().is_none()
                && url
                    .host_str()
                    .is_some_and(|host| hosts.iter().any(|allowed| allowed == host))
            {
                return Some(url.into());
            }
        }
        rest = &tail[end..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_pages_open_only_on_reviewed_hosts() {
        let hosts = vec!["auth.openai.com".to_string()];
        assert_eq!(
            login_url("https://auth.openai.com/oauth/authorize?x=1&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback\n", &hosts)
                .as_deref(),
            Some("https://auth.openai.com/oauth/authorize?x=1&redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback")
        );
        for bad in [
            "https://evil.example/x\n",
            "https://auth.openai.com.evil.example/x\n",
            "https://auth.openai.com@evil.example/x\n",
            "https://auth.openai.com:8443/x\n",
            "http://auth.openai.com/x\n",
            "file:///etc/passwd\n",
            "https://auth.openai.com/x",
        ] {
            assert!(login_url(bad, &hosts).is_none(), "{bad}");
        }
        // A bad address does not hide a good one printed after it.
        assert!(login_url(
            "https://evil.example/x https://auth.openai.com/ok\n",
            &hosts
        )
        .is_some());
    }

    #[test]
    fn the_real_claude_login_output_yields_its_sign_in_page() {
        let hosts = vec!["claude.com".to_string()];
        let real = "Opening browser to sign in\r\nIf the browser didn't open, visit: \u{1b}]8;;https://claude.com/cai/oauth/authorize?code=true&state=s\u{7}https://claude.com/cai/oauth/authorize?code=true&state=s\u{1b}]8;;\u{7}\r\nPaste code here if prompted >";
        assert_eq!(
            login_url(real, &hosts).as_deref(),
            Some("https://claude.com/cai/oauth/authorize?code=true&state=s")
        );
    }

    #[test]
    fn a_terminal_page_is_found_once_even_when_printed_in_pieces() {
        let hosts = vec!["claude.com".to_string()];
        let mut watch = TerminalWatch::default();
        assert!(watch
            .feed(b"visit: https://claude.com/cai/oau", &hosts)
            .is_none());
        assert_eq!(
            watch.feed(b"th?x=1\r\n", &hosts).as_deref(),
            Some("https://claude.com/cai/oauth?x=1")
        );
        assert!(watch
            .feed(b"https://claude.com/again\r\n", &hosts)
            .is_none());
        let qwen_hosts = vec!["chat.qwen.ai".to_string()];
        assert_eq!(
            login_url(
                "\u{1b}[10;1Hhttps://chat.qwen.ai/authorize?user_code=ABCD-1234\u{1b}[0m\u{1b}[11;1HWaiting",
                &qwen_hosts
            )
            .as_deref(),
            Some("https://chat.qwen.ai/authorize?user_code=ABCD-1234")
        );
    }
}
