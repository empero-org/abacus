//! QR codes for the terminal, so a phone can open a pairing link by pointing
//! its camera at the screen.
//!
//! Two modules share one character cell: `▀` paints the upper module, `▄` the
//! lower, `█` both. Cells are about twice as tall as they are wide, so this
//! keeps the modules square. The code is always drawn dark-on-light with
//! explicit colours rather than in the terminal's own foreground: on a dark
//! theme the "natural" rendering comes out inverted, which many phone cameras
//! refuse to read.

use qrcode::{Color, EcLevel, QrCode};

/// Light modules around the code. The standard asks for four; two is what
/// phone cameras need in practice and keeps the code on an 80×24 screen.
pub const QUIET_ZONE: usize = 2;

/// The code for `data` as rows of half-block characters, where ink (`▀ ▄ █`)
/// marks dark modules and a space a light one, quiet zone included. Every row
/// has the same number of characters. `None` when `data` is too long to
/// encode.
pub fn rows(data: &str) -> Option<Vec<String>> {
    // Low error correction: the smallest code, and a screen is not a
    // scratched sticker.
    let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::L).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let size = width + 2 * QUIET_ZONE;
    let dark = |x: usize, y: usize| {
        let inside = (QUIET_ZONE..QUIET_ZONE + width).contains(&x) && (QUIET_ZONE..QUIET_ZONE + width).contains(&y);
        inside && colors[(y - QUIET_ZONE) * width + (x - QUIET_ZONE)] == Color::Dark
    };
    let rows = (0..size)
        .step_by(2)
        .map(|y| {
            (0..size)
                .map(|x| match (dark(x, y), dark(x, y + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect()
        })
        .collect();
    Some(rows)
}

/// The code for `url`, ready to print on a terminal: each row indented two
/// columns and painted black on white with 256-colour escapes (plain 16-colour
/// ones where that is all the terminal claims), newline-terminated. Empty
/// when stdout is not a terminal — a QR code in a pipe or a log is noise — or
/// when the URL cannot be encoded.
pub fn render(url: &str) -> String {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return String::new();
    }
    render_ansi(url, crate::theme::ColorDepth::detect())
}

fn render_ansi(url: &str, depth: crate::theme::ColorDepth) -> String {
    use crate::theme::ColorDepth;
    let Some(rows) = rows(url) else {
        return String::new();
    };
    let paint = match depth {
        ColorDepth::Ansi256 | ColorDepth::TrueColor => "\x1b[38;5;16;48;5;231m",
        ColorDepth::Ansi16 | ColorDepth::None => "\x1b[30;107m",
    };
    rows.iter().map(|row| format!("  {paint}{row}\x1b[0m\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unfold the half blocks back into modules.
    fn modules(rows: &[String]) -> Vec<Vec<bool>> {
        let mut grid = Vec::new();
        for row in rows {
            let (mut top, mut bottom) = (Vec::new(), Vec::new());
            for cell in row.chars() {
                let (upper, lower) = match cell {
                    '█' => (true, true),
                    '▀' => (true, false),
                    '▄' => (false, true),
                    ' ' => (false, false),
                    other => panic!("unexpected cell {other:?}"),
                };
                top.push(upper);
                bottom.push(lower);
            }
            grid.push(top);
            grid.push(bottom);
        }
        grid
    }

    #[test]
    fn rows_reproduce_the_code_inside_a_light_quiet_zone() {
        // A real pairing link: a 256-bit token, base64url, in the fragment.
        let url = format!("https://abacus.empero.org/pair#t={}", "A1b2_C3d4-".repeat(5).get(..43).unwrap());
        let rows = rows(&url).unwrap();
        let code = QrCode::with_error_correction_level(url.as_bytes(), EcLevel::L).unwrap();
        let width = code.width();
        let size = width + 2 * QUIET_ZONE;
        assert_eq!(rows.len(), size.div_ceil(2));
        assert!(rows.iter().all(|row| row.chars().count() == size));
        let grid = modules(&rows);
        let colors = code.to_colors();
        for (y, line) in grid.iter().enumerate().take(size) {
            for (x, &dark) in line.iter().enumerate() {
                let inside =
                    (QUIET_ZONE..QUIET_ZONE + width).contains(&x) && (QUIET_ZONE..QUIET_ZONE + width).contains(&y);
                let expected = inside && colors[(y - QUIET_ZONE) * width + (x - QUIET_ZONE)] == Color::Dark;
                assert_eq!(dark, expected, "module ({x}, {y})");
            }
        }
        // A pairing URL fits a small code: version 4 (33 modules), 19 rows of
        // 37 columns with the quiet zone — comfortably inside 80×24.
        assert!(width <= 33, "width {width}");
        assert!(rows.len() <= 19 && size <= 37);
    }

    #[test]
    fn printed_rows_are_painted_dark_on_light_and_reset() {
        let printed = render_ansi("https://example.test/pair#t=abc", crate::theme::ColorDepth::Ansi256);
        let lines: Vec<&str> = printed.lines().collect();
        assert!(!lines.is_empty());
        for line in lines {
            assert!(line.starts_with("  \x1b[38;5;16;48;5;231m"), "{line:?}");
            assert!(line.ends_with("\x1b[0m"));
        }
        assert!(render_ansi("x", crate::theme::ColorDepth::Ansi16).contains("\x1b[30;107m"));
    }

    #[test]
    fn oversized_data_has_no_code() {
        assert!(rows(&"x".repeat(8_000)).is_none());
        assert_eq!(render_ansi(&"x".repeat(8_000), crate::theme::ColorDepth::Ansi256), "");
    }
}
