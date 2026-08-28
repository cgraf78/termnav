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
