/// Coordinate extensions for editor navigation.
///
/// libphext-rs provides Coordinate with break methods (advance + reset lower dims)
/// but lacks:
///   - Hash/Eq (needed for HashMap-based index)
///   - Dimensional navigation (move forward/backward in a named dimension)
///   - Coordinate arithmetic without the "reset lower dims" semantics
///
/// These are additions the editor needs. Improvements that should eventually
/// migrate upstream to libphext-rs are marked with [UPSTREAM].

use libphext::phext::{Coordinate, YCoordinate, XCoordinate};
use libphext::phext::{COORDINATE_MINIMUM, COORDINATE_MAXIMUM};

// NOTE: Hash + Eq are now derived upstream in libphext-rs on
// ZCoordinate, YCoordinate, XCoordinate, and Coordinate.
// This was the first suggested upstream improvement from phext-lattice.

/// --------------------------------------------------------------------------------------------------------
/// Dimension enum for editor navigation.
/// Maps 1-9 keys to the nine phext dimensions (scroll=innermost, library=outermost).
/// --------------------------------------------------------------------------------------------------------
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Dimension {
    Scroll = 1,
    Section = 2,
    Chapter = 3,
    Book = 4,
    Volume = 5,
    Collection = 6,
    Series = 7,
    Shelf = 8,
    Library = 9,
}

impl Dimension {
    /// Convert 1-9 key index to dimension. Returns None for out of range.
    pub fn from_index(i: u8) -> Option<Dimension> {
        match i {
            1 => Some(Dimension::Scroll),
            2 => Some(Dimension::Section),
            3 => Some(Dimension::Chapter),
            4 => Some(Dimension::Book),
            5 => Some(Dimension::Volume),
            6 => Some(Dimension::Collection),
            7 => Some(Dimension::Series),
            8 => Some(Dimension::Shelf),
            9 => Some(Dimension::Library),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Dimension::Scroll => "Scroll",
            Dimension::Section => "Section",
            Dimension::Chapter => "Chapter",
            Dimension::Book => "Book",
            Dimension::Volume => "Volume",
            Dimension::Collection => "Collection",
            Dimension::Series => "Series",
            Dimension::Shelf => "Shelf",
            Dimension::Library => "Library",
        }
    }

    /// The delimiter byte for this dimension.
    pub fn delimiter(&self) -> u8 {
        match self {
            Dimension::Scroll     => 0x17,
            Dimension::Section    => 0x18,
            Dimension::Chapter    => 0x19,
            Dimension::Book       => 0x1A,
            Dimension::Volume     => 0x1C,
            Dimension::Collection => 0x1D,
            Dimension::Series     => 0x1E,
            Dimension::Shelf      => 0x1F,
            Dimension::Library    => 0x01,
        }
    }
}

/// --------------------------------------------------------------------------------------------------------
/// [UPSTREAM] Navigation trait: move forward/backward in a specific dimension.
///
/// The existing break methods (scroll_break, section_break, etc.) advance AND reset
/// lower dimensions. That's correct for parsing delimiters, but an editor also needs
/// "move to the next book without resetting chapter/section/scroll" for lattice
/// navigation, AND "move backward" which libphext doesn't support at all.
///
/// `navigate` moves in a dimension, resetting lower dims (like the delimiter does).
/// `peek` moves in a dimension WITHOUT resetting lower dims (for UI preview).
/// --------------------------------------------------------------------------------------------------------
pub trait CoordinateNav {
    /// Move forward in the given dimension, resetting lower dimensions.
    /// Returns false if already at COORDINATE_MAXIMUM.
    fn navigate_forward(&mut self, dim: Dimension) -> bool;

    /// Move backward in the given dimension, resetting lower dimensions to 1.
    /// Returns false if already at COORDINATE_MINIMUM.
    fn navigate_backward(&mut self, dim: Dimension) -> bool;

    /// Get the value of a specific dimension.
    fn dimension_value(&self, dim: Dimension) -> usize;

    /// Set the value of a specific dimension (clamped to valid range).
    /// Does NOT reset lower dimensions — caller is responsible.
    fn set_dimension(&mut self, dim: Dimension, value: usize);

    /// Create a coordinate with one dimension changed, lower dims reset to 1.
    /// Non-mutating version of navigate.
    fn with_dimension(&self, dim: Dimension, value: usize) -> Self;
}

impl CoordinateNav for Coordinate {
    fn navigate_forward(&mut self, dim: Dimension) -> bool {
        let current = self.dimension_value(dim);
        if current >= COORDINATE_MAXIMUM {
            return false;
        }
        // Use the built-in break methods which handle the reset semantics
        match dim {
            Dimension::Scroll     => self.scroll_break(),
            Dimension::Section    => self.section_break(),
            Dimension::Chapter    => self.chapter_break(),
            Dimension::Book       => self.book_break(),
            Dimension::Volume     => self.volume_break(),
            Dimension::Collection => self.collection_break(),
            Dimension::Series     => self.series_break(),
            Dimension::Shelf      => self.shelf_break(),
            Dimension::Library    => self.library_break(),
        }
        true
    }

    fn navigate_backward(&mut self, dim: Dimension) -> bool {
        let current = self.dimension_value(dim);
        if current <= COORDINATE_MINIMUM {
            return false;
        }
        // Set the dimension to current - 1, reset all lower dims to 1
        let new_val = current - 1;
        self.set_dimension(dim, new_val);
        // Reset lower dimensions
        match dim {
            Dimension::Library => {
                self.z.shelf = 1; self.z.series = 1;
                self.y.collection = 1; self.y.volume = 1; self.y.book = 1;
                self.x.chapter = 1; self.x.section = 1; self.x.scroll = 1;
            }
            Dimension::Shelf => {
                self.z.series = 1;
                self.y.collection = 1; self.y.volume = 1; self.y.book = 1;
                self.x.chapter = 1; self.x.section = 1; self.x.scroll = 1;
            }
            Dimension::Series => {
                self.y.collection = 1; self.y.volume = 1; self.y.book = 1;
                self.x.chapter = 1; self.x.section = 1; self.x.scroll = 1;
            }
            Dimension::Collection => {
                self.y.volume = 1; self.y.book = 1;
                self.x.chapter = 1; self.x.section = 1; self.x.scroll = 1;
            }
            Dimension::Volume => {
                self.y.book = 1;
                self.x.chapter = 1; self.x.section = 1; self.x.scroll = 1;
            }
            Dimension::Book => {
                self.x.chapter = 1; self.x.section = 1; self.x.scroll = 1;
            }
            Dimension::Chapter => {
                self.x.section = 1; self.x.scroll = 1;
            }
            Dimension::Section => {
                self.x.scroll = 1;
            }
            Dimension::Scroll => {
                // No lower dimensions to reset
            }
        }
        true
    }

    fn dimension_value(&self, dim: Dimension) -> usize {
        match dim {
            Dimension::Scroll     => self.x.scroll,
            Dimension::Section    => self.x.section,
            Dimension::Chapter    => self.x.chapter,
            Dimension::Book       => self.y.book,
            Dimension::Volume     => self.y.volume,
            Dimension::Collection => self.y.collection,
            Dimension::Series     => self.z.series,
            Dimension::Shelf      => self.z.shelf,
            Dimension::Library    => self.z.library,
        }
    }

    fn set_dimension(&mut self, dim: Dimension, value: usize) {
        let clamped = value.clamp(COORDINATE_MINIMUM, COORDINATE_MAXIMUM);
        match dim {
            Dimension::Scroll     => self.x.scroll = clamped,
            Dimension::Section    => self.x.section = clamped,
            Dimension::Chapter    => self.x.chapter = clamped,
            Dimension::Book       => self.y.book = clamped,
            Dimension::Volume     => self.y.volume = clamped,
            Dimension::Collection => self.y.collection = clamped,
            Dimension::Series     => self.z.series = clamped,
            Dimension::Shelf      => self.z.shelf = clamped,
            Dimension::Library    => self.z.library = clamped,
        }
    }

    fn with_dimension(&self, dim: Dimension, value: usize) -> Coordinate {
        let mut result = *self;
        result.set_dimension(dim, value);
        // Reset lower dimensions to 1
        match dim {
            Dimension::Library => {
                result.z.shelf = 1; result.z.series = 1;
                result.y = YCoordinate::new(); result.x = XCoordinate::new();
            }
            Dimension::Shelf => {
                result.z.series = 1;
                result.y = YCoordinate::new(); result.x = XCoordinate::new();
            }
            Dimension::Series => {
                result.y = YCoordinate::new(); result.x = XCoordinate::new();
            }
            Dimension::Collection => {
                result.y.volume = 1; result.y.book = 1;
                result.x = XCoordinate::new();
            }
            Dimension::Volume => {
                result.y.book = 1; result.x = XCoordinate::new();
            }
            Dimension::Book => {
                result.x = XCoordinate::new();
            }
            Dimension::Chapter => {
                result.x.section = 1; result.x.scroll = 1;
            }
            Dimension::Section => {
                result.x.scroll = 1;
            }
            Dimension::Scroll => {}
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libphext::phext::to_coordinate;
    use std::collections::HashMap;

    #[test]
    fn coordinate_is_hashable() {
        let mut map: HashMap<Coordinate, usize> = HashMap::new();
        let c = to_coordinate("2.7.1/8.2.8/4.5.9");
        map.insert(c, 42);
        assert_eq!(map.get(&c), Some(&42));
    }

    #[test]
    fn navigate_forward_and_back() {
        let mut c = to_coordinate("1.1.1/1.1.1/1.1.3");
        assert!(c.navigate_forward(Dimension::Scroll));
        assert_eq!(c.x.scroll, 4);
        assert!(c.navigate_backward(Dimension::Scroll));
        assert_eq!(c.x.scroll, 3);
    }

    #[test]
    fn navigate_backward_resets_lower() {
        let mut c = to_coordinate("1.1.1/1.1.1/3.5.7");
        c.navigate_backward(Dimension::Chapter);
        assert_eq!(c.x.chapter, 2);
        assert_eq!(c.x.section, 1);
        assert_eq!(c.x.scroll, 1);
    }

    #[test]
    fn navigate_at_minimum_returns_false() {
        let mut c = to_coordinate("1.1.1/1.1.1/1.1.1");
        assert!(!c.navigate_backward(Dimension::Library));
    }

    #[test]
    fn dimension_from_index() {
        assert_eq!(Dimension::from_index(1), Some(Dimension::Scroll));
        assert_eq!(Dimension::from_index(9), Some(Dimension::Library));
        assert_eq!(Dimension::from_index(0), None);
        assert_eq!(Dimension::from_index(10), None);
    }

    #[test]
    fn with_dimension_resets_lower() {
        let c = to_coordinate("3.3.3/5.5.5/7.7.7");
        let moved = c.with_dimension(Dimension::Volume, 2);
        assert_eq!(moved.y.volume, 2);
        assert_eq!(moved.y.book, 1);
        assert_eq!(moved.x.chapter, 1);
        // Higher dims preserved
        assert_eq!(moved.y.collection, 5);
        assert_eq!(moved.z.series, 3);
    }
}
