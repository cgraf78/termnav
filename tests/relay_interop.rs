use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
        let server = Self { child, socket };
        server.wait_ready();
        server
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if fs::symlink_metadata(&self.socket)
                .is_ok_and(|metadata| metadata.file_type().is_socket())
            {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("relay socket did not appear: {}", self.socket.display());
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.socket);
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_owned()
}

fn temp_socket(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "termnav-rust-interop-{}-{}",
        std::process::id(),
        name
    ));
    fs::create_dir_all(&directory).expect("create interop directory");
    directory.join("relay.sock")
}

#[test]
fn rust_client_understands_the_python_v2_server() {
    let socket = temp_socket("python-server");
    let mut command = Command::new("python3");
    command
        .arg(root().join("lib/termnav/relay.py"))
        .args(["serve", "--socket"])
        .arg(&socket)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    let _server = Server::start(command, socket.clone());

    let output = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["relay", "send", "pane", "left"])
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
        .arg(root().join("lib/termnav/relay.py"))
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
