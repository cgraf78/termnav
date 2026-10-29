//! Terminal escape construction shared by navigation and tmux context hooks.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

/// Compare the actual opened device, rather than trusting a reusable tty name.
pub(crate) fn same_terminal(expected: &Metadata, actual: &Metadata) -> bool {
    expected.file_type().is_char_device()
        && actual.file_type().is_char_device()
        && expected.dev() == actual.dev()
        && expected.ino() == actual.ino()
        && expected.rdev() == actual.rdev()
}

/// Publish one advisory frame without changing a shared/inherited tty's flags.
///
/// A fresh nonblocking descriptor bounds backpressure without affecting SSH or
/// the caller's foreground terminal. A short write is uncertain delivery: do
/// not replay the frame or append its suffix into another writer's output.
pub(crate) fn write_attention(
    path: &Path,
    expected: &Metadata,
    frame: &[u8],
    valid: impl Fn() -> bool,
) -> io::Result<()> {
    let mut output = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    if !same_terminal(expected, &output.metadata()?) {
        return Err(io::Error::other("attention terminal identity changed"));
    }
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        if !valid() {
            return Err(io::Error::other("attention terminal owner changed"));
        }
        if Instant::now() >= deadline {
            return Err(io::ErrorKind::TimedOut.into());
        }
        match output.write(frame) {
            Ok(count) if count == frame.len() => return Ok(()),
            Ok(_) => return Err(io::Error::other("partial attention frame")),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        let mut ready = libc::pollfd {
            fd: output.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
        if millis == 0 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        // SAFETY: one live owned descriptor, with a bounded absolute deadline.
        let result = unsafe { libc::poll(&mut ready, 1, millis) };
        if result == 0 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return Err(io::Error::other("attention terminal disconnected"));
        }
    }
}

/// Open an existing terminal for output without acquiring it as controlling tty.
pub(crate) fn open_writer(path: &Path) -> io::Result<File> {
    // Android's tty policy can reject O_APPEND even when ordinary writes are
    // permitted. A terminal is already a stream; appending has no meaning for
    // it. Never create or truncate a caller-supplied destination.
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)?;
    if file.metadata()?.is_file() {
        // Existing regular-file capture sinks still need atomic append. Set
        // it only on the opened regular descriptor, without reopening a path
        // that could have changed between inspection and use.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_APPEND) } == -1
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(file)
}

/// Framing required to cross an immediate tmux layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TmuxMode {
    /// Write the terminal escape directly.
    Raw,
    /// Wrap the escape in one tmux passthrough frame.
    Passthrough,
}

/// Build one WezTerm `SetUserVar` escape with explicit tmux framing.
#[must_use]
pub fn user_var(name: &str, value: &str, mode: TmuxMode) -> Vec<u8> {
    let raw = format!(
        "\u{1b}]1337;SetUserVar={name}={}\u{7}",
        base64(value.as_bytes())
    );
    match mode {
        TmuxMode::Raw => raw.into_bytes(),
        TmuxMode::Passthrough => {
            // tmux passthrough escapes embedded ESC bytes by doubling them,
            // then wraps the complete sequence in DCS `tmux; ... ST`.
            let escaped = raw.replace('\u{1b}', "\u{1b}\u{1b}");
            format!("\u{1b}Ptmux;{escaped}\u{1b}\\").into_bytes()
        }
    }
}

/// Choose framing from the terminal type that will interpret an escape.
///
/// A `tmux*` or `screen*` terminal type means another tmux layer sits between
/// the destination and WezTerm; `screen` is tmux's historical default
/// `default-terminal`. GNU screen's own passthrough syntax is not supported.
#[must_use]
pub fn tmux_mode(termname: &str) -> TmuxMode {
    if termname.starts_with("tmux") || termname.starts_with("screen") {
        TmuxMode::Passthrough
    } else {
        TmuxMode::Raw
    }
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{TmuxMode, base64, open_writer, tmux_mode, user_var, write_attention};
    use std::ffi::CStr;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::Path;

    #[test]
    fn attention_writer_uses_fresh_flags_and_rejects_partial_frames() {
        let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        assert!(fd >= 0);
        let master = unsafe { OwnedFd::from_raw_fd(fd) };
        assert_eq!(unsafe { libc::grantpt(fd) }, 0);
        assert_eq!(unsafe { libc::unlockpt(fd) }, 0);
        let name = unsafe { CStr::from_ptr(libc::ptsname(fd)) }
            .to_string_lossy()
            .into_owned();
        let original = open_writer(Path::new(&name)).unwrap();
        let flags = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_GETFL) };
        assert_eq!(flags & libc::O_NONBLOCK, 0);
        let identity = original.metadata().unwrap();
        write_attention(Path::new(&name), &identity, b"small frame", || true).unwrap();
        assert_eq!(
            unsafe { libc::fcntl(original.as_raw_fd(), libc::F_GETFL) },
            flags
        );
        let mut bytes = [0_u8; 11];
        File::from(master.try_clone().unwrap())
            .read_exact(&mut bytes)
            .unwrap();
        assert_eq!(&bytes, b"small frame");
        let error =
            write_attention(Path::new(&name), &identity, &[b'x'; 262144], || true).unwrap_err();
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::Other,
            "partial frame declines without replay"
        );
        assert!(write_attention(Path::new(&name), &identity, b"stale", || false).is_err());
        let wrong = std::fs::metadata("/dev/null").unwrap();
        assert!(write_attention(Path::new(&name), &wrong, b"wrong", || true).is_err());
    }

    #[test]
    fn tty_writer_omits_append_and_delivers_bytes_to_a_real_pty() {
        // Android's tty policy can reject O_APPEND even when O_WRONLY is
        // permitted. Inspect the actual descriptor, not just successful output
        // on Linux where both opens happen to work.
        let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        assert!(master >= 0, "open PTY master");
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        assert_eq!(unsafe { libc::grantpt(master.as_raw_fd()) }, 0);
        assert_eq!(unsafe { libc::unlockpt(master.as_raw_fd()) }, 0);
        // ptsname uses process-global storage, so copy it while locked.
        static PTSNAME: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let name = {
            let _guard = PTSNAME.lock().expect("lock PTY name");
            let name = unsafe { libc::ptsname(master.as_raw_fd()) };
            assert!(!name.is_null(), "name PTY slave");
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned()
        };
        let mut writer = open_writer(Path::new(&name)).expect("open PTY writer");
        let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1, "read writer flags");
        assert_eq!(flags & libc::O_APPEND, 0, "tty must not request append");
        assert_eq!(flags & libc::O_ACCMODE, libc::O_WRONLY);
        writer.write_all(b"termnav tty probe").expect("write PTY");
        let mut ready = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut ready, 1, 1000) },
            1,
            "PTY output ready"
        );
        let mut output = [0_u8; 17];
        File::from(master)
            .read_exact(&mut output)
            .expect("read PTY");
        assert_eq!(&output, b"termnav tty probe");
    }

    #[test]
    fn tty_writer_preserves_a_missing_path_as_an_error() {
        let missing = std::env::temp_dir()
            .join(format!(
                "termnav-missing-{}-{}",
                std::process::id(),
                crate::relay::client::new_nonce()
            ))
            .join("tty");
        assert!(!missing.exists());
        let error = open_writer(&missing).expect_err("missing terminal must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!missing.exists(), "opening a terminal must not create it");
    }

    #[test]
    fn tty_writer_keeps_regular_capture_files_in_atomic_append_mode() {
        let path = std::env::temp_dir().join(format!(
            "termnav-capture-{}-{}",
            std::process::id(),
            crate::relay::client::new_nonce()
        ));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());
        let mut original = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .expect("create capture");
        original.write_all(b"previous;").expect("seed capture");
        let mut writer = open_writer(&path).expect("open capture writer");
        let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(
            flags & libc::O_APPEND,
            0,
            "regular captures keep atomic append"
        );
        writer.write_all(b"next").expect("append capture");
        assert_eq!(std::fs::read(path).expect("read capture"), b"previous;next");
    }

    #[test]
    fn tty_writer_rejects_a_directory() {
        let error = open_writer(&std::env::temp_dir()).expect_err("directory must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::IsADirectory);
    }

    #[test]
    fn base64_matches_terminal_protocol_examples() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }

    #[test]
    fn nested_terminal_types_select_passthrough() {
        assert_eq!(tmux_mode("tmux-256color"), TmuxMode::Passthrough);
        assert_eq!(tmux_mode("screen-256color"), TmuxMode::Passthrough);
        assert_eq!(tmux_mode("xterm-256color"), TmuxMode::Raw);
        assert_eq!(tmux_mode("wezterm"), TmuxMode::Raw);
        assert_eq!(tmux_mode(""), TmuxMode::Raw);
    }

    #[test]
    fn tmux_passthrough_doubles_inner_escape_bytes() {
        let sequence = user_var("TERMNAV_TMUX", "true", TmuxMode::Passthrough);

        assert!(sequence.starts_with(b"\x1bPtmux;\x1b\x1b]1337;"));
        assert!(sequence.ends_with(b"\x1b\\"));
    }
}
