# phext-lattice

The foundation layer for phext-fluent editing. Memory-mapped 9D lattice with O(log n) coordinate lookup and persisted indexes.

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
├── index.rs           — LatticeIndex: sorted coordinate → byte-span table, binary search
├── mmap.rs            — MappedLattice: memory-mapped phext with copy-on-write overlay
├── navigator.rs       — Navigator: 9D cursor model with history, marks, populated-jump
├── search.rs          — Parallel search with early termination
├── term_index.rs      — TermIndex: BM25 ranked full-text index over scrolls
├── names.rs           — NameIndex: title/alias → coordinate, exact and case-folded
├── sidecar.rs         — IndexedLattice: spans, names, BM25 persisted in a parallel-phext sidecar
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

**Span index**: One linear scan on load (or a persisted span table, see below) builds a sorted coordinate → byte-span table. Every subsequent lookup is a binary search, not a linear scan, and the table costs 88 bytes per scroll. This complements libphext's `explode()` function which also returns `HashMap<Coordinate, String>`.

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

**Persisted indexes**: `IndexedLattice` keeps the span index, `NameIndex`, and `TermIndex` in a sidecar directory (`wiki.phext` → `wiki.phext.idx/`), so open loads them instead of rebuilding them. Everything in it is phext. `parallel.phext` mirrors the source coordinate for coordinate: scroll C holds source scroll C's index entry (hash, names, term frequencies). `tables.phext` holds sorted span, name, document, and postings tables that are binary-searched straight from the mmap. `save()` writes only `delta.phext`, again parallel: one entry for each scroll changed since the base. Once the delta passes a threshold it is compacted into the base. If another tool changes the source, the next open compares scroll hashes and records only the scrolls that differ.

```rust
use phext_lattice::{IndexSpec, IndexedLattice, OpenMode};

// record per section: scroll 1 = title, scroll 2 = body
let spec = IndexSpec::new("wiki-v1",
    Box::new(|c| (c.x.scroll == 1).then_some(*c)),                               // names
    Box::new(|c| (c.x.scroll <= 2).then(|| { let mut d = *c; d.x.scroll = 1; d })) // term docs
);
let (mut wiki, mode) = IndexedLattice::open("wiki.phext", spec)?; // Built, Loaded { delta }, or Resynced { changed }
let page = wiki.resolve("ferrel cell");
let hits = wiki.search("glacier instability", 10);
wiki.lattice_mut().write_scroll(page.unwrap(), "Ferrel cell".into());
wiki.save()?; // source rewrite + delta for the edited scroll only
```

On the 49 MB wikibench phext: cold open 36 ms / 9.7 MB RSS (166 ms / 143 MB rebuilding in memory), save adds ~21 ms over the source rewrite, and the sidecar is 2.5× the source.

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

### Local development

Run the editor on your machine without Google or GitHub sign-in.

Linux and macOS:

```bash
cargo run --bin phext-edit -- --local
```

Windows Command Prompt, from the checkout (for example `c:\source\phext-lattice`):

```bat
cd /d c:\source\phext-lattice
set PHEXT_DATA_DIR=
set PHEXT_HOME=
cargo run --bin phext-edit -- --local
```

Windows PowerShell:

```powershell
Set-Location c:\source\phext-lattice
Remove-Item Env:PHEXT_DATA_DIR -ErrorAction SilentlyContinue
Remove-Item Env:PHEXT_HOME -ErrorAction SilentlyContinue
cargo run --bin phext-edit -- --local
```

If PowerShell swallows `--local`, quote the flag: `cargo run --bin phext-edit -- "--local"`.

`--local` listens on `127.0.0.1:8080` and admits every direct request as a local developer, so the editor loads straight into the lattice and `/api/*` answers without a session. Omit the flag and the previous auth gate is unchanged.

The flag is refused unless the process binds to a loopback address (`127.0.0.1`, `::1`, or `localhost`). `--local --host 0.0.0.0` does not start. A request that arrived through a proxy (`X-Forwarded-For`, `X-Real-IP`, `Forwarded`, or a `Host` that is not loopback) is not treated as local. No environment variable and no request header can turn the bypass on.

Phext files for that run come from `--dir` when you pass it, otherwise from `PHEXT_DATA_DIR`, otherwise from `.phext-local` in the current directory (`.\.phext-local` on Windows). That folder is created on first run, with a `welcome.phext` when it does not already contain a phext file. You do not need to create the directory yourself. It is separate from the production library, so a local session does not edit deployed phexts. `PHEXT_HOME` is ignored under `--local` unless you pass `--home`. On Windows, set `PHEXT_DATA_DIR` only to a Windows path such as `C:\source\phexts`. `/var/lib/phext-edit` is not the default.

If startup cannot create or open a file, the process exits with a line that names the operation and the full path, for example `error: open C:\source\phext-lattice\.phext-local\welcome.phext: The system cannot find the file specified. (os error 2)`. Copy that `error:` line if it still fails. Clearing `PHEXT_DATA_DIR` and `PHEXT_HOME` (the commands above) avoids a leftover Unix path from another machine.

### Deployment

Production runs on the host under systemd via
[`deploy/deploy.sh`](deploy/deploy.md). The same binary also ships
as a container image — Docker Compose, non-root, environment-only
configuration — documented in [deploy/CONTAINER.md](deploy/CONTAINER.md).

### Submissions: upload for review

phext-edit's editor is gated by an editors list, which used to make the
operator the only door in. Submissions open a second door at
[`/submit`](https://edit.phext.io/submit):

1. Any **signed-in** user (Google or GitHub — editor or not) uploads a
   `.phext` (10 MiB cap). The name is reduced to a bare `*.phext` name,
   the text is parsed to count its scrolls, and the upload waits as a
   *pending* submission outside the library.
2. Any **approved editor** sees the review queue on the same page and can
   download, accept, or decline each submission.
3. **Accepting** curates the phext into the server's phext library (on a
   name collision it is suffixed with part of the submission id) and
   **admits the originator as an editor** — their account id *and* their
   email are added to the editor list, effective immediately, with no
   operator action. Declining records the decision and admits nobody.

Endpoints (all JSON, session cookie auth; writes need the session's
`x-csrf-token`, as elsewhere):

| Route | Who | Effect |
|-------|-----|--------|
| `GET /submit` | anyone | the upload / review page |
| `POST /api/submissions` | signed-in | upload; phext bytes as the body, file name in `x-filename` |
| `GET /api/submissions` | signed-in | editors see all; others see only their own |
| `GET /api/submissions/{id}/download` | originator or editor | the uploaded phext |
| `POST /api/submissions/{id}/accept` | editors | curate + admit the originator |
| `POST /api/submissions/{id}/decline` | editors | mark declined |

Pending uploads live in a `submissions/` subdirectory of the phext
directory (`--dir`); the library listing scans only the directory
itself, so a pending upload is never listed as a library file. With no
`--dir`, submissions fall back to a `submissions/` directory under the
local data dir (`PHEXT_DATA_DIR`, else `./.phext-local`).

Admission persists in an **editors file**, one entry per line:

- `PHEXT_EDITORS_FILE` names the file when set.
- Otherwise phext-edit defaults to `admitted-editors.txt` inside the
  phext directory (`--dir`), or next to the initial phext file.

`PHEXT_EDITORS` remains the bootstrap list: the file's entries are
loaded in addition to it, and new admissions are appended to the file.
An accepted originator is an editor from their next request onward, and
still is after a restart.

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
