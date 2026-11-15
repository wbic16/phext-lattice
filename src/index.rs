//! Lattice index — homed in holo-phext since the layer2 split.
//!
//! This module was phext-lattice's own `LatticeIndex` (one scan →
//! sorted span table → binary-search lookups) until the
//! re-implementation of phext-lattice on HoloPhext (DESIGN.md step 1
//! in wbic16/holo-phext). The type now lives in exactly one place —
//! `holo_phext::index` — and is re-exported here so every existing
//! `crate::index::…` path in this codebase keeps working unchanged.
//!
//! One API difference: `dimension_extent` now takes
//! `holo_phext::Dimension` instead of `crate::coordinate_ext::Dimension`.
//! Nothing in this repo called it; the method itself is unchanged.

pub use holo_phext::index::{LatticeIndex, ScrollSpan};
