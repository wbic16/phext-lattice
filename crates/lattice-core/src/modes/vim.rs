/// Vim-style editor mode.
///
/// Sub-modes: Normal, Insert, Visual, Command.
/// Normal is the resting state. Motions and operators follow
/// the standard vim grammar: `[count] operator [count] motion`.

use crate::cursor::Position;
use crate::editor::{EditorMode, EditKey, EditKeyCode, EditContext, EditResult, CursorStyle};

#[derive(Debug, Clone, Copy, PartialEq)]
enum VimSubMode {
    Normal,
    Insert,
    Visual,
    Command,
}

/// Vim-style editor mode with Normal/Insert/Visual/Command sub-modes.
#[derive(Debug)]
pub struct VimMode {
    sub_mode: VimSubMode,
    /// Pending command buffer (for multi-key commands like `dd`, `dw`).
    pending: String,
    /// Count prefix for repeated commands.
    count: Option<usize>,
    /// Command-line buffer (for `:` commands).
    command_buf: String,
    /// Pending mode transition.
    next_mode: Option<Box<dyn EditorMode>>,
}

impl VimMode {
    pub fn new() -> VimMode {
        VimMode {
            sub_mode: VimSubMode::Normal,
            pending: String::new(),
            count: None,
            command_buf: String::new(),
            next_mode: None,
        }
    }

    fn effective_count(&self) -> usize {
        self.count.unwrap_or(1)
    }

    fn handle_normal(&mut self, key: EditKey, ctx: &mut EditContext) -> EditResult {
        match &key.code {
            // Count prefix
            EditKeyCode::Char(c) if c.is_ascii_digit() && (*c != '0' || self.count.is_some()) => {
                let digit = *c as usize - '0' as usize;
                self.count = Some(self.count.unwrap_or(0) * 10 + digit);
                return EditResult::Nothing;
            }

            // ── Mode transitions ──
            EditKeyCode::Char('i') => {
                self.sub_mode = VimSubMode::Insert;
                self.clear_pending();
                return EditResult::Status("-- INSERT --".into());
            }
            EditKeyCode::Char('a') => {
                ctx.cursor.move_right(ctx.text, 1);
                self.sub_mode = VimSubMode::Insert;
                self.clear_pending();
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('A') => {
                ctx.cursor.move_end(ctx.text);
                self.sub_mode = VimSubMode::Insert;
                self.clear_pending();
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('I') => {
                ctx.cursor.move_home();
                self.sub_mode = VimSubMode::Insert;
                self.clear_pending();
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('o') => {
                ctx.cursor.move_end(ctx.text);
                ctx.insert("\n");
                self.sub_mode = VimSubMode::Insert;
                self.clear_pending();
                return EditResult::TextChanged;
            }
            EditKeyCode::Char('O') => {
                ctx.cursor.move_home();
                ctx.insert("\n");
                ctx.cursor.move_up(ctx.text, 1);
                self.sub_mode = VimSubMode::Insert;
                self.clear_pending();
                return EditResult::TextChanged;
            }
            EditKeyCode::Char('v') => {
                let pos = ctx.cursor.position();
                ctx.cursor.selection.anchor = pos;
                self.sub_mode = VimSubMode::Visual;
                self.clear_pending();
                return EditResult::Status("-- VISUAL --".into());
            }
            EditKeyCode::Char(':') => {
                self.sub_mode = VimSubMode::Command;
                self.command_buf.clear();
                self.clear_pending();
                return EditResult::Status(":".into());
            }
            EditKeyCode::Escape => {
                self.clear_pending();
                return EditResult::ExitEdit;
            }

            // ── Movement ──
            EditKeyCode::Char('h') | EditKeyCode::Left => {
                let n = self.effective_count();
                ctx.cursor.move_left(ctx.text, n);
                self.clear_pending();
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('l') | EditKeyCode::Right => {
                let n = self.effective_count();
                ctx.cursor.move_right(ctx.text, n);
                self.clear_pending();
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('j') | EditKeyCode::Down => {
                let n = self.effective_count();
                ctx.cursor.move_down(ctx.text, n);
                self.clear_pending();
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('k') | EditKeyCode::Up => {
                let n = self.effective_count();
                ctx.cursor.move_up(ctx.text, n);
                self.clear_pending();
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('0') => {
                ctx.cursor.move_home();
                self.clear_pending();
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('$') => {
                ctx.cursor.move_end(ctx.text);
                self.clear_pending();
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('w') => {
                let n = self.effective_count();
                for _ in 0..n { ctx.cursor.move_word_forward(ctx.text); }
                self.clear_pending();
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('b') => {
                let n = self.effective_count();
                for _ in 0..n { ctx.cursor.move_word_backward(ctx.text); }
                self.clear_pending();
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('g') if self.pending.is_empty() => {
                self.pending.push('g');
                return EditResult::Nothing;
            }
            EditKeyCode::Char('G') => {
                if let Some(n) = self.count {
                    // nG = go to line n
                    let line = n.saturating_sub(1);
                    ctx.cursor.move_to(Position::new(line, 0));
                } else {
                    ctx.cursor.move_bottom(ctx.text);
                }
                self.clear_pending();
                return EditResult::CursorMoved;
            }

            // ── Operators ──
            EditKeyCode::Char('x') => {
                let n = self.effective_count();
                ctx.delete_forward(n);
                self.clear_pending();
                return EditResult::TextChanged;
            }
            EditKeyCode::Char('X') => {
                let n = self.effective_count();
                ctx.delete_backward(n);
                self.clear_pending();
                return EditResult::TextChanged;
            }
            EditKeyCode::Char('d') if self.pending.is_empty() => {
                self.pending.push('d');
                return EditResult::Nothing;
            }
            EditKeyCode::Char('d') if self.pending == "d" => {
                // dd = delete line
                let n = self.effective_count();
                for _ in 0..n { ctx.delete_line(); }
                self.clear_pending();
                return EditResult::TextChanged;
            }

            // ── Undo/Redo ──
            EditKeyCode::Char('u') => {
                self.clear_pending();
                if ctx.undo() {
                    return EditResult::TextChanged;
                }
                return EditResult::Status("Already at oldest change".into());
            }
            EditKeyCode::Char('r') if key.ctrl => {
                self.clear_pending();
                if ctx.redo() {
                    return EditResult::TextChanged;
                }
                return EditResult::Status("Already at newest change".into());
            }

            _ => {}
        }

        // Handle pending 'g' commands
        if self.pending == "g" {
            match &key.code {
                EditKeyCode::Char('g') => {
                    // gg = go to top
                    if let Some(n) = self.count {
                        ctx.cursor.move_to(Position::new(n.saturating_sub(1), 0));
                    } else {
                        ctx.cursor.move_top();
                    }
                    self.clear_pending();
                    return EditResult::CursorMoved;
                }
                _ => {
                    self.clear_pending();
                }
            }
        }

        // Handle pending 'd' + motion
        if self.pending == "d" {
            match &key.code {
                EditKeyCode::Char('w') => {
                    // dw = delete word
                    let n = self.effective_count();
                    for _ in 0..n {
                        let start = ctx.cursor.position().to_offset(ctx.text);
                        ctx.cursor.move_word_forward(ctx.text);
                        let end = ctx.cursor.position().to_offset(ctx.text);
                        ctx.cursor.move_to(Position::from_offset(ctx.text, start));
                        if end > start {
                            ctx.delete_forward(end - start);
                        }
                    }
                    self.clear_pending();
                    return EditResult::TextChanged;
                }
                EditKeyCode::Char('$') => {
                    // d$ = delete to end of line
                    let start = ctx.cursor.position().to_offset(ctx.text);
                    ctx.cursor.move_end(ctx.text);
                    let end = ctx.cursor.position().to_offset(ctx.text);
                    ctx.cursor.move_to(Position::from_offset(ctx.text, start));
                    if end > start {
                        ctx.delete_forward(end - start);
                    }
                    self.clear_pending();
                    return EditResult::TextChanged;
                }
                _ => {
                    self.clear_pending();
                }
            }
        }

        self.clear_pending();
        EditResult::Nothing
    }

    fn handle_insert(&mut self, key: EditKey, ctx: &mut EditContext) -> EditResult {
        match &key.code {
            EditKeyCode::Escape => {
                self.sub_mode = VimSubMode::Normal;
                // Move cursor left one (vim convention on leaving insert)
                ctx.cursor.move_left(ctx.text, 1);
                return EditResult::Status("".into());
            }
            EditKeyCode::Char(c) => {
                ctx.insert(&c.to_string());
                return EditResult::TextChanged;
            }
            EditKeyCode::Enter => {
                ctx.insert_newline();
                return EditResult::TextChanged;
            }
            EditKeyCode::Backspace => {
                ctx.delete_backward(1);
                return EditResult::TextChanged;
            }
            EditKeyCode::Delete => {
                ctx.delete_forward(1);
                return EditResult::TextChanged;
            }
            EditKeyCode::Left => {
                ctx.cursor.move_left(ctx.text, 1);
                return EditResult::CursorMoved;
            }
            EditKeyCode::Right => {
                ctx.cursor.move_right(ctx.text, 1);
                return EditResult::CursorMoved;
            }
            EditKeyCode::Up => {
                ctx.cursor.move_up(ctx.text, 1);
                return EditResult::CursorMoved;
            }
            EditKeyCode::Down => {
                ctx.cursor.move_down(ctx.text, 1);
                return EditResult::CursorMoved;
            }
            EditKeyCode::Home => {
                ctx.cursor.move_home();
                return EditResult::CursorMoved;
            }
            EditKeyCode::End => {
                ctx.cursor.move_end(ctx.text);
                return EditResult::CursorMoved;
            }
            EditKeyCode::Tab => {
                ctx.insert("    "); // 4-space soft tab
                return EditResult::TextChanged;
            }
            _ => {}
        }
        EditResult::Nothing
    }

    fn handle_visual(&mut self, key: EditKey, ctx: &mut EditContext) -> EditResult {
        match &key.code {
            EditKeyCode::Escape => {
                self.sub_mode = VimSubMode::Normal;
                let pos = ctx.cursor.position();
                ctx.cursor.selection.anchor = pos; // collapse selection
                self.clear_pending();
                return EditResult::Status("".into());
            }
            // Movement extends selection
            EditKeyCode::Char('h') | EditKeyCode::Left => {
                let offset = ctx.cursor.position().to_offset(ctx.text);
                let new_offset = offset.saturating_sub(1);
                ctx.cursor.select_to(Position::from_offset(ctx.text, new_offset));
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('l') | EditKeyCode::Right => {
                let offset = ctx.cursor.position().to_offset(ctx.text);
                let new_offset = (offset + 1).min(ctx.text.len());
                ctx.cursor.select_to(Position::from_offset(ctx.text, new_offset));
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('j') | EditKeyCode::Down => {
                ctx.cursor.move_down(ctx.text, 1);
                let pos = ctx.cursor.position();
                ctx.cursor.selection.head = pos;
                return EditResult::CursorMoved;
            }
            EditKeyCode::Char('k') | EditKeyCode::Up => {
                ctx.cursor.move_up(ctx.text, 1);
                let pos = ctx.cursor.position();
                ctx.cursor.selection.head = pos;
                return EditResult::CursorMoved;
            }
            // Delete selection
            EditKeyCode::Char('d') | EditKeyCode::Char('x') => {
                ctx.replace_selection("");
                self.sub_mode = VimSubMode::Normal;
                self.clear_pending();
                return EditResult::TextChanged;
            }
            // Replace selection and enter insert
            EditKeyCode::Char('c') => {
                ctx.replace_selection("");
                self.sub_mode = VimSubMode::Insert;
                self.clear_pending();
                return EditResult::TextChanged;
            }
            _ => {}
        }
        EditResult::Nothing
    }

    fn handle_command(&mut self, key: EditKey, ctx: &mut EditContext) -> EditResult {
        match &key.code {
            EditKeyCode::Escape => {
                self.sub_mode = VimSubMode::Normal;
                self.command_buf.clear();
                return EditResult::Status("".into());
            }
            EditKeyCode::Enter => {
                let cmd = self.command_buf.clone();
                self.sub_mode = VimSubMode::Normal;
                self.command_buf.clear();
                return self.execute_command(&cmd, ctx);
            }
            EditKeyCode::Backspace => {
                self.command_buf.pop();
                if self.command_buf.is_empty() {
                    self.sub_mode = VimSubMode::Normal;
                    return EditResult::Status("".into());
                }
                return EditResult::Status(format!(":{}", self.command_buf));
            }
            EditKeyCode::Char(c) => {
                self.command_buf.push(*c);
                return EditResult::Status(format!(":{}", self.command_buf));
            }
            _ => {}
        }
        EditResult::Nothing
    }

    fn execute_command(&self, cmd: &str, _ctx: &mut EditContext) -> EditResult {
        match cmd.trim() {
            "q" | "q!" => EditResult::ExitEdit,
            "w" => EditResult::Status("Written (save via lattice)".into()),
            "wq" => EditResult::ExitEdit, // save handled by lattice layer
            _ => EditResult::Status(format!("Unknown command: {}", cmd)),
        }
    }

    fn clear_pending(&mut self) {
        self.pending.clear();
        self.count = None;
    }
}

impl EditorMode for VimMode {
    fn name(&self) -> &str {
        match self.sub_mode {
            VimSubMode::Normal => "NORMAL",
            VimSubMode::Insert => "INSERT",
            VimSubMode::Visual => "VISUAL",
            VimSubMode::Command => "COMMAND",
        }
    }

    fn handle_key(&mut self, key: EditKey, ctx: &mut EditContext) -> EditResult {
        match self.sub_mode {
            VimSubMode::Normal => self.handle_normal(key, ctx),
            VimSubMode::Insert => self.handle_insert(key, ctx),
            VimSubMode::Visual => self.handle_visual(key, ctx),
            VimSubMode::Command => self.handle_command(key, ctx),
        }
    }

    fn cursor_style(&self) -> CursorStyle {
        match self.sub_mode {
            VimSubMode::Normal => CursorStyle::Block,
            VimSubMode::Insert => CursorStyle::Line,
            VimSubMode::Visual => CursorStyle::Block,
            VimSubMode::Command => CursorStyle::Line,
        }
    }

    fn status_hint(&self) -> &str {
        match self.sub_mode {
            VimSubMode::Normal => "i:insert a:append o:open d:delete u:undo ^R:redo v:visual Esc:exit",
            VimSubMode::Insert => "Esc:normal",
            VimSubMode::Visual => "d:delete c:change Esc:normal",
            VimSubMode::Command => "Enter:execute Esc:cancel",
        }
    }

    fn transition(&mut self) -> Option<Box<dyn EditorMode>> {
        self.next_mode.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cursor::ScrollCursor;
    use crate::undo::UndoEngine;

    fn test_ctx<'a>(text: &'a mut String, cursor: &'a mut ScrollCursor, undo: &'a mut UndoEngine) -> EditContext<'a> {
        EditContext::new(
            libphext::phext::to_coordinate("1.1.1/1.1.1/1.1.1"),
            text,
            cursor,
            undo,
        )
    }

    #[test]
    fn vim_insert_text() {
        let mut mode = VimMode::new();
        let mut text = String::new();
        let mut cursor = ScrollCursor::new();
        let mut undo = UndoEngine::new();

        // Enter insert mode
        let _r = mode.handle_key(EditKey::char('i'), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        assert_eq!(mode.name(), "INSERT");

        // Type "Hello"
        for c in "Hello".chars() {
            mode.handle_key(EditKey::char(c), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        }
        assert_eq!(text, "Hello");

        // Escape to normal
        mode.handle_key(EditKey::special(EditKeyCode::Escape), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        assert_eq!(mode.name(), "NORMAL");
    }

    #[test]
    fn vim_dd_deletes_line() {
        let mut mode = VimMode::new();
        let mut text = "Line 1\nLine 2\nLine 3".to_string();
        let mut cursor = ScrollCursor::new();
        let mut undo = UndoEngine::new();
        cursor.move_down(&text, 1); // go to line 2

        // dd
        mode.handle_key(EditKey::char('d'), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        mode.handle_key(EditKey::char('d'), &mut test_ctx(&mut text, &mut cursor, &mut undo));

        assert_eq!(text, "Line 1\nLine 3");

        // u to undo
        mode.handle_key(EditKey::char('u'), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        assert_eq!(text, "Line 1\nLine 2\nLine 3");
    }

    #[test]
    fn vim_count_movement() {
        let mut mode = VimMode::new();
        let mut text = "Hello World Foo Bar".to_string();
        let mut cursor = ScrollCursor::new();
        let mut undo = UndoEngine::new();

        // 3l = move right 3
        mode.handle_key(EditKey::char('3'), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        mode.handle_key(EditKey::char('l'), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        assert_eq!(cursor.position(), Position::new(0, 3));
    }

    #[test]
    fn vim_visual_delete() {
        let mut mode = VimMode::new();
        let mut text = "Hello World".to_string();
        let mut cursor = ScrollCursor::new();
        let mut undo = UndoEngine::new();

        // Move to col 5, enter visual, select to col 10, delete
        for _ in 0..5 { mode.handle_key(EditKey::char('l'), &mut test_ctx(&mut text, &mut cursor, &mut undo)); }
        mode.handle_key(EditKey::char('v'), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        for _ in 0..5 { mode.handle_key(EditKey::char('l'), &mut test_ctx(&mut text, &mut cursor, &mut undo)); }
        mode.handle_key(EditKey::char('d'), &mut test_ctx(&mut text, &mut cursor, &mut undo));

        assert_eq!(text, "Hellod"); // " Worl" deleted
    }

    #[test]
    #[allow(non_snake_case)]
    fn vim_gg_and_G() {
        let mut mode = VimMode::new();
        let mut text = "Line 1\nLine 2\nLine 3\nLine 4".to_string();
        let mut cursor = ScrollCursor::new();
        let mut undo = UndoEngine::new();

        // G = go to bottom
        mode.handle_key(EditKey::char('G'), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        assert_eq!(cursor.position().line, 3);

        // gg = go to top
        mode.handle_key(EditKey::char('g'), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        mode.handle_key(EditKey::char('g'), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        assert_eq!(cursor.position().line, 0);

        // 3G = go to line 3
        mode.handle_key(EditKey::char('3'), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        mode.handle_key(EditKey::char('G'), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        assert_eq!(cursor.position().line, 2);
    }

    #[test]
    fn vim_o_open_below() {
        let mut mode = VimMode::new();
        let mut text = "Line 1\nLine 2".to_string();
        let mut cursor = ScrollCursor::new();
        let mut undo = UndoEngine::new();

        // o = open line below
        mode.handle_key(EditKey::char('o'), &mut test_ctx(&mut text, &mut cursor, &mut undo));
        assert_eq!(mode.name(), "INSERT");
        assert_eq!(text, "Line 1\n\nLine 2");
        assert_eq!(cursor.position().line, 1);
    }
}
