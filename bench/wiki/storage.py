#!/usr/bin/env python3
"""Storage accounting for one wikibench output dir (after `wikibench build` + zim_bench.py).

Reports apparent bytes, allocated bytes (st_blocks), inode count, overhead vs. the raw
article payload, and zstd-19 size of the single-file stores. For phext it also breaks
the file into payload / link / alias / title bytes and delimiter bytes.

usage: storage.py <out_dir>
"""
import json, os, sys
import zstandard

BREAKS = set(b"\x01\x17\x18\x19\x1a\x1c\x1d\x1e\x1f")


def tree(p):
    app = alloc = inodes = 0
    for root, dirs, files in os.walk(p):
        for f in files + dirs:
            st = os.lstat(os.path.join(root, f))
            app += st.st_size; alloc += st.st_blocks * 512; inodes += 1
    return app, alloc, inodes


def one(p):
    st = os.stat(p)
    return st.st_size, st.st_blocks * 512, 1


def main(out):
    rr = json.load(open(os.path.join(out, "rust_results.json")))
    payload = rr["payload_bytes"]
    res = {"payload_bytes": payload, "articles": rr["articles"], "stores": {}}
    cctx = zstandard.ZstdCompressor(level=19, threads=-1)
    for name, path in [("files", "files"), ("sqlite_fts5", "wiki.sqlite"), ("sqlite_nofts", "wiki-nofts.sqlite"),
                       ("phext", "wiki.phext"), ("zim_xapian", "wiki.zim"), ("zim_nofts", "wiki-nofts.zim")]:
        full = os.path.join(out, path)
        if not os.path.exists(full):
            continue
        app, alloc, inodes = tree(full) if os.path.isdir(full) else one(full)
        d = {"apparent_bytes": app, "allocated_bytes": alloc, "inodes": inodes,
             "overhead_vs_payload": round(alloc / payload, 3)}
        if not os.path.isdir(full) and not path.endswith(".zim"):
            d["zstd19_bytes"] = len(cctx.compress(open(full, "rb").read()))
        res["stores"][name] = d
    # phext breakdown: rebuild from build (run mutates wiki.phext, so rebuild first)
    data = open(os.path.join(out, "wiki.phext"), "rb").read()
    delim = sum(1 for b in data if b in BREAKS)
    res["phext_breakdown"] = {"delimiter_bytes": delim, "non_payload_bytes": len(data) - payload - delim,
                              "note": "non_payload = titles + link coordinates + aliases"}
    with open(os.path.join(out, "storage.json"), "w") as f:
        json.dump(res, f, indent=2)
    print(json.dumps(res, indent=2))


if __name__ == "__main__":
    main(sys.argv[1])
