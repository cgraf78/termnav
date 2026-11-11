//! Transfer grammar shared by the stage and receive endpoints.
//!
//! WezTerm types one transfer into the receiver popup:
//!
//! ```text
//! <id>:<decoded-length>:<sha256-hex>:<base64>!
//! ```
//!
//! `stage` writes everything after `<id>:` and before `!` to its staged file,
//! so WezTerm only prepends the receiver's id and appends the terminator.
//! [`Parser`] accepts that stream incrementally and rejects the first byte
//! that cannot belong to it: everything that reaches the receiver came from
//! a terminal input stream that other programs and the user can also write.

use sha2::{Digest, Sha256};

use super::{MAX_IMAGE_BYTES, PNG_SIGNATURE};

/// Encode one PNG as the staged text: `<length>:<sha256-hex>:<base64>`.
///
/// # Errors
///
/// Returns a reason when the bytes are not a PNG within [`MAX_IMAGE_BYTES`].
pub fn encode_staged(png: &[u8]) -> Result<String, String> {
    validate_png(png)?;
    let digest = Sha256::digest(png);
    let encoded = crate::terminal::base64(png);
    let mut staged = String::with_capacity(encoded.len() + 96);
    staged.push_str(&png.len().to_string());
    staged.push(':');
    staged.push_str(&super::hex(&digest));
    staged.push(':');
    staged.push_str(&encoded);
    Ok(staged)
}

/// Check the size cap and PNG signature of a complete image.
///
/// # Errors
///
/// Returns a short reason for an empty, oversized, or non-PNG payload.
pub fn validate_png(png: &[u8]) -> Result<(), String> {
    if png.len() > MAX_IMAGE_BYTES {
        return Err("image is larger than 16 MiB".to_owned());
    }
    if png.len() < PNG_SIGNATURE.len() || png[..PNG_SIGNATURE.len()] != PNG_SIGNATURE {
        return Err("clipboard data is not a PNG image".to_owned());
    }
    Ok(())
}

/// Why a transfer was rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Rejection {
    /// A byte that the grammar does not allow at this position. `offset`
    /// counts transfer bytes from the first id byte.
    InvalidByte {
        /// Zero-based position of the byte in the transfer.
        offset: u64,
        /// The rejected byte.
        byte: u8,
    },
    /// The declared length exceeds [`MAX_IMAGE_BYTES`] or cannot hold a PNG.
    Length,
    /// The decoded bytes do not start with the PNG signature.
    NotPng,
    /// The decoded bytes do not match the declared SHA-256 digest.
    Digest,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidByte { offset, .. } => {
                write!(formatter, "unexpected input at byte {offset}")
            }
            Self::Length => formatter.write_str("image is empty or larger than 16 MiB"),
            Self::NotPng => formatter.write_str("data is not a PNG image"),
            Self::Digest => formatter.write_str("checksum mismatch"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    /// Matching the expected id; the value counts bytes matched so far.
    Id(usize),
    /// Reading decimal length digits; the value counts digits read.
    Length(usize),
    /// Reading lowercase hex digest characters; the value counts them.
    Digest(usize),
    /// Reading base64 text.
    Data,
    /// The terminator was accepted.
    Complete,
}

/// Incremental, strict parser for one transfer.
#[derive(Debug)]
pub struct Parser {
    id: Vec<u8>,
    state: State,
    consumed: u64,
    length: usize,
    digest: [u8; 32],
    encoded_seen: usize,
    encoded_length: usize,
    quad: [u8; 4],
    quad_len: usize,
    image: Vec<u8>,
}

/// Progress after feeding one buffer.
#[derive(Debug, Eq, PartialEq)]
pub enum Fed {
    /// More input is needed.
    Partial,
    /// The terminator was the byte at this index of the fed buffer; the
    /// image is available from [`Parser::finish`].
    Complete(usize),
}

impl Parser {
    /// Start a parser for the transfer whose id is `id`.
    #[must_use]
    pub fn new(id: &str) -> Self {
        Self {
            id: id.as_bytes().to_vec(),
            state: State::Id(0),
            consumed: 0,
            length: 0,
            digest: [0; 32],
            encoded_seen: 0,
            encoded_length: 0,
            quad: [0; 4],
            quad_len: 0,
            image: Vec::new(),
        }
    }

    /// Transfer bytes accepted so far, counted from the first id byte.
    #[must_use]
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Whether the header, through the colon after the digest, was accepted.
    #[must_use]
    pub fn header_complete(&self) -> bool {
        matches!(self.state, State::Data | State::Complete)
    }

    /// Whether any transfer byte was accepted.
    #[must_use]
    pub fn started(&self) -> bool {
        self.consumed > 0
    }

    /// Declared decoded length, once the header is complete.
    #[must_use]
    pub fn declared_length(&self) -> usize {
        self.length
    }

    /// Base64 characters still expected before the terminator.
    #[must_use]
    pub fn remaining_encoded(&self) -> usize {
        self.encoded_length.saturating_sub(self.encoded_seen)
    }

    /// Total base64 characters expected, once the header is complete.
    #[must_use]
    pub fn encoded_length(&self) -> usize {
        self.encoded_length
    }

    /// Feed received bytes.
    ///
    /// # Errors
    ///
    /// Returns the first violation; the parser must not be fed again.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Fed, Rejection> {
        for (index, &byte) in bytes.iter().enumerate() {
            self.accept(byte)?;
            self.consumed += 1;
            if self.state == State::Complete {
                return Ok(Fed::Complete(index));
            }
        }
        Ok(Fed::Partial)
    }

    /// Return the validated image after [`Fed::Complete`].
    ///
    /// # Errors
    ///
    /// Returns [`Rejection::Digest`] when the digest does not match.
    pub fn finish(self) -> Result<Vec<u8>, Rejection> {
        debug_assert_eq!(self.state, State::Complete);
        if Sha256::digest(&self.image).as_slice() != self.digest {
            return Err(Rejection::Digest);
        }
        Ok(self.image)
    }

    fn invalid(&self, byte: u8) -> Rejection {
        Rejection::InvalidByte {
            offset: self.consumed,
            byte,
        }
    }

    fn accept(&mut self, byte: u8) -> Result<(), Rejection> {
        match self.state {
            State::Id(matched) if matched < self.id.len() => {
                if byte != self.id[matched] {
                    return Err(self.invalid(byte));
                }
                self.state = State::Id(matched + 1);
            }
            State::Id(_) => {
                if byte != b':' {
                    return Err(self.invalid(byte));
                }
                self.state = State::Length(0);
            }
            State::Length(digits) => match byte {
                // No sign, no leading zero, and no zero length: a PNG has at
                // least its signature.
                b'1'..=b'9' if digits == 0 => {
                    self.length = usize::from(byte - b'0');
                    self.state = State::Length(1);
                }
                b'0'..=b'9' if digits > 0 => {
                    self.length = self.length * 10 + usize::from(byte - b'0');
                    if self.length > MAX_IMAGE_BYTES {
                        return Err(Rejection::Length);
                    }
                    self.state = State::Length(digits + 1);
                }
                b':' if digits > 0 => {
                    if self.length < PNG_SIGNATURE.len() {
                        return Err(Rejection::Length);
                    }
                    self.encoded_length = self.length.div_ceil(3) * 4;
                    self.image.reserve_exact(self.length);
                    self.state = State::Digest(0);
                }
                _ => return Err(self.invalid(byte)),
            },
            State::Digest(count) if count < 64 => {
                let value = match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    _ => return Err(self.invalid(byte)),
                };
                let slot = &mut self.digest[count / 2];
                *slot = if count % 2 == 0 {
                    value << 4
                } else {
                    *slot | value
                };
                self.state = State::Digest(count + 1);
            }
            State::Digest(_) => {
                if byte != b':' {
                    return Err(self.invalid(byte));
                }
                self.state = State::Data;
            }
            State::Data if self.encoded_seen == self.encoded_length => {
                if byte != b'!' {
                    return Err(self.invalid(byte));
                }
                self.state = State::Complete;
            }
            State::Data => self.accept_encoded(byte)?,
            State::Complete => return Err(self.invalid(byte)),
        }
        Ok(())
    }

    fn accept_encoded(&mut self, byte: u8) -> Result<(), Rejection> {
        let padding = (3 - self.length % 3) % 3;
        let value = if self.encoded_seen >= self.encoded_length - padding {
            // Padding is mandatory exactly where the declared length ends.
            if byte != b'=' {
                return Err(self.invalid(byte));
            }
            0
        } else {
            match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return Err(self.invalid(byte)),
            }
        };
        self.quad[self.quad_len] = value;
        self.quad_len += 1;
        self.encoded_seen += 1;
        if self.quad_len == 4 {
            let [a, b, c, d] = self.quad;
            let decoded = [(a << 2) | (b >> 4), (b << 4) | (c >> 2), (c << 6) | d];
            let take = (self.length - self.image.len()).min(3);
            self.image.extend_from_slice(&decoded[..take]);
            self.quad_len = 0;
            if self.image.len() >= PNG_SIGNATURE.len()
                && self.image.len() - take < PNG_SIGNATURE.len()
                && self.image[..PNG_SIGNATURE.len()] != PNG_SIGNATURE
            {
                return Err(Rejection::NotPng);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Fed, Parser, Rejection, encode_staged, validate_png};
    use crate::paste_image::{MAX_IMAGE_BYTES, PNG_SIGNATURE};

    fn png(extra: &[u8]) -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        bytes.extend_from_slice(extra);
        bytes
    }

    fn transfer(id: &str, image: &[u8]) -> Vec<u8> {
        format!("{id}:{}!", encode_staged(image).unwrap()).into_bytes()
    }

    fn parse(id: &str, bytes: &[u8]) -> Result<Vec<u8>, Rejection> {
        let mut parser = Parser::new(id);
        match parser.feed(bytes)? {
            Fed::Complete(index) => {
                assert_eq!(index, bytes.len() - 1, "terminator ends the input");
                parser.finish()
            }
            Fed::Partial => panic!("transfer incomplete"),
        }
    }

    #[test]
    fn round_trips_every_padding_length() {
        for extra in 0..6 {
            let image = png(&vec![0xa5; extra]);
            assert_eq!(parse("abc123", &transfer("abc123", &image)).unwrap(), image);
        }
    }

    #[test]
    fn accepts_input_split_at_every_boundary() {
        let image = png(b"\x00\x01\x02 split boundaries");
        let bytes = transfer("feed", &image);
        for split in 0..bytes.len() {
            let mut parser = Parser::new("feed");
            assert_eq!(parser.feed(&bytes[..split]).unwrap(), Fed::Partial);
            assert_eq!(
                parser.feed(&bytes[split..]).unwrap(),
                Fed::Complete(bytes.len() - split - 1)
            );
            assert_eq!(parser.consumed(), bytes.len() as u64);
            assert_eq!(parser.finish().unwrap(), image);
        }
    }

    #[test]
    fn staged_text_has_the_documented_header() {
        let image = png(b"x");
        let staged = encode_staged(&image).unwrap();
        let mut fields = staged.splitn(3, ':');
        assert_eq!(fields.next(), Some("9"));
        assert_eq!(fields.next().unwrap().len(), 64);
        assert_eq!(fields.next(), Some("iVBORw0KGgp4"));
        assert!(!staged.contains('\n'));
    }

    #[test]
    fn rejects_the_first_byte_outside_the_grammar() {
        let image = png(b"abc");
        let good = transfer("id42", &image);
        let header_end = good.iter().rposition(|&byte| byte == b':').unwrap();
        // (mutated position, replacement byte)
        let cases = [
            (0, b'x'),               // wrong id
            (4, b';'),               // missing id separator
            (5, b'0'),               // leading zero
            (5, b'-'),               // sign
            (7, b'x'),               // junk where the length ends
            (8, b'A'),               // uppercase digest
            (header_end + 1, b'\n'), // newline in base64
            (header_end + 1, b'-'),  // URL-safe alphabet
            (good.len() - 1, b'?'),  // wrong terminator
        ];
        for (position, replacement) in cases {
            let mut bytes = good.clone();
            if bytes[position] == replacement {
                continue;
            }
            bytes[position] = replacement;
            let mut parser = Parser::new("id42");
            match parser.feed(&bytes) {
                Err(Rejection::InvalidByte { offset, byte }) => {
                    assert_eq!(offset, position as u64, "position {position}");
                    assert_eq!(byte, replacement);
                }
                other => panic!("position {position}: {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_padding_in_the_wrong_place_and_extra_data() {
        let image = png(b"a"); // 9 bytes: no padding
        let good = transfer("p", &image);
        let mut early_padding = good.clone();
        let end = early_padding.len() - 2;
        early_padding[end] = b'=';
        assert!(matches!(
            Parser::new("p").feed(&early_padding),
            Err(Rejection::InvalidByte { .. })
        ));
        let mut extra = good[..good.len() - 1].to_vec();
        extra.extend_from_slice(b"AAAA!");
        assert!(matches!(
            Parser::new("p").feed(&extra),
            Err(Rejection::InvalidByte { .. })
        ));

        let padded = transfer("p", &png(b"ab")); // 10 bytes: two padding chars
        let mut missing_padding = padded.clone();
        let end = missing_padding.len() - 2;
        missing_padding[end] = b'A';
        assert!(matches!(
            Parser::new("p").feed(&missing_padding),
            Err(Rejection::InvalidByte { .. })
        ));
    }

    #[test]
    fn truncated_transfer_is_rejected_at_the_early_terminator() {
        let good = transfer("t", &png(&[7; 30]));
        let mut truncated = good[..good.len() - 9].to_vec();
        truncated.push(b'!');
        let offset = truncated.len() as u64 - 1;
        assert_eq!(
            Parser::new("t").feed(&truncated),
            Err(Rejection::InvalidByte { offset, byte: b'!' })
        );
    }

    #[test]
    fn rejects_digest_mismatch_only_at_the_end() {
        let image = png(b"digest");
        let mut bytes = transfer("d", &image);
        // Flip one digest character to another valid hex digit.
        bytes[5] = if bytes[5] == b'0' { b'1' } else { b'0' };
        let mut parser = Parser::new("d");
        assert!(matches!(parser.feed(&bytes), Ok(Fed::Complete(_))));
        assert_eq!(parser.finish(), Err(Rejection::Digest));
    }

    #[test]
    fn rejects_non_png_payloads_as_soon_as_the_signature_is_decoded() {
        let staged = {
            let fake = b"GIF89a-not-a-png";
            let digest = {
                use sha2::Digest;
                crate::paste_image::hex(&sha2::Sha256::digest(fake))
            };
            format!(
                "n:{}:{digest}:{}!",
                fake.len(),
                crate::terminal::base64(fake)
            )
        };
        let mut parser = Parser::new("n");
        assert_eq!(parser.feed(staged.as_bytes()), Err(Rejection::NotPng));
    }

    #[test]
    fn enforces_the_size_cap_and_minimum_length_in_the_header() {
        let over = format!("c:{}:", MAX_IMAGE_BYTES + 1);
        assert_eq!(
            Parser::new("c").feed(over.as_bytes()),
            Err(Rejection::Length)
        );
        let at_cap = format!("c:{}:", MAX_IMAGE_BYTES);
        assert_eq!(
            Parser::new("c").feed(at_cap.as_bytes()),
            Ok(Fed::Partial),
            "exactly 16 MiB is allowed"
        );
        assert_eq!(Parser::new("c").feed(b"c:7:"), Err(Rejection::Length));
        assert!(matches!(
            Parser::new("c").feed(b"c:0"),
            Err(Rejection::InvalidByte { offset: 2, .. })
        ));
    }

    #[test]
    fn validates_staged_images() {
        assert!(validate_png(&png(b"")).is_ok());
        assert!(validate_png(b"\x89PNG").is_err());
        assert!(validate_png(b"not a png at all").is_err());
        assert!(validate_png(&png(&vec![0; MAX_IMAGE_BYTES])).is_err());
        assert!(encode_staged(b"plain text").is_err());
    }

    #[test]
    fn reports_header_progress() {
        let mut parser = Parser::new("h");
        assert!(!parser.started());
        parser.feed(b"h:10:").unwrap();
        assert!(parser.started());
        assert!(!parser.header_complete());
        parser.feed(&[b'0'; 64]).unwrap();
        parser.feed(b":").unwrap();
        assert!(parser.header_complete());
        assert_eq!(parser.declared_length(), 10);
        assert_eq!(parser.encoded_length(), 16);
        assert_eq!(parser.remaining_encoded(), 16);
    }
}
