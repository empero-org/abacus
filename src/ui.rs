//! Presentation primitives for the TUI.
//!
//! `tui.rs` owns application state and event handling; this module owns how
//! things *look*. Everything here is pure — it takes plain data and returns
//! ratatui `Line`/`Span`/`Text` values — so the visual language can be reasoned
//! about (and unit-tested) without standing up an `App`.
//!
//! Two conventions hold across the whole interface:
//!
//! * **A two-column gutter.** Every transcript block reserves columns 0–1 for a
//!   marker and starts its content at column 2. User turns carry a solid accent
//!   rail, assistant prose is unadorned, and tool calls are compact status rows.
//!   The eye can scan the left edge alone and follow the conversation.
//! * **Exact wrapping.** Lines are wrapped here, to a known width, instead of
//!   being handed to ratatui's `Wrap`. That costs a wrap pass but makes the
//!   rendered line count exact, which is what lets scrolling land precisely
//!   instead of drifting against an estimate.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding};
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::theme;

/// Content is inset two columns from the marker gutter.
pub const GUTTER: usize = 2;

/// Every chrome glyph the interface draws, with a same-width ASCII stand-in.
///
/// Terminals that lack a font for box-drawing, braille, or geometric characters
/// render them as blanks — or, worse, as double-width cells, which shifts every
/// column to their right and breaks the alignment the layout depends on. Rather
/// than scatter literals through the draw code and hope, every glyph is named
/// here and paired with a fallback that occupies the same number of cells. The
/// `glyph_pairs_are_width_stable` test is what makes swapping between them safe.
///
/// `ABACUS_ASCII=1` forces the fallback; a `dumb` or `linux` `TERM` selects it
/// automatically.
#[derive(Debug, Clone, Copy)]
pub struct Glyphs {
    /// Solid rail marking a user turn, and the selected row of a list.
    pub bar: &'static str,
    pub prompt: &'static str,
    pub ok: &'static str,
    pub failed: &'static str,
    pub paused: &'static str,
    pub notice: &'static str,
    pub spinner: [&'static str; 10],
    /// Shown in place of a spinner frame when animations are off.
    pub still: &'static str,
    pub rule: &'static str,
    pub meter_full: &'static str,
    pub meter_empty: &'static str,
    pub thumb: &'static str,
    pub track: &'static str,
    pub separator: &'static str,
    /// Fold state of a tool row that has more output behind it.
    pub fold_closed: &'static str,
    pub fold_open: &'static str,
    /// Rail beside quoted prose — heavier than `track` so a block quote is
    /// never mistaken for a code block.
    pub quote_rail: &'static str,
    pub branch: &'static str,
    pub queued: &'static str,
    pub attached: &'static str,
    pub goal: &'static str,
    pub repeat: &'static str,
    pub tasks: &'static str,
    pub down: &'static str,
    /// Vertical ellipsis marking an elided gap between diff hunks.
    pub gap: &'static str,
    /// Half-block wordmark rows, or `None` where the blocks won't render and
    /// the splash falls back to plain letters.
    pub wordmark: Option<[&'static str; 2]>,
}

impl Glyphs {
    pub const UNICODE: Glyphs = Glyphs {
        bar: "▌",
        prompt: "❯",
        ok: "✓",
        failed: "✗",
        paused: "‖",
        notice: "·",
        // Ten frames at ~90ms reads as a smooth rotation without the strobing
        // that shorter cycles produce on slow terminals.
        spinner: ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
        still: "•",
        rule: "─",
        meter_full: "▰",
        meter_empty: "▱",
        thumb: "▐",
        track: "│",
        separator: "·",
        fold_closed: "▸",
        fold_open: "▾",
        quote_rail: "┃",
        branch: "⑂",
        queued: "⧗",
        attached: "⧉",
        goal: "◆",
        repeat: "↻",
        tasks: "▦",
        down: "↓",
        gap: "⋮",
        wordmark: Some(["▄▀█ █▄▄ ▄▀█ █▀▀ █ █ █▀", "█▀█ █▄█ █▀█ █▄▄ █▄█ ▄█"]),
    };

    pub const ASCII: Glyphs = Glyphs {
        bar: "|",
        prompt: ">",
        ok: "+",
        failed: "x",
        paused: "=",
        notice: "-",
        spinner: ["|", "/", "-", "\\", "|", "/", "-", "\\", "|", "/"],
        still: "*",
        rule: "-",
        meter_full: "#",
        meter_empty: "-",
        thumb: "#",
        track: "|",
        separator: ".",
        fold_closed: ">",
        fold_open: "v",
        quote_rail: "<",
        branch: "#",
        queued: "~",
        attached: "@",
        goal: "*",
        repeat: "@",
        tasks: "#",
        down: "v",
        gap: ":",
        wordmark: None,
    };
}

/// The active glyph set, resolved once from the environment.
pub fn glyphs() -> &'static Glyphs {
    static ACTIVE: std::sync::OnceLock<Glyphs> = std::sync::OnceLock::new();
    ACTIVE.get_or_init(|| {
        let forced = std::env::var("ABACUS_ASCII")
            .map(|value| matches!(value.trim(), "1" | "true" | "yes"))
            .unwrap_or(false);
        let bare = std::env::var("TERM")
            .map(|term| matches!(term.trim(), "dumb" | "linux"))
            .unwrap_or(false);
        if forced || bare {
            Glyphs::ASCII
        } else {
            Glyphs::UNICODE
        }
    })
}

/// The status header with a highlight band sweeping across it — the "alive"
/// signal for a running turn. A cosine band (half-width 5 columns, 2-second
/// period) brightens each character from `muted` toward `text`, bold at the
/// peak. Truecolor themes blend smoothly; quantized palettes step through
/// DIM → normal → BOLD; animations off returns one plain span.
pub fn shimmer(text: &str, elapsed: std::time::Duration, animated: bool) -> Vec<Span<'static>> {
    let palette = theme::active();
    if !animated || text.is_empty() {
        return vec![Span::styled(
            text.to_owned(),
            Style::default().fg(palette.text),
        )];
    }
    const PERIOD_MS: u128 = 2_000;
    const HALF_WIDTH: f64 = 5.0;
    let characters: Vec<char> = text.chars().collect();
    let phase = (elapsed.as_millis() % PERIOD_MS) as f64 / PERIOD_MS as f64;
    let sweep = phase * (characters.len() as f64 + 2.0 * HALF_WIDTH) - HALF_WIDTH;
    let blend = match (palette.muted, palette.text) {
        (Color::Rgb(br, bg, bb), Color::Rgb(tr, tg, tb)) => Some(((br, bg, bb), (tr, tg, tb))),
        _ => None,
    };
    characters
        .into_iter()
        .enumerate()
        .map(|(index, ch)| {
            let distance = (index as f64 - sweep).abs();
            let intensity = if distance <= HALF_WIDTH {
                ((distance / HALF_WIDTH) * std::f64::consts::FRAC_PI_2).cos()
            } else {
                0.0
            };
            let mut style = match blend {
                Some(((br, bg, bb), (tr, tg, tb))) => {
                    let mix = |from: u8, to: u8| {
                        (f64::from(from) + (f64::from(to) - f64::from(from)) * intensity).round()
                            as u8
                    };
                    Style::default().fg(Color::Rgb(mix(br, tr), mix(bg, tg), mix(bb, tb)))
                }
                None if intensity < 0.33 => Style::default()
                    .fg(palette.muted)
                    .add_modifier(Modifier::DIM),
                None => Style::default().fg(palette.text),
            };
            if intensity > 0.85 {
                style = style.add_modifier(Modifier::BOLD);
            }
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

/// The spinner frame for `elapsed`, or a static bullet when the user has turned
/// animations off.
pub fn spinner_frame(elapsed: std::time::Duration, animated: bool) -> &'static str {
    let set = glyphs();
    if !animated {
        return set.still;
    }
    set.spinner[(elapsed.as_millis() / 90) as usize % set.spinner.len()]
}

/// Number of spinner frames, for callers keying a cache on the current phase.
pub const SPINNER_FRAMES: usize = 10;

fn primary() -> Color {
    theme::active().primary
}
fn secondary() -> Color {
    theme::active().secondary
}
fn success() -> Color {
    theme::active().success
}
fn warning() -> Color {
    theme::active().warning
}
fn danger() -> Color {
    theme::active().danger
}
fn muted() -> Color {
    theme::active().muted
}
fn text_color() -> Color {
    theme::active().text
}
fn rail() -> Color {
    theme::active().rail
}

// ---------------------------------------------------------------------------
// Small shared pieces
// ---------------------------------------------------------------------------

/// A filled chip — bold inverse text on an accent fill. Used for the wordmark,
/// mode indicator, and modal titles so a status reads at a glance.
pub fn badge(label: &str, fill: Color) -> Span<'static> {
    Span::styled(format!(" {label} "), fill_style(fill))
}

/// The style for a filled chip or a selected row: an accent fill normally,
/// reverse video when the palette carries no colour.
pub fn fill_style(fill: Color) -> Style {
    let palette = theme::active();
    if palette.plain {
        return Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
    }
    Style::default()
        .fg(palette.inverse)
        .bg(fill)
        .add_modifier(Modifier::BOLD)
}

/// The `·` used to separate inline metadata. Dim enough to group without
/// competing with the values on either side.
pub fn dot() -> Span<'static> {
    Span::styled(
        format!("  {}  ", glyphs().separator),
        Style::default().fg(rail()),
    )
}

/// `key label` pairs for a hint strip: the key in the accent, the description
/// muted. Rendered with two spaces between pairs.
pub fn hints(pairs: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(pairs.len() * 3);
    for (index, (key, label)) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("   ", Style::default()));
        }
        spans.push(Span::styled(
            (*key).to_owned(),
            Style::default().fg(primary()).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(muted()),
        ));
    }
    spans
}

/// A horizontal hairline in the rail colour, used to separate the header and
/// footer from the transcript without boxing anything in.
pub fn rule(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        glyphs().rule.repeat(width as usize),
        Style::default().fg(rail()),
    ))
}

/// A segmented capacity meter, e.g. `▰▰▰▱▱▱▱▱`. Filled cells take `color`,
/// empty cells the rail colour so the track stays visible when nearly empty.
pub fn meter(percent: u16, cells: usize, color: Color) -> Vec<Span<'static>> {
    let set = glyphs();
    let filled = ((percent as usize * cells) / 100).min(cells);
    vec![
        Span::styled(set.meter_full.repeat(filled), Style::default().fg(color)),
        Span::styled(
            set.meter_empty.repeat(cells - filled),
            Style::default().fg(rail()),
        ),
    ]
}

/// Truncate to `max` display columns, appending `…` when it doesn't fit. Width
/// is measured in cells, so CJK and emoji don't overrun the column they're
/// budgeted for.
pub fn truncate(value: &str, max: usize) -> String {
    let flat = value.replace(['\n', '\r', '\t'], " ");
    if UnicodeWidthStr::width(flat.as_str()) <= max {
        return flat;
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in flat.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// Human-scaled duration for tool rows: sub-second in ms, then seconds, then
/// minutes. Keeps the right-hand column narrow and comparable.
pub fn format_elapsed(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if ms < 3_600_000 {
        let total = ms / 1_000;
        format!("{}m {:02}s", total / 60, total % 60)
    } else {
        let total = ms / 1_000;
        format!(
            "{}h {:02}m {:02}s",
            total / 3600,
            (total % 3600) / 60,
            total % 60
        )
    }
}

/// Thousands-scaled counts for the token readout — `938`, `12.4k`, `3.1M`.
pub fn format_count(value: u64) -> String {
    if value < 1_000 {
        value.to_string()
    } else if value < 1_000_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    }
}

// ---------------------------------------------------------------------------
// Overlay framing
// ---------------------------------------------------------------------------

/// The shared frame for every modal and popup: a rounded hairline border, a
/// filled title badge, and an optional muted hint strip along the bottom edge.
///
/// Overlays deliberately all use the same border colour — the accent lives in
/// the title badge alone. A screen where each panel outlines itself in a
/// different hue reads as a toy; restraint is what makes it read as a tool.
pub fn overlay_block(title: &str, accent: Color, footer: Option<Line<'static>>) -> Block<'static> {
    let palette = theme::active();
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(palette.border))
        .style(Style::default().bg(palette.overlay))
        .padding(Padding::horizontal(1))
        .title_top(Line::from(vec![
            Span::raw(" "),
            badge(title, accent),
            Span::raw(" "),
        ]));
    if let Some(footer) = footer {
        block = block.title_bottom(footer);
    }
    block
}

/// Bottom-edge hint strip for an overlay, wrapped in spaces so it doesn't touch
/// the corners.
pub fn overlay_hints(pairs: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(hints(pairs));
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// A centred rect of at most `width` × `height`, always leaving a one-cell
/// margin so an overlay never bleeds into the screen edge.
pub fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// Cap a full-width region at `max` columns and centre it. Long measures are
/// hard to read; the transcript and composer share this so they stay aligned
/// with each other on a wide terminal.
pub fn measure(area: Rect, max: u16) -> Rect {
    if area.width <= max {
        return area;
    }
    Rect {
        x: area.x + (area.width - max) / 2,
        width: max,
        ..area
    }
}

// ---------------------------------------------------------------------------
// Wrapping
// ---------------------------------------------------------------------------

/// Total display width of a run of spans.
pub fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// Word-wrap `spans` to `width` columns, prefixing the first produced row with
/// `first` and every continuation row with `cont`.
///
/// Breaks at the last space that fits; a single token longer than the measure
/// is broken hard rather than allowed to overflow. Styles survive the split, so
/// a bold run that straddles a line break stays bold on both rows.
pub fn wrap(
    spans: &[Span<'static>],
    width: usize,
    first: &[Span<'static>],
    cont: &[Span<'static>],
) -> Vec<Line<'static>> {
    let indent = spans_width(first).max(spans_width(cont));
    let avail = width.saturating_sub(indent).max(1);

    // Flatten to styled cells once so the wrap loop can look backwards for a
    // break point without caring about span boundaries.
    let mut cells: Vec<(char, Style)> = Vec::new();
    for span in spans {
        for ch in span.content.chars() {
            cells.push((ch, span.style));
        }
    }

    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut row: Vec<(char, Style)> = Vec::new();
    let mut row_width = 0usize;
    let mut last_space: Option<usize> = None;

    for (ch, style) in cells {
        if ch == '\n' {
            rows.push(std::mem::take(&mut row));
            row_width = 0;
            last_space = None;
            continue;
        }
        let cell_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if row_width + cell_width > avail && !row.is_empty() {
            match last_space {
                // Break at the space, dropping it: the trailing space would
                // otherwise show up as a ragged right edge on a selected row.
                Some(at) if at > 0 => {
                    let tail = row.split_off(at);
                    rows.push(std::mem::take(&mut row));
                    row = tail.into_iter().skip(1).collect();
                }
                _ => rows.push(std::mem::take(&mut row)),
            }
            row_width = row
                .iter()
                .map(|(ch, _)| UnicodeWidthChar::width(*ch).unwrap_or(0))
                .sum();
            last_space = None;
        }
        if ch == ' ' {
            last_space = Some(row.len());
        }
        row.push((ch, style));
        row_width += cell_width;
    }
    rows.push(row);

    rows.into_iter()
        .enumerate()
        .map(|(index, cells)| {
            let mut line: Vec<Span<'static>> = if index == 0 {
                first.to_vec()
            } else {
                cont.to_vec()
            };
            // Regroup consecutive cells that share a style back into spans, so
            // the buffer sees a handful of runs instead of one span per char.
            let mut current = String::new();
            let mut current_style: Option<Style> = None;
            for (ch, style) in cells {
                if current_style != Some(style) {
                    if let Some(previous) = current_style.take()
                        && !current.is_empty()
                    {
                        line.push(Span::styled(std::mem::take(&mut current), previous));
                    }
                    current_style = Some(style);
                }
                current.push(ch);
            }
            if let Some(style) = current_style
                && !current.is_empty()
            {
                line.push(Span::styled(current, style));
            }
            Line::from(line)
        })
        .collect()
}

/// Convenience wrapper for plain indented text with no marker.
fn wrap_plain(spans: &[Span<'static>], width: usize, indent: usize) -> Vec<Line<'static>> {
    let pad = vec![Span::raw(" ".repeat(indent))];
    wrap(spans, width, &pad, &pad)
}

// ---------------------------------------------------------------------------
// Transcript model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    User,
    /// The model's reasoning, kept visually subordinate to its answer.
    Thinking,
    Assistant,
    Tool,
    System,
    Error,
    /// A labelled horizontal rule — "Worked for 2m 03s" after a heavy turn —
    /// so long sessions get scannable structure between work blocks.
    Rule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolStatus {
    Running,
    Ok,
    Failed,
}

/// A tool invocation as the transcript shows it: what ran, against what, how it
/// ended, and how long it took. Keeping these as fields rather than a
/// pre-formatted string is what lets the row show a live spinner, right-align
/// the duration, and colour the outcome.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub summary: String,
    pub status: ToolStatus,
    /// Short preview shown while the row is collapsed.
    pub output: String,
    /// The complete result, retained so the row can be opened. Bounded by
    /// `MAX_RETAINED_OUTPUT` — the collapsed preview is what the transcript
    /// shows by default, and a tool that returns megabytes should not be able
    /// to grow the session's footprint without limit.
    pub full: String,
    pub duration_ms: Option<u64>,
    pub expanded: bool,
}

/// Ceiling on the output retained per tool call for expansion.
pub const MAX_RETAINED_OUTPUT: usize = 64 * 1024;

/// Rows an expanded tool call may occupy before it is cut short. Without a cap
/// a single large result would push every other block out of the scrollback.
const MAX_EXPANDED_ROWS: usize = 400;

impl ToolCall {
    /// Whether opening this row would show anything the collapsed row does not.
    pub fn has_more(&self) -> bool {
        self.full.trim() != self.output.trim() && !self.full.trim().is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: EntryKind,
    pub text: String,
    pub tool: Option<ToolCall>,
}

impl Entry {
    pub fn new(kind: EntryKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            tool: None,
        }
    }

    pub fn tool(call: ToolCall) -> Self {
        Self {
            kind: EntryKind::Tool,
            text: String::new(),
            tool: Some(call),
        }
    }
}

/// A wrapped transcript: the rows to draw, plus where each entry starts and
/// ends within them so a caller can scroll a particular block into view.
#[derive(Debug, Default)]
pub struct Transcript {
    pub lines: Vec<Line<'static>>,
    /// `(first row, row count)` per entry, parallel to the input slice.
    pub spans: Vec<(usize, usize)>,
}

/// Render the whole transcript at `width`. The rows are already wrapped, so
/// `lines.len()` is exactly the height it will occupy — callers can use it
/// directly as the scroll extent.
///
/// `cursor` is the entry the selection sits on, which is drawn brighter than
/// its neighbours; passing `None` renders every block evenly.
pub fn transcript(
    entries: &[Entry],
    width: usize,
    spinner: &str,
    cursor: Option<usize>,
    show_thinking: bool,
) -> Transcript {
    let width = width.max(8);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans = Vec::with_capacity(entries.len());
    let mut previous: Option<EntryKind> = None;

    for (index, entry) in entries.iter().enumerate() {
        // Hiding reasoning hides ALL of it, including blocks already
        // streamed — that is what makes the toggle useful for reading a busy
        // transcript. A zero-height span keeps entry indices aligned for the
        // cursor and click hit-testing.
        if entry.kind == EntryKind::Thinking && !show_thinking {
            spans.push((lines.len(), 0));
            continue;
        }
        // One blank line between blocks, except between consecutive tool rows —
        // a chain of tool calls reads better as a tight group.
        if let Some(previous) = previous {
            let both_tools = previous == EntryKind::Tool && entry.kind == EntryKind::Tool;
            if !both_tools {
                lines.push(Line::from(""));
            }
        }
        let start = lines.len();
        let selected = cursor == Some(index);
        match entry.kind {
            EntryKind::User => lines.extend(user_block(&entry.text, width)),
            EntryKind::Thinking => lines.extend(thinking_block(&entry.text, width)),
            EntryKind::Assistant => lines.extend(assistant_block(&entry.text, width)),
            EntryKind::Tool => {
                if let Some(call) = &entry.tool {
                    lines.extend(tool_block(call, width, spinner, selected));
                }
            }
            EntryKind::System => {
                lines.extend(notice_block(&entry.text, width, muted(), glyphs().notice))
            }
            EntryKind::Error => {
                lines.extend(notice_block(&entry.text, width, danger(), glyphs().failed))
            }
            EntryKind::Rule => lines.push(rule_line(&entry.text, width)),
        }
        // Selected blocks are deliberately not tinted. A click used to paint a
        // whole block in the selection colour with no way to undo it — clicking
        // again folded rather than released — and the tint fought the terminal's
        // own selection, which is the one users actually copy with.
        let _ = selected;
        spans.push((start, lines.len() - start));
        previous = Some(entry.kind);
    }
    Transcript { lines, spans }
}

/// The user's turn, marked by a solid accent rail down the left edge. The rail
/// repeats on wrapped rows so a long prompt stays visually bounded.
fn user_block(body: &str, width: usize) -> Vec<Line<'static>> {
    let tint = theme::active().user_bg;
    let bar = vec![Span::styled(
        format!("{} ", glyphs().bar),
        Style::default().fg(primary()).add_modifier(Modifier::BOLD),
    )];
    let mut lines = Vec::new();
    for paragraph in body.split('\n') {
        let spans = vec![Span::styled(
            paragraph.to_owned(),
            Style::default().fg(text_color()),
        )];
        lines.extend(wrap(&spans, width, &bar, &bar));
    }
    // The prompt renders as a card: a quiet tint band behind the whole block,
    // padded to the measure and given a breathing row above and below, so the
    // messages that structure the conversation stand out at a glance. With no
    // tint in the palette (16-colour, NO_COLOR) the rail alone carries it and
    // the padding rows are omitted rather than wasting two blank lines.
    if tint == Color::Reset {
        return lines;
    }
    let pad_row = || Line::from(Span::styled(" ".repeat(width), Style::default().bg(tint)));
    let mut card = vec![pad_row()];
    for mut line in lines {
        let used: usize = line
            .spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();
        for span in &mut line.spans {
            span.style = span.style.bg(tint);
        }
        if used < width {
            line.spans.push(Span::styled(
                " ".repeat(width - used),
                Style::default().bg(tint),
            ));
        }
        card.push(line);
    }
    card.push(pad_row());
    card
}

/// The model thinking aloud. Dimmed and italic behind a quiet rail, so it reads
/// as working-out rather than as the answer — it sits above the reply it led to
/// and must not compete with it.
fn thinking_block(body: &str, width: usize) -> Vec<Line<'static>> {
    let quiet = rail();
    let marker = vec![Span::styled(
        format!("{} ", glyphs().quote_rail),
        Style::default().fg(quiet),
    )];
    let style = Style::default().fg(quiet).add_modifier(Modifier::ITALIC);
    let mut lines = Vec::new();
    for paragraph in body.split('\n') {
        if paragraph.trim().is_empty() {
            continue;
        }
        lines.extend(wrap(
            &[Span::styled(paragraph.to_owned(), style)],
            width,
            &marker,
            &marker,
        ));
    }
    lines
}

/// Assistant prose: rendered markdown, indented into the content column and
/// otherwise unadorned. This is the material the user actually reads, so it
/// gets no competing decoration.
fn assistant_block(body: &str, width: usize) -> Vec<Line<'static>> {
    let palette = theme::active();
    // Markdown is rendered to the content measure, not the full width, so a
    // fenced block's rules line up with the text they wrap rather than being
    // re-wrapped a moment later.
    let measure = width.saturating_sub(GUTTER);
    let set = glyphs();
    let rendered = crate::markdown::render_at(
        body,
        crate::markdown::MarkdownTheme {
            text: palette.text,
            muted: palette.muted,
            heading: palette.secondary,
            accent: palette.primary,
            code: palette.success,
            code_background: palette.code_bg,
            quote: palette.primary,
            link: palette.primary,
            code_rail: set.track,
            quote_rail: set.quote_rail,
        },
        measure,
    );
    if rendered.lines.is_empty() {
        return vec![Line::from("")];
    }
    let mut lines = Vec::new();
    for line in rendered.lines {
        lines.extend(wrap_plain(&line.spans, width, GUTTER));
    }
    lines
}

/// A tool call: status glyph, name, argument summary, and a right-aligned
/// duration, followed by its output.
///
/// ```text
///   ✓ read_file  src/parser.rs                        240ms
///     1203 lines
/// ```
///
/// Collapsed, the row shows the short preview and a `+` affordance when there
/// is more behind it. Expanded, it shows the retained output in full, capped so
/// one large result cannot bury the rest of the scrollback.
fn tool_block(call: &ToolCall, width: usize, spinner: &str, selected: bool) -> Vec<Line<'static>> {
    let set = glyphs();
    let (glyph, color) = match call.status {
        ToolStatus::Running => (spinner, primary()),
        ToolStatus::Ok => (set.ok, success()),
        ToolStatus::Failed => (set.failed, danger()),
    };
    let mut right = match call.status {
        ToolStatus::Running => "running".to_owned(),
        _ => call.duration_ms.map(format_elapsed).unwrap_or_default(),
    };
    // Say that there is more to see, and which way the row is currently folded.
    if call.has_more() {
        right = format!(
            "{} {right}",
            if call.expanded {
                set.fold_open
            } else {
                set.fold_closed
            }
        );
    }

    // Budget the row: the glyph, name, and duration are fixed; whatever is left
    // belongs to the argument summary.
    let inner = width.saturating_sub(GUTTER);
    let name_width = UnicodeWidthStr::width(call.name.as_str());
    let right_width = UnicodeWidthStr::width(right.as_str());
    let fixed = 2 + name_width + 2 + right_width + 1;
    let summary = if call.summary.is_empty() || inner <= fixed {
        String::new()
    } else {
        truncate(&call.summary, inner - fixed)
    };
    let used = 2 + name_width + 2 + UnicodeWidthStr::width(summary.as_str());
    let pad = inner.saturating_sub(used + right_width).max(1);

    // The selected row is tinted end to end so the cursor is unmistakable even
    // when the transcript is a dense run of tool calls.
    let fill = if selected {
        theme::active().selection
    } else {
        Color::Reset
    };
    let tint = |style: Style| {
        if selected { style.bg(fill) } else { style }
    };

    let mut header = vec![
        Span::styled(" ".repeat(GUTTER), tint(Style::default())),
        Span::styled(
            format!("{glyph} "),
            tint(Style::default().fg(color).add_modifier(Modifier::BOLD)),
        ),
        Span::styled(
            call.name.clone(),
            tint(
                Style::default()
                    .fg(text_color())
                    .add_modifier(Modifier::BOLD),
            ),
        ),
    ];
    if !summary.is_empty() {
        header.push(Span::styled("  ", tint(Style::default())));
        header.push(Span::styled(summary, tint(Style::default().fg(muted()))));
    }
    if !right.is_empty() {
        header.push(Span::styled(" ".repeat(pad), tint(Style::default())));
        header.push(Span::styled(right, tint(Style::default().fg(rail()))));
    }

    let mut lines = vec![Line::from(header)];
    let body_style = if call.status == ToolStatus::Failed {
        Style::default().fg(danger())
    } else {
        Style::default().fg(muted())
    };
    let body = if call.expanded {
        &call.full
    } else {
        &call.output
    };
    let indent = GUTTER + 2;
    let measure = width.saturating_sub(indent);
    for raw in body.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        if lines.len() > MAX_EXPANDED_ROWS {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled("… output truncated".to_owned(), Style::default().fg(rail())),
            ]));
            break;
        }
        if call.expanded {
            // Expanded output is wrapped rather than clipped: the point of
            // opening the row is to read what it says.
            lines.extend(wrap(
                &[Span::styled(raw.to_owned(), body_style)],
                width,
                &[Span::raw(" ".repeat(indent))],
                &[Span::raw(" ".repeat(indent))],
            ));
        } else {
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(indent)),
                Span::styled(truncate(raw, measure), body_style),
            ]));
        }
    }
    lines
}

/// System notices and errors: a coloured glyph in the gutter and body text that
/// hangs under the content column.
/// A full-width dim rule with the label embedded near its left end:
/// `─ Worked for 2m 03s ───────────…`. The label doubles as the information —
/// the rule only appears where real work happened.
fn rule_line(label: &str, width: usize) -> Line<'static> {
    let bar = glyphs().rule;
    let mut text = format!("{bar} {label} ");
    let used = UnicodeWidthStr::width(text.as_str());
    text.push_str(&bar.repeat(width.saturating_sub(used).max(2) / bar.width().max(1)));
    Line::from(Span::styled(text, Style::default().fg(muted())))
}

fn notice_block(body: &str, width: usize, color: Color, glyph: &str) -> Vec<Line<'static>> {
    let first = vec![
        Span::raw(" ".repeat(GUTTER)),
        Span::styled(
            format!("{glyph} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    let cont = vec![Span::raw(" ".repeat(GUTTER + 2))];
    let spans = vec![Span::styled(body.to_owned(), Style::default().fg(color))];
    wrap(&spans, width, &first, &cont)
}

// ---------------------------------------------------------------------------
// Welcome
// ---------------------------------------------------------------------------

/// The facts the splash screen reports. Passed in rather than read from `App`
/// so the layout stays testable.
pub struct Welcome<'a> {
    pub version: &'a str,
    pub workspace: &'a str,
    pub model: &'a str,
    pub mode: &'a str,
    pub branch: Option<&'a str>,
    pub tips: bool,
}

/// Half-block wordmark. Falls back to plain text when the terminal is too
/// narrow for the 22-column art.
fn wordmark(width: usize) -> Vec<Line<'static>> {
    let style = Style::default()
        .fg(secondary())
        .add_modifier(Modifier::BOLD);
    let plain = || vec![Line::from(Span::styled("ABACUS", style))];
    let Some(rows) = glyphs().wordmark else {
        return plain();
    };
    if width < UnicodeWidthStr::width(rows[0]) + 4 {
        return plain();
    }
    rows.into_iter()
        .map(|row| Line::from(Span::styled(row, style)))
        .collect()
}

/// A `label   value` row for the splash screen's facts block, with the labels
/// aligned into a column.
fn fact(label: &str, value: String, value_color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), Style::default().fg(muted())),
        Span::styled(value, Style::default().fg(value_color)),
    ])
}

/// The empty-transcript splash: wordmark, a facts block naming exactly what
/// this session is pointed at, and the four things worth knowing on day one.
pub fn welcome(info: &Welcome<'_>, width: usize) -> Vec<Line<'static>> {
    let mut lines = wordmark(width);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "A focused coding agent for your terminal",
        Style::default().fg(muted()),
    )));
    lines.push(Line::from(""));

    let workspace = match info.branch {
        Some(branch) => format!(
            "{}  {} {branch}",
            truncate(info.workspace, 44),
            glyphs().branch
        ),
        None => truncate(info.workspace, 52),
    };
    lines.push(fact("workspace", workspace, text_color()));
    lines.push(fact("model", truncate(info.model, 52), text_color()));
    lines.push(fact("mode", info.mode.to_owned(), primary()));
    lines.push(fact("version", info.version.to_owned(), muted()));

    if info.tips {
        lines.push(Line::from(""));
        for (key, label, color) in [
            (
                "Build",
                "Describe a change; Abacus inspects, edits, verifies.",
                success(),
            ),
            (
                "Plan",
                "AUTO picks the workflow — shift+tab pins a mode.",
                warning(),
            ),
            (
                "Goal",
                "/goal sets a persistent definition of done.",
                primary(),
            ),
            ("Loop", "/loop runs promise-driven iteration.", secondary()),
        ] {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{key:<7}"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(label.to_owned(), Style::default().fg(muted())),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Type / for commands  ·  @file to attach context  ·  F1 for help",
        Style::default().fg(rail()),
    )));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn user_block_renders_as_a_full_width_tinted_card() {
        let tint = theme::active().user_bg;
        if tint == Color::Reset {
            // NO_COLOR run: the card degrades to the plain rail block.
            return;
        }
        let lines = user_block("hello there", 40);
        assert!(lines.len() >= 3, "padding row above and below the text");
        for line in &lines {
            let width: usize = line
                .spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum();
            assert_eq!(
                width,
                40,
                "every row pads to the measure: {:?}",
                plain(line)
            );
            assert!(
                line.spans.iter().all(|span| span.style.bg == Some(tint)),
                "every span carries the tint"
            );
        }
        assert_eq!(plain(&lines[0]).trim(), "", "top padding row is blank");
        assert!(plain(&lines[1]).contains("hello there"));
    }

    #[test]
    fn wrap_breaks_at_spaces_and_keeps_the_gutter() {
        let spans = vec![Span::raw("alpha beta gamma delta")];
        let bar = vec![Span::raw("▌ ")];
        let lines = wrap(&spans, 14, &bar, &bar);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(plain(line).starts_with("▌ "), "{:?}", plain(line));
            assert!(spans_width(&line.spans) <= 14, "{:?}", plain(line));
        }
        let joined: String = lines
            .iter()
            .map(|line| plain(line).replace("▌ ", ""))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(joined, "alpha beta gamma delta");
    }

    #[test]
    fn wrap_hard_breaks_a_token_longer_than_the_measure() {
        let spans = vec![Span::raw("supercalifragilistic")];
        let lines = wrap(&spans, 10, &[], &[]);
        assert!(lines.len() >= 2);
        for line in &lines {
            assert!(spans_width(&line.spans) <= 10);
        }
    }

    #[test]
    fn wrap_preserves_styles_across_a_break() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let spans = vec![Span::styled("aaaa bbbb", bold), Span::raw(" cccc dddd")];
        let lines = wrap(&spans, 10, &[], &[]);
        assert!(lines[0].spans.iter().all(|span| span.style == bold));
    }

    #[test]
    fn transcript_line_count_matches_the_rendered_rows() {
        let entries = vec![
            Entry::new(EntryKind::User, "please refactor the parser module"),
            Entry::tool(ToolCall {
                name: "read_file".into(),
                summary: "src/parser.rs".into(),
                status: ToolStatus::Ok,
                output: "1203 lines".into(),
                full: "1203 lines".into(),
                duration_ms: Some(240),
                expanded: false,
            }),
        ];
        let text = transcript(&entries, 40, "⠋", None, true);
        for line in &text.lines {
            assert!(
                spans_width(&line.spans) <= 40,
                "line overflows: {:?}",
                plain(line)
            );
        }
    }

    #[test]
    fn running_tools_show_the_spinner_and_finished_tools_the_duration() {
        let running = transcript(
            &[Entry::tool(ToolCall {
                name: "grep".into(),
                summary: "fn parse".into(),
                status: ToolStatus::Running,
                output: String::new(),
                full: String::new(),
                duration_ms: None,
                expanded: false,
            })],
            60,
            "⠹",
            None,
            true,
        );
        let row = plain(&running.lines[0]);
        assert!(row.contains('⠹'), "{row}");
        assert!(row.trim_end().ends_with("running"), "{row}");

        let done = transcript(
            &[Entry::tool(ToolCall {
                name: "grep".into(),
                summary: "fn parse".into(),
                status: ToolStatus::Ok,
                output: String::new(),
                full: String::new(),
                duration_ms: Some(1_500),
                expanded: false,
            })],
            60,
            "⠹",
            None,
            true,
        );
        let row = plain(&done.lines[0]);
        assert!(row.contains('✓'), "{row}");
        assert!(row.trim_end().ends_with("1.5s"), "{row}");
    }

    #[test]
    fn consecutive_tool_rows_group_without_a_blank_between_them() {
        let call = || {
            Entry::tool(ToolCall {
                name: "grep".into(),
                summary: String::new(),
                status: ToolStatus::Ok,
                output: String::new(),
                full: String::new(),
                duration_ms: Some(5),
                expanded: false,
            })
        };
        let text = transcript(&[call(), call()], 60, "⠋", None, true);
        assert_eq!(text.lines.len(), 2, "tool rows should not be separated");
    }

    #[test]
    fn truncate_measures_display_width_not_bytes() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert!(UnicodeWidthStr::width(truncate("日本語のテキスト", 7).as_str()) <= 7);
    }

    #[test]
    fn elapsed_and_counts_stay_narrow() {
        assert_eq!(format_elapsed(240), "240ms");
        assert_eq!(format_elapsed(1_500), "1.5s");
        assert_eq!(format_elapsed(90_000), "1m 30s");
        assert_eq!(format_elapsed(3_723_000), "1h 02m 03s");
        assert_eq!(format_count(938), "938");
        assert_eq!(format_count(12_400), "12.4k");
    }

    /// The whole point of the fallback table: a stand-in that is a different
    /// width would shift every column to its right, which is worse than the
    /// missing glyph it replaces.
    #[test]
    fn glyph_pairs_are_width_stable() {
        let unicode = Glyphs::UNICODE;
        let ascii = Glyphs::ASCII;
        let pairs: Vec<(&str, &str, &str)> = vec![
            ("bar", unicode.bar, ascii.bar),
            ("prompt", unicode.prompt, ascii.prompt),
            ("ok", unicode.ok, ascii.ok),
            ("failed", unicode.failed, ascii.failed),
            ("paused", unicode.paused, ascii.paused),
            ("notice", unicode.notice, ascii.notice),
            ("still", unicode.still, ascii.still),
            ("rule", unicode.rule, ascii.rule),
            ("meter_full", unicode.meter_full, ascii.meter_full),
            ("meter_empty", unicode.meter_empty, ascii.meter_empty),
            ("thumb", unicode.thumb, ascii.thumb),
            ("track", unicode.track, ascii.track),
            ("separator", unicode.separator, ascii.separator),
            ("quote_rail", unicode.quote_rail, ascii.quote_rail),
            ("fold_closed", unicode.fold_closed, ascii.fold_closed),
            ("fold_open", unicode.fold_open, ascii.fold_open),
            ("branch", unicode.branch, ascii.branch),
            ("queued", unicode.queued, ascii.queued),
            ("attached", unicode.attached, ascii.attached),
            ("goal", unicode.goal, ascii.goal),
            ("repeat", unicode.repeat, ascii.repeat),
            ("tasks", unicode.tasks, ascii.tasks),
            ("down", unicode.down, ascii.down),
        ];
        for (name, rich, plain) in pairs {
            assert_eq!(
                UnicodeWidthStr::width(rich),
                UnicodeWidthStr::width(plain),
                "{name}: {rich:?} and {plain:?} differ in width"
            );
            assert_eq!(UnicodeWidthStr::width(rich), 1, "{name} is not one cell");
        }
        for (rich, plain) in unicode.spinner.iter().zip(ascii.spinner.iter()) {
            assert_eq!(UnicodeWidthStr::width(*rich), 1);
            assert_eq!(UnicodeWidthStr::width(*plain), 1);
        }
        assert_eq!(unicode.spinner.len(), SPINNER_FRAMES);
        let rows = unicode.wordmark.expect("unicode wordmark");
        assert_eq!(
            UnicodeWidthStr::width(rows[0]),
            UnicodeWidthStr::width(rows[1]),
            "wordmark rows must be the same width"
        );
    }

    #[test]
    fn meter_fills_proportionally() {
        let spans = meter(50, 8, primary());
        assert_eq!(spans[0].content.chars().count(), 4);
        assert_eq!(spans[1].content.chars().count(), 4);
    }
}
