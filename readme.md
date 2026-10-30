# phext-lattice

The foundation layer for phext-fluent editing. Memory-mapped 9D lattice with O(1) coordinate lookup.

## Performance (choose-your-own-adventure.phext, 4.26 MB)

| Operation | Time |
|-----------|------|
| Open + index (808 scrolls) | ~30ms |
| Coordinate lookup | ~2.4µs |
| Navigate all 807 hops | ~235µs |
| Full roundtrip rebuild | ~4.3ms |
| **Roundtrip fidelity** | **byte-perfect** |

## Offline Wikipedia benchmark

`bench/wiki/` compares phext (via libphext, phext-lattice, and SQ) against plain files, SQLite,
and Kiwix ZIM on a real Wikipedia ZIM, covering lookup latency, storage overhead, link traversal,
incremental updates, and agent retrieval quality. See [`bench/wiki/FINDINGS.md`](bench/wiki/FINDINGS.md)
for the tooling scorecard and [`bench/wiki/RESULTS.md`](bench/wiki/RESULTS.md) for the raw tables.

```bash
bench/wiki/run_all.sh /tmp/wikibench 1 10
```

## Architecture

```
src/
├── coordinate_ext.rs  — Dimension enum, dimensional navigation, CoordinateNav trait
├── index.rs           — LatticeIndex: sparse coordinate → byte-span HashMap, O(1) lookup
├── mmap.rs            — MappedLattice: memory-mapped phext with copy-on-write overlay
├── navigator.rs       — Navigator: 9D cursor model with history, marks, populated-jump
├── search.rs          — Parallel search with early termination
├── term_index.rs      — TermIndex: BM25 ranked full-text index over scrolls
├── names.rs           — NameIndex: title/alias → coordinate, exact and case-folded
├── sentron.rs         — 2×4×5×8 neural topology
├── stats.rs           — Density, sparklines, lattice overview
├── cursor.rs          — Sub-scroll cursor with word navigation
├── editor.rs          — Pluggable editor mode trait
├── modes/             — Vim-style editor mode
├── undo.rs            — Undo/redo engine with phext serialization
└── tts.rs             — 3×5×17+1 coordinate pronunciation system

web/
├── phext-tts.js       — Browser TTS with Web Speech API
└── tts-demo.html      — Interactive pronunciation demo
```

### Key Ideas

**Memory-mapped I/O**: The phext file is mmap'd read-only. Scroll content is only materialized when navigated to. A 4 MB phext uses ~4 MB of virtual address space, not heap.

**O(1) index**: One linear scan on load builds a `HashMap<Coordinate, ScrollSpan>`. Every subsequent lookup is a hash lookup, not a linear scan. This complements libphext's `explode()` function which also returns `HashMap<Coordinate, String>`.

**Copy-on-write overlay**: Edits go into an in-memory `HashMap<Coordinate, String>`. The mmap is never modified. On save, the phext is reconstructed from mmap spans + overlay, re-mapped, and the overlay clears. Byte-perfect roundtrip.

**Ranked search and names**: `search.rs` finds exact text by scanning. `TermIndex` is an inverted index with BM25 ranking for "which scrolls are most about these words". Scrolls can be grouped into documents (e.g. a record's title + body), and `upsert`/`remove` keep it current after edits. `NameIndex` maps the lines of chosen scrolls (titles, aliases) to coordinates, with a case-folded fallback, so `resolve("ferrel cell")` finds "Ferrel cell".

```rust
use phext_lattice::{MappedLattice, NameIndex, TermIndex};

let lat = MappedLattice::open("wiki.phext")?;
let names = NameIndex::from_scrolls(&lat, |c| (c.x.scroll == 1).then_some(*c));
let terms = TermIndex::build(&lat);
let page = names.resolve("ferrel cell");
let hits = terms.search("glacier instability sea level", 10); // Vec<TermHit { coordinate, score }>
```

**Navigator**: The 9D cursor model. Tracks position, active dimension, marks (bookmarks), and full navigation history with back/forward. The `next_populated` / `prev_populated` operations use binary search on the sorted coordinate list.

**TTS Pronunciation**: The 3×5×17+1 system converts coordinates to speakable syllables. Zero = "om" (silence). Values 1-255 map to (onset × vowel × coda) syllables. See `web/tts-demo.html` for an interactive demo.

## Dependencies

- [libphext](https://crates.io/crates/libphext) v0.3.1 — Coordinate types, delimiter constants, phokenize/dephokenize, explode/implode
- [memmap2](https://crates.io/crates/memmap2) — Cross-platform memory-mapped file I/O

## libphext v0.3.1 Features

The following features are now available in libphext v0.3.1:

- **`Hash` + `Eq` + `Ord`** derives on `Coordinate`, `ZCoordinate`, `YCoordinate`, `XCoordinate` — enables HashMap/BTreeMap keys and sorting
- **`Copy` + `Clone`** derives — coordinates are now copyable without explicit `.clone()`
- **`TryFrom<&str>`** for Coordinate — enables `Coordinate::try_from("1.1.1/1.1.1/1.1.1")`
- **`explode()`** — returns `HashMap<Coordinate, String>` for O(1) content lookup
- **`implode()`** — reconstructs phext from HashMap

### Suggested Future Improvements

1. **Dimension-aware navigation** — the `*_break()` methods advance + reset lower dims (correct for parsing), but there's no way to:
   - Move *backward* in a dimension
   - Query which dimension a delimiter byte belongs to
   - Get/set a specific dimension's value without knowing the struct field name

2. **`FromStr` impl** — `TryFrom<&str>` exists but `FromStr` would enable `"1.1.1/1.1.1/1.1.1".parse::<Coordinate>()?` with the `?` operator.

## Binaries

```bash
# Web editor server (default feature)
cargo build --bin phext-edit

# TUI navigator (requires --features tui)
cargo build --bin phext-nav --features tui

# TTS pronunciation tool
cargo build --bin phext-tts
```

### phext-tts Usage

```bash
# Pronounce a coordinate
phext-tts 1.1.1/1.1.1/1.1.1
# → Syllables: a a a om a a a om a a a

# Generate SSML for external TTS
phext-tts ssml 3.1.4/1.5.9/2.6.5 slow

# Extract & pronounce all coords from a phext
phext-tts extract file.phext > pronunciation.md

# Show special coordinates (origin, pi, boundary, etc.)
phext-tts special
```

## License

MIT
