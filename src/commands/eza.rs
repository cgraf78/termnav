//! `termnav eza` hyperlink adapter.

use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
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

    // Remote-link rewriting captures eza's stdout before forwarding it. Force
    // color at that internal pipe boundary so the wrapper does not make a real
    // terminal look non-interactive; the caller's own pipe still receives the
    // same colored stream as the established shell implementation.
    let mut eza_arguments = vec![hyperlink, OsString::from("--color=always")];
    if io::stdout().is_terminal()
        || std::env::var("TERMNAV_EZA_NVIM_LINKS_FORCE_TTY").is_ok_and(|value| value == "1")
    {
        if let Some(width) = terminal_width() {
            eza_arguments.push(OsString::from("--width"));
            eza_arguments.push(OsString::from(width.to_string()));
        }
        if !has_layout_argument(arguments) {
            eza_arguments.push(OsString::from("--grid"));
        }
    }
    eza_arguments.extend_from_slice(arguments);
    let mut command = Command::new(binary);
    command
        .args(eza_arguments)
        .env("TERMNAV_REMOTE_LINK_HOST", &host)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process::spawn_owned_group(&mut command)?;
    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("eza stdout pipe is unavailable"))?;
    let mut child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("eza stderr pipe is unavailable"))?;

    // Poll both pipes on this thread. This keeps stderr streaming without
    // imposing Send on the caller's writer and, unlike a scoped reader thread,
    // gives a downstream write failure one place to kill the complete eza
    // process group before any pipe drain can wait on a surviving descendant.
    let pattern = b"\x1b]8;;file:///";
    let replacement = format!("\x1b]8;;file://{host}/").into_bytes();
    if let Err(error) = stream_output(
        &mut child_stdout,
        &mut child_stderr,
        stdout,
        stderr,
        pattern,
        &replacement,
    ) {
        process::kill_owned_group(&mut child);
        let _ = child.wait();
        return Err(error);
    }
    match child.wait() {
        Ok(status) => Ok(process::status_code(status)),
        Err(error) => {
            process::kill_owned_group(&mut child);
            let _ = child.wait();
            Err(error)
        }
    }
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

#[cfg(test)]
fn rewrite_stream(
    input: &mut dyn Read,
    output: &mut dyn Write,
    pattern: &[u8],
    replacement: &[u8],
) -> io::Result<()> {
    let mut rewriter = Rewriter::new(pattern, replacement);
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            return rewriter.finish(output);
        }
        rewriter.push(&buffer[..count], output)?;
    }
}

struct Rewriter<'a> {
    pattern: &'a [u8],
    replacement: &'a [u8],
    pending: Vec<u8>,
}

impl<'a> Rewriter<'a> {
    fn new(pattern: &'a [u8], replacement: &'a [u8]) -> Self {
        debug_assert!(!pattern.is_empty());
        Self {
            pattern,
            replacement,
            pending: Vec::with_capacity(16 * 1024 + pattern.len()),
        }
    }

    fn push(&mut self, bytes: &[u8], output: &mut dyn Write) -> io::Result<()> {
        self.pending.extend_from_slice(bytes);

        let mut consumed = 0;
        while let Some(relative) = self.pending[consumed..]
            .windows(self.pattern.len())
            .position(|window| window == self.pattern)
        {
            let index = consumed + relative;
            output.write_all(&self.pending[consumed..index])?;
            output.write_all(self.replacement)?;
            consumed = index + self.pattern.len();
        }

        // Bytes that cannot begin a future match are safe to publish now. Keep
        // only the longest suffix that is also a prefix of the search pattern.
        let unmatched = &self.pending[consumed..];
        let retained = (1..self.pattern.len())
            .rev()
            .find(|length| unmatched.ends_with(&self.pattern[..*length]))
            .unwrap_or(0);
        let ready = self.pending.len().saturating_sub(retained);
        output.write_all(&self.pending[consumed..ready])?;
        if retained > 0 {
            self.pending.copy_within(ready.., 0);
        }
        self.pending.truncate(retained);
        output.flush()?;
        Ok(())
    }

    fn finish(&mut self, output: &mut dyn Write) -> io::Result<()> {
        output.write_all(&self.pending)?;
        self.pending.clear();
        output.flush()
    }
}

fn stream_output(
    stdout: &mut (impl Read + AsRawFd),
    stderr: &mut (impl Read + AsRawFd),
    stdout_writer: &mut dyn Write,
    stderr_writer: &mut dyn Write,
    pattern: &[u8],
    replacement: &[u8],
) -> io::Result<()> {
    let mut rewriter = Rewriter::new(pattern, replacement);
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut stdout_buffer = [0_u8; 16 * 1024];
    let mut stderr_buffer = [0_u8; 16 * 1024];
    while stdout_open || stderr_open {
        let mut descriptors = [
            libc::pollfd {
                fd: if stdout_open { stdout.as_raw_fd() } else { -1 },
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            },
            libc::pollfd {
                fd: if stderr_open { stderr.as_raw_fd() } else { -1 },
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }

        if stdout_open && descriptors[0].revents != 0 {
            if descriptors[0].revents & libc::POLLNVAL != 0 {
                return Err(io::Error::other("eza stdout pipe became invalid"));
            }
            let count = stdout.read(&mut stdout_buffer)?;
            if count == 0 {
                stdout_open = false;
                rewriter.finish(stdout_writer)?;
            } else {
                rewriter.push(&stdout_buffer[..count], stdout_writer)?;
            }
        }
        if stderr_open && descriptors[1].revents != 0 {
            if descriptors[1].revents & libc::POLLNVAL != 0 {
                return Err(io::Error::other("eza stderr pipe became invalid"));
            }
            let count = stderr.read(&mut stderr_buffer)?;
            if count == 0 {
                stderr_open = false;
                stderr_writer.flush()?;
            } else {
                stderr_writer.write_all(&stderr_buffer[..count])?;
                stderr_writer.flush()?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use std::io::{self, Read};

    use super::{has_layout_argument, rewrite_stream};

    struct Chunks(Vec<Vec<u8>>);

    impl Read for Chunks {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if self.0.is_empty() {
                return Ok(0);
            }
            let chunk = self.0.remove(0);
            output[..chunk.len()].copy_from_slice(&chunk);
            Ok(chunk.len())
        }
    }

    #[test]
    fn combined_short_layout_flags_are_detected() {
        assert!(has_layout_argument(&[OsString::from("-al")]));
        assert!(!has_layout_argument(&[
            OsString::from("-a"),
            OsString::from("--")
        ]));
    }

    #[test]
    fn hyperlink_rewrite_is_binary_safe_across_input_chunks() {
        let mut input = Chunks(vec![b"a\x1b]8;;fi".to_vec(), b"le:///tmp/x\xff".to_vec()]);
        let mut output = Vec::new();
        rewrite_stream(
            &mut input,
            &mut output,
            b"\x1b]8;;file:///",
            b"\x1b]8;;file://host/",
        )
        .unwrap();
        assert_eq!(output, b"a\x1b]8;;file://host/tmp/x\xff");
    }
}
