//! Ctrl-click target recognition and dispatch.
//!
//! tmux supplies several imperfect views of a click: a semantic hyperlink, a
//! word, a physical line, and optionally a pane capture. Resolution proceeds
//! from the most authoritative source to conservative text heuristics. The
//! output is typed as a browser URL or Neovim file request so transport policy
//! stays out of the parser.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;

use crate::browser;

/// Raw fields supplied by the tmux mouse binding.
#[derive(Clone, Debug, Default)]
pub struct Input {
    /// OSC-8 hyperlink under the pointer.
    pub hyperlink: String,
    /// tmux's word under the pointer.
    pub word: String,
    /// Physical line under the pointer.
    pub line: String,
    /// Horizontal click coordinate.
    pub x: Option<usize>,
    /// Pane working directory.
    pub cwd: String,
    /// Exact client tty that received the click.
    pub client_tty: String,
    /// Clicked pane identifier.
    pub pane: String,
    /// Exact tmux server socket containing the clicked pane.
    pub tmux_socket: String,
    /// Vertical click coordinate.
    pub y: Option<usize>,
    /// Pane top offset within the window.
    pub pane_top: Option<usize>,
    /// Pane left offset within the window.
    pub pane_left: Option<usize>,
}

/// Destination selected from click metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    /// URL for the outer terminal to open locally.
    Url(String),
    /// File target and optional remote host context for Neovim.
    File {
        /// Path with optional line and column suffix.
        target: String,
        /// `terminal` or `remote`.
        source: String,
        /// Remote host when `source` is `remote`.
        context: String,
    },
}

/// Resolve and dispatch one click. Unrecognized text is intentionally a no-op.
pub fn follow(input: &Input) -> io::Result<()> {
    let Some(target) = resolve(input) else {
        return Ok(());
    };
    match target {
        Target::Url(url) => {
            open_url(&url, input);
            Ok(())
        }
        Target::File {
            target,
            source,
            context,
        } => {
            // Pane identity is part of the routing authority, not optional
            // diagnostic context. Preserve the exact clicked pane even across
            // remote fallback so two connections to the same host can never be
            // selected by global pane enumeration order.
            let arguments = vec![
                target,
                input.cwd.clone(),
                source,
                context,
                "tmux".to_owned(),
                input.tmux_socket.clone(),
                input.pane.clone(),
            ];
            // tmux run-shell prints a failed command verbatim. TmuxLink owns
            // user-visible failure translation, so the mouse hook itself must
            // always complete successfully after dispatch.
            let _ = crate::nvim::open::open(crate::nvim::open::Mode::TmuxLink, &arguments);
            Ok(())
        }
    }
}

/// Resolve click metadata without performing terminal or editor side effects.
#[must_use]
pub fn resolve(input: &Input) -> Option<Target> {
    if !input.hyperlink.is_empty()
        && let Some(target) = from_hyperlink(&input.hyperlink)
    {
        return Some(target);
    }
    if !input.pane.is_empty()
        && let Some(target) = from_capture(input)
    {
        return Some(target);
    }
    let quoted_line = input
        .line
        .bytes()
        .any(|byte| matches!(byte, b'\'' | b'"' | b'`'));
    if quoted_line
        && let Some(x) = input.x
        && let Some(target) = from_line(&input.line, x)
    {
        return Some(target);
    }
    if !input.word.is_empty()
        && let Some(target) = from_token(&input.word)
    {
        return Some(target);
    }
    if !quoted_line && let Some(x) = input.x {
        return from_line(&input.line, x);
    }
    None
}

fn from_hyperlink(uri: &str) -> Option<Target> {
    if url_like(uri) {
        return Some(Target::Url(uri.to_owned()));
    }
    if let Some(value) = uri.strip_prefix("nvim-open://") {
        let target = percent_decode(value);
        return Some(if url_like(&target) {
            Target::Url(target)
        } else {
            local_file(target)
        });
    }
    if let Some(value) = uri.strip_prefix("lazygit-edit://") {
        return Some(local_file(percent_decode(value)));
    }
    if let Some(value) = uri.strip_prefix("nvim-remote://") {
        let (host, path) = value.split_once('/')?;
        return Some(remote_or_local_file(
            host,
            percent_decode(&format!("/{path}")),
        ));
    }
    if let Some(value) = uri.strip_prefix("file://") {
        if value.starts_with('/') {
            return Some(local_file(percent_decode(value)));
        }
        let (host, path) = value.split_once('/')?;
        return Some(remote_or_local_file(
            host,
            percent_decode(&format!("/{path}")),
        ));
    }
    None
}

fn remote_or_local_file(host: &str, target: String) -> Target {
    if local_file_host(host) {
        local_file(target)
    } else {
        Target::File {
            target,
            source: "remote".to_owned(),
            context: host.to_owned(),
        }
    }
}

fn local_file(target: String) -> Target {
    Target::File {
        target,
        source: "terminal".to_owned(),
        context: String::new(),
    }
}

fn from_token(original: &str) -> Option<Target> {
    let token = trim_token(original);
    if url_like(token) {
        return Some(Target::Url(token.to_owned()));
    }
    if let Some(target) = extension_target(token, original) {
        return Some(target);
    }
    if let Some(url) = public_url(token) {
        return Some(Target::Url(url));
    }
    path_like(token).then(|| local_file(token.to_owned()))
}

fn from_line(line: &str, x: usize) -> Option<Target> {
    if let Some(target) = spaced_rfc(line, x) {
        return Some(Target::Url(target));
    }
    for (start, end) in token_ranges(line) {
        if x < start || x >= end {
            continue;
        }
        let token = slice_chars(line, start, end);
        if let Some(target) = from_token(token) {
            return Some(target);
        }
        return from_embedded_token(token, x - start);
    }
    None
}

fn from_embedded_token(token: &str, x: usize) -> Option<Target> {
    let ranges = token_ranges(token);
    let clicked = ranges
        .iter()
        .position(|(start, end)| x >= *start && x < *end)?;
    for first in 0..=clicked {
        for last in (clicked..ranges.len()).rev() {
            let candidate = slice_chars(token, ranges[first].0, ranges[last].1);
            if path_like(trim_token(candidate)) {
                return from_token(candidate);
            }
        }
    }
    from_token(slice_chars(token, ranges[clicked].0, ranges[clicked].1))
}

fn token_ranges(line: &str) -> Vec<(usize, usize)> {
    let chars = line.chars().collect::<Vec<_>>();
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        while start < chars.len() && chars[start].is_whitespace() {
            start += 1;
        }
        if start == chars.len() {
            break;
        }
        let quote = chars[start];
        let end = if matches!(quote, '\'' | '"' | '`') {
            chars[start + 1..]
                .iter()
                .position(|character| *character == quote)
                .map_or(chars.len(), |offset| start + offset + 2)
        } else {
            chars[start..]
                .iter()
                .position(|character| character.is_whitespace())
                .map_or(chars.len(), |offset| start + offset)
        };
        ranges.push((start, end));
        start = end;
    }
    ranges
}

fn slice_chars(value: &str, start: usize, end: usize) -> &str {
    let byte_start = value
        .char_indices()
        .nth(start)
        .map_or(value.len(), |pair| pair.0);
    let byte_end = value
        .char_indices()
        .nth(end)
        .map_or(value.len(), |pair| pair.0);
    &value[byte_start..byte_end]
}

fn from_capture(input: &Input) -> Option<Target> {
    let x = input.x?;
    let physical = capture(&input.pane, false)?;
    let joined = capture(&input.pane, true)?;
    let row = click_row(&physical, input.y, &input.line, input.pane_top)?;
    let mut candidates = vec![x];
    if let Some(left) = input.pane_left
        && x >= left
    {
        candidates.push(x - left);
        if x > left {
            candidates.push(x - left - 1);
        }
    }
    candidates
        .into_iter()
        .find_map(|candidate| from_joined_capture(&physical, &joined, row, candidate))
}

fn capture(pane: &str, joined: bool) -> Option<Vec<String>> {
    let mut command = Command::new("tmux");
    command.args(["capture-pane", "-p"]);
    if joined {
        command.arg("-J");
    }
    let output = command
        .args(["-S", "0", "-E", "-", "-t", pane])
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    })
}

fn click_row(
    physical: &[String],
    y: Option<usize>,
    mouse_line: &str,
    pane_top: Option<usize>,
) -> Option<usize> {
    let mut candidates = Vec::new();
    if let Some(y) = y {
        candidates.push(y);
        if y > 0 {
            candidates.push(y - 1);
        }
        if let Some(top) = pane_top
            && y >= top
        {
            candidates.push(y - top);
            if y > top {
                candidates.push(y - top - 1);
            }
        }
    }
    if let Some(candidate) = candidates.iter().copied().find(|index| {
        *index < physical.len() && (mouse_line.is_empty() || physical[*index] == mouse_line)
    }) {
        return Some(candidate);
    }
    if mouse_line.is_empty() {
        return None;
    }
    // The clicked line is elsewhere on the live screen, or not on it at all:
    // copy mode shows history, and streaming output can move the screen
    // between the click and the capture. Only a unique match is the clicked
    // row; otherwise the caller resolves from tmux's copy of the line instead
    // of guessing a row that holds different text.
    let mut matches = physical
        .iter()
        .enumerate()
        .filter(|(_, line)| line.as_str() == mouse_line)
        .map(|(index, _)| index);
    match (matches.next(), matches.next()) {
        (Some(index), None) => Some(index),
        _ => None,
    }
}

fn from_joined_capture(
    physical: &[String],
    joined: &[String],
    clicked_row: usize,
    click_x: usize,
) -> Option<Target> {
    let mut physical_index = 0;
    for logical in joined {
        let mut offset = 0;
        let mut matched = false;
        while physical_index < physical.len() {
            let part = &physical[physical_index];
            if !logical
                .get(offset..)
                .is_some_and(|tail| tail.starts_with(part))
            {
                if !matched {
                    if physical_index == clicked_row {
                        return from_line(part, click_x);
                    }
                    physical_index += 1;
                }
                break;
            }
            if physical_index == clicked_row {
                return from_line(logical, click_x + offset).or_else(|| from_line(part, click_x));
            }
            offset += part.chars().count();
            matched = true;
            physical_index += 1;
            if offset >= logical.chars().count() {
                break;
            }
        }
    }
    None
}

fn public_url(token: &str) -> Option<String> {
    if local_endpoint().is_match(token) {
        return Some(format!("http://{token}"));
    }
    if schemeless_web().is_match(token) {
        return Some(format!("https://{token}"));
    }
    if let Some(captures) = git_remote().captures(token) {
        return Some(format!(
            "https://{}/{}",
            captures.get(1)?.as_str(),
            captures.get(2)?.as_str().trim_end_matches(".git")
        ));
    }
    if let Some(captures) = cve().captures(token) {
        return Some(format!(
            "https://www.cve.org/CVERecord?id=CVE-{}-{}",
            captures.get(1)?.as_str(),
            captures.get(2)?.as_str()
        ));
    }
    rfc().captures(token).map(|captures| {
        format!(
            "https://www.rfc-editor.org/rfc/rfc{}",
            captures.get(1).expect("RFC capture").as_str()
        )
    })
}

fn spaced_rfc(line: &str, x: usize) -> Option<String> {
    for captures in spaced_rfc_regex().captures_iter(line) {
        let whole = captures.get(1)?;
        let start = line[..whole.start()].chars().count();
        let end = start + whole.as_str().chars().count();
        let before = (start > 0).then(|| line.chars().nth(start - 1)).flatten();
        let after = line.chars().nth(end);
        if before.is_some_and(|value| value.is_alphanumeric() || value == '_')
            || after.is_some_and(|value| value.is_alphanumeric() || value == '_')
            || x < start
            || x >= end
        {
            continue;
        }
        return Some(format!(
            "https://www.rfc-editor.org/rfc/rfc{}",
            captures.get(2)?.as_str()
        ));
    }
    None
}

fn url_like(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once(':') else {
        return false;
    };
    if matches!(
        scheme.to_ascii_lowercase().as_str(),
        "file" | "nvim-open" | "nvim-remote" | "lazygit-edit"
    ) {
        return false;
    }
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "ftp" | "mailto"
    ) || generic_url().is_match(value)
}

fn path_like(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if value.chars().any(char::is_whitespace) {
        return value.starts_with('/')
            || value.starts_with("~/")
            || value.starts_with("$HOME/")
            || value.starts_with("${HOME}/")
            || value.starts_with("./")
            || value.starts_with("../")
            || value
                .split_whitespace()
                .next()
                .is_some_and(|part| part.contains('/'));
    }
    value.starts_with('/')
        || value.starts_with("~/")
        || value.starts_with("$HOME/")
        || value.starts_with("${HOME}/")
        || value.starts_with("./")
        || value.starts_with("../")
        || value.contains('/')
        || file_location().is_match(value)
}

fn trim_token(mut token: &str) -> &str {
    token = token.trim_end_matches('\r');
    token = token.trim_start_matches(['"', '\'', '`', '(', '[', '{', '<']);
    token.trim_end_matches(['"', '\'', '`', ')', ']', '}', '>', '.', ',', ';'])
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            output.push((high << 4) | low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn local_file_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if matches!(host.as_str(), "" | "localhost" | "127.0.0.1" | "::1") {
        return true;
    }
    let names = [hostname("-s"), hostname("-f")];
    if names
        .iter()
        .flatten()
        .any(|name| name.eq_ignore_ascii_case(&host))
    {
        return true;
    }
    let published = std::env::var("TERMNAV_REMOTE_LINK_HOST").ok().or_else(|| {
        std::env::var_os("TMUX").and_then(|_| {
            Command::new("tmux")
                .args(["show-environment", "-g", "TERMNAV_REMOTE_LINK_HOST"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| {
                    String::from_utf8_lossy(&output.stdout)
                        .trim()
                        .trim_start_matches("TERMNAV_REMOTE_LINK_HOST=")
                        .to_owned()
                })
        })
    });
    published.is_some_and(|value| value.eq_ignore_ascii_case(&host))
}

fn hostname(flag: &str) -> Option<String> {
    let output = Command::new("hostname").arg(flag).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// How long a fallback opener may take to fail before it counts as launched.
/// Some openers, such as a browser started in the foreground, run for the
/// browser's lifetime; they are left running, never killed.
const OPENER_GRACE: Duration = Duration::from_secs(3);

/// Open a clicked URL on the desktop behind the clicking client.
///
/// Delivery is [`browser::request_click`]: the same hops as `termnav
/// open-url`, so the request crosses an outer tmux, local or across SSH,
/// that would drop a raw write to the clicking client's tty. When the
/// terminal cannot take it, `$BROWSER` runs, as it does for `open-url`
/// callers. When neither opens it, the clicking client is told so with the
/// URL, so a click is never silently lost. Nothing reaches stdout, which tmux
/// would show in the pane.
fn open_url(url: &str, input: &Input) {
    let reason = match browser::Url::parse(url) {
        Err(reason) => reason,
        Ok(parsed) => {
            let click = browser::Click {
                socket: &input.tmux_socket,
                pane: &input.pane,
                client_tty: &input.client_tty,
            };
            match browser::request_click(&parsed, &click) {
                Ok(()) => return,
                Err(failure) => match open_with_browser(&parsed, &input.pane) {
                    Ok(()) => return,
                    Err(opener) => format!("{failure}; {opener}"),
                },
            }
        }
    };
    tell(input, &format!("termnav: could not open {url} ({reason})"));
}

/// Run `$BROWSER` with the URL, detached from the click.
///
/// Inside tmux the opener runs as if from the clicked pane (`TMUX_PANE`), so
/// an opener that delivers through `termnav open-url` chooses among that
/// pane's clients. An entry counts as launched when it exits successfully or
/// is still running after [`OPENER_GRACE`]; one that cannot start or exits
/// with a failure within it passes the URL to the next entry, as other
/// `$BROWSER` callers do.
fn open_with_browser(url: &browser::Url, pane: &str) -> Result<(), String> {
    let commands = browser_commands(&std::env::var("BROWSER").unwrap_or_default(), url.as_str());
    if commands.is_empty() {
        return Err("BROWSER is not set".to_owned());
    }
    let mut failures = Vec::new();
    for (program, arguments) in commands {
        let mut command = Command::new(&program);
        command
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        // A server started inside another tmux pane holds that outer pane's
        // id in its environment; never pass it on as this click's pane.
        if browser::valid_pane(pane) && std::env::var_os("TMUX").is_some() {
            command.env("TMUX_PANE", pane);
        } else {
            command.env_remove("TMUX_PANE");
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                failures.push(format!("{program}: {error}"));
                continue;
            }
        };
        let deadline = std::time::Instant::now() + OPENER_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => {
                    failures.push(format!("{program} failed ({status})"));
                    break;
                }
                Err(error) => {
                    failures.push(format!("{program}: {error}"));
                    break;
                }
                Ok(None) if std::time::Instant::now() >= deadline => return Ok(()),
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
    Err(failures.join("; "))
}

/// The commands a `$BROWSER` value names, in order.
///
/// This is the convention Python's `webbrowser` and xdg-utils share: entries
/// separated by `:`, each a program with optional fixed arguments separated
/// by whitespace, where `%s` stands for the URL and the URL is appended when
/// no argument holds it.
fn browser_commands(value: &str, url: &str) -> Vec<(String, Vec<String>)> {
    value
        .split(':')
        .filter_map(|entry| {
            let mut words = entry.split_whitespace();
            let program = words.next()?.to_owned();
            let mut arguments = words.map(str::to_owned).collect::<Vec<_>>();
            if arguments.iter().any(|argument| argument.contains("%s")) {
                for argument in &mut arguments {
                    *argument = argument.replace("%s", url);
                }
            } else {
                arguments.push(url.to_owned());
            }
            Some((program, arguments))
        })
        .collect()
}

/// Show `message` on the clicking client's status line, or on stderr when it
/// cannot be shown there.
///
/// tmux expands both `#{...}` formats and strftime `%` sequences in
/// display-message, so both are doubled; a URL's `%2F` would otherwise print
/// as a date. `-d` needs tmux 3.2, so a refusal retries without it.
///
/// `-c` names the clicking client. tmux 3.2 rejects it with a usage error
/// (later releases accept it, and show nothing for a client that has gone).
/// Without `-c`, tmux shows the message on the client it considers current:
/// one of the session the pane in `TMUX_PANE` resolves to, or, when that
/// session has no client, the most recently active client of any session. A
/// linked window or a grouped session can make the pane resolve to another
/// session. A click's URL must not reach another session's screen, so the
/// retry names the clicked pane and runs only while that pane resolves to an
/// attached session that is the clicking client's own (any attached session
/// when the click names no client). The check and the display are separate
/// tmux calls, so a client that detaches between them is a residual race.
fn tell(input: &Input, message: &str) {
    if input.tmux_socket.is_empty() && std::env::var_os("TMUX").is_none() {
        eprintln!("{message}");
        return;
    }
    let escaped = message.replace('#', "##").replace('%', "%%");
    let tmux = || {
        let mut command = Command::new("tmux");
        if !input.tmux_socket.is_empty() {
            command.args(["-S", &input.tmux_socket]).env_remove("TMUX");
        }
        command
    };
    let succeeds = |mut command: Command| {
        crate::process::output_timeout(&mut command, Duration::from_secs(2))
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let display = |client: Option<&str>, pane: Option<&str>, delay: bool| {
        let mut command = tmux();
        match pane {
            Some(pane) => command.env("TMUX_PANE", pane),
            None => command.env_remove("TMUX_PANE"),
        };
        command.arg("display-message");
        if let Some(client) = client {
            command.args(["-c", client]);
        }
        if delay {
            command.args(["-d", "10000"]);
        }
        command.arg(&escaped);
        succeeds(command).is_some()
    };
    let client = (!input.client_tty.is_empty()).then_some(input.client_tty.as_str());
    if client.is_some()
        && [true, false]
            .into_iter()
            .any(|delay| display(client, None, delay))
    {
        return;
    }
    let pane = browser::valid_pane(&input.pane).then_some(input.pane.as_str());
    let attached = pane.is_some_and(|pane| {
        let mut command = tmux();
        command.args([
            "display-message",
            "-p",
            "-t",
            pane,
            "#{session_id} #{session_attached}",
        ]);
        let Some(state) = succeeds(command) else {
            return false;
        };
        let Some((session, count)) = state.split_once(' ') else {
            return false;
        };
        if !count.parse::<u32>().is_ok_and(|count| count > 0) {
            return false;
        }
        let Some(client) = client else {
            return true;
        };
        let mut command = tmux();
        command.args(["list-clients", "-F", "#{client_tty} #{session_id}"]);
        succeeds(command).is_some_and(|listing| {
            listing
                .lines()
                .any(|line| line.split_once(' ') == Some((client, session)))
        })
    });
    if attached
        && [true, false]
            .into_iter()
            .any(|delay| display(None, pane, delay))
    {
        return;
    }
    eprintln!("{message}");
}

fn extension_target(token: &str, original: &str) -> Option<Target> {
    let directory = extension_directory()?;
    if !directory.is_dir() {
        return None;
    }
    const SCRIPT: &str = r#"
tmux_follow_token_detectors=()
tmux_follow_register_token_detector() { [ -n "$1" ] && tmux_follow_token_detectors+=("$1"); }
shopt -s nullglob
for extension in "$3"/*.sh; do source "$extension"; done
target= target_kind=file
for detector in "${tmux_follow_token_detectors[@]}"; do
  if "$detector" "$1" "$2"; then printf '%s\0%s' "$target_kind" "$target"; exit 0; fi
done
exit 1
"#;
    let mut command = Command::new("bash");
    command
        .args([
            "--noprofile",
            "--norc",
            "-c",
            SCRIPT,
            "termnav-extension",
            token,
            original,
        ])
        .arg(&directory);
    // Optional shell extensions run synchronously on a click. Bound both time
    // and captured bytes so one broken detector cannot wedge tmux or exhaust
    // the long-lived terminal process's memory.
    let output = crate::process::output_timeout_limited(
        &mut command,
        std::time::Duration::from_millis(500),
        16 * 1024,
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut fields = output.stdout.split(|byte| *byte == 0);
    let kind = String::from_utf8(fields.next()?.to_vec()).ok()?;
    let value = String::from_utf8(fields.next()?.to_vec()).ok()?;
    match kind.as_str() {
        "url" => Some(Target::Url(value)),
        "file" => Some(local_file(value)),
        _ => None,
    }
}

fn extension_directory() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TERMNAV_TMUX_FOLLOW_EXTENSION_DIR") {
        return Some(PathBuf::from(path));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| Path::new(value).is_absolute())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("termnav/tmux-follow/extensions.d"))
}

macro_rules! regex {
    ($name:ident, $pattern:literal) => {
        fn $name() -> &'static Regex {
            static VALUE: OnceLock<Regex> = OnceLock::new();
            VALUE.get_or_init(|| Regex::new($pattern).expect("valid built-in click regex"))
        }
    };
}

regex!(generic_url, r"(?i)^[a-z][a-z0-9+.-]*://");
regex!(
    local_endpoint,
    r"^(localhost|host\.docker\.internal|127(\.[0-9]{1,3}){3}|10(\.[0-9]{1,3}){3}|192\.168(\.[0-9]{1,3}){2}|172\.(1[6-9]|2[0-9]|3[01])(\.[0-9]{1,3}){2}|0\.0\.0\.0|\[::1\]):[0-9]{2,5}($|[/?#])"
);
regex!(
    schemeless_web,
    r"(?i)^(www\.([a-z0-9-]+\.)+[a-z]{2,}(:[0-9]{2,5})?($|[/?#])|([a-z0-9-]+\.)+[a-z]{2,}(:[0-9]{2,5})?/)"
);
regex!(
    git_remote,
    r"^git@(github\.com|gitlab\.com|bitbucket\.org):([A-Za-z0-9_.-]+(/[A-Za-z0-9_.-]+)+)(\.git)?$"
);
regex!(cve, r"(?i)^CVE-([0-9]{4})-([0-9]{4,})$");
regex!(rfc, r"(?i)^RFC-?([0-9]{3,5})$");
regex!(spaced_rfc_regex, r"(?i)(RFC[[:space:]-]+([0-9]{3,5}))");
regex!(
    file_location,
    r"^[^/[:space:]]+\.[-A-Za-z0-9_+]+:[0-9]+(:[0-9]+)?$"
);

#[cfg(test)]
mod tests {
    use super::{Input, Target, browser_commands, click_row, from_joined_capture, resolve};

    fn input(hyperlink: &str, word: &str, line: &str, x: usize) -> Input {
        Input {
            hyperlink: hyperlink.to_owned(),
            word: word.to_owned(),
            line: line.to_owned(),
            x: Some(x),
            cwd: "/repo".to_owned(),
            ..Input::default()
        }
    }

    #[test]
    fn semantic_links_preserve_local_and_remote_context() {
        assert_eq!(
            resolve(&input("file://remote.example/tmp/a%20b:4", "", "", 0)),
            Some(Target::File {
                target: "/tmp/a b:4".to_owned(),
                source: "remote".to_owned(),
                context: "remote.example".to_owned()
            })
        );
        assert_eq!(
            resolve(&input("nvim-open://src/main.rs:4", "", "", 0)),
            Some(Target::File {
                target: "src/main.rs:4".to_owned(),
                source: "terminal".to_owned(),
                context: String::new()
            })
        );
    }

    #[test]
    fn public_tokens_precede_path_fallback() {
        let cases = [
            ("HTTPS://example.com/Upper", "HTTPS://example.com/Upper"),
            (
                "github.com/example/project",
                "https://github.com/example/project",
            ),
            ("localhost:5173?debug=1", "http://localhost:5173?debug=1"),
            (
                "192.168.1.20:8080/status",
                "http://192.168.1.20:8080/status",
            ),
            (
                "git@github.com:example/project.git",
                "https://github.com/example/project",
            ),
            (
                "CVE-2024-12345",
                "https://www.cve.org/CVERecord?id=CVE-2024-12345",
            ),
            ("RFC-9110", "https://www.rfc-editor.org/rfc/rfc9110"),
        ];
        for (token, expected) in cases {
            assert_eq!(
                resolve(&input("", token, "", 0)),
                Some(Target::Url(expected.to_owned())),
                "token {token}"
            );
        }
        assert_eq!(resolve(&input("", "README.md", "", 0)), None);
    }

    #[test]
    fn quoted_and_status_paths_use_the_click_coordinate() {
        assert_eq!(
            resolve(&input(
                "",
                "file",
                "open './project dir/file.rs:12' now",
                20
            )),
            Some(Target::File {
                target: "./project dir/file.rs:12".to_owned(),
                source: "terminal".to_owned(),
                context: String::new()
            })
        );
        assert_eq!(
            resolve(&input("", "sley", "\tmodified:   src/tool/README.md", 20)),
            Some(Target::File {
                target: "src/tool/README.md".to_owned(),
                source: "terminal".to_owned(),
                context: String::new()
            })
        );
    }

    #[test]
    fn line_fallback_only_selects_the_token_under_the_pointer() {
        assert_eq!(
            resolve(&input(
                "",
                "irrelevant",
                "see RFC 9110 and src/main.rs:17",
                5
            )),
            Some(Target::Url(
                "https://www.rfc-editor.org/rfc/rfc9110".to_owned()
            ))
        );
        assert_eq!(
            resolve(&input(
                "",
                "irrelevant",
                "see RFC 9110 and src/main.rs:17",
                22
            )),
            Some(Target::File {
                target: "src/main.rs:17".to_owned(),
                source: "terminal".to_owned(),
                context: String::new(),
            })
        );
    }

    fn screen(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn the_clicked_row_matches_tmux_line() {
        let physical = screen(&["one", "two https://a.example/", "three"]);
        assert_eq!(
            click_row(&physical, Some(1), "two https://a.example/", None),
            Some(1)
        );
        // A pane below the window top: y counts from the window.
        assert_eq!(click_row(&physical, Some(6), "three", Some(4)), Some(2));
        // Without tmux's copy of the line the coordinate decides.
        assert_eq!(click_row(&physical, Some(0), "", None), Some(0));
        assert_eq!(click_row(&physical, Some(9), "", None), None);
    }

    #[test]
    fn a_line_off_the_live_screen_is_not_guessed() {
        // Scrolled copy mode: tmux reports a history line the live screen
        // does not show. This used to panic indexing an empty match list.
        let physical = screen(&["live one", "live two"]);
        assert_eq!(
            click_row(&physical, Some(1), "history https://old.example/", None),
            None
        );
        // Streaming output moved the clicked line to another unique row.
        let moved = screen(&["new", "clicked https://a.example/", "newer"]);
        assert_eq!(
            click_row(&moved, Some(0), "clicked https://a.example/", None),
            Some(1)
        );
        // A line that repeats elsewhere is ambiguous.
        let repeated = screen(&["x", "same", "y", "same"]);
        assert_eq!(click_row(&repeated, Some(0), "same", None), None);
    }

    #[test]
    fn browser_values_follow_the_shared_convention() {
        let url = "https://example.com/a";
        let command = |program: &str, arguments: &[&str]| {
            (
                program.to_owned(),
                arguments
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect(),
            )
        };
        assert_eq!(
            browser_commands("/home/u/.local/bin/browser-open", url),
            vec![command("/home/u/.local/bin/browser-open", &[url])]
        );
        assert_eq!(
            browser_commands("firefox --new-tab", url),
            vec![command("firefox", &["--new-tab", url])]
        );
        assert_eq!(
            browser_commands("chrome --app=%s:w3m", url),
            vec![
                command("chrome", &["--app=https://example.com/a"]),
                command("w3m", &[url]),
            ]
        );
        assert!(browser_commands("", url).is_empty());
        assert!(browser_commands(" : ", url).is_empty());
    }

    #[test]
    fn a_click_without_a_capture_resolves_from_tmux_line() {
        // Without a capture, as when the clicked line is not on the live
        // screen, the URL comes from tmux's copy of the line.
        let line = "see https://old.example/path for details";
        assert_eq!(
            resolve(&input("", "https", line, 8)),
            Some(Target::Url("https://old.example/path".to_owned()))
        );
    }

    #[test]
    fn joined_capture_reconstructs_wrapped_targets() {
        let physical = vec![
            "prefix /very/long/".to_owned(),
            "path/file.cpp:123:45 suffix".to_owned(),
        ];
        let joined = vec!["prefix /very/long/path/file.cpp:123:45 suffix".to_owned()];
        assert_eq!(
            from_joined_capture(&physical, &joined, 1, 4),
            Some(Target::File {
                target: "/very/long/path/file.cpp:123:45".to_owned(),
                source: "terminal".to_owned(),
                context: String::new(),
            })
        );
    }
}
