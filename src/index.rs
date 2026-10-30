/// Lattice Index — sparse coordinate → byte-offset map built from a phext buffer.
///
/// The core data structure for the editor. Instead of libphext's linear scan
/// (get_subspace_coordinates is O(n) on every fetch), we do ONE scan on load
/// to build a sorted span table, then binary-search it (O(log n), ~17 probes
/// for 135k scrolls). Sorted arrays also load straight from a persisted span
/// table (see `sidecar`) without hashing, and take 88 bytes per scroll.
///
/// For memory-mapped files, ScrollSpan stores byte offsets into the mmap,
/// so we never copy scroll content until the user navigates to it.

use libphext::phext::{Coordinate, default_coordinate};

/// A scroll's location within the raw phext byte buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollSpan {
    /// Byte offset where this scroll's content begins (inclusive).
    pub start: usize,
    /// Byte offset where this scroll's content ends (exclusive).
    pub end: usize,
}

impl ScrollSpan {
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// The lattice index: maps every populated coordinate to its byte span.
#[derive(Debug, Clone)]
pub struct LatticeIndex {
    /// All populated coordinates in document order (= sorted order).
    ordered: Vec<Coordinate>,
    /// `spans[i]` is the byte span of `ordered[i]`.
    spans: Vec<ScrollSpan>,
    /// Total size of the indexed buffer in bytes.
    buffer_len: usize,
}

impl LatticeIndex {
    /// Build an index by scanning the raw phext bytes once.
    ///
    /// Time: O(n) where n = buffer length.
    /// Space: O(k) where k = number of populated scrolls.
    pub fn build(buffer: &[u8]) -> LatticeIndex {
        let mut entries: Vec<(Coordinate, ScrollSpan)> = Vec::new();
        let mut coord = default_coordinate();
        let mut scroll_start: usize = 0;
        let len = buffer.len();

        for i in 0..len {
            let byte = buffer[i];
            let dim_break = match byte as char {
                '\x17' => Some(DimBreak::Scroll),
                '\x18' => Some(DimBreak::Section),
                '\x19' => Some(DimBreak::Chapter),
                '\x1A' => Some(DimBreak::Book),
                '\x1C' => Some(DimBreak::Volume),
                '\x1D' => Some(DimBreak::Collection),
                '\x1E' => Some(DimBreak::Series),
                '\x1F' => Some(DimBreak::Shelf),
                '\x01' => Some(DimBreak::Library),
                _ => None,
            };

            if let Some(brk) = dim_break {
                // Commit the current scroll span
                let span = ScrollSpan { start: scroll_start, end: i };
                if !span.is_empty() {
                    entries.push((coord, span));
                }

                // Advance coordinate
                match brk {
                    DimBreak::Scroll     => coord.scroll_break(),
                    DimBreak::Section    => coord.section_break(),
                    DimBreak::Chapter    => coord.chapter_break(),
                    DimBreak::Book       => coord.book_break(),
                    DimBreak::Volume     => coord.volume_break(),
                    DimBreak::Collection => coord.collection_break(),
                    DimBreak::Series     => coord.series_break(),
                    DimBreak::Shelf      => coord.shelf_break(),
                    DimBreak::Library    => coord.library_break(),
                }

                scroll_start = i + 1;
            }
        }

        // Commit the final scroll
        if scroll_start < len {
            let span = ScrollSpan { start: scroll_start, end: len };
            // Only record if non-empty content
            if !span.is_empty() {
                entries.push((coord, span));
            }
        }

        LatticeIndex::from_entries(entries, len)
    }

    /// Build from (coordinate, span) pairs, e.g. a persisted span table or spans
    /// recorded while serializing. Pairs are normally already in document order;
    /// otherwise they are sorted. A repeated coordinate keeps its last span.
    pub fn from_entries(mut entries: Vec<(Coordinate, ScrollSpan)>, buffer_len: usize) -> LatticeIndex {
        if !entries.windows(2).all(|w| w[0].0 < w[1].0) {
            entries.sort_by(|a, b| a.0.cmp(&b.0)); // stable: duplicates stay in input order
            let mut deduped: Vec<(Coordinate, ScrollSpan)> = Vec::with_capacity(entries.len());
            for e in entries {
                match deduped.last_mut() {
                    Some(last) if last.0 == e.0 => *last = e,
                    _ => deduped.push(e),
                }
            }
            entries = deduped;
        }
        let (ordered, spans) = entries.into_iter().unzip();
        LatticeIndex { ordered, spans, buffer_len }
    }

    /// Look up the byte span for a coordinate. O(log n).
    pub fn get(&self, coord: &Coordinate) -> Option<&ScrollSpan> {
        self.ordered.binary_search(coord).ok().map(|i| &self.spans[i])
    }

    /// Check if a coordinate has content.
    pub fn contains(&self, coord: &Coordinate) -> bool {
        self.ordered.binary_search(coord).is_ok()
    }

    /// Number of populated scrolls.
    pub fn scroll_count(&self) -> usize {
        self.ordered.len()
    }

    /// Spans in document order, parallel to `coordinates()`.
    pub fn spans(&self) -> &[ScrollSpan] {
        &self.spans
    }

    /// Total buffer size.
    pub fn buffer_len(&self) -> usize {
        self.buffer_len
    }

    /// Iterator over all populated coordinates in document order.
    pub fn coordinates(&self) -> &[Coordinate] {
        &self.ordered
    }

    /// Find the next populated coordinate after `coord` in document order.
    /// Returns None if `coord` is at or beyond the last populated coordinate.
    pub fn next_populated(&self, coord: &Coordinate) -> Option<Coordinate> {
        // Binary search for the position
        match self.ordered.binary_search(coord) {
            Ok(idx) => {
                // Exact match — return the next one
                if idx + 1 < self.ordered.len() {
                    Some(self.ordered[idx + 1])
                } else {
                    None
                }
            }
            Err(idx) => {
                // Not found — idx is where it would be inserted, so idx is the next
                if idx < self.ordered.len() {
                    Some(self.ordered[idx])
                } else {
                    None
                }
            }
        }
    }

    /// Find the previous populated coordinate before `coord` in document order.
    pub fn prev_populated(&self, coord: &Coordinate) -> Option<Coordinate> {
        match self.ordered.binary_search(coord) {
            Ok(idx) | Err(idx) => {
                if idx > 0 {
                    Some(self.ordered[idx - 1])
                } else {
                    None
                }
            }
        }
    }

    /// Find all populated coordinates that match in a given dimension range.
    /// For example, "all scrolls in book 3 of volume 2" etc.
    /// This is the lattice navigator's workhorse.
    pub fn coordinates_matching<F>(&self, predicate: F) -> Vec<Coordinate>
    where
        F: Fn(&Coordinate) -> bool,
    {
        self.ordered.iter().copied().filter(predicate).collect()
    }

    /// Get the density of a dimension — how many distinct values exist
    /// for a given dimension across all populated coordinates.
    pub fn dimension_extent(&self, dim: crate::coordinate_ext::Dimension) -> Vec<usize> {
        use crate::coordinate_ext::CoordinateNav;
        let mut values: Vec<usize> = self.ordered.iter()
            .map(|c| c.dimension_value(dim))
            .collect();
        values.sort_unstable();
        values.dedup();
        values
    }
}

#[derive(Debug, Clone, Copy)]
enum DimBreak {
    Scroll,
    Section,
    Chapter,
    Book,
    Volume,
    Collection,
    Series,
    Shelf,
    Library,
}

#[cfg(test)]
mod tests {
    use super::*;
    use libphext::phext::to_coordinate;

    fn make_test_phext() -> Vec<u8> {
        // "hello" at 1.1.1/1.1.1/1.1.1, "world" at 1.1.1/1.1.1/1.1.2, "deep" at 1.1.1/1.1.2/1.1.1
        let mut buf = Vec::new();
        buf.extend_from_slice(b"hello");
        buf.push(0x17); // scroll break → 1.1.1/1.1.1/1.1.2
        buf.extend_from_slice(b"world");
        buf.push(0x18); // section break → 1.1.1/1.1.1/1.2.1
        buf.extend_from_slice(b"deep");
        buf
    }

    #[test]
    fn build_and_lookup() {
        let phext = make_test_phext();
        let index = LatticeIndex::build(&phext);

        assert_eq!(index.scroll_count(), 3);

        let base = to_coordinate("1.1.1/1.1.1/1.1.1");
        let span = index.get(&base).unwrap();
        assert_eq!(&phext[span.start..span.end], b"hello");

        let scroll2 = to_coordinate("1.1.1/1.1.1/1.1.2");
        let span2 = index.get(&scroll2).unwrap();
        assert_eq!(&phext[span2.start..span2.end], b"world");

        let section2 = to_coordinate("1.1.1/1.1.1/1.2.1");
        let span3 = index.get(&section2).unwrap();
        assert_eq!(&phext[span3.start..span3.end], b"deep");
    }

    #[test]
    fn next_prev_populated() {
        let phext = make_test_phext();
        let index = LatticeIndex::build(&phext);

        let base = to_coordinate("1.1.1/1.1.1/1.1.1");
        let next = index.next_populated(&base).unwrap();
        assert_eq!(next, to_coordinate("1.1.1/1.1.1/1.1.2"));

        let prev = index.prev_populated(&next).unwrap();
        assert_eq!(prev, base);
    }

    #[test]
    fn empty_coordinate_returns_none() {
        let phext = make_test_phext();
        let index = LatticeIndex::build(&phext);
        let missing = to_coordinate("5.5.5/5.5.5/5.5.5");
        assert!(index.get(&missing).is_none());
    }

    #[test]
    fn from_entries_sorts_and_keeps_last_duplicate() {
        let a = to_coordinate("1.1.1/1.1.1/1.1.2");
        let b = to_coordinate("1.1.1/1.1.1/1.1.1");
        let idx = LatticeIndex::from_entries(vec![
            (a, ScrollSpan { start: 0, end: 1 }),
            (b, ScrollSpan { start: 2, end: 3 }),
            (a, ScrollSpan { start: 4, end: 5 }),
        ], 5);
        assert_eq!(idx.coordinates(), &[b, a]);
        assert_eq!(idx.get(&a), Some(&ScrollSpan { start: 4, end: 5 }));
        assert_eq!(idx.spans().len(), 2);
    }

    #[test]
    fn document_order_preserved() {
        let phext = make_test_phext();
        let index = LatticeIndex::build(&phext);
        let coords = index.coordinates();
        assert_eq!(coords.len(), 3);
        assert!(coords[0] < coords[1]);
        assert!(coords[1] < coords[2]);
    }
}
