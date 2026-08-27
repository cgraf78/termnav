//! Top-level command parsing and dispatch.

use std::ffi::OsString;
use std::io::{self, Write};

const HELP: &str = "usage: termnav <command> [arguments]\n\
\n\
commands:\n\
  navigate  route pane and tab navigation\n\
  relay     communicate across terminal boundaries\n\
  ssh       run an SSH session with Termnav relay transport\n\
  tmux      manage tmux context, focus, and click routing\n\
  nvim      open and route Neovim targets\n\
  vscode    publish VS Code focus\n\
  eza       render terminal-aware directory links\n\
  version   print the embedded build identity\n";

/// Run the CLI and return its process exit status.
pub fn run<I, S>(arguments: I, stdout: &mut dyn Write, stderr: &mut dyn Write) -> io::Result<i32>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments = arguments
        .into_iter()
        .map(Into::into)
        .collect::<Vec<OsString>>();
    let Some(command) = arguments.first().and_then(|value| value.to_str()) else {
        stderr.write_all(b"termnav: a command is required\n")?;
        stderr.write_all(HELP.as_bytes())?;
        return Ok(2);
    };

    match command {
        "-h" | "--help" | "help" => {
            stdout.write_all(HELP.as_bytes())?;
            Ok(0)
        }
        "version" | "--version" => {
            writeln!(stdout, "{}", crate::version::line())?;
            Ok(0)
        }
        "navigate" => crate::commands::navigate::run(&arguments[1..], stdout, stderr),
        "relay" | "ssh" | "tmux" | "nvim" | "vscode" | "eza" => {
            writeln!(stderr, "termnav: {command} is not implemented yet")?;
            Ok(2)
        }
        _ => {
            writeln!(stderr, "termnav: unknown command: {command}")?;
            Ok(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run;

    #[test]
    fn missing_command_is_usage_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        assert_eq!(
            run(Vec::<String>::new(), &mut stdout, &mut stderr).unwrap(),
            2
        );
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
    }
}
