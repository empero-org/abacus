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
    };

    pub fn for_mode(mode: ThemeMode) -> Theme {
        match mode {
            ThemeMode::Dark => Theme::DARK,
            ThemeMode::Light => Theme::LIGHT,
        }
    }
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

    #[test]
    fn colorfgbg_distinguishes_light_and_dark() {
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
    fn explicit_choice_pins_the_mode() {
        assert_eq!(ThemeChoice::Dark.resolve(), ThemeMode::Dark);
        assert_eq!(ThemeChoice::Light.resolve(), ThemeMode::Light);
    }
}
