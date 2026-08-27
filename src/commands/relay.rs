//! `termnav relay` command adapter.

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::fd::RawFd;
use std::path::Path;

use crate::navigation::{Action, Direction};
use crate::relay::server;

const HELP: &str = "usage: termnav relay <send|serve|commit|sweep> [arguments]\n\
\n\
send: termnav relay send ACTION DIRECTION [options]\n\
      ACTION is pane-select, tab-select, or tab-move\n";

/// Parse and execute relay transport and commit operations.
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
        return usage(stderr, "a relay command is required");
    };
    match command {
        "-h" | "--help" | "help" => {
            stdout.write_all(HELP.as_bytes())?;
            Ok(0)
        }
        "send" => send_command(&arguments[1..], stderr),
        "serve" => serve_command(&arguments[1..], stderr),
        "commit" => commit_command(&arguments[1..], stderr),
        "sweep" if arguments.len() == 1 => match server::sweep() {
            Ok(()) => Ok(0),
            Err(error) => {
                writeln!(stderr, "termnav relay sweep: {error}")?;
                Ok(1)
            }
        },
        "sweep" => usage(stderr, "sweep accepts no arguments"),
        _ => usage(stderr, &format!("unknown relay command: {command}")),
    }
}

fn send_command(arguments: &[String], stderr: &mut dyn Write) -> io::Result<i32> {
    let mut positional = Vec::new();
    let mut client_pid = None;
    let mut client_tty = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--client-pid" => {
                let Some(value) = option(arguments, &mut index) else {
                    return usage(stderr, "--client-pid requires a value");
                };
                let Ok(value) = value.parse::<u32>() else {
                    return usage(stderr, "--client-pid requires a positive integer");
                };
                client_pid = Some(value);
            }
            "--client-tty" => {
                let Some(value) = option(arguments, &mut index) else {
                    return usage(stderr, "--client-tty requires a value");
                };
                client_tty = Some(value.to_owned());
            }
            value if value.starts_with('-') => {
                return usage(stderr, &format!("unknown send option: {value}"));
            }
            value => {
                positional.push(value.to_owned());
                index += 1;
            }
        }
    }
    if positional.len() != 2 {
        return usage(stderr, "send requires ACTION and DIRECTION");
    }
    if client_pid.is_some() != client_tty.is_some() {
        return usage(
            stderr,
            "--client-pid and --client-tty must be provided together",
        );
    }
    let action = match Action::parse(&positional[0]) {
        Ok(action) => action,
        Err(message) => return usage(stderr, &message),
    };
    let direction = match Direction::parse(action, &positional[1]) {
        Ok(direction) => direction,
        Err(message) => return usage(stderr, &message),
    };
    Ok(server::send_navigation(
        action,
        direction,
        client_pid,
        client_tty.as_deref(),
        None,
    ))
}

fn serve_command(arguments: &[String], stderr: &mut dyn Write) -> io::Result<i32> {
    let mut socket = None;
    let mut owner_fd: Option<RawFd> = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--socket" => {
                let Some(value) = option(arguments, &mut index) else {
                    return usage(stderr, "--socket requires a value");
                };
                socket = Some(value.to_owned());
            }
            "--owner-fd" => {
                let Some(value) = option(arguments, &mut index) else {
                    return usage(stderr, "--owner-fd requires a value");
                };
                let Ok(value) = value.parse() else {
                    return usage(stderr, "--owner-fd requires an integer");
                };
                owner_fd = Some(value);
            }
            value => return usage(stderr, &format!("unknown serve option: {value}")),
        }
    }
    let Some(socket) = socket else {
        return usage(stderr, "serve requires --socket");
    };
    match server::serve(Path::new(&socket), owner_fd, None) {
        Ok(()) => Ok(0),
        Err(error) => {
            writeln!(stderr, "termnav relay serve: {error}")?;
            Ok(1)
        }
    }
}

fn commit_command(arguments: &[String], stderr: &mut dyn Write) -> io::Result<i32> {
    let mut tmux_socket = None;
    let mut client_tty = None;
    let mut client_pid = None;
    let mut client_created = None;
    let mut passthrough_state = None;
    let mut pane = None;
    let mut index = 0;
    while index < arguments.len() {
        let name = arguments[index].as_str();
        let value = match name {
            "--tmux-socket"
            | "--client-tty"
            | "--client-pid"
            | "--client-created"
            | "--passthrough-decrqm"
            | "--pane" => {
                let Some(value) = option(arguments, &mut index) else {
                    return usage(stderr, &format!("{name} requires a value"));
                };
                value
            }
            _ => return usage(stderr, &format!("unknown commit option: {name}")),
        };
        match name {
            "--tmux-socket" => tmux_socket = Some(value.to_owned()),
            "--client-tty" => client_tty = Some(value.to_owned()),
            "--client-pid" => {
                let Ok(value) = value.parse() else {
                    return usage(stderr, "--client-pid requires an integer");
                };
                client_pid = Some(value);
            }
            "--client-created" => {
                let Ok(value) = value.parse() else {
                    return usage(stderr, "--client-created requires an integer");
                };
                client_created = Some(value);
            }
            "--passthrough-decrqm" => {
                let Ok(value) = value.parse() else {
                    return usage(stderr, "--passthrough-decrqm requires an integer");
                };
                passthrough_state = Some(value);
            }
            "--pane" => pane = Some(value.to_owned()),
            _ => unreachable!(),
        }
    }
    let (Some(tmux_socket), Some(client_tty), Some(client_pid), Some(client_created)) =
        (tmux_socket, client_tty, client_pid, client_created)
    else {
        return usage(
            stderr,
            "commit requires tmux socket and exact client identity",
        );
    };
    Ok(server::commit(
        &tmux_socket,
        &client_tty,
        client_pid,
        client_created,
        passthrough_state,
        pane.as_deref(),
    ))
}

fn option<'a>(arguments: &'a [String], index: &mut usize) -> Option<&'a str> {
    let value = arguments.get(*index + 1)?;
    *index += 2;
    Some(value)
}

fn usage(stderr: &mut dyn Write, message: &str) -> io::Result<i32> {
    writeln!(stderr, "termnav relay: {message}")?;
    stderr.write_all(HELP.as_bytes())?;
    Ok(2)
}
