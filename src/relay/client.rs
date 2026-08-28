//! Low-overhead Unix-socket client used by navigation boundaries.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const MAX_REPLY_BYTES: u64 = 513;

/// Return the fixed-width request identity used by relay navigation.
#[must_use]
pub fn new_nonce() -> String {
    let mut bytes = [0_u8; 6];
    if File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .is_err()
    {
        // `/dev/urandom` exists on every supported production platform. This
        // fallback is for constrained test sandboxes; mixing time and PID keeps
        // the nonce useful for collision avoidance without pretending it is a
        // security token.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mixed = nanos ^ u128::from(std::process::id());
        bytes.copy_from_slice(&mixed.to_le_bytes()[..6]);
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Exchange one newline-delimited JSON object with a relay server.
///
/// A receive-side failure is ambiguous: the server may have committed the
/// gesture before its reply was lost. Return a typed error and let the caller's
/// operation-specific policy decide whether retrying would be safe.
pub fn send(path: &Path, request: &Value, timeout: Duration) -> io::Result<Value> {
    // Prepare bytes before connect(2). The server necessarily keeps a bounded
    // pre-request deadline, so avoid spending any of that scheduling window on
    // serialization after the connection has entered its accept queue.
    let mut payload = serde_json::to_vec(request)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    payload.push(b'\n');

    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(&payload)?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream.take(MAX_REPLY_BYTES));
    reader.read_line(&mut line)?;
    if line.len() > 512 || !line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "relay reply is oversized or truncated",
        ));
    }
    let reply: Value = serde_json::from_str(&line)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if reply.get("v") != Some(&json!(2)) || !reply.is_object() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "relay reply has an unsupported shape",
        ));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::new_nonce;

    #[test]
    fn nonce_has_the_wire_width_and_alphabet() {
        let nonce = new_nonce();

        assert_eq!(nonce.len(), 12);
        assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}
