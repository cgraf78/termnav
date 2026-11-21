//! Bounded image decoding and fitting for terminal previews.
//!
//! Previews run next to untrusted clipboard data, so decoding is capped on
//! the header before any pixel buffer exists and memory stays proportional
//! to the thumbnail rather than the source image.

mod blocks;
mod decode;

pub use blocks::blocks;
pub use decode::{BACKGROUND, DecodeError, Thumbnail, decode, fit};
