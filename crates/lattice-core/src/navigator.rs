/// Lattice Navigator — the cursor model for 9D phext navigation.
///
/// This is the "mind" of the editor's Lattice mode. The navigator tracks
/// the current coordinate, active dimension, and provides the movement
/// primitives that map to keystrokes.

use libphext::phext::{Coordinate, default_coordinate};
use crate::coordinate_ext::{Dimension, CoordinateNav};
use crate::index::LatticeIndex;

/// The navigator state for one cursor in the lattice.
#[derive(Debug)]
pub struct Navigator {
    /// Current coordinate position.
    position: Coordinate,
    /// Which dimension is currently selected for h/l navigation.
    active_dimension: Dimension,
    /// Stack of marks for quick-jump (`m` to mark, `'` to return).
    marks: Vec<Coordinate>,
    /// History for back/forward navigation.
    history: Vec<Coordinate>,
    /// Current position in history (for forward after back).
    history_cursor: usize,
}

impl Navigator {
    pub fn new() -> Navigator {
        Navigator {
            position: default_coordinate(),
            active_dimension: Dimension::Scroll,
            marks: Vec::new(),
            history: vec![default_coordinate()],
            history_cursor: 0,
        }
    }

    /// Create a navigator starting at a specific coordinate.
    pub fn at(coord: Coordinate) -> Navigator {
        Navigator {
            position: coord,
            active_dimension: Dimension::Scroll,
            marks: Vec::new(),
            history: vec![coord],
            history_cursor: 0,
        }
    }

    /// Current position in the lattice.
    pub fn position(&self) -> Coordinate {
        self.position
    }

    /// Currently selected dimension.
    pub fn active_dimension(&self) -> Dimension {
        self.active_dimension
    }

    /// Select a dimension by index (1-9).
    pub fn select_dimension(&mut self, index: u8) -> bool {
        if let Some(dim) = Dimension::from_index(index) {
            self.active_dimension = dim;
            true
        } else {
            false
        }
    }

    /// Move forward in the active dimension (resets lower dims).
    pub fn move_forward(&mut self) -> bool {
        let moved = self.position.navigate_forward(self.active_dimension);
        if moved {
            self.push_history();
        }
        moved
    }

    /// Move backward in the active dimension (resets lower dims).
    pub fn move_backward(&mut self) -> bool {
        let moved = self.position.navigate_backward(self.active_dimension);
        if moved {
            self.push_history();
        }
        moved
    }

    /// Jump directly to a coordinate.
    pub fn goto(&mut self, coord: Coordinate) {
        self.position = coord;
        self.push_history();
    }

    /// Jump to the next populated coordinate (any dimension).
    pub fn next_populated(&mut self, index: &LatticeIndex) -> bool {
        if let Some(next) = index.next_populated(&self.position) {
            self.position = next;
            self.push_history();
            true
        } else {
            false
        }
    }

    /// Jump to the previous populated coordinate (any dimension).
    pub fn prev_populated(&mut self, index: &LatticeIndex) -> bool {
        if let Some(prev) = index.prev_populated(&self.position) {
            self.position = prev;
            self.push_history();
            true
        } else {
            false
        }
    }

    /// Set a mark at the current position.
    pub fn set_mark(&mut self) {
        self.marks.push(self.position);
    }

    /// Jump to the most recent mark (and remove it).
    pub fn jump_to_mark(&mut self) -> bool {
        if let Some(mark) = self.marks.pop() {
            self.position = mark;
            self.push_history();
            true
        } else {
            false
        }
    }

    /// Navigate back in history.
    pub fn history_back(&mut self) -> bool {
        if self.history_cursor > 0 {
            self.history_cursor -= 1;
            self.position = self.history[self.history_cursor];
            true
        } else {
            false
        }
    }

    /// Navigate forward in history.
    pub fn history_forward(&mut self) -> bool {
        if self.history_cursor + 1 < self.history.len() {
            self.history_cursor += 1;
            self.position = self.history[self.history_cursor];
            true
        } else {
            false
        }
    }

    /// Get neighbors in the active dimension: (prev_value, current_value, next_value).
    /// Values are None if at the boundary.
    pub fn dimension_neighbors(&self) -> (Option<usize>, usize, Option<usize>) {
        let current = self.position.dimension_value(self.active_dimension);
        let prev = if current > 1 { Some(current - 1) } else { None };
        let next = if current < libphext::phext::COORDINATE_MAXIMUM { Some(current + 1) } else { None };
        (prev, current, next)
    }

    fn push_history(&mut self) {
        // Truncate forward history if we've navigated back
        self.history.truncate(self.history_cursor + 1);
        self.history.push(self.position);
        self.history_cursor = self.history.len() - 1;

        // Cap history at a reasonable size
        if self.history.len() > 1000 {
            let drain = self.history.len() - 500;
            self.history.drain(..drain);
            self.history_cursor = self.history.len() - 1;
        }
    }
}

/// Summary of the lattice neighborhood around a coordinate,
/// for rendering the coordinate bar and lattice navigator panel.
#[derive(Debug)]
pub struct NeighborhoodSummary {
    /// Current coordinate.
    pub position: Coordinate,
    /// Active dimension name.
    pub dimension_name: &'static str,
    /// Current value in the active dimension.
    pub dimension_value: usize,
    /// Total populated scrolls in the lattice.
    pub total_scrolls: usize,
    /// Whether there's content at the current position.
    pub has_content: bool,
    /// Next populated coordinate (if any).
    pub next: Option<Coordinate>,
    /// Previous populated coordinate (if any).
    pub prev: Option<Coordinate>,
}

impl Navigator {
    /// Build a summary of the current neighborhood for UI rendering.
    pub fn summarize(&self, index: &LatticeIndex) -> NeighborhoodSummary {
        NeighborhoodSummary {
            position: self.position,
            dimension_name: self.active_dimension.name(),
            dimension_value: self.position.dimension_value(self.active_dimension),
            total_scrolls: index.scroll_count(),
            has_content: index.contains(&self.position),
            next: index.next_populated(&self.position),
            prev: index.prev_populated(&self.position),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libphext::phext::to_coordinate;

    #[test]
    fn basic_navigation() {
        let mut nav = Navigator::new();
        assert_eq!(nav.position(), default_coordinate());

        // Select dimension 4 (Book) and move forward
        nav.select_dimension(4);
        assert_eq!(nav.active_dimension(), Dimension::Book);
        assert!(nav.move_forward());
        assert_eq!(nav.position().y.book, 2);
        // Lower dims should reset
        assert_eq!(nav.position().x.chapter, 1);
    }

    #[test]
    fn goto_and_history() {
        let mut nav = Navigator::new();
        let target = to_coordinate("3.3.3/5.5.5/7.7.7");
        nav.goto(target);
        assert_eq!(nav.position(), target);

        // Should be able to go back
        assert!(nav.history_back());
        assert_eq!(nav.position(), default_coordinate());

        // And forward again
        assert!(nav.history_forward());
        assert_eq!(nav.position(), target);
    }

    #[test]
    fn marks() {
        let mut nav = Navigator::new();
        let pos1 = to_coordinate("2.2.2/2.2.2/2.2.2");
        nav.goto(pos1);
        nav.set_mark();

        let pos2 = to_coordinate("5.5.5/5.5.5/5.5.5");
        nav.goto(pos2);

        assert!(nav.jump_to_mark());
        assert_eq!(nav.position(), pos1);
    }

    #[test]
    fn populated_navigation() {
        // Build a test index
        let mut buf = Vec::new();
        buf.extend_from_slice(b"first");
        buf.push(0x17);
        buf.extend_from_slice(b"second");
        buf.push(0x18);
        buf.extend_from_slice(b"third");
        let index = LatticeIndex::build(&buf);

        let mut nav = Navigator::new();
        assert!(nav.next_populated(&index));
        // Should jump to 1.1.1/1.1.1/1.1.2 (past BASE which is 1.1.1/1.1.1/1.1.1)
        // Actually BASE is already there, so next should be 1.1.2
        // Wait - we start at BASE and next_populated looks for the next AFTER current
        assert_eq!(nav.position(), to_coordinate("1.1.1/1.1.1/1.1.2"));

        assert!(nav.next_populated(&index));
        assert_eq!(nav.position(), to_coordinate("1.1.1/1.1.1/1.2.1"));

        // No more
        assert!(!nav.next_populated(&index));

        // Go back
        assert!(nav.prev_populated(&index));
        assert_eq!(nav.position(), to_coordinate("1.1.1/1.1.1/1.1.2"));
    }
}
