#!/usr/bin/env python3
"""Deterministic agent-retrieval query set for the offline-Wikipedia benchmark.

kinds:
  alias        an exact redirect title ("look up X")            -> gold = target article
  alias_lower  the same redirect, lower-cased (agent case noise) -> gold = target article
  keywords     5 high-IDF terms from one non-lead sentence of the article, title words
               removed ("which article says ...")                -> gold = source article
  keywords_hard  the 3 most common (lowest-IDF) non-stopword terms of that same sentence:
               vague recall, the regime where ranking matters    -> gold = source article

usage: make_queries.py <corpus_dir> [n_per_kind=500]
"""
import json, math, os, random, re, sys

TOK = re.compile(r"[^\W_]{2,}")
STOP = set("the and for are was were with that this from which have has had not but its their they his her "
           "into also been more than other such these those can may will would about between during after "
           "over under some most many one two new first used use being both all any each only".split())


def main(d, n=500):
    rnd = random.Random(2130)
    arts = [json.loads(l) for l in open(os.path.join(d, "articles.jsonl"))]
    reds = [json.loads(l) for l in open(os.path.join(d, "redirects.jsonl"))]
    titles = {a["title"] for a in arts}
    df = {}
    for a in arts:
        for t in set(TOK.findall(a["text"].lower())):
            df[t] = df.get(t, 0) + 1
    N = len(arts)
    out = []
    reds = [r for r in reds if r["alias"] not in titles]
    lower_ok = {}
    for r in reds:
        lower_ok.setdefault(r["alias"].lower(), set()).add(r["target"])
    for a in arts:
        lower_ok.setdefault(a["title"].lower(), set()).add(a["id"])
    for r in rnd.sample(reds, n):
        out.append({"kind": "alias", "q": r["alias"], "gold": r["target"]})
    unambiguous = [r for r in reds if len(lower_ok[r["alias"].lower()]) == 1 and r["alias"] != r["alias"].lower()]
    for r in rnd.sample(unambiguous, n):
        out.append({"kind": "alias_lower", "q": r["alias"].lower(), "gold": r["target"]})
    pool = [a for a in arts if len(a["text"]) >= 600]
    rnd.shuffle(pool)
    for a in pool:
        if sum(o["kind"] == "keywords" for o in out) >= n:
            break
        title_toks = set(TOK.findall(a["title"].lower()))
        body = a["text"].split("\n", 2)[-1]
        sents = [s for s in re.split(r"(?<=[.!?])\s+", body)[1:] if len(s) > 80]
        if not sents:
            continue
        s = rnd.choice(sents)
        terms = [t for t in dict.fromkeys(TOK.findall(s.lower()))
                 if len(t) >= 4 and t not in STOP and t not in title_toks and not t.isdigit() and df.get(t, 0) >= 2]
        terms.sort(key=lambda t: (-math.log(N / df[t]), t))
        if len(terms) < 5:
            continue
        out.append({"kind": "keywords", "q": " ".join(terms[:5]), "gold": a["id"], "sentence": s})
        out.append({"kind": "keywords_hard", "q": " ".join(terms[-3:][::-1]), "gold": a["id"], "sentence": s})
    with open(os.path.join(d, "queries.jsonl"), "w") as f:
        for o in out:
            f.write(json.dumps(o, ensure_ascii=False) + "\n")
    print({k: sum(o["kind"] == k for o in out) for k in ("alias", "alias_lower", "keywords", "keywords_hard")})


if __name__ == "__main__":
    main(sys.argv[1], *(int(x) for x in sys.argv[2:3]))
