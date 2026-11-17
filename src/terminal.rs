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

/// The longest [`write_attention`] waits, in total, for a full terminal to
/// accept its frame.
///
/// Only waiting counts: the deadline starts after the first owner proof, so a
/// slow proof never uses it up. Lateness never refuses a write, which cannot
/// block, so a write that follows a slow proof still goes ahead. A write that
/// would block once no wait is left gives up with [`io::ErrorKind::TimedOut`].
/// A declined frame therefore costs one proof, this wait, and a proof after
/// each wake.
const ATTENTION_WAIT: Duration = Duration::from_millis(100);

/// Publish one advisory frame without changing a shared/inherited tty's flags.
///
/// A fresh nonblocking descriptor bounds backpressure without affecting SSH or
/// the caller's foreground terminal. A short write is uncertain delivery: do
/// not replay the frame or append its suffix into another writer's output.
/// `valid` proves the terminal's owner before every write attempt, so a frame
/// never reaches a terminal whose owner changed while this waited.
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
    write_proven(&mut output, frame, ATTENTION_WAIT, valid)
}

/// Write `frame` to the nonblocking `output` only while `valid` proves its
/// owner, waiting at most `wait` in total for it to accept output.
fn write_proven(
    output: &mut File,
    frame: &[u8],
    wait: Duration,
    valid: impl Fn() -> bool,
) -> io::Result<()> {
    let mut deadline = None;
    loop {
        // Every attempt proves the owner first, including one after a wake
        // or an interrupted call.
        if !valid() {
            return Err(io::Error::other("attention terminal owner changed"));
        }
        let until = *deadline.get_or_insert_with(|| Instant::now() + wait);
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
        let remaining = until.saturating_duration_since(Instant::now());
        let millis = remaining.as_millis().min(i32::MAX as u128) as i32;
        if millis == 0 {
            return Err(io::ErrorKind::TimedOut.into());
        }
        // SAFETY: one live owned descriptor, with a bounded absolute deadline.
        let result = unsafe { libc::poll(&mut ready, 1, millis) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            return Err(io::ErrorKind::TimedOut.into());
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

pub(crate) fn base64(input: &[u8]) -> String {
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

/// A PTY master opened with `flags`, its slave's name, and that slave, open.
///
/// Keep the slave open while reading the master: macOS discards unread output
/// once the last slave descriptor closes. ptsname returns process-global
/// storage, so every test copies its name under one lock.
#[cfg(test)]
pub(crate) fn test_pty(flags: libc::c_int) -> (std::os::fd::OwnedFd, String, File) {
    use std::os::fd::FromRawFd;

    static PTSNAME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let fd = unsafe { libc::posix_openpt(flags) };
    assert!(fd >= 0, "open PTY master");
    // SAFETY: a successful posix_openpt returns a newly owned descriptor.
    let master = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    assert_eq!(unsafe { libc::grantpt(fd) }, 0, "grant PTY");
    assert_eq!(unsafe { libc::unlockpt(fd) }, 0, "unlock PTY");
    let name = {
        let _guard = PTSNAME
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let name = unsafe { libc::ptsname(fd) };
        assert!(!name.is_null(), "name PTY slave");
        unsafe { std::ffi::CStr::from_ptr(name) }
            .to_str()
            .expect("PTY name is UTF-8")
            .to_owned()
    };
    let slave = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(&name)
        .expect("open PTY slave");
    (master, name, slave)
}

#[cfg(test)]
mod tests {
    use super::{
        ATTENTION_WAIT, TmuxMode, base64, open_writer, test_pty, tmux_mode, user_var,
        write_attention, write_proven,
    };
    use std::cell::Cell;
    use std::fs::File;
    use std::io::{ErrorKind, Read, Write};
    use std::os::fd::AsRawFd;
    use std::path::Path;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    #[test]
    fn attention_writer_uses_fresh_flags_and_rejects_partial_frames() {
        let (master, name, _slave) = test_pty(libc::O_RDWR | libc::O_NOCTTY);
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
            ErrorKind::Other,
            "partial frame declines without replay"
        );
        assert!(write_attention(Path::new(&name), &identity, b"stale", || false).is_err());
        let wrong = std::fs::metadata("/dev/null").unwrap();
        assert!(write_attention(Path::new(&name), &wrong, b"wrong", || true).is_err());
    }

    /// A nonblocking pipe's reader, and its writer as a frame's output.
    ///
    /// Unlike a PTY, nothing drains a pipe behind the test's back: once full,
    /// it stays full until the test reads it.
    fn pipe() -> (File, File) {
        let (reader, writer) = crate::process::pipe(true).expect("nonblocking pipe");
        (File::from(reader), File::from(writer))
    }

    /// Fill `output` until even one more byte would block, so a frame no
    /// longer than PIPE_BUF would block too.
    fn fill(output: &File) {
        let mut size = 4096;
        loop {
            match (&*output).write(&[b'x'; 4096][..size]) {
                Ok(count) => assert!(count > 0, "pipe accepted nothing"),
                Err(error) if error.kind() == ErrorKind::WouldBlock && size > 1 => size = 1,
                Err(error) if error.kind() == ErrorKind::WouldBlock => return,
                Err(error) => panic!("fill the pipe: {error}"),
            }
        }
    }

    /// Everything `reader` holds now.
    fn drain(reader: &File) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            match (&*reader).read(&mut chunk) {
                Ok(0) => return bytes,
                Ok(count) => bytes.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => return bytes,
                Err(error) => panic!("drain the pipe: {error}"),
            }
        }
    }

    /// One frame that met a full pipe, woke when a thread drained it, and
    /// was then proven by `after_wake`.
    struct Woken {
        result: std::io::Result<()>,
        proofs: u32,
        seen: Vec<u8>,
    }

    /// How long the draining thread in [`woken`] waits after the first
    /// proof. It is unrelated to [`ATTENTION_WAIT`]: it only needs to be
    /// long enough that the first write meets the full pipe, and shorter
    /// than each test's `wait`.
    const DRAIN_DELAY: Duration = Duration::from_millis(100);

    /// Write `frame` into a full pipe that a thread drains [`DRAIN_DELAY`]
    /// after the first proof, which runs `first` and succeeds, with
    /// `after_wake` as every later proof.
    ///
    /// That delay is timing: a first write later than it, or a wake later
    /// than `wait`, leaves one proof and nothing to observe. Retry those; a
    /// writer that skips the proof after its wake, or whose wait has run out
    /// before it starts waiting, also leaves one, every time, so the retries
    /// cannot hide it.
    fn woken(
        frame: &[u8],
        wait: Duration,
        first: impl Fn(),
        after_wake: impl Fn() -> bool,
    ) -> Woken {
        for _ in 0..5 {
            let (reader, mut output) = pipe();
            fill(&output);
            let (release, released) = mpsc::channel();
            let draining = {
                let reader = reader.try_clone().unwrap();
                std::thread::spawn(move || {
                    released.recv().unwrap();
                    std::thread::sleep(DRAIN_DELAY);
                    drain(&reader)
                })
            };
            let proofs = Cell::new(0);
            let result = write_proven(&mut output, frame, wait, || {
                proofs.set(proofs.get() + 1);
                if proofs.get() == 1 {
                    first();
                    release.send(()).unwrap();
                    return true;
                }
                after_wake()
            });
            let mut seen = draining.join().unwrap();
            seen.extend(drain(&reader));
            if proofs.get() > 1 {
                return Woken {
                    result,
                    proofs: proofs.get(),
                    seen,
                };
            }
        }
        panic!("the frame never waited for the pipe and woke after the first proof");
    }

    #[test]
    fn a_slow_first_proof_does_not_use_up_the_wait() {
        let (reader, mut output) = pipe();
        write_proven(&mut output, b"slow proof", ATTENTION_WAIT, || {
            std::thread::sleep(ATTENTION_WAIT * 3 / 2);
            true
        })
        .unwrap();
        assert_eq!(drain(&reader), b"slow proof");
    }

    #[test]
    fn a_slow_first_proof_does_not_use_up_the_wait_for_a_full_pipe() {
        // The first proof outlasts the wait; the wait starts after it, so the
        // frame still waits for the drain and goes out.
        let wait = Duration::from_secs(1);
        let woken = woken(b"FRAME", wait, || std::thread::sleep(wait * 3 / 2), || true);
        woken.result.unwrap();
        assert_eq!(
            woken.proofs, 2,
            "proved before the write and after the wake"
        );
        assert!(woken.seen.windows(5).any(|window| window == b"FRAME"));
    }

    #[test]
    fn a_failed_first_proof_writes_nothing() {
        let (reader, mut output) = pipe();
        let proofs = Cell::new(0);
        let error = write_proven(&mut output, b"not yours", ATTENTION_WAIT, || {
            proofs.set(proofs.get() + 1);
            false
        })
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Other, "{error}");
        assert_eq!(proofs.get(), 1);
        assert!(drain(&reader).is_empty());
    }

    #[test]
    fn an_owner_change_while_waiting_writes_nothing() {
        let woken = woken(b"FRAME", Duration::from_secs(10), || {}, || false);
        let error = woken.result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Other, "{error}");
        assert_eq!(
            woken.proofs, 2,
            "proved before the write and after the wake"
        );
        assert!(!woken.seen.windows(5).any(|window| window == b"FRAME"));
    }

    #[test]
    fn a_slow_proof_after_a_wake_still_writes() {
        // The wait is over by the time this proof returns; the write cannot
        // block, so it still goes ahead.
        let wait = Duration::from_secs(1);
        let woken = woken(
            b"FRAME",
            wait,
            || {},
            || {
                std::thread::sleep(wait * 3 / 2);
                true
            },
        );
        woken.result.unwrap();
        assert_eq!(woken.proofs, 2);
        assert!(woken.seen.windows(5).any(|window| window == b"FRAME"));
    }

    #[test]
    fn a_pipe_that_stays_full_times_out() {
        let (reader, mut output) = pipe();
        fill(&output);
        let proofs = Cell::new(0);
        let started = Instant::now();
        let error = write_proven(&mut output, b"FRAME", ATTENTION_WAIT, || {
            proofs.set(proofs.get() + 1);
            true
        })
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut, "{error}");
        // It waited, rather than giving up at once; poll rounds down to whole
        // milliseconds, so the wait can end a fraction early.
        assert!(
            started.elapsed() >= ATTENTION_WAIT / 2,
            "{:?}",
            started.elapsed()
        );
        assert_eq!(proofs.get(), 1, "a timeout does not prove the owner again");
        assert!(!drain(&reader).contains(&b'F'));
    }

    #[test]
    fn tty_writer_omits_append_and_delivers_bytes_to_a_real_pty() {
        // Android's tty policy can reject O_APPEND even when O_WRONLY is
        // permitted. Inspect the actual descriptor, not just successful output
        // on Linux where both opens happen to work.
        let (master, name, _slave) = test_pty(libc::O_RDWR | libc::O_NOCTTY);
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
