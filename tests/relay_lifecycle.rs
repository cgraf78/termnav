use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // macOS expands its per-user temporary directory to a path long enough to
    // exhaust sockaddr_un before the test adds Termnav's private directory and
    // socket name. /tmp is still owner-isolated below this unique leaf, while
    // leaving enough path budget to exercise the production socket layout.
    PathBuf::from("/tmp").join(format!("tn-{}-{nonce}", std::process::id()))
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn make_old(path: &Path) {
    let path = CString::new(path.as_os_str().as_bytes()).expect("socket path has no NUL");
    let times = [
        libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
        libc::timespec {
            tv_sec: 1,
            tv_nsec: 0,
        },
    ];
    // SAFETY: the C string and two-element timespec array remain valid for the
    // duration of this call, and AT_FDCWD makes the absolute path authoritative.
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) },
        0
    );
}

#[test]
fn sweep_removes_dead_sockets_from_the_private_runtime_directory() {
    let root = temporary_root();
    let private = root.join(format!("termnav-{}", unsafe { libc::getuid() }));
    std::fs::create_dir_all(&private).expect("create private runtime");
    let _cleanup = Cleanup(root.clone());
    let dead = private.join("relay-dead.sock");
    drop(UnixListener::bind(&dead).expect("bind dead socket"));
    make_old(&dead);

    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["relay", "sweep"])
        .env("XDG_RUNTIME_DIR", &root)
        .output()
        .expect("run relay sweep");

    assert!(output.status.success());
    assert!(!dead.exists(), "dead private-runtime socket survived sweep");
}

#[test]
fn sweep_removes_dead_sockets_from_the_short_macos_fallback() {
    let root = temporary_root().join("x".repeat(120));
    std::fs::create_dir_all(&root).expect("create long runtime root");
    let _cleanup = Cleanup(root.clone());
    let fallback = PathBuf::from(format!("/tmp/termnav-{}", unsafe { libc::getuid() }));
    std::fs::create_dir_all(&fallback).expect("create fallback runtime");
    let dead = fallback.join(format!(
        "relay-test-{}-{}.sock",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    drop(UnixListener::bind(&dead).expect("bind fallback socket"));
    make_old(&dead);

    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["relay", "sweep"])
        .env("XDG_RUNTIME_DIR", &root)
        .output()
        .expect("run relay sweep");

    assert!(output.status.success());
    assert!(!dead.exists(), "dead short-runtime socket survived sweep");
}
