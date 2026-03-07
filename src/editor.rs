/// Editor — pluggable editing modes for scroll content.
///
/// The editor sits between the navigator (which picks the coordinate)
/// and the mode (which interprets keystrokes). It owns the cursor,
/// undo engine, and scroll content buffer.
///
/// Modes are pluggable: VimMode is default, but EmacsMode, HelixMode,
/// or custom modes can be swapped in.

use libphext::phext::Coordinate;
use crate::cursor::{ScrollCursor, Position};
use crate::undo::{UndoEngine, EditOp};

/// What happened after processing a key.
#[derive(Debug, Clone, PartialEq)]
pub enum EditResult {
    /// Nothing changed.
    Nothing,
    /// Cursor moved (no text change).
    CursorMoved,
    /// Text was modified.
    TextChanged,
    /// Exit edit mode, return to lattice navigation.
    ExitEdit,
    /// A status message to display.
    Status(String),
}

/// Cursor rendering style.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CursorStyle {
    /// Block cursor (vim normal mode).
    Block,
    /// Line cursor (vim insert mode).
    Line,
    /// Underline cursor.
    Underline,
}

/// A key event abstraction (decoupled from any TUI/GUI framework).
#[derive(Debug, Clone, PartialEq)]
pub struct EditKey {
    pub code: EditKeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditKeyCode {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
}

impl EditKey {
    pub fn char(c: char) -> EditKey {
        EditKey { code: EditKeyCode::Char(c), ctrl: false, alt: false, shift: false }
    }

    pub fn ctrl(c: char) -> EditKey {
        EditKey { code: EditKeyCode::Char(c), ctrl: true, alt: false, shift: false }
    }

    pub fn special(code: EditKeyCode) -> EditKey {
        EditKey { code, ctrl: false, alt: false, shift: false }
    }
}

/// The editing context passed to modes. Provides access to
/// text content, cursor, and edit operations.
pub struct EditContext<'a> {
    /// The scroll coordinate being edited.
    pub coordinate: Coordinate,
    /// The current text content (mutable).
    pub text: &'a mut String,
    /// The cursor state.
    pub cursor: &'a mut ScrollCursor,
    /// The undo engine (for recording edits).
    undo: &'a mut UndoEngine,
}

impl<'a> EditContext<'a> {
    pub fn new(
        coordinate: Coordinate,
        text: &'a mut String,
        cursor: &'a mut ScrollCursor,
        undo: &'a mut UndoEngine,
    ) -> EditContext<'a> {
        EditContext { coordinate, text, cursor, undo }
    }

    /// Insert text at the cursor position.
    pub fn insert(&mut self, s: &str) {
        let offset = self.cursor.position().to_offset(self.text);
        let cursor_before = self.cursor.position();
        let op = EditOp::Insert { offset, text: s.to_string() };
        *self.text = op.apply(self.text);
        self.cursor.move_right(self.text, s.len());
        let cursor_after = self.cursor.position();
        self.undo.record(self.coordinate, op, cursor_before, cursor_after);
    }

    /// Insert a newline at the cursor position.
    pub fn insert_newline(&mut self) {
        self.insert("\n");
    }

    /// Delete n characters before the cursor (backspace).
    pub fn delete_backward(&mut self, n: usize) {
        let offset = self.cursor.position().to_offset(self.text);
        if offset == 0 { return; }
        let delete_start = offset.saturating_sub(n);
        let deleted = self.text[delete_start..offset].to_string();
        let cursor_before = self.cursor.position();
        let op = EditOp::Delete { offset: delete_start, deleted };
        *self.text = op.apply(self.text);
        self.cursor.move_to(Position::from_offset(self.text, delete_start));
        let cursor_after = self.cursor.position();
        self.undo.record(self.coordinate, op, cursor_before, cursor_after);
    }

    /// Delete n characters at the cursor position (delete key).
    pub fn delete_forward(&mut self, n: usize) {
        let offset = self.cursor.position().to_offset(self.text);
        if offset >= self.text.len() { return; }
        let delete_end = (offset + n).min(self.text.len());
        let deleted = self.text[offset..delete_end].to_string();
        let cursor_before = self.cursor.position();
        let op = EditOp::Delete { offset, deleted };
        *self.text = op.apply(self.text);
        let cursor_after = self.cursor.position(); // cursor doesn't move on forward delete
        self.undo.record(self.coordinate, op, cursor_before, cursor_after);
    }

    /// Delete the current line.
    pub fn delete_line(&mut self) {
        let pos = self.cursor.position();
        let lines: Vec<&str> = self.text.lines().collect();
        if pos.line >= lines.len() { return; }

        // Find byte range of the line (including trailing newline)
        let mut offset = 0;
        for (i, line) in self.text.lines().enumerate() {
            if i == pos.line {
                let line_end = offset + line.len();
                let delete_end = if line_end < self.text.len() { line_end + 1 } else { line_end };
                let deleted = self.text[offset..delete_end].to_string();
                let cursor_before = self.cursor.position();
                let op = EditOp::Delete { offset, deleted };
                *self.text = op.apply(self.text);
                self.cursor.move_to(Position::from_offset(self.text, offset));
                let cursor_after = self.cursor.position();
                self.undo.record(self.coordinate, op, cursor_before, cursor_after);
                return;
            }
            offset += line.len() + 1; // +1 for newline
        }
    }

    /// Replace the selected text with new text.
    pub fn replace_selection(&mut self, new_text: &str) {
        let sel = self.cursor.selection;
        if sel.is_cursor() { return; }

        let start = sel.start().to_offset(self.text);
        let end = sel.end().to_offset(self.text);
        let old_text = self.text[start..end].to_string();
        let cursor_before = self.cursor.position();

        let op = EditOp::Replace {
            offset: start,
            old_text,
            new_text: new_text.to_string(),
        };
        *self.text = op.apply(self.text);
        let new_end = start + new_text.len();
        self.cursor.move_to(Position::from_offset(self.text, new_end));
        let cursor_after = self.cursor.position();
        self.undo.record(self.coordinate, op, cursor_before, cursor_after);
    }

    /// Undo the last edit.
    pub fn undo(&mut self) -> bool {
        if let Some((op, cursor_pos)) = self.undo.undo(&self.coordinate) {
            *self.text = op.apply(self.text);
            self.cursor.move_to(cursor_pos);
            true
        } else {
            false
        }
    }

    /// Redo the next edit.
    pub fn redo(&mut self) -> bool {
        if let Some((op, cursor_pos)) = self.undo.redo(&self.coordinate) {
            *self.text = op.apply(self.text);
            self.cursor.move_to(cursor_pos);
            true
        } else {
            false
        }
    }
}

/// The trait that all editor modes implement.
///
/// A mode handles keystrokes and manipulates the editing context.
/// It can return a different mode to transition to (e.g., Normal → Insert).
pub trait EditorMode: std::fmt::Debug {
    /// Name of this mode (shown in status bar).
    fn name(&self) -> &str;

    /// Handle a key event. Returns what happened.
    fn handle_key(&mut self, key: EditKey, ctx: &mut EditContext) -> EditResult;

    /// The cursor style for this mode.
    fn cursor_style(&self) -> CursorStyle;

    /// Status bar hint text.
    fn status_hint(&self) -> &str;

    /// If this mode wants to transition to another mode, return it.
    /// Called after handle_key. None means stay in current mode.
    fn transition(&mut self) -> Option<Box<dyn EditorMode>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::undo::UndoEngine;

    #[test]
    fn context_insert_and_undo() {
        let coord = libphext::phext::to_coordinate("1.1.1/1.1.1/1.1.1");
        let mut text = "Hello".to_string();
        let mut cursor = ScrollCursor::new();
        let mut undo = UndoEngine::new();
        cursor.move_end(&text);

        {
            let mut ctx = EditContext::new(coord, &mut text, &mut cursor, &mut undo);
            ctx.insert(" World");
        }
        assert_eq!(text, "Hello World");

        {
            let mut ctx = EditContext::new(coord, &mut text, &mut cursor, &mut undo);
            assert!(ctx.undo());
        }
        assert_eq!(text, "Hello");
    }

    #[test]
    fn context_delete_backward() {
        let coord = libphext::phext::to_coordinate("1.1.1/1.1.1/1.1.1");
        let mut text = "Hello World".to_string();
        let mut cursor = ScrollCursor::new();
        let mut undo = UndoEngine::new();
        cursor.move_to(Position::new(0, 5));

        {
            let mut ctx = EditContext::new(coord, &mut text, &mut cursor, &mut undo);
            ctx.delete_backward(2);
        }
        assert_eq!(text, "Hel World");

        {
            let mut ctx = EditContext::new(coord, &mut text, &mut cursor, &mut undo);
            assert!(ctx.undo());
        }
        assert_eq!(text, "Hello World");
    }

    #[test]
    fn context_delete_line() {
        let coord = libphext::phext::to_coordinate("1.1.1/1.1.1/1.1.1");
        let mut text = "Line 1\nLine 2\nLine 3".to_string();
        let mut cursor = ScrollCursor::new();
        let mut undo = UndoEngine::new();
        cursor.move_down(&text, 1); // go to line 2

        {
            let mut ctx = EditContext::new(coord, &mut text, &mut cursor, &mut undo);
            ctx.delete_line();
        }
        assert_eq!(text, "Line 1\nLine 3");

        {
            let mut ctx = EditContext::new(coord, &mut text, &mut cursor, &mut undo);
            assert!(ctx.undo());
        }
        assert_eq!(text, "Line 1\nLine 2\nLine 3");
    }
}
