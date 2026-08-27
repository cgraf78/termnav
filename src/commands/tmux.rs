//! `termnav tmux` command adapter.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;

use crate::terminal::{self, TmuxMode};

const HELP: &str = "usage: termnav tmux <context|focus|follow-click> [arguments]\n";

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
        "focus" => focus(&arguments[1..], stdout, stderr),
        _ => usage(
            stderr,
            &format!("unknown or unimplemented tmux command: {command}"),
        ),
    }
}

fn focus(arguments: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> io::Result<i32> {
    let Some(command) = arguments.first().map(String::as_str) else {
        return usage(stderr, "focus requires a command");
    };
    if matches!(command, "-h" | "--help" | "help") {
        stdout.write_all(
            b"usage: termnav tmux focus <claim|release|expire|watch|stop|sync> [options]\n",
        )?;
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
        "watch" => {
            let (Some(socket), Some(pid), Some(tty)) =
                (options.tmux_socket, options.client_pid, options.client_tty)
            else {
                return usage(stderr, "focus watch requires exact client options");
            };
            Ok(crate::focus::watch(
                &socket,
                pid,
                &tty,
                options.lease_ms.unwrap_or(3500),
                options.interval_ms.unwrap_or(1000),
            ))
        }
        "stop" => {
            let (Some(socket), Some(pid), Some(tty)) =
                (options.tmux_socket, options.client_pid, options.client_tty)
            else {
                return usage(stderr, "focus stop requires exact client options");
            };
            Ok(crate::focus::stop_watch(&socket, pid, &tty))
        }
        "sync" => {
            let (Some(socket), Some(pid), Some(tty)) =
                (options.tmux_socket, options.client_pid, options.client_tty)
            else {
                return usage(stderr, "focus sync requires exact client options");
            };
            Ok(if crate::focus::sync_client_style(&socket, pid, &tty) {
                0
            } else {
                1
            })
        }
        _ => usage(stderr, &format!("unknown focus command: {command}")),
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
    let mode = if termname.starts_with("tmux") || termname.starts_with("screen") {
        TmuxMode::Passthrough
    } else {
        TmuxMode::Raw
    };
    let mut file = match OpenOptions::new()
        .append(true)
        .custom_flags(libc::O_NOCTTY)
        .open(tty)
    {
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
