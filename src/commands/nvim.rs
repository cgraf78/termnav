//! `termnav nvim` command adapter.

use std::ffi::OsString;
use std::io::{self, Write};

const HELP: &str = "usage: termnav nvim <open|ssh-open> [arguments]\n";

/// Parse and execute Neovim integration commands.
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
        return usage(stderr, "a Neovim command is required");
    };
    match command {
        "-h" | "--help" | "help" => {
            stdout.write_all(HELP.as_bytes())?;
            Ok(0)
        }
        "ssh-open" if arguments.len() == 3 => {
            crate::nvim::remote::ssh_open(&arguments[1], &arguments[2])
        }
        "ssh-open" => usage(stderr, "ssh-open requires HOST and TARGET"),
        "open" => {
            let Some(mode) = arguments.get(1).map(String::as_str) else {
                return usage(stderr, "open requires cli, link, or tmux-link mode");
            };
            let mode = match mode {
                "cli" => crate::nvim::open::Mode::Cli,
                "link" => crate::nvim::open::Mode::Link,
                "tmux-link" => crate::nvim::open::Mode::TmuxLink,
                _ => return usage(stderr, &format!("unknown open mode: {mode}")),
            };
            crate::nvim::open::open(mode, &arguments[2..])
        }
        _ => usage(
            stderr,
            &format!("unknown or unimplemented Neovim command: {command}"),
        ),
    }
}

fn usage(stderr: &mut dyn Write, message: &str) -> io::Result<i32> {
    writeln!(stderr, "termnav nvim: {message}")?;
    stderr.write_all(HELP.as_bytes())?;
    Ok(2)
}
