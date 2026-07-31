//! Styled output for the commands that run outside the TUI — `setup`, `doctor`,
//! and the inline warnings on startup.
//!
//! Those commands print to an ordinary terminal, so they cannot borrow
//! ratatui's styling. This is the thin layer they share instead: a handful of
//! SGR wrappers and the layout primitives (headings, rules, fields, status
//! rows) that keep them looking like the same product as the TUI.
//!
//! Colour is deliberately restricted to the basic sixteen plus bold and dim.
//! These are the codes every terminal renders correctly, and the alternative —
//! duplicating the TUI's quantization here — would buy a nicer violet in
//! exchange for a second thing to keep in step. Whether colour is emitted at
//! all comes from the same [`ColorDepth`](crate::theme::ColorDepth) the TUI
//! uses, so `NO_COLOR` and `ABACUS_COLOR` behave identically in both.

use std::io::{IsTerminal, Write};
use std::sync::OnceLock;

use crate::theme::ColorDepth;
use unicode_width::UnicodeWidthStr;

/// Width the wizard and diagnostics lay out to. Narrow enough for a split pane,
/// wide enough for a URL and a note beside it.
pub const WIDTH: usize = 74;

/// Whether to emit SGR codes at all: only when stdout is a terminal *and* the
/// resolved colour depth is more than nothing.
pub fn colored() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::io::stdout().is_terminal() && ColorDepth::detect() != ColorDepth::None)
}

fn sgr(code: &str, value: &str) -> String {
    if !colored() || value.is_empty() {
        return value.to_owned();
    }
    format!("\x1b[{code}m{value}\x1b[0m")
}

pub fn bold(value: &str) -> String {
    sgr("1", value)
}
pub fn dim(value: &str) -> String {
    sgr("2", value)
}
/// The violet accent's stand-in — magenta is the closest of the sixteen.
pub fn accent(value: &str) -> String {
    sgr("95", value)
}
pub fn ok(value: &str) -> String {
    sgr("92", value)
}
pub fn warn(value: &str) -> String {
    sgr("93", value)
}
pub fn err(value: &str) -> String {
    sgr("91", value)
}
/// Bold accent on a fill, for the wordmark — the console echo of `ui::badge`.
pub fn badge(value: &str) -> String {
    if !colored() {
        return format!("[ {value} ]");
    }
    format!("\x1b[1;45;97m {value} \x1b[0m")
}

/// Glyph set for the plain-terminal commands, following the same ASCII
/// fallback rule as the TUI so a terminal that cannot draw one cannot draw
/// the other either.
pub struct Marks {
    pub pass: &'static str,
    pub fail: &'static str,
    pub warn: &'static str,
    pub rule: &'static str,
    pub bullet: &'static str,
    pub arrow: &'static str,
}

pub fn marks() -> &'static Marks {
    const RICH: Marks = Marks {
        pass: "✓",
        fail: "✗",
        warn: "!",
        rule: "─",
        bullet: "•",
        arrow: "›",
    };
    const PLAIN: Marks = Marks {
        pass: "+",
        fail: "x",
        warn: "!",
        rule: "-",
        bullet: "*",
        arrow: ">",
    };
    static ACTIVE: OnceLock<&Marks> = OnceLock::new();
    ACTIVE.get_or_init(|| {
        if crate::ui::glyphs().wordmark.is_some() {
            &RICH
        } else {
            &PLAIN
        }
    })
}

/// The product banner, printed once at the top of a wizard run.
pub fn banner(subtitle: &str) {
    println!();
    println!("  {}  {}", badge("ABACUS"), dim(subtitle));
    println!("  {}", dim(&marks().rule.repeat(WIDTH)));
}

/// A numbered step header: `2/3  Connect and choose a model`.
pub fn step(index: usize, total: usize, title: &str) {
    println!();
    println!(
        "  {}  {}",
        accent(&bold(&format!("{index}/{total}"))),
        bold(title)
    );
}

/// A muted note under a heading or prompt.
pub fn note(text: &str) {
    println!("       {}", dim(text));
}

pub fn blank() {
    println!();
}

/// A `label   value` row. Indented past the status-glyph column so plain
/// fields and [`check`] rows share one value column.
pub fn field(label: &str, value: &str) {
    println!("    {}  {value}", dim(&format!("{label:<11}")));
}

/// `n thing` / `n things`, for summary lines.
pub fn count(n: usize, singular: &str) -> String {
    if n == 1 {
        format!("{n} {singular}")
    } else {
        format!("{n} {singular}s")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Pass,
    Warn,
    Fail,
}

impl Health {
    fn glyph(self) -> String {
        let marks = marks();
        match self {
            Health::Pass => ok(marks.pass),
            Health::Warn => warn(marks.warn),
            Health::Fail => err(marks.fail),
        }
    }
}

/// A diagnostic row: glyph, aligned label, then the finding.
pub fn check(health: Health, label: &str, detail: &str) {
    println!(
        "  {} {}  {detail}",
        health.glyph(),
        dim(&format!("{label:<11}"))
    );
}

/// A section heading inside `doctor`.
pub fn section(title: &str) {
    println!();
    println!("  {}", bold(title));
}

/// Pad `value` to `width` display columns, measuring in cells so a wide glyph
/// does not push the column that follows it.
pub fn pad(value: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(value);
    if used >= width {
        return value.to_owned();
    }
    format!("{value}{}", " ".repeat(width - used))
}

/// Read a line, showing `default` in the prompt when there is one.
pub fn prompt(label: &str, default: Option<&str>) -> anyhow::Result<String> {
    let rendered = match default {
        Some(default) => format!(
            "  {} {label} {}: ",
            accent(marks().arrow),
            dim(&format!("[{default}]"))
        ),
        None => format!("  {} {label}: ", accent(marks().arrow)),
    };
    print!("{rendered}");
    std::io::stdout().flush()?;
    let mut value = String::new();
    std::io::stdin().read_line(&mut value)?;
    let value = value.trim();
    Ok(if value.is_empty() {
        default.unwrap_or_default().to_owned()
    } else {
        value.to_owned()
    })
}

/// Read a number in `min..=max`, re-asking until it is one.
pub fn prompt_index(label: &str, min: usize, max: usize) -> anyhow::Result<usize> {
    loop {
        let raw = prompt(label, None)?;
        match raw.trim().parse::<usize>() {
            Ok(value) if (min..=max).contains(&value) => return Ok(value),
            _ => println!(
                "      {}",
                err(&format!("Enter a number from {min} to {max}."))
            ),
        }
    }
}

/// Read a yes/no answer, defaulting on an empty line.
pub fn confirm(label: &str, default: bool) -> anyhow::Result<bool> {
    let suffix = if default { "Y/n" } else { "y/N" };
    loop {
        let value = prompt(&format!("{label} {}", dim(&format!("[{suffix}]"))), None)?;
        if value.is_empty() {
            return Ok(default);
        }
        match value.to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("      {}", err("Answer y or n.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styling_is_inert_when_colour_is_off() {
        // Tests do not run against a terminal, so `colored()` is false and every
        // wrapper must be a no-op. A stray escape would corrupt piped output.
        assert!(!colored());
        assert_eq!(bold("x"), "x");
        assert_eq!(accent("x"), "x");
        assert_eq!(badge("ABACUS"), "[ ABACUS ]");
    }

    #[test]
    fn pad_measures_display_width() {
        assert_eq!(pad("ab", 5), "ab   ");
        assert_eq!(pad("abcdef", 3), "abcdef");
        assert_eq!(UnicodeWidthStr::width(pad("日本", 6).as_str()), 6);
    }
}
