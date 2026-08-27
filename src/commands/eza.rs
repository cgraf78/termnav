//! `termnav eza` hyperlink adapter.

use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Write};
use std::process::{Command, Stdio};

use crate::process;

/// Run eza with file hyperlinks labeled for the current remote host.
pub fn run(
    arguments: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> io::Result<i32> {
    let binary = std::env::var_os("TERMNAV_EZA_BINARY").unwrap_or_else(|| OsString::from("eza"));
    let hyperlink = hyperlink_argument(&binary);
    let Some(host) = crate::links::remote_host() else {
        return Command::new(binary)
            .arg(hyperlink)
            .args(arguments)
            .status()
            .map(process::status_code);
    };

    let mut eza_arguments = vec![hyperlink];
    if io::stdout().is_terminal()
        || std::env::var("TERMNAV_EZA_NVIM_LINKS_FORCE_TTY").is_ok_and(|value| value == "1")
    {
        eza_arguments.push(OsString::from("--color=always"));
        if let Some(width) = terminal_width() {
            eza_arguments.push(OsString::from("--width"));
            eza_arguments.push(OsString::from(width.to_string()));
        }
        if !has_layout_argument(arguments) {
            eza_arguments.push(OsString::from("--grid"));
        }
    }
    eza_arguments.extend_from_slice(arguments);
    let output = Command::new(binary)
        .args(eza_arguments)
        .env("TERMNAV_REMOTE_LINK_HOST", &host)
        .stdin(Stdio::inherit())
        .output()?;

    // eza emits only one hostless file URI prefix. Rewriting bytes avoids a
    // UTF-8 assumption about filenames and preserves every ANSI byte exactly.
    let pattern = b"\x1b]8;;file:///";
    let replacement = format!("\x1b]8;;file://{host}/").into_bytes();
    stdout.write_all(&replace_all(&output.stdout, pattern, &replacement))?;
    stderr.write_all(&output.stderr)?;
    Ok(process::status_code(output.status))
}

fn hyperlink_argument(binary: &OsStr) -> OsString {
    let mut command = Command::new(binary);
    command.args(["--hyperlink=always", "--version"]);
    if process::output_timeout(&mut command, std::time::Duration::from_secs(2))
        .is_ok_and(|output| output.status.success())
    {
        OsString::from("--hyperlink=always")
    } else {
        OsString::from("--hyperlink")
    }
}

fn terminal_width() -> Option<u32> {
    if let Ok(width) = std::env::var("COLUMNS")
        && let Ok(width) = width.parse::<u32>()
        && width > 0
    {
        return Some(width);
    }
    let mut command = Command::new("tput");
    command.arg("cols");
    let output = process::output_timeout(&mut command, std::time::Duration::from_secs(2)).ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

fn has_layout_argument(arguments: &[OsString]) -> bool {
    for argument in arguments {
        let value = argument.to_string_lossy();
        match value.as_ref() {
            "-1" | "--oneline" | "-l" | "--long" | "-G" | "--grid" | "-T" | "--tree" => {
                return true;
            }
            "--" => return false,
            value
                if value.starts_with("--oneline=")
                    || value.starts_with("--long=")
                    || value.starts_with("--grid=")
                    || value.starts_with("--tree=") =>
            {
                return true;
            }
            value
                if value.starts_with('-')
                    && !value.starts_with("--")
                    && value[1..].bytes().any(|byte| b"1lGT".contains(&byte)) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn replace_all(input: &[u8], pattern: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut offset = 0;
    while let Some(relative) = input[offset..]
        .windows(pattern.len())
        .position(|window| window == pattern)
    {
        let index = offset + relative;
        output.extend_from_slice(&input[offset..index]);
        output.extend_from_slice(replacement);
        offset = index + pattern.len();
    }
    output.extend_from_slice(&input[offset..]);
    output
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{has_layout_argument, replace_all};

    #[test]
    fn combined_short_layout_flags_are_detected() {
        assert!(has_layout_argument(&[OsString::from("-al")]));
        assert!(!has_layout_argument(&[
            OsString::from("-a"),
            OsString::from("--")
        ]));
    }

    #[test]
    fn hyperlink_rewrite_is_binary_safe() {
        assert_eq!(
            replace_all(
                b"a\x1b]8;;file:///tmp/x\xff",
                b"\x1b]8;;file:///",
                b"\x1b]8;;file://host/"
            ),
            b"a\x1b]8;;file://host/tmp/x\xff"
        );
    }
}
