//! Browser-open requests delivered to the outer terminal.
//!
//! WezTerm's `routes.setup()` handles [`USER_VAR`] by opening its value on the
//! machine that runs the terminal. The request travels back through the
//! terminal byte stream, so it crosses SSH and tmux without a second
//! transport. The URL is base64-encoded inside the escape, which means its
//! content can never terminate the sequence or inject other terminal
//! controls. [`Url::parse`] additionally limits what Termnav will publish on
//! behalf of a possibly remote process. It cannot constrain bytes that other
//! programs write to the terminal themselves, so the WezTerm handler in
//! `lib/termnav/wezterm/link-routes.lua` enforces the same policy again; the
//! WezTerm integration suite pins the two copies to the same decisions.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::process;
use crate::terminal::{self, TmuxMode};

/// WezTerm user variable that carries one browser-open request.
pub const USER_VAR: &str = "TERMNAV_OPEN_URL";

/// Schemes Termnav will ask the local desktop to open.
///
/// `open_url_schemes` in `lib/termnav/wezterm/link-routes.lua` must list the
/// same values; the WezTerm integration suite fails when they differ.
///
/// An allowlist rather than a denylist: the request usually originates on a
/// remote host, and desktop handlers for other schemes reach local files
/// (`file:`, `vscode://file/...`), mount shares (`smb:`), or launch programs
/// (single-letter Windows drive "schemes", `ms-*:`).
pub const SCHEMES: &[&str] = &["http", "https", "mailto"];

/// tmux metadata queries share the bounded budget used by other Termnav
/// subprocess probes; a wedged server must fail the request, not hang it.
const TMUX_TIMEOUT: Duration = Duration::from_secs(2);

/// A URL accepted for forwarding to the outer terminal's desktop opener.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Url(String);

impl Url {
    /// Validate an absolute URL whose scheme is in [`SCHEMES`].
    ///
    /// Control characters are rejected even though base64 framing would carry
    /// them safely: desktop openers treat them inconsistently, and no real
    /// browser URL needs them.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason when the value is empty, contains
    /// control characters, lacks a scheme, or uses an unsupported scheme.
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.is_empty() {
            return Err("URL is empty".to_owned());
        }
        if value.chars().any(char::is_control) {
            return Err("URL contains control characters".to_owned());
        }
        let Some((scheme, rest)) = value.split_once(':') else {
            return Err("URL requires a scheme".to_owned());
        };
        if rest.is_empty() {
            return Err("URL requires a scheme and a target".to_owned());
        }
        let scheme = scheme.to_ascii_lowercase();
        if !SCHEMES.contains(&scheme.as_str()) {
            return Err(format!(
                "unsupported URL scheme: {scheme} (allowed: {})",
                SCHEMES.join(", ")
            ));
        }
        Ok(Self(value.to_owned()))
    }

    /// Borrow the validated URL text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Build the escape for one request with explicit tmux framing.
#[must_use]
pub fn escape(url: &Url, mode: TmuxMode) -> Vec<u8> {
    terminal::user_var(USER_VAR, url.as_str(), mode)
}

/// Deliver one request to the terminal that displays the calling process.
///
/// Inside tmux the request goes straight to the tty of the most recently
/// active ordinary client attached to the caller's own session, as click
/// routing and navigation already do. That bypasses the pane, so it neither
/// needs `allow-passthrough` on this server nor interleaves with a full-screen
/// pane application such as Neovim. Outside tmux it goes to `terminal`, or to
/// the controlling terminal when the caller supplies none. Either destination
/// is wrapped once when its terminal type says another tmux layer sits
/// between it and WezTerm.
///
/// `terminal` exists for callers without a controlling terminal of their
/// own: Neovim's TUI runs the editor in a separate session, so its children
/// cannot open `/dev/tty` even though the editor's stderr is the terminal.
/// It is ignored inside tmux, where the attached client is authoritative.
///
/// There is deliberately no stdout fallback: a caller that captures stdout
/// would otherwise receive the escape and mistake that for delivery.
///
/// # Errors
///
/// Returns an error when no attached tmux client or terminal device can be
/// found, or when writing to it fails.
pub fn request(url: &Url, terminal: Option<&Path>) -> io::Result<()> {
    let (tty, termname) = match std::env::var("TMUX") {
        Ok(tmux) if !tmux.is_empty() => {
            let (tty, termname) = tmux_client(&tmux)?;
            (PathBuf::from(tty), termname)
        }
        _ => (
            terminal.map_or_else(|| PathBuf::from("/dev/tty"), Path::to_path_buf),
            std::env::var("TERM").unwrap_or_default(),
        ),
    };
    write(&tty, &escape(url, terminal::tmux_mode(&termname)))
}

/// Find the client that should receive a request from this tmux pane.
///
/// tmux's own best-client lookup falls back to clients of *other* sessions
/// when the caller's session is detached, which could open the URL on a
/// different machine. Restrict the choice to clients of the session tmux
/// resolves for the caller's pane, prefer those currently showing the pane's
/// window (a linked window can belong to several sessions), and skip clients
/// that cannot act on the request: control-mode clients, whose tty carries
/// the control protocol, and VS Code's xterm.js terminals, which ignore
/// WezTerm user vars and would swallow it while reporting success.
fn tmux_client(tmux: &str) -> io::Result<(String, String)> {
    let target = match std::env::var("TMUX_PANE") {
        Ok(pane) if valid_pane(&pane) => pane,
        // `$TMUX` is `socket,server-pid,session-id`; the last field names the
        // session when a command runs without pane context.
        _ => match tmux.rsplit(',').next() {
            Some(session) if !session.is_empty() && !session.starts_with('-') => {
                format!("${session}")
            }
            _ => return Err(io::Error::other("tmux session is unknown")),
        },
    };
    // One tmux invocation answers both questions so the window and client
    // views come from the same server state.
    let mut command = Command::new("tmux");
    command.args([
        "display-message",
        "-p",
        "-t",
        &target,
        "#{window_id}",
        ";",
        "list-clients",
        "-t",
        &target,
        "-F",
        "#{client_control_mode}\t#{client_activity}\t#{window_id}\t#{client_termtype}\t\
         #{client_tty}\t#{client_termname}",
    ]);
    let output = process::output_timeout(&mut command, TMUX_TIMEOUT)?;
    if !output.status.success() {
        return Err(io::Error::other("tmux client query failed"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (window, listing) = stdout.split_once('\n').unwrap_or((&stdout, ""));
    select_client(window, listing).ok_or_else(|| {
        // Writing raw bytes to the pane instead would be swallowed by tmux.
        io::Error::new(io::ErrorKind::NotFound, "no attached tmux client")
    })
}

fn valid_pane(value: &str) -> bool {
    value.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// Pick the receiving client from `list-clients` rows.
///
/// Eligible clients showing `window` win over other clients of the session;
/// within a group the most recently active wins, ties keeping tmux's
/// first-listed client so repeated calls agree.
fn select_client(window: &str, listing: &str) -> Option<(String, String)> {
    let eligible =
        listing
            .lines()
            .filter_map(|line| {
                let mut fields = line.splitn(6, '\t');
                let control = fields.next()?;
                let activity = fields.next()?.parse::<u64>().unwrap_or(0);
                let client_window = fields.next()?;
                let termtype = fields.next()?;
                let tty = fields.next()?;
                let termname = fields.next().unwrap_or("");
                // Older tmux leaves the termtype empty; that only disables the
                // xterm.js filter.
                (control != "1" && !termtype.starts_with("xterm.js") && !tty.is_empty())
                    .then_some((client_window == window, activity, tty, termname))
            })
            .collect::<Vec<_>>();
    // `max_by_key` keeps the last maximum; reverse so ties keep the first.
    eligible
        .iter()
        .rev()
        .max_by_key(|(showing, activity, _, _)| (*showing, *activity))
        .map(|(_, _, tty, termname)| ((*tty).to_owned(), (*termname).to_owned()))
}

fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    OpenOptions::new()
        .append(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)?
        .write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::{TmuxMode, Url, escape, select_client, valid_pane};

    #[test]
    fn accepts_browser_urls_case_insensitively() {
        for value in [
            "https://example.com/path?a=1&b=two three",
            "http://localhost:3000",
            "HTTPS://EXAMPLE.COM",
            "mailto:someone@example.com",
        ] {
            assert_eq!(Url::parse(value).unwrap().as_str(), value);
        }
    }

    #[test]
    fn rejects_values_without_a_scheme_or_target() {
        for value in ["", "example.com", "/tmp/file", "https:", ":x", "ht tp://x"] {
            assert!(Url::parse(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn rejects_control_characters() {
        for value in [
            "https://x/\n",
            "https://x/\u{1b}]0;title\u{7}",
            "https://x/\u{7f}",
            "https://x/\u{9b}",
        ] {
            assert!(Url::parse(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn rejects_schemes_that_reach_local_resources_or_programs() {
        for value in [
            "file:///etc/passwd",
            "FILE:///Applications/Calculator.app",
            "C:\\Windows\\System32\\calc.exe",
            "vscode://file/etc/passwd",
            "smb://server/share",
            "javascript:alert(1)",
            "data:text/html,x",
            "ms-msdt:/id",
            "nvim-open://src/main.rs",
            "lazygit-edit://path",
        ] {
            assert!(Url::parse(value).is_err(), "accepted {value:?}");
        }
    }

    #[test]
    fn escapes_match_the_established_wire_format() {
        let url = Url::parse("https://example.com/").unwrap();
        // Byte-for-byte the sequences the retired shell publisher emitted:
        // `printf '\e]1337;SetUserVar=TERMNAV_OPEN_URL=%s\a'` and its
        // tmux DCS wrapper with doubled inner ESC bytes.
        assert_eq!(
            escape(&url, TmuxMode::Raw),
            b"\x1b]1337;SetUserVar=TERMNAV_OPEN_URL=aHR0cHM6Ly9leGFtcGxlLmNvbS8=\x07"
        );
        assert_eq!(
            escape(&url, TmuxMode::Passthrough),
            b"\x1bPtmux;\x1b\x1b]1337;SetUserVar=TERMNAV_OPEN_URL=aHR0cHM6Ly9leGFtcGxlLmNvbS8=\x07\x1b\\"
        );
    }

    #[test]
    fn client_selection_prefers_recent_ordinary_clients() {
        let listing = "0\t100\t@1\t\t/dev/pts/1\txterm-256color\n\
                       1\t300\t@1\t\t/dev/pts/2\txterm-256color\n\
                       0\t200\t@1\tWezTerm 20240203\t/dev/pts/3\ttmux-256color\n";
        assert_eq!(
            select_client("@1", listing),
            Some(("/dev/pts/3".to_owned(), "tmux-256color".to_owned()))
        );
    }

    #[test]
    fn client_selection_prefers_clients_showing_the_pane_window() {
        let listing = "0\t900\t@2\t\t/dev/pts/1\txterm\n0\t5\t@1\t\t/dev/pts/2\txterm\n";
        assert_eq!(select_client("@1", listing).unwrap().0, "/dev/pts/2");
        // Without a client on that window, any session client still serves.
        assert_eq!(select_client("@9", listing).unwrap().0, "/dev/pts/1");
    }

    #[test]
    fn client_selection_breaks_ties_by_listing_order() {
        let listing = "0\t5\t@1\t\t/dev/pts/1\txterm\n0\t5\t@1\t\t/dev/pts/2\txterm\n";
        assert_eq!(select_client("@1", listing).unwrap().0, "/dev/pts/1");
    }

    #[test]
    fn client_selection_skips_clients_that_cannot_act() {
        assert_eq!(select_client("@1", ""), None);
        assert_eq!(select_client("@1", "1\t9\t@1\t\t/dev/pts/2\txterm\n"), None);
        assert_eq!(
            select_client("@1", "0\t9\t@1\txterm.js(6.1.0)\t/dev/pts/2\txterm\n"),
            None
        );
        assert_eq!(select_client("@1", "0\t9\t@1\t\t\t\n"), None);
    }

    #[test]
    fn only_tmux_pane_ids_are_used_as_targets() {
        assert!(valid_pane("%12"));
        for value in ["", "%", "12", "%1x", "{mouse}", "~"] {
            assert!(!valid_pane(value), "accepted {value:?}");
        }
    }
}
