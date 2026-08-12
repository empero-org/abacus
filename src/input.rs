#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Insert,
    Normal,
}

impl InputMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Normal => "NORMAL",
        }
    }
}

/// Undo steps kept per composer. Deep enough to walk back a paragraph,
/// shallow enough that the buffer stays small.
const UNDO_DEPTH: usize = 200;

#[derive(Debug, Default, Clone)]
pub struct InputBuffer {
    chars: Vec<char>,
    cursor: usize,
    /// Selected range, set by select-all and cleared by any edit.
    selection: Option<(usize, usize)>,
    undo_stack: Vec<(Vec<char>, usize)>,
    redo_stack: Vec<(Vec<char>, usize)>,
    /// Whether the current run of typing is still folding into one undo step.
    coalescing: bool,
}

impl InputBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn clear(&mut self) {
        if !self.chars.is_empty() {
            self.checkpoint(false);
        }
        self.chars.clear();
        self.cursor = 0;
        self.selection = None;
        self.coalescing = false;
    }

    pub fn take(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }

    pub fn insert(&mut self, ch: char) {
        // Whitespace ends a run, so undo lands on word boundaries.
        self.checkpoint(!ch.is_whitespace());
        self.chars.insert(self.cursor, ch);
        self.cursor += 1;
    }

    /// Insert a whole string as one undo step — a paste is one edit, not one
    /// per character.
    pub fn insert_str(&mut self, text: &str) {
        self.checkpoint(false);
        let chars: Vec<char> = text.chars().collect();
        let count = chars.len();
        self.chars.splice(self.cursor..self.cursor, chars);
        self.cursor += count;
        self.coalescing = false;
    }

    /// The whitespace-delimited token ending at the cursor, used to drive
    /// `@file` and `/command` completion against what is currently being typed.
    pub fn token_before_cursor(&self) -> String {
        self.chars[self.token_start()..self.cursor].iter().collect()
    }

    fn token_start(&self) -> usize {
        let mut start = self.cursor;
        while start > 0 && !self.chars[start - 1].is_whitespace() {
            start -= 1;
        }
        start
    }

    /// Replace the token ending at the cursor (see `token_before_cursor`) with
    /// `replacement`, leaving the cursor at the end of the inserted text.
    pub fn replace_token_before_cursor(&mut self, replacement: &str) {
        self.checkpoint(false);
        let start = self.token_start();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
        self.insert_str(replacement);
    }

    pub fn backspace(&mut self) {
        self.checkpoint(false);
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        self.checkpoint(false);
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    pub fn move_start(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1] != '\n' {
            self.cursor -= 1;
        }
    }

    pub fn move_end(&mut self) {
        while self.cursor < self.chars.len() && self.chars[self.cursor] != '\n' {
            self.cursor += 1;
        }
    }

    pub fn move_up(&mut self) {
        let (row, col) = self.cursor_position();
        if row > 0 {
            self.set_position(row - 1, col);
        }
    }

    pub fn move_down(&mut self) {
        let (row, col) = self.cursor_position();
        if row + 1 < self.line_count() {
            self.set_position(row + 1, col);
        }
    }

    pub fn move_word_forward(&mut self) {
        while self.cursor < self.chars.len() && !self.chars[self.cursor].is_alphanumeric() {
            self.cursor += 1;
        }
        while self.cursor < self.chars.len() && self.chars[self.cursor].is_alphanumeric() {
            self.cursor += 1;
        }
    }

    pub fn move_word_backward(&mut self) {
        while self.cursor > 0 && !self.chars[self.cursor - 1].is_alphanumeric() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && self.chars[self.cursor - 1].is_alphanumeric() {
            self.cursor -= 1;
        }
    }

    pub fn delete_word_backward(&mut self) {
        self.checkpoint(false);
        let end = self.cursor;
        self.move_word_backward();
        self.chars.drain(self.cursor..end);
    }

    pub fn delete_to_start(&mut self) {
        self.checkpoint(false);
        let end = self.cursor;
        self.move_start();
        self.chars.drain(self.cursor..end);
    }

    pub fn delete_to_end(&mut self) {
        self.checkpoint(false);
        let start = self.cursor;
        self.move_end();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn cursor_position(&self) -> (usize, usize) {
        let before = &self.chars[..self.cursor];
        let row = before.iter().filter(|&&ch| ch == '\n').count();
        let col = before.iter().rev().take_while(|&&ch| ch != '\n').count();
        (row, col)
    }

    pub fn line_count(&self) -> usize {
        self.chars.iter().filter(|&&ch| ch == '\n').count() + 1
    }

    /// The buffer laid out at `width` columns, as `(start, end)` char ranges —
    /// one per visual row. Long lines wrap at a space where there is one, so
    /// the composer grows downward instead of scrolling a single line sideways.
    pub fn wrapped_rows(&self, width: usize) -> Vec<(usize, usize)> {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut line_start = 0;
        // Walk logical lines, wrapping each; an empty line still occupies a row.
        for (index, &ch) in self.chars.iter().enumerate() {
            if ch == '\n' {
                Self::wrap_line(&self.chars[line_start..index], line_start, width, &mut rows);
                line_start = index + 1;
            }
        }
        Self::wrap_line(&self.chars[line_start..], line_start, width, &mut rows);
        rows
    }

    fn wrap_line(line: &[char], offset: usize, width: usize, rows: &mut Vec<(usize, usize)>) {
        if line.is_empty() {
            rows.push((offset, offset));
            return;
        }
        let mut start = 0;
        while start < line.len() {
            if line.len() - start <= width {
                rows.push((offset + start, offset + line.len()));
                return;
            }
            // Break at the last space inside the window, so words stay whole;
            // a single long token is cut at the edge rather than overflowing.
            let hard = start + width;
            let split = line[start..hard]
                .iter()
                .rposition(|&ch| ch == ' ')
                .map(|position| start + position + 1)
                .filter(|candidate| *candidate > start)
                .unwrap_or(hard);
            rows.push((offset + start, offset + split));
            start = split;
        }
    }

    /// How many visual rows the buffer occupies at `width`.
    pub fn wrapped_line_count(&self, width: usize) -> usize {
        self.wrapped_rows(width).len().max(1)
    }

    /// The cursor's position in wrapped space: which visual row, and how far
    /// into it.
    pub fn wrapped_cursor(&self, width: usize) -> (usize, usize) {
        let rows = self.wrapped_rows(width);
        for (index, &(start, end)) in rows.iter().enumerate() {
            // `<=` on the end so a cursor sitting at a wrap point belongs to the
            // row it was typed into, not the next one.
            if self.cursor >= start && self.cursor <= end {
                return (index, self.cursor - start);
            }
        }
        rows.last()
            .map(|&(start, end)| (rows.len().saturating_sub(1), end - start))
            .unwrap_or((0, 0))
    }

    /// Move a visual row up/down, honouring wrapping. Returns false when the
    /// cursor is already on the first/last row, which is the caller's cue to
    /// fall through to prompt history.
    pub fn move_up_wrapped(&mut self, width: usize) -> bool {
        let (row, col) = self.wrapped_cursor(width);
        if row == 0 {
            return false;
        }
        let rows = self.wrapped_rows(width);
        let (start, end) = rows[row - 1];
        self.cursor = (start + col).min(end);
        true
    }

    pub fn move_down_wrapped(&mut self, width: usize) -> bool {
        let rows = self.wrapped_rows(width);
        let (row, col) = self.wrapped_cursor(width);
        if row + 1 >= rows.len() {
            return false;
        }
        let (start, end) = rows[row + 1];
        self.cursor = (start + col).min(end);
        true
    }

    /// Select everything. Any edit clears the selection.
    pub fn select_all(&mut self) {
        self.selection = (!self.chars.is_empty()).then_some((0, self.chars.len()));
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// The selected range as `(start, end)` char indices, if any.
    pub fn selection(&self) -> Option<(usize, usize)> {
        self.selection
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection?;
        Some(
            self.chars[start..end.min(self.chars.len())]
                .iter()
                .collect(),
        )
    }

    /// Restore the buffer to before the last edit. Runs of typed characters
    /// collapse into one step, so undo moves in words rather than keystrokes.
    pub fn undo(&mut self) -> bool {
        let Some((chars, cursor)) = self.undo_stack.pop() else {
            return false;
        };
        self.redo_stack
            .push((std::mem::take(&mut self.chars), self.cursor));
        self.chars = chars;
        self.cursor = cursor.min(self.chars.len());
        self.selection = None;
        self.coalescing = false;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some((chars, cursor)) = self.redo_stack.pop() else {
            return false;
        };
        self.undo_stack
            .push((std::mem::take(&mut self.chars), self.cursor));
        self.chars = chars;
        self.cursor = cursor.min(self.chars.len());
        self.selection = None;
        self.coalescing = false;
        true
    }

    /// Record the pre-edit state. `coalesce` groups a run of ordinary typing
    /// into a single undo step; anything else starts a fresh one.
    fn checkpoint(&mut self, coalesce: bool) {
        self.selection = None;
        self.redo_stack.clear();
        if coalesce && self.coalescing {
            return;
        }
        self.undo_stack.push((self.chars.clone(), self.cursor));
        if self.undo_stack.len() > UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
        self.coalescing = coalesce;
    }

    fn set_position(&mut self, target_row: usize, target_col: usize) {
        let mut row = 0;
        let mut index = 0;
        while row < target_row && index < self.chars.len() {
            if self.chars[index] == '\n' {
                row += 1;
            }
            index += 1;
        }

        let mut col = 0;
        while index < self.chars.len() && self.chars[index] != '\n' && col < target_col {
            index += 1;
            col += 1;
        }
        self.cursor = index;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_wraps_at_words_and_the_cursor_follows() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("the quick brown fox jumps");
        // At 10 columns this needs several rows, broken at spaces.
        let rows = buffer.wrapped_rows(10);
        assert!(rows.len() >= 3, "{rows:?}");
        assert_eq!(buffer.wrapped_line_count(10), rows.len());
        let text: Vec<String> = rows
            .iter()
            .map(|&(start, end)| buffer.chars[start..end].iter().collect())
            .collect();
        assert!(
            text.iter().all(|row| row.len() <= 10),
            "no row exceeds the width: {text:?}"
        );
        assert!(text[0].starts_with("the"), "{text:?}");

        // The cursor at the very end sits on the last row.
        let (row, _) = buffer.wrapped_cursor(10);
        assert_eq!(row, rows.len() - 1);
        // And at the start, on the first.
        buffer.move_start();
        while buffer.cursor > 0 {
            buffer.move_left();
        }
        assert_eq!(buffer.wrapped_cursor(10), (0, 0));
    }

    #[test]
    fn a_single_long_token_still_wraps() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str(&"x".repeat(25));
        let rows = buffer.wrapped_rows(10);
        assert_eq!(rows.len(), 3, "hard-broken when there is no space");
    }

    #[test]
    fn undo_groups_typing_and_redo_replays_it() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("hello");
        for ch in " world".chars() {
            buffer.insert(ch);
        }
        assert_eq!(buffer.text(), "hello world");

        // The typed run collapses into one step rather than six.
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "hello ");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "hello");
        assert!(buffer.undo());
        assert_eq!(buffer.text(), "");
        assert!(!buffer.undo(), "nothing left to undo");

        assert!(buffer.redo());
        assert_eq!(buffer.text(), "hello");
        assert!(buffer.redo());
        assert_eq!(buffer.text(), "hello ");
    }

    #[test]
    fn editing_after_undo_clears_the_redo_branch() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("one");
        buffer.insert_str("two");
        assert!(buffer.undo());
        buffer.insert_str("three");
        assert!(!buffer.redo(), "the abandoned branch is gone");
        assert_eq!(buffer.text(), "onethree");
    }

    #[test]
    fn select_all_yields_the_text_and_any_edit_clears_it() {
        let mut buffer = InputBuffer::new();
        assert!(buffer.selection().is_none());
        buffer.insert_str("copy me");
        buffer.select_all();
        assert_eq!(buffer.selected_text().as_deref(), Some("copy me"));
        buffer.insert('!');
        assert!(
            buffer.selection().is_none(),
            "typing dismisses the selection"
        );
    }

    #[test]
    fn arrows_move_by_wrapped_row_then_report_the_edge() {
        let mut buffer = InputBuffer::new();
        buffer.insert_str("aaaa bbbb cccc");
        // Cursor at the end, on the last row.
        assert!(buffer.move_up_wrapped(5), "moves up a visual row");
        assert!(!buffer.move_up_wrapped(5) || buffer.wrapped_cursor(5).0 == 0);
        while buffer.move_up_wrapped(5) {}
        assert_eq!(buffer.wrapped_cursor(5).0, 0);
        assert!(
            !buffer.move_up_wrapped(5),
            "edge reached — caller uses history"
        );
        assert!(buffer.move_down_wrapped(5));
    }

    #[test]
    fn edits_unicode_by_character_not_byte() {
        let mut input = InputBuffer::new();
        input.insert_str("a🧮b");
        input.move_left();
        input.backspace();
        assert_eq!(input.text(), "ab");
    }

    #[test]
    fn vertical_movement_clamps_to_shorter_line() {
        let mut input = InputBuffer::new();
        input.insert_str("abcdef\nxy");
        input.move_up();
        assert_eq!(input.cursor_position(), (0, 2));
        input.move_end();
        input.move_down();
        assert_eq!(input.cursor_position(), (1, 2));
    }

    #[test]
    fn token_before_cursor_tracks_the_at_mention_being_typed() {
        let mut input = InputBuffer::new();
        input.insert_str("explain @src/ma");
        assert_eq!(input.token_before_cursor(), "@src/ma");
        input.replace_token_before_cursor("@src/main.rs");
        input.insert(' ');
        assert_eq!(input.text(), "explain @src/main.rs ");
        // A trailing space means there is no active token to complete.
        assert_eq!(input.token_before_cursor(), "");
    }

    #[test]
    fn word_delete_matches_editor_expectations() {
        let mut input = InputBuffer::new();
        input.insert_str("hello, small world");
        input.delete_word_backward();
        assert_eq!(input.text(), "hello, small ");
    }
}
