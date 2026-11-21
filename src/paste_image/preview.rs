//! Inline preview of a saved clipboard image.
//!
//! The zsh widget runs this after a paste with stdout on the terminal. It must
//! never fail loudly, so every rejection or error prints nothing and exits 0.
//! No terminal is ever queried: the size comes from `TIOCGWINSZ` on stdout,
//! then tmux, then fixed defaults.

use std::ffi::CString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
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

/// One source's report of the pane size. Terminals and tmux report 0 for
/// what they do not know, so each half is kept only when fully non-zero and
/// the halves are merged independently: a tty that knows its columns but not
/// its pixels must still bound the width.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Measured {
    cells: Option<(u16, u16)>,
    cell_pixels: Option<(u16, u16)>,
}

impl Measured {
    fn new(cols: u16, rows: u16, cell: (u16, u16)) -> Measured {
        let both = |pair: (u16, u16)| (pair.0 > 0 && pair.1 > 0).then_some(pair);
        Measured {
            cells: both((cols, rows)),
            cell_pixels: both(cell),
        }
    }

    fn complete(self) -> bool {
        self.cells.is_some() && self.cell_pixels.is_some()
    }
}

/// Combine the tty report with a lazy tmux one, then the defaults.
///
/// `tmux` runs at most once, and only when the tty left something unknown.
fn merge(tty: Measured, tmux: impl FnOnce() -> Measured) -> Pane {
    let fallback = if tty.complete() {
        Measured::default()
    } else {
        tmux()
    };
    let (cols, rows) = tty
        .cells
        .or(fallback.cells)
        .unwrap_or((FALLBACK.cols, FALLBACK.rows));
    Pane {
        cols,
        rows,
        cell: tty
            .cell_pixels
            .or(fallback.cell_pixels)
            .unwrap_or(FALLBACK.cell),
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
/// (compared by device and inode). Opening the basename relative to the
/// trusted directory descriptor keeps it there even if an alias or ancestor
/// changes after the comparison. `O_NOFOLLOW` rejects a final symlink and
/// `O_NONBLOCK` keeps a FIFO from hanging the open.
///
/// # Errors
///
/// Returns an error for any file that fails those checks, is owned by
/// another user, or exceeds [`MAX_IMAGE_BYTES`].
pub fn open_clip(path: &Path, directory: &Path) -> io::Result<File> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_clip_name(name))
        .ok_or_else(rejected)?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(directory)?;
    let trusted = directory.metadata()?;
    let uid = unsafe { libc::getuid() };
    let parent = fs::metadata(path.parent().ok_or_else(rejected)?)?;
    if trusted.uid() != uid || (parent.dev(), parent.ino()) != (trusted.dev(), trusted.ino()) {
        return Err(rejected());
    }
    let name = CString::new(name).map_err(|_| rejected())?;
    // SAFETY: both the directory descriptor and the NUL-terminated basename
    // stay live for openat. Without O_CREAT this call takes no mode argument.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh descriptor, handed to this sole owner.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !owned_regular(&metadata, uid) || metadata.len() > MAX_IMAGE_BYTES as u64 {
        return Err(rejected());
    }
    Ok(file)
}

/// What the terminal on stdout reports; empty when it is not a tty.
fn tty_size() -> Measured {
    // SAFETY: `winsize` is plain old data, and the ioctl only fills it.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } != 0 {
        return Measured::default();
    }
    let width = size.ws_xpixel.checked_div(size.ws_col).unwrap_or(0);
    let height = size.ws_ypixel.checked_div(size.ws_row).unwrap_or(0);
    Measured::new(size.ws_col, size.ws_row, (width, height))
}

/// Parse `#{pane_width} #{pane_height} #{window_cell_width} #{window_cell_height}`.
fn parse_tmux(text: &str) -> Option<Measured> {
    let mut fields = text.split_whitespace().map(|field| field.parse::<u16>());
    let mut next = || fields.next()?.ok();
    let measured = Measured::new(next()?, next()?, (next()?, next()?));
    // Extra fields mean the format changed under us; do not guess.
    fields.next().is_none().then_some(measured)
}

/// Ask tmux for the size of `$TMUX_PANE`, for when stdout is not the tty
/// (the widget redirects it, and tmux may not forward pixel sizes).
fn tmux_size() -> Measured {
    query_tmux().unwrap_or_default()
}

fn query_tmux() -> Option<Measured> {
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
    let pane = merge(tty_size(), tmux_size);
    let rows = rows_for(
        rows.or_else(environment_rows).unwrap_or(DEFAULT_ROWS),
        pane.rows,
    )?;
    let (cell_width, cell_height) = (u32::from(pane.cell.0), u32::from(pane.cell.1));
    let width_limit = u32::from(pane.cols) * cell_width;
    let mut height_limit = u32::from(rows) * cell_height;
    if mode == Mode::Sixel {
        // tmux 3.6 reserves the height of the full final band. Fit only as
        // many bands as the requested rows can physically hold.
        height_limit -= height_limit % image::BAND_ROWS as u32;
    }
    let thumbnail = image::decode(file.take(MAX_IMAGE_BYTES as u64), |width, height| {
        let (w, h) = image::fit((width, height), (width_limit, height_limit))?;
        Some(match mode {
            Mode::Sixel => (w, h),
            // Sample in physical pixels first: font cells are not necessarily
            // twice as tall as wide. Round up so sub-cell images stay visible.
            Mode::Blocks => (w.div_ceil(cell_width), (2 * h).div_ceil(cell_height)),
        })
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
    use super::{Measured, Mode, Pane, merge, open_clip, owned_regular, parse_tmux, rows_for};
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const CLIP: &str = "clip-20261121T000000Z-012345abcdef.png";

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
        assert_eq!(parse_tmux("120 40 9 18\n"), Some(full(120, 40, 9, 18)));
        // Zeros are valid reports of "unknown", kept per half.
        assert_eq!(
            parse_tmux("120 40 0 18"),
            Some(Measured {
                cells: Some((120, 40)),
                cell_pixels: None
            })
        );
        for invalid in ["", "120 40 9", "120 40 9 18 1", "a b c d"] {
            assert_eq!(parse_tmux(invalid), None, "{invalid:?}");
        }
    }

    fn full(cols: u16, rows: u16, w: u16, h: u16) -> Measured {
        Measured::new(cols, rows, (w, h))
    }

    fn pane(cols: u16, rows: u16, w: u16, h: u16) -> Pane {
        Pane {
            cols,
            rows,
            cell: (w, h),
        }
    }

    fn never() -> Measured {
        panic!("tmux must not be queried")
    }

    #[test]
    fn complete_tty_report_skips_tmux() {
        assert_eq!(merge(full(100, 30, 8, 16), never), pane(100, 30, 8, 16));
    }

    #[test]
    fn tty_cells_without_pixels_take_tmux_cell_size() {
        let merged = merge(full(60, 20, 0, 0), || full(200, 50, 7, 14));
        assert_eq!(merged, pane(60, 20, 7, 14));
    }

    #[test]
    fn missing_tty_uses_tmux_in_full() {
        let merged = merge(Measured::default(), || full(90, 33, 9, 18));
        assert_eq!(merged, pane(90, 33, 9, 18));
    }

    #[test]
    fn zero_tmux_cell_size_defaults_but_keeps_cells() {
        assert_eq!(
            merge(Measured::default(), || full(90, 33, 0, 0)),
            pane(90, 33, 10, 20)
        );
        assert_eq!(
            merge(full(60, 20, 0, 0), || full(90, 33, 0, 0)),
            pane(60, 20, 10, 20)
        );
    }

    #[test]
    fn nothing_known_gives_defaults() {
        assert_eq!(
            merge(Measured::default(), Measured::default),
            pane(80, 24, 10, 20)
        );
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
        let file = root.join(CLIP);
        fs::write(&file, b"x").unwrap();
        assert!(open_clip(&file, &root.join("clips")).is_err());
    }

    #[test]
    fn rejects_symlink() {
        let root = scratch("symlink");
        let target = root.join("clips/other.png");
        fs::write(&target, b"x").unwrap();
        let link = root.join("clips").join(CLIP);
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_clip(&link, &root.join("clips")).is_err());
    }

    #[test]
    fn rejects_fifo_without_blocking() {
        let root = scratch("fifo");
        let fifo = root.join("clips").join(CLIP);
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let start = Instant::now();
        assert!(open_clip(&fifo, &root.join("clips")).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn rejects_wrong_name() {
        let root = scratch("name");
        for name in ["clip-x.txt", ".clip-a.tmp", "other.png", "clip-.png"] {
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
        let file = nested.join(CLIP);
        fs::write(&file, b"x").unwrap();
        assert!(open_clip(&file, &root.join("clips")).is_err());
    }

    #[test]
    fn rejects_directory_named_like_a_clip() {
        let root = scratch("dirclip");
        let directory = root.join("clips").join(CLIP);
        fs::create_dir(&directory).unwrap();
        assert!(open_clip(&directory, &root.join("clips")).is_err());
    }

    #[test]
    fn rejects_a_symlink_as_the_trusted_directory() {
        let root = scratch("linked-dir");
        let actual = root.join("clips");
        fs::write(actual.join(CLIP), b"x").unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        assert!(open_clip(&alias.join(CLIP), &alias).is_err());
    }

    #[test]
    fn accepts_owned_clip() {
        let root = scratch("ok");
        let file = root.join("clips").join(CLIP);
        fs::write(&file, b"x").unwrap();
        assert!(open_clip(&file, &root.join("clips")).is_ok());
    }

    #[test]
    fn rejects_oversized_clip() {
        let root = scratch("big");
        let file = root.join("clips").join(CLIP);
        let handle = fs::File::create(&file).unwrap();
        handle
            .set_len(crate::paste_image::MAX_IMAGE_BYTES as u64 + 1)
            .unwrap();
        assert!(open_clip(&file, &root.join("clips")).is_err());
    }

    #[test]
    fn owner_must_match() {
        let root = scratch("owner");
        let file = root.join("clips").join(CLIP);
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
