use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};

use png::{ColorType, DecodeOptions, Decoder, DecodingError, Limits, Transformations};

/// Background that transparent pixels are composited onto.
///
/// It matches a dark terminal so transparent regions blend in instead of
/// showing whatever RGB the encoder left under a zero alpha.
pub const BACKGROUND: [u8; 3] = [0x1e, 0x1e, 0x1e];

/// Longest accepted image side in pixels.
const MAX_SIDE: u32 = 16384;

/// Largest accepted pixel count, which bounds the interlaced buffer.
const MAX_PIXELS: u64 = 64 * 1024 * 1024;

/// Bounds the averaging arrays to 14 MiB even with unusually large tty geometry.
const MAX_THUMBNAIL_PIXELS: u32 = 512 * 1024;

/// Allocation budget handed to the PNG decoder for its own buffers.
const LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// A downscaled image as row-major opaque RGB.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Thumbnail {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height` pixels in row-major order.
    pub pixels: Vec<[u8; 3]>,
}

impl Thumbnail {
    /// Halve both dimensions for a smaller terminal encoding, preserving
    /// the mean colour of narrow lines and odd-sized edges.
    pub(super) fn half(&self) -> Thumbnail {
        let source = (self.width, self.height);
        let target = (self.width.div_ceil(2), self.height.div_ceil(2));
        let mut average = Average::new(source, target, ColorType::Rgb);
        for (y, row) in self.pixels.chunks_exact(self.width as usize).enumerate() {
            average.add(y as u32, row.as_flattened());
        }
        average.finish()
    }
}

/// Why an image could not be turned into a [`Thumbnail`].
#[derive(Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// The header or decoder budget exceeds the preview limits.
    TooLarge,
    /// The image decodes to a pixel format the previewer does not handle.
    Unsupported,
    /// The stream is not a valid PNG, or the requested target is empty.
    Invalid(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => f.write_str("image exceeds the preview size limits"),
            Self::Unsupported => f.write_str("unsupported image format"),
            Self::Invalid(reason) => write!(f, "invalid image: {reason}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<DecodingError> for DecodeError {
    fn from(error: DecodingError) -> Self {
        match error {
            DecodingError::LimitsExceeded => Self::TooLarge,
            other => Self::Invalid(other.to_string()),
        }
    }
}

/// Largest size that fits the pixel bounds, keeping aspect ratio.
///
/// The image is never enlarged, and each side stays at least one pixel.
/// Returns `None` when any source or bound dimension is zero.
pub fn fit(source: (u32, u32), bounds: (u32, u32)) -> Option<(u32, u32)> {
    let (sw, sh) = (u64::from(source.0), u64::from(source.1));
    let (box_w, box_h) = (u64::from(bounds.0), u64::from(bounds.1));
    if sw == 0 || sh == 0 || box_w == 0 || box_h == 0 {
        return None;
    }
    if sw <= box_w && sh <= box_h {
        return Some(source);
    }
    // Compare ratios by cross-multiplying to stay in exact integers.
    let (w, h) = if box_w * sh <= box_h * sw {
        (box_w, (sh * box_w / sw).max(1))
    } else {
        ((sw * box_h / sh).max(1), box_h)
    };
    // Both are bounded by the source, which came from u32 inputs.
    Some((w as u32, h as u32))
}

/// Adapts a forward-only reader to the `BufRead + Seek` bound the decoder
/// declares but never exercises when decoding.
struct Forward<R>(BufReader<R>);

impl<R: Read> Read for Forward<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<R: Read> BufRead for Forward<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.0.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.0.consume(amount);
    }
}

impl<R> Seek for Forward<R> {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "stream is not seekable",
        ))
    }
}

/// Box-filter accumulator whose memory scales with the thumbnail.
struct Average {
    columns: Vec<usize>,
    width: usize,
    height: usize,
    source_height: u64,
    sums: Vec<[u64; 3]>,
    counts: Vec<u32>,
    channels: usize,
    alpha: bool,
    grey: bool,
}

impl Average {
    /// Maps each source pixel to the output cell that contains it.
    fn new(source: (u32, u32), target: (u32, u32), color: ColorType) -> Self {
        let (sw, sh) = (u64::from(source.0), u64::from(source.1));
        let (tw, th) = (u64::from(target.0), u64::from(target.1));
        let columns = (0..sw).map(|x| (x * tw / sw) as usize).collect();
        let cells = (tw * th) as usize;
        let (channels, alpha, grey) = match color {
            ColorType::Grayscale => (1, false, true),
            ColorType::GrayscaleAlpha => (2, true, true),
            ColorType::Rgb => (3, false, false),
            _ => (4, true, false),
        };
        Self {
            columns,
            width: tw as usize,
            height: th as usize,
            source_height: sh,
            sums: vec![[0; 3]; cells],
            counts: vec![0; cells],
            channels,
            alpha,
            grey,
        }
    }

    /// Folds one source row into the output cells it overlaps.
    fn add(&mut self, y: u32, row: &[u8]) {
        let oy = (u64::from(y) * self.height as u64 / self.source_height) as usize;
        let base = oy * self.width;
        for (x, px) in row.chunks_exact(self.channels).enumerate() {
            let rgb = self.rgb(px);
            let i = base + self.columns[x];
            for (sum, channel) in self.sums[i].iter_mut().zip(rgb) {
                *sum += u64::from(channel);
            }
            self.counts[i] += 1;
        }
    }

    /// Converts one pixel to opaque RGB over [`BACKGROUND`].
    fn rgb(&self, px: &[u8]) -> [u8; 3] {
        let color: [u8; 3] = if self.grey {
            [px[0]; 3]
        } else {
            [px[0], px[1], px[2]]
        };
        if !self.alpha {
            return color;
        }
        let a = u32::from(px[self.channels - 1]);
        let mut out = [0; 3];
        for ((o, c), b) in out.iter_mut().zip(color).zip(BACKGROUND) {
            // Rounded a/255 blend of foreground over background.
            *o = ((u32::from(c) * a + u32::from(b) * (255 - a) + 127) / 255) as u8;
        }
        out
    }

    fn finish(self) -> Thumbnail {
        let pixels = self
            .sums
            .iter()
            .zip(&self.counts)
            .map(|(sum, &count)| {
                let count = u64::from(count.max(1));
                sum.map(|s| ((s + count / 2) / count) as u8)
            })
            .collect();
        Thumbnail {
            width: self.width as u32,
            height: self.height as u32,
            pixels,
        }
    }
}

/// Decodes a PNG into a thumbnail whose size `target` picks from the source.
///
/// The IHDR is checked against the size caps before `target` runs or any
/// pixel memory is reserved. The size `target` returns is clamped to the
/// source so every output pixel averages at least one source pixel.
pub fn decode(
    reader: impl Read,
    target: impl FnOnce(u32, u32) -> Option<(u32, u32)>,
) -> Result<Thumbnail, DecodeError> {
    let mut options = DecodeOptions::default();
    // Preview pixels do not use text or ICC profiles. Check the stream's
    // integrity even for ancillary chunks; avoid retaining their metadata.
    options.set_ignore_text_chunk(true);
    options.set_ignore_iccp_chunk(true);
    options.set_skip_ancillary_crc_failures(false);
    options.set_ignore_adler32(false);
    let mut decoder = Decoder::new_with_options(Forward(BufReader::new(reader)), options);
    decoder.set_limits(Limits { bytes: LIMIT_BYTES });
    // Expanding to 8-bit grey/RGB(A) leaves four layouts to handle.
    decoder.set_transformations(Transformations::EXPAND | Transformations::STRIP_16);
    let info = decoder.read_header_info()?;
    let (width, height) = (info.width, info.height);
    if width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(DecodeError::TooLarge);
    }
    let (tw, th) = target(width, height)
        .filter(|&(w, h)| w > 0 && h > 0)
        .ok_or_else(|| DecodeError::Invalid("empty target size".into()))?;
    let (tw, th) = (tw.min(width), th.min(height));
    let (tw, th) = if tw * th > MAX_THUMBNAIL_PIXELS {
        let scale = (f64::from(MAX_THUMBNAIL_PIXELS) / f64::from(tw * th)).sqrt();
        (
            ((f64::from(tw) * scale) as u32).max(1),
            ((f64::from(th) * scale) as u32).max(1),
        )
    } else {
        (tw, th)
    };

    let mut reader = decoder.read_info()?;
    let (color, depth) = reader.output_color_type();
    if depth != png::BitDepth::Eight || color == ColorType::Indexed {
        return Err(DecodeError::Unsupported);
    }
    let mut average = Average::new((width, height), (tw, th), color);

    if reader.info().interlaced {
        // Adam7 passes interleave across the whole image, so rows can only
        // be averaged once the frame is complete; the pixel cap bounds this.
        let size = reader.output_buffer_size().ok_or(DecodeError::TooLarge)?;
        let mut frame = vec![0; size];
        reader.next_frame(&mut frame)?;
        let line = size / height as usize;
        for (y, row) in frame.chunks_exact(line).enumerate() {
            average.add(y as u32, row);
        }
    } else {
        let mut y = 0;
        while let Some(row) = reader.next_row()? {
            average.add(y, row.data());
            y += 1;
        }
    }
    // Reading the final row only flushes the image data. A complete IEND
    // (and its checksum) must also be present before a preview is emitted.
    reader.finish()?;
    Ok(average.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a PNG with the real encoder so fixtures follow the spec.
    ///
    /// The encoder cannot write Adam7, so interlaced fixtures are assembled
    /// by hand and support 8-bit RGB only.
    fn encode(
        width: u32,
        height: u32,
        color: png::ColorType,
        depth: png::BitDepth,
        data: &[u8],
        interlaced: bool,
    ) -> Vec<u8> {
        if interlaced {
            assert_eq!((color, depth), (png::ColorType::Rgb, png::BitDepth::Eight));
            return adam7_rgb8(width, height, data);
        }
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (!(crc & 1)).wrapping_add(1));
            }
        }
        !crc
    }

    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }

    /// Wraps `raw` in a zlib stream of stored blocks (no compression).
    fn zlib_stored(raw: &[u8]) -> Vec<u8> {
        assert!(raw.len() <= 0xffff, "fixture needs one stored block");
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in raw {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        let mut out = vec![0x78, 0x01, 0x01];
        out.extend_from_slice(&(raw.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
        out.extend_from_slice(raw);
        out.extend_from_slice(&((b << 16) | a).to_be_bytes());
        out
    }

    /// Hand-assembles an Adam7 8-bit RGB PNG with unfiltered scanlines.
    fn adam7_rgb8(width: u32, height: u32, data: &[u8]) -> Vec<u8> {
        // (x start, y start, x step, y step) for the seven passes.
        const PASSES: [(u32, u32, u32, u32); 7] = [
            (0, 0, 8, 8),
            (4, 0, 8, 8),
            (0, 4, 4, 8),
            (2, 0, 4, 4),
            (0, 2, 2, 4),
            (1, 0, 2, 2),
            (0, 1, 1, 2),
        ];
        let mut raw = Vec::new();
        for (x0, y0, dx, dy) in PASSES {
            if x0 >= width {
                continue;
            }
            for y in (y0..height).step_by(dy as usize) {
                raw.push(0);
                for x in (x0..width).step_by(dx as usize) {
                    let at = ((y * width + x) * 3) as usize;
                    raw.extend_from_slice(&data[at..at + 3]);
                }
            }
        }
        let mut header = Vec::new();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[8, 2, 0, 0, 1]);
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut out, b"IHDR", &header);
        chunk(&mut out, b"IDAT", &zlib_stored(&raw));
        chunk(&mut out, b"IEND", &[]);
        out
    }

    fn rgb8(width: u32, height: u32, data: &[u8]) -> Vec<u8> {
        encode(
            width,
            height,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            data,
            false,
        )
    }

    /// Decodes at the source size so assertions see exact pixel values.
    fn decode_full(png: &[u8]) -> Result<Thumbnail, DecodeError> {
        decode(png, |w, h| Some((w, h)))
    }

    /// Writes only the signature and IHDR, which is all the caps inspect.
    fn header_only(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap();
        out
    }

    #[test]
    fn fit_preserves_aspect_and_never_upscales() {
        assert_eq!(fit((400, 200), (800, 240)), Some((400, 200)));
        assert_eq!(fit((4000, 2000), (800, 240)), Some((480, 240)));
    }

    #[test]
    fn fit_handles_degenerate_boxes() {
        let (width, _) = fit((1, 10000), (800, 240)).unwrap();
        assert!(width >= 1);
        assert_eq!(fit((10, 10), (0, 240)), None);
        assert_eq!(fit((0, 5), (800, 240)), None);
        assert_eq!(fit((10, 10), (800, 0)), None);
        let (width, _) = fit((100, 100), (10, 60)).unwrap();
        assert!(width <= 10);
    }

    #[test]
    fn downscale_averages_areas() {
        let mut data = Vec::new();
        for _ in 0..2 {
            data.extend_from_slice(&[255, 255, 255, 255, 255, 255]);
            data.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        }
        let png = rgb8(4, 2, &data);
        let thumb = decode(png.as_slice(), |_, _| Some((2, 1))).unwrap();
        assert_eq!((thumb.width, thumb.height), (2, 1));
        assert_eq!(thumb.pixels, [[255, 255, 255], [0, 0, 0]]);
    }

    #[test]
    fn downscale_mixes_uneven_areas() {
        // Two white pixels and one black pixel collapse to a rounded mean.
        let png = rgb8(3, 1, &[255, 255, 255, 255, 255, 255, 0, 0, 0]);
        let thumb = decode(png.as_slice(), |_, _| Some((1, 1))).unwrap();
        assert_eq!(thumb.pixels, [[170, 170, 170]]);
    }

    #[test]
    fn target_is_clamped_to_source() {
        let png = rgb8(1, 1, &[9, 8, 7]);
        let thumb = decode(png.as_slice(), |_, _| Some((50, 50))).unwrap();
        assert_eq!((thumb.width, thumb.height), (1, 1));
        assert_eq!(thumb.pixels, [[9, 8, 7]]);
    }

    #[test]
    fn empty_target_is_invalid() {
        let png = rgb8(1, 1, &[0, 0, 0]);
        assert!(matches!(
            decode(png.as_slice(), |_, _| None),
            Err(DecodeError::Invalid(_))
        ));
        assert!(matches!(
            decode(png.as_slice(), |_, _| Some((0, 1))),
            Err(DecodeError::Invalid(_))
        ));
    }

    #[test]
    fn alpha_composites_on_background() {
        let transparent = encode(
            1,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[255, 0, 0, 0],
            false,
        );
        assert_eq!(decode_full(&transparent).unwrap().pixels, [BACKGROUND]);
        let opaque = encode(
            1,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[255, 0, 0, 255],
            false,
        );
        assert_eq!(decode_full(&opaque).unwrap().pixels, [[255, 0, 0]]);
    }

    #[test]
    fn grey_alpha_composites_on_background() {
        let png = encode(
            1,
            1,
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            &[200, 0],
            false,
        );
        assert_eq!(decode_full(&png).unwrap().pixels, [BACKGROUND]);
    }

    #[test]
    fn palette_trns_grey_and_sixteen_bit_normalize() {
        // Palette: index 0 is red, index 1 is green and fully transparent.
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, 2, 1);
        encoder.set_color(png::ColorType::Indexed);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_palette(vec![255, 0, 0, 0, 255, 0]);
        encoder.set_trns(vec![255, 0]);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[0, 1]).unwrap();
        writer.finish().unwrap();
        assert_eq!(decode_full(&out).unwrap().pixels, [[255, 0, 0], BACKGROUND]);

        // One-bit grey expands 1 to white and 0 to black (0b10 packed left).
        let grey1 = encode(
            2,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::One,
            &[0b1000_0000],
            false,
        );
        assert_eq!(
            decode_full(&grey1).unwrap().pixels,
            [[255, 255, 255], [0, 0, 0]]
        );

        // Sixteen-bit RGB keeps the high byte of each sample.
        let deep = encode(
            1,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Sixteen,
            &[0xff, 0xff, 0x80, 0x00, 0x00, 0xff],
            false,
        );
        assert_eq!(decode_full(&deep).unwrap().pixels, [[255, 128, 0]]);
    }

    #[test]
    fn interlaced_decodes_under_cap() {
        let data: Vec<u8> = (0..8 * 8 * 3).map(|i| (i * 7 % 256) as u8).collect();
        let plain = encode(
            8,
            8,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            &data,
            false,
        );
        let adam7 = encode(8, 8, png::ColorType::Rgb, png::BitDepth::Eight, &data, true);
        let expected = decode_full(&plain).unwrap();
        assert_eq!(decode_full(&adam7).unwrap(), expected);
        let small = decode(adam7.as_slice(), |_, _| Some((4, 4))).unwrap();
        let small_plain = decode(plain.as_slice(), |_, _| Some((4, 4))).unwrap();
        assert_eq!(small, small_plain);
    }

    #[test]
    fn ihdr_caps_reject_before_allocation() {
        assert!(matches!(
            decode_full(&header_only(20000, 10)),
            Err(DecodeError::TooLarge)
        ));
        assert!(matches!(
            decode_full(&header_only(16384, 4097)),
            Err(DecodeError::TooLarge)
        ));
        // The target callback must never run for a rejected header.
        assert!(matches!(
            decode(header_only(20000, 10).as_slice(), |_, _| unreachable!()),
            Err(DecodeError::TooLarge)
        ));
    }

    #[test]
    fn corrupt_inputs_fail() {
        let good = rgb8(2, 2, &[10; 12]);
        assert!(decode_full(&good).is_ok());

        let truncated = &good[..good.len() - 20];
        assert!(decode_full(truncated).is_err());

        // Flip a byte inside the IHDR payload so its CRC no longer matches.
        let mut bad_crc = good.clone();
        bad_crc[20] ^= 0x01;
        assert!(decode_full(&bad_crc).is_err());

        assert!(decode_full(b"not a png").is_err());
        assert!(decode_full(b"").is_err());
    }

    #[test]
    fn truncated_iend_is_invalid() {
        let mut png = rgb8(1, 1, &[1, 2, 3]);
        png.pop();
        assert!(decode_full(&png).is_err());
    }

    #[test]
    fn corrupt_iend_crc_is_invalid() {
        let mut png = rgb8(1, 1, &[1, 2, 3]);
        *png.last_mut().unwrap() ^= 1;
        assert!(decode_full(&png).is_err());
    }

    #[test]
    fn corrupt_ancillary_crc_is_invalid() {
        let mut png = rgb8(1, 1, &[1, 2, 3]);
        let mut text = Vec::new();
        chunk(&mut text, b"tEXt", b"Title\0Icon");
        *text.last_mut().unwrap() ^= 1;
        // Ancillary metadata is allowed between IDAT and IEND.
        png.splice(png.len() - 12..png.len() - 12, text);
        assert!(decode_full(&png).is_err());
    }

    #[test]
    fn huge_target_is_bounded_before_accumulation() {
        let png = rgb8(1600, 800, &vec![17; 1600 * 800 * 3]);
        let thumbnail = decode_full(&png).unwrap();
        assert!(thumbnail.pixels.len() <= 512 * 1024);
        assert!(thumbnail.width.abs_diff(thumbnail.height * 2) <= 1);
    }

    #[test]
    fn errors_display_without_panicking() {
        assert!(!DecodeError::TooLarge.to_string().is_empty());
        assert!(!DecodeError::Unsupported.to_string().is_empty());
        assert!(DecodeError::Invalid("x".into()).to_string().contains('x'));
    }
}
