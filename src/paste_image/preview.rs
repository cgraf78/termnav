//! Inline preview of a saved clipboard image.
//!
//! The zsh widget runs this after a paste with stdout on the terminal. It must
//! never fail loudly, so every rejection or error prints nothing and exits 0.
//! No terminal is ever queried: the size comes from `TIOCGWINSZ` on stdout,
//! then tmux, then fixed defaults.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use super::receive::{image_directory, is_clip_name};
use super::{MAX_IMAGE_BYTES, PREVIEW_ROWS_SETTING};
use crate::image;

/// Rows used when neither `--rows` nor the environment picks a height.
const DEFAULT_ROWS: u16 = 12;

/// Bound on the tmux size query; the preview runs inside an input widget.
const TMUX_TIMEOUT: Duration = Duration::from_millis(500);

/// Pane geometry used when neither the tty nor tmux reports one.
const FALLBACK: Pane = Pane {
    cols: 80,
    rows: 24,
    cell: (10, 20),
};

/// How the thumbnail is drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// DEC sixel graphics.
    Sixel,
    /// Truecolor half-block text cells.
    Blocks,
}

impl Mode {
    /// Parse `sixel` or `blocks`.
    pub fn parse(value: &str) -> Option<Mode> {
        match value {
            "sixel" => Some(Mode::Sixel),
            "blocks" => Some(Mode::Blocks),
            _ => None,
        }
    }

    /// The setting and pane-option spelling of this mode.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Sixel => "sixel",
            Mode::Blocks => "blocks",
        }
    }
}

/// Pane size in text cells plus the pixel size of one cell.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pane {
    cols: u16,
    rows: u16,
    cell: (u16, u16),
}

impl Pane {
    /// Build a pane, rejecting any zero (terminals report 0 when unknown).
    fn new(cols: u16, rows: u16, cell: (u16, u16)) -> Option<Pane> {
        (cols > 0 && rows > 0 && cell.0 > 0 && cell.1 > 0).then_some(Pane { cols, rows, cell })
    }
}

/// Whether `metadata` describes a regular file owned by `uid`.
pub(crate) fn owned_regular(metadata: &Metadata, uid: u32) -> bool {
    metadata.file_type().is_file() && metadata.uid() == uid
}

fn rejected() -> io::Error {
    io::Error::from(io::ErrorKind::PermissionDenied)
}

/// Open `path` only when it is a clip this user saved in `directory`.
///
/// The path text can come from an editable shell buffer, so it is untrusted:
/// the file must be named like a clip and sit directly in the image directory
/// (compared after canonicalizing both parents). It is opened `O_NOFOLLOW`
/// so a planted symlink is refused, and `O_NONBLOCK` so a FIFO cannot hang
/// the open; the descriptor is then checked rather than the path, which
/// leaves no window between check and use.
///
/// # Errors
///
/// Returns an error for any file that fails those checks, is owned by
/// another user, or exceeds [`MAX_IMAGE_BYTES`].
pub fn open_clip(path: &Path, directory: &Path) -> io::Result<File> {
    let name = path.file_name().and_then(|name| name.to_str());
    if !name.is_some_and(is_clip_name) {
        return Err(rejected());
    }
    let parent = path.parent().ok_or_else(rejected)?.canonicalize()?;
    if parent != directory.canonicalize()? {
        return Err(rejected());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    let uid = unsafe { libc::getuid() };
    if !owned_regular(&metadata, uid) || metadata.len() > MAX_IMAGE_BYTES as u64 {
        return Err(rejected());
    }
    Ok(file)
}

/// Size of the terminal on stdout, or `None` when it is not a sized tty.
fn tty_size() -> Option<Pane> {
    // SAFETY: `winsize` is plain old data, and the ioctl only fills it.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } != 0 {
        return None;
    }
    let width = size.ws_xpixel.checked_div(size.ws_col)?;
    let height = size.ws_ypixel.checked_div(size.ws_row)?;
    Pane::new(size.ws_col, size.ws_row, (width, height))
}

/// Parse `#{pane_width} #{pane_height} #{window_cell_width} #{window_cell_height}`.
fn parse_tmux(text: &str) -> Option<Pane> {
    let mut fields = text.split_whitespace().map(|field| field.parse::<u16>());
    let mut next = || fields.next()?.ok();
    let pane = Pane::new(next()?, next()?, (next()?, next()?));
    // Extra fields mean the format changed under us; do not guess.
    pane.filter(|_| fields.next().is_none())
}

/// Ask tmux for the size of `$TMUX_PANE`, for when stdout is not the tty
/// (the widget redirects it, and tmux may not forward pixel sizes).
fn tmux_size() -> Option<Pane> {
    let pane = std::env::var("TMUX_PANE")
        .ok()
        .filter(|pane| !pane.is_empty())?;
    let mut command = Command::new("tmux");
    command.args([
        "display",
        "-p",
        "-t",
        &pane,
        "#{pane_width} #{pane_height} #{window_cell_width} #{window_cell_height}",
    ]);
    let output = crate::process::output_timeout(&mut command, TMUX_TIMEOUT).ok()?;
    if !output.status.success() {
        return None;
    }
    parse_tmux(std::str::from_utf8(&output.stdout).ok()?)
}

/// Preview height in rows for a pane `pane_rows` tall.
///
/// The preview never takes more than half the pane or all but two rows (one
/// for the prompt and one of context), and is at least one row. Panes under
/// three rows get none.
fn rows_for(requested: u16, pane_rows: u16) -> Option<u16> {
    if pane_rows < 3 {
        return None;
    }
    Some(requested.min(pane_rows / 2).min(pane_rows - 2).max(1))
}

/// The environment's row request; unparsable or zero values are ignored.
fn environment_rows() -> Option<u16> {
    std::env::var(PREVIEW_ROWS_SETTING)
        .ok()?
        .parse::<u16>()
        .ok()
        .filter(|rows| *rows > 0)
}

/// Decode and draw the clip, or `None` on any rejection.
fn render(path: &Path, mode: Mode, rows: Option<u16>) -> Option<String> {
    let file = open_clip(path, &image_directory().ok()?).ok()?;
    let pane = tty_size().or_else(tmux_size).unwrap_or(FALLBACK);
    let rows = rows_for(
        rows.or_else(environment_rows).unwrap_or(DEFAULT_ROWS),
        pane.rows,
    )?;
    // A half block is one pixel wide and two tall, so blocks mode fits in
    // those units; sixel needs the terminal's real cell size.
    let cell = match mode {
        Mode::Blocks => (1, 2),
        Mode::Sixel => pane.cell,
    };
    // The target must always come from `fit` so memory stays proportional
    // to the thumbnail, never to the source image.
    let thumbnail = image::decode(file.take(MAX_IMAGE_BYTES as u64), |width, height| {
        image::fit((width, height), (pane.cols, rows), cell)
    })
    .ok()?;
    let mut text = match mode {
        Mode::Blocks => image::blocks(&thumbnail),
        Mode::Sixel => image::sixel(&thumbnail),
    };
    if text.is_empty() {
        return None;
    }
    // The block renderer already ends every line with a newline.
    if !text.ends_with('\n') {
        text.push('\n');
    }
    Some(text)
}

/// Render the clip at `path` to `out`.
///
/// `rows` overrides `TERMNAV_PASTE_IMAGE_PREVIEW_ROWS`, which overrides the
/// default of 12. Always returns 0 and prints nothing on any failure, so a
/// broken preview can never disturb the paste that triggered it.
pub fn run(path: &Path, mode: Mode, rows: Option<u16>, out: &mut dyn Write) -> i32 {
    if let Some(text) = render(path, mode, rows) {
        // One write, so a rejected clip leaves no partial output; a closed
        // terminal is not worth reporting.
        let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{Mode, open_clip, owned_regular, parse_tmux, rows_for};
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn scratch(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("termnav-preview-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("clips")).unwrap();
        root
    }

    #[test]
    fn rows_are_capped_by_the_pane() {
        assert_eq!(rows_for(12, 24), Some(12));
        assert_eq!(rows_for(12, 20), Some(10));
        assert_eq!(rows_for(12, 4), Some(2));
        assert_eq!(rows_for(12, 3), Some(1));
        assert_eq!(rows_for(1, 24), Some(1));
        assert_eq!(rows_for(12, 2), None);
        assert_eq!(rows_for(12, 0), None);
    }

    #[test]
    fn tmux_sizes_parse_strictly() {
        let pane = parse_tmux("120 40 9 18\n").unwrap();
        assert_eq!((pane.cols, pane.rows, pane.cell), (120, 40, (9, 18)));
        for invalid in [
            "",
            "120 40 9",
            "120 40 0 18",
            "120 40 9 18 1",
            "a b c d",
            "0 0 0 0",
        ] {
            assert_eq!(parse_tmux(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn mode_round_trips() {
        assert_eq!(Mode::parse("sixel"), Some(Mode::Sixel));
        assert_eq!(Mode::parse("blocks"), Some(Mode::Blocks));
        assert_eq!(Mode::parse("auto"), None);
        assert_eq!(Mode::Sixel.as_str(), "sixel");
        assert_eq!(Mode::Blocks.as_str(), "blocks");
    }

    #[test]
    fn rejects_outside_directory() {
        let root = scratch("outside");
        let file = root.join("clip-a.png");
        fs::write(&file, b"x").unwrap();
        assert!(open_clip(&file, &root.join("clips")).is_err());
    }

    #[test]
    fn rejects_symlink() {
        let root = scratch("symlink");
        let target = root.join("clips/clip-real.png");
        fs::write(&target, b"x").unwrap();
        let link = root.join("clips/clip-link.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_clip(&link, &root.join("clips")).is_err());
    }

    #[test]
    fn rejects_fifo_without_blocking() {
        let root = scratch("fifo");
        let fifo = root.join("clips/clip-fifo.png");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        assert!(open_clip(&fifo, &root.join("clips")).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn rejects_wrong_name() {
        let root = scratch("name");
        for name in ["clip-x.txt", ".clip-a.tmp", "other.png"] {
            let file = root.join("clips").join(name);
            fs::write(&file, b"x").unwrap();
            assert!(open_clip(&file, &root.join("clips")).is_err(), "{name}");
        }
    }

    #[test]
    fn rejects_subdirectory_file() {
        let root = scratch("subdir");
        let nested = root.join("clips/nested");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("clip-a.png");
        fs::write(&file, b"x").unwrap();
        assert!(open_clip(&file, &root.join("clips")).is_err());
    }

    #[test]
    fn rejects_directory_named_like_a_clip() {
        let root = scratch("dirclip");
        let directory = root.join("clips/clip-dir.png");
        fs::create_dir(&directory).unwrap();
        assert!(open_clip(&directory, &root.join("clips")).is_err());
    }

    #[test]
    fn accepts_owned_clip() {
        let root = scratch("ok");
        let file = root.join("clips/clip-a.png");
        fs::write(&file, b"x").unwrap();
        assert!(open_clip(&file, &root.join("clips")).is_ok());
    }

    #[test]
    fn rejects_oversized_clip() {
        let root = scratch("big");
        let file = root.join("clips/clip-big.png");
        let handle = fs::File::create(&file).unwrap();
        handle
            .set_len(crate::paste_image::MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        assert!(open_clip(&file, &root.join("clips")).is_err());
    }

    #[test]
    fn owner_must_match() {
        let root = scratch("owner");
        let file = root.join("clips/clip-a.png");
        fs::write(&file, b"x").unwrap();
        let metadata = fs::metadata(&file).unwrap();
        let uid = unsafe { libc::getuid() };
        assert!(owned_regular(&metadata, uid));
        assert!(!owned_regular(&metadata, uid.wrapping_add(1)));
        assert!(!owned_regular(
            &fs::metadata(root.join("clips")).unwrap(),
            uid
        ));
    }
}
