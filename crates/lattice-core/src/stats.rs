/// Lattice Statistics — structural analysis for UI rendering.
///
/// Provides the data the TUI needs to show *where* content lives
/// in the 9D space: density distributions, scroll metrics, and
/// coordinate clustering.

use libphext::phext::Coordinate;
use crate::coordinate_ext::{Dimension, CoordinateNav};
use crate::index::LatticeIndex;

/// Per-scroll statistics for the status bar and info panel.
#[derive(Debug, Clone)]
pub struct ScrollStats {
    pub byte_size: usize,
    pub line_count: usize,
    pub word_count: usize,
    pub char_count: usize,
}

impl ScrollStats {
    /// Compute stats from raw scroll bytes.
    pub fn from_bytes(bytes: &[u8]) -> ScrollStats {
        let content = std::str::from_utf8(bytes).unwrap_or("");
        ScrollStats {
            byte_size: bytes.len(),
            line_count: if content.is_empty() { 0 } else { content.lines().count() },
            word_count: content.split_whitespace().count(),
            char_count: content.chars().count(),
        }
    }

    /// Human-readable size.
    pub fn size_display(&self) -> String {
        if self.byte_size < 1024 {
            format!("{} B", self.byte_size)
        } else if self.byte_size < 1024 * 1024 {
            format!("{:.1} KB", self.byte_size as f64 / 1024.0)
        } else {
            format!("{:.2} MB", self.byte_size as f64 / (1024.0 * 1024.0))
        }
    }
}

/// Density profile for one dimension: how many populated scrolls
/// exist at each value along that dimension.
#[derive(Debug, Clone)]
pub struct DimensionDensity {
    pub dimension: Dimension,
    /// (value, count) pairs sorted by value.
    pub distribution: Vec<(usize, usize)>,
    /// Maximum count across all values (for normalization).
    pub max_count: usize,
    /// Total populated scrolls.
    pub total: usize,
}

impl DimensionDensity {
    /// Build the density profile for one dimension across all populated coordinates.
    pub fn build(dimension: Dimension, index: &LatticeIndex) -> DimensionDensity {
        let mut counts: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        for coord in index.coordinates() {
            let val = coord.dimension_value(dimension);
            *counts.entry(val).or_insert(0) += 1;
        }

        let mut distribution: Vec<(usize, usize)> = counts.into_iter().collect();
        distribution.sort_unstable_by_key(|(v, _)| *v);

        let max_count = distribution.iter().map(|(_, c)| *c).max().unwrap_or(0);
        let total = index.scroll_count();

        DimensionDensity { dimension, distribution, max_count, total }
    }

    /// Render a sparkline string showing relative density.
    /// Width is the number of characters available.
    pub fn sparkline(&self, width: usize) -> String {
        if self.distribution.is_empty() || width == 0 {
            return String::new();
        }

        let blocks = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

        // If we have more buckets than width, we need to bin
        if self.distribution.len() <= width {
            // One char per value
            self.distribution.iter().map(|(_, count)| {
                if self.max_count == 0 { ' ' }
                else {
                    let level = (*count as f64 / self.max_count as f64 * 7.0) as usize;
                    blocks[level.min(7)]
                }
            }).collect()
        } else {
            // Bin into `width` buckets
            let min_val = self.distribution.first().map(|(v, _)| *v).unwrap_or(1);
            let max_val = self.distribution.last().map(|(v, _)| *v).unwrap_or(1);
            let range = (max_val - min_val + 1) as f64;
            let mut buckets = vec![0usize; width];

            for &(val, count) in &self.distribution {
                let bucket = ((val - min_val) as f64 / range * width as f64) as usize;
                let bucket = bucket.min(width - 1);
                buckets[bucket] += count;
            }

            let bucket_max = buckets.iter().copied().max().unwrap_or(0);
            buckets.iter().map(|&count| {
                if bucket_max == 0 { ' ' }
                else if count == 0 { ' ' }
                else {
                    let level = (count as f64 / bucket_max as f64 * 7.0) as usize;
                    blocks[level.min(7)]
                }
            }).collect()
        }
    }

    /// How many distinct values exist in this dimension.
    pub fn distinct_values(&self) -> usize {
        self.distribution.len()
    }

    /// The range of populated values.
    pub fn range(&self) -> Option<(usize, usize)> {
        if self.distribution.is_empty() {
            None
        } else {
            Some((
                self.distribution.first().unwrap().0,
                self.distribution.last().unwrap().0,
            ))
        }
    }
}

/// Neighborhood context: what's near the current coordinate.
#[derive(Debug)]
pub struct Neighborhood {
    /// Populated coordinates sharing the same Z arm.
    pub same_z: usize,
    /// Populated coordinates sharing the same Y arm.
    pub same_y: usize,
    /// Populated coordinates sharing the same X arm (same section).
    pub same_x: usize,
    /// Nearest populated coordinates in each dimension (backward, forward).
    pub dim_neighbors: [(Option<Coordinate>, Option<Coordinate>); 9],
}

impl Neighborhood {
    /// Build neighborhood context for a coordinate.
    pub fn build(coord: &Coordinate, index: &LatticeIndex) -> Neighborhood {
        let same_z = index.coordinates_matching(|c| {
            c.z == coord.z
        }).len();
        let same_y = index.coordinates_matching(|c| {
            c.z == coord.z && c.y == coord.y
        }).len();
        let same_x = index.coordinates_matching(|c| {
            c.z == coord.z && c.y == coord.y &&
            c.x.chapter == coord.x.chapter && c.x.section == coord.x.section
        }).len();

        let mut dim_neighbors = [(None, None); 9];
        for dim_idx in 0..9 {
            let dim = Dimension::from_index(dim_idx + 1).unwrap();
            let current_val = coord.dimension_value(dim);

            // Find nearest populated coordinate with lower dim value
            let mut backward = coord.clone();
            let back = if current_val > 1 {
                backward.set_dimension(dim, current_val - 1);
                // Search for nearest populated at or before this
                index.prev_populated(&backward)
                    .filter(|c| c.dimension_value(dim) < current_val)
            } else {
                None
            };

            // Find nearest populated coordinate with higher dim value
            let mut forward = coord.clone();
            forward.set_dimension(dim, current_val + 1);
            let fwd = index.next_populated(&forward)
                .filter(|c| c.dimension_value(dim) > current_val);

            dim_neighbors[dim_idx as usize] = (back, fwd);
        }

        Neighborhood { same_z, same_y, same_x, dim_neighbors }
    }
}

/// Global lattice overview stats.
#[derive(Debug, Clone)]
pub struct LatticeOverview {
    pub total_scrolls: usize,
    pub total_bytes: usize,
    pub dimension_densities: Vec<DimensionDensity>,
    pub avg_scroll_size: usize,
    pub largest_scroll: Option<(Coordinate, usize)>,
    pub smallest_scroll: Option<(Coordinate, usize)>,
}

impl LatticeOverview {
    /// Build a complete overview of the lattice.
    pub fn build(_buffer: &[u8], index: &LatticeIndex) -> LatticeOverview {
        let total_scrolls = index.scroll_count();
        let total_bytes = index.buffer_len();

        let dimension_densities: Vec<DimensionDensity> = (1..=9u8)
            .map(|i| DimensionDensity::build(Dimension::from_index(i).unwrap(), index))
            .collect();

        let mut largest: Option<(Coordinate, usize)> = None;
        let mut smallest: Option<(Coordinate, usize)> = None;
        let mut content_bytes = 0usize;

        for &coord in index.coordinates() {
            if let Some(span) = index.get(&coord) {
                let size = span.len();
                content_bytes += size;

                match &largest {
                    None => largest = Some((coord, size)),
                    Some((_, max)) if size > *max => largest = Some((coord, size)),
                    _ => {}
                }
                match &smallest {
                    None => smallest = Some((coord, size)),
                    Some((_, min)) if size < *min => smallest = Some((coord, size)),
                    _ => {}
                }
            }
        }

        let avg_scroll_size = if total_scrolls > 0 { content_bytes / total_scrolls } else { 0 };

        LatticeOverview {
            total_scrolls,
            total_bytes,
            dimension_densities,
            avg_scroll_size,
            largest_scroll: largest,
            smallest_scroll: smallest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_phext() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"Hello World from BASE");
        buf.push(0x17);
        buf.extend_from_slice(b"Short");
        buf.push(0x18);
        buf.extend_from_slice(b"This is a longer scroll with more words in it for testing");
        buf
    }

    #[test]
    fn scroll_stats_basic() {
        let data = b"Hello World\nSecond line\nThird";
        let stats = ScrollStats::from_bytes(data);
        assert_eq!(stats.line_count, 3);
        assert_eq!(stats.word_count, 5);
        assert_eq!(stats.byte_size, data.len());
    }

    #[test]
    fn dimension_density() {
        let phext = test_phext();
        let index = LatticeIndex::build(&phext);
        let density = DimensionDensity::build(Dimension::Scroll, &index);
        assert!(density.distinct_values() > 0);
        assert_eq!(density.total, 3);
    }

    #[test]
    fn sparkline_render() {
        let phext = test_phext();
        let index = LatticeIndex::build(&phext);
        let density = DimensionDensity::build(Dimension::Scroll, &index);
        let spark = density.sparkline(20);
        assert!(!spark.is_empty());
    }

    #[test]
    fn lattice_overview() {
        let phext = test_phext();
        let index = LatticeIndex::build(&phext);
        let overview = LatticeOverview::build(&phext, &index);
        assert_eq!(overview.total_scrolls, 3);
        assert!(overview.largest_scroll.is_some());
        assert!(overview.smallest_scroll.is_some());
        assert_eq!(overview.dimension_densities.len(), 9);
    }

    #[test]
    fn size_display() {
        assert_eq!(ScrollStats::from_bytes(b"hi").size_display(), "2 B");
        assert_eq!(ScrollStats::from_bytes(&vec![0u8; 2048]).size_display(), "2.0 KB");
    }
}
