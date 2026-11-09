#!/usr/bin/env python3
"""Extract a Kiwix Wikipedia ZIM into a canonical, format-neutral corpus.

Output (one directory):
  articles.jsonl   {"id", "path", "title", "text", "links": [target ids]}
  redirects.jsonl  {"alias", "target"}  (alias title -> article id)

Every store in the benchmark (files, SQLite, ZIM, phext) is built from these
exact bytes, so storage and latency comparisons hold the payload constant.

usage: extract_zim.py <in.zim> <out_dir>
"""
import json, os, re, sys
from html.parser import HTMLParser
from urllib.parse import unquote
from libzim.reader import Archive

SKIP_TAGS = {"script", "style", "noscript", "sup", "table", "figure", "math"}
BLOCK_TAGS = {"p", "h1", "h2", "h3", "h4", "h5", "h6", "li", "dd", "dt", "div", "br", "tr", "section"}
# Bytes that are phext dimension breaks; they never appear in Wikipedia prose,
# but we strip them so every store holds byte-identical payloads.
FOOTER = "This article is issued from Wikipedia."
PHEXT_BREAKS = re.compile("[\x01\x07\x17\x18\x19\x1a\x1c\x1d\x1e\x1f]")


class Article(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.out, self.links, self.skip = [], [], 0

    def handle_starttag(self, tag, attrs):
        if tag in SKIP_TAGS:
            self.skip += 1
        elif tag == "a" and not self.skip:
            href = dict(attrs).get("href") or ""
            if href and "://" not in href and not href.startswith("#"):
                self.links.append(unquote(href.split("#")[0]).lstrip("./"))
        if tag in BLOCK_TAGS:
            self.out.append("\n")

    def handle_endtag(self, tag):
        if tag in SKIP_TAGS and self.skip:
            self.skip -= 1
        if tag in BLOCK_TAGS:
            self.out.append("\n")

    def handle_data(self, data):
        if not self.skip:
            self.out.append(data)

    def text(self):
        t = PHEXT_BREAKS.sub("", "".join(self.out))
        t = re.sub(r"[ \t\r\f\v]+", " ", t)
        t = re.sub(r" *\n *", "\n", t)
        return re.sub(r"\n{3,}", "\n\n", t).strip()


def main(zim_path, out_dir):
    zim = Archive(zim_path)
    os.makedirs(out_dir, exist_ok=True)
    articles, redirects = {}, []
    for i in range(zim.all_entry_count):
        e = zim._get_entry_by_id(i)
        if e.is_redirect:
            redirects.append((e.title, e.get_redirect_entry().path))
            continue
        item = e.get_item()
        if item.mimetype != "text/html" or e.path in ("mainPage", "index"):
            continue
        p = Article()
        p.feed(bytes(item.content).decode("utf-8", "replace"))
        lines = [l for l in p.text().split("\n") if not l.startswith(FOOTER)]
        while lines and lines[0] in ("", e.title):   # drop repeated <title>/<h1>
            lines.pop(0)
        articles[e.path] = (e.title, e.title + "\n\n" + "\n".join(lines).strip(), p.links)

    paths = sorted(articles)                      # stable ids: sorted by path
    ids = {p: n for n, p in enumerate(paths)}
    alias = {}
    for title, target in redirects:               # resolve to article ids
        if target in ids:
            alias[title] = ids[target]

    with open(os.path.join(out_dir, "articles.jsonl"), "w") as f:
        for p in paths:
            title, text, links = articles[p]
            out = []
            for l in links:
                t = ids.get(l)
                if t is None and l.replace("_", " ") in alias:
                    t = alias[l.replace("_", " ")]
                if t is not None and t != ids[p] and t not in out:
                    out.append(t)
            f.write(json.dumps({"id": ids[p], "path": p, "title": title,
                                "text": text, "links": out}, ensure_ascii=False) + "\n")
    with open(os.path.join(out_dir, "redirects.jsonl"), "w") as f:
        for a, t in sorted(alias.items()):
            f.write(json.dumps({"alias": a, "target": t}, ensure_ascii=False) + "\n")
    edges = sum(len(articles[p][2]) for p in paths)
    print(f"{len(paths)} articles, {len(alias)} redirects, {edges} raw hrefs -> {out_dir}")


if __name__ == "__main__":
    main(*sys.argv[1:3])
