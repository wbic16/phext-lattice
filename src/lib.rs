/// phext-lattice — the foundation layer for phext-fluent editing.
///
/// Provides:
/// - `coordinate_ext` — Hash/Eq, Dimension enum, dimensional navigation for Coordinate
/// - `index` — O(1) coordinate → byte-span lookup built from a single scan
/// - `mmap` — Memory-mapped phext files with copy-on-write editing
/// - `navigator` — 9D cursor model for lattice navigation
/// - `search` — Parallel search with early termination
/// - `term_index` — BM25 ranked full-text index over scrolls
/// - `names` — Title/alias → coordinate lookup, exact and case-folded
/// - `sentron` — 2×4×5×8 neural topology
/// - `stats` — Density, sparklines, lattice overview
/// - `cursor` — Sub-scroll cursor with word navigation
/// - `editor` — Pluggable editor mode trait
/// - `modes` — Vim-style editor mode
/// - `undo` — Undo/redo engine with phext serialization

pub mod coordinate_ext;
pub mod cursor;
pub mod editor;
pub mod index;
pub mod mmap;
pub mod modes;
pub mod names;
pub mod navigator;
#[cfg(feature = "web")]
pub mod oauth;
pub mod search;
pub mod sentron;
pub mod stats;
pub mod term_index;
pub mod tts;
pub mod uml6d;
pub mod undo;

// Re-exports for convenience
pub use coordinate_ext::{Dimension, CoordinateNav};
pub use cursor::{ScrollCursor, Position, Selection};
pub use editor::{EditorMode, EditContext, EditKey, EditKeyCode, EditResult, CursorStyle};
pub use index::{LatticeIndex, ScrollSpan};
pub use mmap::MappedLattice;
pub use modes::VimMode;
pub use names::{NameIndex, fold};
pub use navigator::Navigator;
pub use search::{search_lattice, search_lattice_parallel, search_lattice_auto, search_coordinates, SearchHit};
pub use sentron::{Sentron, Neuron, Axon, AxisGroup, SENTRON_CAPACITY};
pub use stats::{ScrollStats, DimensionDensity, LatticeOverview, Neighborhood};
pub use term_index::{TermIndex, TermHit, Bm25Params, tokenize};
pub use undo::{UndoEngine, EditOp, UndoRecord};
pub use tts::{CoordPronunciation, byte_to_syllable, dim_to_syllables, to_ssml, generate_pronunciation_doc, special_coords};
pub use uml6d::{Phase, ArtifactAddr, UmlContext, trace_artifact, artifacts_in_phase};
