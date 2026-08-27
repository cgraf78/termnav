//! Terminal escape construction shared by navigation and tmux context hooks.

/// Framing required to cross an immediate tmux layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TmuxMode {
    /// Write the terminal escape directly.
    Raw,
    /// Wrap the escape in one tmux passthrough frame.
    Passthrough,
}

/// Build one WezTerm `SetUserVar` escape with explicit tmux framing.
#[must_use]
pub fn user_var(name: &str, value: &str, mode: TmuxMode) -> Vec<u8> {
    let raw = format!(
        "\u{1b}]1337;SetUserVar={name}={}\u{7}",
        base64(value.as_bytes())
    );
    match mode {
        TmuxMode::Raw => raw.into_bytes(),
        TmuxMode::Passthrough => {
            // tmux passthrough escapes embedded ESC bytes by doubling them,
            // then wraps the complete sequence in DCS `tmux; ... ST`.
            let escaped = raw.replace('\u{1b}', "\u{1b}\u{1b}");
            format!("\u{1b}Ptmux;{escaped}\u{1b}\\").into_bytes()
        }
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
    use super::{TmuxMode, base64, user_var};

    #[test]
    fn base64_matches_terminal_protocol_examples() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"ab"), "YWI=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }

    #[test]
    fn tmux_passthrough_doubles_inner_escape_bytes() {
        let sequence = user_var("TERMNAV_TMUX", "true", TmuxMode::Passthrough);

        assert!(sequence.starts_with(b"\x1bPtmux;\x1b\x1b]1337;"));
        assert!(sequence.ends_with(b"\x1b\\"));
    }
}
