use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

/// Return a process-unique root for concurrently executing integration tests.
pub(crate) fn temporary_root(label: &str) -> PathBuf {
    // Hosted macOS can return the same wall-clock timestamp to neighboring test
    // threads. PID plus an atomic sequence provides uniqueness without sleeps,
    // and Cargo's test directory keeps executable fixtures beside binaries that
    // have already passed the host's execution policy.
    let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("termnav-{label}-{}-{sequence}", std::process::id()))
}

/// Own an unsigned child whose environment remains inspectable on macOS 27.
#[allow(dead_code)] // Only environment-dependent suites need this shared fixture.
pub(crate) fn environment_fixture() -> std::process::Command {
    let mut command =
        std::process::Command::new(std::env::current_exe().expect("find test executable"));
    command
        .args(["--ignored", "--exact", "common::environment_fixture_child"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

/// Create a short private socket root without moving executable fixtures.
#[allow(dead_code)] // Socket-dependent suites opt into the platform pathname limit.
pub(crate) fn socket_root() -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;

    let temporary = std::env::temp_dir();
    // macOS's /var/folders TMPDIR plus a test label can exceed sockaddr_un.
    // Executables stay in Cargo's approved target tree; only sockets use /tmp.
    let parent = if temporary.as_os_str().as_encoded_bytes().len() > 60
        && std::path::Path::new("/tmp").is_dir()
    {
        std::path::Path::new("/tmp")
    } else {
        temporary.as_path()
    };
    for _ in 0..100 {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = parent.join(format!("tn-sock-{}-{sequence}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => return root,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("cannot create private socket fixture: {error}"),
        }
    }
    panic!("cannot allocate unique socket fixture");
}

#[test]
#[ignore = "owned child fixture for environment inspection tests"]
fn environment_fixture_child() {
    use std::io::Read;

    // Protected platform executables such as /bin/sleep hide their environment
    // on macOS 27. The parent holds this lifetime pipe and kills/reaps the exact
    // child; no scheduler delay or unrelated live process is part of the test.
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .expect("read fixture lifetime pipe");
}
