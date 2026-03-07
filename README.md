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

## Architecture

```
lattice-core/
├── coordinate_ext.rs  — Dimension enum, dimensional navigation, CoordinateNav trait
├── index.rs           — LatticeIndex: sparse coordinate → byte-span HashMap, O(1) lookup
├── mmap.rs            — MappedLattice: memory-mapped phext with copy-on-write overlay
└── navigator.rs       — Navigator: 9D cursor model with history, marks, populated-jump
```

### Key Ideas

**Memory-mapped I/O**: The phext file is mmap'd read-only. Scroll content is only materialized when navigated to. A 4 MB phext uses ~4 MB of virtual address space, not heap.

**O(1) index**: One linear scan on load builds a `HashMap<Coordinate, ScrollSpan>`. Every subsequent lookup is a hash lookup, not a linear scan. This is the critical improvement over libphext-rs's `get_subspace_coordinates` which is O(n) per call.

**Copy-on-write overlay**: Edits go into an in-memory `HashMap<Coordinate, String>`. The mmap is never modified. On save, the phext is reconstructed from mmap spans + overlay, re-mapped, and the overlay clears. Byte-perfect roundtrip.

**Navigator**: The 9D cursor model. Tracks position, active dimension, marks (bookmarks), and full navigation history with back/forward. The `next_populated` / `prev_populated` operations use binary search on the sorted coordinate list.

## Dependencies

- [libphext-rs](https://github.com/wbic16/libphext-rs) — Coordinate types, delimiter constants, phokenize/dephokenize
- [memmap2](https://crates.io/crates/memmap2) — Cross-platform memory-mapped file I/O

## Upstream Improvements to libphext-rs

This project identified and applied the following improvements to libphext-rs:

1. **`Hash` + `Eq` derives** on `ZCoordinate`, `YCoordinate`, `XCoordinate`, and `Coordinate`. Without these, coordinates can't be used as HashMap keys, which blocks O(1) indexing.

### Suggested Future Improvements

2. **`Ord` derive** on Coordinate — currently only `PartialOrd`. Full `Ord` would enable `BTreeMap` indexing and `.sort()` without custom comparators.

3. **Dimension-aware navigation** — the `*_break()` methods advance + reset lower dims (correct for parsing), but there's no way to:
   - Move *backward* in a dimension
   - Query which dimension a delimiter byte belongs to
   - Get/set a specific dimension's value without knowing the struct field name

4. **Byte-offset index** — `get_subspace_coordinates` does a linear scan every time. An `index()` function exists but returns a phext string, not a usable data structure. A `HashMap<Coordinate, (usize, usize)>` built once and reused would be a major performance win for any application that does multiple lookups.

5. **`FromStr` impl** — `to_coordinate` is a free function. A `FromStr` impl would enable `"1.1.1/1.1.1/1.1.1".parse::<Coordinate>()`.

## License

MIT
