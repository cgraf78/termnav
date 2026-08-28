//! Top-level command parsing and dispatch.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;

const HELP: &str = r#"usage: termnav <command> [arguments]

commands:
  navigate   route pane and tab navigation
  relay      communicate across terminal boundaries
  ssh        run an SSH session with Termnav relay transport
  link-host  print the host represented by the current terminal context
  tmux       manage tmux context, focus, and click routing
  nvim       open and route Neovim targets
  vscode     publish VS Code focus
  eza        render terminal-aware directory links
  asset-path print the absolute path of an installed runtime asset
  version    print the embedded build identity
"#;

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
        "version" | "--version" if arguments.len() == 1 => {
            writeln!(stdout, "{}", crate::version::line())?;
            Ok(0)
        }
        "version" | "--version" => {
            writeln!(stderr, "usage: termnav version")?;
            Ok(2)
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
        "link-host"
            if arguments.len() == 2
                && matches!(arguments[1].to_str(), Some("-h" | "--help" | "help")) =>
        {
            writeln!(stdout, "usage: termnav link-host")?;
            Ok(0)
        }
        "link-host" => {
            writeln!(stderr, "termnav link-host: no arguments are accepted")?;
            Ok(2)
        }
        "nvim" => crate::commands::nvim::run(&arguments[1..], stdout, stderr),
        "tmux" => crate::commands::tmux::run(&arguments[1..], stdout, stderr),
        "eza" => crate::commands::eza::run(&arguments[1..], stdout, stderr),
        "asset-path"
            if arguments.len() == 2
                && matches!(arguments[1].to_str(), Some("-h" | "--help" | "help")) =>
        {
            writeln!(stdout, "usage: termnav asset-path RELATIVE_PATH")?;
            Ok(0)
        }
        "asset-path" if arguments.len() == 2 => match arguments[1].to_str() {
            Some(relative) => match crate::assets::resolve(Path::new(relative)) {
                Ok(asset) => {
                    writeln!(stdout, "{}", asset.display())?;
                    Ok(0)
                }
                Err(error) => {
                    writeln!(stderr, "termnav asset-path: {error}")?;
                    Ok(1)
                }
            },
            None => {
                writeln!(stderr, "termnav asset-path: path must be valid UTF-8")?;
                Ok(2)
            }
        },
        "asset-path" => {
            writeln!(stderr, "usage: termnav asset-path RELATIVE_PATH")?;
            Ok(2)
        }
        "vscode" => crate::commands::vscode::run(&arguments[1..], stdout, stderr),
        _ => {
            writeln!(stderr, "termnav: unknown command: {command}")?;
            Ok(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::rc::Rc;

    use super::run;

    struct ProcessLocalWriter {
        bytes: Vec<u8>,
        _not_send: Rc<()>,
    }

    impl Write for ProcessLocalWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

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

    #[test]
    fn public_cli_accepts_a_process_local_error_writer() {
        // Only the eza adapter needs concurrent pipe handling. Keep that
        // implementation detail from imposing Send on embedders of the public
        // command dispatcher, whose writers may intentionally contain Rc state.
        let mut stdout = Vec::new();
        let mut stderr = ProcessLocalWriter {
            bytes: Vec::new(),
            _not_send: Rc::new(()),
        };

        assert_eq!(
            run(Vec::<String>::new(), &mut stdout, &mut stderr).unwrap(),
            2
        );
        assert!(!stderr.bytes.is_empty());
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
