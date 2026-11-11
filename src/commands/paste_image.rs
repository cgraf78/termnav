//! `termnav paste-image` command adapter.

use std::ffi::OsString;
use std::io::{self, Write};

use crate::paste_image::{receive, stage};

const HELP: &str = r#"usage:
  termnav paste-image stage
  termnav paste-image receive --target-pane PANE --client-name NAME
       [--client-pid PID --client-tty TTY] [--tmux-socket PATH]
"#;

/// Parse and execute paste-image commands.
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
    match arguments.first().map(String::as_str) {
        Some("-h" | "--help" | "help") => {
            stdout.write_all(HELP.as_bytes())?;
            Ok(0)
        }
        Some("stage") if arguments.len() == 1 => match stage::stage() {
            Ok(stage::Staged::Path(path)) => {
                writeln!(stdout, "{}", path.display())?;
                Ok(0)
            }
            Ok(stage::Staged::Declined(_)) => Ok(3),
            Err(error) => {
                writeln!(stderr, "termnav paste-image stage: {error}")?;
                Ok(1)
            }
        },
        Some("stage") => usage(stderr, "stage accepts no arguments"),
        Some("receive") => match parse_receive(&arguments[1..]) {
            Ok(options) => Ok(receive::run(&options, stdout)),
            Err(message) => usage(stderr, &message),
        },
        Some(command) => usage(stderr, &format!("unknown paste-image command: {command}")),
        None => usage(stderr, "a paste-image command is required"),
    }
}

fn parse_receive(arguments: &[String]) -> Result<receive::Options, String> {
    let mut options = receive::Options::default();
    let mut index = 0;
    while index < arguments.len() {
        let name = arguments[index].as_str();
        let Some(value) = arguments.get(index + 1) else {
            return Err(format!("{name} requires a value"));
        };
        match name {
            "--target-pane" if crate::browser::valid_pane(value) => {
                options.target_pane = value.clone();
            }
            "--target-pane" => return Err("target pane must be a tmux pane id (%N)".to_owned()),
            "--client-name" if !value.is_empty() => options.client_name = value.clone(),
            "--client-name" => return Err("client name must not be empty".to_owned()),
            "--client-pid" => {
                options.client_pid = Some(
                    value
                        .parse()
                        .ok()
                        .filter(|pid| *pid > 0)
                        .ok_or_else(|| "invalid client PID".to_owned())?,
                );
            }
            "--client-tty" if !value.is_empty() => options.client_tty = Some(value.clone()),
            "--client-tty" => return Err("client tty must not be empty".to_owned()),
            "--tmux-socket" if !value.is_empty() => options.socket = value.clone(),
            "--tmux-socket" => return Err("tmux socket must not be empty".to_owned()),
            _ => return Err(format!("unknown receive option: {name}")),
        }
        index += 2;
    }
    if options.target_pane.is_empty() || options.client_name.is_empty() {
        return Err("receive requires --target-pane and --client-name".to_owned());
    }
    if options.client_pid.is_some() != options.client_tty.is_some() {
        return Err("--client-pid and --client-tty must be given together".to_owned());
    }
    Ok(options)
}

fn usage(stderr: &mut dyn Write, message: &str) -> io::Result<i32> {
    writeln!(stderr, "termnav paste-image: {message}")?;
    stderr.write_all(HELP.as_bytes())?;
    Ok(2)
}

#[cfg(test)]
mod tests {
    use super::parse_receive;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn receive_requires_a_pane_and_a_client() {
        let options = parse_receive(&strings(&[
            "--target-pane",
            "%3",
            "--client-name",
            "/dev/pts/4",
            "--client-pid",
            "42",
            "--client-tty",
            "/dev/pts/4",
        ]))
        .unwrap();
        assert_eq!(options.target_pane, "%3");
        assert_eq!(options.client_pid, Some(42));
        for invalid in [
            &["--client-name", "c"][..],
            &["--target-pane", "%1"],
            &["--target-pane", "1", "--client-name", "c"],
            &["--target-pane", "%1", "--client-name", ""],
            &[
                "--target-pane",
                "%1",
                "--client-name",
                "c",
                "--client-pid",
                "4",
            ],
            &[
                "--target-pane",
                "%1",
                "--client-name",
                "c",
                "--client-pid",
                "x",
            ],
            &["--target-pane", "%1", "--client-name", "c", "--bogus", "x"],
            &["--target-pane"],
        ] {
            assert!(parse_receive(&strings(invalid)).is_err(), "{invalid:?}");
        }
    }
}
