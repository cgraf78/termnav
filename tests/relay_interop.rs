use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_SOCKET_ROOT: AtomicU64 = AtomicU64::new(0);

struct Server {
    child: Child,
    socket: PathBuf,
}

impl Server {
    fn start(mut command: Command, socket: PathBuf) -> Self {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let child = command.spawn().expect("start relay server");
        let mut server = Self { child, socket };
        server.wait_ready();
        server
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if relay_request(&self.socket, Duration::ZERO)
                .is_ok_and(|reply| reply == serde_json::json!({"v": 2, "result": "error"}))
            {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("inspect relay server") {
                let mut stderr = String::new();
                if let Some(mut pipe) = self.child.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("relay server exited {status}: {stderr}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "relay request loop did not become ready: {}",
            self.socket.display()
        );
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket);
        if let Some(parent) = self.socket.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_owned()
}

fn temp_socket(name: &str) -> PathBuf {
    // Darwin's sockaddr_un permits only 103 non-NUL pathname bytes, while its
    // ordinary per-user temporary directory is already quite long. `/tmp` is
    // shared, so combine process and atomic identities rather than relying on
    // timestamps or serial test execution.
    let sequence = NEXT_SOCKET_ROOT.fetch_add(1, Ordering::Relaxed);
    let directory =
        PathBuf::from("/tmp").join(format!("tnri-{}-{sequence}-{name}", std::process::id()));
    fs::create_dir_all(&directory).expect("create interop directory");
    directory.join("relay.sock")
}

fn relay_request(socket: &Path, delay: Duration) -> Result<serde_json::Value, String> {
    let mut client =
        UnixStream::connect(socket).map_err(|error| format!("connect relay client: {error}"))?;
    client
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("bound delayed relay read");
    thread::sleep(delay);
    client
        .write_all(b"{\"v\":2,\"op\":\"readiness-probe\"}\n")
        .map_err(|error| format!("write relay request: {error}"))?;
    let mut reply = String::new();
    client
        .read_to_string(&mut reply)
        .map_err(|error| format!("read relay reply: {error}"))?;
    serde_json::from_str(&reply).map_err(|error| format!("parse relay reply: {error}"))
}

fn assert_delayed_request_survives(socket: &Path) {
    // A connected process can be descheduled before its first write on a busy
    // CI worker or laptop. The relay must tolerate that ordinary scheduling
    // gap without turning the user's first navigation gesture into EPIPE.
    let reply =
        relay_request(socket, Duration::from_millis(500)).expect("exchange delayed relay request");
    assert_eq!(reply, serde_json::json!({"v": 2, "result": "error"}));
}

#[test]
fn rust_dispatch_keeps_the_frozen_v2_envelope() {
    // The relay constants are about to move behind one Rust vocabulary module.
    // Lock the external JSON envelope first so that cleanup cannot accidentally
    // create a protocol version that only the Rust half of an SSH hop speaks.
    assert_eq!(
        termnav::relay::server::dispatch(&serde_json::json!({
            "v": 1,
            "op": "readiness-probe",
        })),
        serde_json::json!({"v": 2, "result": "error"})
    );
    assert_eq!(
        termnav::relay::server::dispatch(&serde_json::json!({
            "v": 2,
            "op": "readiness-probe",
        })),
        serde_json::json!({"v": 2, "result": "error"})
    );
}

#[test]
fn rust_client_understands_the_python_v2_server() {
    let socket = temp_socket("python-server");
    let mut command = Command::new("python3");
    command
        .arg(root().join("test/support/python-peer/relay.py"))
        .args(["serve", "--socket"])
        .arg(&socket)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    let _server = Server::start(command, socket.clone());

    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["relay", "send", "pane-select", "left"])
        .env("TERMNAV_PARENT_RELAY", &socket)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("run Rust relay client");

    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn python_client_understands_the_rust_v2_server() {
    let socket = temp_socket("rust-server");
    let mut command = Command::new(env!("CARGO_BIN_EXE_termnav"));
    command
        .args(["relay", "serve", "--socket"])
        .arg(&socket)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    let _server = Server::start(command, socket.clone());

    let output = Command::new("python3")
        .arg(root().join("test/support/python-peer/relay.py"))
        .args(["send", "pane", "left"])
        .env("TERMNAV_PARENT_RELAY", &socket)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("run Python relay client");

    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn rust_server_accepts_a_scheduled_client_after_connect() {
    let socket = temp_socket("rust-delayed-writer");
    let mut command = Command::new(env!("CARGO_BIN_EXE_termnav"));
    command
        .args(["relay", "serve", "--socket"])
        .arg(&socket)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    let _server = Server::start(command, socket.clone());

    assert_delayed_request_survives(&socket);
}

#[test]
fn frozen_python_peer_accepts_a_scheduled_client_after_connect() {
    let socket = temp_socket("python-delayed-writer");
    let mut command = Command::new("python3");
    command
        .arg(root().join("test/support/python-peer/relay.py"))
        .args(["serve", "--socket"])
        .arg(&socket)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    let _server = Server::start(command, socket.clone());

    assert_delayed_request_survives(&socket);
}
