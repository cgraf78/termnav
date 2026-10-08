//! `termnav open-url` command adapter.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;

use crate::browser::{self, Url};

const HELP: &str = "usage: termnav open-url [--tty PATH] URL\n";

/// Validate one URL and ask the outer terminal to open it locally.
pub fn run(
    arguments: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> io::Result<i32> {
    if matches!(
        arguments.first().and_then(|value| value.to_str()),
        Some("-h" | "--help" | "help")
    ) && arguments.len() == 1
    {
        stdout.write_all(HELP.as_bytes())?;
        return Ok(0);
    }
    let (terminal, value) = match arguments {
        // URLs never start with `-`, so a lone dash argument is an option.
        [value] if !value.to_str().is_some_and(|value| value.starts_with('-')) => (None, value),
        [option, terminal, value] if option == "--tty" => {
            if terminal.is_empty() {
                return usage(stderr, "--tty requires a path");
            }
            (Some(Path::new(terminal)), value)
        }
        [option, ..] if option.to_str().is_some_and(|value| value.starts_with('-')) => {
            return usage(
                stderr,
                &format!(
                    "unknown option or missing URL: {}",
                    option.to_string_lossy()
                ),
            );
        }
        _ => return usage(stderr, "exactly one URL is required"),
    };
    let Some(value) = value.to_str() else {
        return usage(stderr, "URL must be valid UTF-8");
    };
    let url = match Url::parse(value) {
        Ok(url) => url,
        Err(message) => return usage(stderr, &message),
    };
    match browser::request(&url, terminal) {
        Ok(()) => Ok(0),
        Err(error) => {
            writeln!(stderr, "termnav open-url: {error}")?;
            Ok(1)
        }
    }
}

fn usage(stderr: &mut dyn Write, message: &str) -> io::Result<i32> {
    writeln!(stderr, "termnav open-url: {message}")?;
    stderr.write_all(HELP.as_bytes())?;
    Ok(2)
}
