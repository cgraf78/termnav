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

/// Why a request was not written to a terminal.
#[derive(Debug)]
pub enum Undelivered {
    /// The destination is not known to act on WezTerm user vars, so writing
    /// would silently drop the URL. Callers should use their own opener. The
    /// text names what was detected, for diagnostics only.
    Declined(String),
    /// No destination could be found, or writing to it failed.
    Failed(io::Error),
}

impl From<io::Error> for Undelivered {
    fn from(error: io::Error) -> Self {
        Self::Failed(error)
    }
}

impl std::fmt::Display for Undelivered {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Declined(reason) => formatter.write_str(reason),
            Self::Failed(error) => error.fmt(formatter),
        }
    }
}

/// What interprets bytes written to one terminal hop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Terminal {
    /// WezTerm itself.
    WezTerm,
    /// Another tmux layer, which forwards one passthrough frame outward.
    Tmux,
    /// Any other or unidentified terminal.
    Other,
}

/// Terminal identity a process inherits from its environment.
#[derive(Clone, Copy, Debug, Default)]
struct Environment<'a> {
    term_program: Option<&'a str>,
    wezterm_pane: Option<&'a str>,
    /// GNU screen's session name.
    sty: Option<&'a str>,
    term: &'a str,
}

impl std::fmt::Display for Environment<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let unset = |value: Option<&str>| value.filter(|value| !value.is_empty()).is_none();
        match self.term_program.filter(|value| !value.is_empty()) {
            Some(program) => write!(formatter, "TERM_PROGRAM={program}"),
            None if !unset(self.sty) => formatter.write_str("GNU screen"),
            None => write!(formatter, "TERM={}", self.term),
        }
    }
}

/// Identify a terminal from the variables its child processes inherit.
///
/// `TERM_PROGRAM` is set by the terminal that started the process and
/// replaces an inherited value, so it decides whenever it is present; this
/// keeps a VS Code terminal launched from a WezTerm shell from passing on a
/// stale `WEZTERM_PANE`. GNU screen sets no `TERM_PROGRAM` and cannot forward
/// a tmux passthrough frame, so its `STY` declines next. `TERM=wezterm` is
/// WezTerm's own terminfo name and, unlike the others, crosses SSH by default.
/// A remaining `tmux*` or `screen*` `TERM` names a tmux layer, e.g. an SSH
/// session started inside local tmux.
fn identify(environment: &Environment<'_>) -> Terminal {
    let set = |value: Option<&str>| value.is_some_and(|value| !value.is_empty());
    match environment.term_program.filter(|value| !value.is_empty()) {
        Some("WezTerm") => Terminal::WezTerm,
        Some("tmux") => Terminal::Tmux,
        Some(_) => Terminal::Other,
        None if set(environment.sty) => Terminal::Other,
        None if set(environment.wezterm_pane) => Terminal::WezTerm,
        None if environment.term.starts_with("wezterm") => Terminal::WezTerm,
        None if terminal::tmux_mode(environment.term) == TmuxMode::Passthrough => Terminal::Tmux,
        None => Terminal::Other,
    }
}

/// Identify the terminal behind one tmux client.
///
/// tmux 3.2 and later record the terminal's XTVERSION reply, and nothing
/// else, as `client_termtype`, which also survives SSH; WezTerm answers
/// `WezTerm <version>` and tmux answers `tmux <version>`. Without a reply
/// (older tmux, a non-xterm `TERM`, or a terminal that ignores the query) the
/// client process environment and terminal name decide instead.
fn identify_client(client: &Client) -> Terminal {
    if client.termtype.starts_with("WezTerm") {
        return Terminal::WezTerm;
    }
    if client.termtype.starts_with("tmux") {
        return Terminal::Tmux;
    }
    if !client.termtype.is_empty() {
        return Terminal::Other;
    }
    // One snapshot: on macOS each separate lookup would spawn its own `ps`.
    let values = process::parent_environment(client.pid, &["TERM_PROGRAM", "WEZTERM_PANE", "STY"])
        .map(|(_, values)| values)
        .unwrap_or_default();
    let value = |index: usize| values.get(index).and_then(Option::as_deref);
    identify(&Environment {
        term_program: value(0),
        wezterm_pane: value(1),
        sty: value(2),
        term: &client.termname,
    })
}

/// Deliver one request to the WezTerm that displays the calling process.
///
/// Inside tmux the request goes straight to the tty of the most recently
/// active ordinary client attached to the caller's own session, as click
/// routing and navigation already do. That bypasses the pane, so it neither
/// needs `allow-passthrough` on this server nor interleaves with a full-screen
/// pane application such as Neovim. When that client runs inside another
/// local tmux, the same choice repeats on the parent server for the pane that
/// hosts it, and the request goes directly to the outermost client. Outside
/// tmux it goes to `terminal`, or to the controlling terminal when the caller
/// supplies none.
///
/// Delivery happens only when the destination is known to be WezTerm, which
/// is the only terminal that acts on the request; anything else would drop the
/// URL while this reported success. The one exception is a tmux layer that
/// cannot be inspected from this host, such as the local tmux behind an SSH
/// session, or one whose tmux reply proves it is tmux although no local pane
/// owns the client's tty: it receives one passthrough frame and its own outer
/// terminal is trusted, as before. A layer only guessed from inherited
/// variables is not trusted that way. A tmux client is identified by its
/// XTVERSION reply (falling back to its environment without one), a plain
/// terminal by `TERM_PROGRAM`, `STY`, `WEZTERM_PANE`, or `TERM`; the private
/// `identify` helper owns the plain-terminal order.
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
/// Returns [`Undelivered::Declined`] before writing anything when the
/// destination is not known to be WezTerm, and [`Undelivered::Failed`] when
/// no attached tmux client or terminal device can be found, or when writing
/// to it fails.
pub fn request(url: &Url, terminal: Option<&Path>) -> Result<(), Undelivered> {
    match std::env::var("TMUX") {
        Ok(tmux) if !tmux.is_empty() => request_tmux(url, &tmux),
        _ => {
            let variable = |name| std::env::var(name).ok();
            let (term_program, wezterm_pane, sty) = (
                variable("TERM_PROGRAM"),
                variable("WEZTERM_PANE"),
                variable("STY"),
            );
            let term = variable("TERM").unwrap_or_default();
            let environment = Environment {
                term_program: term_program.as_deref(),
                wezterm_pane: wezterm_pane.as_deref(),
                sty: sty.as_deref(),
                term: &term,
            };
            let mode = match identify(&environment) {
                Terminal::WezTerm => terminal::tmux_mode(&term),
                Terminal::Tmux => TmuxMode::Passthrough,
                Terminal::Other => {
                    return Err(Undelivered::Declined(format!(
                        "the terminal is not known to be WezTerm ({environment})"
                    )));
                }
            };
            let tty = terminal.map_or_else(|| PathBuf::from("/dev/tty"), Path::to_path_buf);
            Ok(write(&tty, &escape(url, mode))?)
        }
    }
}

/// Bound on local tmux nesting followed from one request. Real setups nest
/// two or three deep; the limit only stops a pathological process tree.
const MAX_TMUX_DEPTH: usize = 8;

fn request_tmux(url: &Url, tmux: &str) -> Result<(), Undelivered> {
    let mut seen = vec![canonical_socket(
        tmux.rsplitn(3, ',').last().unwrap_or(tmux),
    )];
    let (mut client, _) = tmux_client(None, &own_target(tmux)?)?;
    for _ in 0..MAX_TMUX_DEPTH {
        let mode = match identify_client(&client) {
            Terminal::WezTerm => terminal::tmux_mode(&client.termname),
            Terminal::Other => {
                return Err(Undelivered::Declined(format!(
                    "the attached tmux client is not known to be WezTerm \
                     (terminal type {:?}, terminal name {:?})",
                    client.termtype, client.termname
                )));
            }
            Terminal::Tmux => match parent_client(&client, &mut seen)? {
                Parent::Local(parent) => {
                    client = parent;
                    continue;
                }
                Parent::Uninspectable => TmuxMode::Passthrough,
                // A tmux reply proves the terminal is tmux even when ancestry
                // cannot name it; a guess from inherited variables does not.
                Parent::Unrelated if !client.termtype.is_empty() => TmuxMode::Passthrough,
                Parent::Unrelated => {
                    return Err(Undelivered::Declined(format!(
                        "the attached tmux client's terminal could not be identified \
                         (terminal name {:?})",
                        client.termname
                    )));
                }
            },
        };
        return Ok(write(Path::new(&client.tty), &escape(url, mode))?);
    }
    Err(Undelivered::Declined(
        "too many nested tmux layers".to_owned(),
    ))
}

/// Where the tmux layer behind a client leads.
enum Parent {
    /// The client of the local tmux whose pane hosts the client.
    Local(Client),
    /// No local tmux in the client's ancestry: the layer is typically across
    /// SSH, and only its passthrough can reach further.
    Uninspectable,
    /// Ancestry names a local tmux that does not host the client, e.g. a
    /// terminal emulator launched from a tmux pane runs it, or the walk
    /// would revisit a server.
    Unrelated,
}

/// Follow the tmux layer behind `client` to the next local client, if any.
///
/// A client started inside a pane of another local tmux has that pane in its
/// process ancestry. Ancestry alone can be stale, so the pane must also own
/// the client's tty.
fn parent_client(client: &Client, seen: &mut Vec<PathBuf>) -> io::Result<Parent> {
    let Some((socket, pane)) = crate::navigation::process_tmux_parent(client.pid) else {
        return Ok(Parent::Uninspectable);
    };
    let canonical = canonical_socket(&socket);
    if seen.contains(&canonical) {
        return Ok(Parent::Unrelated);
    }
    let (parent, pane_tty) = tmux_client(Some(&socket), &pane)?;
    if pane_tty != client.tty {
        return Ok(Parent::Unrelated);
    }
    seen.push(canonical);
    Ok(Parent::Local(parent))
}

/// The pane, or the session when a command runs without pane context, that
/// identifies the caller to its own tmux server.
fn own_target(tmux: &str) -> io::Result<String> {
    match std::env::var("TMUX_PANE") {
        Ok(pane) if valid_pane(&pane) => Ok(pane),
        // `$TMUX` is `socket,server-pid,session-id`; the last field names the
        // session when a command runs without pane context.
        _ => match tmux.rsplit(',').next() {
            Some(session) if !session.is_empty() && !session.starts_with('-') => {
                Ok(format!("${session}"))
            }
            _ => Err(io::Error::other("tmux session is unknown")),
        },
    }
}

/// Compare servers by socket file so a symlinked path cannot hide a cycle.
fn canonical_socket(socket: &str) -> PathBuf {
    std::fs::canonicalize(socket).unwrap_or_else(|_| PathBuf::from(socket))
}

/// One attached tmux client eligible to receive a request.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Client {
    pid: u32,
    termtype: String,
    tty: String,
    termname: String,
}

/// Find the client that should receive a request for `target`, along with
/// the tty of the pane `target` resolves to.
///
/// `server` is `None` for the caller's own server, which `$TMUX` selects, and
/// the socket of a parent server otherwise.
///
/// tmux's own best-client lookup falls back to clients of *other* sessions
/// when the target session is detached, which could open the URL on a
/// different machine. Restrict the choice to clients of the session tmux
/// resolves for the target, prefer those currently showing its window (a
/// linked window can belong to several sessions), and skip clients that
/// cannot act on the request: control-mode clients, whose tty carries the
/// control protocol, and VS Code's xterm.js terminals, which ignore WezTerm
/// user vars and would swallow it.
fn tmux_client(server: Option<&str>, target: &str) -> io::Result<(Client, String)> {
    let mut command = Command::new("tmux");
    if let Some(socket) = server {
        command.args(["-S", socket]);
        // An inherited TMUX value can retarget the command on older tmux
        // releases despite the explicit socket.
        command.env_remove("TMUX");
    }
    // One tmux invocation answers both questions so the window and client
    // views come from the same server state.
    command.args([
        "display-message",
        "-p",
        "-t",
        target,
        "#{window_id}\t#{pane_tty}",
        ";",
        "list-clients",
        "-t",
        target,
        "-F",
        "#{client_control_mode}\t#{client_activity}\t#{window_id}\t#{client_termtype}\t\
         #{client_tty}\t#{client_termname}\t#{client_pid}",
    ]);
    let output = process::output_timeout(&mut command, TMUX_TIMEOUT)?;
    if !output.status.success() {
        return Err(io::Error::other("tmux client query failed"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (header, listing) = stdout.split_once('\n').unwrap_or((&stdout, ""));
    let (window, pane_tty) = header.split_once('\t').unwrap_or((header, ""));
    let client = select_client(window, listing).ok_or_else(|| {
        // Writing raw bytes to the pane instead would be swallowed by tmux.
        io::Error::new(io::ErrorKind::NotFound, "no attached tmux client")
    })?;
    Ok((client, pane_tty.to_owned()))
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
/// first-listed client so repeated calls agree. The choice deliberately does
/// not prefer WezTerm clients: the most recent client is where the user is,
/// and opening the URL on another machine's WezTerm would be worse than
/// declining.
fn select_client(window: &str, listing: &str) -> Option<Client> {
    let eligible = listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(7, '\t');
            let control = fields.next()?;
            let activity = fields.next()?.parse::<u64>().unwrap_or(0);
            let client_window = fields.next()?;
            let termtype = fields.next()?;
            let tty = fields.next()?;
            let termname = fields.next().unwrap_or("");
            let pid = fields.next().and_then(|pid| pid.parse().ok()).unwrap_or(0);
            // Older tmux leaves the termtype empty; that only disables the
            // xterm.js filter.
            (control != "1" && !termtype.starts_with("xterm.js") && !tty.is_empty()).then(|| {
                (
                    client_window == window,
                    activity,
                    Client {
                        pid,
                        termtype: termtype.to_owned(),
                        tty: tty.to_owned(),
                        termname: termname.to_owned(),
                    },
                )
            })
        })
        .collect::<Vec<_>>();
    // `max_by_key` keeps the last maximum; reverse so ties keep the first.
    eligible
        .into_iter()
        .rev()
        .max_by_key(|(showing, activity, _)| (*showing, *activity))
        .map(|(_, _, client)| client)
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
    use super::{
        Client, Environment, Terminal, TmuxMode, Url, escape, identify, identify_client,
        select_client, valid_pane,
    };

    fn plain(term_program: Option<&str>, wezterm_pane: Option<&str>, term: &str) -> Terminal {
        identify(&Environment {
            term_program,
            wezterm_pane,
            sty: None,
            term,
        })
    }

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
        let listing = "0\t100\t@1\t\t/dev/pts/1\txterm-256color\t11\n\
                       1\t300\t@1\t\t/dev/pts/2\txterm-256color\t12\n\
                       0\t200\t@1\tWezTerm 20240203\t/dev/pts/3\ttmux-256color\t13\n";
        assert_eq!(
            select_client("@1", listing),
            Some(Client {
                pid: 13,
                termtype: "WezTerm 20240203".to_owned(),
                tty: "/dev/pts/3".to_owned(),
                termname: "tmux-256color".to_owned(),
            })
        );
        // Rows without a parseable pid still select; pid 0 reads no process.
        assert_eq!(
            select_client("@1", "0\t1\t@1\t\t/dev/pts/4\txterm\n").map(|client| client.pid),
            Some(0)
        );
    }

    #[test]
    fn client_selection_prefers_clients_showing_the_pane_window() {
        let listing = "0\t900\t@2\t\t/dev/pts/1\txterm\n0\t5\t@1\t\t/dev/pts/2\txterm\n";
        assert_eq!(select_client("@1", listing).unwrap().tty, "/dev/pts/2");
        // Without a client on that window, any session client still serves.
        assert_eq!(select_client("@9", listing).unwrap().tty, "/dev/pts/1");
    }

    #[test]
    fn client_selection_breaks_ties_by_listing_order() {
        let listing = "0\t5\t@1\t\t/dev/pts/1\txterm\n0\t5\t@1\t\t/dev/pts/2\txterm\n";
        assert_eq!(select_client("@1", listing).unwrap().tty, "/dev/pts/1");
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
    fn environments_identify_wezterm_tmux_layers_and_other_terminals() {
        assert_eq!(
            plain(Some("WezTerm"), None, "xterm-256color"),
            Terminal::WezTerm
        );
        assert_eq!(plain(None, Some("3"), "xterm-256color"), Terminal::WezTerm);
        assert_eq!(plain(None, None, "wezterm"), Terminal::WezTerm);
        assert_eq!(plain(Some("tmux"), None, "xterm-256color"), Terminal::Tmux);
        assert_eq!(plain(None, None, "tmux-256color"), Terminal::Tmux);
        assert_eq!(plain(None, None, "screen-256color"), Terminal::Tmux);
        // Empty values are unset values, not identities.
        assert_eq!(plain(Some(""), Some(""), "tmux-256color"), Terminal::Tmux);
        // An SSH session from WezTerm without forwarded variables, or any
        // other terminal, cannot be shown to act on the request.
        assert_eq!(plain(None, None, "xterm-256color"), Terminal::Other);
        assert_eq!(plain(None, None, ""), Terminal::Other);
        assert_eq!(
            plain(Some("iTerm.app"), None, "xterm-256color"),
            Terminal::Other
        );
    }

    #[test]
    fn gnu_screen_declines_unless_a_terminal_program_is_set() {
        let screen = |term_program, term| {
            identify(&Environment {
                term_program,
                wezterm_pane: Some("3"),
                sty: Some("1234.pts-0.host"),
                term,
            })
        };
        // screen cannot forward a tmux passthrough frame, even when its own
        // terminal is WezTerm.
        assert_eq!(screen(None, "screen-256color"), Terminal::Other);
        assert_eq!(screen(None, "wezterm"), Terminal::Other);
        // A terminal started inside screen sets its own TERM_PROGRAM.
        assert_eq!(screen(Some("WezTerm"), "xterm-256color"), Terminal::WezTerm);
        assert_eq!(screen(Some("tmux"), "tmux-256color"), Terminal::Tmux);
    }

    #[test]
    fn declines_name_the_detected_terminal() {
        let describe = |term_program, sty, term| {
            Environment {
                term_program,
                wezterm_pane: None,
                sty,
                term,
            }
            .to_string()
        };
        assert_eq!(
            describe(Some("vscode"), None, "xterm"),
            "TERM_PROGRAM=vscode"
        );
        assert_eq!(describe(None, Some("1.x"), "screen"), "GNU screen");
        assert_eq!(
            describe(Some(""), None, "xterm-256color"),
            "TERM=xterm-256color"
        );
    }

    #[test]
    fn terminal_program_outranks_inherited_wezterm_markers() {
        // A VS Code or Terminal.app window opened from a WezTerm shell keeps
        // WEZTERM_PANE but replaces TERM_PROGRAM.
        assert_eq!(
            plain(Some("vscode"), Some("3"), "xterm-256color"),
            Terminal::Other
        );
        assert_eq!(
            plain(Some("Apple_Terminal"), Some("3"), "wezterm"),
            Terminal::Other
        );
        assert_eq!(
            plain(Some("tmux"), Some("3"), "tmux-256color"),
            Terminal::Tmux
        );
    }

    fn client(termtype: &str, termname: &str) -> Client {
        Client {
            pid: 0,
            termtype: termtype.to_owned(),
            tty: "/dev/pts/1".to_owned(),
            termname: termname.to_owned(),
        }
    }

    #[test]
    fn tmux_clients_are_identified_by_their_xtversion_reply() {
        let wezterm = client("WezTerm 20240203-110809-5046fc22", "xterm-256color");
        assert_eq!(identify_client(&wezterm), Terminal::WezTerm);
        // The reply wins over a terminal name chosen by configuration.
        assert_eq!(
            identify_client(&client("WezTerm 20240203", "tmux-256color")),
            Terminal::WezTerm
        );
        assert_eq!(
            identify_client(&client("tmux 3.4", "tmux-256color")),
            Terminal::Tmux
        );
        assert_eq!(
            identify_client(&client("tmux 3.4", "xterm-256color")),
            Terminal::Tmux
        );
        for termtype in [
            "iTerm2 3.5.0",
            "ghostty 1.1.0",
            "XTerm(390)",
            "kitty(0.39.0)",
        ] {
            assert_eq!(
                identify_client(&client(termtype, "wezterm")),
                Terminal::Other,
                "{termtype}"
            );
        }
    }

    #[test]
    fn tmux_clients_without_a_reply_fall_back_to_their_terminal_name() {
        assert_eq!(identify_client(&client("", "wezterm")), Terminal::WezTerm);
        assert_eq!(
            identify_client(&client("", "screen-256color")),
            Terminal::Tmux
        );
        assert_eq!(
            identify_client(&client("", "xterm-256color")),
            Terminal::Other
        );
    }

    #[test]
    fn only_tmux_pane_ids_are_used_as_targets() {
        assert!(valid_pane("%12"));
        for value in ["", "%", "12", "%1x", "{mouse}", "~"] {
            assert!(!valid_pane(value), "accepted {value:?}");
        }
    }
}
