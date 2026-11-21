//! Fixed-palette DEC sixel rendering of a thumbnail.

use std::fmt::Write;

use super::Thumbnail;

/// Registers 0..216 are the 6x6x6 colour cube, 216..240 are greys.
const CUBE_REGISTERS: usize = 216;
const REGISTERS: usize = 240;

/// Pixel rows encoded by one sixel band.
pub(crate) const BAND_ROWS: usize = 6;

/// Shortest run worth the `!<count><char>` form; shorter runs are smaller literally.
const MIN_RLE_RUN: usize = 4;

/// tmux accepts at most 10,000 pixels on either axis. Its 3.6 decoder
/// allocates complete six-row bands, including the final partial band.
const MAX_WIDTH: usize = 10_000;
const MAX_HEIGHT: usize = MAX_WIDTH / BAND_ROWS * BAND_ROWS;

/// A same-size tmux re-encoding pads to cell boundaries and can add a '-'
/// for each empty band. Even 10,000 padded rows add fewer than 2 KiB.
const MAX_SEQUENCE_BYTES: usize = 1024 * 1024 - 2048;

/// Returns the RGB value of palette register `index` (0..240).
fn color(index: usize) -> [u8; 3] {
    if index < CUBE_REGISTERS {
        // Cube levels are 0, 51, ..., 255, with red the most significant digit.
        let level = |digit: usize| (digit * 51) as u8;
        [level(index / 36), level(index / 6 % 6), level(index % 6)]
    } else {
        [8 + 10 * (index - CUBE_REGISTERS) as u8; 3]
    }
}

/// Squared RGB distance from `rgb` to palette register `index`.
fn distance(index: usize, rgb: [u8; 3]) -> i32 {
    color(index)
        .iter()
        .zip(rgb)
        .map(|(&p, c)| (i32::from(p) - i32::from(c)).pow(2))
        .sum()
}

/// Returns the palette register nearest to `rgb` by squared RGB distance.
///
/// The fixed palette needs no per-image analysis, so output is deterministic
/// and tmux can re-encode it cheaply per client. Ties go to the lower index,
/// which prefers the cube over the greys.
///
/// Thumbnails hold hundreds of thousands of pixels, so this avoids scanning
/// all 240 entries: distance is separable per channel, so the nearest cube
/// entry is the nearest level in each channel, and the nearest grey is one
/// of the two greys bracketing the channel mean.
pub fn quantize(rgb: [u8; 3]) -> u8 {
    // Levels are 51 apart (odd), so a channel never ties between two levels.
    let level = |c: u8| (usize::from(c) + 25) / 51;
    let [r, g, b] = rgb.map(level);
    let mut best = 36 * r + 6 * g + b;

    // Grey k sits at 8 + 10k, so the real-valued optimum is (sum - 24) / 30.
    let sum: i32 = rgb.iter().map(|&c| i32::from(c)).sum();
    let low = ((sum - 24).div_euclid(30)).clamp(0, 23) as usize;
    let high = (low + 1).min(23);
    let mut best_distance = distance(best, rgb);
    // Strict `<` keeps the cube, then the lower grey, on ties.
    for k in [low, high] {
        let index = CUBE_REGISTERS + k;
        let d = distance(index, rgb);
        if d < best_distance {
            best = index;
            best_distance = d;
        }
    }
    best as u8
}

/// Renders `thumbnail` as a standard DEC sixel sequence.
///
/// Sixel has no truecolor, so pixels snap to [`quantize`]'s palette. The
/// introducer's `P2 = 0` keeps the background opaque, and the raster
/// attributes tell the terminal the exact pixel size so it need not guess.
/// An empty thumbnail yields an empty string.
pub fn sixel(thumbnail: &Thumbnail) -> String {
    let width = thumbnail.width as usize;
    let height = thumbnail.height as usize;
    if width == 0 || height == 0 {
        return String::new();
    }
    // Only complete sequences reach the terminal. Each retry visits at most
    // half as many samples, and a single pixel always fits the budget.
    let mut source = thumbnail;
    let mut smaller;
    loop {
        if let Some(text) = encode(source) {
            return text;
        }
        smaller = source.half();
        source = &smaller;
    }
}

/// Encode one candidate, stopping at the buffer limit before the next layer.
fn encode(thumbnail: &Thumbnail) -> Option<String> {
    let width = thumbnail.width as usize;
    let height = thumbnail.height as usize;
    if width > MAX_WIDTH || height > MAX_HEIGHT {
        return None;
    }
    let indexed: Vec<u8> = thumbnail.pixels.iter().map(|&p| quantize(p)).collect();

    let mut used = [false; REGISTERS];
    for &i in &indexed {
        used[usize::from(i)] = true;
    }
    let mut out = format!("\x1bP0;0;0q\"1;1;{width};{height}");
    // All registers are defined up front so each band can select by number.
    // A nested tmux can re-emit undefined intermediate registers with an
    // invalid colour type. Number only the used colours, without gaps.
    let mut registers = [0usize; REGISTERS];
    for (register, index) in (0..REGISTERS).filter(|&i| used[i]).enumerate() {
        registers[index] = register;
        let [r, g, b] = color(index).map(percent);
        // Writing to a String cannot fail.
        let _ = write!(out, "#{register};2;{r};{g};{b}");
    }

    let bands = height.div_ceil(BAND_ROWS);
    for band in 0..bands {
        let rows = band * BAND_ROWS..(band * BAND_ROWS + BAND_ROWS).min(height);
        // One sixel mask per column for each colour present in this band.
        let mut masks: Vec<Option<Vec<u8>>> = vec![None; REGISTERS];
        for (bit, y) in rows.enumerate() {
            for x in 0..width {
                let index = usize::from(indexed[y * width + x]);
                masks[index].get_or_insert_with(|| vec![0; width])[x] |= 1 << bit;
            }
        }
        let mut present = masks
            .iter()
            .enumerate()
            .filter_map(|(i, m)| m.as_ref().map(|m| (i, m)))
            .peekable();
        while let Some((index, mask)) = present.next() {
            let _ = write!(out, "#{}", registers[index]);
            push_runs(&mut out, mask);
            // Reserve space for a band separator and the two-byte terminator.
            if out.len() + 3 > MAX_SEQUENCE_BYTES {
                return None;
            }
            // Carriage return so the next colour overlays the same band.
            if present.peek().is_some() {
                out.push('$');
            }
        }
        if band + 1 < bands {
            out.push('-');
        }
    }
    out.push_str("\x1b\\");
    Some(out)
}

/// Converts an 8-bit channel to the 0..=100 percent sixel uses, rounding half up.
fn percent(component: u8) -> u8 {
    ((u32::from(component) * 200 + 255) / 510) as u8
}

/// Appends `masks` as sixel characters, run-length encoded.
///
/// Trailing empty sixels are dropped: the terminal leaves those columns
/// untouched anyway, so emitting them only costs bytes.
fn push_runs(out: &mut String, masks: &[u8]) {
    let end = masks.iter().rposition(|&m| m != 0).map_or(0, |i| i + 1);
    for run in masks[..end].chunk_by(|a, b| a == b) {
        let ch = char::from(63 + run[0]);
        if run.len() >= MIN_RLE_RUN {
            let _ = write!(out, "!{}{ch}", run.len());
        } else {
            out.extend(std::iter::repeat_n(ch, run.len()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thumb(width: u32, height: u32, pixels: Vec<[u8; 3]>) -> Thumbnail {
        Thumbnail {
            width,
            height,
            pixels,
        }
    }

    const RED: [u8; 3] = [255, 0, 0];

    /// Exhaustive scan that defines the nearest-with-ties-to-cube rule.
    fn quantize_reference(rgb: [u8; 3]) -> u8 {
        // `min_by_key` returns the first minimum, so ties go to the lower index.
        (0..REGISTERS).min_by_key(|&i| distance(i, rgb)).unwrap() as u8
    }

    #[test]
    fn quantize_matches_reference_on_lattice() {
        for r in (0..=255u8).step_by(5) {
            for g in (0..=255u8).step_by(5) {
                for b in (0..=255u8).step_by(5) {
                    assert_eq!(
                        quantize([r, g, b]),
                        quantize_reference([r, g, b]),
                        "{r},{g},{b}"
                    );
                }
            }
        }
    }

    #[test]
    fn quantize_matches_reference_on_greys_and_palette() {
        for v in 0..=255u8 {
            assert_eq!(quantize([v; 3]), quantize_reference([v; 3]), "grey {v}");
        }
        for i in 0..REGISTERS {
            assert_eq!(
                quantize(color(i)),
                quantize_reference(color(i)),
                "register {i}"
            );
        }
    }

    #[test]
    fn sixel_large_gradient_completes() {
        let pixels = (0..1500 * 360)
            .map(|i| [(i % 1500 / 6) as u8, (i / 1500) as u8, (i % 251) as u8])
            .collect();
        assert!(sixel(&thumb(1500, 360, pixels)).ends_with("\x1b\\"));
    }

    #[test]
    fn detailed_image_fits_tmux_input_buffer() {
        // At native resolution this deterministic texture takes 1,047,212
        // bytes, inside tmux's input limit but without room for re-encoding.
        let mut state = 17_u32;
        let pixels = (0..1053 * 210)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let [r, g, b, _] = state.to_le_bytes();
                [r, g, b]
            })
            .collect();
        let text = sixel(&thumb(1053, 210, pixels));
        assert!(text.starts_with("\x1bP") && text.ends_with("\x1b\\"));
        assert!(text.len() <= 1024 * 1024 - 2048, "{} bytes", text.len());
    }

    #[test]
    fn tmux_width_limit_applies_even_when_a_run_is_small() {
        let image = |width| sixel(&thumb(width, 1, vec![RED; width as usize]));
        assert!(image(10_000).contains("\"1;1;10000;1#"));
        assert!(image(10_001).contains("\"1;1;5001;1#"));
    }

    #[test]
    fn tmux_height_limit_counts_complete_sixel_bands() {
        let image = |height| sixel(&thumb(1, height, vec![RED; height as usize]));
        assert!(image(9996).contains("\"1;1;1;9996#"));
        assert!(image(9997).contains("\"1;1;1;4999#"));
    }

    #[test]
    fn quantize_pure_colours() {
        assert_eq!(quantize([0, 0, 0]), 0);
        assert_eq!(quantize([255, 255, 255]), 215);
        assert_eq!(quantize(RED), 180);
    }

    #[test]
    fn quantize_prefers_grey_when_closer() {
        // 128 sits between cube levels 102 and 153 but exactly on grey 8 + 10 * 12.
        assert_eq!(quantize([128, 128, 128]), 216 + 12);
    }

    #[test]
    fn sixel_one_pixel_exact() {
        assert_eq!(
            sixel(&thumb(1, 1, vec![RED])),
            "\x1bP0;0;0q\"1;1;1;1#0;2;100;0;0#0@\x1b\\"
        );
    }

    #[test]
    fn sixel_two_colours_share_a_band() {
        let t = thumb(2, 1, vec![[0, 0, 0], RED]);
        assert_eq!(
            sixel(&t),
            "\x1bP0;0;0q\"1;1;2;1#0;2;0;0;0#1;2;100;0;0#0@$#1?@\x1b\\"
        );
    }

    #[test]
    fn sixel_partial_last_band() {
        let t = thumb(1, 7, vec![RED; 7]);
        assert_eq!(sixel(&t), "\x1bP0;0;0q\"1;1;1;7#0;2;100;0;0#0~-#0@\x1b\\");
    }

    #[test]
    fn sixel_run_length_boundary() {
        let short = sixel(&thumb(3, 1, vec![RED; 3]));
        assert!(short.contains("#0@@@\x1b\\"), "{short:?}");
        let long = sixel(&thumb(4, 1, vec![RED; 4]));
        assert!(long.contains("#0!4@\x1b\\"), "{long:?}");
    }

    #[test]
    fn sixel_register_count_capped() {
        let pixels = (0..64 * 64)
            .map(|i| [(i % 64 * 4) as u8, (i / 64 * 4) as u8, (i % 251) as u8])
            .collect();
        let out = sixel(&thumb(64, 64, pixels));
        assert!(out.matches(";2;").count() <= 240);
    }

    #[test]
    fn sixel_deterministic() {
        let t = thumb(2, 2, vec![RED, [0, 255, 0], [0, 0, 255], [9, 9, 9]]);
        assert_eq!(sixel(&t), sixel(&t));
    }

    #[test]
    fn sixel_empty_thumbnail_is_empty() {
        assert_eq!(sixel(&thumb(0, 0, vec![])), "");
        assert_eq!(sixel(&thumb(3, 0, vec![])), "");
        assert_eq!(sixel(&thumb(0, 3, vec![])), "");
    }
}
