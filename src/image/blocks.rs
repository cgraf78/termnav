//! Truecolor half-block rendering of a thumbnail.

use super::{BACKGROUND, Thumbnail};

/// Renders `thumbnail` as truecolor `▀` cells, two pixel rows per text line.
///
/// Each cell's foreground is the top pixel and its background the bottom
/// pixel. An odd last row gets [`BACKGROUND`] as its bottom half. Every
/// line ends with a reset so a truncated or wrapped line cannot leak
/// colour into the surrounding terminal.
pub fn blocks(thumbnail: &Thumbnail) -> String {
    let width = thumbnail.width as usize;
    let height = thumbnail.height as usize;
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    for top_row in (0..height).step_by(2) {
        let top = &thumbnail.pixels[top_row * width..(top_row + 1) * width];
        // Rows are consecutive, so the bottom row directly follows the top.
        let bottom = (top_row + 1 < height)
            .then(|| &thumbnail.pixels[(top_row + 1) * width..(top_row + 2) * width]);
        // `None` forces the first cell of each line to emit both colours,
        // since the previous line ended with a reset.
        let mut prev_top = None;
        let mut prev_bottom = None;
        for x in 0..width {
            let (t, b) = (top[x], bottom.map_or(BACKGROUND, |row| row[x]));
            // Colour changes are tracked per channel: a repeated colour
            // costs nothing, which keeps large previews compact.
            if prev_top != Some(t) {
                push_sgr(&mut out, 38, t);
                prev_top = Some(t);
            }
            if prev_bottom != Some(b) {
                push_sgr(&mut out, 48, b);
                prev_bottom = Some(b);
            }
            out.push('▀');
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

/// Appends a truecolor SGR sequence; `base` is 38 (foreground) or 48 (background).
fn push_sgr(out: &mut String, base: u8, [r, g, b]: [u8; 3]) {
    out.push_str(&format!("\x1b[{base};2;{r};{g};{b}m"));
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

    #[test]
    fn blocks_two_by_two_exact() {
        let t = thumb(2, 2, vec![[1, 2, 3], [4, 5, 6], [7, 8, 9], [10, 11, 12]]);
        assert_eq!(
            blocks(&t),
            "\x1b[38;2;1;2;3m\x1b[48;2;7;8;9m▀\
             \x1b[38;2;4;5;6m\x1b[48;2;10;11;12m▀\x1b[0m\n"
        );
    }

    #[test]
    fn blocks_repeats_only_changed_channels() {
        // Top colours match, bottom colours differ, then the reverse.
        let t = thumb(
            3,
            2,
            vec![
                [1, 1, 1],
                [1, 1, 1],
                [2, 2, 2],
                [9, 9, 9],
                [8, 8, 8],
                [8, 8, 8],
            ],
        );
        assert_eq!(
            blocks(&t),
            "\x1b[38;2;1;1;1m\x1b[48;2;9;9;9m▀\
             \x1b[48;2;8;8;8m▀\
             \x1b[38;2;2;2;2m▀\x1b[0m\n"
        );
    }

    #[test]
    fn blocks_first_cell_of_each_line_emits_both() {
        let t = thumb(1, 4, vec![[1, 1, 1], [2, 2, 2], [1, 1, 1], [2, 2, 2]]);
        assert_eq!(
            blocks(&t),
            "\x1b[38;2;1;1;1m\x1b[48;2;2;2;2m▀\x1b[0m\n\
             \x1b[38;2;1;1;1m\x1b[48;2;2;2;2m▀\x1b[0m\n"
        );
    }

    #[test]
    fn blocks_odd_height_pads_with_background() {
        let t = thumb(1, 3, vec![[1, 2, 3], [4, 5, 6], [7, 8, 9]]);
        let [r, g, b] = BACKGROUND;
        assert_eq!(
            blocks(&t),
            format!(
                "\x1b[38;2;1;2;3m\x1b[48;2;4;5;6m▀\x1b[0m\n\
                 \x1b[38;2;7;8;9m\x1b[48;2;{r};{g};{b}m▀\x1b[0m\n"
            )
        );
    }

    #[test]
    fn blocks_resets_every_line() {
        let t = thumb(2, 5, vec![[3, 3, 3]; 10]);
        let out = blocks(&t);
        assert_eq!(out.lines().count(), 3);
        assert!(out.lines().all(|l| l.ends_with("\x1b[0m")));
        assert!(out.ends_with("\x1b[0m\n"));
    }

    #[test]
    fn blocks_empty_thumbnail_is_empty() {
        assert_eq!(blocks(&thumb(0, 0, vec![])), "");
    }
}
