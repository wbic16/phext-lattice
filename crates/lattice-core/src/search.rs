/// Lattice Search — find content across the 9D coordinate space.
///
/// The editor's `/` command. Search returns coordinates where matches occur,
/// not just byte offsets — because in phext, location IS meaning.

use libphext::phext::Coordinate;
use crate::index::LatticeIndex;

/// A search hit: coordinate + context around the match.
#[derive(Debug, Clone)]
pub struct SearchHit {
    /// The coordinate where the match was found.
    pub coordinate: Coordinate,
    /// Byte offset of the match within the scroll content.
    pub offset: usize,
    /// Length of the matched text.
    pub length: usize,
    /// A snippet of surrounding context.
    pub context: String,
}

/// Search the lattice for a text pattern.
/// Returns hits in document order (by coordinate).
///
/// This does a linear scan across all populated scrolls — acceptable for
/// interactive use (808 scrolls × avg ~5KB = ~4MB scanned in <10ms).
/// For larger lattices, we'd add an inverted index.
pub fn search_lattice(
    buffer: &[u8],
    index: &LatticeIndex,
    pattern: &str,
    case_sensitive: bool,
    max_results: usize,
) -> Vec<SearchHit> {
    let pattern_lower = if case_sensitive {
        String::new()
    } else {
        pattern.to_lowercase()
    };

    let mut hits = Vec::new();
    let context_radius = 40; // chars of context on each side

    for &coord in index.coordinates() {
        if hits.len() >= max_results {
            break;
        }

        if let Some(span) = index.get(&coord) {
            let bytes = &buffer[span.start..span.end];
            let content = match std::str::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let search_content = if case_sensitive {
                content.to_string()
            } else {
                content.to_lowercase()
            };

            let search_pattern = if case_sensitive {
                pattern
            } else {
                &pattern_lower
            };

            // Find all occurrences in this scroll
            let mut start = 0;
            while let Some(pos) = search_content[start..].find(search_pattern) {
                let absolute_pos = start + pos;

                // Build context snippet
                let ctx_start = content[..absolute_pos]
                    .char_indices()
                    .rev()
                    .nth(context_radius)
                    .map(|(i, _)| i)
                    .unwrap_or(0);

                let ctx_end = content[absolute_pos + pattern.len()..]
                    .char_indices()
                    .nth(context_radius)
                    .map(|(i, _)| absolute_pos + pattern.len() + i)
                    .unwrap_or(content.len());

                let mut context = String::new();
                if ctx_start > 0 {
                    context.push_str("…");
                }
                context.push_str(&content[ctx_start..ctx_end]);
                if ctx_end < content.len() {
                    context.push_str("…");
                }

                hits.push(SearchHit {
                    coordinate: coord,
                    offset: absolute_pos,
                    length: pattern.len(),
                    context,
                });

                if hits.len() >= max_results {
                    break;
                }

                start = absolute_pos + pattern.len();
            }
        }
    }

    hits
}

/// Search for coordinates matching a coordinate pattern string.
/// Supports partial coordinates like "3.3.3" (matches any coordinate
/// where any arm starts with 3.3.3).
pub fn search_coordinates(
    index: &LatticeIndex,
    pattern: &str,
) -> Vec<Coordinate> {
    let pattern_str = pattern.trim();
    index.coordinates()
        .iter()
        .copied()
        .filter(|c| format!("{}", c).contains(pattern_str))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::LatticeIndex;
    use libphext::phext::to_coordinate;

    fn test_phext() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"Hello World from BASE");
        buf.push(0x17); // scroll break
        buf.extend_from_slice(b"This is scroll two with hello again");
        buf.push(0x18); // section break
        buf.extend_from_slice(b"Section two has different content");
        buf
    }

    #[test]
    fn search_finds_matches() {
        let phext = test_phext();
        let index = LatticeIndex::build(&phext);
        let hits = search_lattice(&phext, &index, "hello", false, 100);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].coordinate, to_coordinate("1.1.1/1.1.1/1.1.1"));
        assert_eq!(hits[1].coordinate, to_coordinate("1.1.1/1.1.1/1.1.2"));
    }

    #[test]
    fn search_case_sensitive() {
        let phext = test_phext();
        let index = LatticeIndex::build(&phext);
        let hits = search_lattice(&phext, &index, "Hello", true, 100);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].coordinate, to_coordinate("1.1.1/1.1.1/1.1.1"));
    }

    #[test]
    fn search_respects_max_results() {
        let phext = test_phext();
        let index = LatticeIndex::build(&phext);
        let hits = search_lattice(&phext, &index, "hello", false, 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_coordinates_partial() {
        let phext = test_phext();
        let index = LatticeIndex::build(&phext);
        let matches = search_coordinates(&index, "1.1.1");
        assert_eq!(matches.len(), 3); // all three are under 1.1.1
    }
}
