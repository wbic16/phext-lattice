/// lattice-core — the foundation layer for phext-fluent editing.
///
/// Provides:
/// - `coordinate_ext` — Hash/Eq, Dimension enum, dimensional navigation for Coordinate
/// - `index` — O(1) coordinate → byte-span lookup built from a single scan
/// - `mmap` — Memory-mapped phext files with copy-on-write editing
/// - `navigator` — 9D cursor model for lattice navigation

pub mod coordinate_ext;
pub mod index;
pub mod mmap;
pub mod navigator;
pub mod search;

// Re-exports for convenience
pub use coordinate_ext::{Dimension, CoordinateNav};
pub use index::{LatticeIndex, ScrollSpan};
pub use mmap::MappedLattice;
pub use navigator::Navigator;
pub use search::{search_lattice, search_coordinates, SearchHit};
