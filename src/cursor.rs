/// Intra-scroll cursor — position and selection within a single scroll.
///
/// This is the bridge between Lattice mode (inter-scroll navigation)
/// and Edit mode (intra-scroll text manipulation). Every cursor
/// movement in Edit mode operates through this model.
///
/// Follows the Helix/vim philosophy: selections are first-class.
/// A cursor without a selection is just a zero-width selection.

/// Position within a scroll's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Line number (0-indexed).
    pub line: usize,
    /// Column (0-indexed, character offset — not byte offset).
    pub col: usize,
}

impl Position {
    pub fn new(line: usize, col: usize) -> Position {
        Position { line, col }
    }

    pub fn origin() -> Position {
        Position { line: 0, col: 0 }
    }

    /// Convert a byte offset in text to a Position.
    pub fn from_offset(text: &str, offset: usize) -> Position {
        let offset = offset.min(text.len());
        let mut line = 0;
        let mut col = 0;
        for (i, ch) in text.char_indices() {
            if i >= offset { break; }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        Position { line, col }
    }

    /// Convert this Position to a byte offset in text.
    pub fn to_offset(&self, text: &str) -> usize {
        let mut current_line = 0;
        let mut current_col = 0;
        for (i, ch) in text.char_indices() {
            if current_line == self.line && current_col == self.col {
                return i;
            }
            if ch == '\n' {
                if current_line == self.line {
                    // Past end of this line — clamp to newline
                    return i;
                }
                current_line += 1;
                current_col = 0;
            } else {
                current_col += 1;
            }
        }
        text.len()
    }
}

impl std::fmt::Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.line + 1, self.col + 1)
    }
}

/// A selection range within a scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// The anchor (where selection started).
    pub anchor: Position,
    /// The head (where cursor currently is).
    pub head: Position,
}

impl Selection {
    /// A zero-width selection (cursor) at a position.
    pub fn cursor(pos: Position) -> Selection {
        Selection { anchor: pos, head: pos }
    }

    /// Whether this is a zero-width selection (just a cursor).
    pub fn is_cursor(&self) -> bool {
        self.anchor == self.head
    }

    /// The start of the selection (earlier position).
    pub fn start(&self) -> Position {
        if self.anchor.line < self.head.line
            || (self.anchor.line == self.head.line && self.anchor.col <= self.head.col)
        {
            self.anchor
        } else {
            self.head
        }
    }

    /// The end of the selection (later position).
    pub fn end(&self) -> Position {
        if self.anchor.line < self.head.line
            || (self.anchor.line == self.head.line && self.anchor.col <= self.head.col)
        {
            self.head
        } else {
            self.anchor
        }
    }
}

/// The cursor state within a scroll.
#[derive(Debug, Clone)]
pub struct ScrollCursor {
    /// Primary selection (always exists — cursor is a zero-width selection).
    pub selection: Selection,
    /// Additional selections for multi-cursor editing (future).
    pub extra_selections: Vec<Selection>,
    /// Desired column for vertical movement (remembers horizontal position
    /// when moving through lines of different lengths).
    pub sticky_col: Option<usize>,
}

impl ScrollCursor {
    /// New cursor at the origin (0,0).
    pub fn new() -> ScrollCursor {
        ScrollCursor {
            selection: Selection::cursor(Position::origin()),
            extra_selections: Vec::new(),
            sticky_col: None,
        }
    }

    /// New cursor at a specific position.
    pub fn at(pos: Position) -> ScrollCursor {
        ScrollCursor {
            selection: Selection::cursor(pos),
            extra_selections: Vec::new(),
            sticky_col: None,
        }
    }

    /// Current cursor position (selection head).
    pub fn position(&self) -> Position {
        self.selection.head
    }

    /// Move cursor to a position, collapsing any selection.
    pub fn move_to(&mut self, pos: Position) {
        self.selection = Selection::cursor(pos);
        self.sticky_col = None;
    }

    /// Extend selection to a position (keep anchor, move head).
    pub fn select_to(&mut self, pos: Position) {
        self.selection.head = pos;
        self.sticky_col = None;
    }

    /// Move cursor left by n characters within text.
    pub fn move_left(&mut self, text: &str, n: usize) {
        let offset = self.selection.head.to_offset(text);
        let new_offset = offset.saturating_sub(n);
        self.move_to(Position::from_offset(text, new_offset));
    }

    /// Move cursor right by n characters within text.
    pub fn move_right(&mut self, text: &str, n: usize) {
        let offset = self.selection.head.to_offset(text);
        let new_offset = (offset + n).min(text.len());
        self.move_to(Position::from_offset(text, new_offset));
    }

    /// Move cursor up by n lines, preserving column.
    pub fn move_up(&mut self, text: &str, n: usize) {
        let pos = self.selection.head;
        if pos.line == 0 { return; }

        let target_col = self.sticky_col.unwrap_or(pos.col);
        let new_line = pos.line.saturating_sub(n);
        let line_len = line_length(text, new_line);
        let new_col = target_col.min(line_len);

        self.selection = Selection::cursor(Position::new(new_line, new_col));
        self.sticky_col = Some(target_col);
    }

    /// Move cursor down by n lines, preserving column.
    pub fn move_down(&mut self, text: &str, n: usize) {
        let pos = self.selection.head;
        let total_lines = text.lines().count().max(1);
        if pos.line + 1 >= total_lines { return; }

        let target_col = self.sticky_col.unwrap_or(pos.col);
        let new_line = (pos.line + n).min(total_lines - 1);
        let line_len = line_length(text, new_line);
        let new_col = target_col.min(line_len);

        self.selection = Selection::cursor(Position::new(new_line, new_col));
        self.sticky_col = Some(target_col);
    }

    /// Move to beginning of current line.
    pub fn move_home(&mut self) {
        self.move_to(Position::new(self.selection.head.line, 0));
    }

    /// Move to end of current line.
    pub fn move_end(&mut self, text: &str) {
        let len = line_length(text, self.selection.head.line);
        self.move_to(Position::new(self.selection.head.line, len));
    }

    /// Move to start of text.
    pub fn move_top(&mut self) {
        self.move_to(Position::origin());
    }

    /// Move to end of text.
    pub fn move_bottom(&mut self, text: &str) {
        let last_line = text.lines().count().saturating_sub(1);
        let last_col = line_length(text, last_line);
        self.move_to(Position::new(last_line, last_col));
    }

    /// Move to the next word boundary.
    pub fn move_word_forward(&mut self, text: &str) {
        let offset = self.selection.head.to_offset(text);
        let bytes = text.as_bytes();
        let len = bytes.len();
        let mut i = offset;

        // Skip current word chars
        while i < len && !bytes[i].is_ascii_whitespace() { i += 1; }
        // Skip whitespace
        while i < len && bytes[i].is_ascii_whitespace() { i += 1; }

        self.move_to(Position::from_offset(text, i));
    }

    /// Move to the previous word boundary.
    pub fn move_word_backward(&mut self, text: &str) {
        let offset = self.selection.head.to_offset(text);
        let bytes = text.as_bytes();
        if offset == 0 { return; }
        let mut i = offset - 1;

        // Skip whitespace
        while i > 0 && bytes[i].is_ascii_whitespace() { i -= 1; }
        // Skip word chars
        while i > 0 && !bytes[i].is_ascii_whitespace() { i -= 1; }
        if i > 0 { i += 1; }

        self.move_to(Position::from_offset(text, i));
    }
}

/// Get the character length of a specific line in text.
fn line_length(text: &str, line_idx: usize) -> usize {
    text.lines().nth(line_idx).map(|l| l.chars().count()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Hello World\nSecond line\nThird";

    #[test]
    fn position_roundtrip() {
        let text = SAMPLE;
        for offset in 0..=text.len() {
            let pos = Position::from_offset(text, offset);
            let back = pos.to_offset(text);
            // Roundtrip may clamp to line end, but should be <= original
            assert!(back <= offset || back == offset,
                "offset {} -> {:?} -> {}", offset, pos, back);
        }
    }

    #[test]
    fn position_line_col() {
        let pos = Position::from_offset(SAMPLE, 12); // 'S' in "Second"
        assert_eq!(pos.line, 1);
        assert_eq!(pos.col, 0);
    }

    #[test]
    fn cursor_movement() {
        let mut cursor = ScrollCursor::new();
        cursor.move_right(SAMPLE, 5);
        assert_eq!(cursor.position(), Position::new(0, 5));

        cursor.move_down(SAMPLE, 1);
        assert_eq!(cursor.position().line, 1);

        cursor.move_home();
        assert_eq!(cursor.position().col, 0);

        cursor.move_end(SAMPLE);
        assert_eq!(cursor.position().col, 11); // "Second line" = 11 chars
    }

    #[test]
    fn cursor_sticky_col() {
        let text = "Short\nA much longer line\nTiny";
        let mut cursor = ScrollCursor::new();
        cursor.move_end(text); // col 5 on "Short"
        cursor.move_down(text, 1); // "A much longer line" — stays at col 5
        assert_eq!(cursor.position(), Position::new(1, 5));
        cursor.move_down(text, 1); // "Tiny" — clamps to col 4
        assert_eq!(cursor.position(), Position::new(2, 4));
        // But sticky should remember 5
        cursor.move_up(text, 1);
        assert_eq!(cursor.position(), Position::new(1, 5));
    }

    #[test]
    fn selection_ordering() {
        let sel = Selection {
            anchor: Position::new(2, 5),
            head: Position::new(0, 3),
        };
        assert_eq!(sel.start(), Position::new(0, 3));
        assert_eq!(sel.end(), Position::new(2, 5));
        assert!(!sel.is_cursor());
    }

    #[test]
    fn word_movement() {
        let text = "hello world foo";
        let mut cursor = ScrollCursor::new();
        cursor.move_word_forward(text);
        assert_eq!(cursor.position(), Position::new(0, 6)); // start of "world"
        cursor.move_word_forward(text);
        assert_eq!(cursor.position(), Position::new(0, 12)); // start of "foo"
        cursor.move_word_backward(text);
        assert_eq!(cursor.position(), Position::new(0, 6)); // back to "world"
    }
}
