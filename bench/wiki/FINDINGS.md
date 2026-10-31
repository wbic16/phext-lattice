# wikibench findings: phext as an offline Wikipedia

Numbers are from [`RESULTS.md`](RESULTS.md) (scale 1 = 3,820 real articles / 4.1 MB; scale 10 =
38,200 articles / 40.8 MB). "Best conventional" means the best of files, SQLite, and ZIM.

## Summary

| dimension | phext today | best conventional | verdict |
|---|---|---|---|
| warm lookup | **1.1 µs** by coordinate, 3.1 µs by title (phext-lattice; binary search since the span index became sorted arrays, was 0.7 / 2.1 µs with hashing) | 4.0 µs SQLite | phext wins |
| cold open | **36 ms + 9.7 MB RSS** with the `IndexedLattice` sidecar; 166 ms / 143 MB rebuilding indexes in memory | 2.9 ms / 1.4 MB SQLite | much closer; the span table is still parsed on open, so it grows with scroll count |
| storage | 1.20× payload, 1 inode | 0.63× ZIM (zstd clusters) | ZIM wins; phext beats SQLite (1.52×) and files (7.6×) |
| link traversal | **43 µs** 2-hop BFS (phext-lattice) | 84 µs SQLite | phext wins |
| backlinks | 7.9 ms (full scan) | 2.2 µs SQLite | phext loses: no reverse index |
| edit one article | 163 ms at 49 MB (rewrites the whole file); the sidecar delta adds ~21 ms | 0.3 ms SQLite / files | phext loses, and it gets worse as the file grows |
| add one article | **0.16 ms** raw append; reopening re-indexes in 86 ms (sidecar resync) or 35 ms (span scan only) | 0.18 ms SQLite | the format wins; the tooling can't use it yet |
| retrieval, vague queries | **R@1 0.338, 4.0 KB read, 38 µs** (`TermIndex`); grep-style search was 0.174 / 17 KB / 24 ms | R@1 0.328, 4.2 KB, 571 µs (FTS5) | phext wins since `TermIndex` |
| retrieval, case noise | **100%, 0.6 µs** (`NameIndex::resolve`); exact-only was 0% | 100%, 331 µs SQLite NOCASE | phext wins since `NameIndex` |

The format holds up well. Delimiters are 0.3% of the file. Record-per-section coordinates make
lookup and traversal hash lookups, and a new article is an O(1) append. Every loss is a
**tooling** gap. Closed so far: ranked search (`TermIndex`), case-folded names
(`NameIndex`), and persisted indexes (`IndexedLattice`, a parallel-phext sidecar with a
per-save delta, so open no longer rebuilds anything and a save touches only the edited
scrolls' index entries). Still open: a reverse-link index, and a source write path that
touches less than the whole file. Each remaining gap has a measured baseline that shows the
target can be reached.

## Tooling scorecard

Each row is one metric that `run_all.sh` reproduces, with today's value at scale 10 and a target
taken from the best conventional store or from a prototype in the harness.

| # | metric (key in JSON) | repo | today | target | how |
|---|---|---|---|---|---|
| 1 | open → first article, `sidecar.open_first_ms` | phext-lattice | ✅ 36 ms cold, 12 ms warm with `IndexedLattice` (in-memory rebuild: 166 ms) | ≤ 10 ms, flat in file size | most of what remains is parsing the 135k-row span table; binary-search it in the mmap instead |
| 2 | RSS after open, `sidecar.rss_kb` | phext-lattice | ✅ 9.7 MB ≈ 73 B/scroll with `IndexedLattice` (in-memory rebuild: 143 MB) | ≤ 32 B/scroll of heap | `LatticeIndex` is now sorted arrays; pack the 9 dimensions (10 bits each) instead of 9 `usize`s |
| 3 | index build throughput, `index_rebuild_warm_ms` | phext-lattice | 37 ms / 49 MB ≈ 1.3 GB/s | ≥ 4 GB/s | skip runs of bytes ≥ 0x20 (memchr-style) instead of a `match` per byte |
| 4 | durable single edit, `updates.phext_lattice_save_each` | phext-lattice | 163 ms (`save()` no longer rescans; was 237 ms); sidecar delta adds ~21 ms (`sidecar.edit_save_sync`) | ≤ 1 ms, flat | append-only edit journal (a phext of changed scrolls, fsync) replayed into the overlay; compact in the background |
| 5 | append then readable, `sidecar.resync_after_append_ms` | phext-lattice | 86 ms (scan + hash compare, recorded into the delta) | ≤ 5 ms | when the source only grew, check the old length's prefix and index just the tail |
| 6 | backlinks, `backlinks.phext_lattice_scan` | phext-lattice | 7.9 ms | ≤ 10 µs | reverse-link map built when link scrolls are indexed (SQLite: 2.2 µs) |
| 7 | vague-query R@1 / bytes read, `retrieval.keywords_hard` | phext-lattice | ✅ **0.338 / 4,011 B** with `TermIndex` and the sidecar (was 0.174 / 17,169 B) | ≥ 0.33 / ≤ 4.2 KB | done; FTS5 is 0.328 / 4,169 B |
| 8 | ranked search latency, `phext_lattice_term_index`, `sidecar.keywords*` | phext-lattice | ✅ 8–38 µs in memory, 63–133 µs from the sidecar mmap (grep-style was 24–38 ms; FTS5 265–562 µs) | ≤ 1 ms | done. Sidecar queries decode text postings per query; a binary postings scroll or a hot-term cache would close the gap to in-memory |
| 9 | case-insensitive name hit rate, `retrieval.alias_lower` | phext-lattice | ✅ 100% at 0.6 µs with `NameIndex::resolve` (was 0%) | 100% | done; the folded map costs ≈ 1.5× the exact-only name map's build time (row 1) |
| 10 | `fetch` p50, `lookup_warm.phext_libphext_fetch` | libphext-rs | 12.8 ms at 49 MB (linear) | ≤ 5 µs | a span index type in libphext (move `LatticeIndex` upstream), `fetch_indexed()` |
| 11 | `replace` + write, `phext_libphext_replace_rewrite` | libphext-rs | 124 ms at 49 MB | O(scroll) | splice API returning (offset, old_len, new bytes) so callers can patch in place or journal |
| 12 | HTTP select p50, `lookup_warm.sq_http_by_coord` | SQ | 86 µs (one TCP connection per request) | ≤ 25 µs | HTTP keep-alive; per-request overhead is ~100× the lookup |
| 13 | HTTP update, `updates.sq_http_update` | SQ | 72 ms at 49 MB, **not fsynced** | ≤ 1 ms, durable | `implode_ref` + `fs::write` of the whole phext on every mutation: journal + fsync (see #4) |
| 14 | 2-hop traversal over HTTP, `traversal_2hop.sq_http_query_api` | SQ | 778 µs (18× phext-lattice) | ≤ 100 µs | server-side expand (`/api/v2/query?links=<coord>&depth=2`), so a BFS is one request |
| 15 | cold start, `open.sq_http.open_first_ms` | SQ | 171 ms (explode into `HashMap<Coordinate, String>`) | ≤ 20 ms | host on `MappedLattice` instead of `explode` |
| 17 | sidecar size, `sidecar.bytes` | phext-lattice | 2.5× the source (parallel 1.05×, tables 1.45×) | ≤ 1× | names tables repeat two 35-byte coordinates per row and per-scroll term vectors are plain text; pack coordinates, skip folded rows equal to the exact name |
| 18 | sidecar build / compaction, `sidecar.build_ms`, `compact_ms` | phext-lattice | 2.5 s once; 367 ms per compaction at 49 MB | ≤ 1 s / ≤ 200 ms | both rewrite ~120 MB and fsync; build is single-threaded after tokenizing |
| 16 | storage, `storage.json` | format | 1.20× payload | ≤ 0.65× with random access | seekable compressed phext: one zstd frame per chapter (≈ ZIM cluster) + span index; whole-file zstd-19 is 0.29× but sequential-only |

## What each comparison showed

**Lookup latency.** Indexed phext is the fastest store measured. Lookup by coordinate is
0.6–1.1 µs, and by title through the name map 1.3–3.1 µs, against 2.9–4.6 µs for files and
SQLite. All of that speed depends on having an index. Without one, `libphext::fetch`
is a linear scan: 1.5 ms at 4 MB and 13 ms at 49 MB. With the `IndexedLattice` sidecar the
index is loaded instead of rebuilt: cold open drops from 166 ms to 36 ms at 49 MB, and title
lookups binary-search the mmap'd name table (5.9 µs) instead of a heap map (3.1 µs). SQ adds about 80 µs of HTTP per call,
mostly from the TCP connection it sets up for every request.

**Storage overhead.** Phext structure is close to free: 13,892 delimiter bytes for 13,522 scrolls.
The 18–20% overhead is titles, link coordinates (17–20 bytes of text each), and aliases. Files
cost 7.5× on disk because of 4 KB blocks and 24k inodes (242k at scale 10). ZIM is the only store
that compresses while staying random-access. A whole-file zstd'd phext is smaller than ZIM
(1.18 MB against 2.49 MB at scale 1) but can only be read sequentially. Ignore the zstd column at
scale 10: the replicas are near-duplicates and compress unrealistically well.

**Link traversal.** Storing links as coordinates pays off. phext-lattice runs a 2-hop BFS over
about 40 nodes in 43 µs, 2× faster than SQLite and 7–25× faster than ZIM (through Python). Reverse traversal is the
opposite: phext has no backlink index, so "what links here" scans every link scroll.

**Persisted indexes.** `IndexedLattice` keeps a sidecar directory of phexts beside the
source: `parallel.phext`, where the scroll at coordinate C is source scroll C's index entry
(hash, document, token count, names, term frequencies), and `tables.phext`, which holds sorted,
mmap-searchable span, name, document, and postings tables. A `save()` writes only `delta.phext`,
parallel again: one entry per scroll changed since the base, with its old and new entries and
its span correction. Opening applies the delta while parsing the span table, and every query
merges base rows with the delta. Tests check that the results equal a fresh rebuild, including
BM25 scores. When another tool rewrites or appends to the source, reopening compares every
scroll's hash with its base entry (86–89 ms at 49 MB) and records only the differences. The cost
is disk: the sidecar is 2.5× the source.

**Incremental updates.** These show the clearest split between the format and the tooling.
Appending a new article to a record-per-section phext is a single `O_APPEND` write plus fsync
(0.16 ms, faster than SQLite). Every tool, though, rewrites or re-indexes the whole file:
`MappedLattice::save()` takes 237 ms, `libphext::replace` + write 124 ms, and SQ `update` 72 ms
without fsync. All of these grow linearly with file size. ZIM is read-only, so any edit is a full
rebuild (21 s at scale 10).

**Agent retrieval quality.** On exact entity lookup every store scores 100%, and phext's
`NameIndex` is the fastest at 0.1–0.5 µs. With lower-cased input, exact-only maps (files, ZIM's
title index, phext before `NameIndex`) find nothing; `NameIndex::resolve` and SQLite (NOCASE)
find 100%, at 0.6 µs and 331 µs. In this libzim build, ZIM's suggestion search did not fold case either. On specific
keyword queries every method reaches R@10 ≈ 1.0. On vague queries ranking decides the outcome.
Grep-style search over phext (rank by the number of terms matched) finds the right article first
17% of the time, and the agent reads 17 KB to reach it. `TermIndex` (BM25 over the same scrolls)
finds it first 34% of the time with 4 KB read: 4× less context for the agent and about 600× lower
latency. It edges out FTS5 (33%, 571 µs) and Xapian (27%, 146 µs) at a fraction of their latency.

## Bug found while benchmarking

`phext-lattice` case-insensitive search panicked (`byte index out of bounds`) on real Wikipedia
text. It found matches in a `to_lowercase()` copy and then used those byte offsets to slice the
original scroll. Lowercasing changes byte lengths outside ASCII (`İ` → `i̇`, `K` (Kelvin) → `k`),
so the offsets no longer lined up. The fix in this branch keeps a lowered→original offset map
for non-ASCII scrolls and an allocation-free ASCII fast path. A regression test covers it.
