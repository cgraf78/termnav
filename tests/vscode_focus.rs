use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

mod common;

fn temporary_root() -> PathBuf {
    common::temporary_root("vscode-focus")
}

fn receive(listener: UnixListener) -> Vec<u8> {
    let (mut connection, _) = listener.accept().expect("accept focus request");
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = connection.read(&mut buffer).expect("read focus request");
        assert!(read > 0, "focus request ended before its body");
        request.extend_from_slice(&buffer[..read]);
        let Some(headers_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..headers_end]);
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .and_then(|value| value.parse::<usize>().ok())
            .expect("request has content length");
        if request.len() >= headers_end + 4 + length {
            break;
        }
    }
    connection
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
        .expect("reply to focus request");
    request
}

#[test]
fn direct_focus_update_posts_authenticated_ancestry_to_the_window_socket() {
    let root = temporary_root();
    std::fs::create_dir_all(&root).expect("create test root");
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    let socket = root.join("window.sock");
    let listener = UnixListener::bind(&socket).expect("bind window socket");
    let server = thread::spawn(move || receive(listener));

    let token = "a".repeat(64);
    let status = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["vscode", "focus", "claim", "nvim-a", "2", "7", "12345"])
        .env("TERMNAV_VSCODE_SOCKET", &socket)
        .env("TERMNAV_VSCODE_TOKEN", &token)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .status()
        .expect("publish focus update");
    assert!(status.success());

    let request =
        String::from_utf8(server.join().expect("join focus server")).expect("request is utf-8");
    assert!(request.starts_with("POST /nvim-focus HTTP/1.1\r\n"));
    assert!(request.contains("\"version\":2"));
    assert!(request.contains("\"operation\":\"claim\""));
    assert!(request.contains("\"source\":\"nvim-a\""));
    assert!(request.contains(&format!("\"token\":\"{token}\"")));
    assert!(request.contains("\"ancestors\":["));
}

struct ChildCleanup(Child);

impl Drop for ChildCleanup {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn tmux_claim_targets_only_the_focused_client_showing_the_editor_pane() {
    let root = temporary_root();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("create fake bin");
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());

    let tmux = bin.join("tmux");
    std::fs::write(&tmux, "#!/bin/sh\nprintf '%b' \"$TERMNAV_TMUX_CLIENTS\"\n")
        .expect("write fake tmux");
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755))
        .expect("make fake tmux executable");

    let socket_a = root.join("a.sock");
    let socket_b = root.join("b.sock");
    let listener_a = UnixListener::bind(&socket_a).expect("bind first window");
    let listener_b = UnixListener::bind(&socket_b).expect("bind second window");
    listener_b
        .set_nonblocking(true)
        .expect("make second listener nonblocking");
    let token_a = "a".repeat(64);
    let token_b = "b".repeat(64);
    let child_a = Command::new("sleep")
        .arg("30")
        .env("TERMNAV_VSCODE_SOCKET", &socket_a)
        .env("TERMNAV_VSCODE_TOKEN", &token_a)
        .spawn()
        .expect("start first client");
    let child_b = Command::new("sleep")
        .arg("30")
        .env("TERMNAV_VSCODE_SOCKET", &socket_b)
        .env("TERMNAV_VSCODE_TOKEN", &token_b)
        .spawn()
        .expect("start second client");
    let child_a = ChildCleanup(child_a);
    let child_b = ChildCleanup(child_b);

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && termnav::process::environment(child_a.0.id(), "TERMNAV_VSCODE_TOKEN").as_deref()
            != Some(token_a.as_str())
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        termnav::process::environment(child_a.0.id(), "TERMNAV_VSCODE_TOKEN").as_deref(),
        Some(token_a.as_str())
    );

    let server = thread::spawn(move || receive(listener_a));
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let clients = format!(
        "{}|%1|attached,focused\n{}|%2|attached\n",
        child_a.0.id(),
        child_b.0.id()
    );
    let status = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["vscode", "focus", "claim", "nvim-tmux", "1", "1", "20000"])
        .env("PATH", path)
        .env("TERMNAV_TMUX_CLIENTS", clients)
        .env("TMUX", "/tmp/tmux.sock,1,0")
        .env("TMUX_PANE", "%1")
        .status()
        .expect("publish tmux focus update");
    assert!(status.success());
    let request =
        String::from_utf8(server.join().expect("join focus server")).expect("request is utf-8");
    assert!(request.contains(&format!("\"token\":\"{token_a}\"")));
    assert!(matches!(
        listener_b.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[test]
fn focus_input_and_missing_window_credentials_fail_closed() {
    let invalid = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["vscode", "focus", "claim", "bad source", "1", "1", "1"])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("run invalid focus update");
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid source"));

    let unavailable = Command::new(env!("CARGO_BIN_EXE_termnav"))
        .args(["vscode", "focus", "claim", "nvim-a", "1", "1", "1"])
        .env("TERMNAV_VSCODE_SOCKET", "/tmp/window.sock")
        .env_remove("TERMNAV_VSCODE_TOKEN")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .status()
        .expect("run focus update without token");
    assert_eq!(unavailable.code(), Some(10));
}
