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

#[derive(Debug, Default, Clone)]
pub struct InputBuffer {
    chars: Vec<char>,
    cursor: usize,
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
        self.chars.clear();
        self.cursor = 0;
    }

    pub fn take(&mut self) -> String {
        let text = self.text();
        self.clear();
        text
    }

    pub fn insert(&mut self, ch: char) {
        self.chars.insert(self.cursor, ch);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.insert(ch);
        }
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
        let start = self.token_start();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
        self.insert_str(replacement);
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
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
        let end = self.cursor;
        self.move_word_backward();
        self.chars.drain(self.cursor..end);
    }

    pub fn delete_to_start(&mut self) {
        let end = self.cursor;
        self.move_start();
        self.chars.drain(self.cursor..end);
    }

    pub fn delete_to_end(&mut self) {
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
