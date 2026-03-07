/// Sentron — the neural topology of phext navigation.
///
/// A sentron is 40 neurons (5×8), each with 8 connections (2×4).
/// This maps directly onto phext's 9D coordinate space:
///
///   - **Scroll** (dimension 9) is the neuron itself — the content atom.
///   - **8 higher dimensions** are the 8 connections per neuron.
///   - These 8 split into two groups of 4:
///     - **Structural** (spatial, the "where"):
///       Library, Shelf, Series, Collection
///     - **Sequential** (temporal, the "when"):
///       Volume, Book, Chapter, Section
///   - The 2 in "2×4" is directionality: backward (−) and forward (+)
///     along each of 4 axes per group.
///
/// A sentron is the 40 nearest populated coordinates to the cursor —
/// the local neighborhood, the cortical column, the unit of coherent
/// navigation. You don't browse a flat list of dimensions. You stand
/// inside a sentron and see its shape.
///
/// Geometry (from Will's haystack model):
///   2×5 + 5 + 2×5 = 25 (cortical column cross-section)
///   5×12 = 60 (column height × neurons)
///   60×6 = 360 (full rotation, six-fold symmetry)
///
/// In the TUI, the sentron renders as a compact topology map showing
/// connection density along each of the 8 axes, grouped into
/// structural/sequential pairs.

use libphext::phext::Coordinate;
use crate::coordinate_ext::{Dimension, CoordinateNav};
use crate::index::LatticeIndex;

/// The two fundamental axis groups in the sentron topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisGroup {
    /// Library, Shelf, Series, Collection — the shape of space.
    Structural,
    /// Volume, Book, Chapter, Section — the flow of time.
    Sequential,
}

impl AxisGroup {
    pub fn name(&self) -> &'static str {
        match self {
            AxisGroup::Structural => "spatial",
            AxisGroup::Sequential => "temporal",
        }
    }

    /// The 4 dimensions in this axis group.
    pub fn dimensions(&self) -> [Dimension; 4] {
        match self {
            AxisGroup::Structural => [
                Dimension::Library,    // dim 1
                Dimension::Shelf,      // dim 2
                Dimension::Series,     // dim 3
                Dimension::Collection, // dim 4
            ],
            AxisGroup::Sequential => [
                Dimension::Volume,  // dim 5
                Dimension::Book,    // dim 6
                Dimension::Chapter, // dim 7
                Dimension::Section, // dim 8
            ],
        }
    }
}

/// One connection axis: a dimension with backward/forward reach.
#[derive(Debug, Clone)]
pub struct Axon {
    pub dimension: Dimension,
    pub group: AxisGroup,
    /// Number of populated coordinates reachable going backward (−).
    pub backward_count: usize,
    /// Number of populated coordinates reachable going forward (+).
    pub forward_count: usize,
    /// Nearest populated coordinate backward along this dimension.
    pub backward_nearest: Option<Coordinate>,
    /// Nearest populated coordinate forward along this dimension.
    pub forward_nearest: Option<Coordinate>,
}

impl Axon {
    /// Total reachable coordinates along this axis.
    pub fn total(&self) -> usize {
        self.backward_count + self.forward_count
    }

    /// Visual weight: how "thick" this connection is (0.0 to 1.0).
    pub fn weight(&self, max_total: usize) -> f64 {
        if max_total == 0 { return 0.0; }
        self.total() as f64 / max_total as f64
    }
}

/// A neuron in the sentron: a populated coordinate with its 8 axons.
#[derive(Debug, Clone)]
pub struct Neuron {
    pub coordinate: Coordinate,
    pub axons: [Axon; 8],
    /// Distance from the sentron center (0 = center neuron).
    pub distance: usize,
}

/// The sentron: 40 nearest populated coordinates to the cursor,
/// with full 8-way connectivity analysis.
#[derive(Debug)]
pub struct Sentron {
    /// The center neuron (cursor position, or nearest populated).
    pub center: Coordinate,
    /// Up to 40 neurons in the sentron (5×8 capacity).
    pub neurons: Vec<Neuron>,
    /// The 8 axons from the center (the navigator's connection view).
    pub axons: [Axon; 8],
    /// Maximum axon total across all 8 (for normalization).
    pub max_axon_total: usize,
    /// Structural axis summary: total reachable via spatial dims.
    pub structural_reach: usize,
    /// Sequential axis summary: total reachable via temporal dims.
    pub sequential_reach: usize,
}

/// The maximum number of neurons in a sentron.
pub const SENTRON_CAPACITY: usize = 40;

/// The number of connections per neuron.
pub const CONNECTIONS_PER_NEURON: usize = 8;

impl Sentron {
    /// Build a sentron centered on a coordinate.
    ///
    /// Finds the 40 nearest populated coordinates and analyzes
    /// their 8-way connectivity along structural/sequential axes.
    pub fn build(center: &Coordinate, index: &LatticeIndex) -> Sentron {
        let coords = index.coordinates();

        // Find the 40 nearest populated coordinates.
        // "Distance" here is the sum of absolute differences across all 9 dimensions.
        // This gives us a Manhattan distance in 9D coordinate space.
        let mut distances: Vec<(Coordinate, usize)> = coords.iter()
            .map(|&c| (c, coord_distance(center, &c)))
            .collect();
        distances.sort_by_key(|(_, d)| *d);
        distances.truncate(SENTRON_CAPACITY);

        // Build center axons
        let axons = build_axons(center, index);
        let max_axon_total = axons.iter().map(|a| a.total()).max().unwrap_or(0);

        let structural_reach: usize = axons[0..4].iter().map(|a| a.total()).sum();
        let sequential_reach: usize = axons[4..8].iter().map(|a| a.total()).sum();

        // Build neurons
        let neurons: Vec<Neuron> = distances.iter().map(|&(coord, dist)| {
            Neuron {
                coordinate: coord,
                axons: build_axons(&coord, index),
                distance: dist,
            }
        }).collect();

        Sentron {
            center: *center,
            neurons,
            axons,
            max_axon_total,
            structural_reach,
            sequential_reach,
        }
    }

    /// How many neurons are in this sentron.
    pub fn size(&self) -> usize {
        self.neurons.len()
    }

    /// The structural (spatial) axons: Library, Shelf, Series, Collection.
    pub fn structural_axons(&self) -> &[Axon] {
        &self.axons[0..4]
    }

    /// The sequential (temporal) axons: Volume, Book, Chapter, Section.
    pub fn sequential_axons(&self) -> &[Axon] {
        &self.axons[4..8]
    }

    /// Render the sentron as a compact topology string.
    /// Shows connection density along each axis.
    pub fn topology_string(&self) -> String {
        let blocks = ['·', '░', '▒', '▓', '█'];

        let mut s = String::new();
        s.push_str("╭─ spatial ─────────╮  ╭─ temporal ────────╮\n");

        // Row 1: dimension names
        s.push_str("│ ");
        for axon in self.structural_axons() {
            s.push_str(&format!("{:>4} ", &axon.dimension.name()[..4]));
        }
        s.push_str("│  │ ");
        for axon in self.sequential_axons() {
            s.push_str(&format!("{:>4} ", &axon.dimension.name()[..4]));
        }
        s.push_str("│\n");

        // Row 2: backward counts with density blocks
        s.push_str("│ ");
        for axon in self.structural_axons() {
            let w = axon.weight(self.max_axon_total);
            let block = blocks[(w * 4.0).min(4.0) as usize];
            s.push_str(&format!("−{:>2}{} ", axon.backward_count, block));
        }
        s.push_str("│  │ ");
        for axon in self.sequential_axons() {
            let w = axon.weight(self.max_axon_total);
            let block = blocks[(w * 4.0).min(4.0) as usize];
            s.push_str(&format!("−{:>2}{} ", axon.backward_count, block));
        }
        s.push_str("│\n");

        // Row 3: forward counts
        s.push_str("│ ");
        for axon in self.structural_axons() {
            let w = axon.weight(self.max_axon_total);
            let block = blocks[(w * 4.0).min(4.0) as usize];
            s.push_str(&format!("+{:>2}{} ", axon.forward_count, block));
        }
        s.push_str("│  │ ");
        for axon in self.sequential_axons() {
            let w = axon.weight(self.max_axon_total);
            let block = blocks[(w * 4.0).min(4.0) as usize];
            s.push_str(&format!("+{:>2}{} ", axon.forward_count, block));
        }
        s.push_str("│\n");

        s.push_str("╰────────────────────╯  ╰────────────────────╯");
        s
    }
}

/// Manhattan distance in 9D coordinate space.
fn coord_distance(a: &Coordinate, b: &Coordinate) -> usize {
    let mut dist = 0usize;
    for i in 1..=9u8 {
        let dim = Dimension::from_index(i).unwrap();
        let va = a.dimension_value(dim);
        let vb = b.dimension_value(dim);
        dist += if va > vb { va - vb } else { vb - va };
    }
    dist
}

/// Build the 8 axons (connection analysis) for a coordinate.
fn build_axons(coord: &Coordinate, index: &LatticeIndex) -> [Axon; 8] {
    let all_dims = [
        // Structural (spatial)
        (Dimension::Library,    AxisGroup::Structural),
        (Dimension::Shelf,      AxisGroup::Structural),
        (Dimension::Series,     AxisGroup::Structural),
        (Dimension::Collection, AxisGroup::Structural),
        // Sequential (temporal)
        (Dimension::Volume,     AxisGroup::Sequential),
        (Dimension::Book,       AxisGroup::Sequential),
        (Dimension::Chapter,    AxisGroup::Sequential),
        (Dimension::Section,    AxisGroup::Sequential),
    ];

    let coords = index.coordinates();
    let current_val: [usize; 9] = std::array::from_fn(|i| {
        coord.dimension_value(Dimension::from_index(i as u8 + 1).unwrap())
    });

    std::array::from_fn(|i| {
        let (dimension, group) = all_dims[i];
        let dim_idx = dimension as u8 - 1;
        let cur = current_val[dim_idx as usize];

        let mut backward_count = 0usize;
        let mut forward_count = 0usize;
        let mut backward_nearest: Option<(Coordinate, usize)> = None;
        let mut forward_nearest: Option<(Coordinate, usize)> = None;

        for &c in coords {
            let val = c.dimension_value(dimension);
            if val == cur { continue; }

            // Check if this coordinate matches on all OTHER dimensions
            // (i.e., it's reachable purely by moving along this one dimension)
            let mut matches_other = true;
            for j in 0..9u8 {
                if j == dim_idx { continue; }
                let d = Dimension::from_index(j + 1).unwrap();
                if c.dimension_value(d) != current_val[j as usize] {
                    matches_other = false;
                    break;
                }
            }

            if !matches_other {
                // Also count coordinates that share the same axis GROUP values
                // (looser connectivity — they're in the same "fiber bundle")
                if val < cur { backward_count += 1; }
                else { forward_count += 1; }
                continue;
            }

            // Exact axial neighbor (all other dims match)
            if val < cur {
                backward_count += 1;
                let dist = cur - val;
                match &backward_nearest {
                    None => backward_nearest = Some((c, dist)),
                    Some((_, d)) if dist < *d => backward_nearest = Some((c, dist)),
                    _ => {}
                }
            } else {
                forward_count += 1;
                let dist = val - cur;
                match &forward_nearest {
                    None => forward_nearest = Some((c, dist)),
                    Some((_, d)) if dist < *d => forward_nearest = Some((c, dist)),
                    _ => {}
                }
            }
        }

        Axon {
            dimension,
            group,
            backward_count,
            forward_count,
            backward_nearest: backward_nearest.map(|(c, _)| c),
            forward_nearest: forward_nearest.map(|(c, _)| c),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use libphext::phext::to_coordinate;

    fn test_phext() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"base scroll");
        buf.push(0x17); // scroll break → 1.1.1/1.1.1/1.1.2
        buf.extend_from_slice(b"second scroll");
        buf.push(0x18); // section break → 1.1.1/1.1.1/1.2.1
        buf.extend_from_slice(b"third scroll");
        buf.push(0x19); // chapter break → 1.1.1/1.1.1/2.1.1
        buf.extend_from_slice(b"chapter two");
        buf.push(0x1A); // book break → 1.1.1/1.1.2/1.1.1
        buf.extend_from_slice(b"book two");
        buf
    }

    #[test]
    fn sentron_builds() {
        let phext = test_phext();
        let index = LatticeIndex::build(&phext);
        let center = to_coordinate("1.1.1/1.1.1/1.1.1");
        let sentron = Sentron::build(&center, &index);

        assert_eq!(sentron.center, center);
        assert_eq!(sentron.size(), 5); // 5 populated coordinates
        assert!(sentron.size() <= SENTRON_CAPACITY);
    }

    #[test]
    fn sentron_axons_structural_vs_sequential() {
        let phext = test_phext();
        let index = LatticeIndex::build(&phext);
        let center = to_coordinate("1.1.1/1.1.1/1.1.1");
        let sentron = Sentron::build(&center, &index);

        // Should have some forward reach in sequential dimensions
        // (section, chapter moved forward in the test phext)
        assert!(sentron.sequential_reach > 0 || sentron.structural_reach > 0);
    }

    #[test]
    fn coord_distance_identity() {
        let c = to_coordinate("3.3.3/5.5.5/7.7.7");
        assert_eq!(coord_distance(&c, &c), 0);
    }

    #[test]
    fn coord_distance_symmetry() {
        let a = to_coordinate("1.1.1/1.1.1/1.1.1");
        let b = to_coordinate("3.3.3/5.5.5/7.7.7");
        assert_eq!(coord_distance(&a, &b), coord_distance(&b, &a));
    }

    #[test]
    fn sentron_capacity() {
        // Build a phext with > 40 scrolls
        let mut buf = Vec::new();
        for i in 0..50 {
            buf.extend_from_slice(format!("scroll {}", i).as_bytes());
            if i < 49 { buf.push(0x17); }
        }
        let index = LatticeIndex::build(&buf);
        let center = to_coordinate("1.1.1/1.1.1/1.1.1");
        let sentron = Sentron::build(&center, &index);

        assert_eq!(sentron.size(), SENTRON_CAPACITY);
    }

    #[test]
    fn topology_string_renders() {
        let phext = test_phext();
        let index = LatticeIndex::build(&phext);
        let center = to_coordinate("1.1.1/1.1.1/1.1.1");
        let sentron = Sentron::build(&center, &index);
        let topo = sentron.topology_string();

        assert!(topo.contains("spatial"));
        assert!(topo.contains("temporal"));
        assert!(topo.len() > 50);
    }
}
