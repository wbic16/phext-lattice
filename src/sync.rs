/// Scroll-by-scroll diff and merge between two phexts (Will's direction,
/// 2026-11-20, task 29.6.9/8.6.11/7.6.9).
///
/// Two nodes holding the same phext — the Wikibox case: the same sealed
/// baseline corpus — synchronize edits by exchanging only the scrolls
/// that differ, never the whole file. The protocol is the one SQ's API
/// and the editor's seed cache already speak:
///
/// 1. Each side builds an **envelope**: every populated coordinate with
///    the xxh3_128 hash of its scroll's stored bytes, and nothing else.
///    Hashes cross the wire; content does not (see `phext_edit`'s
///    `/api/seed`, whose entries are exactly this envelope).
/// 2. **Diff** compares two envelopes and names the coordinates that
///    were added, changed, or removed — content-free.
/// 3. The sender turns a diff into **change scrolls**: coordinate,
///    content (or a removal), and the *base hash* — the hash the
///    sender's edit was based on, i.e. the receiver's expected current
///    hash for that coordinate.
/// 4. **Merge** applies a change scroll only when the receiver's current
///    hash still equals the base hash. A coordinate whose current hash
///    matches neither the base nor the incoming content is a
///    **conflict**: it is reported in the merge report and left exactly
///    as it was. Merge never silently overwrites.
///
/// Scope: the active universe of each lattice, matching the seed
/// envelope's scope. Multiverse addresses are the caller's concern,
/// exactly as with `/api/seed`.

use std::collections::BTreeMap;

use libphext::phext::Coordinate;
use xxhash_rust::xxh3::xxh3_128;

use crate::mmap::MappedLattice;

/// The hash of one scroll's stored bytes: xxh3_128, hex of the 16-byte
/// digest, high half first (`u128::to_be_bytes`) — the seed cache's
/// hash, so an envelope built here compares equal to `/api/seed`'s.
pub fn hash_stored(stored: &[u8]) -> String {
    xxh3_128(stored).to_be_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// One envelope entry: a populated coordinate and its scroll's hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvelopeEntry {
    pub coordinate: Coordinate,
    pub hash: String,
}

/// A coordinate present on both sides whose hashes differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedEntry {
    pub coordinate: Coordinate,
    pub local_hash: String,
    pub peer_hash: String,
}

/// The difference between two envelopes, from the local side's view:
/// `only_local` coordinates are absent from the peer's envelope,
/// `only_peer` coordinates are absent from the local one. All three
/// lists are in coordinate-string order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScrollDiff {
    pub changed: Vec<ChangedEntry>,
    pub only_local: Vec<EnvelopeEntry>,
    pub only_peer: Vec<EnvelopeEntry>,
}

impl ScrollDiff {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.only_local.is_empty() && self.only_peer.is_empty()
    }
}

/// One change scroll: the unit two nodes exchange. `content` is the
/// scroll's stored form; `None` removes the scroll. `base_hash` is the
/// hash the change was made against — the receiver's expected current
/// hash — or `None` when the sender's base held no scroll there (a
/// creation). An empty `content` string is a removal: a scroll exists
/// only when it holds at least a byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeScroll {
    pub coordinate: Coordinate,
    pub base_hash: Option<String>,
    pub content: Option<String>,
}

/// A change merge refused: the receiver's current hash matches neither
/// the change's base hash nor its incoming content, so applying it
/// would silently overwrite an edit the sender never saw. The scroll
/// is left untouched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub coordinate: Coordinate,
    pub base_hash: Option<String>,
    pub current_hash: Option<String>,
    pub incoming_hash: Option<String>,
}

/// The outcome of a merge: which changes were applied, which were
/// already in place (same content on both sides — a re-sent change is
/// not a conflict), and which were refused as conflicts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub applied: Vec<Coordinate>,
    pub already_applied: Vec<Coordinate>,
    pub conflicts: Vec<Conflict>,
}

impl MergeReport {
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// The envelope of a lattice's active universe: one entry per populated
/// coordinate holding at least a byte, in coordinate-string order.
/// Byte-for-byte the same entries `/api/seed` serves.
pub fn envelope(lattice: &MappedLattice) -> Vec<EnvelopeEntry> {
    let mut entries: Vec<EnvelopeEntry> = lattice
        .populated_coordinates()
        .iter()
        .filter_map(|coord| {
            let stored = lattice.read_scroll_bytes(coord)?;
            if stored.is_empty() {
                None
            } else {
                Some(EnvelopeEntry { coordinate: *coord, hash: hash_stored(&stored) })
            }
        })
        .collect();
    entries.sort_by_key(|e| e.coordinate.to_string());
    entries
}

/// The current hash of one coordinate, or `None` when the coordinate
/// holds no bytes.
fn current_hash(lattice: &MappedLattice, coord: &Coordinate) -> Option<String> {
    let stored = lattice.read_scroll_bytes(coord)?;
    if stored.is_empty() {
        None
    } else {
        Some(hash_stored(&stored))
    }
}

/// Diff two envelopes. Content-free: only coordinates and hashes are
/// compared.
pub fn diff_envelopes(local: &[EnvelopeEntry], peer: &[EnvelopeEntry]) -> ScrollDiff {
    let local_map: BTreeMap<String, (Coordinate, &str)> =
        local.iter().map(|e| (e.coordinate.to_string(), (e.coordinate, e.hash.as_str()))).collect();
    let peer_map: BTreeMap<String, (Coordinate, &str)> =
        peer.iter().map(|e| (e.coordinate.to_string(), (e.coordinate, e.hash.as_str()))).collect();

    let mut diff = ScrollDiff::default();
    for (key, (coord, hash)) in &local_map {
        match peer_map.get(key) {
            None => diff.only_local.push(EnvelopeEntry { coordinate: *coord, hash: hash.to_string() }),
            Some((_, peer_hash)) if *peer_hash != *hash => {
                diff.changed.push(ChangedEntry {
                    coordinate: *coord,
                    local_hash: hash.to_string(),
                    peer_hash: peer_hash.to_string(),
                });
            }
            _ => {}
        }
    }
    for (key, (coord, hash)) in &peer_map {
        if !local_map.contains_key(key) {
            diff.only_peer.push(EnvelopeEntry { coordinate: *coord, hash: hash.to_string() });
        }
    }
    diff
}

/// The change scrolls that turn `base` into `source`: for every
/// coordinate where the two lattices differ, the source's stored form
/// (or a removal, for coordinates only `base` holds), each carrying
/// the base's current hash as its base hash. Applying the result to
/// `base` with [`merge`] is conflict-free by construction.
pub fn changes_between(source: &MappedLattice, base: &MappedLattice) -> Vec<ChangeScroll> {
    let mut changes = Vec::new();
    // Coordinates the source holds whose base state differs (new or
    // changed): content from the source, base hash from the base.
    for entry in envelope(source) {
        let base_hash = current_hash(base, &entry.coordinate);
        if base_hash.as_deref() != Some(entry.hash.as_str()) {
            changes.push(ChangeScroll {
                coordinate: entry.coordinate,
                base_hash,
                content: source.read_scroll(&entry.coordinate),
            });
        }
    }
    // Coordinates only the base holds: removals.
    for entry in envelope(base) {
        if current_hash(source, &entry.coordinate).is_none() {
            changes.push(ChangeScroll {
                coordinate: entry.coordinate,
                base_hash: Some(entry.hash.clone()),
                content: None,
            });
        }
    }
    changes.sort_by_key(|c| c.coordinate.to_string());
    changes
}

/// Apply change scrolls to `lattice`, one coordinate at a time. A
/// change applies when the lattice's current hash equals the change's
/// base hash; a change whose incoming content already matches the
/// current scroll is reported as already applied; anything else is a
/// conflict and is skipped — the scroll is never silently overwritten.
pub fn merge(lattice: &mut MappedLattice, changes: &[ChangeScroll]) -> MergeReport {
    let mut report = MergeReport::default();
    for change in changes {
        // An empty stored form is a removal: a scroll exists only when
        // it holds at least a byte.
        let content: Option<&str> = change.content.as_deref().filter(|c| !c.is_empty());
        let incoming_hash = content.map(|c| hash_stored(c.as_bytes()));
        let current = current_hash(lattice, &change.coordinate);
        if current == incoming_hash {
            report.already_applied.push(change.coordinate);
        } else if current == change.base_hash {
            match content {
                // The stored form is authoritative at this layer: the
                // byte path, so binary scrolls sync as faithfully as
                // text.
                Some(text) => lattice.write_scroll_raw(change.coordinate, text.to_string()),
                None => lattice.delete_scroll(change.coordinate),
            }
            report.applied.push(change.coordinate);
        } else {
            report.conflicts.push(Conflict {
                coordinate: change.coordinate,
                base_hash: change.base_hash.clone(),
                current_hash: current,
                incoming_hash,
            });
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use libphext::phext::to_coordinate;

    /// A three-scroll phext as literal bytes: "alpha" at the root
    /// (1.1.1/1.1.1/1.1.1), "beta" one scroll break later
    /// (1.1.1/1.1.1/1.1.2), "gamma" one section break later
    /// (1.1.1/1.1.1/1.2.1). Authored as text here, never by sync code.
    fn fixture() -> MappedLattice {
        MappedLattice::from_bytes(b"alpha\x17beta\x18gamma")
    }

    #[test]
    fn envelope_hashes_match_the_seed_convention() {
        let lattice = fixture();
        let env = envelope(&lattice);
        assert_eq!(env.len(), 3);
        assert_eq!(env[0].coordinate, to_coordinate("1.1.1/1.1.1/1.1.1"));
        assert_eq!(env[0].hash, hash_stored(b"alpha"));
        assert_eq!(env[1].hash, hash_stored(b"beta"));
        assert_eq!(env[2].hash, hash_stored(b"gamma"));
    }

    #[test]
    fn diff_of_identical_envelopes_is_empty() {
        let lattice = fixture();
        let env = envelope(&lattice);
        assert!(diff_envelopes(&env, &env).is_empty());
    }

    #[test]
    fn diff_names_exactly_the_edited_coordinates() {
        let base = fixture();
        let mut edited = fixture();
        edited.write_scroll(to_coordinate("1.1.1/1.1.1/1.1.2"), "beta, revised".to_string());
        edited.write_scroll(to_coordinate("1.1.1/1.1.1/1.1.3"), "delta, new".to_string());
        edited.delete_scroll(to_coordinate("1.1.1/1.1.1/1.2.1"));
        let diff = diff_envelopes(&envelope(&base), &envelope(&edited));
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].coordinate, to_coordinate("1.1.1/1.1.1/1.1.2"));
        assert_eq!(diff.only_local.len(), 1, "the deleted scroll");
        assert_eq!(diff.only_local[0].coordinate, to_coordinate("1.1.1/1.1.1/1.2.1"));
        assert_eq!(diff.only_peer.len(), 1, "the added scroll");
        assert_eq!(diff.only_peer[0].coordinate, to_coordinate("1.1.1/1.1.1/1.1.3"));
        let _ = base;
    }

    #[test]
    fn merge_reproduces_the_edited_bytes() {
        let base = fixture();
        let mut edited = fixture();
        edited.write_scroll(to_coordinate("1.1.1/1.1.1/1.1.2"), "beta, revised".to_string());
        edited.write_scroll(to_coordinate("1.1.1/1.1.1/1.1.3"), "delta, new".to_string());
        edited.delete_scroll(to_coordinate("1.1.1/1.1.1/1.2.1"));

        let mut target = fixture();
        let changes = changes_between(&edited, &base);
        let report = merge(&mut target, &changes);
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.applied.len(), 3);
        assert_eq!(target.to_phext_bytes(), edited.to_phext_bytes());

        // Re-sending the same changes is a no-op, not a conflict.
        let report = merge(&mut target, &changes);
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(report.already_applied.len(), 3);
        assert!(report.applied.is_empty());
    }

    #[test]
    fn merge_reports_a_planted_conflict_and_keeps_the_scroll() {
        let base = fixture();
        let mut edited = fixture();
        edited.write_scroll(to_coordinate("1.1.1/1.1.1/1.1.2"), "beta, revised".to_string());
        let changes = changes_between(&edited, &base);

        // The receiver edited the same scroll after the base was taken.
        let mut target = fixture();
        target.write_scroll(to_coordinate("1.1.1/1.1.1/1.1.2"), "beta, receiver's own edit".to_string());
        let report = merge(&mut target, &changes);
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.conflicts[0].coordinate, to_coordinate("1.1.1/1.1.1/1.1.2"));
        assert!(report.applied.is_empty());
        assert_eq!(
            target.read_scroll(&to_coordinate("1.1.1/1.1.1/1.1.2")).as_deref(),
            Some("beta, receiver's own edit"),
            "a conflicted scroll is never silently overwritten"
        );
    }
}
