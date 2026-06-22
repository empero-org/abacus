use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};

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
}

pub fn render(markdown: &str, theme: MarkdownTheme) -> Text<'static> {
    Renderer::new(theme).render(markdown)
}

struct Renderer {
    theme: MarkdownTheme,
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
    fn new(theme: MarkdownTheme) -> Self {
        Self {
            theme,
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
                format!("${math}$"),
                self.style()
                    .fg(self.theme.code)
                    .add_modifier(Modifier::ITALIC),
            ),
            Event::DisplayMath(math) => {
                self.flush_line(false);
                self.lines.push(Line::from(Span::styled(
                    format!("  {math}"),
                    Style::default()
                        .fg(self.theme.code)
                        .bg(self.theme.code_background),
                )));
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
                let title = if language.is_empty() {
                    "╭─ code".to_owned()
                } else {
                    format!("╭─ {language}")
                };
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
            Tag::Table(_) => self.table = Some(TableState::default()),
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
                self.lines.push(Line::from(Span::styled(
                    "╰─",
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
        let mut widths = vec![1_usize; columns];
        for row in &table.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.chars().count().min(40));
            }
        }
        for (row_index, row) in table.rows.iter().enumerate() {
            let mut spans = vec![Span::styled("│ ", Style::default().fg(self.theme.muted))];
            for (index, width) in widths.iter().enumerate().take(columns) {
                let value = row.get(index).map(String::as_str).unwrap_or("");
                spans.push(Span::styled(
                    format!("{value:<width$}"),
                    Style::default()
                        .fg(if row_index < table.head_rows {
                            self.theme.heading
                        } else {
                            self.theme.text
                        })
                        .add_modifier(if row_index < table.head_rows {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ));
                spans.push(Span::styled(" │ ", Style::default().fg(self.theme.muted)));
            }
            self.lines.push(Line::from(spans));
            if row_index + 1 == table.head_rows {
                self.lines.push(Line::from(Span::styled(
                    format!(
                        "├{}┤",
                        widths
                            .iter()
                            .map(|width| "─".repeat(width + 2))
                            .collect::<Vec<_>>()
                            .join("┼")
                    ),
                    Style::default().fg(self.theme.muted),
                )));
            }
        }
        self.blank_line();
    }

    fn ensure_prefix(&mut self) {
        if self.current.is_empty() {
            for _ in 0..self.quote_depth {
                self.current
                    .push(Span::styled("│ ", Style::default().fg(self.theme.quote)));
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
                "│ ",
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
        if !self.current.is_empty() || force {
            self.lines
                .push(Line::from(std::mem::take(&mut self.current)));
        }
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
