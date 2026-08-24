//! Minimal single-line text field shared by the API-key modal, the command
//! palette filter, the provider search and the reasoning-map filter.
//!
//! Tracks a byte-cursor (always on a char boundary) so insert/delete/edit
//! operations stay UTF-8 safe regardless of input width.

/// A single-line editable text field with a cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditField {
    value: String,
    /// Byte offset into `value` (always on a char boundary).
    cursor: usize,
}

impl EditField {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_value(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.len();
        Self { value, cursor }
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    /// Byte length of the field.
    pub fn len(&self) -> usize {
        self.value.len()
    }

    /// Replace the whole value; the cursor moves to the end.
    pub fn set(&mut self, value: impl Into<String>) {
        self.value = value.into();
        self.cursor = self.value.len();
    }

    pub fn clear(&mut self) {
        self.value.clear();
        self.cursor = 0;
    }

    /// Insert `c` at the cursor.
    pub fn insert(&mut self, c: char) {
        self.value.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Remove the char before the cursor (no-op at the start).
    pub fn backspace(&mut self) {
        if let Some((idx, _)) = self.value[..self.cursor].char_indices().next_back() {
            self.value.remove(idx);
            self.cursor = idx;
        }
    }

    /// Remove the char at the cursor (no-op at the end).
    pub fn delete(&mut self) {
        if self.cursor < self.value.len() {
            self.value.remove(self.cursor);
        }
    }

    /// Move the cursor by `delta` characters, clamped to the field.
    pub fn move_cursor(&mut self, delta: i32) {
        let before = self.value[..self.cursor].chars().count();
        let total = self.value.chars().count();
        let target = (before as i32 + delta).clamp(0, total as i32) as usize;
        self.cursor = self
            .value
            .char_indices()
            .nth(target)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len());
    }

    /// Insert `text` at the cursor. The caller strips control characters
    /// (see `reduce::paste_text`); this just splices chars in order.
    pub fn paste(&mut self, text: &str) {
        for c in text.chars() {
            self.insert(c);
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }
}

impl From<&str> for EditField {
    fn from(s: &str) -> Self {
        Self::from_value(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace_respect_cursor() {
        let mut f = EditField::from_value("abc");
        f.move_cursor(-1);
        f.insert('X');
        assert_eq!(f.as_str(), "abXc");
        f.backspace();
        assert_eq!(f.as_str(), "abc");
        // Backspace at the start is a no-op.
        f.move_cursor(-100);
        f.backspace();
        assert_eq!(f.as_str(), "abc");
    }

    #[test]
    fn delete_removes_char_at_cursor() {
        let mut f = EditField::from_value("abc");
        f.move_cursor(-2);
        f.delete();
        assert_eq!(f.as_str(), "ac");
        // Delete at the end is a no-op.
        f.move_cursor(100);
        f.delete();
        assert_eq!(f.as_str(), "ac");
    }

    #[test]
    fn move_cursor_clamps_and_is_utf8_safe() {
        let mut f = EditField::from_value("héllo"); // 'é' is 2 bytes
        f.move_cursor(-100); // clamp to the start
        assert_eq!(f.as_str(), "héllo");
        // Cursor never lands mid-character: moving by char counts works.
        f.insert('!');
        assert_eq!(f.as_str(), "!héllo");
        f.move_cursor(100);
        f.insert('?');
        assert_eq!(f.as_str(), "!héllo?");
        // -3 from the end lands after the second char ('é').
        let mut g = EditField::from_value("héllo");
        g.move_cursor(-3);
        g.insert('!');
        assert_eq!(g.as_str(), "hé!llo");
    }

    #[test]
    fn set_resets_cursor_to_end() {
        let mut f = EditField::from_value("abc");
        f.move_cursor(-1);
        f.set("xyz");
        f.insert('Z');
        assert_eq!(f.as_str(), "xyzZ");
        f.clear();
        assert!(f.is_empty());
    }
}
