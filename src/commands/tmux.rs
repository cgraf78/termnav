//! `termnav tmux` command adapter.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::Path;

use crate::terminal;

const HELP: &str = r#"usage:
  termnav tmux context --tty TTY --client-termname TERM [--control-mode 0|1]
  termnav tmux attention --tmux-socket PATH --client-name NAME --client-pid PID
       --client-created EPOCH --working N --pending N --failed N
  termnav tmux focus claim --parent-tmux PATH --parent-pane ID --token TOKEN --lease-ms N
  termnav tmux focus release --parent-tmux PATH --parent-pane ID --token TOKEN
  termnav tmux focus expire --parent-tmux PATH --parent-pane ID
  termnav tmux focus watch --tmux-socket PATH CLIENT [--lease-ms N] [--interval-ms N]
  termnav tmux focus stop|sync --tmux-socket PATH CLIENT
       CLIENT is --client-name NAME (a hook's #{hook_client}) or
       --client-pid PID --client-tty TTY
  termnav tmux follow-click [--stdin | [HYPERLINK WORD LINE X CWD TTY PANE Y TOP LEFT SOCKET]]
"#;
const FOLLOW_CLICK_STDIN_LIMIT: u64 = 64 * 1024;

/// Parse and execute tmux integration commands.
pub fn run(
    arguments: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> io::Result<i32> {
    let arguments = match arguments
        .iter()
        .map(|value| value.clone().into_string())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(arguments) => arguments,
        Err(_) => return usage(stderr, "arguments must be valid UTF-8"),
    };
    let Some(command) = arguments.first().map(String::as_str) else {
        return usage(stderr, "a tmux command is required");
    };
    match command {
        "-h" | "--help" | "help" => {
            stdout.write_all(HELP.as_bytes())?;
            Ok(0)
        }
        "context" => context(&arguments[1..], stdout, stderr),
        "attention" => crate::attention::command(&arguments[1..], stderr),
        "focus" => focus(&arguments[1..], stdout, stderr),
        "follow-click" => follow_click(&arguments[1..], stderr),
        _ => usage(
            stderr,
            &format!("unknown or unimplemented tmux command: {command}"),
        ),
    }
}

fn follow_click(arguments: &[String], stderr: &mut dyn Write) -> io::Result<i32> {
    let fields = if arguments.first().map(String::as_str) == Some("--stdin") {
        if arguments.len() != 1 {
            return usage(stderr, "--stdin cannot be combined with positional fields");
        }
        let mut input = Vec::new();
        io::stdin()
            .take(FOLLOW_CLICK_STDIN_LIMIT + 1)
            .read_to_end(&mut input)?;
        if input.len() as u64 > FOLLOW_CLICK_STDIN_LIMIT {
            return usage(stderr, "follow-click stdin exceeds 64 KiB");
        }
        let Ok(input) = String::from_utf8(input) else {
            return usage(stderr, "follow-click stdin must be valid UTF-8");
        };
        let fields = input.lines().map(str::to_owned).collect::<Vec<_>>();
        if fields.len() > 11 {
            return usage(stderr, "follow-click stdin contains more than eleven lines");
        }
        fields
    } else {
        arguments.to_vec()
    };
    if fields.len() > 11 {
        return usage(stderr, "follow-click accepts at most eleven fields");
    }
    let field = |index: usize| fields.get(index).cloned().unwrap_or_default();
    let number = |index: usize| fields.get(index).and_then(|value| value.parse().ok());
    let input = crate::click::Input {
        hyperlink: field(0),
        word: field(1),
        line: field(2),
        x: number(3),
        cwd: field(4),
        client_tty: field(5),
        pane: field(6),
        y: number(7),
        pane_top: number(8),
        pane_left: number(9),
        tmux_socket: field(10),
    };
    crate::click::follow(&input)?;
    Ok(0)
}

fn focus(arguments: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> io::Result<i32> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return usage(stderr, "focus requires a command");
    };
    if matches!(command, "-h" | "--help" | "help") {
        stdout.write_all(HELP.as_bytes())?;
        return Ok(0);
    }
    let options = match FocusOptions::parse(&arguments[1..]) {
        Ok(options) => options,
        Err(message) => return usage(stderr, &message),
    };
    match command {
        "claim" => {
            let (Some(socket), Some(pane), Some(token), Some(lease)) = (
                options.parent_tmux,
                options.parent_pane,
                options.token,
                options.lease_ms,
            ) else {
                return usage(
                    stderr,
                    "focus claim requires parent, token, and lease options",
                );
            };
            let Some(published) = crate::focus::claim(&socket, &pane, &token, lease) else {
                return Ok(1);
            };
            if published.start_expirer && !crate::focus::start_expirer(&socket, &pane) {
                let _ = crate::focus::release(&socket, &pane, &token);
                return Ok(1);
            }
            Ok(0)
        }
        "release" => {
            let (Some(socket), Some(pane), Some(token)) =
                (options.parent_tmux, options.parent_pane, options.token)
            else {
                return usage(stderr, "focus release requires parent and token options");
            };
            Ok(if crate::focus::release(&socket, &pane, &token) {
                0
            } else {
                1
            })
        }
        "expire" => {
            let (Some(socket), Some(pane)) = (options.parent_tmux, options.parent_pane) else {
                return usage(stderr, "focus expire requires parent options");
            };
            Ok(crate::focus::expire(&socket, &pane))
        }
        "watch" | "stop" | "sync" => {
            // Leave room for scheduling and relay delays between renewals.
            // Normal SSH shutdown releases its claim without waiting for this
            // crash fallback, so it need not trade resilience for a fast exit.
            let lease_ms = options.lease_ms.unwrap_or(3500);
            let interval_ms = options.interval_ms.unwrap_or(1000);
            // Reject invalid timing before asking tmux about the client, so the
            // result does not depend on whether that client still exists.
            if command == "watch" && !crate::focus::valid_watch_timing(lease_ms, interval_ms) {
                return usage(stderr, "focus watch lease must cover two intervals");
            }
            let (socket, pid, tty, name) = match focus_client(options, command) {
                FocusClient::Exact {
                    socket,
                    pid,
                    tty,
                    name,
                } => (socket, pid, tty, name),
                FocusClient::Absent => return Ok(0),
                FocusClient::Invalid(message) => return usage(stderr, &message),
            };
            Ok(match command {
                "watch" => crate::focus::watch(&socket, pid, &tty, lease_ms, interval_ms),
                "stop" => crate::focus::stop_watch(&socket, pid, &tty, name.as_deref()),
                _ if crate::focus::sync_client_style(&socket, pid, &tty) => 0,
                _ => 1,
            })
        }
        _ => usage(stderr, &format!("unknown focus command: {command}")),
    }
}

/// The client a watch, stop, or sync request targets.
enum FocusClient {
    /// One live interactive client.
    Exact {
        socket: String,
        pid: u32,
        tty: String,
        name: Option<String>,
    },
    /// The named client has gone or is a control-mode transport, so there is
    /// no terminal focus to publish. A watcher left by a departed client exits
    /// on its next heartbeat.
    Absent,
    /// The options do not name exactly one client.
    Invalid(String),
}

/// Resolve the client from either its tmux name or its exact PID and tty.
///
/// Notification hooks must pass `#{hook_client}`: their `#{client_pid}` and
/// `#{client_tty}` describe the session's most recently active client, which
/// in a shared session can be a different client from the one that triggered
/// the hook. Command hooks such as `after-select-pane` run with the invoking
/// client and keep the PID and tty form, which older configurations also use.
fn focus_client(options: FocusOptions, command: &str) -> FocusClient {
    let missing = || FocusClient::Invalid(format!("focus {command} requires exact client options"));
    let Some(socket) = options.tmux_socket else {
        return missing();
    };
    match (options.client_name, options.client_pid, options.client_tty) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => FocusClient::Invalid(
            "--client-name cannot be combined with --client-pid or --client-tty".to_owned(),
        ),
        (Some(name), None, None) => match crate::focus::named_client(&socket, &name) {
            Some(client) if !client.control_mode => FocusClient::Exact {
                socket,
                pid: client.pid,
                tty: client.tty,
                name: Some(name),
            },
            _ => FocusClient::Absent,
        },
        (None, Some(pid), Some(tty)) => FocusClient::Exact {
            socket,
            pid,
            tty,
            name: None,
        },
        _ => missing(),
    }
}

#[derive(Default)]
struct FocusOptions {
    parent_tmux: Option<String>,
    parent_pane: Option<String>,
    token: Option<String>,
    lease_ms: Option<u64>,
    interval_ms: Option<u64>,
    tmux_socket: Option<String>,
    client_pid: Option<u32>,
    client_tty: Option<String>,
    client_name: Option<String>,
}

impl FocusOptions {
    fn parse(arguments: &[String]) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut index = 0;
        while index < arguments.len() {
            let name = arguments[index].as_str();
            let Some(value) = arguments.get(index + 1) else {
                return Err(format!("{name} requires a value"));
            };
            match name {
                "--parent-tmux" => parsed.parent_tmux = Some(value.clone()),
                "--parent-pane" => parsed.parent_pane = Some(value.clone()),
                "--token" if crate::focus::valid_token(value) => parsed.token = Some(value.clone()),
                "--token" => return Err("token must be 24 lowercase hex characters".to_owned()),
                "--lease-ms" => {
                    let lease = value.parse().map_err(|_| "invalid lease".to_owned())?;
                    if !crate::focus::valid_lease(lease) {
                        return Err("lease is outside the supported range".to_owned());
                    }
                    parsed.lease_ms = Some(lease);
                }
                "--interval-ms" => {
                    let interval = value.parse().map_err(|_| "invalid interval".to_owned())?;
                    if !crate::focus::valid_interval(interval) {
                        return Err("interval is outside the supported range".to_owned());
                    }
                    parsed.interval_ms = Some(interval);
                }
                "--tmux-socket" => parsed.tmux_socket = Some(value.clone()),
                "--client-pid" => {
                    parsed.client_pid =
                        Some(value.parse().map_err(|_| "invalid client PID".to_owned())?);
                }
                "--client-tty" => parsed.client_tty = Some(value.clone()),
                "--client-name" if value.is_empty() => {
                    return Err("client name must not be empty".to_owned());
                }
                "--client-name" => parsed.client_name = Some(value.clone()),
                _ => return Err(format!("unknown focus option: {name}")),
            }
            index += 2;
        }
        Ok(parsed)
    }
}

fn context(
    arguments: &[String],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> io::Result<i32> {
    let mut tty = None;
    let mut termname = None;
    let mut control = false;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == "-h" || argument == "--help" {
            stdout.write_all(
                b"usage: termnav tmux context --tty TTY --client-termname TERM [--control-mode 0|1]\n",
            )?;
            return Ok(0);
        }
        let (name, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(name, value)| {
                (name, Some(value))
            });
        let value = match name {
            "--tty" | "--client-termname" | "--control-mode" => {
                if let Some(value) = inline {
                    index += 1;
                    value
                } else {
                    let Some(value) = arguments.get(index + 1) else {
                        return usage(stderr, &format!("{name} requires a value"));
                    };
                    index += 2;
                    value
                }
            }
            _ => return usage(stderr, &format!("unknown context option: {argument}")),
        };
        match name {
            "--tty" => tty = Some(value.to_owned()),
            "--client-termname" => termname = Some(value.to_owned()),
            "--control-mode" => match value {
                "0" => control = false,
                "1" => control = true,
                _ => return usage(stderr, "--control-mode must be 0 or 1"),
            },
            _ => unreachable!(),
        }
    }
    if control {
        return Ok(0);
    }
    let Some(tty) = tty.filter(|value| !value.is_empty()) else {
        return usage(stderr, "--tty is required");
    };
    let Some(termname) = termname.filter(|value| !value.is_empty()) else {
        return usage(stderr, "--client-termname is required");
    };
    let mode = terminal::tmux_mode(&termname);
    let mut file = match terminal::open_writer(Path::new(&tty)) {
        Ok(file) => file,
        Err(error) => {
            writeln!(stderr, "termnav tmux context: {error}")?;
            return Ok(1);
        }
    };
    file.write_all(&terminal::user_var("TERMNAV_TMUX", "true", mode))?;
    Ok(0)
}

fn usage(stderr: &mut dyn Write, message: &str) -> io::Result<i32> {
    writeln!(stderr, "termnav tmux: {message}")?;
    stderr.write_all(HELP.as_bytes())?;
    Ok(2)
}
