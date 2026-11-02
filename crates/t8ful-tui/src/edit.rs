//! One text buffer and one key table for every editable field.
//!
//! The composer, the model and session filters, the masked login prompt, and a
//! question's custom answer all edit through [`Editor`], so a key means the same
//! thing in each of them. The cursor is a character index, not a byte offset, so
//! multi-byte text moves one glyph at a time.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// One editing command, independent of where the text is being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    Insert(char),
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    KillToEnd,
    KillToStart,
    KillWordBack,
    Yank,
    WordLeft,
    WordRight,
}

/// Map a key event to an editing command, if it is one.
///
/// Modifier-carrying keys are matched before the plain text arm so `Alt-b`
/// moves by a word instead of typing `b`.
pub fn binding(key: &KeyEvent) -> Option<Edit> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        KeyCode::Char('a') if control => Some(Edit::LineStart),
        KeyCode::Char('e') if control => Some(Edit::LineEnd),
        KeyCode::Char('b') if control => Some(Edit::Left),
        KeyCode::Char('f') if control => Some(Edit::Right),
        KeyCode::Char('p') if control => Some(Edit::Up),
        KeyCode::Char('n') if control => Some(Edit::Down),
        KeyCode::Char('k') if control => Some(Edit::KillToEnd),
        KeyCode::Char('u') if control => Some(Edit::KillToStart),
        KeyCode::Char('w') if control => Some(Edit::KillWordBack),
        KeyCode::Char('y') if control => Some(Edit::Yank),
        KeyCode::Char('h') if control => Some(Edit::Backspace),
        KeyCode::Char('d') if control => Some(Edit::Delete),
        KeyCode::Char('m') if control => Some(Edit::Newline),
        KeyCode::Char('b') if alt => Some(Edit::WordLeft),
        KeyCode::Char('f') if alt => Some(Edit::WordRight),
        KeyCode::Home => Some(Edit::LineStart),
        KeyCode::End => Some(Edit::LineEnd),
        KeyCode::Left => Some(Edit::Left),
        KeyCode::Right => Some(Edit::Right),
        KeyCode::Up => Some(Edit::Up),
        KeyCode::Down => Some(Edit::Down),
        KeyCode::Backspace => Some(Edit::Backspace),
        KeyCode::Delete => Some(Edit::Delete),
        KeyCode::Enter => Some(Edit::Newline),
        KeyCode::Char(character) if !control && !alt => Some(Edit::Insert(character)),
        _ => None,
    }
}

/// A text buffer with a character cursor and one kill ring entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Editor {
    text: String,
    /// Character index of the cursor, `0..=text.chars().count()`.
    cursor: usize,
    /// Last killed text; `ctrl+y` inserts it again.
    kill: String,
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_text(text: impl Into<String>) -> Self {
        let text = text.into();

        Self {
            cursor: text.chars().count(),
            text,
            kill: String::new(),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// Character index of the cursor.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Move the cursor to a character index, clamped to the buffer.
    pub fn set_cursor(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.len());
    }

    /// Take the buffer contents, leaving it empty.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    /// Replace the buffer and place the cursor at the end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.len();
    }

    /// Byte offset of a character index.
    fn byte(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map(|(offset, _)| offset)
            .unwrap_or(self.text.len())
    }

    pub fn insert(&mut self, character: char) {
        let at = self.byte(self.cursor);

        self.text.insert(at, character);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, text: &str) {
        let at = self.byte(self.cursor);

        self.text.insert_str(at, text);
        self.cursor += text.chars().count();
    }

    /// Remove the character before the cursor. Returns whether anything changed.
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }

        let start = self.byte(self.cursor - 1);
        let end = self.byte(self.cursor);

        self.text.replace_range(start..end, "");
        self.cursor -= 1;

        true
    }

    /// Remove the character at the cursor. Returns whether anything changed.
    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.len() {
            return false;
        }

        let start = self.byte(self.cursor);
        let end = self.byte(self.cursor + 1);

        self.text.replace_range(start..end, "");

        true
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    /// Character range of the line containing `cursor`.
    ///
    /// Scans the text without collecting it: this runs on every cursor
    /// movement, and the composer can hold kilobytes of prompt.
    fn line_bounds(&self, cursor: usize) -> (usize, usize) {
        let mut start = 0;
        let mut index = 0;
        let mut end = None;

        for character in self.text.chars() {
            if character == '\n' {
                if index < cursor {
                    start = index + 1;
                } else {
                    end = Some(index);

                    break;
                }
            }

            index += 1;
        }

        (start, end.unwrap_or(index))
    }

    pub fn line_start(&mut self) {
        let (start, _) = self.line_bounds(self.cursor);

        self.cursor = start;
    }

    pub fn line_end(&mut self) {
        let (_, end) = self.line_bounds(self.cursor);

        self.cursor = end;
    }

    /// Column of the cursor within its line.
    fn column(&self) -> usize {
        let (start, _) = self.line_bounds(self.cursor);

        self.cursor - start
    }

    /// Move up one line, keeping the column where the line is long enough.
    pub fn move_up(&mut self) {
        let (start, _) = self.line_bounds(self.cursor);

        if start == 0 {
            return;
        }

        let previous_end = start - 1;
        let (previous_start, _) = self.line_bounds(previous_end);
        let column = self.column();

        self.cursor = (previous_start + column).min(previous_end);
    }

    /// Move down one line, keeping the column where the line is long enough.
    pub fn move_down(&mut self) {
        let (_, end) = self.line_bounds(self.cursor);

        if end >= self.len() {
            return;
        }

        let next_start = end + 1;
        let (_, next_end) = self.line_bounds(next_start);
        let column = self.column();

        self.cursor = (next_start + column).min(next_end);
    }

    /// Kill from the cursor to the end of the line. Returns whether the text
    /// changed.
    pub fn kill_to_end(&mut self) -> bool {
        let (_, end) = self.line_bounds(self.cursor);

        if end <= self.cursor {
            return false;
        }

        let start = self.byte(self.cursor);
        let stop = self.byte(end);

        self.kill = self.text[start..stop].to_owned();
        self.text.replace_range(start..stop, "");

        true
    }

    /// Kill from the start of the line to the cursor. Returns whether the text
    /// changed.
    pub fn kill_to_start(&mut self) -> bool {
        let (start, _) = self.line_bounds(self.cursor);

        if start >= self.cursor {
            return false;
        }

        let from = self.byte(start);
        let to = self.byte(self.cursor);

        self.kill = self.text[from..to].to_owned();
        self.text.replace_range(from..to, "");
        self.cursor = start;

        true
    }

    /// Kill the word before the cursor, leaving the whitespace in front of it.
    /// Returns whether the text changed.
    pub fn kill_word_back(&mut self) -> bool {
        let chars = self.text.chars().collect::<Vec<_>>();
        let mut start = self.cursor;

        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }

        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }

        if start == self.cursor {
            return false;
        }

        let from = self.byte(start);
        let to = self.byte(self.cursor);

        self.kill = self.text[from..to].to_owned();
        self.text.replace_range(from..to, "");
        self.cursor = start;

        true
    }

    /// Insert the last killed text. Returns whether the text changed.
    pub fn yank(&mut self) -> bool {
        if self.kill.is_empty() {
            return false;
        }

        let kill = self.kill.clone();

        self.insert_str(&kill);

        true
    }

    pub fn word_left(&mut self) {
        let chars = self.text.chars().collect::<Vec<_>>();
        let mut index = self.cursor;

        while index > 0 && chars[index - 1].is_whitespace() {
            index -= 1;
        }

        while index > 0 && !chars[index - 1].is_whitespace() {
            index -= 1;
        }

        self.cursor = index;
    }

    pub fn word_right(&mut self) {
        let chars = self.text.chars().collect::<Vec<_>>();
        let mut index = self.cursor;

        while index < chars.len() && !chars[index].is_whitespace() {
            index += 1;
        }

        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }

        self.cursor = index;
    }

    /// Apply one editing command. Returns whether the text changed.
    ///
    /// Each arm reports its own change, so no copy of the buffer is needed to
    /// compare against: a keystroke in a large prompt stays cheap.
    pub fn apply(&mut self, edit: Edit) -> bool {
        match edit {
            Edit::Insert(character) => {
                self.insert(character);

                true
            }
            Edit::Newline => {
                self.insert('\n');

                true
            }
            Edit::Backspace => self.backspace(),
            Edit::Delete => self.delete(),
            Edit::Left => {
                self.left();

                false
            }
            Edit::Right => {
                self.right();

                false
            }
            Edit::Up => {
                self.move_up();

                false
            }
            Edit::Down => {
                self.move_down();

                false
            }
            Edit::LineStart => {
                self.line_start();

                false
            }
            Edit::LineEnd => {
                self.line_end();

                false
            }
            Edit::KillToEnd => self.kill_to_end(),
            Edit::KillToStart => self.kill_to_start(),
            Edit::KillWordBack => self.kill_word_back(),
            Edit::Yank => self.yank(),
            Edit::WordLeft => {
                self.word_left();

                false
            }
            Edit::WordRight => {
                self.word_right();

                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{Edit, Editor, binding};

    fn editor(text: &str) -> Editor {
        Editor::from_text(text)
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn insertion_and_deletion_follow_the_cursor() {
        let mut editor = editor("ac");

        editor.set_cursor(1);
        editor.insert('b');
        assert_eq!(editor.as_str(), "abc");
        assert_eq!(editor.cursor(), 2);

        editor.backspace();
        assert_eq!(editor.as_str(), "ac");
        assert_eq!(editor.cursor(), 1);

        editor.delete();
        assert_eq!(editor.as_str(), "a");
        assert_eq!(editor.cursor(), 1);
        assert!(!editor.delete());
    }

    #[test]
    fn line_motion_and_kills_share_one_buffer() {
        let mut editor = editor("alpha beta\nsecond");

        editor.set_cursor(5);
        editor.kill_to_end();
        assert_eq!(editor.as_str(), "alpha\nsecond");
        assert_eq!(editor.cursor(), 5);

        editor.yank();
        assert_eq!(editor.as_str(), "alpha beta\nsecond");

        editor.line_end();
        assert_eq!(editor.cursor(), 10);
        editor.kill_to_start();
        assert_eq!(editor.as_str(), "\nsecond");
        assert_eq!(editor.cursor(), 0);

        editor.set_text("one two");
        editor.kill_word_back();
        assert_eq!(editor.as_str(), "one ");
    }

    #[test]
    fn word_motion_stops_at_word_boundaries() {
        let mut editor = editor("one two three");

        editor.word_left();
        assert_eq!(editor.cursor(), 8);
        editor.word_left();
        assert_eq!(editor.cursor(), 4);

        editor.word_right();
        assert_eq!(editor.cursor(), 8, "the cursor stops after the whitespace");

        editor.set_cursor(0);
        editor.kill_word_back();
        assert_eq!(editor.as_str(), "one two three");
    }

    #[test]
    fn vertical_motion_keeps_the_column_when_it_fits() {
        let mut editor = editor("abcdef\nxy\nabcdef");

        editor.set_cursor(4);

        editor.move_down();
        assert_eq!(editor.cursor(), 9, "clamped to the short line");
        editor.move_down();
        assert_eq!(editor.cursor(), 12, "the column is measured after clamping");
        editor.move_up();
        assert_eq!(editor.cursor(), 9);
    }

    #[test]
    fn bindings_match_emacs_and_never_insert_modified_letters() {
        let control = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;

        assert_eq!(
            binding(&key(KeyCode::Char('a'), control)),
            Some(Edit::LineStart)
        );
        assert_eq!(
            binding(&key(KeyCode::Char('e'), control)),
            Some(Edit::LineEnd)
        );
        assert_eq!(binding(&key(KeyCode::Char('p'), control)), Some(Edit::Up));
        assert_eq!(binding(&key(KeyCode::Char('n'), control)), Some(Edit::Down));
        assert_eq!(
            binding(&key(KeyCode::Char('k'), control)),
            Some(Edit::KillToEnd)
        );
        assert_eq!(binding(&key(KeyCode::Char('y'), control)), Some(Edit::Yank));
        assert_eq!(binding(&key(KeyCode::Char('b'), alt)), Some(Edit::WordLeft));
        assert_eq!(
            binding(&key(KeyCode::Char('b'), KeyModifiers::NONE)),
            Some(Edit::Insert('b'))
        );
        assert_eq!(binding(&key(KeyCode::Char('e'), alt)), None);
    }
}
