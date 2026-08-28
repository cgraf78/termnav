//! Top-level command parsing and dispatch.

use std::ffi::OsString;
use std::io::{self, Write};

const HELP: &str = "usage: termnav <command> [arguments]\n\
\n\
commands:\n\
  navigate  route pane and tab navigation\n\
  relay     communicate across terminal boundaries\n\
  ssh       run an SSH session with Termnav relay transport\n\
  link-host print the host represented by the current terminal context\n\
  tmux      manage tmux context, focus, and click routing\n\
  nvim      open and route Neovim targets\n\
  vscode    publish VS Code focus\n\
  eza       render terminal-aware directory links\n\
  version   print the embedded build identity\n";

/// Run the CLI and return its process exit status.
pub fn run<I, S>(
    arguments: I,
    stdout: &mut dyn Write,
    stderr: &mut (dyn Write + Send),
) -> io::Result<i32>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments = arguments
        .into_iter()
        .map(Into::into)
        .collect::<Vec<OsString>>();
    let Some(first) = arguments.first() else {
        stderr.write_all(b"termnav: a command is required\n")?;
        stderr.write_all(HELP.as_bytes())?;
        return Ok(2);
    };
    let Some(command) = first.to_str() else {
        stderr.write_all(b"termnav: command must be valid UTF-8\n")?;
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
        "relay" => crate::commands::relay::run(&arguments[1..], stdout, stderr),
        "ssh" => crate::commands::ssh::run(&arguments[1..]),
        // Link identity belongs to the terminal context, not to any one link
        // consumer. Ripgrep, eza, and editor routing all share this policy.
        "link-host" if arguments.len() == 1 => {
            writeln!(stdout, "{}", crate::links::host())?;
            Ok(0)
        }
        "link-host" => {
            writeln!(stderr, "termnav link-host: no arguments are accepted")?;
            Ok(2)
        }
        "nvim" => crate::commands::nvim::run(&arguments[1..], stdout, stderr),
        "tmux" => crate::commands::tmux::run(&arguments[1..], stdout, stderr),
        "eza" => crate::commands::eza::run(&arguments[1..], stdout, stderr),
        "vscode" => crate::commands::vscode::run(&arguments[1..], stdout, stderr),
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
    fn missing_command_is_a_usage_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        assert_eq!(
            run(Vec::<String>::new(), &mut stdout, &mut stderr).unwrap(),
            2
        );
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_command_is_not_mistaken_for_ripgrep_query() {
        use std::os::unix::ffi::OsStringExt;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let command = std::ffi::OsString::from_vec(vec![0xff]);

        assert_eq!(run([command], &mut stdout, &mut stderr).unwrap(), 2);
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
    }
}
