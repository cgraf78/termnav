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

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quote_preserves_spaces_and_apostrophes() {
        assert_eq!(quote("a b's"), "'a b'\\''s'");
        assert_eq!(quote(""), "''");
    }
}
