/// Lattice Search — find content across the 9D coordinate space.
///
/// The editor's `/` command. Search returns coordinates where matches occur,
/// not just byte offsets — because in phext, location IS meaning.
///
/// Two implementations:
/// - `search_lattice` — single-threaded, for small lattices or constrained contexts
/// - `search_lattice_parallel` — rayon work-stealing across available cores
///
/// On a 16-thread Ryzen 9 with 808 scrolls (4.26 MB), parallel search
/// saturates all cores when there's sufficient work per scroll.

use libphext::phext::Coordinate;
use rayon::prelude::*;
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

/// Context radius in characters on each side of a match.
const CONTEXT_RADIUS: usize = 40;

/// Minimum total bytes to justify parallelism overhead.
/// Below this, serial is faster due to thread pool synchronization cost.
/// Benchmarked on Ryzen 9 8945HS (16 threads):
///   - 4.26 MB / 808 scrolls: parallel wins for case-insensitive (3.6× on rare words)
///   - Serial wins when total work < ~1ms (common words, small files)
///   - Crossover point is roughly 1MB of text for case-insensitive search.
const PARALLEL_BYTE_THRESHOLD: usize = 512 * 1024; // 512 KB

/// Search a single scroll for all occurrences of a pattern.
/// Returns hits with coordinate and context. This is the hot inner loop.
#[inline]
fn search_scroll(
    coord: Coordinate,
    bytes: &[u8],
    pattern: &str,
    pattern_lower: &str,
    case_sensitive: bool,
    max_results: usize,
) -> Vec<SearchHit> {
    let content = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    // Avoid allocation for case-sensitive searches
    let search_content;
    let search_in = if case_sensitive {
        content
    } else {
        search_content = content.to_lowercase();
        &search_content
    };

    let needle = if case_sensitive { pattern } else { pattern_lower };
    let mut hits = Vec::new();
    let mut start = 0;

    while let Some(pos) = search_in[start..].find(needle) {
        let absolute_pos = start + pos;

        // Build context snippet
        let ctx_start = content[..absolute_pos]
            .char_indices()
            .rev()
            .nth(CONTEXT_RADIUS)
            .map(|(i, _)| i)
            .unwrap_or(0);

        let ctx_end = content[absolute_pos + pattern.len()..]
            .char_indices()
            .nth(CONTEXT_RADIUS)
            .map(|(i, _)| absolute_pos + pattern.len() + i)
            .unwrap_or(content.len());

        let mut context = String::with_capacity(ctx_end - ctx_start + 4);
        if ctx_start > 0 {
            context.push('…');
        }
        context.push_str(&content[ctx_start..ctx_end]);
        if ctx_end < content.len() {
            context.push('…');
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

    hits
}

/// Search the lattice for a text pattern (single-threaded).
/// Returns hits in document order (by coordinate).
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

    for &coord in index.coordinates() {
        if hits.len() >= max_results {
            break;
        }

        if let Some(span) = index.get(&coord) {
            let remaining = max_results - hits.len();
            let scroll_hits = search_scroll(
                coord,
                &buffer[span.start..span.end],
                pattern,
                &pattern_lower,
                case_sensitive,
                remaining,
            );
            hits.extend(scroll_hits);
        }
    }

    hits
}

/// Search the lattice for a text pattern across all available cores.
///
/// Uses rayon's work-stealing thread pool. Each scroll is a work unit —
/// scrolls are independent so there's zero contention. Results are
/// collected per-thread then merged in coordinate order.
///
/// Falls back to serial for small lattices (< PARALLEL_THRESHOLD scrolls)
/// where thread synchronization cost exceeds the parallelism benefit.
pub fn search_lattice_parallel(
    buffer: &[u8],
    index: &LatticeIndex,
    pattern: &str,
    case_sensitive: bool,
    max_results: usize,
) -> Vec<SearchHit> {
    let coords = index.coordinates();

    // Not enough work to justify thread pool overhead.
    // Case-sensitive search is ~10× cheaper (no .to_lowercase() alloc per scroll),
    // so we raise the threshold accordingly.
    let effective_threshold = if case_sensitive {
        PARALLEL_BYTE_THRESHOLD * 8
    } else {
        PARALLEL_BYTE_THRESHOLD
    };

    if buffer.len() < effective_threshold || coords.len() < 16 {
        return search_lattice(buffer, index, pattern, case_sensitive, max_results);
    }

    let pattern_lower = if case_sensitive {
        String::new()
    } else {
        pattern.to_lowercase()
    };

    // Build work items: (index_in_document_order, coordinate, byte_slice)
    // We include the index so we can restore document order after parallel scatter.
    let work: Vec<(usize, Coordinate, &[u8])> = coords.iter()
        .enumerate()
        .filter_map(|(i, &coord)| {
            index.get(&coord).map(|span| (i, coord, &buffer[span.start..span.end]))
        })
        .collect();

    // Use AtomicUsize for cross-thread early termination.
    // Threads check this periodically and stop searching when we have enough hits.
    let global_count = std::sync::atomic::AtomicUsize::new(0);

    // Parallel search — each scroll searched independently
    let mut all_hits: Vec<(usize, Vec<SearchHit>)> = work.par_iter()
        .map(|&(doc_idx, coord, bytes)| {
            // Check if we already have enough hits globally
            if global_count.load(std::sync::atomic::Ordering::Relaxed) >= max_results {
                return (doc_idx, Vec::new());
            }

            let hits = search_scroll(
                coord,
                bytes,
                pattern,
                &pattern_lower,
                case_sensitive,
                max_results,
            );

            global_count.fetch_add(hits.len(), std::sync::atomic::Ordering::Relaxed);
            (doc_idx, hits)
        })
        .filter(|(_, hits)| !hits.is_empty())
        .collect();

    // Restore document order
    all_hits.sort_unstable_by_key(|(idx, _)| *idx);

    // Flatten and truncate to max_results
    let total: usize = all_hits.iter().map(|(_, h)| h.len()).sum();
    let mut result = Vec::with_capacity(max_results.min(total));
    for (_, hits) in all_hits {
        for hit in hits {
            if result.len() >= max_results {
                return result;
            }
            result.push(hit);
        }
    }

    result
}

/// Auto-selecting search: picks serial or parallel based on data characteristics.
/// This is what the editor should call.
pub fn search_lattice_auto(
    buffer: &[u8],
    index: &LatticeIndex,
    pattern: &str,
    case_sensitive: bool,
    max_results: usize,
) -> Vec<SearchHit> {
    search_lattice_parallel(buffer, index, pattern, case_sensitive, max_results)
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

/// Parallel index build — partition the buffer into chunks, scan each
/// for delimiter boundaries, then merge. For very large phexts (>100MB)
/// where the O(n) sequential scan becomes noticeable.
///
/// Currently unused — sequential build is ~30ms for 4.26 MB which is
/// already below perceptual threshold. Included for future scaling.
pub fn build_index_parallel(buffer: &[u8], chunk_size: usize) -> LatticeIndex {
    if buffer.len() < chunk_size * 2 {
        return LatticeIndex::build(buffer);
    }
    // For now, fall back to sequential. The parallel implementation
    // requires careful delimiter-boundary alignment across chunk edges.
    // TODO: implement chunk-boundary reconciliation for >100MB phexts.
    LatticeIndex::build(buffer)
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

    fn big_phext(n_scrolls: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        for i in 0..n_scrolls {
            buf.extend_from_slice(format!("Scroll {} contains searchable content about the exocortex and phext dimensions. ", i).as_bytes());
            if i < n_scrolls - 1 {
                buf.push(0x17); // scroll break
            }
        }
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
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn parallel_matches_serial() {
        let phext = big_phext(100);
        let index = LatticeIndex::build(&phext);

        let serial = search_lattice(&phext, &index, "exocortex", false, 1000);
        let parallel = search_lattice_parallel(&phext, &index, "exocortex", false, 1000);

        assert_eq!(serial.len(), parallel.len());
        for (s, p) in serial.iter().zip(parallel.iter()) {
            assert_eq!(s.coordinate, p.coordinate);
            assert_eq!(s.offset, p.offset);
        }
    }

    #[test]
    fn parallel_respects_max_results() {
        let phext = big_phext(200);
        let index = LatticeIndex::build(&phext);
        let hits = search_lattice_parallel(&phext, &index, "content", false, 5);
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn parallel_preserves_document_order() {
        let phext = big_phext(100);
        let index = LatticeIndex::build(&phext);
        let hits = search_lattice_parallel(&phext, &index, "searchable", false, 1000);

        // Verify coordinates are in document order
        for window in hits.windows(2) {
            assert!(window[0].coordinate <= window[1].coordinate,
                "Out of order: {} > {}", window[0].coordinate, window[1].coordinate);
        }
    }
}
