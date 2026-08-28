//! Transactional store for terminal commit directives.
//!
//! DECRQM replies carry no application nonce. The filesystem store supplies
//! the missing transaction identity and serializes all relay servers that feed
//! one physical tmux client. Files remain intentionally compatible with the
//! Python v2 implementation so independently updated hosts can interoperate.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::protocol::VERSION;

const PREPARE_STALE: Duration = Duration::from_secs(30);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(6);
const COMMIT_POLL: Duration = Duration::from_millis(10);

/// Action deferred until the terminal confirms output ordering.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DirectiveAction {
    /// Execute the selected navigation action in this tmux server.
    Execute,
    /// Forward the terminal receipt into the next nested tmux pane.
    Forward,
}

/// Durable v2 directive shared with older Python peers.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Directive {
    /// Wire version.
    pub v: u8,
    /// Request nonce.
    pub nonce: String,
    /// Monotonic per-server publication order.
    #[serde(default)]
    pub seq: u64,
    /// Deferred action kind.
    pub action: DirectiveAction,
    /// Whether outward preparation has completed.
    #[serde(default)]
    pub ready: bool,
    /// Whether the terminal reply handler has claimed this record.
    #[serde(default)]
    pub claimed: bool,
    /// Wall-clock publication time used only for crash-recovery expiry.
    #[serde(default)]
    pub prepared_at: f64,
    /// Process that owns the pending transaction.
    #[serde(default)]
    pub owner_pid: u32,
    /// Exact tmux server socket.
    pub tmux_socket: String,
    /// Session selected with the physical client snapshot.
    pub session: String,
    /// Exact physical client tty.
    pub local_client_tty: String,
    /// Exact physical client PID.
    pub local_client_pid: u32,
    /// tmux client creation time guarding PID/tty reuse.
    pub local_client_created: u64,
    /// Request start time used by bounded focus selection.
    pub started_at: u64,
    /// Pane expected to execute a local action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_pane: Option<String>,
    /// Session-qualified target for a local action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_pane: Option<String>,
    /// Relay wire scope for a local action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Direction for a local action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// Pane that receives the next commit key for a forwarding action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forward_pane: Option<String>,
}

impl Directive {
    /// Validate the complete action-specific schema before publication/claim.
    #[must_use]
    pub fn valid(&self) -> bool {
        if self.v != VERSION
            || !valid_nonce(&self.nonce)
            || self.tmux_socket.is_empty()
            || self.session.is_empty()
            || self.local_client_tty.is_empty()
            || self.local_client_pid == 0
        {
            return false;
        }
        match self.action {
            DirectiveAction::Execute => {
                self.expected_pane.as_deref().is_some_and(valid_pane)
                    && self
                        .target_pane
                        .as_ref()
                        .is_some_and(|value| !value.is_empty())
                    && valid_wire(
                        self.scope.as_deref().unwrap_or_default(),
                        self.direction.as_deref().unwrap_or_default(),
                    )
            }
            DirectiveAction::Forward => self.forward_pane.as_deref().is_some_and(valid_pane),
        }
    }

    fn client_key(&self) -> (&str, u32, u64) {
        (
            &self.local_client_tty,
            self.local_client_pid,
            self.local_client_created,
        )
    }
}

/// Owner-only store for one tmux server's pending directives.
pub struct Store {
    directory: PathBuf,
    socket: String,
}

impl Store {
    /// Open the store associated with one exact tmux socket.
    pub fn open(tmux_socket: &str) -> io::Result<Self> {
        let directory = directive_directory(tmux_socket)?;
        Ok(Self {
            directory,
            socket: tmux_socket.to_owned(),
        })
    }

    /// Allocate a monotonic sequence and durably publish one pending action.
    pub fn arm(&self, directive: &mut Directive) -> io::Result<u64> {
        if !directive.valid() || directive.tmux_socket != self.socket {
            return Err(invalid("invalid directive"));
        }
        directive.ready = false;
        directive.claimed = false;
        directive.prepared_at = wall_seconds();
        directive.owner_pid = std::process::id();

        let _lock = self.lock()?;
        if self.poison_path(directive).exists() {
            return Err(invalid("client commit stream is poisoned"));
        }
        if self.matching_exists(directive)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "client already has an outstanding directive",
            ));
        }
        if self.result_path(&directive.nonce).exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "directive result already exists",
            ));
        }

        let next = self.read_sequence().saturating_add(1);
        self.atomic_replace(&self.directory.join(".seq"), next.to_string().as_bytes())?;
        directive.seq = next;
        let payload = serde_json::to_vec(directive).map_err(invalid)?;
        self.publish_exclusive(&self.directive_path(&directive.nonce), &payload)?;
        Ok(next)
    }

    /// Make a prepared action visible to the nonce-less terminal reply.
    pub fn mark_committed(&self, nonce: &str) -> io::Result<bool> {
        if !valid_nonce(nonce) {
            return Ok(false);
        }
        let _lock = self.lock()?;
        let path = self.directive_path(nonce);
        let Some(mut directive) = read_directive(&path)? else {
            return Ok(false);
        };
        directive.ready = true;
        let payload = serde_json::to_vec(&directive).map_err(invalid)?;
        self.atomic_replace(&path, &payload)?;
        Ok(true)
    }

    /// Read one exact directive without consuming it.
    pub fn read(&self, nonce: &str) -> io::Result<Option<Directive>> {
        if !valid_nonce(nonce) {
            return Ok(None);
        }
        let _lock = self.lock()?;
        read_directive(&self.directive_path(nonce))
    }

    /// Claim the oldest ready directive for one exact physical client.
    pub fn claim(
        &self,
        client_tty: &str,
        client_pid: u32,
        client_created: u64,
    ) -> io::Result<Option<Directive>> {
        let _lock = self.lock()?;
        let mut candidates = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            match read_directive(&path)? {
                Some(directive)
                    if directive.client_key() == (client_tty, client_pid, client_created) =>
                {
                    candidates.push((directive.seq, path));
                }
                Some(_) => {}
                None => candidates.push((u64::MAX, path)),
            }
        }
        candidates.sort_by_key(|(sequence, _)| *sequence);

        for (_, path) in candidates {
            let Some(mut directive) = self.load_claimable(&path)? else {
                continue;
            };
            if directive.tmux_socket != self.socket {
                continue;
            }
            directive.claimed = true;
            let payload = serde_json::to_vec(&directive).map_err(invalid)?;
            self.atomic_replace(&path, &payload)?;
            return Ok(Some(directive));
        }
        Ok(None)
    }

    /// Publish the definitive commit result and retire its pending action.
    pub fn record_result(&self, nonce: &str, handled: bool) -> io::Result<()> {
        let payload: &[u8] = if handled { b"handled\n" } else { b"error\n" };
        match self.publish_exclusive(&self.result_path(nonce), payload) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let _ = self.remove(nonce)?;
        Ok(())
    }

    /// Remove one prepared path and its receipt as a best-effort abort.
    pub fn discard(&self, nonce: &str) -> io::Result<()> {
        let _ = self.remove(nonce)?;
        match fs::remove_file(self.result_path(nonce)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Wait for the terminal handler's durable receipt.
    ///
    /// Timeout removes the exact action before poisoning the physical client.
    /// A very late nonce-less reply can then never execute a later gesture.
    pub fn wait_for_consumption(&self, nonce: &str) -> io::Result<bool> {
        if let Some(result) = self.take_result(nonce)? {
            return Ok(result);
        }
        let Some(directive) = self.read(nonce)? else {
            return Ok(false);
        };
        let deadline = Instant::now() + COMMIT_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(result) = self.take_result(nonce)? {
                return Ok(result);
            }
            thread::sleep(COMMIT_POLL);
        }
        let _ = self.remove(nonce)?;
        let _ = fs::remove_file(self.result_path(nonce));
        match self.publish_exclusive(
            &self.poison_path(&directive),
            b"lost terminal commit response\n",
        ) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        Ok(false)
    }

    fn lock(&self) -> io::Result<FileLock> {
        let path = self.directory.join(".directives.lock");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(FileLock(file))
    }

    fn directive_path(&self, nonce: &str) -> PathBuf {
        self.directory.join(format!("{nonce}.json"))
    }

    fn result_path(&self, nonce: &str) -> PathBuf {
        self.directory.join(format!("{nonce}.result"))
    }

    fn poison_path(&self, directive: &Directive) -> PathBuf {
        let identity = format!(
            "{}\0{}\0{}",
            directive.local_client_tty, directive.local_client_pid, directive.local_client_created
        );
        self.directory
            .join(format!(".poison-{}", digest_prefix(&identity, 16)))
    }

    fn read_sequence(&self) -> u64 {
        fs::read_to_string(self.directory.join(".seq"))
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0)
    }

    fn matching_exists(&self, expected: &Directive) -> io::Result<bool> {
        let now = wall_seconds();
        for entry in fs::read_dir(&self.directory)? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(current) = read_directive(&path)? else {
                continue;
            };
            if current.client_key() != expected.client_key() {
                continue;
            }
            if !current.ready && now - current.prepared_at > PREPARE_STALE.as_secs_f64() {
                let _ = fs::remove_file(path);
                continue;
            }
            if !pid_alive(current.owner_pid) {
                let _ = fs::remove_file(path);
                if current.ready {
                    let _ = self.publish_exclusive(
                        &self.poison_path(&current),
                        b"lost terminal commit response\n",
                    );
                    return Ok(true);
                }
                continue;
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn load_claimable(&self, path: &Path) -> io::Result<Option<Directive>> {
        let Some(directive) = read_directive(path)? else {
            let _ = fs::remove_file(path);
            return Ok(None);
        };
        if !pid_alive(directive.owner_pid) {
            let _ = fs::remove_file(path);
            if directive.ready {
                let _ = self.publish_exclusive(
                    &self.poison_path(&directive),
                    b"lost terminal commit response\n",
                );
            }
            return Ok(None);
        }
        if !directive.ready || directive.claimed {
            return Ok(None);
        }
        Ok(Some(directive))
    }

    fn remove(&self, nonce: &str) -> io::Result<bool> {
        if !valid_nonce(nonce) {
            return Ok(false);
        }
        let _lock = self.lock()?;
        match fs::remove_file(self.directive_path(nonce)) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn take_result(&self, nonce: &str) -> io::Result<Option<bool>> {
        let path = self.result_path(nonce);
        let result = match fs::read_to_string(&path) {
            Ok(value) if value.trim() == "handled" => Some(true),
            Ok(value) if value.trim() == "error" => Some(false),
            Ok(_) => None,
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if result.is_some() {
            let _ = fs::remove_file(path);
        }
        Ok(result)
    }

    fn publish_exclusive(&self, path: &Path, payload: &[u8]) -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        if let Err(error) = file.write_all(payload).and_then(|()| file.sync_all()) {
            drop(file);
            let _ = fs::remove_file(path);
            return Err(error);
        }
        Ok(())
    }

    fn atomic_replace(&self, path: &Path, payload: &[u8]) -> io::Result<()> {
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| invalid("state path has no file name"))?;
        let temporary = self
            .directory
            .join(format!(".tmp-{name}-{}", std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(payload)?;
            file.sync_all()?;
            fs::rename(&temporary, path)
        })();
        let _ = fs::remove_file(temporary);
        result
    }
}

struct FileLock(File);

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn directive_directory(tmux_socket: &str) -> io::Result<PathBuf> {
    let digest = digest_prefix(tmux_socket, 12);
    crate::runtime::private_subdirectory(&[OsStr::new("directives"), OsStr::new(&digest)])
}

fn read_directive(path: &Path) -> io::Result<Option<Directive>> {
    let mut data = Vec::new();
    match File::open(path).and_then(|mut file| file.read_to_end(&mut data)) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let directive = serde_json::from_slice::<Directive>(&data).ok();
    Ok(directive.filter(Directive::valid))
}

fn digest_prefix(value: &str, length: usize) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    encoded[..length].to_owned()
}

fn wall_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn valid_nonce(value: &str) -> bool {
    value.len() == 12 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_pane(value: &str) -> bool {
    value.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn valid_wire(scope: &str, direction: &str) -> bool {
    match scope {
        "pane" => matches!(direction, "left" | "down" | "up" | "right"),
        "window" => matches!(direction, "next" | "previous"),
        "move" => matches!(direction, "left" | "right"),
        _ => false,
    }
}

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Directive, DirectiveAction};

    #[test]
    fn action_specific_schema_fails_closed() {
        let mut directive = Directive {
            v: 2,
            nonce: "aaaaaaaaaaaa".to_owned(),
            seq: 0,
            action: DirectiveAction::Execute,
            ready: false,
            claimed: false,
            prepared_at: 0.0,
            owner_pid: 1,
            tmux_socket: "/tmp/tmux.sock".to_owned(),
            session: "$1".to_owned(),
            local_client_tty: "/dev/pts/1".to_owned(),
            local_client_pid: 1,
            local_client_created: 80,
            started_at: 100,
            expected_pane: Some("%1".to_owned()),
            target_pane: Some("$1:.%1".to_owned()),
            scope: Some("pane".to_owned()),
            direction: Some("left".to_owned()),
            forward_pane: None,
        };
        assert!(directive.valid());

        directive.direction = Some("diagonal".to_owned());
        assert!(!directive.valid());
        directive.action = DirectiveAction::Forward;
        assert!(!directive.valid());
        directive.forward_pane = Some("%2".to_owned());
        assert!(directive.valid());
    }
}
