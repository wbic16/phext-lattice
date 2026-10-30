# wikibench findings: phext as an offline Wikipedia

Numbers are from [`RESULTS.md`](RESULTS.md) (scale 1 = 3,820 real articles / 4.1 MB; scale 10 =
38,200 articles / 40.8 MB). "Best conventional" means the best of files, SQLite, and ZIM.

## Summary

| dimension | phext today | best conventional | verdict |
|---|---|---|---|
| warm lookup | **0.7 µs** by coordinate, 2.1 µs by title (phext-lattice) | 4.1 µs SQLite | phext wins |
| cold open | 191 ms + 175 MB RSS at 49 MB (index and `NameIndex` rebuilt each open) | 3.9 ms / 1.3 MB SQLite | phext loses, and it gets worse as the file grows |
| storage | 1.20× payload, 1 inode | 0.63× ZIM (zstd clusters) | ZIM wins; phext beats SQLite (1.52×) and files (7.6×) |
| link traversal | **43 µs** 2-hop BFS (phext-lattice) | 84 µs SQLite | phext wins |
| backlinks | 7.9 ms (full scan) | 2.2 µs SQLite | phext loses: no reverse index |
| edit one article | 124–237 ms at 49 MB (rewrites the whole file) | 0.3 ms SQLite / files | phext loses, and it gets worse as the file grows |
| add one article | **0.16 ms** raw append, but 57 ms to re-index | 0.18 ms SQLite | the format wins; the tooling can't use it yet |
| retrieval, vague queries | **R@1 0.338, 4.0 KB read, 38 µs** (`TermIndex`); grep-style search was 0.174 / 17 KB / 24 ms | R@1 0.328, 4.2 KB, 571 µs (FTS5) | phext wins since `TermIndex` |
| retrieval, case noise | **100%, 0.6 µs** (`NameIndex::resolve`); exact-only was 0% | 100%, 331 µs SQLite NOCASE | phext wins since `NameIndex` |

The format holds up well. Delimiters are 0.3% of the file. Record-per-section coordinates make
lookup and traversal hash lookups, and a new article is an O(1) append. Every loss is a
**tooling** gap: phext has no persisted index, no reverse index, and no write path that
touches less than the whole file. The ranked term index and case folding gaps are closed:
`phext_lattice::TermIndex` (BM25) and `phext_lattice::NameIndex` (exact + case-folded names)
now ship in phext-lattice, and the benchmark uses them. Each remaining gap has a measured
baseline that shows the target can be reached.

## Tooling scorecard

Each row is one metric that `run_all.sh` reproduces, with today's value at scale 10 and a target
taken from the best conventional store or from a prototype in the harness.

| # | metric (key in JSON) | repo | today | target | how |
|---|---|---|---|---|---|
| 1 | open → first article, `open.phext_lattice.open_first_ms` | phext-lattice | 191 ms (index 63, `NameIndex` 128) | ≤ 10 ms, flat in file size | persist `LatticeIndex` + name map as a sidecar keyed by file checksum, or mmap a sorted span table |
| 2 | RSS after open, `open.phext_lattice.rss_kb` | phext-lattice | 175 MB: 49 MB of touched mmap pages + ≈ 920 B/scroll of heap (span index + exact and folded name maps) | ≤ 32 B/scroll of heap | sorted `Vec<(packed coord, u32 start, u32 len)>` + binary search instead of `HashMap<Coordinate, ScrollSpan>` + `Vec<Coordinate>` |
| 3 | index build throughput, `index_rebuild_warm_ms` | phext-lattice | 56 ms / 49 MB ≈ 0.9 GB/s | ≥ 4 GB/s | skip runs of bytes ≥ 0x20 (memchr-style) instead of a `match` per byte |
| 4 | durable single edit, `updates.phext_lattice_save_each` | phext-lattice | 237 ms (14× scale 1) | ≤ 1 ms, flat | append-only edit journal (a phext of changed scrolls, fsync) replayed into the overlay; compact in the background |
| 5 | append then readable, `phext_lattice_reopen_after_append_ms` | phext-lattice | 57 ms | ≤ 1 ms | `MappedLattice::refresh()` that remaps and indexes only the new tail |
| 6 | backlinks, `backlinks.phext_lattice_scan` | phext-lattice | 7.9 ms | ≤ 10 µs | reverse-link map built when link scrolls are indexed (SQLite: 2.2 µs) |
| 7 | vague-query R@1 / bytes read, `retrieval.keywords_hard` | phext-lattice | ✅ **0.338 / 4,011 B** with `TermIndex` (was 0.174 / 17,169 B) | ≥ 0.33 / ≤ 4.2 KB | done; FTS5 is 0.328 / 4,169 B. Next: persist the index so its 71 ms / 7.6 MB build (4 MB corpus) leaves open |
| 8 | ranked search latency, `phext_lattice_term_index` | phext-lattice | ✅ 8.5–38 µs/query (grep-style was 24–38 ms) | ≤ 1 ms | done. Exact-text `search_lattice` still re-lowercases every scroll per term |
| 9 | case-insensitive name hit rate, `retrieval.alias_lower` | phext-lattice | ✅ 100% at 0.6 µs with `NameIndex::resolve` (was 0%) | 100% | done; the folded map costs ≈ 1.5× the exact-only name map's build time (row 1) |
| 10 | `fetch` p50, `lookup_warm.phext_libphext_fetch` | libphext-rs | 12.8 ms at 49 MB (linear) | ≤ 5 µs | a span index type in libphext (move `LatticeIndex` upstream), `fetch_indexed()` |
| 11 | `replace` + write, `phext_libphext_replace_rewrite` | libphext-rs | 124 ms at 49 MB | O(scroll) | splice API returning (offset, old_len, new bytes) so callers can patch in place or journal |
| 12 | HTTP select p50, `lookup_warm.sq_http_by_coord` | SQ | 86 µs (one TCP connection per request) | ≤ 25 µs | HTTP keep-alive; per-request overhead is ~100× the lookup |
| 13 | HTTP update, `updates.sq_http_update` | SQ | 72 ms at 49 MB, **not fsynced** | ≤ 1 ms, durable | `implode_ref` + `fs::write` of the whole phext on every mutation: journal + fsync (see #4) |
| 14 | 2-hop traversal over HTTP, `traversal_2hop.sq_http_query_api` | SQ | 778 µs (18× phext-lattice) | ≤ 100 µs | server-side expand (`/api/v2/query?links=<coord>&depth=2`), so a BFS is one request |
| 15 | cold start, `open.sq_http.open_first_ms` | SQ | 171 ms (explode into `HashMap<Coordinate, String>`) | ≤ 20 ms | host on `MappedLattice` instead of `explode` |
| 16 | storage, `storage.json` | format | 1.20× payload | ≤ 0.65× with random access | seekable compressed phext: one zstd frame per chapter (≈ ZIM cluster) + span index; whole-file zstd-19 is 0.29× but sequential-only |

## What each comparison showed

**Lookup latency.** Indexed phext is the fastest store measured. Lookup by coordinate is
0.5–0.7 µs, and by title through the name map 1.0–2.1 µs, against 2.9–4.6 µs for files and
SQLite. All of that speed depends on the index being in memory. Without it, `libphext::fetch`
is a linear scan: 1.5 ms at 4 MB and 12.8 ms at 49 MB. SQ adds about 80 µs of HTTP per call,
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
