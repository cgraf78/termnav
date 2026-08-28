//! `termnav eza` hyperlink adapter.

use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Read, Write};
use std::process::{Command, Stdio};
use std::thread;

use crate::process;

/// Run eza with file hyperlinks labeled for the current remote host.
pub fn run(
    arguments: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut (dyn Write + Send),
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
    let mut child = Command::new(binary)
        .args(eza_arguments)
        .env("TERMNAV_REMOTE_LINK_HOST", &host)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("eza stdout pipe is unavailable"))?;
    let mut child_stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("eza stderr pipe is unavailable"))?;

    // stdout must remain interactive even though Termnav has to inspect it.
    // Drain stderr directly on a scoped worker so diagnostics stream without an
    // unbounded capture buffer and cannot fill their pipe while this thread
    // incrementally rewrites stdout. The scope proves both borrows end before
    // `run` returns, without weakening the CLI's ordinary writer abstraction.
    thread::scope(|scope| {
        let stderr_reader = scope.spawn(|| copy_stream(&mut child_stderr, stderr));

        // eza emits only one hostless file URI prefix. Rewriting bytes avoids a
        // UTF-8 assumption about filenames and preserves every ANSI byte
        // exactly. The matcher retains only a possible partial prefix between
        // reads, so a large directory begins rendering before eza exits without
        // missing a prefix split across two kernel pipe reads.
        let pattern = b"\x1b]8;;file:///";
        let replacement = format!("\x1b]8;;file://{host}/").into_bytes();
        let rewrite_result = rewrite_stream(&mut child_stdout, stdout, pattern, &replacement);
        if rewrite_result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait();
        let stderr_result = stderr_reader
            .join()
            .map_err(|_| io::Error::other("eza stderr reader panicked"))?;
        rewrite_result?;
        stderr_result?;
        Ok(process::status_code(status?))
    })
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

fn rewrite_stream(
    input: &mut dyn Read,
    output: &mut dyn Write,
    pattern: &[u8],
    replacement: &[u8],
) -> io::Result<()> {
    debug_assert!(!pattern.is_empty());
    let mut pending = Vec::with_capacity(16 * 1024 + pattern.len());
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            output.write_all(&pending)?;
            output.flush()?;
            return Ok(());
        }
        pending.extend_from_slice(&buffer[..count]);

        let mut consumed = 0;
        while let Some(relative) = pending[consumed..]
            .windows(pattern.len())
            .position(|window| window == pattern)
        {
            let index = consumed + relative;
            output.write_all(&pending[consumed..index])?;
            output.write_all(replacement)?;
            consumed = index + pattern.len();
        }

        // Bytes that cannot begin a future match are safe to publish now. Keep
        // only the longest suffix that is also a prefix of the search pattern.
        let unmatched = &pending[consumed..];
        let retained = (1..pattern.len())
            .rev()
            .find(|length| unmatched.ends_with(&pattern[..*length]))
            .unwrap_or(0);
        let ready = pending.len().saturating_sub(retained);
        output.write_all(&pending[consumed..ready])?;
        if retained > 0 {
            pending.copy_within(ready.., 0);
        }
        pending.truncate(retained);
        output.flush()?;
    }
}

fn copy_stream(input: &mut dyn Read, output: &mut dyn Write) -> io::Result<()> {
    let mut buffer = [0_u8; 16 * 1024];
    let mut first_error = None;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            if first_error.is_none()
                && let Err(error) = output.flush()
            {
                first_error = Some(error);
            }
            return first_error.map_or(Ok(()), Err);
        }
        if first_error.is_none() {
            if let Err(error) = output.write_all(&buffer[..count]) {
                first_error = Some(error);
            } else if let Err(error) = output.flush() {
                first_error = Some(error);
            }
        }
        // If the destination fails, continue draining the child pipe. Returning
        // early here could block eza forever before the main thread can reap it.
    }
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
