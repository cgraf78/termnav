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
    let host = crate::links::remote_host();

    // Remote-link rewriting captures eza's stdout before forwarding it. Force
    // color at that internal pipe boundary so the wrapper does not make a real
    // terminal look non-interactive; the caller's own pipe still receives the
    // same colored stream as the established shell implementation.
    let mut eza_arguments = vec![hyperlink];
    if host.is_some() || io::stdout().is_terminal() {
        eza_arguments.push(OsString::from("--color=always"));
    }
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
    let replacement = host.map_or_else(
        || pattern.to_vec(),
        |host| format!("\x1b]8;;file://{host}/").into_bytes(),
    );
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
    osc8: Osc8,
}

impl<'a> Rewriter<'a> {
    fn new(pattern: &'a [u8], replacement: &'a [u8]) -> Self {
        debug_assert!(!pattern.is_empty());
        Self {
            pattern,
            replacement,
            pending: Vec::with_capacity(16 * 1024 + pattern.len()),
            osc8: Osc8::default(),
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
            self.osc8.push(&self.pending[consumed..index], output)?;
            self.osc8.push(self.replacement, output)?;
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
        self.osc8.push(&self.pending[consumed..ready], output)?;
        if retained > 0 {
            self.pending.copy_within(ready.., 0);
        }
        self.pending.truncate(retained);
        output.flush()?;
        Ok(())
    }

    fn finish(&mut self, output: &mut dyn Write) -> io::Result<()> {
        self.osc8.push(&self.pending, output)?;
        self.osc8.finish(output)?;
        self.pending.clear();
        output.flush()
    }
}

/// Normalize only the URI field of OSC 8 for stable WezTerm's parser.
///
/// Stable WezTerm discards semicolon-bearing links. Encoding that URI byte is
/// equivalent on newer terminals too, while labels, parameters, existing
/// percent escapes, non-UTF8 filenames, and arbitrary chunk boundaries survive.
#[derive(Default)]
struct Osc8 {
    prefix: usize,
    parameters: bool,
    uri: bool,
    escape: bool,
    target: Vec<u8>,
    oversized: bool,
}

impl Osc8 {
    fn finish(&mut self, output: &mut dyn Write) -> io::Result<()> {
        // EOF may interrupt an OSC sequence. Preserve its bytes rather than
        // swallowing a partial URI; oversized targets are deliberately closed.
        if self.oversized {
            output.write_all(b"\x1b\\")?;
        } else {
            output.write_all(&self.target)?;
        }
        self.target.clear();
        Ok(())
    }

    fn push(&mut self, bytes: &[u8], output: &mut dyn Write) -> io::Result<()> {
        const PREFIX: &[u8] = b"\x1b]8;";
        let mut normalized = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if self.uri {
                if byte == 7 || (self.escape && byte == b'\\') {
                    if self.escape && byte == b'\\' {
                        self.target.pop();
                    }
                    if !self.oversized
                        && let Some(target) = eza_uri(&self.target)
                    {
                        normalized.extend_from_slice(&target);
                    }
                    normalized.extend_from_slice(if byte == 7 { b"\x07" } else { b"\x1b\\" });
                    self.target.clear();
                    self.oversized = false;
                    self.uri = false;
                } else if self.target.len() < 64 * 1024 {
                    self.target.push(byte);
                } else {
                    self.oversized = true;
                }
                self.escape = byte == 0x1b;
            } else if self.parameters {
                normalized.push(byte);
                if byte == b';' {
                    self.parameters = false;
                    self.uri = true;
                } else if byte == 7 || (self.escape && byte == b'\\') {
                    self.parameters = false;
                }
                self.escape = byte == 0x1b;
            } else {
                normalized.push(byte);
                if byte == PREFIX[self.prefix] {
                    self.prefix += 1;
                    if self.prefix == PREFIX.len() {
                        self.prefix = 0;
                        self.parameters = true;
                    }
                } else {
                    self.prefix = usize::from(byte == PREFIX[0]);
                }
            }
        }
        output.write_all(&normalized)
    }
}

/// Repair eza's partial URI encoding only when its local path is unambiguous.
///
/// Eza 0.23 escapes spaces but leaves literal percent signs unchanged. Guessing
/// whether "%20" means a space can open a different existing file. Both paths
/// are checked on the producing machine, even when the published URI has a
/// remote authority. Ambiguity disables this explicit link, preserving labels.
fn eza_uri(bytes: &[u8]) -> Option<Vec<u8>> {
    let original = || {
        bytes
            .iter()
            .flat_map(|byte| {
                if *byte == b';' {
                    b"%3B".to_vec()
                } else {
                    vec![*byte]
                }
            })
            .collect()
    };
    let Ok(uri) = std::str::from_utf8(bytes) else {
        return Some(original());
    };
    let Some(rest) = uri.strip_prefix("file://") else {
        return Some(original());
    };
    let Some(slash) = rest.find('/') else {
        return Some(original());
    };
    let raw = &rest[slash..];
    if !raw.contains('%') {
        return Some(original());
    }
    let mut seen = std::collections::HashSet::new();
    let mut existing = None;
    // Mixed literal/encoded escapes need every interpretation, not just "all
    // raw" and "all decoded". Cap expansion before any metadata reads so one
    // elaborate filename cannot cause unbounded work on a listing.
    for candidate in path_candidates(raw)? {
        if seen.insert(candidate.clone()) && std::path::Path::new(&candidate).try_exists().ok()? {
            if existing.is_some() {
                return None;
            }
            existing = Some(candidate);
        }
    }
    let Some(path) = existing else {
        // The entry may have disappeared since eza enumerated it. Without an
        // authoritative path, preserve its existing URI instead of inventing one.
        return Some(original());
    };
    let mut result = format!("file://{}", &rest[..slash]).into_bytes();
    for byte in path.bytes() {
        if byte <= 0x20 || matches!(byte, b'%' | b';' | b'#' | b'?' | 0x7f) {
            result.extend_from_slice(format!("%{byte:02X}").as_bytes());
        } else {
            result.push(byte);
        }
    }
    Some(result)
}

/// Enumerate at most 256 interpretations of eight encoded Unicode characters.
fn path_candidates(mut path: &str) -> Option<Vec<String>> {
    let mut candidates = vec![String::new()];
    let mut escapes = 0;
    while let Some(index) = path.find('%') {
        for candidate in &mut candidates {
            candidate.push_str(&path[..index]);
        }
        path = &path[index..];
        let Some((length, decoded)) = encoded_character(path) else {
            for candidate in &mut candidates {
                candidate.push('%');
            }
            path = &path[1..];
            continue;
        };
        escapes += 1;
        if escapes > 8 {
            return None;
        }
        let mut decoded_candidates = candidates.clone();
        for candidate in &mut candidates {
            candidate.push_str(&path[..length]);
        }
        for candidate in &mut decoded_candidates {
            candidate.push_str(&decoded);
        }
        candidates.extend(decoded_candidates);
        path = &path[length..];
    }
    for candidate in &mut candidates {
        candidate.push_str(path);
    }
    Some(candidates)
}

/// Decode a whole scalar; partial UTF-8 cannot name an eza-hyperlinked path.
fn encoded_character(path: &str) -> Option<(usize, String)> {
    let byte_at = |index: usize| {
        let part = path.get(index..index + 3)?.strip_prefix('%')?;
        u8::from_str_radix(part, 16).ok()
    };
    let first = byte_at(0)?;
    let width = match first {
        0..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let bytes = (0..width)
        .map(|index| byte_at(index * 3))
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes)
        .ok()
        .map(|decoded| (width * 3, decoded))
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

    #[test]
    fn hyperlink_semicolons_are_encoded_only_inside_the_uri() {
        let mut input = Chunks(vec![
            b"plain; text\x1b]8;id=a;b;file:///tmp/".to_vec(),
            b"one;two%3Bthree\x1b".to_vec(),
            b"\\label; unchanged\x1b]8;;\x07".to_vec(),
        ]);
        let mut output = Vec::new();
        rewrite_stream(&mut input, &mut output, b"file:///", b"file://host/").unwrap();
        // OSC 8 parameters end at the first semicolon after the command. Use
        // a conventional id parameter: the semicolon begins the URI field.
        assert_eq!(output, b"plain; text\x1b]8;id=a;b%3Bfile://host/tmp/one%3Btwo%3Bthree\x1b\\label; unchanged\x1b]8;;\x07");
    }

    #[test]
    fn incomplete_osc8_prefix_is_preserved_at_eof() {
        let mut input = Chunks(vec![b"text\x1b]8;".to_vec()]);
        let mut output = Vec::new();
        rewrite_stream(&mut input, &mut output, b"file:///", b"file://host/").unwrap();
        assert_eq!(output, b"text\x1b]8;");
    }

    fn file_uri_case(actual: &str, wire: &str, other: Option<&str>) -> (String, Vec<u8>) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("termnav-eza-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(actual), "fixture").unwrap();
        if let Some(other) = other {
            std::fs::write(root.join(other), "other fixture").unwrap();
        }
        let prefix = format!("file://{}/", root.display());
        let bytes = format!("\x1b]8;;{prefix}{wire}\x1b\\label\x1b]8;;\x1b\\").into_bytes();
        let mut input = Chunks(vec![bytes]);
        let mut output = Vec::new();
        rewrite_stream(&mut input, &mut output, b"file:///", b"file:///").unwrap();
        std::fs::remove_dir_all(root).unwrap();
        (prefix, output)
    }

    #[test]
    fn eza_literal_percent_escape_uses_the_unique_existing_filename() {
        let (prefix, output) = file_uri_case("a%20b", "a%20b", None);
        assert_eq!(
            output,
            format!("\x1b]8;;{prefix}a%2520b\x1b\\label\x1b]8;;\x1b\\").into_bytes()
        );
    }

    #[test]
    fn eza_encoded_space_retains_the_unique_existing_filename() {
        let (prefix, output) = file_uri_case("a b", "a%20b", None);
        assert_eq!(
            output,
            format!("\x1b]8;;{prefix}a%20b\x1b\\label\x1b]8;;\x1b\\").into_bytes()
        );
    }

    #[test]
    fn eza_ambiguous_percent_escape_disables_the_link_without_changing_label() {
        let (_, output) = file_uri_case("a%20b", "a%20b", Some("a b"));
        assert_eq!(output, b"\x1b]8;;\x1b\\label\x1b]8;;\x1b\\");
    }

    #[test]
    fn eza_invalid_percent_hex_is_a_literal_filename() {
        let (prefix, output) = file_uri_case("a%GG", "a%GG", None);
        assert_eq!(
            output,
            format!("\x1b]8;;{prefix}a%25GG\x1b\\label\x1b]8;;\x1b\\").into_bytes()
        );
    }

    #[test]
    fn eza_percent_repair_preserves_unicode() {
        let (prefix, output) = file_uri_case("界%20a", "%E7%95%8C%20a", None);
        assert_eq!(
            output,
            format!("\x1b]8;;{prefix}界%2520a\x1b\\label\x1b]8;;\x1b\\").into_bytes()
        );
    }

    #[test]
    fn eza_mixed_literal_and_encoded_spaces_never_choose_a_sibling() {
        let (_, output) = file_uri_case("a%20b c", "a%20b%20c", Some("a b c"));
        assert_eq!(output, b"\x1b]8;;\x1b\\label\x1b]8;;\x1b\\");
    }

    #[test]
    fn eza_eight_ambiguous_characters_remain_bounded_and_supported() {
        let (prefix, output) = file_uri_case("a        b", "a%20%20%20%20%20%20%20%20b", None);
        assert_eq!(
            output,
            format!("\x1b]8;;{prefix}a%20%20%20%20%20%20%20%20b\x1b\\label\x1b]8;;\x1b\\")
                .into_bytes()
        );
    }

    #[test]
    fn eza_nine_ambiguous_characters_disable_the_link_before_unbounded_work() {
        let (_, output) = file_uri_case("a         b", "a%20%20%20%20%20%20%20%20%20b", None);
        assert_eq!(output, b"\x1b]8;;\x1b\\label\x1b]8;;\x1b\\");
    }
}
