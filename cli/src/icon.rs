//! The octo app icon, embedded at compile time and rendered to the terminal.

use image::imageops::FilterType;

/// The octo app icon PNG, baked into the `octo` binary.
static ICON_PNG: &[u8] = include_bytes!("../assets/octo.png");

/// Render the embedded icon as ANSI truecolor half-block art, `cols` columns
/// wide. Each text row encodes two pixel rows via the upper-half-block glyph
/// (`▀`): foreground colour is the top pixel, background colour the bottom.
/// Returns an empty string if the embedded PNG fails to decode.
pub fn render(cols: u32) -> String {
    // Two pixel rows per text row, so the pixel grid must have an even height.
    let size = cols.max(2) & !1;
    let Ok(img) = image::load_from_memory(ICON_PNG) else {
        return String::new();
    };
    let img = img.resize_exact(size, size, FilterType::Triangle).to_rgb8();

    let mut out = String::new();
    let mut y = 0;
    while y + 1 < size {
        for x in 0..size {
            let t = img.get_pixel(x, y).0;
            let b = img.get_pixel(x, y + 1).0;
            out.push_str(&format!(
                "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m\u{2580}",
                t[0], t[1], t[2], b[0], b[1], b[2],
            ));
        }
        out.push_str("\x1b[0m\n");
        y += 2;
    }
    out
}
