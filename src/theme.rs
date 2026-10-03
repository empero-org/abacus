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
/// appearance; `Dark`/`Light` pin it; `Named` loads a file from the themes
/// directory.
///
/// Serialized as a bare string, so the three built-in names round-trip through
/// an existing settings file untouched and any other string is read as a theme
/// name — which is also what makes an unreadable or deleted theme file a
/// recoverable condition rather than a settings file that no longer parses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    #[default]
    Auto,
    Dark,
    Light,
    Named(String),
}

impl ThemeChoice {
    /// Resolve to a concrete mode, detecting the terminal appearance for
    /// `Auto`. A named theme declares its own polarity in the file, so this
    /// answers for the built-ins only and the loader consults the file.
    pub fn resolve(&self) -> ThemeMode {
        match self {
            ThemeChoice::Dark => ThemeMode::Dark,
            ThemeChoice::Light => ThemeMode::Light,
            ThemeChoice::Auto | ThemeChoice::Named(_) => detect_mode().unwrap_or(ThemeMode::Dark),
        }
    }

    pub fn label(&self) -> &str {
        match self {
            ThemeChoice::Auto => "auto",
            ThemeChoice::Dark => "dark",
            ThemeChoice::Light => "light",
            ThemeChoice::Named(name) => name,
        }
    }

    pub fn parse(value: &str) -> ThemeChoice {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => ThemeChoice::Auto,
            "dark" => ThemeChoice::Dark,
            "light" => ThemeChoice::Light,
            _ => ThemeChoice::Named(value.trim().to_owned()),
        }
    }
}

impl serde::Serialize for ThemeChoice {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.label())
    }
}

impl<'de> serde::Deserialize<'de> for ThemeChoice {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Ok(ThemeChoice::parse(&value))
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
    /// Tint band behind a user message, so the prompts that structure the
    /// conversation read as cards without needing a heavier marker.
    pub user_bg: Color,
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
        user_bg: Color::Rgb(33, 28, 46),
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
        user_bg: Color::Rgb(237, 232, 243),
        overlay: Color::Rgb(250, 248, 244),
        plain: false,
    };

    /// The palette with every role reset to the terminal's own colours, for
    /// `NO_COLOR`. Structure — bold, the gutter rails, the badges' reverse
    /// video — carries the whole interface, so it stays legible rather than
    /// merely uncoloured.
    pub fn plain() -> Theme {
        Theme { plain: true, ..Theme::DARK }.map(|_| Color::Reset)
    }

    /// The built-in palette for a mode, before any depth adaptation.
    pub fn base(mode: ThemeMode) -> Theme {
        match mode {
            ThemeMode::Dark => Theme::DARK,
            ThemeMode::Light => Theme::LIGHT,
        }
    }

    pub fn for_mode(mode: ThemeMode) -> Theme {
        Theme::for_mode_at(mode, ColorDepth::detect())
    }

    /// Resolve a palette at a given colour depth. The depth is applied here, at
    /// the single point every palette comes from, so no draw site can emit a
    /// colour the terminal cannot render.
    pub fn for_mode_at(mode: ThemeMode, depth: ColorDepth) -> Theme {
        Theme::base(mode).adapt(mode, depth)
    }

    /// Fit this palette to what the terminal can actually render.
    ///
    /// A loaded theme file goes through exactly this, so a custom palette is
    /// quantized on a 256-colour terminal rather than emitting truecolor
    /// escapes that get approximated unpredictably. At sixteen colours the
    /// custom colours are discarded outright and the role mapping takes over:
    /// there is no meaningful nearest neighbour in a palette that small, and
    /// the mapping at least keeps the interface's structure legible.
    pub fn adapt(self, mode: ThemeMode, depth: ColorDepth) -> Theme {
        match depth {
            ColorDepth::None => Theme::plain(),
            ColorDepth::TrueColor => self,
            ColorDepth::Ansi256 => self.map(quantize_256),
            ColorDepth::Ansi16 => self.map_roles(mode),
        }
    }

    /// Apply `f` to every colour role, leaving `plain` alone.
    fn map(mut self, f: impl Fn(Color) -> Color) -> Theme {
        for role in THEME_ROLES {
            let color = role_color(&self, role).expect("a listed role");
            set_role(&mut self, role, f(color));
        }
        self
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
        let accent = if dark { Color::LightMagenta } else { Color::Magenta };
        let second = if dark { Color::LightRed } else { Color::Red };
        Theme {
            primary: accent,
            secondary: second,
            success: if dark { Color::LightGreen } else { Color::Green },
            warning: if dark { Color::LightYellow } else { Color::Yellow },
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
            add_fg: if dark { Color::LightGreen } else { Color::Green },
            add_bg: Color::Reset,
            del_fg: if dark { Color::LightRed } else { Color::Red },
            del_bg: Color::Reset,
            rail: Color::DarkGray,
            selection: if dark { Color::DarkGray } else { Color::Gray },
            // Same rationale as `surface`: sixteen colours have no subtle band.
            user_bg: Color::Reset,
            overlay: Color::Reset,
            plain: self.plain,
        }
    }
}

// ---------------------------------------------------------------------------
// Theme files
// ---------------------------------------------------------------------------

/// A colour as written in a theme file: `"#b66bff"`, a `vars` entry by name,
/// an xterm-256 index, or `""` for the terminal's own colour.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum ColorValue {
    Index(u8),
    Text(String),
}

/// A theme file: a named palette, then roles pointing into it.
///
/// The indirection is the whole point. A theme is a handful of colours used in
/// twenty places, and a flat role-to-hex map means changing the accent is
/// twenty edits with nineteen chances to miss one. Naming the colours once in
/// `vars` and referring to them by name makes the palette the thing you edit
/// and the roles a description of where it goes.
///
/// Every role is optional and falls back to the built-in palette for `mode`,
/// so a file that sets nothing but `primary` is a complete, valid theme.
///
/// ```json
/// {
///   "name": "midnight",
///   "mode": "dark",
///   "vars": { "violet": "#b66bff", "ink": "#e9e4f0" },
///   "colors": { "primary": "violet", "text": "ink", "border": "" }
/// }
/// ```
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ThemeFile {
    pub name: String,
    /// Which built-in palette the unset roles come from, and which polarity
    /// the sixteen-colour fallback assumes. Defaults to dark.
    pub mode: String,
    pub vars: std::collections::BTreeMap<String, ColorValue>,
    pub colors: std::collections::BTreeMap<String, ColorValue>,
}

/// Every role a theme file may set, paired with a mutable accessor.
///
/// One list, used by the loader, the exporter, and the error message that
/// tells you what you misspelled — so a role can never be loadable but not
/// exportable, or rejected by a spelling the exporter itself produced.
macro_rules! theme_roles {
    ($($name:literal => $field:ident),* $(,)?) => {
        pub const THEME_ROLES: &[&str] = &[$($name),*];

        fn set_role(theme: &mut Theme, role: &str, color: Color) -> bool {
            match role {
                $($name => { theme.$field = color; true })*
                _ => false,
            }
        }

        fn role_color(theme: &Theme, role: &str) -> Option<Color> {
            match role {
                $($name => Some(theme.$field),)*
                _ => None,
            }
        }
    };
}

theme_roles! {
    "primary" => primary,
    "secondary" => secondary,
    "success" => success,
    "warning" => warning,
    "danger" => danger,
    "muted" => muted,
    "border" => border,
    "surface" => surface,
    "text" => text,
    "inverse" => inverse,
    "code_bg" => code_bg,
    "add_fg" => add_fg,
    "add_bg" => add_bg,
    "del_fg" => del_fg,
    "del_bg" => del_bg,
    "rail" => rail,
    "selection" => selection,
    "user_bg" => user_bg,
    "overlay" => overlay,
}

impl ThemeFile {
    fn mode(&self) -> ThemeMode {
        match self.mode.trim().to_ascii_lowercase().as_str() {
            "light" => ThemeMode::Light,
            _ => ThemeMode::Dark,
        }
    }

    /// Build a palette, then fit it to the terminal.
    pub fn to_theme(&self, depth: ColorDepth) -> Result<Theme, String> {
        let mode = self.mode();
        let mut theme = Theme::base(mode);
        for (role, value) in &self.colors {
            let color = self.resolve_value(value, &mut Vec::new())?;
            if !set_role(&mut theme, role, color) {
                return Err(format!("unknown colour role `{role}` — expected one of {}", THEME_ROLES.join(", ")));
            }
        }
        Ok(theme.adapt(mode, depth))
    }

    /// Follow a value to a colour, chasing `vars` references.
    ///
    /// `visited` is the chain so far: a theme that defines `a: "b"` and
    /// `b: "a"` would otherwise recurse until the stack runs out, and a stack
    /// overflow is a much worse way to learn about a typo than a message
    /// naming the loop.
    fn resolve_value(&self, value: &ColorValue, visited: &mut Vec<String>) -> Result<Color, String> {
        let text = match value {
            ColorValue::Index(index) => return Ok(Color::Indexed(*index)),
            ColorValue::Text(text) => text.trim(),
        };
        if text.is_empty() {
            return Ok(Color::Reset);
        }
        if let Some(hex) = text.strip_prefix('#') {
            return parse_hex(hex).ok_or_else(|| format!("`{text}` is not a #rrggbb colour"));
        }
        if visited.iter().any(|seen| seen == text) {
            visited.push(text.to_owned());
            return Err(format!("circular colour reference: {}", visited.join(" → ")));
        }
        let Some(next) = self.vars.get(text) else {
            return Err(format!("no `{text}` in this theme's vars"));
        };
        visited.push(text.to_owned());
        self.resolve_value(next, visited)
    }

    /// A theme file describing `theme` exactly — every role as a literal hex
    /// value, ready to be edited into something else.
    ///
    /// This is the answer to "how do I write one of these": export the palette
    /// you are already looking at and change the colours you care about.
    pub fn from_theme(name: &str, mode: ThemeMode, theme: &Theme) -> ThemeFile {
        let mut colors = std::collections::BTreeMap::new();
        for role in THEME_ROLES {
            let Some(color) = role_color(theme, role) else {
                continue;
            };
            colors.insert((*role).to_owned(), ColorValue::Text(color_to_text(color)));
        }
        ThemeFile {
            name: name.to_owned(),
            mode: match mode {
                ThemeMode::Dark => "dark".to_owned(),
                ThemeMode::Light => "light".to_owned(),
            },
            vars: std::collections::BTreeMap::new(),
            colors,
        }
    }
}

fn parse_hex(hex: &str) -> Option<Color> {
    // `#abc` is the shorthand every stylesheet accepts; refusing it here would
    // be a gratuitous difference from what people already type.
    let expanded;
    let hex = match hex.len() {
        3 => {
            expanded = hex.chars().flat_map(|ch| [ch, ch]).collect::<String>();
            expanded.as_str()
        }
        6 => hex,
        _ => return None,
    };
    let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
    Some(Color::Rgb(channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

fn color_to_text(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Reset => String::new(),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

/// Theme names available under `themes_dir`, sorted, without the built-ins.
pub fn available(themes_dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(themes_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension()? == "json").then(|| path.file_stem()?.to_str().map(str::to_owned)).flatten()
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Read and build a named theme.
pub fn load(themes_dir: &std::path::Path, name: &str, depth: ColorDepth) -> Result<Theme, String> {
    let path = themes_dir.join(format!("{name}.json"));
    let content =
        std::fs::read_to_string(&path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let file: ThemeFile = serde_json::from_str(&content).map_err(|error| format!("{}: {error}", path.display()))?;
    file.to_theme(depth).map_err(|error| format!("{}: {error}", path.display()))
}

/// The palette a choice selects, and what went wrong if anything did.
///
/// A broken theme file must never take the interface down with it — you would
/// have no readable way to fix it. A failed load falls back to the detected
/// built-in and hands the reason back for the status line to carry.
pub fn resolve(choice: &ThemeChoice, themes_dir: &std::path::Path) -> (Theme, Option<String>) {
    let depth = ColorDepth::detect();
    match choice {
        ThemeChoice::Named(name) => match load(themes_dir, name, depth) {
            Ok(theme) => (theme, None),
            Err(error) => (Theme::for_mode_at(ThemeChoice::Auto.resolve(), depth), Some(error)),
        },
        other => (Theme::for_mode_at(other.resolve(), depth), None),
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

    if grey_distance < cube_distance { Color::Indexed(232 + level) } else { Color::Indexed(cube_index) }
}

fn distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> i32 {
    let dr = a.0 as i32 - b.0 as i32;
    let dg = a.1 as i32 - b.1 as i32;
    let db = a.2 as i32 - b.2 as i32;
    dr * dr + dg * dg + db * db
}

static ACTIVE: RwLock<Theme> = RwLock::new(Theme::DARK);

/// The active palette's roles as plain functions, so draw code reads
/// `muted()` rather than `theme::active().muted` at every call site.
macro_rules! roles {
    ($($role:ident),*) => {
        $(pub fn $role() -> Color {
            active().$role
        })*
    };
}
roles!(primary, secondary, success, warning, danger, muted, border, surface, text, inverse, rail);

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
    Some(if index == 7 || index >= 9 { ThemeMode::Light } else { ThemeMode::Dark })
}

#[cfg(target_os = "macos")]
fn mode_from_macos_appearance() -> Option<ThemeMode> {
    // `defaults read -g AppleInterfaceStyle` prints "Dark" in dark mode and
    // exits non-zero (key absent) in light mode. Safe, fast, no TTY probing.
    let output = std::process::Command::new("defaults").args(["read", "-g", "AppleInterfaceStyle"]).output().ok()?;
    if !output.status.success() {
        return Some(ThemeMode::Light);
    }
    if String::from_utf8_lossy(&output.stdout).trim().eq_ignore_ascii_case("dark") {
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
        for role in [theme.primary, theme.success, theme.warning, theme.danger, theme.text] {
            assert!(matches!(role, Color::Indexed(_)), "{role:?} not quantized");
        }
        let distinct = [theme.primary, theme.success, theme.warning, theme.danger, theme.muted, theme.text];
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
                theme.primary, theme.secondary, theme.success, theme.warning, theme.danger, theme.muted, theme.border,
                theme.surface, theme.text, theme.inverse, theme.code_bg, theme.add_fg, theme.add_bg, theme.del_fg,
                theme.del_bg, theme.rail, theme.selection, theme.overlay,
            ] {
                assert_eq!(role, Color::Reset);
            }
        }
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
        assert_ne!(Theme::for_mode_at(ThemeMode::Dark, ColorDepth::TrueColor).text, Color::Reset);
    }

    fn theme_file(json: &str) -> ThemeFile {
        serde_json::from_str(json).expect("valid theme json")
    }

    #[test]
    fn a_theme_file_resolves_vars_and_inherits_the_rest() {
        let file = theme_file(
            r##"{
                "name": "midnight",
                "mode": "dark",
                "vars": { "violet": "#b66bff", "accent": "violet" },
                "colors": { "primary": "accent", "surface": 234, "border": "" }
            }"##,
        );
        let theme = file.to_theme(ColorDepth::TrueColor).expect("loads");
        // A var chain resolves all the way down to the literal.
        assert_eq!(theme.primary, Color::Rgb(0xb6, 0x6b, 0xff));
        assert_eq!(theme.surface, Color::Indexed(234));
        assert_eq!(theme.border, Color::Reset, "empty means the terminal's own");
        // Everything unset comes from the built-in palette for the mode.
        assert_eq!(theme.text, Theme::DARK.text);
        assert_eq!(theme.success, Theme::DARK.success);
    }

    #[test]
    fn a_theme_file_reports_what_is_wrong_with_it() {
        // A cycle names the loop rather than overflowing the stack.
        let cyclic = theme_file(r##"{ "vars": { "a": "b", "b": "a" }, "colors": { "primary": "a" } }"##);
        let error = cyclic.to_theme(ColorDepth::TrueColor).unwrap_err();
        assert!(error.contains("circular"), "{error}");

        let missing = theme_file(r##"{ "colors": { "primary": "nope" } }"##);
        assert!(missing.to_theme(ColorDepth::TrueColor).unwrap_err().contains("vars"));

        let unknown = theme_file(r##"{ "colors": { "primry": "#ffffff" } }"##);
        let error = unknown.to_theme(ColorDepth::TrueColor).unwrap_err();
        assert!(error.contains("primry") && error.contains("primary"), "{error}");

        let bad_hex = theme_file(r##"{ "colors": { "primary": "#gg00zz" } }"##);
        assert!(bad_hex.to_theme(ColorDepth::TrueColor).unwrap_err().contains("#rrggbb"));
    }

    #[test]
    fn a_custom_palette_is_still_fitted_to_the_terminal() {
        let file = theme_file(r##"{ "colors": { "primary": "#b66bff" } }"##);
        // Truecolor keeps it; 256 quantizes it; sixteen falls back to the role
        // mapping, which cannot honour an arbitrary colour at all.
        assert!(matches!(file.to_theme(ColorDepth::TrueColor).unwrap().primary, Color::Rgb(..)));
        assert!(matches!(file.to_theme(ColorDepth::Ansi256).unwrap().primary, Color::Indexed(_)));
        assert_eq!(file.to_theme(ColorDepth::Ansi16).unwrap().primary, Color::LightMagenta);
        assert!(file.to_theme(ColorDepth::None).unwrap().plain);
    }

    #[test]
    fn an_exported_palette_loads_back_as_itself() {
        // The export is the documentation, so it has to be a file the loader
        // accepts — and one that round-trips without drift.
        let file = ThemeFile::from_theme("mine", ThemeMode::Dark, &Theme::DARK);
        let json = serde_json::to_string(&file).expect("serializes");
        let reloaded: ThemeFile = serde_json::from_str(&json).expect("parses");
        let theme = reloaded.to_theme(ColorDepth::TrueColor).expect("loads");
        assert_eq!(theme.primary, Theme::DARK.primary);
        assert_eq!(theme.user_bg, Theme::DARK.user_bg);
        assert_eq!(theme.rail, Theme::DARK.rail);
        assert_eq!(file.colors.len(), THEME_ROLES.len(), "every role exported");
    }

    #[test]
    fn shorthand_hex_is_accepted() {
        let file = theme_file(r##"{ "colors": { "primary": "#f0a" } }"##);
        assert_eq!(file.to_theme(ColorDepth::TrueColor).unwrap().primary, Color::Rgb(0xff, 0x00, 0xaa));
    }

    #[test]
    fn a_theme_choice_is_a_bare_string_in_the_settings_file() {
        for (text, expected) in [
            ("auto", ThemeChoice::Auto),
            ("dark", ThemeChoice::Dark),
            ("light", ThemeChoice::Light),
            ("midnight", ThemeChoice::Named("midnight".to_owned())),
        ] {
            assert_eq!(ThemeChoice::parse(text), expected);
            let map = std::collections::BTreeMap::from([("theme", &expected)]);
            let toml = toml::to_string(&map).expect("serializes");
            assert!(toml.contains(text), "{toml}");
        }
    }

    #[test]
    fn a_broken_theme_falls_back_instead_of_taking_the_screen_with_it() {
        let directory = std::env::temp_dir().join(format!("abacus-theme-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("temp dir");
        std::fs::write(directory.join("broken.json"), "{ not json").expect("write");
        std::fs::write(directory.join("good.json"), r##"{ "colors": { "primary": "#123456" } }"##).expect("write");

        assert_eq!(available(&directory), vec!["broken", "good"]);
        let (_, error) = resolve(&ThemeChoice::Named("broken".to_owned()), &directory);
        assert!(error.is_some(), "a parse failure is reported");
        let (_, error) = resolve(&ThemeChoice::Named("absent".to_owned()), &directory);
        assert!(error.is_some(), "so is a missing file");
        let (_, error) = resolve(&ThemeChoice::Named("good".to_owned()), &directory);
        assert_eq!(error, None);
        // A directory that does not exist is simply "no themes", not an error.
        assert!(available(&directory.join("nowhere")).is_empty());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn explicit_choice_pins_the_mode() {
        assert_eq!(ThemeChoice::Dark.resolve(), ThemeMode::Dark);
        assert_eq!(ThemeChoice::Light.resolve(), ThemeMode::Light);
    }
}
