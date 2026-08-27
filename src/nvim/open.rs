//! Neovim target routing across sockets, tmux panes, and remote sessions.
//!
//! Registry parsing and transport selection live here so terminal links, tmux
//! clicks, and shell launchers share one routing policy. Every fallback narrows
//! scope: an exact socket first, then the current tmux window, then (only for a
//! remote-origin link) an existing ControlMaster or matching remote pane.

use std::collections::HashSet;
use std::env;
use std::ffi::{CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, SystemTime};

/// User-facing invocation mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// Preserve command-line filename semantics exactly.
    Cli,
    /// Return a routing failure to the caller.
    Link,
    /// Translate routing failure into a concise tmux-visible message.
    TmuxLink,
}

/// Parsed editor target and cursor coordinates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Target {
    /// Path passed to Neovim.
    pub file: String,
    /// One-based line number.
    pub line: u64,
    /// One-based column, or zero when unspecified.
    pub column: u64,
}

/// Open one target using the mode's routing and failure contract.
pub fn open(mode: Mode, arguments: &[String]) -> io::Result<i32> {
    match mode {
        Mode::Cli => {
            let Some(file) = arguments.first().filter(|value| !value.is_empty()) else {
                return Ok(1);
            };
            let cwd = arguments.get(1).map(String::as_str).unwrap_or_default();
            Ok(if current_window(&Target::cli(file), cwd, "cli") {
                0
            } else {
                1
            })
        }
        Mode::Link | Mode::TmuxLink => {
            let input = arguments.first().map(String::as_str).unwrap_or_default();
            let cwd = arguments.get(1).map(String::as_str).unwrap_or_default();
            let source = arguments.get(2).map(String::as_str).unwrap_or("terminal");
            let context = arguments.get(3).map(String::as_str).unwrap_or_default();
            let result = open_link(input, cwd, source, context);
            if mode == Mode::Link || result == 0 {
                return Ok(result);
            }
            let message = if input.is_empty() {
                "No file link target was provided".to_owned()
            } else if source == "remote" && !context.is_empty() {
                format!("No nvim session found for {context}: {input}")
            } else {
                format!("No nvim session found for file link: {input}")
            };
            show_message(&if result == 1 {
                message
            } else {
                format!("{message} (opener exit {result})")
            });
            Ok(0)
        }
    }
}

fn current_tool_paths() -> Vec<PathBuf> {
    let paths = env::split_paths(&env::var_os("PATH").unwrap_or_default()).collect::<Vec<_>>();
    // Termux has no conventional /usr/bin: its package prefix is the platform
    // system tree. Treat that bin directory like /usr/bin so a sparse SSH PATH
    // still prefers the user's managed Neovim, matching Linux and macOS.
    #[cfg(target_os = "android")]
    let platform_prefix = env::var_os("PREFIX")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    #[cfg(not(target_os = "android"))]
    let platform_prefix: Option<PathBuf> = None;
    env::var_os("HOME").map_or(paths.clone(), |home| {
        augmented_tool_paths(Path::new(&home), paths, platform_prefix.as_deref())
    })
}

fn augmented_tool_paths(
    home: &Path,
    mut paths: Vec<PathBuf>,
    platform_prefix: Option<&Path>,
) -> Vec<PathBuf> {
    let fallbacks = [
        home.join(".local/bin"),
        home.join(".local/share/mise/shims"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    let platform_bin = platform_prefix.map(|prefix| prefix.join("bin"));
    let mut insertion = paths
        .iter()
        .position(|path| {
            platform_bin.as_ref() == Some(path)
                || matches!(
                    path.to_str(),
                    Some(
                        "/bin"
                            | "/sbin"
                            | "/usr/bin"
                            | "/usr/sbin"
                            | "/usr/local/bin"
                            | "/usr/local/sbin"
                            | "/opt/homebrew/bin"
                            | "/opt/local/bin"
                    )
                )
        })
        .unwrap_or(paths.len());
    for fallback in fallbacks {
        // Preserve explicit caller prefixes such as test doubles and remote
        // administrator toolchains, while still preferring the user's managed
        // tools over generic system binaries in sparse SSH and GUI PATHs.
        if !paths.contains(&fallback) {
            paths.insert(insertion, fallback);
            insertion += 1;
        }
    }
    paths
}

fn tool_command(name: &str) -> Command {
    let paths = current_tool_paths();
    let current = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let executable = resolve_tool(name, &paths, &current).unwrap_or_else(|| PathBuf::from(name));
    let mut command = Command::new(executable);
    if let Ok(path) = env::join_paths(paths) {
        // Pass the same augmented PATH to script adapters they may use. Keeping
        // lookup and inheritance together avoids platform-specific spawn
        // behavior without mutating this process's global environment.
        command.env("PATH", path);
    }
    command
}

fn resolve_tool(name: &str, paths: &[PathBuf], current: &Path) -> Option<PathBuf> {
    paths.iter().find_map(|directory| {
        let candidate = directory.join(name);
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            // An empty or relative PATH component names a location below the
            // current directory. Spell it explicitly so Command cannot repeat
            // PATH lookup and select a different executable.
            current.join(candidate)
        };
        executable(&candidate).then_some(candidate)
    })
}

fn executable(path: &Path) -> bool {
    if !fs::metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // access(X_OK) follows the current process identity and filesystem ACLs;
    // inspecting mode bits alone can select a file this user cannot execute.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

impl Target {
    fn cli(file: &str) -> Self {
        Self {
            file: file.to_owned(),
            line: 1,
            column: 0,
        }
    }

    /// Parse terminal link suffixes and expand only stable home spellings.
    #[must_use]
    pub fn link(input: &str) -> Self {
        Self::link_with_home(input, std::env::var_os("HOME").as_deref())
    }

    fn link_with_home(input: &str, home: Option<&OsStr>) -> Self {
        let (file, line, column) = split_location(input);
        let file = file
            .strip_prefix("a/")
            .or_else(|| file.strip_prefix("b/"))
            .unwrap_or(file);
        Self {
            file: expand_home(file, home),
            line,
            column,
        }
    }
}

fn split_location(input: &str) -> (&str, u64, u64) {
    let mut parts = input.rsplitn(3, ':');
    let last = parts.next().unwrap_or_default();
    let second = parts.next();
    let third = parts.next();
    if let (Some(line_text), Some(file)) = (second, third)
        && let (Ok(line), Ok(column)) = (line_text.parse::<u64>(), last.parse::<u64>())
    {
        return (file, line, column);
    }
    if let (Some(file), Ok(line)) = (second, last.parse::<u64>()) {
        return (file, line, 0);
    }
    (input, 1, 0)
}

fn expand_home(file: &str, home: Option<&OsStr>) -> String {
    let Some(home) = home else {
        return file.to_owned();
    };
    let home = home.to_string_lossy();
    for prefix in ["~/", "$HOME/", "${HOME}/"] {
        if let Some(relative) = file.strip_prefix(prefix) {
            return format!("{home}/{relative}");
        }
    }
    if matches!(file, "$HOME" | "${HOME}") {
        return home.into_owned();
    }
    file.to_owned()
}

fn open_link(input: &str, cwd: &str, source: &str, context: &str) -> i32 {
    if input.is_empty() {
        return 0;
    }
    let target = Target::link(input);
    if source != "remote" {
        if source == "nvim" && open_socket(Path::new(context), &target, cwd, source) {
            return 0;
        }
        if std::env::var_os("TMUX").is_some() {
            return if current_window(&target, cwd, source) {
                0
            } else {
                1
            };
        }
        if open_registry(&target, cwd, source) {
            return 0;
        }
        return 1;
    }

    if !context.is_empty() {
        match crate::nvim::remote::ssh_open(context, input) {
            Ok(0) => return 0,
            Ok(10 | 12 | 13) => {}
            Ok(code) => return code,
            Err(_) => return 1,
        }
    }
    if remote_tmux_fallback(context, input) {
        0
    } else {
        1
    }
}

fn current_window(target: &Target, cwd: &str, source: &str) -> bool {
    let Some(output) = tmux_output(&["list-panes", "-F", "#{pane_id}\t#{pane_current_command}"])
    else {
        return false;
    };
    let mut first = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((pane, command)) = line.split_once('\t') else {
            continue;
        };
        if command != "nvim" {
            continue;
        }
        first.get_or_insert_with(|| pane.to_owned());
        if open_pane_registry(pane, target, cwd, source) {
            return true;
        }
    }
    first.is_some_and(|pane| tmux_send(&pane, target, cwd))
}

fn open_registry(target: &Target, cwd: &str, source: &str) -> bool {
    let Some(state) = state_directory() else {
        return false;
    };
    let registry = state.join("registry");
    let latest = registry.join("current/latest");
    let legacy = state.join("current");
    let owners = registry.join("current/owners");
    open_records(
        ordered_records(
            &registry,
            &latest,
            &owners,
            &legacy,
            Some(&state.join("panes")),
        ),
        target,
        cwd,
        source,
    )
}

fn open_pane_registry(pane: &str, target: &Target, cwd: &str, source: &str) -> bool {
    let Some(state) = state_directory() else {
        return false;
    };
    let registry = state.join("registry");
    let key = pane_key(pane);
    let latest = registry.join("panes").join(&key).join("latest");
    let owners = registry.join("panes").join(&key).join("owners");
    let legacy = state.join("panes").join(legacy_pane_key(pane));
    open_records(
        ordered_records(&registry, &latest, &owners, &legacy, None),
        target,
        cwd,
        source,
    )
}

fn state_directory() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| Path::new(value).is_absolute())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
        })?;
    Some(base.join("nvim-tmux-open"))
}

#[derive(Debug)]
struct Record {
    path: PathBuf,
    sequence: String,
    owner: String,
    socket: PathBuf,
}

fn read_record(path: &Path, expected_owner: Option<&str>, registry: &Path) -> Option<Record> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let lines = fs::read_to_string(path)
        .ok()?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if lines.len() != 4
        || lines[0] != "v2"
        || lines[1].len() != 20
        || !lines[1].bytes().all(|byte| byte.is_ascii_digit())
        || !valid_owner(&lines[2])
        || expected_owner.is_some_and(|owner| owner != lines[2])
        || lines[3].is_empty()
    {
        return None;
    }
    let marker = registry.join("owners").join(&lines[2]);
    let marker_metadata = fs::symlink_metadata(&marker).ok()?;
    if !marker_metadata.file_type().is_file()
        || fs::read_to_string(marker).ok()?.lines().collect::<Vec<_>>() != ["v2"]
    {
        return None;
    }
    Some(Record {
        path: path.to_owned(),
        sequence: lines[1].clone(),
        owner: lines[2].clone(),
        socket: PathBuf::from(&lines[3]),
    })
}

fn valid_owner(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

fn owner_records(directory: &Path, registry: &Path) -> Vec<Record> {
    let mut records = fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let owner = entry.file_name();
            read_record(&entry.path(), owner.to_str(), registry)
        })
        .collect::<Vec<_>>();
    records.sort_by(|left, right| {
        right
            .sequence
            .cmp(&left.sequence)
            .then_with(|| right.owner.cmp(&left.owner))
    });
    records
}

fn ordered_records(
    registry: &Path,
    latest: &Path,
    owners: &Path,
    legacy: &Path,
    legacy_glob: Option<&Path>,
) -> Vec<PathBuf> {
    let latest_record = read_record(latest, None, registry);
    let owner_records = owner_records(owners, registry);
    let newest = latest_record
        .as_ref()
        .map(|record| record.path.as_path())
        .or_else(|| owner_records.first().map(|record| record.path.as_path()));
    let legacy_first = legacy.exists() && newest.is_none_or(|path| !newer_than(path, legacy));
    let mut paths = Vec::new();
    if legacy_first {
        paths.push(legacy.to_owned());
    }
    if let Some(record) = latest_record {
        paths.push(record.path);
    }
    paths.extend(owner_records.into_iter().map(|record| record.path));
    if !legacy_first {
        paths.push(legacy.to_owned());
    }
    if let Some(directory) = legacy_glob
        && let Ok(entries) = fs::read_dir(directory)
    {
        paths.extend(entries.filter_map(Result::ok).map(|entry| entry.path()));
    }
    paths
}

fn newer_than(left: &Path, right: &Path) -> bool {
    let modified = |path: &Path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    };
    modified(left) > modified(right)
}

fn open_records(paths: Vec<PathBuf>, target: &Target, cwd: &str, source: &str) -> bool {
    let Some(state) = state_directory() else {
        return false;
    };
    let registry = state.join("registry");
    let mut seen = HashSet::new();
    for path in paths {
        let socket = if path.starts_with(&registry) {
            read_record(
                &path,
                path.parent().and_then(|parent| {
                    (parent.file_name() == Some(OsStr::new("owners")))
                        .then(|| path.file_name()?.to_str())
                        .flatten()
                }),
                &registry,
            )
            .map(|record| record.socket)
        } else {
            read_legacy_socket(&path)
        };
        let Some(socket) = socket.filter(|socket| seen.insert(socket.clone())) else {
            continue;
        };
        if open_socket(&socket, target, cwd, source) {
            return true;
        }
    }
    false
}

fn read_legacy_socket(path: &Path) -> Option<PathBuf> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let value = fs::read_to_string(path).ok()?;
    let mut lines = value.lines();
    let socket = lines.next()?.to_owned();
    (lines.next().is_none() && !socket.is_empty()).then(|| PathBuf::from(socket))
}

fn open_socket(socket: &Path, target: &Target, cwd: &str, source: &str) -> bool {
    if socket.as_os_str().is_empty() || !socket_is_unix(socket) {
        return false;
    }
    let expression = format!(
        "luaeval('_G.nvim_tmux_open(_A[1], _A[2], _A[3], _A[4], _A[5])', [{}, {}, {}, {}, {}])",
        vim_quote(&target.file),
        target.line,
        target.column,
        vim_quote(cwd),
        vim_quote(source)
    );
    tool_command("nvim")
        .args([
            "--server",
            &socket.to_string_lossy(),
            "--remote-expr",
            &expression,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn socket_is_unix(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

fn pane_key(pane: &str) -> String {
    let tmux = std::env::var("TMUX").unwrap_or_default();
    pane_key_for(&tmux, pane)
}

fn pane_key_for(tmux: &str, pane: &str) -> String {
    let mut fields = tmux.rsplitn(3, ',').collect::<Vec<_>>();
    fields.reverse();
    let (socket, pid) = if fields.len() == 3 {
        (fields[0], fields[1])
    } else {
        (tmux, "")
    };
    let encoded = [socket.as_bytes(), pid.as_bytes(), pane.as_bytes()]
        .into_iter()
        .enumerate()
        .flat_map(|(index, bytes)| {
            let prefix = (index > 0).then_some(0_u8);
            prefix.into_iter().chain(bytes.iter().copied())
        })
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    encoded
        .as_bytes()
        .chunks(120)
        .enumerate()
        .map(|(index, chunk)| {
            let text = String::from_utf8_lossy(chunk);
            if index == 0 {
                format!("v1-{text}")
            } else {
                format!("/{text}")
            }
        })
        .collect()
}

fn legacy_pane_key(pane: &str) -> String {
    let tmux = std::env::var("TMUX").unwrap_or_default();
    let socket = tmux.split(',').next().unwrap_or_default();
    let raw = if socket.is_empty() || socket == tmux {
        pane.to_owned()
    } else {
        format!("{socket}:{pane}")
    };
    raw.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || "_.-".contains(character) {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn tmux_send(pane: &str, target: &Target, cwd: &str) -> bool {
    let command = tmux_edit_command(target, cwd);
    tmux_status(&["send-keys", "-t", pane, "-H", "1c", "0e"])
        && tmux_status(&["send-keys", "-t", pane, "Escape"])
        && tmux_status(&["send-keys", "-t", pane, "-l", &command])
        && tmux_status(&["send-keys", "-t", pane, "Enter"])
}

fn tmux_edit_command(target: &Target, cwd: &str) -> String {
    let file = if Path::new(&target.file).is_absolute() || cwd.is_empty() {
        target.file.clone()
    } else {
        format!("{}/{file}", cwd.trim_end_matches('/'), file = target.file)
    };
    let escaped = file.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        ":exe 'e '.fnameescape(\"{escaped}\")|call cursor({},{})",
        target.line.max(1),
        target.column.max(1)
    )
}

fn remote_tmux_fallback(expected_host: &str, input: &str) -> bool {
    let Some(output) = tmux_output(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}\t#{pane_current_command}\t#{pane_start_command}\t#{pane_pid}",
    ]) else {
        return false;
    };
    let pane = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let mut fields = line.splitn(4, '\t');
            let pane = fields.next()?;
            let command = fields.next()?;
            let start = fields.next()?;
            let pid = fields.next()?.parse().ok()?;
            remote_pane_matches(command, start, pid, expected_host).then(|| pane.to_owned())
        });
    let Some(pane) = pane else {
        return false;
    };
    if !tmux_status(&["send-keys", "-t", &pane, "C-b", ":"]) {
        return false;
    }
    thread::sleep(Duration::from_millis(150));
    let command = crate::nvim::remote::remote_nvim_command(&["tmux-link", input]);
    let quoted = tmux_quote(&command);
    tmux_status(&[
        "send-keys",
        "-t",
        &pane,
        "-l",
        &format!("run-shell {quoted}"),
    ]) && tmux_status(&["send-keys", "-t", &pane, "Enter"])
}

fn remote_pane_matches(command: &str, start: &str, pid: u32, expected: &str) -> bool {
    let actual = remote_host(command, start, pid);
    if expected.is_empty() {
        return is_remote_command(command) && actual.is_some();
    }
    actual.is_some_and(|actual| hosts_match(&actual, expected))
}

fn remote_host(command: &str, start: &str, pane_pid: u32) -> Option<String> {
    if is_remote_command(command) {
        parse_remote_command(command, start).or_else(|| {
            foreground_command(pane_pid, command)
                .as_deref()
                .and_then(|line| parse_remote_command(command, line))
        })
    } else {
        extension_remote_host(command, start).or_else(|| {
            foreground_command(pane_pid, command)
                .as_deref()
                .and_then(|line| extension_remote_host(command, line))
        })
    }
}

fn parse_remote_command(command: &str, value: &str) -> Option<String> {
    let mut words = value.split_whitespace();
    if words.next()?.rsplit('/').next()? != command {
        return None;
    }
    let mut skip = false;
    for token in words {
        if skip {
            skip = false;
            continue;
        }
        if token == "--" {
            continue;
        }
        if option_takes_value(command, token) {
            skip = true;
            continue;
        }
        if token.starts_with('-') {
            continue;
        }
        return normalize_remote(token);
    }
    None
}

fn option_takes_value(command: &str, option: &str) -> bool {
    if command == "et" {
        matches!(
            option,
            "-c" | "-i" | "-l" | "-o" | "-p" | "-r" | "-S" | "-s" | "-t"
        )
    } else {
        matches!(
            option,
            "-B" | "-b"
                | "-c"
                | "-D"
                | "-E"
                | "-e"
                | "-F"
                | "-I"
                | "-i"
                | "-J"
                | "-L"
                | "-l"
                | "-m"
                | "-O"
                | "-o"
                | "-p"
                | "-Q"
                | "-R"
                | "-S"
                | "-W"
                | "-w"
        )
    }
}

fn normalize_remote(candidate: &str) -> Option<String> {
    let candidate = candidate.rsplit('@').next().unwrap_or(candidate);
    let candidate = candidate
        .strip_prefix("HostName=")
        .unwrap_or(candidate)
        .split(':')
        .next()
        .unwrap_or_default();
    (!candidate.is_empty()).then(|| candidate.to_owned())
}

fn is_remote_command(command: &str) -> bool {
    matches!(command, "ssh" | "mosh" | "et")
}

fn extension_remote_host(command: &str, start: &str) -> Option<String> {
    let output = tool_command("nvim-remote-pane-host")
        .args([command, start])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn foreground_command(root: u32, wanted: &str) -> Option<String> {
    let output = tool_command("ps")
        .args([
            "-axww", "-o", "pid=", "-o", "ppid=", "-o", "pgid=", "-o", "tpgid=", "-o", "comm=",
            "-o", "args=",
        ])
        .output()
        .ok()?;
    foreground_from_snapshot(&String::from_utf8_lossy(&output.stdout), root, wanted)
}

fn foreground_from_snapshot(snapshot: &str, root: u32, wanted: &str) -> Option<String> {
    let rows = snapshot
        .lines()
        .filter_map(parse_process_row)
        .collect::<Vec<_>>();
    rows.iter()
        .filter(|row| row.executable == wanted && row.foreground > 0 && row.group == row.foreground)
        .filter_map(|row| depth(row.pid, root, &rows).map(|depth| (depth, row.pid, &row.arguments)))
        .max_by_key(|(depth, pid, _)| (*depth, *pid))
        .map(|(_, _, arguments)| arguments.clone())
}

fn parse_process_row(line: &str) -> Option<ProcessRow> {
    let mut rest = line.trim_start();
    let mut fields = Vec::with_capacity(5);
    for _ in 0..5 {
        let end = rest.find(char::is_whitespace)?;
        fields.push(&rest[..end]);
        rest = rest[end..].trim_start();
    }
    if rest.is_empty() {
        return None;
    }
    Some(ProcessRow {
        pid: fields[0].parse().ok()?,
        parent: fields[1].parse().ok()?,
        group: fields[2].parse().ok()?,
        foreground: fields[3].parse().ok()?,
        executable: fields[4].rsplit('/').next().unwrap_or(fields[4]).to_owned(),
        arguments: rest.to_owned(),
    })
}

struct ProcessRow {
    pid: u32,
    parent: u32,
    group: i32,
    foreground: i32,
    executable: String,
    arguments: String,
}

fn depth(mut pid: u32, root: u32, rows: &[ProcessRow]) -> Option<usize> {
    for depth in 0..=rows.len() {
        if pid == root {
            return Some(depth);
        }
        pid = rows.iter().find(|row| row.pid == pid)?.parent;
    }
    None
}

fn hosts_match(actual: &str, expected: &str) -> bool {
    actual == expected
        || actual.split('.').next() == Some(expected)
        || expected.split('.').next() == Some(actual)
}

fn show_message(message: &str) {
    if std::env::var_os("TMUX").is_some() {
        let popup = format!(
            "printf '%s\\n\\nPress Enter to close.\\n' {}; IFS= read -r _",
            crate::shell::quote(message)
        );
        if tmux_status(&[
            "display-popup",
            "-E",
            "-w",
            "72",
            "-h",
            "7",
            "-T",
            "nvim open",
            &popup,
        ]) {
            return;
        }
        if tmux_status(&["display-message", message]) {
            return;
        }
    }
    eprintln!("{message}");
}

fn tmux_output(arguments: &[&str]) -> Option<Output> {
    let output = tool_command("tmux").args(arguments).output().ok()?;
    output.status.success().then_some(output)
}

fn tmux_status(arguments: &[&str]) -> bool {
    tool_command("tmux")
        .args(arguments)
        .status()
        .is_ok_and(|status| status.success())
}

fn vim_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn tmux_quote(value: &str) -> String {
    let value = crate::shell::escape_tmux_format(value);
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::{
        Target, augmented_tool_paths, executable, foreground_from_snapshot, hosts_match,
        pane_key_for, parse_remote_command, resolve_tool, tmux_edit_command, tmux_quote,
    };
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tool_test_root() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("termnav-tool-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn sparse_path_keeps_caller_tools_ahead_of_user_and_system_fallbacks() {
        let home = std::path::Path::new("/home/test");
        let paths = vec![
            std::path::PathBuf::from("/tmp/caller-tools"),
            std::path::PathBuf::from("/usr/bin"),
            std::path::PathBuf::from("/bin"),
        ];

        assert_eq!(
            augmented_tool_paths(home, paths, None),
            vec![
                std::path::PathBuf::from("/tmp/caller-tools"),
                std::path::PathBuf::from("/home/test/.local/bin"),
                std::path::PathBuf::from("/home/test/.local/share/mise/shims"),
                std::path::PathBuf::from("/opt/homebrew/bin"),
                std::path::PathBuf::from("/usr/local/bin"),
                std::path::PathBuf::from("/usr/bin"),
                std::path::PathBuf::from("/bin"),
            ]
        );
    }

    #[test]
    fn termux_system_bin_stays_after_user_tool_fallbacks() {
        let home = std::path::Path::new("/data/data/com.termux/files/home/test");
        let paths = vec![
            std::path::PathBuf::from("/tmp/caller-tools"),
            std::path::PathBuf::from("/data/data/com.termux/files/usr/bin"),
        ];

        assert_eq!(
            augmented_tool_paths(
                home,
                paths,
                Some(std::path::Path::new("/data/data/com.termux/files/usr")),
            ),
            vec![
                std::path::PathBuf::from("/tmp/caller-tools"),
                std::path::PathBuf::from("/data/data/com.termux/files/home/test/.local/bin"),
                std::path::PathBuf::from(
                    "/data/data/com.termux/files/home/test/.local/share/mise/shims"
                ),
                std::path::PathBuf::from("/opt/homebrew/bin"),
                std::path::PathBuf::from("/usr/local/bin"),
                std::path::PathBuf::from("/data/data/com.termux/files/usr/bin"),
            ]
        );
    }

    #[test]
    fn tmux_command_quote_preserves_literal_format_markers() {
        assert_eq!(
            tmux_quote("open /tmp/#{session_name}/#(printf unsafe)"),
            "\"open /tmp/##{session_name}/##(printf unsafe)\""
        );
    }

    #[test]
    fn tool_resolution_skips_an_inaccessible_shadow() {
        let root = tool_test_root();
        let shadow = root.join("shadow");
        let valid = root.join("valid");
        fs::create_dir_all(&shadow).expect("create shadow directory");
        fs::create_dir_all(&valid).expect("create valid directory");
        fs::write(shadow.join("tmux"), "shadow").expect("write shadow tool");
        fs::write(valid.join("tmux"), "valid").expect("write valid tool");
        fs::set_permissions(shadow.join("tmux"), fs::Permissions::from_mode(0o001))
            .expect("make shadow inaccessible to its owner");
        fs::set_permissions(valid.join("tmux"), fs::Permissions::from_mode(0o700))
            .expect("make valid tool executable");

        // Root can execute any regular file carrying an execute bit, so that
        // identity cannot construct this ordinary-user permission boundary.
        if !executable(&shadow.join("tmux")) {
            assert_eq!(
                resolve_tool("tmux", &[shadow, valid.clone()], &root),
                Some(valid.join("tmux"))
            );
        }
        fs::remove_dir_all(root).expect("remove tool fixture");
    }

    #[test]
    fn empty_path_component_resolves_to_an_explicit_current_directory() {
        let root = tool_test_root();
        fs::create_dir_all(&root).expect("create tool fixture");
        let tool = root.join("tmux");
        fs::write(&tool, "tool").expect("write tool");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o700))
            .expect("make tool executable");

        assert_eq!(resolve_tool("tmux", &[PathBuf::new()], &root), Some(tool));
        fs::remove_dir_all(root).expect("remove tool fixture");
    }

    #[test]
    fn link_locations_and_home_references_are_structured() {
        assert_eq!(
            Target::link_with_home(
                "a/~/src/main.rs:12:4",
                Some(std::ffi::OsStr::new("/home/test")),
            ),
            Target {
                file: "/home/test/src/main.rs".to_owned(),
                line: 12,
                column: 4
            }
        );
        assert_eq!(Target::link("name:words").line, 1);
    }

    #[test]
    fn remote_command_parser_skips_transport_options() {
        assert_eq!(
            parse_remote_command("ssh", "ssh -p 22 user@example.test"),
            Some("example.test".to_owned())
        );
        assert_eq!(
            parse_remote_command("ssh", "ssh -t dev1 tmux attach"),
            Some("dev1".to_owned())
        );
        assert_eq!(parse_remote_command("ssh", "exec /usr/bin/zsh -l"), None);
        assert!(hosts_match("dev.example.test", "dev"));
    }

    #[test]
    fn pane_registry_keys_keep_server_and_socket_identity() {
        let comma = pane_key_for("/tmp/tmux,comma.sock,101,0", "%1");
        assert_ne!(comma, pane_key_for("/tmp/tmux,comma.sock,202,0", "%1"));
        assert_ne!(comma, pane_key_for("/tmp/tmux,comma.sock,101,0", "%2"));
        assert_ne!(
            pane_key_for("/tmp/a:b,101,0", "%1"),
            pane_key_for("/tmp/a/b,101,0", "%1")
        );
        assert!(comma.split('/').all(|component| component.len() <= 123));
    }

    #[test]
    fn foreground_transport_selection_is_order_independent_and_cycle_safe() {
        let ordered = "999 1 999 999 ssh ssh unrelated.example\n\
201 100 201 210 ssh ssh background.example\n\
211 210 210 210 sleep sleep 30\n\
210 100 210 210 /usr/bin/ssh ssh dev1\n\
100 1 100 210 zsh -zsh\n";
        let shuffled = "210 100 210 210 /usr/bin/ssh ssh dev1\n\
100 1 100 210 zsh -zsh\n\
201 100 201 210 ssh ssh background.example\n\
999 1 999 999 ssh ssh unrelated.example\n";
        assert_eq!(
            foreground_from_snapshot(ordered, 100, "ssh"),
            Some("ssh dev1".to_owned())
        );
        assert_eq!(
            foreground_from_snapshot(shuffled, 100, "ssh"),
            Some("ssh dev1".to_owned())
        );
        assert_eq!(
            foreground_from_snapshot(
                "300 301 300 300 ssh ssh cycle.example\n301 300 300 300 zsh zsh\n",
                100,
                "ssh",
            ),
            None
        );
    }

    #[test]
    fn socketless_tmux_fallback_builds_one_literal_edit_command() {
        let command = tmux_edit_command(
            &Target {
                file: "weird [name]*.md".to_owned(),
                line: 12,
                column: 34,
            },
            "/tmp/project",
        );

        assert_eq!(
            command,
            r#":exe 'e '.fnameescape("/tmp/project/weird [name]*.md")|call cursor(12,34)"#
        );
    }
}
