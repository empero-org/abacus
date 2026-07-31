//! Color theme for the TUI, derived from the Empero palette (empero.org).
//!
//! Empero ships a warm "paper" light theme and a deep-violet "midnight" dark
//! theme, both built around a single violet accent. We mirror that here so the
//! terminal UI matches the brand, and we keep a light and dark variant so the
//! interface stays legible on either kind of terminal.
//!
//! The active theme is a process-global: the TUI has dozens of free-standing
//! draw helpers, so threading a `Theme` through every one of them would be far
//! more churn than value. It is set once at startup (and on `/theme`), read on
//! every frame.

use std::sync::RwLock;

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
}

/// How the user wants the theme resolved. `Auto` detects the terminal/OS
/// appearance; `Dark`/`Light` pin it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    Auto,
    Dark,
    Light,
}

impl ThemeChoice {
    /// Resolve to a concrete mode, detecting the terminal appearance for `Auto`.
    pub fn resolve(self) -> ThemeMode {
        match self {
            ThemeChoice::Dark => ThemeMode::Dark,
            ThemeChoice::Light => ThemeMode::Light,
            ThemeChoice::Auto => detect_mode().unwrap_or(ThemeMode::Dark),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::Auto => "auto",
            ThemeChoice::Dark => "dark",
            ThemeChoice::Light => "light",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub primary: Color,   // violet accent: interactive text, links, selection
    pub secondary: Color, // headings, the ABACUS wordmark, normal-mode badge
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub muted: Color,   // secondary/subdued text
    pub border: Color,  // panel outlines — must stay visible on the base bg
    pub surface: Color, // subtle panel fill
    pub text: Color,    // primary foreground
    pub inverse: Color, // text drawn on a bright accent fill (badges)
    pub code_bg: Color, // inline/code-block and diff-gutter background
    pub add_fg: Color,  // diff additions
    pub add_bg: Color,
    pub del_fg: Color, // diff deletions
    pub del_bg: Color,
    /// Transcript gutter rails and hairline rules. Quieter than `border` so a
    /// full-width rule reads as structure, not as a boxed-in panel.
    pub rail: Color,
    /// Fill behind the selected row of a list, palette, or picker.
    pub selection: Color,
    /// Fill for raised surfaces — modals and the completion popup — so an
    /// overlay reads as floating above the transcript rather than punched
    /// through it.
    pub overlay: Color,
    /// True when every role is `Reset`. Fills carry no meaning in that state,
    /// so anything that relies on one — badges, selected rows — switches to
    /// reverse video instead of quietly disappearing.
    pub plain: bool,
}

impl Theme {
    /// Empero "midnight": deep violet-black paper, lavender ink, violet accent.
    pub const DARK: Theme = Theme {
        primary: Color::Rgb(182, 107, 255),
        secondary: Color::Rgb(229, 143, 198),
        success: Color::Rgb(110, 210, 140),
        warning: Color::Rgb(240, 192, 96),
        danger: Color::Rgb(240, 122, 122),
        muted: Color::Rgb(141, 135, 148),
        border: Color::Rgb(84, 76, 108),
        surface: Color::Rgb(22, 18, 32),
        text: Color::Rgb(233, 228, 240),
        inverse: Color::Rgb(14, 11, 20),
        code_bg: Color::Rgb(28, 23, 40),
        add_fg: Color::Rgb(110, 210, 140),
        add_bg: Color::Rgb(18, 46, 32),
        del_fg: Color::Rgb(240, 122, 122),
        del_bg: Color::Rgb(54, 24, 30),
        rail: Color::Rgb(62, 56, 80),
        selection: Color::Rgb(45, 33, 68),
        overlay: Color::Rgb(27, 22, 39),
        plain: false,
    };

    /// Empero "paper": warm off-white, near-black ink, violet accent.
    pub const LIGHT: Theme = Theme {
        primary: Color::Rgb(107, 43, 217),
        secondary: Color::Rgb(200, 38, 124),
        success: Color::Rgb(31, 122, 77),
        warning: Color::Rgb(154, 106, 18),
        danger: Color::Rgb(179, 36, 58),
        muted: Color::Rgb(116, 110, 124),
        border: Color::Rgb(176, 166, 152),
        surface: Color::Rgb(232, 227, 218),
        text: Color::Rgb(21, 18, 28),
        inverse: Color::Rgb(244, 241, 236),
        code_bg: Color::Rgb(232, 227, 218),
        add_fg: Color::Rgb(31, 122, 77),
        add_bg: Color::Rgb(214, 236, 222),
        del_fg: Color::Rgb(179, 36, 58),
        del_bg: Color::Rgb(244, 220, 222),
        rail: Color::Rgb(198, 190, 178),
        selection: Color::Rgb(226, 216, 243),
        overlay: Color::Rgb(250, 248, 244),
        plain: false,
    };

    /// The palette with every role reset to the terminal's own colours, for
    /// `NO_COLOR`. Structure — bold, the gutter rails, the badges' reverse
    /// video — carries the whole interface, so it stays legible rather than
    /// merely uncoloured.
    pub const PLAIN: Theme = Theme {
        primary: Color::Reset,
        secondary: Color::Reset,
        success: Color::Reset,
        warning: Color::Reset,
        danger: Color::Reset,
        muted: Color::Reset,
        border: Color::Reset,
        surface: Color::Reset,
        text: Color::Reset,
        inverse: Color::Reset,
        code_bg: Color::Reset,
        add_fg: Color::Reset,
        add_bg: Color::Reset,
        del_fg: Color::Reset,
        del_bg: Color::Reset,
        rail: Color::Reset,
        selection: Color::Reset,
        overlay: Color::Reset,
        plain: true,
    };

    pub fn for_mode(mode: ThemeMode) -> Theme {
        Theme::for_mode_at(mode, ColorDepth::detect())
    }

    /// Resolve a palette at a given colour depth. The depth is applied here, at
    /// the single point every palette comes from, so no draw site can emit a
    /// colour the terminal cannot render.
    pub fn for_mode_at(mode: ThemeMode, depth: ColorDepth) -> Theme {
        let base = match mode {
            ThemeMode::Dark => Theme::DARK,
            ThemeMode::Light => Theme::LIGHT,
        };
        match depth {
            ColorDepth::None => Theme::PLAIN,
            ColorDepth::TrueColor => base,
            ColorDepth::Ansi256 => base.map(quantize_256),
            ColorDepth::Ansi16 => base.map_roles(mode),
        }
    }

    /// Apply `f` to every colour role, leaving `plain` alone.
    fn map(self, f: impl Fn(Color) -> Color) -> Theme {
        Theme {
            primary: f(self.primary),
            secondary: f(self.secondary),
            success: f(self.success),
            warning: f(self.warning),
            danger: f(self.danger),
            muted: f(self.muted),
            border: f(self.border),
            surface: f(self.surface),
            text: f(self.text),
            inverse: f(self.inverse),
            code_bg: f(self.code_bg),
            add_fg: f(self.add_fg),
            add_bg: f(self.add_bg),
            del_fg: f(self.del_fg),
            del_bg: f(self.del_bg),
            rail: f(self.rail),
            selection: f(self.selection),
            overlay: f(self.overlay),
            plain: self.plain,
        }
    }

    /// The sixteen-colour palette, assigned by role rather than by nearest RGB.
    ///
    /// Nearest-neighbour matching is the obvious approach and it fails badly
    /// here: every one of the dark surface colours lands on `Black`, so the
    /// panel fills, rails, and code backgrounds all collapse into the canvas
    /// and the interface loses its structure. The mapping below picks by intent
    /// instead, and flips between the normal and bright halves of the palette
    /// depending on which side of the contrast the text sits on.
    fn map_roles(self, mode: ThemeMode) -> Theme {
        let dark = mode == ThemeMode::Dark;
        let accent = if dark {
            Color::LightMagenta
        } else {
            Color::Magenta
        };
        let second = if dark { Color::LightRed } else { Color::Red };
        Theme {
            primary: accent,
            secondary: second,
            success: if dark {
                Color::LightGreen
            } else {
                Color::Green
            },
            warning: if dark {
                Color::LightYellow
            } else {
                Color::Yellow
            },
            danger: if dark { Color::LightRed } else { Color::Red },
            muted: if dark { Color::Gray } else { Color::DarkGray },
            border: if dark { Color::DarkGray } else { Color::Gray },
            // Surfaces stay on the canvas colour: a sixteen-colour terminal has
            // no shade between black and bright-black that reads as "slightly
            // raised", and guessing produces a muddy band.
            surface: Color::Reset,
            text: if dark { Color::White } else { Color::Black },
            inverse: if dark { Color::Black } else { Color::White },
            code_bg: Color::Reset,
            add_fg: if dark {
                Color::LightGreen
            } else {
                Color::Green
            },
            add_bg: Color::Reset,
            del_fg: if dark { Color::LightRed } else { Color::Red },
            del_bg: Color::Reset,
            rail: Color::DarkGray,
            selection: if dark { Color::DarkGray } else { Color::Gray },
            overlay: Color::Reset,
            plain: self.plain,
        }
    }
}

/// How much colour the terminal can actually render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

impl ColorDepth {
    /// Detect from the environment, without probing the terminal.
    ///
    /// `NO_COLOR` wins outright (https://no-color.org). Otherwise `COLORTERM`
    /// is the only reliable truecolor signal; `TERM` naming a 256-colour entry
    /// is the fallback, and anything else is assumed to be a plain sixteen.
    pub fn detect() -> ColorDepth {
        if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
            return ColorDepth::None;
        }
        if let Ok(value) = std::env::var("ABACUS_COLOR") {
            match value.trim().to_ascii_lowercase().as_str() {
                "none" | "off" => return ColorDepth::None,
                "16" | "ansi" => return ColorDepth::Ansi16,
                "256" => return ColorDepth::Ansi256,
                "true" | "truecolor" | "24bit" => return ColorDepth::TrueColor,
                _ => {}
            }
        }
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        let colorterm = colorterm.trim().to_ascii_lowercase();
        if colorterm == "truecolor" || colorterm == "24bit" {
            return ColorDepth::TrueColor;
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if term.contains("256color") {
            return ColorDepth::Ansi256;
        }
        if term.is_empty() || term == "dumb" {
            return ColorDepth::None;
        }
        ColorDepth::Ansi16
    }
}

/// Nearest xterm-256 index for an RGB colour, considering both the 6×6×6 cube
/// and the 24-step grey ramp and taking whichever is closer. The greys matter:
/// most of this palette is near-neutral, and the cube's grey diagonal is coarse
/// enough that snapping to it visibly tints the surfaces.
fn quantize_256(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest_step = |value: u8| {
        STEPS
            .iter()
            .enumerate()
            .min_by_key(|(_, step)| (**step as i32 - value as i32).abs())
            .map(|(index, step)| (index as u8, *step))
            .expect("STEPS is non-empty")
    };
    let (ri, rv) = nearest_step(r);
    let (gi, gv) = nearest_step(g);
    let (bi, bv) = nearest_step(b);
    let cube_index = 16 + 36 * ri + 6 * gi + bi;
    let cube_distance = distance((r, g, b), (rv, gv, bv));

    // Grey ramp: indices 232..=255 run 8, 18, 28, … 238.
    let average = (r as u32 + g as u32 + b as u32) / 3;
    let level = ((average as i32 - 8) / 10).clamp(0, 23) as u8;
    let grey = 8 + level * 10;
    let grey_distance = distance((r, g, b), (grey, grey, grey));

    if grey_distance < cube_distance {
        Color::Indexed(232 + level)
    } else {
        Color::Indexed(cube_index)
    }
}

fn distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> i32 {
    let dr = a.0 as i32 - b.0 as i32;
    let dg = a.1 as i32 - b.1 as i32;
    let db = a.2 as i32 - b.2 as i32;
    dr * dr + dg * dg + db * db
}

static ACTIVE: RwLock<Theme> = RwLock::new(Theme::DARK);

/// The active theme, copied out (cheap — `Theme` is `Copy`).
pub fn active() -> Theme {
    *ACTIVE.read().expect("theme lock poisoned")
}

pub fn set_active(theme: Theme) {
    *ACTIVE.write().expect("theme lock poisoned") = theme;
}

/// Best-effort detection of the terminal's appearance, without any escape-code
/// probing that could steal a keystroke or hang. In order: an explicit
/// `ABACUS_THEME` override, the `COLORFGBG` hint many terminals export, then the
/// macOS system appearance. Returns `None` when nothing is conclusive.
pub fn detect_mode() -> Option<ThemeMode> {
    if let Ok(value) = std::env::var("ABACUS_THEME") {
        match value.trim().to_ascii_lowercase().as_str() {
            "dark" => return Some(ThemeMode::Dark),
            "light" => return Some(ThemeMode::Light),
            _ => {}
        }
    }
    if let Some(mode) = mode_from_colorfgbg() {
        return Some(mode);
    }
    mode_from_macos_appearance()
}

/// `COLORFGBG` is `foreground;background` (sometimes with a middle field) where
/// the values are ANSI color indices. A background index of 0–6 or 8 is a dark
/// terminal; 7 or 9–15 is light.
fn mode_from_colorfgbg() -> Option<ThemeMode> {
    let value = std::env::var("COLORFGBG").ok()?;
    let background = value.split(';').next_back()?.trim();
    let index: u8 = background.parse().ok()?;
    Some(if index == 7 || index >= 9 {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    })
}

#[cfg(target_os = "macos")]
fn mode_from_macos_appearance() -> Option<ThemeMode> {
    // `defaults read -g AppleInterfaceStyle` prints "Dark" in dark mode and
    // exits non-zero (key absent) in light mode. Safe, fast, no TTY probing.
    let output = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
        .ok()?;
    if !output.status.success() {
        return Some(ThemeMode::Light);
    }
    if String::from_utf8_lossy(&output.stdout)
        .trim()
        .eq_ignore_ascii_case("dark")
    {
        Some(ThemeMode::Dark)
    } else {
        Some(ThemeMode::Light)
    }
}

#[cfg(not(target_os = "macos"))]
fn mode_from_macos_appearance() -> Option<ThemeMode> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests mutate process-wide environment variables, which the test
    /// harness otherwise runs concurrently. Without a lock they intermittently
    /// observe each other's `NO_COLOR` / `ABACUS_COLOR` writes.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn colorfgbg_distinguishes_light_and_dark() {
        let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
        unsafe {
            std::env::set_var("COLORFGBG", "15;0");
        }
        assert_eq!(mode_from_colorfgbg(), Some(ThemeMode::Dark));
        unsafe {
            std::env::set_var("COLORFGBG", "0;15");
        }
        assert_eq!(mode_from_colorfgbg(), Some(ThemeMode::Light));
        unsafe {
            std::env::set_var("COLORFGBG", "0;default;15");
        }
        assert_eq!(mode_from_colorfgbg(), Some(ThemeMode::Light));
        unsafe {
            std::env::remove_var("COLORFGBG");
        }
        assert_eq!(mode_from_colorfgbg(), None);
    }

    #[test]
    fn quantizing_to_256_keeps_roles_distinct() {
        // Collapsing the palette is only useful if the roles that carry meaning
        // survive as different indices.
        let theme = Theme::for_mode_at(ThemeMode::Dark, ColorDepth::Ansi256);
        for role in [
            theme.primary,
            theme.success,
            theme.warning,
            theme.danger,
            theme.text,
        ] {
            assert!(matches!(role, Color::Indexed(_)), "{role:?} not quantized");
        }
        let distinct = [
            theme.primary,
            theme.success,
            theme.warning,
            theme.danger,
            theme.muted,
            theme.text,
        ];
        for (index, first) in distinct.iter().enumerate() {
            for second in &distinct[index + 1..] {
                assert_ne!(first, second, "roles collapsed onto the same index");
            }
        }
        // The near-black surfaces belong on the grey ramp, not the colour cube.
        assert!(matches!(theme.surface, Color::Indexed(232..=255)));
    }

    #[test]
    fn ansi16_keeps_text_and_canvas_apart() {
        // The failure mode this mapping exists to avoid: every dark role
        // landing on Black, so panels vanish into the canvas.
        let dark = Theme::for_mode_at(ThemeMode::Dark, ColorDepth::Ansi16);
        assert_eq!(dark.text, Color::White);
        assert_ne!(dark.text, dark.muted);
        assert_ne!(dark.muted, dark.rail);
        let light = Theme::for_mode_at(ThemeMode::Light, ColorDepth::Ansi16);
        assert_eq!(light.text, Color::Black);
        assert_ne!(light.primary, dark.primary, "accents flip with polarity");
    }

    #[test]
    fn depth_detection_reads_the_environment() {
        let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
        unsafe {
            std::env::set_var("ABACUS_COLOR", "256");
        }
        assert_eq!(ColorDepth::detect(), ColorDepth::Ansi256);
        unsafe {
            std::env::set_var("ABACUS_COLOR", "16");
        }
        assert_eq!(ColorDepth::detect(), ColorDepth::Ansi16);
        unsafe {
            std::env::remove_var("ABACUS_COLOR");
        }
    }

    /// A `NO_COLOR` run must not emit a single colour escape, whichever mode
    /// was resolved.
    #[test]
    fn no_color_flattens_every_role() {
        let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
        unsafe {
            std::env::set_var("NO_COLOR", "1");
        }
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let theme = Theme::for_mode(mode);
            for role in [
                theme.primary,
                theme.secondary,
                theme.success,
                theme.warning,
                theme.danger,
                theme.muted,
                theme.border,
                theme.surface,
                theme.text,
                theme.inverse,
                theme.code_bg,
                theme.add_fg,
                theme.add_bg,
                theme.del_fg,
                theme.del_bg,
                theme.rail,
                theme.selection,
                theme.overlay,
            ] {
                assert_eq!(role, Color::Reset);
            }
        }
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
        assert_ne!(
            Theme::for_mode_at(ThemeMode::Dark, ColorDepth::TrueColor).text,
            Color::Reset
        );
    }

    #[test]
    fn explicit_choice_pins_the_mode() {
        assert_eq!(ThemeChoice::Dark.resolve(), ThemeMode::Dark);
        assert_eq!(ThemeChoice::Light.resolve(), ThemeMode::Light);
    }
}
