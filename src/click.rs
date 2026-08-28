//! Ctrl-click target recognition and dispatch.
//!
//! tmux supplies several imperfect views of a click: a semantic hyperlink, a
//! word, a physical line, and optionally a pane capture. Resolution proceeds
//! from the most authoritative source to conservative text heuristics. The
//! output is typed as a browser URL or Neovim file request so transport policy
//! stays out of the parser.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use regex::Regex;

use crate::terminal::{self, TmuxMode};

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
        Target::Url(url) => open_url(&url, &input.client_tty),
        Target::File {
            target,
            source,
            context,
        } => {
            let mut arguments = vec![target, input.cwd.clone(), source];
            if !context.is_empty() {
                arguments.push(context);
            }
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
    let matches = physical
        .iter()
        .enumerate()
        .filter(|(_, line)| !mouse_line.is_empty() && line.as_str() == mouse_line)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    (matches.len() == 1)
        .then_some(matches[0])
        .or_else(|| candidates.into_iter().find(|index| *index < physical.len()))
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

fn open_url(url: &str, explicit_tty: &str) -> io::Result<()> {
    let escape = terminal::user_var("term_open_url", url, TmuxMode::Raw);
    if write_tty(explicit_tty, &escape).is_ok() {
        return Ok(());
    }
    if std::env::var_os("TMUX").is_some()
        && let Ok(output) = Command::new("tmux")
            .args(["display-message", "-p", "#{client_tty}"])
            .output()
        && output.status.success()
        && write_tty(String::from_utf8_lossy(&output.stdout).trim(), &escape).is_ok()
    {
        return Ok(());
    }
    io::stdout().write_all(&escape)
}

fn write_tty(path: &str, bytes: &[u8]) -> io::Result<()> {
    if path.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no tty"));
    }
    OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)?
        .write_all(bytes)
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
    let output = Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            SCRIPT,
            "termnav-extension",
            token,
            original,
        ])
        .arg(&directory)
        .stdin(Stdio::null())
        .output()
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
    use super::{Input, Target, from_joined_capture, resolve};

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
