//! Bounded image decoding and fitting for terminal previews.
//!
//! Previews run next to untrusted clipboard data, so decoding is capped on
//! the header before any pixel buffer exists and memory stays proportional
//! to the thumbnail rather than the source image.

mod decode;

pub use decode::{BACKGROUND, DecodeError, Thumbnail, decode, fit};
