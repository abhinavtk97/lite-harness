//! A readline-ish, multi-line-capable text buffer with a movable cursor.
//! Deliberately its own small type rather than a bare `String` on `App` --
//! cursor-aware editing (arrow keys, Home/End, word-delete) has enough of
//! its own logic to deserve isolated tests, and char-indexed (not
//! byte-indexed) storage sidesteps UTF-8 boundary bugs entirely for the
//! cost of a `Vec<char>` instead of a `String`.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputBox {
    chars: Vec<char>,
    cursor: usize,
}

impl InputBox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Number of lines the current text spans (always >= 1, even when empty).
    pub fn line_count(&self) -> usize {
        self.chars.iter().filter(|&&c| c == '\n').count() + 1
    }

    /// (column, row) of the cursor within the multi-line text, both
    /// 0-based -- for positioning the terminal's real cursor.
    pub fn cursor_row_col(&self) -> (u16, u16) {
        let before = &self.chars[..self.cursor];
        let row = before.iter().filter(|&&c| c == '\n').count() as u16;
        let col = before.iter().rev().take_while(|&&c| c != '\n').count() as u16;
        (col, row)
    }

    pub fn insert_char(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    /// Deletes the character immediately before the cursor, if any.
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    /// Readline's Ctrl+W: skips any whitespace immediately before the
    /// cursor, then deletes back through the word before that.
    pub fn delete_word_before_cursor(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
        while self.cursor > 0 && !self.chars[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.chars.len());
    }

    /// To the start of the current line (just past the nearest preceding
    /// '\n', or the very start of the text).
    pub fn move_line_start(&mut self) {
        self.cursor = self.current_line_start();
    }

    /// To the end of the current line (the next '\n', or the very end of
    /// the text).
    pub fn move_line_end(&mut self) {
        self.cursor = self.current_line_end();
    }

    fn current_line_start(&self) -> usize {
        self.chars[..self.cursor].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1)
    }

    fn current_line_end(&self) -> usize {
        self.chars[self.cursor..].iter().position(|&c| c == '\n').map_or(self.chars.len(), |i| self.cursor + i)
    }

    /// Empties the buffer and returns whatever text it held -- used when a
    /// prompt is submitted.
    pub fn take(&mut self) -> String {
        let text = self.text();
        self.chars.clear();
        self.cursor = 0;
        text
    }

    /// Replaces the buffer's contents wholesale and puts the cursor at the
    /// end -- used by the slash-command autocomplete dropdown to fill in a
    /// selection (Tab, or Enter on a non-exact prefix match).
    pub fn set_text(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> InputBox {
        let mut b = InputBox::new();
        for c in s.chars() {
            b.insert_char(c);
        }
        b
    }

    #[test]
    fn typing_appends_and_advances_the_cursor() {
        let b = typed("hi");
        assert_eq!(b.text(), "hi");
        assert_eq!(b.cursor(), 2);
    }

    #[test]
    fn backspace_at_the_start_is_a_no_op() {
        let mut b = InputBox::new();
        b.backspace();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn left_then_insert_puts_the_character_in_the_middle() {
        let mut b = typed("ac");
        b.move_left();
        b.insert_char('b');
        assert_eq!(b.text(), "abc");
        assert_eq!(b.cursor(), 2);
    }

    #[test]
    fn right_does_not_overrun_the_end() {
        let mut b = typed("ab");
        b.move_right();
        b.move_right();
        b.move_right();
        assert_eq!(b.cursor(), 2);
    }

    #[test]
    fn backspace_after_moving_left_deletes_the_character_before_the_cursor() {
        let mut b = typed("abc");
        b.move_left();
        b.backspace();
        assert_eq!(b.text(), "ac");
        assert_eq!(b.cursor(), 1);
    }

    #[test]
    fn delete_word_before_cursor_skips_trailing_whitespace_then_removes_the_word() {
        let mut b = typed("run echo hi  ");
        b.delete_word_before_cursor();
        assert_eq!(b.text(), "run echo ");
    }

    #[test]
    fn delete_word_before_cursor_on_a_single_word_clears_it_entirely() {
        let mut b = typed("hello");
        b.delete_word_before_cursor();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn newline_then_home_and_end_navigate_within_the_current_line_only() {
        let mut b = typed("line one\nline two");
        b.move_line_start();
        assert_eq!(b.cursor(), 9, "start of the second line, not the whole buffer");
        b.move_line_end();
        assert_eq!(b.cursor(), b.text().chars().count());
    }

    #[test]
    fn home_on_the_first_line_goes_to_the_very_start() {
        let mut b = typed("ab\ncd");
        b.move_left();
        b.move_left();
        b.move_left();
        assert_eq!(b.cursor(), 2, "positioned right after 'b', still on the first line");
        b.move_line_start();
        assert_eq!(b.cursor(), 0);
    }

    #[test]
    fn line_count_reflects_embedded_newlines() {
        assert_eq!(InputBox::new().line_count(), 1);
        assert_eq!(typed("one\ntwo\nthree").line_count(), 3);
    }

    #[test]
    fn cursor_row_col_tracks_position_across_lines() {
        let mut b = typed("ab\ncd");
        assert_eq!(b.cursor_row_col(), (2, 1));
        b.move_left();
        b.move_left();
        assert_eq!(b.cursor_row_col(), (0, 1));
        b.move_left();
        assert_eq!(b.cursor_row_col(), (2, 0), "just before the newline, end of line 0");
    }

    #[test]
    fn take_empties_the_buffer_and_resets_the_cursor() {
        let mut b = typed("hello");
        let text = b.take();
        assert_eq!(text, "hello");
        assert!(b.is_empty());
        assert_eq!(b.cursor(), 0);
    }
}
