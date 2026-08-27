//! Top-level command parsing and dispatch.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;

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

/// Translate fixed-name integrations and temporary rollout aliases to the
/// authoritative CLI grammar.
///
/// Dispatching by `argv[0]` keeps one compiled artifact while satisfying tools
/// such as ripgrep that can name an executable but cannot provide arguments.
/// Most aliases are removed after the consumer migration; `nvim-link-host`
/// remains because that external interface is permanently argument-less.
#[must_use]
pub fn normalize_argv(
    program: &OsString,
    arguments: impl IntoIterator<Item = OsString>,
) -> Vec<OsString> {
    let name = Path::new(program)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("termnav");
    let prefix: &[&str] = match name {
        "termnav-navigate" => &["navigate"],
        "termnav-relay" => &["relay"],
        "termnav-tmux-context" => &["tmux", "context"],
        "termnav-tmux-focus" => &["tmux", "focus"],
        "nvim-link-host" => &["nvim", "link-host"],
        "nvim-ssh-control-open" => &["nvim", "ssh-open"],
        "nvim-tmux-open" => &["nvim", "open"],
        "tmux-follow-click" => &["tmux", "follow-click"],
        "vscode-nvim-focus" => &["vscode", "focus"],
        "eza-nvim-links" => &["eza"],
        _ => &[],
    };
    prefix.iter().map(OsString::from).chain(arguments).collect()
}

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
        "relay" => crate::commands::relay::run(&arguments[1..], stdout, stderr),
        "ssh" => crate::commands::ssh::run(&arguments[1..]),
        "nvim" => crate::commands::nvim::run(&arguments[1..], stdout, stderr),
        "tmux" => crate::commands::tmux::run(&arguments[1..], stdout, stderr),
        "eza" => crate::commands::eza::run(&arguments[1..], stdout, stderr),
        "vscode" => {
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
    use std::ffi::OsString;

    use super::{normalize_argv, run};

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

    #[test]
    fn fixed_name_host_helper_maps_to_the_unified_command() {
        assert_eq!(
            normalize_argv(&OsString::from("/prefix/bin/nvim-link-host"), []),
            ["nvim", "link-host"].map(OsString::from)
        );
    }
}
