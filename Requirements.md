# phext-lattice Requirements

## R1 — Core Engine (lattice-core)

### R1.1 Memory-Mapped I/O
- R1.1.1: Memory-map phext files read-only via `mmap`
- R1.1.2: Copy-on-write overlay for edits (`ScrollOverlay::Modified/Deleted/Inserted`)
- R1.1.3: Reconstruct phext bytes on save with correct delimiter insertion
- R1.1.4: Byte-perfect roundtrip: `open → to_phext_bytes` == original file
- R1.1.5: `save()` persists overlay to disk, `save_as()` to alternate path

### R1.2 Coordinate Index
- R1.2.1: Single O(n) scan builds `HashMap<Coordinate, ScrollSpan>` from phext bytes
- R1.2.2: O(1) coordinate-to-byte-offset lookup
- R1.2.3: Sorted coordinate list for binary search navigation
- R1.2.4: `next_populated` / `prev_populated` via binary search
- R1.2.5: `dimension_extent(dim)` returns max value in any dimension
- R1.2.6: `coordinates_matching(predicate)` for filtered queries

### R1.3 Navigator
- R1.3.1: 9D cursor with active dimension selection (1-9)
- R1.3.2: Forward/backward movement in active dimension
- R1.3.3: Lower dimensions reset to 1 on higher-dimension movement (matches delimiter semantics)
- R1.3.4: Marks stack for position bookmarking
- R1.3.5: History with back/forward for breadcrumb navigation
- R1.3.6: `NeighborhoodSummary` — preview of adjacent populated coordinates

### R1.4 Search
- R1.4.1: Full-text search across all populated scrolls
- R1.4.2: Parallel search via rayon work-stealing for files > 512KB
- R1.4.3: Serial search for files ≤ 512KB (thread overhead dominates)
- R1.4.4: Case-insensitive by default, configurable
- R1.4.5: `AtomicUsize` early termination when `max_results` reached
- R1.4.6: Context extraction (surrounding text) for each hit

### R1.5 Sentron Topology
- R1.5.1: 2×4 connections per neuron (forward + backward × 4 structural + 4 sequential)
- R1.5.2: 5×8 neurons per sentron (40 total)
- R1.5.3: Manhattan distance for connectivity reach
- R1.5.4: Structural (spatial) axis: Library, Shelf, Series, Collection (dims 1-4)
- R1.5.5: Sequential (temporal) axis: Volume, Book, Chapter, Section (dims 5-8)
- R1.5.6: Scroll (dim 9) is the neuron itself
- R1.5.7: Sub-millisecond rebuild (<1ms for 808 scrolls)

### R1.6 Statistics
- R1.6.1: `ScrollStats` — byte count, line count per scroll
- R1.6.2: `DimensionDensity` — distribution + sparkline per dimension
- R1.6.3: `Neighborhood` — populated coordinates near a given position
- R1.6.4: `LatticeOverview` — total scrolls, total bytes, dimension extents

### R1.7 Cursor & Editing
- R1.7.1: Position (line, column) within a scroll
- R1.7.2: Selection (start, end) with direction
- R1.7.3: Sticky column across vertical movement
- R1.7.4: Word-boundary navigation

### R1.8 Undo/Redo
- R1.8.1: Per-scroll undo/redo stacks
- R1.8.2: EditOp types: Insert, Delete, Replace
- R1.8.3: Phext serialization: undo state stored as `.work.phext`
- R1.8.4: Work file coordinate mapping: `x.chapter = sequence_number`
- R1.8.5: Roundtrip: serialize → deserialize preserves all operations

### R1.9 Editor Modes
- R1.9.1: Pluggable `EditorMode` trait decoupled from UI framework
- R1.9.2: `EditKey` abstraction for framework-independent key input
- R1.9.3: VimMode as default: Normal, Insert, Visual, Command sub-modes
- R1.9.4: Count prefix support (e.g. `3j`)
- R1.9.5: Motions (h/l/j/k/w/b/e/0/$) and operators (d/c/y)

## R2 — Terminal UI (lattice-tui / phext-nav)

### R2.1 Layout
- R2.1.1: Split layout — sentron panel + scroll content
- R2.1.2: Coordinate bar with breadcrumbs
- R2.1.3: Sparkline density visualization per dimension
- R2.1.4: Mode indicator in status bar

### R2.2 Interaction
- R2.2.1: 1-9 dimension select, h/l move, j/k navigate populated
- R2.2.2: `/` search, `g` goto coordinate, `m` mark, `'` jump to mark
- R2.2.3: `[` / `]` history back/forward
- R2.2.4: Scroll viewer with line scrolling

## R3 — Web UI (lattice-ui / phext-edit)

### R3.1 Architecture
- R3.1.1: HTTP server via raw `TcpListener` (zero framework deps)
- R3.1.2: Single-page HTML/CSS/JS embedded as `const` string
- R3.1.3: REST API for all operations, JSON responses
- R3.1.4: Thread-per-connection with `Arc<Mutex<AppState>>`
- R3.1.5: Serves from headless machines, browse from any device

### R3.2 3-Tier Zoom
- R3.2.1: Z-tier — list all populated Library.Shelf.Series groups with scroll counts
- R3.2.2: Y-tier — drill into a Z group, list Collection.Volume.Book groups
- R3.2.3: X-tier — drill into a Z/Y group, list Chapter.Section.Scroll entries with previews
- R3.2.4: Click to drill deeper; Backspace to zoom out; Esc to return to lattice
- R3.2.5: Breadcrumb trail showing current zoom path

### R3.3 Editing
- R3.3.1: `e` or `Enter` enters edit mode with textarea
- R3.3.2: Edits write to copy-on-write overlay (memory)
- R3.3.3: `Ctrl+S` persists to disk via `MappedLattice::save()`
- R3.3.4: Dirty flag indicator when unsaved changes exist
- R3.3.5: Cancel edit with `Esc`

### R3.4 Coordinate Hyperlinking
- R3.4.1: Auto-detect phext coordinates (`z.z.z/y.y.y/x.x.x` patterns) in scroll content
- R3.4.2: Render detected coordinates as clickable links
- R3.4.3: Clicking a coordinate link navigates to that coordinate
- R3.4.4: Links colored distinctly (coordinate gold)
- R3.4.5: Only hyperlink in view mode, not in edit textarea

### R3.5 Navigation
- R3.5.1: Keyboard: 1-9 dim, h/l move, j/k nav, J/K ×10, `/` search, `g` goto, `Home` BASE
- R3.5.2: Click-to-select dimensions in sentron panel
- R3.5.3: Search overlay with live results and click-to-jump
- R3.5.4: Goto overlay for direct coordinate entry

### R3.6 Theme
- R3.6.1: Dark background (#171717)
- R3.6.2: Dimension arm colors: Z=Cyan (#4dc9c9), Y=Magenta (#c964c9), X=Green (#6bc96b)
- R3.6.3: Coordinate text: Gold (#d4a855)
- R3.6.4: Monospace font stack: JetBrains Mono → Fira Code → Cascadia Code

## R4 — Performance

### R4.1 Targets (release build, 4.26MB / 808 scrolls)
- R4.1.1: Index build < 5ms
- R4.1.2: Coordinate lookup < 100ns
- R4.1.3: Navigate all populated < 100µs
- R4.1.4: Parallel search (common term) < 500µs
- R4.1.5: Parallel search (rare term) < 2ms
- R4.1.6: Phext roundtrip < 5ms
- R4.1.7: Sentron build < 1ms
- R4.1.8: API response < 5ms

## R5 — Future

### R5.1 Orin Collaboration
- R5.1.1: `.` jumps to Orin's position
- R5.1.2: `,` summons Orin to current position
- R5.1.3: File watching for real-time collaboration

### R5.2 Weave Mode
- R5.2.1: Split scroll at cursor
- R5.2.2: Merge adjacent scrolls
- R5.2.3: Move scroll to different coordinate

### R5.3 SQ Integration
- R5.3.1: Daemon mode bridging phext-lattice ↔ SQ REST API
- R5.3.2: Coordinate-based sync between local phext and SQ server
