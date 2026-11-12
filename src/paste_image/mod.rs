//! In-band clipboard image paste from WezTerm into the innermost tmux pane.
//!
//! The desktop never serves its clipboard. After the user presses Ctrl-V on a
//! tmux pane, WezTerm runs [`stage`] locally and sends [`TRIGGER`] into that
//! pane. The innermost tmux layer opens a popup running [`receive`], which
//! announces itself through the exact attached client's outward user-variable
//! path ([`USER_VAR`]); only then does WezTerm type the staged image into the
//! popup, with acknowledgement-based flow control. The receiver validates the
//! stream strictly, writes the PNG privately, and bracket-pastes its path into
//! the pane the popup was opened for.
//!
//! The WezTerm endpoint lives in `lib/termnav/wezterm/paste-image.lua`; the
//! grammar and limits it relies on are defined here and documented in
//! `docs/reference.md`.

pub mod protocol;
pub mod receive;
pub mod stage;

/// WezTerm user variable that carries receiver announcements.
pub const USER_VAR: &str = "TERMNAV_PASTE_IMAGE";

/// Private key sequence WezTerm sends to ask tmux for a receiver.
///
/// It is the CSI-u form of an otherwise unused code point, which tmux matches
/// as `user-keys[14]` when that entry is set to `\e[777010u`.
pub const TRIGGER: &str = "\u{1b}[777010u";

/// Largest decoded PNG accepted by either endpoint.
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// Receiver acknowledgement spacing, in transfer bytes.
pub const ACK_INTERVAL: u64 = 128 * 1024;

/// The eight-byte signature every PNG file starts with.
pub const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// Return a lowercase hexadecimal identifier of `bytes` random bytes.
///
/// Identifiers name one transfer and one staged file. They prevent a
/// receiver from accepting a stream meant for another popup and staged files
/// from colliding; they are not secrets, so the time-and-PID fallback used in
/// sandboxes without `/dev/urandom` is acceptable.
pub(crate) fn random_hex(bytes: usize) -> String {
    use std::io::Read;

    let mut buffer = vec![0_u8; bytes];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut buffer))
        .is_err()
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut mixed = nanos ^ (u128::from(std::process::id()) << 64);
        for byte in &mut buffer {
            *byte = mixed as u8;
            mixed = mixed.rotate_right(8) ^ 0x9e37_79b9_7f4a_7c15;
        }
    }
    hex(&buffer)
}

/// Encode bytes as lowercase hexadecimal.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    bytes.iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    })
}

#[cfg(test)]
mod tests {
    use super::{hex, random_hex};

    #[test]
    fn identifiers_are_lowercase_hex_of_the_requested_width() {
        let first = random_hex(8);
        assert_eq!(first.len(), 16);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_ne!(first, random_hex(8));
        assert_eq!(hex(&[0, 0xab, 0x10]), "00ab10");
    }
}
