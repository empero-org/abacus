use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy)]
pub struct MarkdownTheme {
    pub text: Color,
    pub muted: Color,
    pub heading: Color,
    pub accent: Color,
    pub code: Color,
    pub code_background: Color,
    pub quote: Color,
    pub link: Color,
    /// Rail down the left of a fenced code block.
    pub code_rail: &'static str,
    /// Rail down the left of a block quote. Deliberately a different weight
    /// from `code_rail` — quoted prose and code are different things and
    /// should not share a marker.
    pub quote_rail: &'static str,
}

impl Default for MarkdownTheme {
    fn default() -> Self {
        Self {
            text: Color::White,
            muted: Color::DarkGray,
            heading: Color::LightCyan,
            accent: Color::LightBlue,
            code: Color::LightGreen,
            code_background: Color::Rgb(15, 23, 42),
            quote: Color::LightBlue,
            link: Color::LightCyan,
            code_rail: "\u{2502}",
            quote_rail: "\u{2503}",
        }
    }
}

#[derive(Debug)]
struct ListState {
    next: Option<u64>,
}

#[derive(Debug, Default)]
struct TableState {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
    in_cell: bool,
    head_rows: usize,
    in_head: bool,
    alignments: Vec<Alignment>,
}

/// Render `markdown` without a known measure: code fences get no filled
/// background, since there is no width to fill to.
pub fn render(markdown: &str, theme: MarkdownTheme) -> Text<'static> {
    Renderer::new(theme, None).render(markdown)
}

/// Render `markdown` to a known measure. Code blocks become a filled slab
/// exactly `width` columns wide, which is what lets a fenced block read as one
/// object instead of as a ragged rule with text under it.
pub fn render_at(markdown: &str, theme: MarkdownTheme, width: usize) -> Text<'static> {
    Renderer::new(theme, Some(width.max(8))).render(markdown)
}

struct Renderer {
    theme: MarkdownTheme,
    /// Render measure, when the caller knows one.
    width: Option<usize>,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<ListState>,
    item_prefix_pending: Option<String>,
    quote_depth: usize,
    code_block: Option<String>,
    links: Vec<(String, bool)>,
    images: Vec<String>,
    table: Option<TableState>,
}

impl Renderer {
    fn new(theme: MarkdownTheme, width: Option<usize>) -> Self {
        Self {
            theme,
            width,
            lines: Vec::new(),
            current: Vec::new(),
            styles: vec![Style::default().fg(theme.text)],
            lists: Vec::new(),
            item_prefix_pending: None,
            quote_depth: 0,
            code_block: None,
            links: Vec::new(),
            images: Vec::new(),
            table: None,
        }
    }

    fn render(mut self, markdown: &str) -> Text<'static> {
        let options = Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TABLES
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_MATH
            | Options::ENABLE_GFM;
        for event in Parser::new_ext(markdown, options) {
            self.event(event);
        }
        self.flush_line(false);
        trim_blank_edges(&mut self.lines);
        Text::from(self.lines)
    }

    fn event(&mut self, event: Event<'_>) {
        if self.table.is_some() && self.table_event(&event) {
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.span(
                code.into_string(),
                self.style()
                    .fg(self.theme.code)
                    .bg(self.theme.code_background),
            ),
            Event::InlineMath(math) => self.span(
                prettify_math(&math),
                self.style()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::ITALIC),
            ),
            Event::DisplayMath(math) => {
                self.flush_line(false);
                self.blank_line();
                for line in prettify_math(&math).split('\n') {
                    if line.trim().is_empty() {
                        continue;
                    }
                    self.lines.push(Line::from(Span::styled(
                        format!("    {}", line.trim()),
                        Style::default()
                            .fg(self.theme.accent)
                            .add_modifier(Modifier::ITALIC),
                    )));
                }
                self.blank_line();
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.span(html.into_string(), self.style().fg(self.theme.muted))
            }
            Event::FootnoteReference(reference) => self.span(
                format!("[^{reference}]"),
                self.style().fg(self.theme.accent),
            ),
            Event::SoftBreak => self.span(" ".to_owned(), self.style()),
            Event::HardBreak => self.flush_line(true),
            Event::Rule => {
                self.flush_line(false);
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(48),
                    Style::default().fg(self.theme.muted),
                )));
                self.blank_line();
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "[✓] " } else { "[ ] " };
                self.ensure_prefix();
                self.span(marker.to_owned(), self.style().fg(self.theme.accent));
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.flush_line(false);
                self.blank_line();
                let style = heading_style(level, self.theme);
                self.styles.push(style);
            }
            Tag::BlockQuote(_) => {
                self.flush_line(false);
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_line(false);
                self.blank_line();
                let language = match kind {
                    CodeBlockKind::Fenced(value) => value.into_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                let label = if language.is_empty() {
                    "code".to_owned()
                } else {
                    language.clone()
                };
                let mut title = format!("╭─ {label} ");
                if let Some(width) = self.width {
                    let used = UnicodeWidthStr::width(title.as_str());
                    title.push_str(&"─".repeat(width.saturating_sub(used)));
                }
                self.lines.push(Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(self.theme.muted)
                        .bg(self.theme.code_background),
                )));
                self.code_block = Some(language);
            }
            Tag::List(start) => self.lists.push(ListState { next: start }),
            Tag::Item => {
                self.flush_line(false);
                let indent = "  ".repeat(self.lists.len().saturating_sub(1));
                let marker = self
                    .lists
                    .last_mut()
                    .and_then(|list| {
                        list.next.as_mut().map(|value| {
                            let marker = format!("{value}. ");
                            *value += 1;
                            marker
                        })
                    })
                    .unwrap_or_else(|| "• ".to_owned());
                self.item_prefix_pending = Some(format!("{indent}{marker}"));
            }
            Tag::Emphasis => self
                .styles
                .push(self.style().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self.styles.push(self.style().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(self.style().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.links.push((dest_url.into_string(), false));
                self.styles.push(
                    self.style()
                        .fg(self.theme.link)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Image { dest_url, .. } => {
                self.images.push(dest_url.into_string());
                self.span("▧ ".to_owned(), self.style().fg(self.theme.accent));
            }
            Tag::FootnoteDefinition(name) => {
                self.flush_line(false);
                self.span(format!("[^{name}] "), self.style().fg(self.theme.accent));
            }
            Tag::Table(alignments) => {
                self.table = Some(TableState {
                    alignments,
                    ..TableState::default()
                })
            }
            Tag::HtmlBlock
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript => {}
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line(false);
                self.blank_line();
            }
            TagEnd::Heading(_) => {
                self.flush_line(false);
                self.styles.pop();
                self.blank_line();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_line(false);
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.blank_line();
            }
            TagEnd::CodeBlock => {
                self.flush_line(false);
                let mut rule = "╰".to_owned();
                rule.push_str(&"─".repeat(self.width.unwrap_or(2).saturating_sub(1)));
                self.lines.push(Line::from(Span::styled(
                    rule,
                    Style::default()
                        .fg(self.theme.muted)
                        .bg(self.theme.code_background),
                )));
                self.code_block = None;
                self.blank_line();
            }
            TagEnd::List(_) => {
                self.flush_line(false);
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank_line();
                }
            }
            TagEnd::Item => self.flush_line(false),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some((destination, _)) = self.links.pop()
                    && !destination.is_empty()
                {
                    self.span(
                        format!(" ({destination})"),
                        self.style().fg(self.theme.muted),
                    );
                }
            }
            TagEnd::Image => {
                if let Some(destination) = self.images.pop()
                    && !destination.is_empty()
                {
                    self.span(
                        format!(" ({destination})"),
                        self.style().fg(self.theme.muted),
                    );
                }
            }
            TagEnd::FootnoteDefinition => {
                self.flush_line(false);
                self.blank_line();
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.render_table(table);
                }
            }
            TagEnd::HtmlBlock
            | TagEnd::MetadataBlock(_)
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.code_block.is_some() {
            for (index, line) in text.split('\n').enumerate() {
                if index > 0 {
                    self.flush_line(true);
                }
                if !line.is_empty() {
                    self.ensure_code_prefix();
                    self.span(
                        line.to_owned(),
                        Style::default()
                            .fg(self.theme.code)
                            .bg(self.theme.code_background),
                    );
                }
            }
            return;
        }
        self.ensure_prefix();
        if let Some((_, has_text)) = self.links.last_mut() {
            *has_text = true;
        }
        self.span(text.to_owned(), self.style());
    }

    fn table_event(&mut self, event: &Event<'_>) -> bool {
        let Some(table) = self.table.as_mut() else {
            return false;
        };
        match event {
            Event::Start(Tag::TableHead) => {
                table.in_head = true;
                table.row.clear();
            }
            Event::End(TagEnd::TableHead) => {
                if !table.row.is_empty() {
                    table.rows.push(std::mem::take(&mut table.row));
                }
                table.in_head = false;
                table.head_rows = table.rows.len();
            }
            Event::Start(Tag::TableRow) => table.row.clear(),
            Event::End(TagEnd::TableRow) => {
                table.rows.push(std::mem::take(&mut table.row));
                if table.in_head {
                    table.head_rows = table.rows.len();
                }
            }
            Event::Start(Tag::TableCell) => {
                table.cell.clear();
                table.in_cell = true;
            }
            Event::End(TagEnd::TableCell) => {
                table.row.push(std::mem::take(&mut table.cell));
                table.in_cell = false;
            }
            Event::Text(value) | Event::Code(value) if table.in_cell => table.cell.push_str(value),
            Event::InlineMath(value) if table.in_cell => table.cell.push_str(&prettify_math(value)),
            Event::SoftBreak | Event::HardBreak if table.in_cell => table.cell.push(' '),
            Event::End(TagEnd::Table) => return false,
            _ => {}
        }
        true
    }

    fn render_table(&mut self, table: TableState) {
        self.flush_line(false);
        if table.rows.is_empty() {
            return;
        }
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);

        // Natural width per column, then shave the widest column one cell at a
        // time until the table fits the measure. Shaving the widest first
        // means prose columns absorb the squeeze while short key/number
        // columns keep their one-line alignment; the squeezed cells re-wrap
        // onto continuation rows inside their own column instead of the whole
        // line overflowing and being re-broken by the transcript wrapper —
        // which is exactly the ragged mess this replaces.
        let mut widths = vec![1_usize; columns];
        for row in &table.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }
        let overhead = 3 * columns + 1;
        let budget = self
            .width
            .unwrap_or(88)
            .saturating_sub(overhead)
            .max(4 * columns);
        let mut total: usize = widths.iter().sum();
        while total > budget {
            let Some((widest, _)) = widths
                .iter()
                .enumerate()
                .max_by_key(|(_, width)| **width)
                .filter(|(_, width)| **width > 4)
            else {
                break;
            };
            widths[widest] -= 1;
            total -= 1;
        }

        let muted = Style::default().fg(self.theme.muted);
        let rule = |left: char, junction: char, right: char| {
            let bars = widths
                .iter()
                .map(|width| "─".repeat(width + 2))
                .collect::<Vec<_>>()
                .join(&junction.to_string());
            format!("{left}{bars}{right}")
        };

        self.lines
            .push(Line::from(Span::styled(rule('┌', '┬', '┐'), muted)));
        for (row_index, row) in table.rows.iter().enumerate() {
            let cells: Vec<Vec<String>> = (0..columns)
                .map(|column| {
                    wrap_cell(
                        row.get(column).map(String::as_str).unwrap_or(""),
                        widths[column],
                    )
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
            let style = if row_index < table.head_rows {
                Style::default()
                    .fg(self.theme.heading)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(self.theme.text)
            };
            for text_row in 0..height {
                let mut spans = vec![Span::styled("│ ", muted)];
                for column in 0..columns {
                    let value = cells[column]
                        .get(text_row)
                        .map(String::as_str)
                        .unwrap_or("");
                    let pad = widths[column].saturating_sub(UnicodeWidthStr::width(value));
                    let (left, right) = match table.alignments.get(column) {
                        Some(Alignment::Right) => (pad, 0),
                        Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                        _ => (0, pad),
                    };
                    spans.push(Span::styled(
                        format!("{}{}{}", " ".repeat(left), value, " ".repeat(right)),
                        style,
                    ));
                    spans.push(Span::styled(
                        if column + 1 == columns {
                            " │"
                        } else {
                            " │ "
                        },
                        muted,
                    ));
                }
                self.lines.push(Line::from(spans));
            }
            if row_index + 1 == table.head_rows {
                self.lines
                    .push(Line::from(Span::styled(rule('├', '┼', '┤'), muted)));
            }
        }
        self.lines
            .push(Line::from(Span::styled(rule('└', '┴', '┘'), muted)));
        self.blank_line();
    }

    fn ensure_prefix(&mut self) {
        if self.current.is_empty() {
            for _ in 0..self.quote_depth {
                self.current.push(Span::styled(
                    format!("{} ", self.theme.quote_rail),
                    Style::default().fg(self.theme.quote),
                ));
            }
            if let Some(prefix) = self.item_prefix_pending.take() {
                self.current
                    .push(Span::styled(prefix, Style::default().fg(self.theme.accent)));
            }
        }
    }

    fn ensure_code_prefix(&mut self) {
        if self.current.is_empty() {
            self.current.push(Span::styled(
                format!("{} ", self.theme.code_rail),
                Style::default()
                    .fg(self.theme.muted)
                    .bg(self.theme.code_background),
            ));
        }
    }

    fn span(&mut self, value: String, style: Style) {
        self.current.push(Span::styled(value, style));
    }

    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn flush_line(&mut self, force: bool) {
        if self.current.is_empty() && !force {
            return;
        }
        // Inside a fenced block, pad the row out to the measure so the filled
        // background forms a rectangle rather than tracking the ragged right
        // edge of the code.
        if self.code_block.is_some()
            && let Some(width) = self.width
        {
            if self.current.is_empty() {
                self.ensure_code_prefix();
            }
            let used: usize = self
                .current
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum();
            if used < width {
                self.current.push(Span::styled(
                    " ".repeat(width - used),
                    Style::default().bg(self.theme.code_background),
                ));
            }
        }
        self.lines
            .push(Line::from(std::mem::take(&mut self.current)));
    }

    fn blank_line(&mut self) {
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(Line::from(""));
        }
    }
}

fn heading_style(level: HeadingLevel, theme: MarkdownTheme) -> Style {
    let color = if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
        theme.heading
    } else {
        theme.text
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// Word-wrap one table cell to `width` display columns. Words longer than the
/// column are hard-broken so a pathological token (a URL, a hash) cannot push
/// the column wall out of alignment.
fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut used = 0_usize;
    for word in text.split_whitespace() {
        let word_width = UnicodeWidthStr::width(word);
        if used > 0 && used + 1 + word_width > width {
            lines.push(std::mem::take(&mut current));
            used = 0;
        }
        if word_width > width {
            for ch in word.chars() {
                let ch_width = ch.width().unwrap_or(0);
                if used + ch_width > width && used > 0 {
                    lines.push(std::mem::take(&mut current));
                    used = 0;
                }
                current.push(ch);
                used += ch_width;
            }
            continue;
        }
        if used > 0 {
            current.push(' ');
            used += 1;
        }
        current.push_str(word);
        used += word_width;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

/// Translate a LaTeX fragment into plain Unicode: Greek letters, operators,
/// sub/superscripts, fractions. Aimed at the math models actually emit in
/// chat — anything unrecognised passes through untouched rather than erroring,
/// so worst case the reader sees the original TeX.
fn prettify_math(tex: &str) -> String {
    let chars: Vec<char> = tex.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\\' => {
                let start = index + 1;
                let mut end = start;
                while end < chars.len() && chars[end].is_ascii_alphabetic() {
                    end += 1;
                }
                if end == start {
                    // "\," and friends: spacing and escaped punctuation.
                    if let Some(&next) = chars.get(start) {
                        match next {
                            ',' | ';' | ':' | ' ' => out.push(' '),
                            '!' => {}
                            _ => out.push(next),
                        }
                        index = start + 1;
                    } else {
                        index = start;
                    }
                    continue;
                }
                let command: String = chars[start..end].iter().collect();
                index = end;
                match command.as_str() {
                    "frac" | "tfrac" | "dfrac" => {
                        let (numerator, next) = read_group(&chars, index);
                        let (denominator, next) = read_group(&chars, next);
                        index = next;
                        out.push_str(&fraction_part(&prettify_math(&numerator)));
                        out.push('/');
                        out.push_str(&fraction_part(&prettify_math(&denominator)));
                    }
                    "sqrt" => {
                        let (inner, next) = read_group(&chars, index);
                        index = next;
                        out.push('√');
                        out.push_str(&fraction_part(&prettify_math(&inner)));
                    }
                    "text" | "mathrm" | "mathit" | "mathbf" | "mathsf" | "mathcal" | "mathbb"
                    | "operatorname" | "textrm" | "textbf" | "textit" => {
                        let (inner, next) = read_group(&chars, index);
                        index = next;
                        out.push_str(&prettify_math(&inner));
                    }
                    "left" | "right" | "big" | "Big" | "bigg" | "Bigg" | "displaystyle"
                    | "limits" => {}
                    "quad" | "qquad" => out.push_str("  "),
                    other => match math_symbol(other) {
                        Some(symbol) => out.push_str(symbol),
                        None => {
                            out.push('\\');
                            out.push_str(other);
                        }
                    },
                }
            }
            '^' | '_' => {
                let is_superscript = chars[index] == '^';
                let (group, next) = read_group(&chars, index + 1);
                index = next;
                let rendered = prettify_math(&group);
                let mapped = if is_superscript {
                    map_script(&rendered, superscript_char)
                } else {
                    map_script(&rendered, subscript_char)
                };
                match mapped {
                    Some(script) => out.push_str(&script),
                    None => {
                        out.push(if is_superscript { '^' } else { '_' });
                        if rendered.chars().count() > 3 || rendered.contains(' ') {
                            out.push('(');
                            out.push_str(&rendered);
                            out.push(')');
                        } else {
                            out.push_str(&rendered);
                        }
                    }
                }
            }
            '{' | '}' => index += 1,
            ch => {
                out.push(ch);
                index += 1;
            }
        }
    }
    out
}

/// Read the argument after a TeX command: a balanced `{…}` group, a `\command`,
/// or a single character. Returns the content and the index just past it.
fn read_group(chars: &[char], index: usize) -> (String, usize) {
    match chars.get(index) {
        Some('{') => {
            let mut depth = 1;
            let mut end = index + 1;
            while end < chars.len() && depth > 0 {
                match chars[end] {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
                end += 1;
            }
            let inner: String = chars[index + 1..end.saturating_sub(1)].iter().collect();
            (inner, end)
        }
        Some('\\') => {
            let mut end = index + 1;
            while end < chars.len() && chars[end].is_ascii_alphabetic() {
                end += 1;
            }
            (chars[index..end].iter().collect(), end)
        }
        Some(&ch) => (ch.to_string(), index + 1),
        None => (String::new(), index),
    }
}

/// Parenthesise a fraction part that holds more than one token, so
/// `\frac{p_i}{\sum p_k}` reads `pᵢ/(Σ pₖ)` rather than `pᵢ/Σ pₖ`.
fn fraction_part(part: &str) -> String {
    let compound = part.contains(' ') || part.chars().any(|ch| "+-±·×/=".contains(ch));
    if compound && !(part.starts_with('(') && part.ends_with(')')) {
        format!("({part})")
    } else {
        part.to_owned()
    }
}

fn map_script(text: &str, map: fn(char) -> Option<char>) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    text.chars().map(map).collect()
}

fn superscript_char(ch: char) -> Option<char> {
    Some(match ch {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'f' => 'ᶠ',
        'g' => 'ᵍ',
        'h' => 'ʰ',
        'i' => 'ⁱ',
        'j' => 'ʲ',
        'k' => 'ᵏ',
        'l' => 'ˡ',
        'm' => 'ᵐ',
        'n' => 'ⁿ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        'r' => 'ʳ',
        's' => 'ˢ',
        't' => 'ᵗ',
        'u' => 'ᵘ',
        'v' => 'ᵛ',
        'w' => 'ʷ',
        'x' => 'ˣ',
        'y' => 'ʸ',
        'z' => 'ᶻ',
        'T' => 'ᵀ',
        _ => return None,
    })
}

fn subscript_char(ch: char) -> Option<char> {
    Some(match ch {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

fn math_symbol(command: &str) -> Option<&'static str> {
    Some(match command {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" | "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" | "vartheta" => "θ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "rho" => "ρ",
        "sigma" => "σ",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" | "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "times" => "×",
        "cdot" => "·",
        "div" => "÷",
        "pm" => "±",
        "mp" => "∓",
        "leq" | "le" => "≤",
        "geq" | "ge" => "≥",
        "neq" | "ne" => "≠",
        "approx" => "≈",
        "sim" => "∼",
        "simeq" => "≃",
        "equiv" => "≡",
        "propto" => "∝",
        "infty" => "∞",
        "sum" => "Σ",
        "prod" => "Π",
        "int" => "∫",
        "partial" => "∂",
        "nabla" => "∇",
        "in" => "∈",
        "notin" => "∉",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" => "⊆",
        "supseteq" => "⊇",
        "cup" => "∪",
        "cap" => "∩",
        "setminus" => "∖",
        "forall" => "∀",
        "exists" => "∃",
        "neg" | "lnot" => "¬",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "to" | "rightarrow" => "→",
        "leftarrow" | "gets" => "←",
        "Rightarrow" | "implies" => "⇒",
        "Leftarrow" => "⇐",
        "leftrightarrow" => "↔",
        "Leftrightarrow" | "iff" => "⇔",
        "mapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "ldots" | "dots" | "cdots" => "…",
        "prime" => "′",
        "circ" => "∘",
        "star" => "⋆",
        "bullet" => "•",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "odot" => "⊙",
        "langle" => "⟨",
        "rangle" => "⟩",
        "lVert" | "rVert" | "Vert" => "‖",
        "lvert" | "rvert" | "vert" | "mid" => "|",
        "ell" => "ℓ",
        "hbar" => "ℏ",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "emptyset" | "varnothing" => "∅",
        "angle" => "∠",
        "perp" => "⊥",
        "parallel" => "∥",
        "therefore" => "∴",
        "because" => "∵",
        "degree" => "°",
        "max" => "max",
        "min" => "min",
        "log" => "log",
        "ln" => "ln",
        "exp" => "exp",
        "sin" => "sin",
        "cos" => "cos",
        "tan" => "tan",
        "argmax" => "argmax",
        "argmin" => "argmin",
        _ => return None,
    })
}

fn trim_blank_edges(lines: &mut Vec<Line<'static>>) {
    while lines.first().is_some_and(|line| line.spans.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.spans.is_empty()) {
        lines.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_rich_commonmark_without_leaking_markup() {
        let text = render(
            "# Release\n\nUse **bold**, *care*, `cargo test`, and [docs](https://example.test).\n\n> Important\n\n- [x] tested\n- shipped",
            MarkdownTheme::default(),
        );
        let plain = text
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(plain.contains("Release"));
        assert!(plain.contains("[✓]"));
        assert!(plain.contains("https://example.test"));
        assert!(!plain.contains("**bold**"));
        assert!(text.lines.iter().any(|line| line.spans.iter().any(|span| {
            span.content.contains("bold") && span.style.add_modifier.contains(Modifier::BOLD)
        })));
    }

    fn plain_lines(text: &Text<'static>) -> Vec<String> {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn wide_tables_wrap_inside_their_columns_and_respect_the_measure() {
        let markdown = "| # | Fix | Type |\n|---|-----|------|\n\
            | P1 | Node-limited balance loss now uses the effective routable pool so the controller and the loss agree on the target distribution | correctness |\n\
            | P2 | Capacity factor with overflow token-dropping, off by default | throughput |";
        let text = render_at(markdown, MarkdownTheme::default(), 72);
        let lines = plain_lines(&text);
        for line in &lines {
            assert!(
                UnicodeWidthStr::width(line.as_str()) <= 72,
                "line exceeds the measure: {line:?}"
            );
        }
        // The long Fix cell wrapped onto continuation rows inside its column,
        // so the table is taller than one physical line per row…
        let body_rows = lines.iter().filter(|line| line.contains("│")).count();
        assert!(body_rows > 4, "expected wrapped rows, got {lines:#?}");
        // …and every border row still spans identical column walls.
        let top = lines.iter().find(|line| line.starts_with('┌')).unwrap();
        let bottom = lines.iter().find(|line| line.starts_with('└')).unwrap();
        assert_eq!(
            UnicodeWidthStr::width(top.as_str()),
            UnicodeWidthStr::width(bottom.as_str())
        );
        // The narrow "#" column kept its content on one line.
        assert!(lines.iter().any(|line| line.contains("P1")));
        assert!(lines.iter().any(|line| line.contains("P2")));
    }

    #[test]
    fn table_alignment_pads_the_correct_side() {
        let markdown = "| Name | Count |\n|:-----|------:|\n| a | 1 |\n| bbbb | 22 |";
        let text = render_at(markdown, MarkdownTheme::default(), 60);
        let lines = plain_lines(&text);
        // Right-aligned count column: the value hugs the right wall.
        let row = lines.iter().find(|line| line.contains(" 1 │")).unwrap();
        assert!(row.contains("│     1 │"), "{row:?}");
    }

    #[test]
    fn inline_math_renders_as_unicode_without_dollar_signs() {
        let text = render(
            r"The weight is $w_i = p_i / \sum_k p_k$ with $\alpha^2 \cdot \beta$.",
            MarkdownTheme::default(),
        );
        let plain = plain_lines(&text).join("\n");
        assert!(plain.contains("wᵢ = pᵢ / Σₖ pₖ"), "{plain}");
        assert!(plain.contains("α² · β"), "{plain}");
        assert!(!plain.contains('$'), "{plain}");
    }

    #[test]
    fn display_math_becomes_an_indented_unicode_block() {
        let text = render(
            "$$\\mathcal{L} = \\frac{\\alpha}{N} \\sum_{i=1}^{N} f_i \\cdot P_i$$",
            MarkdownTheme::default(),
        );
        let plain = plain_lines(&text).join("\n");
        assert!(plain.contains("L = α/N Σ"), "{plain}");
        assert!(plain.contains("fᵢ · Pᵢ"), "{plain}");
        assert!(!plain.contains("\\frac"), "{plain}");
    }

    #[test]
    fn math_prettifier_handles_fractions_scripts_and_unknowns() {
        assert_eq!(prettify_math(r"\frac{p_i}{\sum_k p_k}"), "pᵢ/(Σₖ pₖ)");
        assert_eq!(prettify_math(r"E = mc^2"), "E = mc²");
        assert_eq!(prettify_math(r"x^{n+1}"), "xⁿ⁺¹");
        assert_eq!(prettify_math(r"a_{eff}"), "a_eff");
        assert_eq!(prettify_math(r"\sqrt{2}"), "√2");
        assert_eq!(prettify_math(r"A^T \cdot B"), "Aᵀ · B");
        assert_eq!(prettify_math(r"\text{loss} \to 0"), "loss → 0");
        // Unknown commands survive verbatim rather than vanishing.
        assert_eq!(prettify_math(r"\somenewthing x"), "\\somenewthing x");
    }

    #[test]
    fn math_inside_table_cells_is_prettified() {
        let markdown = "| Term | Formula |\n|---|---|\n| routing | $w_i = p_i / \\sum_k p_k$ |";
        let text = render_at(markdown, MarkdownTheme::default(), 60);
        let plain = plain_lines(&text).join("\n");
        assert!(plain.contains("wᵢ = pᵢ / Σₖ pₖ"), "{plain}");
    }

    #[test]
    fn renders_fenced_code_and_tables_as_terminal_blocks() {
        let text = render(
            "```rust\nfn main() {}\n```\n\n| Name | State |\n|---|---|\n| tests | green |",
            MarkdownTheme::default(),
        );
        let plain = text
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(plain.contains("╭─ rust"));
        assert!(plain.contains("fn main() {}"));
        assert!(plain.contains("╰─"));
        assert!(plain.contains("tests"));
        assert!(plain.contains("green"));
        assert!(plain.contains('┼'));
    }
}
