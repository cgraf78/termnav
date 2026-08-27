//! POSIX shell command encoding at explicit subprocess boundaries.

/// Quote one argument so a POSIX shell receives its bytes as one word.
#[must_use]
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Join an argument vector for APIs, such as tmux, that accept shell text.
#[must_use]
pub fn join(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|argument| quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Preserve literal text across tmux's format-expansion layer.
///
/// Commands such as `run-shell` expand tmux formats before invoking the user's
/// shell, even when a `#` appears inside shell quotes. Doubling the marker is
/// tmux's literal encoding and prevents paths from becoming format or command
/// substitutions before ordinary shell quoting gets a chance to protect them.
#[must_use]
pub fn escape_tmux_format(value: &str) -> String {
    value.replace('#', "##")
}

#[cfg(test)]
mod tests {
    use super::{escape_tmux_format, quote};

    #[test]
    fn quote_preserves_spaces_and_apostrophes() {
        assert_eq!(quote("a b's"), "'a b'\\''s'");
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn tmux_format_escape_blocks_interpolation_and_command_substitution() {
        assert_eq!(
            escape_tmux_format("/tmp/#{session_name}/#(printf unsafe)"),
            "/tmp/##{session_name}/##(printf unsafe)"
        );
    }
}
