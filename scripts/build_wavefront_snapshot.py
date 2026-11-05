#!/usr/bin/env python3
"""Build web/wavefront-snapshot.json from wavefront.phext.

The snapshot is the claim-mode lookup table for edit.phext.io: normalized
Twitter handle -> placements (coordinate + region) in Will Bickford's
wavefront map. The source of truth is wavefront.phext itself — its index
scroll (2.2.2/2.2.2/2.2.2) already lists every placement as
`Name (@handle) — coordinate — region`, so this script is a pure
extractor: it adds no placements and drops none.

Source of the phext: wbic16/human@exo once Will merges branch
emise/wavefront-phext; until then, the branch build at
~/workspace/wavefront/wavefront.phext. Regenerate after any republication
of the phext:

    python3 scripts/build_wavefront_snapshot.py /path/to/wavefront.phext \
        --source "wbic16/human@exo wavefront.phext"

Output: web/wavefront-snapshot.json (checked in; embedded by the server).
"""

import argparse
import json
import re
import sys

# Phext delimiter hierarchy (highest -> lowest), per libphext:
# library shelf series collection volume book chapter section scroll
LEVELS = [0x01, 0x1F, 0x1E, 0x1D, 0x1C, 0x1A, 0x19, 0x18, 0x17]
DELIM_TO_LEVEL = {byte: i for i, byte in enumerate(LEVELS)}
INDEX_COORD = "2.2.2/2.2.2/2.2.2"
COORD_RE = re.compile(r"^\d+\.\d+\.\d+/\d+\.\d+\.\d+/\d+\.\d+\.\d+$")
REGIONS = {"the Hearth": "hearth", "the Shoals": "shoals", "open water": "open_water"}

# The index line for Will's own reference placement carries no handle in
# parentheses; his primary account is @wbic16 (his bot @wbic32 has its own
# line and is a separate snapshot key on purpose — a claimant authenticates
# as one account or the other).
HANDLE_ALIASES = {"Will Bickford": "wbic16"}


def fmt(coord):
    return ".".join(str(c) for c in coord[0:3]) + "/" + \
        ".".join(str(c) for c in coord[3:6]) + "/" + \
        ".".join(str(c) for c in coord[6:9])


def parse_phext(data):
    """Dead-reckoning parse: walk bytes, split on delimiters, map to coords."""
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


def handle_of(name_part):
    m = re.search(r"\(@([A-Za-z0-9_]+)\)", name_part)
    if m:
        return m.group(1)
    if name_part.startswith("@"):
        return name_part[1:].split()[0]
    return HANDLE_ALIASES.get(name_part)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("phext", help="path to wavefront.phext")
    ap.add_argument("--source", default="", help="provenance string for the snapshot meta")
    ap.add_argument("--out", default="web/wavefront-snapshot.json")
    args = ap.parse_args()

    scrolls = parse_phext(open(args.phext, "rb").read())
    if INDEX_COORD not in scrolls:
        sys.exit(f"index scroll {INDEX_COORD} not found in {args.phext}")

    people = {}   # normalized handle -> entry
    placements = 0
    for line in scrolls[INDEX_COORD].splitlines():
        parts = [p.strip() for p in line.split(" — ")]
        coord_at = [i for i, p in enumerate(parts) if COORD_RE.match(p)]
        if not coord_at:
            continue
        i = coord_at[0]
        coord, region_raw = parts[i], parts[i + 1] if i + 1 < len(parts) else ""
        if region_raw not in REGIONS:
            continue
        name_part = parts[0]
        label = parts[1] if i == 2 else ""  # e.g. Orai's "arrival" / "rupture"
        handle = handle_of(name_part)
        if not handle:
            sys.exit(f"no handle extractable from index line: {line!r}")
        name = re.sub(r"\s*\(@[A-Za-z0-9_]+\)\s*$", "", name_part).lstrip("@").strip()
        entry = people.setdefault(handle.lower(), {
            "handle": "@" + handle, "name": name or handle, "placements": []})
        entry["placements"].append({
            "coordinate": coord, "region": REGIONS[region_raw], "label": label})
        placements += 1

    snapshot = {
        "meta": {
            "source": args.source or args.phext,
            "people_note": "placements counts index lines; handles counts distinct "
                           "accounts (wbic16 and wbic32 are one person, two accounts)",
            "placements": placements,
            "handles": len(people),
        },
        "people": [people[k] for k in sorted(people)],
    }
    with open(args.out, "w") as f:
        json.dump(snapshot, f, indent=1, sort_keys=False)
        f.write("\n")
    print(f"{args.out}: {placements} placements, {len(people)} handles")


if __name__ == "__main__":
    main()
