//! Production adapters for tmux, process ancestry, relays, and terminals.

use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use super::{Action, Backend, Client, Direction, Outcome, Scope, choose_client};
use crate::process;
use crate::relay::client::{new_nonce, send};

const FIELD_SEPARATOR: char = '|';
const DECLINED_MARKER: &str = "__TERMNAV_DECLINED__";
const ERROR_MARKER: &str = "__TERMNAV_ERROR__";
const TMUX_TIMEOUT: Duration = Duration::from_secs(2);

/// Concrete navigation backend for the invoking process.
///
/// The environment is captured once. A single gesture therefore cannot see a
/// half-updated mixture if a shell hook changes variables while routing, while
/// live client/process state is deliberately re-read at every safety boundary.
pub struct SystemBackend {
    environment: HashMap<String, String>,
}

impl SystemBackend {
    /// Build a backend from the process environment.
    #[must_use]
    pub fn from_current_environment() -> Self {
        Self {
            environment: env::vars().collect(),
        }
    }

    /// Build a backend around an explicit environment for deterministic tests.
    #[must_use]
    pub fn new(environment: HashMap<String, String>) -> Self {
        Self { environment }
    }

    fn tmux(&self, scope: &Scope, arguments: &[String]) -> Option<Output> {
        let mut command = Command::new("tmux");
        command.args(["-S", &scope.socket]);
        command.args(arguments);
        // An inherited TMUX value can silently retarget nested calls despite an
        // explicit socket on older tmux releases. Removing it makes the socket
        // argument the single source of server identity.
        command.env_remove("TMUX");
        process::output_timeout(&mut command, TMUX_TIMEOUT).ok()
    }

    fn outcome(output: Option<Output>) -> Outcome {
        let Some(output) = output else {
            return Outcome::Error;
        };
        let stdout = String::from_utf8_lossy(&output.stdout);
        match stdout.trim() {
            DECLINED_MARKER => Outcome::Declined,
            ERROR_MARKER => Outcome::Error,
            _ if output.status.success() => Outcome::Handled,
            _ => Outcome::Error,
        }
    }

    fn all_clients(&self, socket: &str) -> Vec<Client> {
        let scope = Scope {
            socket: socket.to_owned(),
            pane: String::new(),
            session: None,
        };
        let format = tmux_format(&[
            "client_activity",
            "client_pid",
            "client_tty",
            "client_termtype",
            "session_id",
            "pane_id",
            "client_flags",
            "client_control_mode",
            "client_created",
        ]);
        let Some(output) = self.tmux(
            &scope,
            &["list-clients".to_owned(), "-F".to_owned(), format],
        ) else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        parse_clients(socket, &String::from_utf8_lossy(&output.stdout))
            .into_iter()
            .filter(|client| !client.control)
            .collect()
    }

    fn matching_clients(&self, expected: &Client) -> Vec<Client> {
        if expected.socket.is_empty() {
            return vec![expected.clone()];
        }
        self.all_clients(&expected.socket)
            .into_iter()
            .filter(|current| {
                current.pane == expected.pane
                    && (expected.session.is_empty() || current.session == expected.session)
            })
            .collect()
    }

    fn same_route(current: &Client, expected: &Client) -> bool {
        current.pid == expected.pid
            && current.tty == expected.tty
            && current.pane == expected.pane
            && current.socket == expected.socket
            && (expected.created == 0 || current.created == expected.created)
            && (expected.session.is_empty() || current.session == expected.session)
    }

    fn move_tab(&self, scope: &Scope, direction: Direction) -> Outcome {
        let Some(fields) = self.tmux(
            scope,
            &[
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                scope.target(),
                tmux_format(&["session_windows", "window_id"]),
            ],
        ) else {
            return Outcome::Error;
        };
        let text = String::from_utf8_lossy(&fields.stdout);
        let parts = text.trim().split(FIELD_SEPARATOR).collect::<Vec<_>>();
        let Some(count) = parts.first().and_then(|value| value.parse::<usize>().ok()) else {
            return Outcome::Error;
        };
        if !fields.status.success() || parts.len() != 2 {
            return Outcome::Error;
        }
        if count <= 1 {
            return Outcome::Declined;
        }

        let Some(session) = scope.session.as_ref() else {
            return Outcome::Error;
        };
        let Some(listed) = self.tmux(
            scope,
            &[
                "list-windows".to_owned(),
                "-t".to_owned(),
                session.clone(),
                "-F".to_owned(),
                "#{window_id}".to_owned(),
            ],
        ) else {
            return Outcome::Error;
        };
        let windows = String::from_utf8_lossy(&listed.stdout)
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let Some(source_index) = windows.iter().position(|window| window == parts[1]) else {
            return Outcome::Error;
        };
        if !listed.status.success() {
            return Outcome::Error;
        }
        let target_index = match direction {
            Direction::Left => source_index.checked_sub(1),
            Direction::Right => source_index
                .checked_add(1)
                .filter(|index| *index < windows.len()),
            _ => None,
        };
        let Some(target_index) = target_index else {
            // Reordering at the end is an owned no-op, unlike selecting beyond
            // an edge, which must bubble to the next navigation scope.
            return Outcome::Handled;
        };
        let swap = shell_join(&[
            "swap-window".to_owned(),
            "-d".to_owned(),
            "-s".to_owned(),
            format!("{session}:{}", parts[1]),
            "-t".to_owned(),
            format!("{session}:{}", windows[target_index]),
        ]);
        Self::outcome(self.tmux(
            scope,
            &[
                "if-shell".to_owned(),
                "-F".to_owned(),
                "-t".to_owned(),
                scope.target(),
                "#{&&:#{window_active},#{pane_active}}".to_owned(),
                swap,
                shell_join(&[
                    "display-message".to_owned(),
                    "-p".to_owned(),
                    ERROR_MARKER.to_owned(),
                ]),
            ],
        ))
    }

    fn terminal_environment(&self, pid: u32, name: &str) -> Option<String> {
        process::environment(pid, name).or_else(|| self.environment.get(name).cloned())
    }

    fn vscode_socket(&self, socket: &str, token: &str, direction: Direction) -> bool {
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return false;
        }
        let payload = json!({ "direction": direction.as_str(), "token": token }).to_string();
        let request = http_request("/switch-tab", &payload, &[("Accept", "application/json")]);
        let Ok(mut stream) = UnixStream::connect(socket) else {
            return false;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        http_success(&mut stream, request.as_bytes())
    }

    fn vscode_mcp(&self, direction: Direction) -> bool {
        let state = self
            .environment
            .get("XDG_STATE_HOME")
            .filter(|value| Path::new(value).is_absolute())
            .cloned()
            .or_else(|| {
                self.environment
                    .get("HOME")
                    .map(|home| format!("{home}/.local/state"))
            });
        let Some(state) = state else {
            return false;
        };
        let Ok(token) = fs::read_to_string(format!("{state}/dot/vscode-mcp-auth-token")) else {
            return false;
        };
        let token = token.trim_end_matches('\n');
        if token.is_empty() {
            return false;
        }
        let command = if direction == Direction::Previous {
            "workbench.action.terminal.focusPrevious"
        } else {
            "workbench.action.terminal.focusNext"
        };
        let call = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "execute_command",
                "arguments": { "command": command },
            },
        });
        let Some(response) = self.vscode_mcp_post(&call, token) else {
            return false;
        };
        if response.get("error").is_none() {
            return true;
        }

        // The devserver MCP endpoint may outlive its initialization state.
        // Retry once after the protocol handshake, but never loop in response
        // to arbitrary server failures on a keypress path.
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2024-11-05", "capabilities": {} },
        });
        let Some(initialized) = self.vscode_mcp_post(&initialize, token) else {
            return false;
        };
        initialized.get("error").is_none()
            && self
                .vscode_mcp_post(&call, token)
                .is_some_and(|reply| reply.get("error").is_none())
    }

    fn vscode_mcp_post(&self, payload: &Value, token: &str) -> Option<Value> {
        let port = self
            .environment
            .get("VSCODE_MCP_PORT")
            .map_or("9876", String::as_str);
        let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .ok()?;
        let body = payload.to_string();
        let request = http_request(
            "/mcp",
            &body,
            &[("Authorization", &format!("Bearer {token}"))],
        );
        let response = http_response(&mut stream, request.as_bytes())?;
        let (_, body) = response.split_once("\r\n\r\n")?;
        serde_json::from_str(body).ok()
    }

    fn write_wezterm_var(tty: &str, name: &str, direction: Direction) -> bool {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let value = format!("{}:{nanos}.{}", direction.as_str(), std::process::id());
        let encoded = base64(value.as_bytes());
        let sequence = format!("\u{1b}]1337;SetUserVar={name}={encoded}\u{7}");
        let Ok(mut descriptor) = OpenOptions::new()
            .append(true)
            .custom_flags(libc::O_NOCTTY)
            .open(tty)
        else {
            return false;
        };
        descriptor.write_all(sequence.as_bytes()).is_ok()
    }
}

impl Backend for SystemBackend {
    fn current_scope(&mut self) -> Option<Scope> {
        let tmux = self.environment.get("TMUX")?;
        let pane = self.environment.get("TMUX_PANE")?;
        if !valid_pane(pane) {
            return None;
        }
        let mut parts = tmux.rsplitn(3, ',');
        let _flags = parts.next()?;
        let _pid = parts.next()?;
        let socket = parts.next()?;
        if socket.is_empty() {
            return None;
        }
        Some(Scope {
            socket: socket.to_owned(),
            pane: pane.clone(),
            session: None,
        })
    }

    fn execute(&mut self, scope: &Scope, action: Action, direction: Direction) -> Outcome {
        if action == Action::PaneSelect {
            let (edge, flag) = match direction {
                Direction::Left => ("pane_at_left", "L"),
                Direction::Down => ("pane_at_bottom", "D"),
                Direction::Up => ("pane_at_top", "U"),
                Direction::Right => ("pane_at_right", "R"),
                _ => return Outcome::Error,
            };
            let select = shell_join(&[
                "select-pane".to_owned(),
                "-t".to_owned(),
                scope.pane.clone(),
                format!("-{flag}"),
            ]);
            let declined = shell_join(&[
                "display-message".to_owned(),
                "-p".to_owned(),
                DECLINED_MARKER.to_owned(),
            ]);
            let error = shell_join(&[
                "display-message".to_owned(),
                "-p".to_owned(),
                ERROR_MARKER.to_owned(),
            ]);
            let inner = shell_join(&[
                "if-shell".to_owned(),
                "-F".to_owned(),
                "-t".to_owned(),
                scope.pane.clone(),
                format!("#{{!=:#{{{edge}}},1}}"),
                select,
                declined,
            ]);
            return Self::outcome(self.tmux(
                scope,
                &[
                    "if-shell".to_owned(),
                    "-F".to_owned(),
                    "-t".to_owned(),
                    scope.target(),
                    "#{&&:#{>:#{window_active_clients},0},#{pane_active}}".to_owned(),
                    inner,
                    error,
                ],
            ));
        }

        let Some(session) = scope.session.as_ref() else {
            // Tab operations are session-relative. A pane alone is ambiguous
            // when its window is linked into multiple sessions.
            return Outcome::Error;
        };
        if action == Action::TabMove {
            return self.move_tab(scope, direction);
        }
        let command = if direction == Direction::Previous {
            "previous-window"
        } else {
            "next-window"
        };
        let owned = shell_join(&[
            "if-shell".to_owned(),
            "-F".to_owned(),
            "#{>:#{session_windows},1}".to_owned(),
            shell_join(&[command.to_owned(), "-t".to_owned(), session.clone()]),
            shell_join(&[
                "display-message".to_owned(),
                "-p".to_owned(),
                DECLINED_MARKER.to_owned(),
            ]),
        ]);
        Self::outcome(self.tmux(
            scope,
            &[
                "if-shell".to_owned(),
                "-F".to_owned(),
                "-t".to_owned(),
                scope.target(),
                "#{&&:#{window_active},#{pane_active}}".to_owned(),
                owned,
                shell_join(&[
                    "display-message".to_owned(),
                    "-p".to_owned(),
                    ERROR_MARKER.to_owned(),
                ]),
            ],
        ))
    }

    fn resolve_client(&mut self, scope: &Scope, started_at: u64) -> Option<Client> {
        let candidates = self
            .all_clients(&scope.socket)
            .into_iter()
            .filter(|client| {
                client.pane == scope.pane
                    && scope
                        .session
                        .as_ref()
                        .is_none_or(|session| &client.session == session)
            })
            .collect::<Vec<_>>();
        choose_client(&candidates, started_at, 2)
    }

    fn refresh_client(&mut self, client: &Client) -> Option<Client> {
        if client.socket.is_empty() {
            return Some(client.clone());
        }
        let matches = self
            .all_clients(&client.socket)
            .into_iter()
            .filter(|current| {
                current.pid == client.pid
                    && current.tty == client.tty
                    && (client.created == 0 || current.created == client.created)
            })
            .collect::<Vec<_>>();
        (matches.len() == 1).then(|| matches[0].clone())
    }

    fn inspect_scope(&mut self, scope: &Scope, started_at: u64) -> (Option<Scope>, Option<Client>) {
        let candidates = self
            .all_clients(&scope.socket)
            .into_iter()
            .filter(|client| {
                client.pane == scope.pane
                    && scope
                        .session
                        .as_ref()
                        .is_none_or(|session| &client.session == session)
            })
            .collect::<Vec<_>>();
        let selected = choose_client(&candidates, started_at, 2);
        let mut sessions = candidates
            .iter()
            .map(|client| client.session.as_str())
            .collect::<Vec<_>>();
        sessions.sort_unstable();
        sessions.dedup();
        let session = scope.session.clone().or_else(|| {
            if sessions.len() == 1 {
                Some(sessions[0].to_owned())
            } else {
                selected.as_ref().map(|client| client.session.clone())
            }
        });
        (
            session.map(|session| Scope {
                socket: scope.socket.clone(),
                pane: scope.pane.clone(),
                session: Some(session),
            }),
            selected,
        )
    }

    fn refresh_scope(&mut self, scope: &Scope) -> Option<Scope> {
        let target = if let Some(session) = &scope.session {
            session.clone()
        } else {
            let output = self.tmux(
                scope,
                &[
                    "display-message".to_owned(),
                    "-p".to_owned(),
                    "-t".to_owned(),
                    scope.pane.clone(),
                    "#{window_id}".to_owned(),
                ],
            )?;
            let window = String::from_utf8(output.stdout).ok()?.trim().to_owned();
            if !output.status.success() || !window.starts_with('@') {
                return None;
            }
            window
        };
        let output = self.tmux(
            scope,
            &[
                "display-message".to_owned(),
                "-p".to_owned(),
                "-t".to_owned(),
                target,
                "#{pane_id}".to_owned(),
            ],
        )?;
        let pane = String::from_utf8(output.stdout).ok()?.trim().to_owned();
        if !output.status.success() || !valid_pane(&pane) {
            return None;
        }
        Some(Scope {
            socket: scope.socket.clone(),
            pane,
            session: scope.session.clone(),
        })
    }

    fn validate_client(&mut self, client: &Client, started_at: u64) -> bool {
        let current = self.matching_clients(client);
        if client.exact {
            return current
                .iter()
                .any(|candidate| Self::same_route(candidate, client));
        }
        choose_client(&current, started_at, 2)
            .is_some_and(|selected| Self::same_route(&selected, client))
    }

    fn parent_scope(&mut self, client: &Client) -> Option<Scope> {
        let (socket, pane) = process_tmux_parent(client.pid)?;
        Some(Scope {
            socket,
            pane,
            session: None,
        })
    }

    fn relay(&mut self, client: &Client, action: Action, direction: Direction) -> Outcome {
        let Some(path) = process::environment(client.pid, "TERMNAV_PARENT_RELAY") else {
            return Outcome::Declined;
        };
        if path.is_empty() {
            return Outcome::Declined;
        }
        if !process::tty_matches(client.pid, &client.tty) {
            return Outcome::Error;
        }
        let scope = match action {
            Action::PaneSelect => "pane",
            Action::TabSelect => "window",
            Action::TabMove => "move",
        };
        let request = json!({
            "v": 2,
            "op": "navigate",
            "scope": scope,
            "direction": direction.as_str(),
            "nonce": new_nonce(),
        });
        let Ok(reply) = send(Path::new(&path), &request, Duration::from_secs(8)) else {
            return Outcome::Error;
        };
        match reply.get("result").and_then(Value::as_str) {
            Some("armed" | "emitted") => Outcome::Handled,
            Some("declined") => Outcome::Declined,
            _ => Outcome::Error,
        }
    }

    fn terminal(
        &mut self,
        client: Option<&Client>,
        action: Action,
        direction: Direction,
    ) -> Outcome {
        let (pid, tty, termtype) = client.map_or_else(
            || {
                (
                    std::process::id(),
                    "/dev/tty".to_owned(),
                    self.environment.get("TERM").cloned().unwrap_or_default(),
                )
            },
            |client| (client.pid, client.tty.clone(), client.termtype.clone()),
        );
        let term_program = self
            .terminal_environment(pid, "TERM_PROGRAM")
            .unwrap_or_default();
        if termtype.starts_with("xterm.js") || term_program == "vscode" {
            if action != Action::TabSelect {
                return Outcome::Handled;
            }
            let socket = process::environment(pid, "TERMNAV_VSCODE_SOCKET").unwrap_or_default();
            let token = process::environment(pid, "TERMNAV_VSCODE_TOKEN").unwrap_or_default();
            let handled = if !socket.is_empty() {
                self.vscode_socket(&socket, &token, direction)
            } else {
                self.terminal_environment(pid, "TERMNAV_VSCODE_FALLBACK_BACKEND")
                    .is_some_and(|backend| backend == "mcp")
                    && self.vscode_mcp(direction)
            };
            return if handled {
                Outcome::Handled
            } else {
                Outcome::Error
            };
        }

        if termtype.starts_with("tmux") || termtype.starts_with("screen") {
            return Outcome::Handled;
        }
        if action == Action::PaneSelect {
            return Outcome::Handled;
        }
        let name = if action == Action::TabSelect {
            "DOT_SWITCH_TAB"
        } else {
            "DOT_MOVE_TAB"
        };
        if Self::write_wezterm_var(&tty, name, direction) {
            Outcome::Handled
        } else {
            Outcome::Error
        }
    }
}

/// Parse tmux's constrained machine-readable client format.
#[must_use]
pub fn parse_clients(socket: &str, output: &str) -> Vec<Client> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split(FIELD_SEPARATOR).collect::<Vec<_>>();
            if fields.len() != 9 || !valid_pane(fields[5]) {
                return None;
            }
            Some(Client {
                activity: fields[0].parse().ok()?,
                pid: fields[1].parse().ok()?,
                tty: (!fields[2].is_empty()).then(|| fields[2].to_owned())?,
                termtype: fields[3].to_owned(),
                session: (!fields[4].is_empty()).then(|| fields[4].to_owned())?,
                pane: fields[5].to_owned(),
                focused: fields[6].split(',').any(|flag| flag == "focused"),
                control: fields[7] == "1",
                socket: socket.to_owned(),
                exact: false,
                created: fields[8].parse().ok()?,
            })
        })
        .collect()
}

/// Find the nearest complete parent tmux identity without a depth limit.
#[must_use]
pub fn process_tmux_parent(mut pid: u32) -> Option<(String, String)> {
    let mut visited = std::collections::HashSet::new();
    while pid > 1 && visited.insert(pid) {
        let tmux = process::environment(pid, "TMUX").unwrap_or_default();
        let pane = process::environment(pid, "TMUX_PANE").unwrap_or_default();
        let parts = tmux.rsplitn(3, ',').collect::<Vec<_>>();
        if parts.len() == 3 && !parts[2].is_empty() && valid_pane(&pane) {
            return Some((parts[2].to_owned(), pane));
        }
        pid = process::parent(pid)?;
    }
    None
}

fn valid_pane(value: &str) -> bool {
    value.strip_prefix('%').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn tmux_format(fields: &[&str]) -> String {
    fields
        .iter()
        .map(|field| format!("#{{{field}}}"))
        .collect::<Vec<_>>()
        .join("|")
}

fn shell_join(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|argument| {
            // tmux parses nested command strings with a shell-like grammar.
            // Always quoting is slightly longer but prevents IDs or paths from
            // acquiring syntax if a future tmux format expands unexpectedly.
            format!("'{}'", argument.replace('\'', "'\\''"))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn http_request(path: &str, body: &str, headers: &[(&str, &str)]) -> String {
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);
    request
}

fn http_success(stream: &mut impl ReadWrite, request: &[u8]) -> bool {
    http_response(stream, request).is_some_and(|response| {
        response
            .lines()
            .next()
            .is_some_and(|status| status.starts_with("HTTP/1.1 2"))
    })
}

fn http_response(stream: &mut impl ReadWrite, request: &[u8]) -> Option<String> {
    stream.write_all(request).ok()?;
    let _ = stream.shutdown_write();
    let mut response = Vec::new();
    stream.take(1_048_576).read_to_end(&mut response).ok()?;
    String::from_utf8(response).ok()
}

trait ReadWrite: Read + Write {
    fn shutdown_write(&self) -> std::io::Result<()>;
}

impl ReadWrite for UnixStream {
    fn shutdown_write(&self) -> std::io::Result<()> {
        self.shutdown(Shutdown::Write)
    }
}

impl ReadWrite for TcpStream {
    fn shutdown_write(&self) -> std::io::Result<()> {
        self.shutdown(Shutdown::Write)
    }
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{base64, parse_clients};

    #[test]
    fn client_parser_preserves_route_identity() {
        let clients = parse_clients(
            "/tmp/tmux.sock",
            "100|10|/dev/pts/10|xterm.js(6.1)|$1|%2|attached,focused,UTF-8|0|80\n",
        );

        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].pid, 10);
        assert!(clients[0].focused);
        assert!(!clients[0].control);
        assert_eq!(clients[0].created, 80);
    }

    #[test]
    fn base64_matches_terminal_protocol_examples() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }
}
