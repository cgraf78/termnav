//! `termnav navigate` command adapter.

use std::ffi::OsString;
use std::io::{self, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::navigation::{Action, Client, Direction, Navigator, Scope, SystemBackend};

const CONTINUATION_TTL: Duration = Duration::from_millis(250);

const HELP: &str = "usage: termnav navigate [OPTIONS] ACTION DIRECTION\n\
\n\
actions: pane-select, tab-select, tab-move\n\
\n\
options:\n\
  --parent             start outside a tmux scope that already declined\n\
  --client-pid PID     exact source client process ID\n\
  --client-tty TTY     exact source client tty\n\
  --client-created N   exact source client creation time\n\
  --client-termtype T  exact source client terminal type\n\
  --source-socket PATH exact source tmux socket\n\
  --source-pane ID     exact source tmux pane\n\
  --source-session ID  optional exact source tmux session\n\
  --emit-continuation  print bounded state for one ordered successor\n\
  --continuation JSON  resume from revalidated prior routing state\n";

#[derive(Default)]
struct Options {
    parent: bool,
    client_pid: Option<u32>,
    client_tty: Option<String>,
    client_created: Option<u64>,
    client_termtype: Option<String>,
    source_socket: Option<String>,
    source_pane: Option<String>,
    source_session: Option<String>,
    emit_continuation: bool,
    continuation: Option<String>,
    positional: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct Continuation {
    version: u8,
    expires_at_monotonic_ms: u64,
    client: Option<Client>,
    scope: Option<Scope>,
}

/// Parse and execute one navigation request.
pub fn run(
    arguments: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> io::Result<i32> {
    let arguments = match arguments
        .iter()
        .map(|value| value.clone().into_string())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(arguments) => arguments,
        Err(_) => return usage(stderr, "arguments must be valid UTF-8"),
    };
    if arguments
        .iter()
        .any(|argument| argument == "-h" || argument == "--help")
    {
        stdout.write_all(HELP.as_bytes())?;
        return Ok(0);
    }
    let options = match parse(&arguments) {
        Ok(options) => options,
        Err(message) => return usage(stderr, &message),
    };
    if options.positional.len() != 2 {
        return usage(stderr, "action and direction are required");
    }
    let action = match Action::parse(&options.positional[0]) {
        Ok(action) => action,
        Err(message) => return usage(stderr, &message),
    };
    let direction = match Direction::parse(action, &options.positional[1]) {
        Ok(direction) => direction,
        Err(message) => return usage(stderr, &message),
    };
    let exact_client = match exact_client(&options) {
        Ok(client) => client,
        Err(message) => return usage(stderr, message),
    };
    if options.continuation.is_some() && (options.parent || exact_client.is_some()) {
        return usage(
            stderr,
            "--continuation cannot be combined with --parent or exact client options",
        );
    }

    let continuation = match options.continuation.as_deref() {
        Some(value) => match serde_json::from_str::<Continuation>(value) {
            Ok(state)
                if state.version == 1 && state.expires_at_monotonic_ms >= monotonic_millis() =>
            {
                Some(state)
            }
            Ok(_) => None,
            Err(_) => return usage(stderr, "--continuation requires valid Termnav state"),
        },
        None => None,
    };
    let (continuing_client, continuing_scope) = continuation
        .map(|state| (state.client, state.scope))
        .unwrap_or_default();

    let mut backend = SystemBackend::from_current_environment();
    let mut navigator = Navigator::new(&mut backend, now_seconds);
    let result = navigator.route(
        action,
        direction,
        !options.parent,
        exact_client,
        continuing_client,
        continuing_scope,
    );
    if options.emit_continuation && result.outcome == crate::navigation::Outcome::Handled {
        let continuation = Continuation {
            version: 1,
            // Continuations bridge adjacent one-shot jobs on one running host.
            // A monotonic deadline cannot be extended by NTP or a manual wall
            // clock correction and therefore preserves the intended short
            // gesture-burst lifetime.
            expires_at_monotonic_ms: monotonic_millis()
                .saturating_add(CONTINUATION_TTL.as_millis() as u64),
            client: result.client,
            scope: result.scope,
        };
        serde_json::to_writer(&mut *stdout, &continuation)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        writeln!(stdout)?;
    }
    Ok(result.outcome as i32)
}

fn parse(arguments: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "--parent" => {
                options.parent = true;
                index += 1;
            }
            "--emit-continuation" => {
                options.emit_continuation = true;
                index += 1;
            }
            "--continuation" => {
                options.continuation = Some(value(arguments, &mut index, argument)?.to_owned());
            }
            "--client-pid" => {
                options.client_pid = Some(
                    value(arguments, &mut index, argument)?
                        .parse()
                        .map_err(|_| "--client-pid requires a non-negative integer".to_owned())?,
                );
            }
            "--client-created" => {
                options.client_created = Some(
                    value(arguments, &mut index, argument)?
                        .parse()
                        .map_err(|_| {
                            "--client-created requires a non-negative integer".to_owned()
                        })?,
                );
            }
            "--client-tty" => {
                options.client_tty = Some(value(arguments, &mut index, argument)?.to_owned());
            }
            "--client-termtype" => {
                options.client_termtype = Some(value(arguments, &mut index, argument)?.to_owned());
            }
            "--source-socket" => {
                options.source_socket = Some(value(arguments, &mut index, argument)?.to_owned());
            }
            "--source-pane" => {
                options.source_pane = Some(value(arguments, &mut index, argument)?.to_owned());
            }
            "--source-session" => {
                options.source_session = Some(value(arguments, &mut index, argument)?.to_owned());
            }
            "--" => {
                options
                    .positional
                    .extend_from_slice(&arguments[index + 1..]);
                break;
            }
            value if value.starts_with('-') => return Err(format!("unknown option: {value}")),
            _ => {
                options.positional.push(argument.clone());
                index += 1;
            }
        }
    }
    Ok(options)
}

fn value<'a>(arguments: &'a [String], index: &mut usize, option: &str) -> Result<&'a str, String> {
    let Some(value) = arguments.get(*index + 1) else {
        return Err(format!("{option} requires a value"));
    };
    *index += 2;
    Ok(value)
}

fn exact_client(options: &Options) -> Result<Option<Client>, &'static str> {
    let identity_present = [
        options.client_pid.is_some(),
        options.client_tty.is_some(),
        options.client_created.is_some(),
        options.client_termtype.is_some(),
        options.source_socket.is_some(),
        options.source_pane.is_some(),
    ];
    if !options.parent {
        if identity_present.into_iter().any(|present| present) {
            return Err("exact client options require --parent");
        }
        return Ok(None);
    }
    if identity_present.into_iter().any(|present| !present) {
        return Err(
            "--client-pid, --client-tty, --client-created, --client-termtype, \
             --source-socket, and --source-pane are required with --parent",
        );
    }

    Ok(Some(Client {
        activity: 0,
        pid: options.client_pid.expect("validated above"),
        tty: options.client_tty.clone().expect("validated above"),
        termtype: options.client_termtype.clone().expect("validated above"),
        session: options.source_session.clone().unwrap_or_default(),
        pane: options.source_pane.clone().expect("validated above"),
        focused: false,
        control: false,
        socket: options.source_socket.clone().expect("validated above"),
        exact: true,
        created: options.client_created.expect("validated above"),
    }))
}

fn usage(stderr: &mut dyn Write, message: &str) -> io::Result<i32> {
    writeln!(stderr, "termnav navigate: {message}")?;
    stderr.write_all(HELP.as_bytes())?;
    Ok(2)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn monotonic_millis() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return 0;
    }
    (time.tv_sec as u64)
        .saturating_mul(1_000)
        .saturating_add((time.tv_nsec as u64) / 1_000_000)
}
