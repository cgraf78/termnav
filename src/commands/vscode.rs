//! `termnav vscode` command adapter.

use std::ffi::OsString;
use std::io::{self, Write};

use crate::vscode::{Operation, PublishOutcome, Update};

const HELP: &str = "usage: termnav vscode focus claim|release SOURCE CYCLE SEQUENCE OBSERVED\n";

/// Parse and execute VS Code integration commands.
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
    if matches!(
        arguments.first().map(String::as_str),
        Some("-h" | "--help" | "help")
    ) {
        stdout.write_all(HELP.as_bytes())?;
        return Ok(0);
    }
    let [command, operation, source, cycle, sequence, observed] = arguments.as_slice() else {
        return usage(
            stderr,
            "focus requires an operation and ordered update fields",
        );
    };
    if command != "focus" {
        return usage(stderr, &format!("unknown VS Code command: {command}"));
    }
    let operation = match operation.as_str() {
        "claim" => Operation::Claim,
        "release" => Operation::Release,
        _ => return usage(stderr, &format!("invalid operation: {operation}")),
    };
    let update = match Update::new(operation, source, cycle, sequence, observed) {
        Ok(update) => update,
        Err(message) => return usage(stderr, message),
    };
    Ok(match crate::vscode::publish(&update) {
        PublishOutcome::Posted => 0,
        PublishOutcome::Failed => 1,
        PublishOutcome::Unavailable => 10,
    })
}

fn usage(stderr: &mut dyn Write, message: &str) -> io::Result<i32> {
    writeln!(stderr, "termnav vscode: {message}")?;
    stderr.write_all(HELP.as_bytes())?;
    Ok(2)
}
