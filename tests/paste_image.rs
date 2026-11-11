//! `termnav paste-image stage` against fake clipboard readers.
//!
//! Receiver behavior needs a real tmux client and terminal; it is covered by
//! `test/paste-image.py` through `test/suites/paste-image-test`.

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
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n{pause}case \"$1\" in\n  types) printf '{types}' ;;\n  png) cat '{}' ;;\n  *) exit 9 ;;\nesac\n",
                data.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
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
    fs::write(
        &script,
        "#!/bin/sh\ncase \"$1\" in\n  types) printf 'image/png\\n' ;;\n  png) cat /dev/zero ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
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
