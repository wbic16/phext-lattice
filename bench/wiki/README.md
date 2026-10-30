# wikibench — offline Wikipedia as phext

How does a phext hold up as an offline Wikipedia next to the formats people actually ship
one in — a directory of files, SQLite, and Kiwix ZIM — and which phext tooling gaps does
that expose? This harness measures five things for each store, from the same article bytes:

| metric | what is measured |
|---|---|
| **lookup latency** | cold open → first article (page cache dropped), then warm title → text p50/p99 |
| **storage overhead** | bytes on disk / raw article payload, inode count, zstd-19 size of single-file stores |
| **link traversal** | 2-hop BFS from 200 seeds reading every node it reaches; "what links here" backlinks |
| **incremental updates** | durable edit of one article; adding one new article |
| **agent retrieval quality** | alias lookup (exact + lower-cased) and keyword search: R@1, R@10, MRR@10, bytes an agent must read |

Results: [`RESULTS.md`](RESULTS.md). Findings and the tooling scorecard: [`FINDINGS.md`](FINDINGS.md).

## Running

```bash
pip install libzim zstandard
(cd ../SQ && cargo build --release)              # SQ_BIN overrides the path
bench/wiki/run_all.sh /tmp/wikibench 1 10        # work dir, then scales
```

`run_all.sh` clones `openzim/zim-testing-suite` for its real Kiwix Wikipedia ZIM
(`wikipedia_en_climate_change_mini_2024-06`: 3,820 articles, 16,566 redirects), extracts it,
builds every store, runs both harnesses, and rewrites `RESULTS.md`. Run as root to get
cold-cache open numbers (`/proc/sys/vm/drop_caches`); otherwise `cold_cache: false` is recorded.

## Pieces

| file | role |
|---|---|
| `extract_zim.py` | ZIM → `articles.jsonl` (id, title, plain text, resolved outbound links) + `redirects.jsonl` |
| `make_queries.py` | deterministic query set: 500 each of `alias`, `alias_lower`, `keywords`, `keywords_hard` |
| `../../examples/wikibench.rs` | builds files / SQLite / phext; measures files, SQLite, libphext, phext-lattice, SQ over HTTP |
| `zim_bench.py` | builds a text ZIM from the same corpus with libzim + Xapian; measures ZIM, plus Python calibration rows |
| `storage.py` | on-disk accounting |
| `report.py` | JSON → `RESULTS.md` |
| `results/` | the raw JSON behind `RESULTS.md` (scales 1 and 10) |

## Store layouts

All stores hold byte-identical article text; links and aliases are stored the way each
format would naturally hold them.

- **files** — `a/<title>` holds the text, `l/<title>` the linked titles, and each redirect is a
  symlink `a/<alias> → <title>`. Title lookup is a directory lookup.
- **SQLite** — `articles(id, title UNIQUE, body)`, `links(src, dst)` WITHOUT ROWID with a
  `(dst, src)` index, `redirects(alias, target)`, FTS5 external-content table kept in sync by
  triggers. WAL with `synchronous=FULL`. `wiki-nofts.sqlite` is the same without FTS5.
- **ZIM** — one `text/plain` item per article (Xapian-indexed, front article), a hidden
  `_links/<title>` item per article, libzim redirects for aliases. zstd clusters, libzim defaults.
- **phext** — one *record per section*. Article *n* lives at
  `1.1.1/1.1.(n/999²+1)/((n/999)%999+1).(n%999+1).f` with fields
  `f = 1` title, `2` text, `3` outbound links (one coordinate per line), `4` aliases.
  Because records are contiguous and in coordinate order, adding an article is an append at EOF.
  There is no persisted name index; the title/alias → record map is rebuilt from fields 1 and 4
  on open, and that time is charged to "open".

## Access paths measured for phext

| path | repo | how |
|---|---|---|
| `libphext::fetch` | libphext-rs | linear scan of the whole buffer per lookup (no index) |
| `libphext::explode` | libphext-rs | parse everything into `HashMap<Coordinate, String>` once, then hash lookups |
| `MappedLattice` | phext-lattice | mmap + `LatticeIndex` (coordinate → byte span), copy-on-write overlay, `save()` |
| `search_lattice_auto` | phext-lattice | parallel case-insensitive substring search, used grep-style for retrieval |
| `sq host` | SQ | HTTP `select`, `update`, and the v0.6 Coordinate Query API (`/api/v2/query` with a set) |
| `NameIndex` | phext-lattice | title/alias scrolls → record, exact `get` and case-folded `resolve` |
| `TermIndex` | phext-lattice | BM25 inverted index over title + body scrolls, grouped per record |

The benchmark charges `NameIndex` to phext-lattice's open time; `TermIndex` build time is reported separately.

## Caveats

- The corpus is small (4 MB of lead-section text). Scale N>1 replicates it with suffixed
  titles, which is fine for latency/size scaling but not for retrieval, so retrieval runs at scale 1 only.
- ZIM is measured through python-libzim. The `calibration: python sqlite3` row is the same SQLite
  query as the Rust row, so the gap between them is the Python overhead to subtract mentally (a few µs).
- Queries are generated from the corpus, not written by people, and no LLM is in the loop:
  "agent retrieval quality" here means what an agent's search/lookup tool returns and how many
  bytes the agent must read to reach the right article.
- One machine, one run per scale (4 vCPU cloud VM, ext4-like overlay). Treat differences under ~20% as noise.
