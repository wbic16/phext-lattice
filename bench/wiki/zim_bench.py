#!/usr/bin/env python3
"""ZIM side of the offline-Wikipedia benchmark (python-libzim, i.e. C++ libzim + Xapian).

Builds a text ZIM from the same corpus the Rust harness uses (same payload bytes, same
scale-replication rule), then measures open, lookup, traversal, alias resolution,
full-text retrieval, and rebuild cost. Also times Python sqlite3/file reads against the
Rust-built stores so Python-vs-Rust call overhead can be read off directly.

usage: zim_bench.py <corpus_dir> <out_dir> [scale] [--original <distributed.zim>]
"""
import json, os, sqlite3, sys, time
from libzim.reader import Archive
from libzim.search import Query, Searcher
from libzim.suggestion import SuggestionSearcher
from libzim.writer import Creator, Hint, IndexData, Item, StringProvider

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))


def load_corpus(d, scale):
    base = [json.loads(l) for l in open(os.path.join(d, "articles.jsonl"))]
    aliases = {}
    for l in open(os.path.join(d, "redirects.jsonl")):
        r = json.loads(l)
        if r["alias"] != base[r["target"]]["title"]:
            aliases.setdefault(r["target"], []).append(r["alias"])
    n, out = len(base), []
    for r in range(scale):
        sfx = (lambda s: s) if r == 0 else (lambda s, r=r: f"{s} ({r + 1})")
        for a in base:
            out.append({"title": sfx(a["title"]),
                        "text": a["text"] if r == 0 else a["text"].replace(a["title"], sfx(a["title"]), 1),
                        "links": [l + r * n for l in a["links"]],
                        "aliases": [sfx(x) for x in aliases.get(a["id"], [])]})
    return out


class Doc(IndexData):
    def __init__(self, title, text):
        super().__init__()
        self.t, self.x = title, text
    def has_indexdata(self): return True
    def get_title(self): return self.t
    def get_content(self): return self.x
    def get_keywords(self): return ""
    def get_wordcount(self): return len(self.x.split())


class Text(Item):
    def __init__(self, path, title, text, front, index):
        super().__init__()
        self.p, self.t, self.x, self.f, self.i = path, title, text, front, index
    def get_path(self): return self.p
    def get_title(self): return self.t
    def get_mimetype(self): return "text/plain"
    def get_contentprovider(self): return StringProvider(self.x)
    def get_hints(self): return {Hint.FRONT_ARTICLE: self.f, Hint.COMPRESS: True}
    def get_indexdata(self): return Doc(self.t, self.x) if self.i else None


def build_zim(arts, path, fulltext=True):
    if os.path.exists(path):
        os.remove(path)
    paths = {a["title"] for a in arts}
    t = time.perf_counter()
    with Creator(path).config_indexing(fulltext, "eng").config_nbworkers(os.cpu_count()) as c:
        c.set_mainpath(arts[0]["title"])
        for k, v in {"Title": "wikibench", "Language": "eng", "Creator": "wikibench",
                     "Publisher": "wikibench", "Date": "2026-10-30", "Description": "bench",
                     "Name": "wikibench"}.items():
            c.add_metadata(k, v)
        for a in arts:
            c.add_item(Text(a["title"], a["title"], a["text"], True, True))
            c.add_item(Text("_links/" + a["title"], "", "\n".join(arts[l]["title"] for l in a["links"]), False, False))
        for i, a in enumerate(arts):
            for al in a["aliases"]:
                if al not in paths:
                    paths.add(al)
                    c.add_redirection(al, al, a["title"], {Hint.FRONT_ARTICLE: True})  # Kiwix lists redirects in the title index
    return (time.perf_counter() - t) * 1000


def stats(ds):
    ds = sorted(ds)
    pick = lambda q: round(ds[round((len(ds) - 1) * q)] * 1e6, 2)
    return {"n": len(ds), "p50_us": pick(.5), "p90_us": pick(.9), "p99_us": pick(.99),
            "mean_us": round(sum(ds) / len(ds) * 1e6, 2)}


def time_each(items, f):
    ds = []
    for it in items:
        t = time.perf_counter(); f(it); ds.append(time.perf_counter() - t)
    return stats(ds)


def drop_caches():
    os.sync()
    try:
        open("/proc/sys/vm/drop_caches", "w").write("3"); return True
    except OSError:
        return False


class Rng:  # same xorshift as the Rust harness, so both pick the same articles
    def __init__(self, s): self.s = s
    def below(self, n):
        m = (1 << 64) - 1
        self.s ^= (self.s << 13) & m; self.s ^= self.s >> 7; self.s ^= (self.s << 17) & m
        return self.s % n


def fname(t):
    s = t.replace("%", "%25").replace("/", "%2F")
    return s  # titles in this corpus fit in 240 bytes; the Rust side truncates longer ones


def main(corpus, out, scale=1, original=None):
    arts = load_corpus(corpus, scale)
    n = len(arts)
    titles = [a["title"] for a in arts]
    r = {"scale": scale, "articles": n}
    zp = os.path.join(out, "wiki.zim")
    r["build_ms"] = round(build_zim(arts, zp, True), 2)
    r["build_nofts_ms"] = round(build_zim(arts, os.path.join(out, "wiki-nofts.zim"), False), 2)
    r["size_bytes"] = os.path.getsize(zp)
    r["size_nofts_bytes"] = os.path.getsize(os.path.join(out, "wiki-nofts.zim"))

    rng = Rng(0x9E3779B97F4A7C15 ^ scale)
    lookups = [rng.below(n) for _ in range(2000)]
    r["cold_cache"] = drop_caches()
    t = time.perf_counter()
    z = Archive(zp)
    body = bytes(z.get_entry_by_title(titles[lookups[0]]).get_item().content).decode()
    r["open_first_ms"] = round((time.perf_counter() - t) * 1000, 2)
    assert body == arts[lookups[0]]["text"]

    for root, _, files in os.walk(out):   # warm everything, same as the Rust harness
        for f in files:
            with open(os.path.join(root, f), "rb") as fh:
                while fh.read(1 << 20): pass
    look = {}
    look["zim_by_title"] = time_each(lookups, lambda i: bytes(z.get_entry_by_title(titles[i]).get_item().content))
    look["zim_by_path"] = time_each(lookups, lambda i: bytes(z.get_entry_by_path(titles[i]).get_item().content))
    db = sqlite3.connect(os.path.join(out, "wiki.sqlite"))
    look["py_sqlite_by_title"] = time_each(lookups, lambda i: db.execute("SELECT body FROM articles WHERE title=?", (titles[i],)).fetchone()[0])
    look["py_file_read"] = time_each(lookups, lambda i: open(os.path.join(out, "files", "a", fname(titles[i])), "rb").read())
    r["lookup_warm"] = look
    r["python_overhead_note"] = "compare py_sqlite_by_title here with sqlite in rust_results.json to calibrate"

    seeds = [rng.below(n) for _ in range(200)]
    def bfs(seed):
        seen, q, b = {seed}, [(seed, 0)], 0
        while q:
            u, d = q.pop(0)
            b += len(bytes(z.get_entry_by_path(titles[u]).get_item().content))
            if d == 2: continue
            for t in bytes(z.get_entry_by_path("_links/" + titles[u]).get_item().content).decode().split("\n"):
                if t:
                    v = idx[t]
                    if v not in seen: seen.add(v); q.append((v, d + 1))
        return b
    idx = {t: i for i, t in enumerate(titles)}
    r["traversal_2hop"] = time_each(seeds, bfs)
    targets = [rng.below(n) for _ in range(100)]
    def backlinks(t):
        return sum(1 for a in titles if titles[t] in bytes(z.get_entry_by_path("_links/" + a).get_item().content).decode().split("\n"))
    r["backlinks_scan"] = time_each(targets[:5], backlinks)

    if scale == 1:
        qs = [json.loads(l) for l in open(os.path.join(corpus, "queries.jsonl"))]
        ret = {}
        for kind in ("alias", "alias_lower"):
            q = [x for x in qs if x["kind"] == kind]
            hits = []
            def exact(x):
                try:
                    e = z.get_entry_by_title(x["q"])
                    hits.append(e.get_item().path == titles[x["gold"]])
                except KeyError:
                    hits.append(False)
            lat = time_each(q, exact)
            ret.setdefault(kind, {})["zim_title_index"] = {"queries": len(q), "hit_rate": round(sum(hits) / len(q), 3), "latency": lat}
            sugg, hits = SuggestionSearcher(z), []
            def suggest(x):
                res = list(sugg.suggest(x["q"]).getResults(0, 1))
                if res:
                    e = z.get_entry_by_path(res[0])
                    hits.append((e.get_redirect_entry() if e.is_redirect else e).path == titles[x["gold"]])
                else:
                    hits.append(False)
            lat = time_each(q, suggest)
            ret[kind]["zim_suggestion_top1"] = {"queries": len(q), "hit_rate": round(sum(hits) / len(q), 3), "latency": lat}
            searcher, hits = Searcher(z), []
            def fulltext_top1(x):
                res = list(searcher.search(Query().set_query(x["q"])).getResults(0, 1))
                hits.append(bool(res) and res[0] == titles[x["gold"]])
            lat = time_each(q, fulltext_top1)
            ret[kind]["zim_xapian_fulltext_top1"] = {"queries": len(q), "hit_rate": round(sum(hits) / len(q), 3), "latency": lat}
        q = [x for x in qs if x["kind"].startswith("keywords")]
        searcher = Searcher(z)
        for kind in sorted({x["kind"] for x in q}):
            res, ctx = [], []
            def ft(x):
                ranked = [idx.get(p, -1) for p in searcher.search(Query().set_query(x["q"])).getResults(0, 10)]
                res.append((x["gold"], ranked))
            lat = time_each([x for x in q if x["kind"] == kind], ft)
            r1 = sum(rk[:1] == [g] for g, rk in res); r10 = sum(g in rk for g, rk in res)
            mrr = sum(1 / (rk.index(g) + 1) for g, rk in res if g in rk)
            for g, rk in res:
                upto = rk.index(g) + 1 if g in rk else len(rk)
                ctx.append(sum(len(arts[d]["text"].encode()) for d in rk[:upto] if d >= 0))
            ctx.sort()
            k = len(res)
            ret.setdefault(kind, {})["zim_xapian"] = {"queries": k, "recall@1": round(r1 / k, 3), "recall@10": round(r10 / k, 3),
                                                       "mrr@10": round(mrr / k, 3), "context_bytes_p50": ctx[k // 2], "latency": lat}
        r["retrieval"] = ret

    if original:
        drop_caches()
        t = time.perf_counter()
        o = Archive(original)
        e = o.main_entry.get_item()
        r["original_zim"] = {"path": os.path.basename(original), "size_bytes": os.path.getsize(original),
                             "entries": o.all_entry_count, "articles": o.article_count,
                             "open_first_ms": round((time.perf_counter() - t) * 1000, 2)}
        es = (o._get_entry_by_id(i) for i in range(o.all_entry_count))
        names = [e.path for e in es if not e.is_redirect and e.get_item().mimetype == "text/html"]
        names = [names[Rng(7 + i).below(len(names))] for i in range(2000)]
        r["original_zim"]["lookup_warm_by_path_html"] = time_each(names, lambda p: bytes(o.get_entry_by_path(p).get_item().content))

    with open(os.path.join(out, "zim_results.json"), "w") as f:
        json.dump(r, f, indent=2)
    print(json.dumps(r, indent=2))


if __name__ == "__main__":
    a = sys.argv[1:]
    orig = None
    if "--original" in a:
        i = a.index("--original"); orig = a[i + 1]; del a[i:i + 2]
    main(a[0], a[1], int(a[2]) if len(a) > 2 else 1, orig)
