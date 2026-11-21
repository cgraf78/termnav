//! `termnav paste-image stage` against fake clipboard readers, and the
//! receiver's tmux version gate against a fake tmux.
//!
//! The rest of the receiver needs a real tmux client and terminal; it is
//! covered by `test/paste-image.py` through `test/suites/paste-image-test`.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

mod common;

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = common::temporary_root("paste-image");
        for directory in ["run", "home", "tmp"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        fs::set_permissions(root.join("run"), fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }

    /// Write a reader that offers `types` and prints `image` for `png`.
    fn adapter(&self, types: &str, image: &[u8], delay: Option<&str>) -> PathBuf {
        let data = self.root.join("clipboard.bin");
        fs::write(&data, image).unwrap();
        let script = self.root.join("adapter");
        let pause = delay
            .map(|seconds| format!("sleep {seconds}\n"))
            .unwrap_or_default();
        common::write_executable(
            &script,
            &format!(
                "#!/bin/sh\n{pause}case \"$1\" in\n  types) printf '{types}' ;;\n  png) cat '{}' ;;\n  *) exit 9 ;;\nesac\n",
                data.display()
            ),
        );
        script
    }

    fn command(&self, adapter: Option<&Path>) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_termnav"));
        command
            .args(["paste-image", "stage"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("TMPDIR", self.root.join("tmp"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"));
        if let Some(adapter) = adapter {
            command.env("TERMNAV_PASTE_IMAGE_CLIPBOARD", adapter);
        }
        command
    }

    fn stage(&self, adapter: Option<&Path>) -> Output {
        self.command(adapter).output().unwrap()
    }

    fn stage_directory(&self) -> PathBuf {
        let uid = unsafe { libc::getuid() };
        self.root
            .join("run")
            .join(format!("termnav-{uid}"))
            .join("paste-image")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn png(size: usize) -> Vec<u8> {
    let mut bytes = PNG_SIGNATURE.to_vec();
    bytes.extend((0..size - PNG_SIGNATURE.len()).map(|index| (index * 31 % 251) as u8));
    bytes
}

#[test]
fn an_image_only_clipboard_is_staged_owner_only() {
    let fixture = Fixture::new();
    let image = png(5000);
    let adapter = fixture.adapter("image/png\\nimage/bmp\\n", &image, None);
    let output = fixture.stage(Some(&adapter));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let path = PathBuf::from(stdout.strip_suffix('\n').unwrap());
    assert!(path.starts_with(fixture.stage_directory()), "{path:?}");
    let metadata = fs::metadata(&path).unwrap();
    assert_eq!(metadata.mode() & 0o777, 0o600);
    assert_eq!(
        fs::metadata(fixture.stage_directory()).unwrap().mode() & 0o777,
        0o700
    );
    let staged = fs::read_to_string(&path).unwrap();
    let mut fields = staged.splitn(3, ':');
    assert_eq!(fields.next(), Some("5000"));
    let digest = fields.next().unwrap();
    assert_eq!(digest.len(), 64);
    assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let encoded = fields.next().unwrap();
    assert_eq!(encoded.len(), 5000_usize.div_ceil(3) * 4);
    assert!(
        encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte)),
        "single-line base64"
    );
    assert!(encoded.starts_with("iVBORw0KGg"));
}

#[test]
fn text_or_missing_images_decline_with_status_three() {
    let fixture = Fixture::new();
    for types in [
        "image/png\\ntext/plain;charset=utf-8\\n",
        "UTF8_STRING\\nimage/png\\n",
        "image/jpeg\\n",
        "",
    ] {
        let adapter = fixture.adapter(types, &png(100), None);
        let output = fixture.stage(Some(&adapter));
        assert_eq!(output.status.code(), Some(3), "{types}: {output:?}");
        assert!(output.stdout.is_empty());
    }
    let staged = fs::read_dir(fixture.stage_directory()).unwrap().count();
    assert_eq!(staged, 0, "declines stage nothing");
}

#[test]
fn invalid_or_oversized_images_fail_without_staging() {
    let fixture = Fixture::new();
    let not_png = fixture.adapter("image/png\\n", b"GIF89a not a png", None);
    let output = fixture.stage(Some(&not_png));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let oversized = fixture.adapter("image/png\\n", &png(16 * 1024 * 1024 + 1), None);
    let output = fixture.stage(Some(&oversized));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(fs::read_dir(fixture.stage_directory()).unwrap().count(), 0);
}

#[test]
fn exactly_sixteen_mebibytes_is_staged() {
    // The reader limit bounds the subprocess only, so validating, hashing,
    // and encoding the largest accepted image cannot time out on a slow host.
    let fixture = Fixture::new();
    let size = 16 * 1024 * 1024;
    let adapter = fixture.adapter("image/png\\n", &png(size), None);
    let output = fixture.stage(Some(&adapter));
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let path = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim_end());
    let header = format!("{size}:");
    let staged = fs::read(&path).unwrap();
    assert!(staged.starts_with(header.as_bytes()));
    assert_eq!(staged.len(), header.len() + 65 + size.div_ceil(3) * 4);
    assert_eq!(fs::read_dir(fixture.stage_directory()).unwrap().count(), 1);
}

#[test]
fn a_runaway_reader_stops_one_byte_past_the_cap() {
    let fixture = Fixture::new();
    let script = fixture.root.join("runaway");
    common::write_executable(
        &script,
        "#!/bin/sh\ncase \"$1\" in\n  types) printf 'image/png\\n' ;;\n  png) cat /dev/zero ;;\nesac\n",
    );
    let output = fixture.stage(Some(&script));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("larger than 16 MiB"),
        "{output:?}"
    );
    assert_eq!(fs::read_dir(fixture.stage_directory()).unwrap().count(), 0);
}

#[test]
fn a_hung_reader_is_bounded_by_the_read_limit() {
    let fixture = Fixture::new();
    let adapter = fixture.adapter("image/png\\n", &png(100), Some("60"));
    let started = Instant::now();
    let output = fixture.stage(Some(&adapter));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    // The limit is five seconds; the margin only absorbs runner scheduling.
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
}

#[cfg(not(target_os = "macos"))]
#[test]
fn no_desktop_clipboard_declines() {
    let fixture = Fixture::new();
    let output = fixture.stage(None);
    assert_eq!(output.status.code(), Some(3), "{output:?}");
}

#[test]
fn stale_staged_files_are_pruned() {
    let fixture = Fixture::new();
    let adapter = fixture.adapter("image/png\\n", &png(100), None);
    assert_eq!(fixture.stage(Some(&adapter)).status.code(), Some(0));
    let directory = fixture.stage_directory();
    let stale = directory.join("stage-0123456789abcdef01234567.b64");
    let unrelated = directory.join("keep.txt");
    fs::write(&stale, b"old").unwrap();
    fs::write(&unrelated, b"old").unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    for path in [&stale, &unrelated] {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    assert_eq!(fixture.stage(Some(&adapter)).status.code(), Some(0));
    assert!(!stale.exists());
    assert!(unrelated.exists());
}

#[test]
fn usage_errors_exit_two() {
    for arguments in [
        &["paste-image"][..],
        &["paste-image", "stage", "extra"],
        &["paste-image", "receive"],
        &["paste-image", "receive", "--target-pane", "%1"],
        &["paste-image", "bogus"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
            .args(arguments)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
    }
}

#[test]
fn a_receiver_on_an_unsupported_tmux_tells_the_client_and_does_nothing_else() {
    let (major, minor) = common::tmux_floor();
    let older = format!("{major}.{}a", minor - 1);
    let fixture = Fixture::new();
    let bin = fixture.root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = fixture.root.join("tmux.log");
    let tmux = bin.join("tmux");
    common::write_executable(
        &tmux,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\n\
             case \" $* \" in *\" display-message -p #{{version}} \"*) echo {older};; esac\n",
            log.display()
        ),
    );
    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args([
            "paste-image",
            "receive",
            "--target-pane",
            "%3",
            "--client-name",
            "/dev/pts/receiver",
            "--tmux-socket",
            "/tmp/termnav-fake.sock",
        ])
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", fixture.root.join("home"))
        .env("TMPDIR", fixture.root.join("tmp"))
        .env("XDG_RUNTIME_DIR", fixture.root.join("run"))
        .output()
        .unwrap();
    let refusal = format!("needs tmux {major}.{minor} or newer (server is {older})");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains(&format!("termnav paste-image: {refusal}")),
        "{output:?}"
    );
    // The version query and the message, and nothing else: no lookup, no
    // paste.
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        format!(
            "-S /tmp/termnav-fake.sock display-message -p #{{version}}\n\
             -S /tmp/termnav-fake.sock display-message -c /dev/pts/receiver -d 5000 termnav: {refusal}\n"
        )
    );
}

/// Preview tests run the binary with stdout piped, so the size falls back to
/// 80x24 cells of 10x20 pixels. The environment is cleared, which also keeps
/// the tmux size fallback away from any real server.
struct Preview {
    fixture: Fixture,
}

impl Preview {
    fn new() -> Self {
        let fixture = Fixture::new();
        let directory = fixture.clips();
        fs::create_dir_all(&directory).unwrap();
        for path in [directory.parent().unwrap(), &directory] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self { fixture }
    }

    /// Write a valid PNG named `name` into the clip directory.
    fn clip(&self, name: &str, width: u32, height: u32) -> PathBuf {
        let path = self.fixture.clips().join(name);
        fs::write(&path, gradient(width, height)).unwrap();
        path
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_termnav"))
            .arg("paste-image")
            .arg("preview")
            .args(arguments)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.fixture.root.join("home"))
            .env("TMPDIR", self.fixture.root.join("tmp"))
            .env("XDG_RUNTIME_DIR", self.fixture.root.join("run"))
            .env("XDG_CACHE_HOME", self.fixture.root.join("cache"))
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .output()
            .unwrap()
    }
}

impl Fixture {
    fn clips(&self) -> PathBuf {
        self.root.join("cache/termnav/paste-images")
    }
}

/// An RGB PNG with a diagonal gradient, so every thumbnail cell differs.
fn gradient(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().unwrap();
    let data: Vec<u8> = (0..height)
        .flat_map(|y| (0..width).flat_map(move |x| [(x % 256) as u8, (y % 256) as u8, 128]))
        .collect();
    writer.write_image_data(&data).unwrap();
    writer.finish().unwrap();
    bytes
}

#[test]
fn preview_blocks_renders_a_clip() {
    let preview = Preview::new();
    let clip = preview.clip("clip-20260101T000000Z-aaaaaaaaaaaa.png", 800, 600);
    let output = preview.run(&["--mode", "blocks", "--", clip.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("\x1b[38;2;"), "{stdout:?}");
    assert!(stdout.ends_with('\n'));
    assert!(stdout.lines().count() <= 12, "{}", stdout.lines().count());
    assert!(output.stderr.is_empty());
}

#[test]
fn preview_sixel_renders_a_clip() {
    let preview = Preview::new();
    let clip = preview.clip("clip-20260101T000000Z-bbbbbbbbbbbb.png", 800, 600);
    let output = preview.run(&["--mode", "sixel", "--", clip.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("\x1bP"), "{stdout:?}");
    assert!(stdout.ends_with("\x1b\\\n"), "{stdout:?}");
}

#[test]
fn preview_rejections_are_silent() {
    let preview = Preview::new();
    let directory = preview.fixture.clips();
    let missing = directory.join("clip-20260101T000000Z-cccccccccccc.png");
    let outside_directory = preview.fixture.root.join("outside");
    fs::create_dir_all(&outside_directory).unwrap();
    let outside = outside_directory.join("clip-20260101T000000Z-dddddddddddd.png");
    fs::write(&outside, gradient(10, 10)).unwrap();
    let not_png = directory.join("clip-20260101T000000Z-eeeeeeeeeeee.png");
    fs::write(&not_png, b"GIF89a not a png").unwrap();
    let removed = preview.clip("clip-20260101T000000Z-ffffffffffff.png", 10, 10);
    fs::remove_file(&removed).unwrap();
    for path in [&missing, &outside, &not_png, &removed] {
        for mode in ["blocks", "sixel"] {
            let output = preview.run(&["--mode", mode, "--", path.to_str().unwrap()]);
            assert_eq!(output.status.code(), Some(0), "{path:?}: {output:?}");
            assert!(output.stdout.is_empty(), "{path:?}: {output:?}");
            assert!(output.stderr.is_empty(), "{path:?}: {output:?}");
        }
    }
}

#[test]
fn preview_rows_option_limits_height() {
    let preview = Preview::new();
    let clip = preview.clip("clip-20260101T000000Z-111111111111.png", 800, 800);
    let output = preview.run(&["--rows", "4", "--", clip.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let lines = String::from_utf8(output.stdout).unwrap().lines().count();
    assert!((1..=4).contains(&lines), "{lines}");
}

#[test]
fn preview_environment_rows_apply_unless_invalid() {
    let preview = Preview::new();
    let clip = preview.clip("clip-20260101T000000Z-222222222222.png", 800, 800);
    let path = clip.to_str().unwrap();
    let run = |rows: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
            .args(["paste-image", "preview", "--", path])
            .env_clear()
            .env("HOME", preview.fixture.root.join("home"))
            .env("XDG_CACHE_HOME", preview.fixture.root.join("cache"))
            .env("TERMNAV_PASTE_IMAGE_PREVIEW_ROWS", rows)
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().lines().count()
    };
    assert!((1..=3).contains(&run("3")));
    for invalid in ["0", "x", ""] {
        assert!(run(invalid) > 3, "{invalid:?} falls back to 12");
    }
}

#[test]
fn preview_usage_errors() {
    let preview = Preview::new();
    let clip = preview.clip("clip-20260101T000000Z-333333333333.png", 10, 10);
    let path = clip.to_str().unwrap();
    for arguments in [
        &[][..],
        &["--"],
        &["--mode", "auto", "--", path],
        &["--mode", "bogus", "--", path],
        &["--rows", "0", "--", path],
        &["--rows", "x", "--", path],
        &["--bogus", "--", path],
        &["--mode"],
        &["--", path, "extra"],
        &[path],
    ] {
        let output = preview.run(arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("usage:"),
            "{arguments:?}: {output:?}"
        );
    }
}
