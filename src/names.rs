/// Name Index — human names (titles, aliases) → coordinates, exact and case-folded.
///
/// Coordinates are the lattice's addresses, but people and agents ask by name. A phext
/// that stores titles or aliases as scrolls can build this map on open; lookups are
/// then hash probes. The folded map is what makes "ferrel cell" find "Ferrel cell":
/// on wikibench, exact-only lookup found 0% of lower-cased redirect titles and the
/// folded map found 100%, for ~3 ms of build time over 20k names.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::{BuildHasherDefault, Hasher};
use libphext::phext::Coordinate;
use crate::mmap::MappedLattice;

/// Case-fold a name for lookup: Unicode lowercase with runs of whitespace collapsed
/// to one space and the ends trimmed.
pub fn fold(name: &str) -> String {
    // fast path: ASCII that is already single-spaced and trimmed
    let tidy = !name.starts_with(' ') && !name.ends_with(' ') && !name.contains("  ")
        && !name.bytes().any(|b| b != b' ' && b.is_ascii_whitespace());
    if name.is_ascii() && tidy {
        return name.to_ascii_lowercase();
    }
    let mut out = String::with_capacity(name.len());
    for word in name.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().flat_map(char::to_lowercase));
    }
    out
}

/// FxHash (rustc's hasher): names are not attacker-chosen keys here, and SipHash
/// was the largest cost of building the index on open.
#[derive(Default, Clone, Copy)]
struct FxHasher(u64);

impl Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        for &b in chunks.remainder() {
            self.add(b as u64);
        }
    }
    fn write_u8(&mut self, b: u8) {
        self.add(b as u64);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

type FxMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// Exact and folded name → coordinate maps. When two names collide, the first one
/// inserted wins, so building in document order prefers earlier scrolls.
#[derive(Debug, Clone, Default)]
pub struct NameIndex {
    exact: FxMap<String, Coordinate>,
    folded: FxMap<String, Coordinate>,
    /// Folded keys claimed by more than one distinct coordinate.
    ambiguous: usize,
}

impl NameIndex {
    pub fn new() -> NameIndex {
        NameIndex::default()
    }

    /// Build from the lattice: every non-empty line of each scroll that `select` maps
    /// to `Some(target)` becomes a name for `target`. Return the scroll's own
    /// coordinate to name it, or another one (e.g. a record's body) to point there.
    pub fn from_scrolls<F>(lattice: &MappedLattice, select: F) -> NameIndex
    where
        F: Fn(&Coordinate) -> Option<Coordinate>,
    {
        let mut names = NameIndex::new();
        let mut add = |target: Coordinate, text: &str| {
            for line in text.lines() {
                names.insert(line, target);
            }
        };
        match lattice.raw_buffer() {
            // no pending edits: read names straight out of the mmap
            Some(buffer) => {
                for c in lattice.index().coordinates() {
                    if let (Some(target), Some(span)) = (select(c), lattice.index().get(c)) {
                        add(target, &String::from_utf8_lossy(&buffer[span.start..span.end]));
                    }
                }
            }
            None => {
                for c in lattice.populated_coordinates() {
                    if let (Some(target), Some(text)) = (select(&c), lattice.read_scroll(&c)) {
                        add(target, &text);
                    }
                }
            }
        }
        names
    }

    /// Register `name` for `coord`. Blank names are ignored. Returns false if the exact
    /// name was already taken (the earlier coordinate is kept).
    pub fn insert(&mut self, name: &str, coord: Coordinate) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        match self.exact.entry(name.to_string()) {
            Entry::Occupied(_) => return false,
            Entry::Vacant(slot) => { slot.insert(coord); }
        }
        match self.folded.entry(fold(name)) {
            Entry::Vacant(slot) => { slot.insert(coord); }
            Entry::Occupied(owner) if *owner.get() != coord => self.ambiguous += 1,
            Entry::Occupied(_) => {}
        }
        true
    }

    /// Exact (case-sensitive) lookup.
    pub fn get(&self, name: &str) -> Option<Coordinate> {
        self.exact.get(name.trim()).copied()
    }

    /// Case-insensitive lookup through the folded map.
    pub fn get_folded(&self, name: &str) -> Option<Coordinate> {
        self.folded.get(&fold(name)).copied()
    }

    /// Exact match first, then folded: what an agent's "open <name>" should call.
    pub fn resolve(&self, name: &str) -> Option<Coordinate> {
        self.get(name).or_else(|| self.get_folded(name))
    }

    /// Distinct exact names.
    pub fn len(&self) -> usize {
        self.exact.len()
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty()
    }

    /// How many inserts hit a folded key already owned by a different coordinate.
    /// Those names resolve exactly but fold to the first owner.
    pub fn ambiguous(&self) -> usize {
        self.ambiguous
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libphext::phext::to_coordinate;

    fn c(s: &str) -> Coordinate {
        to_coordinate(s)
    }

    #[test]
    fn fold_lowercases_and_normalizes_whitespace() {
        assert_eq!(fold("  Ferrel   Cell "), "ferrel cell");
        assert_eq!(fold("ÉTÉ\tİzmir"), "été i̇zmir");
        assert_eq!(fold("Coal Dam"), "coal dam");
        assert_eq!(fold("a\tb"), "a b");
    }

    #[test]
    fn exact_then_folded_resolution() {
        let mut n = NameIndex::new();
        assert!(n.insert("Ferrel cell", c("1.1.1/1.1.1/1.1.1")));
        assert!(n.insert("Coal dam", c("1.1.1/1.1.1/1.1.2")));
        assert_eq!(n.get("Ferrel cell"), Some(c("1.1.1/1.1.1/1.1.1")));
        assert_eq!(n.get("ferrel cell"), None);
        assert_eq!(n.get_folded("FERREL  cell"), Some(c("1.1.1/1.1.1/1.1.1")));
        assert_eq!(n.resolve("coal DAM"), Some(c("1.1.1/1.1.1/1.1.2")));
        assert_eq!(n.resolve("missing"), None);
        assert!(!n.insert("   ", c("1.1.1/1.1.1/1.1.3")));
    }

    #[test]
    fn first_owner_wins_and_ambiguity_is_counted() {
        let mut n = NameIndex::new();
        n.insert("Mercury", c("1.1.1/1.1.1/1.1.1"));
        assert!(!n.insert("Mercury", c("1.1.1/1.1.1/1.1.9")));
        n.insert("MERCURY", c("1.1.1/1.1.1/1.1.2"));
        assert_eq!(n.get("MERCURY"), Some(c("1.1.1/1.1.1/1.1.2")));
        assert_eq!(n.get_folded("mercury"), Some(c("1.1.1/1.1.1/1.1.1")));
        assert_eq!(n.ambiguous(), 1);
        assert_eq!(n.len(), 2);
    }

    #[test]
    fn builds_from_title_and_alias_scrolls() {
        // record per section: scroll 1 title, 2 body, 3 aliases (one per line)
        let lat = MappedLattice::from_bytes(
            "Coal\x17about coal\x17Hard coal\nAnthracite\x18Wind\x17about wind".as_bytes());
        let names = NameIndex::from_scrolls(&lat, |s| {
            (s.x.scroll == 1 || s.x.scroll == 3).then(|| { let mut b = *s; b.x.scroll = 2; b })
        });
        assert_eq!(names.len(), 4);
        assert_eq!(names.resolve("anthracite"), Some(c("1.1.1/1.1.1/1.1.2")));
        assert_eq!(names.get("Wind"), Some(c("1.1.1/1.1.1/1.2.2")));
        assert_eq!(lat.read_scroll(&names.resolve("hard COAL").unwrap()).unwrap(), "about coal");
    }
}
