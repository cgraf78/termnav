//! Desktop side: read a clipboard PNG and stage it for WezTerm to send.
//!
//! Staging runs only after the user pressed Ctrl-V, from a scheduled WezTerm
//! callback. It never decides anything for text: a clipboard that also holds
//! text declines so WezTerm's normal text paste remains the whole behavior.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use super::MAX_IMAGE_BYTES;
use super::protocol;

/// Hard limit for the clipboard reader subprocesses of one `stage` run.
///
/// It bounds external programs that can wedge (a clipboard owner that never
/// answers), not Termnav's own validation, hashing, and encoding, which are
/// linear in the capped image size. WezTerm runs staging asynchronously, so
/// the limit only delays image pastes, never text.
pub const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Environment variable naming a replacement clipboard reader.
///
/// The command is run as `COMMAND types` (print offered MIME types, one per
/// line) and `COMMAND png` (print the PNG). Intended for tests; it also lets
/// an unsupported desktop supply its own reader.
pub const ADAPTER_ENV: &str = "TERMNAV_PASTE_IMAGE_CLIPBOARD";

/// Staged files older than this are removed by later `stage` runs. WezTerm
/// normally deletes its file within seconds; the margin covers a transfer
/// that is still reading an older file.
const STALE_AFTER: Duration = Duration::from_secs(10 * 60);

/// Name prefix of the files this command owns in its runtime directory.
const STAGE_PREFIX: &str = "stage-";

/// Output of one staging attempt.
#[derive(Debug)]
pub enum Staged {
    /// The image was staged at this path.
    Path(PathBuf),
    /// The clipboard holds no PNG-convertible image, or also holds text.
    Declined(&'static str),
}

/// Stage the desktop clipboard's image, if it holds only an image.
///
/// # Errors
///
/// Returns an error when the clipboard reader fails or times out, the data is
/// not a PNG within [`MAX_IMAGE_BYTES`], or the staged file cannot be written.
pub fn stage() -> io::Result<Staged> {
    let deadline = Instant::now() + READ_TIMEOUT;
    let directory = crate::runtime::private_subdirectory(&[OsStr::new("paste-image")])?;
    prune(&directory, SystemTime::now());
    let png = match read_clipboard(&directory, deadline)? {
        Clipboard::Image(png) => png,
        Clipboard::Text => return Ok(Staged::Declined("the clipboard holds text")),
        Clipboard::Empty => return Ok(Staged::Declined("the clipboard holds no PNG image")),
    };
    protocol::validate_png(&png).map_err(io::Error::other)?;
    let path = directory.join(format!("{STAGE_PREFIX}{}.b64", super::random_hex(12)));
    let result = create_private(&path).and_then(|file| {
        let mut output = io::BufWriter::with_capacity(256 * 1024, file);
        protocol::write_staged(&png, &mut output)?;
        output.into_inner().map_err(|error| error.into_error())?;
        Ok(())
    });
    if let Err(error) = result {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok(Staged::Path(path))
}

/// Create a new owner-only file, refusing to follow or replace anything.
pub(crate) fn create_private(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// Remove this command's staged files older than [`STALE_AFTER`].
fn prune(directory: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let uid = unsafe { libc::getuid() };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(STAGE_PREFIX) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !metadata.file_type().is_file() || metadata.uid() != uid {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > STALE_AFTER);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// What the clipboard offers.
#[derive(Debug, Eq, PartialEq)]
enum Clipboard {
    Image(Vec<u8>),
    Text,
    Empty,
}

/// MIME types or X selection targets a text paste would use.
fn is_text_type(name: &str) -> bool {
    let name = name.trim();
    name.split(';')
        .next()
        .is_some_and(|base| base.trim() == "text/plain")
        || matches!(name, "UTF8_STRING" | "STRING" | "TEXT" | "COMPOUND_TEXT")
}

/// Decide from an offered type list whether a PNG read is worthwhile.
fn classify(types: &str) -> Option<Clipboard> {
    if types.lines().any(is_text_type) {
        return Some(Clipboard::Text);
    }
    if !types.lines().any(|line| line.trim() == "image/png") {
        return Some(Clipboard::Empty);
    }
    None
}

fn read_clipboard(directory: &Path, deadline: Instant) -> io::Result<Clipboard> {
    if let Some(adapter) = std::env::var_os(ADAPTER_ENV).filter(|value| !value.is_empty()) {
        return read_with(directory, deadline, |argument| {
            let mut command = Command::new(&adapter);
            command.arg(argument);
            command
        });
    }
    native(directory, deadline)
}

#[cfg(target_os = "macos")]
fn native(directory: &Path, deadline: Instant) -> io::Result<Clipboard> {
    // One osascript run decides and reads: each launch costs ~100 ms. Text
    // classes win as on other platforms; PNG and TIFF screenshots both
    // coerce to PNG. The script writes into a file this process created
    // owner-only, so the image never passes through a world-readable path.
    const SCRIPT: &str = r#"on run argv
  set hasImage to false
  repeat with entry in (clipboard info)
    set kind to item 1 of entry
    if kind is in {string, Unicode text, «class utf8», «class ut16»} then return "text"
    if kind is in {«class @PNG@», TIFF picture} then set hasImage to true
  end repeat
  if not hasImage then return "none"
  set target to open for access (POSIX file (item 1 of argv)) with write permission
  try
    set eof target to 0
    write (the clipboard as «class @PNG@») to target
    close access target
  on error
    close access target
    return "none"
  end try
  return "png"
end run"#;
    let path = directory.join(format!("{STAGE_PREFIX}read-{}.png", super::random_hex(12)));
    drop(create_private(&path)?);
    let result = (|| {
        let mut command = Command::new("osascript");
        // The pasteboard's PNG class code is spliced in so spell checkers do
        // not read the four-character code as a misspelled word.
        let script = SCRIPT.replace("@PNG@", concat!("PNG", "f"));
        command.arg("-e").arg(script).arg(&path);
        let output = run(&mut command, deadline, 64)?;
        if !output.status.success() {
            // An empty pasteboard is a successful "none"; failure here means
            // the script itself could not run.
            return Err(io::Error::other(format!(
                "osascript failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        match String::from_utf8_lossy(&output.stdout).trim() {
            "text" => Ok(Clipboard::Text),
            "png" => read_capped(&path).map(Clipboard::Image),
            _ => Ok(Clipboard::Empty),
        }
    })();
    let _ = fs::remove_file(&path);
    result
}

#[cfg(not(target_os = "macos"))]
fn native(directory: &Path, deadline: Instant) -> io::Result<Clipboard> {
    let set = |name| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    if set("WAYLAND_DISPLAY") {
        let wayland = read_with(directory, deadline, |argument| {
            let mut command = Command::new("wl-paste");
            match argument {
                "types" => command.arg("--list-types"),
                _ => command.args(["--no-newline", "--type", "image/png"]),
            };
            command
        });
        match wayland {
            Err(error) if error.kind() == io::ErrorKind::NotFound && set("DISPLAY") => {}
            result => return result,
        }
    }
    if set("DISPLAY") {
        return read_with(directory, deadline, |argument| {
            let mut command = Command::new("xclip");
            command.args(["-selection", "clipboard", "-o", "-t"]);
            command.arg(match argument {
                "types" => "TARGETS",
                _ => "image/png",
            });
            command
        });
    }
    Ok(Clipboard::Empty)
}

/// Run the two-step `types` then `png` protocol shared by every reader.
fn read_with(
    directory: &Path,
    deadline: Instant,
    command: impl Fn(&str) -> Command,
) -> io::Result<Clipboard> {
    let output = run(&mut command("types"), deadline, 64 * 1024)?;
    if !output.status.success() {
        // wl-paste and xclip fail on an empty or ownerless clipboard.
        return Ok(Clipboard::Empty);
    }
    if let Some(decided) = classify(&String::from_utf8_lossy(&output.stdout)) {
        return Ok(decided);
    }
    let path = directory.join(format!("{STAGE_PREFIX}read-{}.png", super::random_hex(12)));
    let result = read_png_into(&mut command("png"), &path, deadline);
    let _ = fs::remove_file(&path);
    result
}

/// Run a PNG reader with its stdout redirected to a new owner-only file.
///
/// The kernel writes the image straight into the file, so the transfer is
/// not paced by a polling pipe drain, and a file-size resource limit stops a
/// runaway reader one byte past the cap.
fn read_png_into(command: &mut Command, path: &Path, deadline: Instant) -> io::Result<Clipboard> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "clipboard read timed out",
        ));
    }
    let file = create_private(path)?;
    let limit = (MAX_IMAGE_BYTES + 1) as libc::rlim_t;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::null());
    // SAFETY: the hook only calls async-signal-safe setrlimit between fork
    // and exec and allocates nothing.
    unsafe {
        command.pre_exec(move || {
            let bound = libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            };
            if libc::setrlimit(libc::RLIMIT_FSIZE, &bound) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let status = crate::process::status_timeout(command, remaining)?;
    let size = fs::symlink_metadata(path)?.len();
    if size > MAX_IMAGE_BYTES as u64 {
        return Err(io::Error::other("image is larger than 16 MiB"));
    }
    if !status.success() || size == 0 {
        return Ok(Clipboard::Empty);
    }
    read_capped(path).map(Clipboard::Image)
}

fn run(
    command: &mut Command,
    deadline: Instant,
    max_bytes: usize,
) -> io::Result<std::process::Output> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "clipboard read timed out",
        ));
    }
    // stderr shares the byte cap; readers print little there.
    crate::process::output_timeout_limited(command, remaining, max_bytes.saturating_add(4096))
}

fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(io::Error::other("image is larger than 16 MiB"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::{Clipboard, STALE_AFTER, classify, is_text_type, prune, read_capped};
    use std::fs;
    use std::time::{Duration, SystemTime};

    #[test]
    fn text_types_decline_even_beside_an_image() {
        assert_eq!(classify("image/png\ntext/plain\n"), Some(Clipboard::Text));
        assert_eq!(
            classify("TARGETS\nimage/png\nUTF8_STRING\n"),
            Some(Clipboard::Text)
        );
        assert_eq!(
            classify("text/plain;charset=utf-8\n"),
            Some(Clipboard::Text)
        );
        assert_eq!(classify("image/png\ntext/html\n"), None);
        assert_eq!(classify("image/png\n"), None);
        assert_eq!(classify("image/jpeg\n"), Some(Clipboard::Empty));
        assert_eq!(classify(""), Some(Clipboard::Empty));
        assert!(is_text_type(" STRING "));
        assert!(!is_text_type("text/html"));
        assert!(!is_text_type("text/plainish"));
    }

    #[test]
    fn prune_removes_only_stale_owned_stage_files() {
        let root = std::env::temp_dir().join(format!(
            "termnav-stage-prune-{}-{}",
            std::process::id(),
            crate::paste_image::random_hex(4)
        ));
        fs::create_dir(&root).unwrap();
        let old = root.join("stage-old.b64");
        let fresh = root.join("stage-fresh.b64");
        let other = root.join("unrelated-old");
        for path in [&old, &fresh, &other] {
            fs::write(path, b"x").unwrap();
        }
        let link = root.join("stage-link");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        let now = SystemTime::now() + STALE_AFTER + Duration::from_secs(5);
        let fresh_file = fs::File::options().write(true).open(&fresh).unwrap();
        fresh_file
            .set_modified(now - Duration::from_secs(1))
            .unwrap();
        prune(&root, now);
        assert!(!old.exists());
        assert!(fresh.exists());
        assert!(other.exists());
        assert!(fs::symlink_metadata(&link).is_ok(), "symlinks are skipped");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn capped_reads_refuse_oversized_files_and_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "termnav-stage-cap-{}-{}",
            std::process::id(),
            crate::paste_image::random_hex(4)
        ));
        fs::create_dir(&root).unwrap();
        let big = root.join("big");
        let file = fs::File::create(&big).unwrap();
        file.set_len(crate::paste_image::MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        assert!(read_capped(&big).is_err());
        let link = root.join("link");
        std::os::unix::fs::symlink(&big, &link).unwrap();
        assert!(read_capped(&link).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
