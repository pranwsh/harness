//! Multi-line input editor: a text buffer (which may hold `\n` from
//! Shift+Enter) with a byte-index cursor kept on a `char` boundary.
//!
//! No terminal or ratatui types live here; every operation is a plain
//! function on the state so it can be unit-tested without a tty.
//! Visual operations take the current wrap `width` explicitly, so the
//! editor never needs to know the terminal size.

use crate::wrap::{ch_width, display_width, wrap_spans};

/// A cursor edit that either mutated the buffer or was a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// Buffer and/or cursor changed.
    Changed,
    /// Nothing changed (e.g. backspace at position 0).
    Unchanged,
}

impl Edit {
    /// True when this edit mutated buffer or cursor.
    pub fn is_changed(self) -> bool {
        matches!(self, Edit::Changed)
    }
}

/// Editable text buffer with a byte-index cursor kept on a `char`
/// boundary at all times. The buffer may hold `\n` (Shift+Enter); all
/// row math below is over *visual* rows after word wrap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Editor {
    text: String,
    cursor: usize,
}

impl Editor {
    pub fn new() -> Self {
        Editor {
            text: String::new(),
            cursor: 0,
        }
    }

    /// The current line contents.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Byte offset of the cursor.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// True when the buffer holds no characters.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Inserts a character at the cursor and advances past it.
    pub fn insert(&mut self, ch: char) -> Edit {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        Edit::Changed
    }

    /// Deletes the character before the cursor, if any.
    pub fn backspace(&mut self) -> Edit {
        if self.cursor == 0 {
            return Edit::Unchanged;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
        Edit::Changed
    }

    /// Deletes the character at the cursor, if any.
    pub fn delete(&mut self) -> Edit {
        if self.cursor >= self.text.len() {
            return Edit::Unchanged;
        }
        let next = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or_else(|| self.text.len());
        self.text.drain(self.cursor..next);
        Edit::Changed
    }

    /// Moves one character left.
    pub fn left(&mut self) -> Edit {
        if self.cursor == 0 {
            return Edit::Unchanged;
        }
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        Edit::Changed
    }

    /// Moves one character right.
    pub fn right(&mut self) -> Edit {
        if self.cursor >= self.text.len() {
            return Edit::Unchanged;
        }
        self.cursor = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or_else(|| self.text.len());
        Edit::Changed
    }

    /// Number of visual rows the buffer occupies at `width` columns.
    /// Always at least 1, even for an empty buffer.
    pub fn row_count(&self, width: usize) -> usize {
        wrap_spans(&self.text, width.max(1)).len().max(1)
    }

    /// Visual `(row, column)` of the cursor at `width` columns. The
    /// column is in display columns from the row's first character.
    pub fn cursor_visual(&self, width: usize) -> (usize, usize) {
        let (row, col) = self.locate(width.max(1));
        (row, col)
    }

    /// Moves the cursor one visual row up, keeping the column when the
    /// target row is wide enough. No-op on the first row.
    pub fn move_up(&mut self, width: usize) -> Edit {
        self.move_vertical(width, -1)
    }

    /// Moves the cursor one visual row down, keeping the column when
    /// the target row is wide enough. No-op on the last row.
    pub fn move_down(&mut self, width: usize) -> Edit {
        self.move_vertical(width, 1)
    }

    /// Moves to the start of the current visual row.
    pub fn home(&mut self, width: usize) -> Edit {
        let width = width.max(1);
        let (row, _) = self.locate(width);
        let (start, _) = wrap_spans(&self.text, width)[row];
        if self.cursor == start {
            return Edit::Unchanged;
        }
        self.cursor = start;
        Edit::Changed
    }

    /// Moves to the end of the current visual row.
    pub fn end(&mut self, width: usize) -> Edit {
        let width = width.max(1);
        let (row, _) = self.locate(width);
        let (_, end) = wrap_spans(&self.text, width)[row];
        if self.cursor == end {
            return Edit::Unchanged;
        }
        self.cursor = end;
        Edit::Changed
    }

    /// Locates the cursor: index of its visual row and its display
    /// column within that row. A cursor sitting exactly on a row
    /// boundary belongs to the row it ends.
    fn locate(&self, width: usize) -> (usize, usize) {
        let rows = wrap_spans(&self.text, width);
        for (i, &(start, end)) in rows.iter().enumerate() {
            if self.cursor <= end {
                let col = display_width(&self.text[start..self.cursor.min(end)]);
                return (i, col);
            }
        }
        // Cursor past the final row end (append-only tail): pin to it.
        let last = rows.len() - 1;
        let (start, end) = rows[last];
        (last, display_width(&self.text[start..end]))
    }

    /// Moves the cursor `delta` visual rows (`-1` up, `+1` down),
    /// landing on the same display column or the row's end, whichever
    /// comes first.
    fn move_vertical(&mut self, width: usize, delta: isize) -> Edit {
        let width = width.max(1);
        let rows = wrap_spans(&self.text, width);
        let (row, col) = self.locate(width);
        let target = (row as isize + delta).clamp(0, rows.len() as isize - 1) as usize;
        if target == row {
            return Edit::Unchanged;
        }
        let (start, end) = rows[target];
        let mut byte = start;
        let mut taken = 0usize;
        for (i, ch) in self.text[start..end].char_indices() {
            let cw = ch_width(ch);
            if taken + cw > col {
                break;
            }
            taken += cw;
            byte = start + i + ch.len_utf8();
        }
        if byte == self.cursor {
            return Edit::Unchanged;
        }
        self.cursor = byte;
        Edit::Changed
    }

    /// Empties the buffer and resets the cursor. Returns the previous text.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace() {
        let mut ed = Editor::new();
        assert_eq!(ed.insert('a'), Edit::Changed);
        assert_eq!(ed.insert('b'), Edit::Changed);
        assert_eq!(ed.text(), "ab");
        assert_eq!(ed.cursor(), 2);
        assert_eq!(ed.backspace(), Edit::Changed);
        assert_eq!(ed.text(), "a");
        assert_eq!(ed.cursor(), 1);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut ed = Editor::new();
        assert_eq!(ed.backspace(), Edit::Unchanged);
        ed.insert('x');
        ed.home(20);
        assert_eq!(ed.backspace(), Edit::Unchanged);
        assert_eq!(ed.text(), "x");
    }

    #[test]
    fn delete_at_end_is_noop() {
        let mut ed = Editor::new();
        assert_eq!(ed.delete(), Edit::Unchanged);
    }

    #[test]
    fn delete_removes_char_at_cursor() {
        let mut ed = Editor::new();
        ed.insert('a');
        ed.insert('b');
        ed.home(20);
        assert_eq!(ed.delete(), Edit::Changed);
        assert_eq!(ed.text(), "b");
        assert_eq!(ed.cursor(), 0);
    }

    #[test]
    fn cursor_motion() {
        let mut ed = Editor::new();
        for c in "abc".chars() {
            ed.insert(c);
        }
        assert_eq!(ed.left(), Edit::Changed);
        assert_eq!(ed.cursor(), 2);
        assert_eq!(ed.left(), Edit::Changed);
        assert_eq!(ed.left(), Edit::Changed);
        assert_eq!(ed.left(), Edit::Unchanged);
        assert_eq!(ed.cursor(), 0);
        assert_eq!(ed.right(), Edit::Changed);
        assert_eq!(ed.right(), Edit::Changed);
        assert_eq!(ed.right(), Edit::Changed);
        assert_eq!(ed.right(), Edit::Unchanged);
        assert_eq!(ed.cursor(), 3);
    }

    #[test]
    fn home_end() {
        let mut ed = Editor::new();
        for c in "ab".chars() {
            ed.insert(c);
        }
        assert_eq!(ed.home(20), Edit::Changed);
        assert_eq!(ed.cursor(), 0);
        assert_eq!(ed.home(20), Edit::Unchanged);
        assert_eq!(ed.end(20), Edit::Changed);
        assert_eq!(ed.cursor(), 2);
        assert_eq!(ed.end(20), Edit::Unchanged);
    }

    #[test]
    fn multibyte_boundaries() {
        let mut ed = Editor::new();
        for c in "aéü".chars() {
            ed.insert(c);
        }
        assert_eq!(ed.text(), "aéü");
        assert_eq!(ed.cursor(), 1 + 2 + 2);
        ed.home(20);
        ed.right(); // now before 'é'
        assert_eq!(ed.delete(), Edit::Changed);
        assert_eq!(ed.text(), "aü");
        assert_eq!(ed.cursor(), 1);
        ed.backspace();
        assert_eq!(ed.text(), "ü");
    }

    #[test]
    fn take_clears() {
        let mut ed = Editor::new();
        for c in "hi".chars() {
            ed.insert(c);
        }
        assert_eq!(ed.take(), "hi");
        assert!(ed.is_empty());
        assert_eq!(ed.cursor(), 0);
        assert_eq!(ed.take(), "");
    }

    fn typed(text: &str) -> Editor {
        let mut ed = Editor::new();
        for c in text.chars() {
            ed.insert(c);
        }
        ed
    }

    #[test]
    fn row_count_counts_visual_rows() {
        assert_eq!(typed("").row_count(10), 1);
        assert_eq!(typed("aaa bbb").row_count(10), 1);
        assert_eq!(typed("aaa bbb").row_count(3), 2);
        assert_eq!(typed("a\nb\nc").row_count(10), 3);
    }

    #[test]
    fn cursor_visual_tracks_rows_and_columns() {
        let mut ed = typed("aaa bbb");
        ed.home(10);
        assert_eq!(ed.cursor_visual(3), (0, 0));
        ed.right();
        ed.right();
        ed.right();
        assert_eq!(ed.cursor_visual(3), (0, 3));
        ed.right();
        assert_eq!(ed.cursor_visual(3), (1, 0));
        ed.end(3);
        assert_eq!(ed.cursor_visual(3), (1, 3));
    }

    #[test]
    fn move_up_down_walks_visual_rows() {
        let mut ed = typed("aaa bbb ccc");
        ed.end(3);
        assert_eq!(ed.cursor_visual(3), (2, 3));
        assert_eq!(ed.move_up(3), Edit::Changed);
        assert_eq!(ed.cursor_visual(3), (1, 3));
        assert_eq!(ed.move_up(3), Edit::Changed);
        assert_eq!(ed.cursor_visual(3), (0, 3));
        assert_eq!(ed.move_up(3), Edit::Unchanged);
        assert_eq!(ed.move_down(3), Edit::Changed);
        assert_eq!(ed.cursor_visual(3), (1, 3));
    }

    #[test]
    fn move_down_clamps_to_short_rows() {
        // "abcdefghi x" at width 4 wraps to ["abcd", "efgh", "i x"].
        let mut ed = typed("abcdefghi x");
        assert_eq!(ed.move_up(4), Edit::Changed);
        assert_eq!(ed.move_up(4), Edit::Changed);
        assert_eq!(ed.cursor_visual(4), (0, 3));
        ed.right();
        assert_eq!(ed.cursor_visual(4), (0, 4));
        assert_eq!(ed.move_down(4), Edit::Changed);
        assert_eq!(ed.cursor_visual(4), (1, 4));
        assert_eq!(ed.move_down(4), Edit::Changed);
        // Column 4 does not fit the 3-wide last row: pinned to its end.
        assert_eq!(ed.cursor_visual(4), (2, 3));
        assert_eq!(ed.move_down(4), Edit::Unchanged);
    }

    #[test]
    fn move_up_down_crosses_hard_lines() {
        let mut ed = typed("ab\ncdef");
        ed.end(10);
        assert_eq!(ed.cursor_visual(10), (1, 4));
        assert_eq!(ed.move_up(10), Edit::Changed);
        assert_eq!(ed.cursor_visual(10), (0, 2));
        assert_eq!(ed.move_up(10), Edit::Unchanged);
    }

    #[test]
    fn home_end_stick_to_visual_rows() {
        let mut ed = typed("aaa bbb ccc");
        ed.end(3);
        assert_eq!(ed.home(3), Edit::Changed);
        assert_eq!(ed.cursor_visual(3), (2, 0));
        assert_eq!(ed.home(3), Edit::Unchanged);
        assert_eq!(ed.end(3), Edit::Changed);
        assert_eq!(ed.cursor_visual(3), (2, 3));
        assert_eq!(ed.move_up(3), Edit::Changed);
        assert_eq!(ed.end(3), Edit::Unchanged);
    }
}
