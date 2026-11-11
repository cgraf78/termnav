//! Popup side: accept one typed image transfer and paste its saved path.
//!
//! The receiver runs in a short-lived tmux popup opened by the innermost tmux
//! layer for one pane and one client. Its input is that client's keystrokes,
//! which WezTerm writes only after the receiver's `ready` announcement reached
//! it through the exact client's outward route. Everything typed into the
//! popup is untrusted: the parser rejects the first unexpected byte, and on any
//! failure the receiver keeps discarding input until the terminator so bytes
//! still in flight never reach the pane once the popup closes.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::protocol::{Fed, Parser, Rejection};
use super::{ACK_INTERVAL, MAX_IMAGE_BYTES, USER_VAR};
use crate::browser::{self, Destination};

/// Give up after this long without receiving a byte.
pub const IDLE_LIMIT: Duration = Duration::from_secs(30);

/// Saved images older than this are removed by later receivers.
pub const KEEP_IMAGES: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Quiet period that ends discarding after input the user typed before any
/// transfer started. WezTerm streams continuously once it has begun, so a
/// short silence proves nothing is in flight.
const ABORT_QUIET: Duration = Duration::from_secs(1);

/// tmux commands are bounded like other Termnav probes.
const TMUX_TIMEOUT: Duration = Duration::from_secs(2);

/// Validated `receive` arguments.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Socket of the tmux server running the popup; empty selects `$TMUX`.
    pub socket: String,
    /// Pane that receives the saved image path.
    pub target_pane: String,
    /// tmux client name, for messages.
    pub client_name: String,
    /// Exact client process, when known.
    pub client_pid: Option<u32>,
    /// Exact client tty, when known; resolved from the name otherwise.
    pub client_tty: Option<String>,
}

/// Why a transfer did not produce an image, and how to drain its input.
#[derive(Debug)]
struct Failure {
    reason: String,
    drain: Drain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Drain {
    /// The terminator was already consumed, or input ended.
    None,
    /// Discard until the terminator or this long without input.
    Until(Duration),
}

impl Failure {
    fn new(reason: impl Into<String>, drain: Drain) -> Self {
        Self {
            reason: reason.into(),
            drain,
        }
    }
}

/// Run one receiver and return its exit status.
///
/// `progress` is the popup's display. Status 0 means the image was saved and
/// its path pasted; 1 means nothing was pasted and a message was shown.
pub fn run(options: &Options, progress: &mut dyn Write) -> i32 {
    let socket = if options.socket.is_empty() {
        match std::env::var("TMUX") {
            Ok(tmux) if !tmux.is_empty() => socket_of(&tmux).to_owned(),
            _ => {
                let _ = writeln!(progress, "termnav paste-image: the tmux server is unknown");
                return 1;
            }
        }
    } else {
        options.socket.clone()
    };
    let tmux = Tmux {
        socket: &socket,
        client_name: &options.client_name,
        pane: &options.target_pane,
    };
    let destination = match resolve(&socket, options) {
        Ok(destination) => destination,
        Err(reason) => {
            tmux.tell(&format!("Image paste unavailable: {reason}"));
            return 1;
        }
    };
    let _raw = RawMode::enable();
    let id = super::random_hex(8);
    if destination.send(USER_VAR, &format!("ready:{id}")).is_err() {
        tmux.tell("Image paste unavailable: cannot reach the terminal");
        return 1;
    }
    let _ = write!(progress, "\r\x1b[KWaiting for the image... (Esc cancels)");
    let _ = progress.flush();

    let outcome = receive(&id, &destination, progress).and_then(|image| {
        save(&image)
            .map_err(|error| Failure::new(format!("cannot save the image: {error}"), Drain::None))
    });
    match outcome {
        Ok(path) => {
            let _ = destination.send(USER_VAR, &format!("done:{id}"));
            if let Err(error) = tmux.paste(&path) {
                tmux.tell(&format!(
                    "Saved {} but could not paste it: {error}",
                    path.display()
                ));
                return 1;
            }
            0
        }
        Err(failure) => {
            let _ = destination.send(USER_VAR, &format!("cancel:{id}"));
            let _ = write!(progress, "\r\x1b[KImage paste failed: {}", failure.reason);
            let _ = progress.flush();
            if let Drain::Until(quiet) = failure.drain {
                drain(quiet);
            }
            tmux.tell(&format!("Image paste failed: {}", failure.reason));
            1
        }
    }
}

/// The socket path in a `$TMUX` value (`socket,server-pid,session-id`).
fn socket_of(tmux: &str) -> &str {
    tmux.rsplitn(3, ',').last().unwrap_or(tmux)
}

fn resolve(socket: &str, options: &Options) -> Result<Destination, String> {
    let (tty, pid) = match &options.client_tty {
        Some(tty) => (tty.clone(), options.client_pid),
        None => match crate::focus::named_client(socket, &options.client_name) {
            Some(client) if !client.control_mode => (client.tty, Some(client.pid)),
            Some(_) => return Err("the tmux client is a control client".to_owned()),
            None => return Err("the tmux client is not attached".to_owned()),
        },
    };
    browser::exact_client_destination(socket, &tty, pid).map_err(|error| error.to_string())
}

/// Read one transfer from standard input.
fn receive(
    id: &str,
    destination: &Destination,
    progress: &mut dyn Write,
) -> Result<Vec<u8>, Failure> {
    let mut parser = Parser::new(id);
    let mut acknowledged: Option<u64> = None;
    let mut header_length = 0;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = match read_input(&mut buffer, IDLE_LIMIT) {
            Input::Data(count) => count,
            Input::Timeout if parser.started() => {
                return Err(Failure::new("the transfer stalled", Drain::None));
            }
            Input::Timeout => {
                return Err(Failure::new("the terminal did not respond", Drain::None));
            }
            Input::Closed => return Err(Failure::new("input closed", Drain::None)),
        };
        let before = parser.consumed();
        let rejection = match parser.feed(&buffer[..count]) {
            Ok(Fed::Partial) => None,
            Ok(Fed::Complete(_)) => {
                return parser
                    .finish()
                    .map_err(|rejection| Failure::new(rejection.to_string(), Drain::None));
            }
            Err(rejection) => Some(rejection),
        };
        if let Some(rejection) = rejection {
            // The parser stops at the offending byte without counting it.
            let index = (parser.consumed() - before) as usize;
            let rest = &buffer[index.min(count)..count];
            let reason = match rejection {
                // Anything typed before the transfer began is the user's own
                // keystroke, most likely Esc or Ctrl-C to dismiss the popup.
                Rejection::InvalidByte { offset: 0, .. } => "cancelled".to_owned(),
                Rejection::InvalidByte { byte: b'!', .. } => {
                    "the transfer was truncated".to_owned()
                }
                rejection => rejection.to_string(),
            };
            let drain = if rest.contains(&b'!') {
                // The terminator already arrived; nothing is left in flight.
                Drain::None
            } else if before == 0 && index == 0 {
                Drain::Until(ABORT_QUIET)
            } else {
                Drain::Until(IDLE_LIMIT)
            };
            return Err(Failure::new(reason, drain));
        }
        if !parser.header_complete() {
            continue;
        }
        if header_length == 0 {
            header_length = parser.consumed() - parser_data_seen(&parser);
        }
        let consumed = parser.consumed();
        // The first acknowledgement follows the header and opens WezTerm's
        // full window; later ones are spaced so each value is distinct.
        if acknowledged.is_none_or(|last| consumed - last >= ACK_INTERVAL) {
            if destination
                .send(USER_VAR, &format!("ack:{id}:{consumed}"))
                .is_err()
            {
                return Err(Failure::new(
                    "cannot reach the terminal",
                    Drain::Until(IDLE_LIMIT),
                ));
            }
            acknowledged = Some(consumed);
            let total = header_length + parser.encoded_length() as u64 + 1;
            let _ = write!(
                progress,
                "\r\x1b[KReceiving image: {}% of {:.1} MiB",
                consumed * 100 / total.max(1),
                parser.declared_length() as f64 / (1024.0 * 1024.0)
            );
            let _ = progress.flush();
        }
    }
}

/// Base64 characters accepted so far.
fn parser_data_seen(parser: &Parser) -> u64 {
    (parser.encoded_length() - parser.remaining_encoded()) as u64
}

/// Discard input until the terminator or `quiet` without input.
fn drain(quiet: Duration) {
    // A transfer is at most the encoded cap plus its header; a stream that
    // keeps going past that without a terminator is not WezTerm's.
    let mut budget = MAX_IMAGE_BYTES.div_ceil(3) * 4 + 1024 * 1024;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        match read_input(&mut buffer, quiet) {
            Input::Data(count) => {
                if buffer[..count].contains(&b'!') || count >= budget {
                    return;
                }
                budget -= count;
            }
            Input::Timeout | Input::Closed => return,
        }
    }
}

enum Input {
    Data(usize),
    Timeout,
    Closed,
}

/// Read from standard input, waiting at most `limit` for the first byte.
fn read_input(buffer: &mut [u8], limit: Duration) -> Input {
    let deadline = Instant::now() + limit;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
        if millis == 0 {
            return Input::Timeout;
        }
        let mut ready = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one stack pollfd for the process's own standard input.
        let result = unsafe { libc::poll(&mut ready, 1, millis) };
        if result == 0 {
            return Input::Timeout;
        }
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Input::Closed;
        }
        // Read directly: a buffered reader could hold bytes poll cannot see.
        // SAFETY: `buffer` is valid for writes of its full length.
        let count =
            unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
        match count {
            0 => return Input::Closed,
            count if count > 0 => return Input::Data(count as usize),
            _ => match io::Error::last_os_error().kind() {
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => {}
                _ => return Input::Closed,
            },
        }
    }
}

/// Raw terminal input for the popup, restored on drop.
///
/// Canonical mode would echo the transfer, cap lines at 4 KiB, and turn
/// Ctrl-C into a signal. Pending input is discarded first: keys typed before
/// `ready` cannot belong to the transfer.
struct RawMode(Option<libc::termios>);

impl RawMode {
    fn enable() -> Self {
        // SAFETY: termios calls on the process's own standard input with
        // stack-owned structures.
        unsafe {
            if libc::isatty(libc::STDIN_FILENO) != 1 {
                return Self(None);
            }
            let mut original = std::mem::zeroed::<libc::termios>();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
                return Self(None);
            }
            let mut raw = original;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
                return Self(None);
            }
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
            Self(Some(original))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(original) = self.0 {
            // SAFETY: restores the attributes read in `enable`.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original);
            }
        }
    }
}

/// tmux operations for the popup's server, client, and target pane.
struct Tmux<'a> {
    socket: &'a str,
    client_name: &'a str,
    pane: &'a str,
}

impl Tmux<'_> {
    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        // An inherited TMUX value can retarget commands on older tmux despite
        // the explicit socket.
        command
            .args(["-S", self.socket])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        command
    }

    fn succeeds(mut command: Command) -> bool {
        crate::process::output_timeout(&mut command, TMUX_TIMEOUT)
            .is_ok_and(|output| output.status.success())
    }

    /// Bracket-paste `path` into the target pane through a private buffer.
    fn paste(&self, path: &Path) -> io::Result<()> {
        let buffer = format!("termnav-paste-image-{}", super::random_hex(6));
        let mut command = self.command();
        command
            .args(["set-buffer", "-b", &buffer, "--"])
            .arg(path)
            .args([
                ";",
                "paste-buffer",
                "-p",
                "-d",
                "-b",
                &buffer,
                "-t",
                self.pane,
            ]);
        let output = crate::process::output_timeout(&mut command, TMUX_TIMEOUT)?;
        if output.status.success() {
            return Ok(());
        }
        // Do not leave the path in a named buffer when the paste failed.
        let mut cleanup = self.command();
        cleanup.args(["delete-buffer", "-b", &buffer]);
        let _ = crate::process::output_timeout(&mut cleanup, TMUX_TIMEOUT);
        Err(io::Error::other(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }

    /// Show a short message on the client, falling back to the pane's
    /// session where tmux 3.2 rejects `display-message -c`.
    fn tell(&self, message: &str) {
        // display-message expands formats and strftime sequences.
        let escaped = format!("termnav: {message}")
            .replace('#', "##")
            .replace('%', "%%");
        let attempts: [&[&str]; 3] = [
            &["-c", self.client_name, "-d", "5000"],
            &["-c", self.client_name],
            &["-t", self.pane],
        ];
        for arguments in attempts {
            if arguments.contains(&"") {
                continue;
            }
            let mut command = self.command();
            command.arg("display-message").args(arguments).arg(&escaped);
            if Self::succeeds(command) {
                return;
            }
        }
    }
}

/// Directory that holds received images, created owner-only.
///
/// `$XDG_CACHE_HOME/termnav/paste-images`, or `$HOME/.cache/...` when the XDG
/// value is unset or relative. Both Termnav directories must be real
/// directories owned by this user; they are tightened to 0700.
///
/// # Errors
///
/// Returns an error when no absolute base exists or a directory is a
/// symlink, not a directory, or owned by another user.
pub fn image_directory() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".cache"))
        })
        .ok_or_else(|| io::Error::other("no absolute cache directory"))?;
    fs::create_dir_all(&base)?;
    let termnav = base.join("termnav");
    crate::runtime::ensure_private_directory(&termnav)?;
    let images = termnav.join("paste-images");
    crate::runtime::ensure_private_directory(&images)?;
    Ok(images)
}

/// Save one validated image and return its absolute path.
fn save(image: &[u8]) -> io::Result<PathBuf> {
    let directory = image_directory()?;
    let now = SystemTime::now();
    prune(&directory, now);
    write_image(&directory, image, now)
}

/// Write `image` atomically as `clip-<UTC timestamp>-<random>.png`.
pub(crate) fn write_image(directory: &Path, image: &[u8], now: SystemTime) -> io::Result<PathBuf> {
    let name = format!("clip-{}-{}.png", utc_timestamp(now), super::random_hex(6));
    let temporary = directory.join(format!(".{name}.tmp"));
    let result = (|| {
        let mut file = super::stage::create_private(&temporary)?;
        file.write_all(image)?;
        file.sync_all()?;
        let path = directory.join(&name);
        fs::rename(&temporary, &path)?;
        Ok(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Remove this receiver's images and leftovers older than [`KEEP_IMAGES`].
pub(crate) fn prune(directory: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let uid = unsafe { libc::getuid() };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let owned = (name.starts_with("clip-") && name.ends_with(".png"))
            || (name.starts_with(".clip-") && name.ends_with(".tmp"));
        if !owned {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !metadata.file_type().is_file() || metadata.uid() != uid {
            continue;
        }
        let expired = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > KEEP_IMAGES);
        if expired {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Format `time` as a compact UTC timestamp such as `20261111T093005Z`.
pub(crate) fn utc_timestamp(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let days = (seconds / 86_400) as i64;
    let remainder = seconds % 86_400;
    // Howard Hinnant's days-to-civil conversion for the proleptic Gregorian
    // calendar.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        remainder / 3600,
        remainder / 60 % 60,
        remainder % 60
    )
}

#[cfg(test)]
mod tests {
    use super::{KEEP_IMAGES, prune, socket_of, utc_timestamp, write_image};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn scratch(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "termnav-receive-{label}-{}-{}",
            std::process::id(),
            crate::paste_image::random_hex(4)
        ));
        fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn timestamps_are_utc_calendar_dates() {
        assert_eq!(utc_timestamp(UNIX_EPOCH), "19700101T000000Z");
        assert_eq!(
            utc_timestamp(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "20000229T000000Z"
        );
        assert_eq!(
            utc_timestamp(UNIX_EPOCH + Duration::from_secs(1_794_384_305)),
            "20261111T080505Z"
        );
    }

    #[test]
    fn socket_comes_from_the_tmux_value() {
        assert_eq!(socket_of("/tmp/a,b/sock,123,4"), "/tmp/a,b/sock");
        assert_eq!(socket_of("/tmp/sock"), "/tmp/sock");
    }

    #[test]
    fn images_are_written_owner_only_without_leftovers() {
        let root = scratch("write");
        let path = write_image(&root, b"\x89PNG\r\n\x1a\nbody", SystemTime::now()).unwrap();
        assert!(path.is_absolute());
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("clip-") && name.ends_with(".png"),
            "{name}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"\x89PNG\r\n\x1a\nbody");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1, "no temporary file");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prune_removes_only_expired_receiver_files() {
        let root = scratch("prune");
        let now = SystemTime::now() + KEEP_IMAGES + Duration::from_secs(60);
        let expired = root.join("clip-20200101T000000Z-aaaaaaaaaaaa.png");
        let leftover = root.join(".clip-20200101T000000Z-bbbbbbbbbbbb.png.tmp");
        let recent = root.join("clip-20990101T000000Z-cccccccccccc.png");
        let foreign = root.join("screenshot.png");
        for path in [&expired, &leftover, &recent, &foreign] {
            fs::write(path, b"x").unwrap();
        }
        fs::File::options()
            .write(true)
            .open(&recent)
            .unwrap()
            .set_modified(now - Duration::from_secs(60))
            .unwrap();
        let link = root.join("clip-link.png");
        std::os::unix::fs::symlink(&foreign, &link).unwrap();
        prune(&root, now);
        assert!(!expired.exists());
        assert!(!leftover.exists());
        assert!(recent.exists());
        assert!(foreign.exists());
        assert!(fs::symlink_metadata(&link).is_ok());
        fs::remove_dir_all(&root).unwrap();
    }
}
