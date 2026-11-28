/// Undo/Redo — edit history encoded as phext.
///
/// The work file (`<name>.work.phext`) is a phext lattice where each
/// scroll is a serialized edit operation. The coordinate encodes:
///
///   Z arm: mirrors the target scroll's Z (library.shelf.series)
///   Y arm: mirrors the target scroll's Y (collection.volume.book)
///   X arm: chapter = file-wide serialization sequence, section = 1,
///          scroll = 1
///
/// The chapter is a serialization sequence (records are written in
/// target-coordinate order, then per-target sequence order), not the
/// per-target edit sequence: two targets sharing Z/Y arms and a
/// per-target sequence would otherwise collide at one work
/// coordinate and merge into a single scroll. The full target
/// coordinate is recorded in each scroll's `target:` header line,
/// so a reloaded record re-targets the exact scroll that was edited.
/// (Work files written before the header existed still load; their
/// targets are recovered from the work coordinate with the X arm
/// reset to 1.1.1, as before.)
///
/// Each scroll contains a text-encoded edit record:
///   ```text
///   op:insert
///   offset:42
///   len:11
///   cursor:3:7>3:18
///   ---
///   hello world
///   ```
///
/// This means the work file is itself a navigable phext — you can
/// open it in phext-nav and browse the edit history of any scroll
/// by navigating to its Z/Y coordinates.

use libphext::phext::{Coordinate, default_coordinate, to_coordinate};
use crate::cursor::Position;
use std::collections::HashMap;

/// An edit operation that can be undone/redone.
#[derive(Debug, Clone)]
pub enum EditOp {
    /// Insert text at byte offset.
    Insert {
        offset: usize,
        text: String,
    },
    /// Delete text at byte offset (stores deleted text for undo).
    Delete {
        offset: usize,
        deleted: String,
    },
    /// Replace a range of text.
    Replace {
        offset: usize,
        old_text: String,
        new_text: String,
    },
}

/// Largest UTF-8 char boundary in `text` at or below `offset`.
/// `offset` must already be clamped to `text.len()`.
fn floor_char_boundary(text: &str, mut offset: usize) -> usize {
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

impl EditOp {
    /// Apply this operation to text, returning the new text.
    ///
    /// Offsets are byte offsets. An offset (or range end) that lands
    /// inside a multi-byte UTF-8 character is snapped down to the
    /// nearest char boundary, so no offset value can panic. Ops whose
    /// offsets already sit on char boundaries — every offset the
    /// editor itself produces — behave exactly as before, and the
    /// apply/inverse round trip is preserved for them.
    pub fn apply(&self, text: &str) -> String {
        match self {
            EditOp::Insert { offset, text: ins } => {
                let offset = floor_char_boundary(text, (*offset).min(text.len()));
                let mut result = String::with_capacity(text.len() + ins.len());
                result.push_str(&text[..offset]);
                result.push_str(ins);
                result.push_str(&text[offset..]);
                result
            }
            EditOp::Delete { offset, deleted } => {
                let offset = floor_char_boundary(text, (*offset).min(text.len()));
                let end = floor_char_boundary(text, (offset + deleted.len()).min(text.len()));
                let mut result = String::with_capacity(text.len() - (end - offset));
                result.push_str(&text[..offset]);
                result.push_str(&text[end..]);
                result
            }
            EditOp::Replace { offset, old_text, new_text } => {
                let offset = floor_char_boundary(text, (*offset).min(text.len()));
                let end = floor_char_boundary(text, (offset + old_text.len()).min(text.len()));
                let mut result = String::with_capacity(text.len() - (end - offset) + new_text.len());
                result.push_str(&text[..offset]);
                result.push_str(new_text);
                result.push_str(&text[end..]);
                result
            }
        }
    }

    /// Compute the inverse operation (for undo).
    pub fn inverse(&self) -> EditOp {
        match self {
            EditOp::Insert { offset, text } => EditOp::Delete {
                offset: *offset,
                deleted: text.clone(),
            },
            EditOp::Delete { offset, deleted } => EditOp::Insert {
                offset: *offset,
                text: deleted.clone(),
            },
            EditOp::Replace { offset, old_text, new_text } => EditOp::Replace {
                offset: *offset,
                old_text: new_text.clone(),
                new_text: old_text.clone(),
            },
        }
    }

    /// Serialize to the phext scroll format.
    pub fn to_scroll(&self, cursor_before: Position, cursor_after: Position) -> String {
        let (op_name, offset, len) = match self {
            EditOp::Insert { offset, text } => ("insert", *offset, text.len()),
            EditOp::Delete { offset, deleted } => ("delete", *offset, deleted.len()),
            EditOp::Replace { offset, old_text, .. } => ("replace", *offset, old_text.len()),
        };

        let content = match self {
            EditOp::Insert { text, .. } => text.as_str(),
            EditOp::Delete { deleted, .. } => deleted.as_str(),
            EditOp::Replace { old_text, new_text, .. } => {
                // Encode both old and new separated by a marker
                return format!(
                    "op:{}\noffset:{}\nlen:{}\ncursor:{}>{}\n---\n{}\n===\n{}",
                    op_name, offset, len, cursor_before, cursor_after, old_text, new_text,
                );
            }
        };

        format!(
            "op:{}\noffset:{}\nlen:{}\ncursor:{}>{}\n---\n{}",
            op_name, offset, len, cursor_before, cursor_after, content,
        )
    }

    /// Deserialize from a phext scroll.
    pub fn from_scroll(scroll: &str) -> Option<(EditOp, Position, Position)> {
        let mut lines = scroll.lines();

        let op_line = lines.next()?;
        let op_name = op_line.strip_prefix("op:")?;

        let offset_line = lines.next()?;
        let offset: usize = offset_line.strip_prefix("offset:")?.parse().ok()?;

        let _len_line = lines.next()?; // len — we reconstruct from content

        let cursor_line = lines.next()?;
        let cursor_str = cursor_line.strip_prefix("cursor:")?;
        let (before_str, after_str) = cursor_str.split_once('>')?;
        let cursor_before = parse_position(before_str)?;
        let cursor_after = parse_position(after_str)?;

        let separator = lines.next()?;
        if separator != "---" { return None; }

        // Remaining lines are content
        let rest: String = lines.collect::<Vec<_>>().join("\n");

        let op = match op_name {
            "insert" => EditOp::Insert { offset, text: rest },
            "delete" => EditOp::Delete { offset, deleted: rest },
            "replace" => {
                let (old, new) = rest.split_once("\n===\n")?;
                EditOp::Replace {
                    offset,
                    old_text: old.to_string(),
                    new_text: new.to_string(),
                }
            }
            _ => return None,
        };

        Some((op, cursor_before, cursor_after))
    }
}

fn parse_position(s: &str) -> Option<Position> {
    let (line_str, col_str) = s.split_once(':')?;
    let line: usize = line_str.parse().ok()?;
    let col: usize = col_str.parse().ok()?;
    Some(Position::new(line.saturating_sub(1), col.saturating_sub(1)))
}

/// An undo record: operation + cursor state.
#[derive(Debug, Clone)]
pub struct UndoRecord {
    /// Which scroll this edit applies to.
    pub target: Coordinate,
    /// The edit operation.
    pub op: EditOp,
    /// Cursor position before the edit.
    pub cursor_before: Position,
    /// Cursor position after the edit.
    pub cursor_after: Position,
    /// Edit sequence number (1-indexed, per target scroll).
    pub sequence: usize,
}

/// Per-scroll undo/redo stack.
#[derive(Debug)]
struct ScrollHistory {
    /// All recorded edits for this scroll.
    records: Vec<UndoRecord>,
    /// Current position in the history (index of next undo).
    cursor: usize,
}

impl ScrollHistory {
    fn new() -> ScrollHistory {
        ScrollHistory {
            records: Vec::new(),
            cursor: 0,
        }
    }

    fn push(&mut self, record: UndoRecord) {
        // Truncate any redo history
        self.records.truncate(self.cursor);
        self.records.push(record);
        self.cursor = self.records.len();
    }

    fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    fn can_redo(&self) -> bool {
        self.cursor < self.records.len()
    }

    fn undo(&mut self) -> Option<&UndoRecord> {
        if self.cursor > 0 {
            self.cursor -= 1;
            Some(&self.records[self.cursor])
        } else {
            None
        }
    }

    fn redo(&mut self) -> Option<&UndoRecord> {
        if self.cursor < self.records.len() {
            let record = &self.records[self.cursor];
            self.cursor += 1;
            Some(record)
        } else {
            None
        }
    }

    fn depth(&self) -> usize {
        self.cursor
    }

    fn total(&self) -> usize {
        self.records.len()
    }
}

/// The undo engine: manages edit history across all scrolls.
///
/// Each scroll has its own independent undo/redo stack.
/// The full history can be serialized to a work phext file.
#[derive(Debug)]
pub struct UndoEngine {
    /// Per-scroll undo stacks, keyed by target coordinate.
    stacks: HashMap<Coordinate, ScrollHistory>,
    /// Global edit counter for work file sequencing.
    global_sequence: usize,
}

impl UndoEngine {
    pub fn new() -> UndoEngine {
        UndoEngine {
            stacks: HashMap::new(),
            global_sequence: 0,
        }
    }

    /// Record an edit operation.
    pub fn record(
        &mut self,
        target: Coordinate,
        op: EditOp,
        cursor_before: Position,
        cursor_after: Position,
    ) {
        self.global_sequence += 1;
        let stack = self.stacks.entry(target).or_insert_with(ScrollHistory::new);
        let sequence = stack.total() + 1;
        stack.push(UndoRecord {
            target,
            op,
            cursor_before,
            cursor_after,
            sequence,
        });
    }

    /// Undo the last edit on a scroll. Returns the inverse operation
    /// and the cursor position to restore.
    pub fn undo(&mut self, target: &Coordinate) -> Option<(EditOp, Position)> {
        let stack = self.stacks.get_mut(target)?;
        let record = stack.undo()?;
        Some((record.op.inverse(), record.cursor_before))
    }

    /// Redo the next edit on a scroll.
    pub fn redo(&mut self, target: &Coordinate) -> Option<(EditOp, Position)> {
        let stack = self.stacks.get_mut(target)?;
        let record = stack.redo()?;
        Some((record.op.clone(), record.cursor_after))
    }

    /// Can we undo on this scroll?
    pub fn can_undo(&self, target: &Coordinate) -> bool {
        self.stacks.get(target).map(|s| s.can_undo()).unwrap_or(false)
    }

    /// Can we redo on this scroll?
    pub fn can_redo(&self, target: &Coordinate) -> bool {
        self.stacks.get(target).map(|s| s.can_redo()).unwrap_or(false)
    }

    /// Undo depth for a scroll.
    pub fn depth(&self, target: &Coordinate) -> usize {
        self.stacks.get(target).map(|s| s.depth()).unwrap_or(0)
    }

    /// Total edits recorded globally.
    pub fn total_edits(&self) -> usize {
        self.global_sequence
    }

    /// Which scrolls have been modified.
    pub fn dirty_scrolls(&self) -> Vec<Coordinate> {
        self.stacks.iter()
            .filter(|(_, stack)| stack.depth() > 0)
            .map(|(coord, _)| *coord)
            .collect()
    }

    /// Serialize the full undo history to a phext byte vector.
    ///
    /// Each undo record becomes a scroll in the work phext.
    /// Coordinate mapping:
    ///   Z arm = target scroll's Z arm
    ///   Y arm = target scroll's Y arm
    ///   X arm = chapter = file-wide serialization sequence (records
    ///   are emitted in target-coordinate order, then per-target
    ///   sequence order), section = 1, scroll = 1
    ///
    /// Each scroll begins with a `target:` header line carrying the
    /// full target coordinate; see the module docs for why.
    pub fn to_work_phext(&self) -> Vec<u8> {
        let mut records: Vec<(&Coordinate, &UndoRecord)> = Vec::new();
        for (target, stack) in &self.stacks {
            for record in &stack.records {
                records.push((target, record));
            }
        }
        records.sort_by(|(ta, ra), (tb, rb)| {
            ta.cmp(tb).then(ra.sequence.cmp(&rb.sequence))
        });

        let mut entries: Vec<(Coordinate, String)> = Vec::new();
        for (i, (target, record)) in records.iter().enumerate() {
            let work_coord = work_coordinate(target, i + 1);
            let scroll = format!(
                "target:{}\n{}",
                target,
                record.op.to_scroll(record.cursor_before, record.cursor_after)
            );
            entries.push((work_coord, scroll));
        }

        // Sort by coordinate for deterministic output
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));

        // Build phext bytes
        let mut bytes = Vec::new();
        let mut first = true;
        let mut current = default_coordinate();

        for (coord, content) in entries {
            if !first {
                // Emit delimiters to advance from current to coord
                let delims = delimiters_between(&current, &coord);
                bytes.extend_from_slice(&delims);
            }
            bytes.extend_from_slice(content.as_bytes());
            current = coord;
            first = false;
        }

        bytes
    }

    /// Load undo history from a work phext.
    pub fn from_work_phext(bytes: &[u8], _target_index: &crate::index::LatticeIndex) -> UndoEngine {
        let work_index = crate::index::LatticeIndex::build(bytes);
        let mut engine = UndoEngine::new();

        for &coord in work_index.coordinates() {
            if let Some(span) = work_index.get(&coord) {
                let scroll = std::str::from_utf8(&bytes[span.start..span.end]).unwrap_or("");
                // New-format scrolls carry the full target coordinate
                // in a `target:` header line. Legacy work phexts (no
                // header) fall back to the coordinate-derived target,
                // whose X arm resets to 1.1.1 — the pre-fix behavior.
                let (target, op_scroll) = match scroll.strip_prefix("target:") {
                    Some(rest) => match rest.split_once('\n') {
                        Some((addr, body)) => (to_coordinate(addr), body),
                        None => (source_coordinate(&coord), scroll),
                    },
                    None => (source_coordinate(&coord), scroll),
                };
                if let Some((op, cursor_before, cursor_after)) = EditOp::from_scroll(op_scroll) {
                    // Per-target sequence, recomputed exactly as
                    // `record` computes it (the work coordinate's
                    // chapter is a file-wide serialization sequence).
                    let sequence = engine.stacks.get(&target).map(|s| s.total()).unwrap_or(0) + 1;
                    engine.global_sequence += 1;
                    let stack = engine.stacks.entry(target).or_insert_with(ScrollHistory::new);
                    stack.push(UndoRecord {
                        target,
                        op,
                        cursor_before,
                        cursor_after,
                        sequence,
                    });
                }
            }
        }

        engine
    }
}

/// Map a source scroll coordinate + edit sequence to a work file coordinate.
fn work_coordinate(source: &Coordinate, sequence: usize) -> Coordinate {
    let mut c = *source;
    c.x.chapter = sequence;
    c.x.section = 1;
    c.x.scroll = 1;
    c
}

/// Recover the source scroll coordinate from a work file coordinate.
fn source_coordinate(work: &Coordinate) -> Coordinate {
    let mut c = *work;
    c.x.chapter = 1;
    c.x.section = 1;
    c.x.scroll = 1;
    c
}

/// Compute the delimiter bytes needed to advance from one coordinate to another.
/// This is a simplified version — only handles advancing (not backward).
fn delimiters_between(from: &Coordinate, to: &Coordinate) -> Vec<u8> {
    // Check each dimension from highest to lowest
    if from.z.library != to.z.library { return vec![0x01]; }
    if from.z.shelf != to.z.shelf { return vec![0x1F]; }
    if from.z.series != to.z.series { return vec![0x1E]; }
    if from.y.collection != to.y.collection { return vec![0x1D]; }
    if from.y.volume != to.y.volume { return vec![0x1C]; }
    if from.y.book != to.y.book { return vec![0x1A]; }
    if from.x.chapter != to.x.chapter { return vec![0x19]; }
    if from.x.section != to.x.section { return vec![0x18]; }
    if from.x.scroll != to.x.scroll { return vec![0x17]; }
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;
    use libphext::phext::to_coordinate;

    #[test]
    fn edit_op_insert_apply() {
        let op = EditOp::Insert { offset: 5, text: " beautiful".to_string() };
        let result = op.apply("Hello World");
        assert_eq!(result, "Hello beautiful World");
    }

    #[test]
    fn edit_op_delete_apply() {
        let op = EditOp::Delete { offset: 5, deleted: " World".to_string() };
        let result = op.apply("Hello World");
        assert_eq!(result, "Hello");
    }

    #[test]
    fn edit_op_replace_apply() {
        let op = EditOp::Replace {
            offset: 6,
            old_text: "World".to_string(),
            new_text: "Lattice".to_string(),
        };
        let result = op.apply("Hello World");
        assert_eq!(result, "Hello Lattice");
    }

    #[test]
    fn edit_op_apply_non_char_boundary_offset() {
        // STITCH++ probe:c1 — byte 3 is inside the 3-byte char in "-\u{fffd}"
        // (bytes 1..4). Every op kind must tolerate it without panicking.
        let text = "-\u{fffd}";
        let replace = EditOp::Replace {
            offset: 3,
            old_text: String::new(),
            new_text: String::new(),
        };
        assert_eq!(replace.apply(text), text);
        let insert = EditOp::Insert { offset: 3, text: "x".to_string() };
        assert_eq!(insert.apply(text), "-x\u{fffd}");
        // Offset 2 snaps down to the char's start (1); the range end
        // (1 + 3 bytes) is the char's end, so the char is deleted.
        let delete = EditOp::Delete { offset: 2, deleted: "\u{fffd}".to_string() };
        assert_eq!(delete.apply(text), "-");
        // Round trip on multi-byte text with a boundary-aligned
        // offset is exact, as before.
        let op = EditOp::Insert { offset: 1, text: "xy".to_string() };
        let applied = op.apply(text);
        assert_eq!(applied, "-xy\u{fffd}");
        assert_eq!(op.inverse().apply(&applied), text);
    }

    #[test]
    fn edit_op_inverse() {
        let op = EditOp::Insert { offset: 5, text: "xyz".to_string() };
        let inv = op.inverse();
        let text = op.apply("Hello");
        let restored = inv.apply(&text);
        assert_eq!(restored, "Hello");
    }

    #[test]
    fn edit_op_serialize_roundtrip() {
        let op = EditOp::Insert { offset: 42, text: "hello world".to_string() };
        let before = Position::new(2, 6);
        let after = Position::new(2, 17);
        let scroll = op.to_scroll(before, after);
        let (recovered_op, recovered_before, recovered_after) = EditOp::from_scroll(&scroll).unwrap();

        assert_eq!(recovered_before, before);
        assert_eq!(recovered_after, after);
        match recovered_op {
            EditOp::Insert { offset, text } => {
                assert_eq!(offset, 42);
                assert_eq!(text, "hello world");
            }
            _ => panic!("wrong op type"),
        }
    }

    #[test]
    fn undo_redo_basic() {
        let mut engine = UndoEngine::new();
        let target = to_coordinate("1.1.1/1.1.1/1.1.1");
        let mut text = "Hello World".to_string();

        // Insert
        let op = EditOp::Insert { offset: 5, text: " Beautiful".to_string() };
        let new_text = op.apply(&text);
        engine.record(target, op, Position::new(0, 5), Position::new(0, 15));
        text = new_text;
        assert_eq!(text, "Hello Beautiful World");

        // Undo
        let (undo_op, cursor) = engine.undo(&target).unwrap();
        text = undo_op.apply(&text);
        assert_eq!(text, "Hello World");
        assert_eq!(cursor, Position::new(0, 5));

        // Redo
        let (redo_op, cursor) = engine.redo(&target).unwrap();
        text = redo_op.apply(&text);
        assert_eq!(text, "Hello Beautiful World");
        assert_eq!(cursor, Position::new(0, 15));
    }

    #[test]
    fn undo_truncates_redo() {
        let mut engine = UndoEngine::new();
        let target = to_coordinate("1.1.1/1.1.1/1.1.1");

        // Two edits
        engine.record(target, EditOp::Insert { offset: 0, text: "A".to_string() },
            Position::origin(), Position::new(0, 1));
        engine.record(target, EditOp::Insert { offset: 1, text: "B".to_string() },
            Position::new(0, 1), Position::new(0, 2));

        // Undo one
        engine.undo(&target);

        // New edit should truncate redo
        engine.record(target, EditOp::Insert { offset: 1, text: "C".to_string() },
            Position::new(0, 1), Position::new(0, 2));

        assert!(!engine.can_redo(&target));
        assert_eq!(engine.depth(&target), 2); // A + C
    }

    #[test]
    fn work_phext_roundtrip() {
        let mut engine = UndoEngine::new();
        let t1 = to_coordinate("1.1.1/1.1.1/1.1.1");
        let t2 = to_coordinate("3.3.3/5.1.2/1.5.2");

        engine.record(t1, EditOp::Insert { offset: 0, text: "Hello".to_string() },
            Position::origin(), Position::new(0, 5));
        engine.record(t1, EditOp::Delete { offset: 0, deleted: "He".to_string() },
            Position::new(0, 0), Position::new(0, 0));
        engine.record(t2, EditOp::Insert { offset: 0, text: "World".to_string() },
            Position::origin(), Position::new(0, 5));

        let work_bytes = engine.to_work_phext();
        assert!(!work_bytes.is_empty());

        // Verify it's valid phext
        let work_index = crate::index::LatticeIndex::build(&work_bytes);
        assert_eq!(work_index.scroll_count(), 3); // 3 edits

        // Load back
        let source_index = crate::index::LatticeIndex::build(b"dummy");
        let loaded = UndoEngine::from_work_phext(&work_bytes, &source_index);
        assert_eq!(loaded.dirty_scrolls().len(), 2); // two target scrolls
    }

    #[test]
    fn work_phext_roundtrip_preserves_full_targets() {
        // STITCH++ main:c9 — two targets sharing Z/Y arms and a
        // per-target sequence number, differing only in X scroll.
        // The pre-fix encoding wrote both to one work coordinate and
        // merged them (total_edits 2 -> 1), and reset every reloaded
        // target's X arm to 1.1.1.
        let mut engine = UndoEngine::new();
        let t1 = to_coordinate("54.15.54/55.75.1/99.90.16");
        let t2 = to_coordinate("54.15.54/55.75.1/99.90.15");

        engine.record(t1, EditOp::Insert { offset: 0, text: "alpha".to_string() },
            Position::origin(), Position::origin());
        engine.record(t2, EditOp::Insert { offset: 0, text: "alpha".to_string() },
            Position::origin(), Position::origin());
        assert_eq!(engine.total_edits(), 2);

        let work_bytes = engine.to_work_phext();
        let source_index = crate::index::LatticeIndex::build(b"dummy");
        let mut loaded = UndoEngine::from_work_phext(&work_bytes, &source_index);

        // No record lost to a work-coordinate collision...
        assert_eq!(loaded.total_edits(), 2);
        // ...and both targets return with full coordinates intact.
        let dirty = loaded.dirty_scrolls();
        assert_eq!(dirty.len(), 2);
        assert!(dirty.contains(&t1));
        assert!(dirty.contains(&t2));
        assert!(loaded.undo(&t1).is_some());
        assert!(loaded.undo(&t2).is_some());
    }

    #[test]
    fn work_phext_legacy_format_still_loads() {
        // A pre-fix work phext: a bare op scroll (no `target:` header)
        // at the legacy work coordinate. It must still load, with the
        // target recovered from the work coordinate (X arm 1.1.1).
        let target = to_coordinate("1.1.1/1.1.1/1.1.5");
        let op = EditOp::Insert { offset: 0, text: "World".to_string() };
        let scroll = op.to_scroll(Position::origin(), Position::new(0, 5));
        let work_coord = work_coordinate(&target, 1);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&delimiters_between(&default_coordinate(), &work_coord));
        bytes.extend_from_slice(scroll.as_bytes());

        let source_index = crate::index::LatticeIndex::build(b"dummy");
        let loaded = UndoEngine::from_work_phext(&bytes, &source_index);
        assert_eq!(loaded.total_edits(), 1);
        assert_eq!(loaded.dirty_scrolls(), vec![to_coordinate("1.1.1/1.1.1/1.1.1")]);
    }

    #[test]
    fn dirty_scrolls() {
        let mut engine = UndoEngine::new();
        let t1 = to_coordinate("1.1.1/1.1.1/1.1.1");

        assert!(engine.dirty_scrolls().is_empty());

        engine.record(t1, EditOp::Insert { offset: 0, text: "x".to_string() },
            Position::origin(), Position::new(0, 1));
        assert_eq!(engine.dirty_scrolls().len(), 1);

        // Undo — still dirty (record exists, just rewound)
        engine.undo(&t1);
        // depth is 0 now, so not dirty
        assert!(engine.dirty_scrolls().is_empty());
    }
}
