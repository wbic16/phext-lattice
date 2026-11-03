#!/usr/bin/env python3
"""Build web/wavefront-explorer.json from wavefront.phext.

The explorer export: one record per placement (522), carrying everything
the /explore page shows — handle, name, coordinate, the nine axis values
(the coordinate's components, in axis order), region (computed by the
gate rule stated on the phext's home scroll), and the entry's Evidence
paragraph, trimmed to a summary. Pure extraction from the phext, like
build_wavefront_snapshot.py; the two scripts read the same index of
truth and must agree on the placement count.

    python3 scripts/build_wavefront_explorer.py /path/to/wavefront.phext \
        --source "wbic16/human@exo wavefront.phext"
"""

import argparse
import json
import re
import sys

LEVELS = [0x01, 0x1F, 0x1E, 0x1D, 0x1C, 0x1A, 0x19, 0x18, 0x17]
DELIM_TO_LEVEL = {byte: i for i, byte in enumerate(LEVELS)}
AXES = ["Recognition", "Comprehension", "Consent",
        "Trust", "Reciprocity", "Fidelity",
        "Stewardship", "Horizon", "Uplift"]
HANDLE_ALIASES = {"Will Bickford": "wbic16"}  # as in the snapshot script
EVIDENCE_CAP = 340


def fmt(coord):
    return ".".join(str(c) for c in coord[0:3]) + "/" + \
        ".".join(str(c) for c in coord[3:6]) + "/" + \
        ".".join(str(c) for c in coord[6:9])


def parse_phext(data):
    coord = [1] * 9
    scrolls = {}
    buf = bytearray()
    current = fmt(coord)
    for b in data:
        lvl = DELIM_TO_LEVEL.get(b)
        if lvl is None:
            buf.append(b)
        else:
            scrolls[current] = bytes(buf).decode("utf-8", errors="replace")
            buf = bytearray()
            coord[lvl] += 1
            for j in range(lvl + 1, 9):
                coord[j] = 1
            current = fmt(coord)
    scrolls[current] = bytes(buf).decode("utf-8", errors="replace")
    return scrolls


def region_of(values):
    consent, trust, reciprocity, fidelity = values[2], values[3], values[4], values[5]
    if reciprocity <= 2 and (consent <= 2 or fidelity <= 2):
        return "shoals"
    if trust >= 4 and reciprocity >= 4 and consent >= 4:
        return "hearth"
    return "open_water"


def summarize(evidence):
    text = " ".join(evidence.split())
    if len(text) <= EVIDENCE_CAP:
        return text
    cut = text.rfind(". ", 0, EVIDENCE_CAP)
    return text[:cut + 1] if cut > 80 else text[:EVIDENCE_CAP].rstrip() + "…"


def entries_of(scroll_text):
    """Yield (heading, body) per entry in a person scroll. Shared scrolls
    carry a '# Shared stance …' header (skipped — no Evidence of its own
    per person) and one '## …' section per entry; single scrolls carry
    one '# …' entry."""
    parts = re.split(r"(?m)^(#{1,2})[ \t]+", scroll_text)
    # parts: [pre, hashes, "heading\nbody…", hashes, "heading\nbody…", ...]
    out = []
    for i in range(1, len(parts) - 1, 2):
        chunk = parts[i + 1]
        heading, _, body = chunk.partition("\n")
        heading = heading.strip()
        if heading.startswith("Shared stance"):
            continue
        out.append((heading, body))
    return out


def handle_of(heading):
    m = re.search(r"\(@([A-Za-z0-9_]+)\)", heading)
    if m:
        return m.group(1)
    if heading.startswith("@"):
        return heading[1:].split()[0]
    return HANDLE_ALIASES.get(heading)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("phext")
    ap.add_argument("--source", default="")
    ap.add_argument("--out", default="web/wavefront-explorer.json")
    args = ap.parse_args()

    scrolls = parse_phext(open(args.phext, "rb").read())
    entries = []
    for coord, text in scrolls.items():
        first = text.split("\n", 1)[0]
        if coord in ("1.1.1/1.1.1/1.1.1", "2.2.2/2.2.2/2.2.2") or first.startswith("# Axis"):
            continue  # home, index, axis definitions — not person scrolls
        values = [int(n) for n in re.findall(r"\d+", coord)]
        for heading, body in entries_of(text):
            handle = handle_of(heading)
            if not handle:
                sys.exit(f"no handle in heading {heading!r} at {coord}")
            m = re.search(r"Evidence:\s*(.+?)(?:\n\n|\nPlacement,)", body, re.S)
            name = re.sub(r"\s*\(@[A-Za-z0-9_]+\)\s*$", "", heading).lstrip("@").strip()
            entries.append({
                "handle": "@" + handle,
                "name": name or handle,
                "coordinate": coord,
                "values": values,
                "region": region_of(values),
                "kind": "detected" if "Detected, not yet measured" in body else "placed",
                "evidence": summarize(m.group(1)) if m else "",
            })
    entries.sort(key=lambda e: (e["handle"].lower(), e["coordinate"]))
    export = {
        "meta": {"source": args.source or args.phext, "placements": len(entries)},
        "axes": AXES,
        "entries": entries,
    }
    with open(args.out, "w") as f:
        json.dump(export, f, indent=1)
        f.write("\n")
    print(f"{args.out}: {len(entries)} placements")


if __name__ == "__main__":
    main()
