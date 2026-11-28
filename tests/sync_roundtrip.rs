/// Round-trip proofs for scroll-by-scroll diff and merge
/// (`phext_lattice::sync`, task 29.6.9/8.6.11/7.6.9).
///
/// Two layers:
/// - Fixture proofs on small phexts written as literal bytes in this
///   file — never produced by the sync code under test.
/// - A corpus proof on a real phext the sync code never authored: the
///   first file found at `$PHEXT_SYNC_CORPUS`, the workspace
///   tasks.phext, or the hosted CYOA corpus. Skipped when none exists,
///   in the pattern of `tests/real_phext.rs`. The corpus file is only
///   ever read; every edit happens on in-memory copies.

use phext_lattice::sync::{changes_between, diff_envelopes, envelope, merge};
use phext_lattice::MappedLattice;
use libphext::phext::{to_coordinate, Coordinate};

fn corpus_path() -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("PHEXT_SYNC_CORPUS") {
        candidates.push(p.into());
    }
    candidates.push("/home/hatch/workspace/phext/tasks.phext".into());
    candidates.push("/source/human/choose-your-own-adventure.phext".into());
    candidates.into_iter().find(|p| p.exists())
}

/// Edit exactly three populated coordinates of `lattice` (revise two,
/// delete one) plus add one new scroll. Returns the touched coordinates
/// in (revised, added, deleted) groups.
fn edit_k_scrolls(lattice: &mut MappedLattice) -> (Vec<Coordinate>, Coordinate, Coordinate) {
    let populated = lattice.populated_coordinates();
    assert!(populated.len() >= 3, "corpus must hold at least 3 scrolls");
    let revised: Vec<Coordinate> = populated.iter().take(2).cloned().collect();
    let deleted = populated[2];
    for coord in &revised {
        let mut content = lattice.read_scroll(coord).unwrap_or_default();
        content.push_str("\n[sync round-trip revision]");
        lattice.write_scroll(*coord, content);
    }
    lattice.delete_scroll(deleted);
    // A coordinate far from anything the corpus uses.
    let added = to_coordinate("99.99.99/99.99.99/99.99.99");
    assert!(!lattice.has_scroll(&added), "added coordinate must start empty");
    lattice.write_scroll(added, "sync round-trip addition".to_string());
    (revised, added, deleted)
}

#[test]
fn corpus_diff_and_merge_round_trip() {
    let Some(path) = corpus_path() else {
        eprintln!("Skipping corpus proof: no corpus phext found");
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    let base = MappedLattice::from_bytes(&bytes);
    let base_env = envelope(&base);

    // Diff of a file against itself is empty.
    assert!(diff_envelopes(&base_env, &base_env).is_empty());

    // Edit k scrolls in a copy; the diff names exactly those.
    let mut edited = MappedLattice::from_bytes(&bytes);
    let (revised, added, deleted) = edit_k_scrolls(&mut edited);
    let edited_env = envelope(&edited);
    let diff = diff_envelopes(&base_env, &edited_env);
    assert_eq!(diff.changed.len(), revised.len(), "changed = the revised scrolls");
    for coord in &revised {
        assert!(diff.changed.iter().any(|c| c.coordinate == *coord), "missing revised {coord}");
    }
    assert_eq!(diff.only_peer.len(), 1, "only_peer = the added scroll");
    assert_eq!(diff.only_peer[0].coordinate, added);
    assert_eq!(diff.only_local.len(), 1, "only_local = the deleted scroll");
    assert_eq!(diff.only_local[0].coordinate, deleted);

    // Merge the change scrolls into a fresh copy of the base: the
    // result is the edited file, byte for byte.
    let changes = changes_between(&edited, &base);
    let mut target = MappedLattice::from_bytes(&bytes);
    let report = merge(&mut target, &changes);
    assert!(report.is_clean(), "unexpected conflicts: {:?}", report.conflicts);
    assert_eq!(report.applied.len(), revised.len() + 2, "revised + added + deleted");
    assert_eq!(target.to_phext_bytes(), edited.to_phext_bytes(), "merge must reproduce the edited file byte-exactly");

    // A planted conflict is detected and reported, not merged: move
    // one target scroll to a third state, then replay the changes.
    let mut conflicted = MappedLattice::from_bytes(&bytes);
    let victim = revised[0];
    conflicted.write_scroll(victim, "a third state the sender never saw".to_string());
    let report = merge(&mut conflicted, &changes);
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.conflicts[0].coordinate, victim);
    assert_eq!(
        conflicted.read_scroll(&victim).as_deref(),
        Some("a third state the sender never saw"),
        "a conflicted scroll is never silently overwritten"
    );
    // The non-conflicted changes still applied.
    assert_eq!(report.applied.len(), revised.len() + 1, "the rest of the change set applies");
}

#[test]
fn fixture_diff_and_merge_round_trip() {
    // Literal phext bytes, authored here as text: three scrolls.
    let base = MappedLattice::from_bytes(b"alpha\x17beta\x18gamma");
    assert!(diff_envelopes(&envelope(&base), &envelope(&base)).is_empty());

    let mut edited = MappedLattice::from_bytes(b"alpha\x17beta\x18gamma");
    edited.write_scroll(to_coordinate("1.1.1/1.1.1/1.1.2"), "beta, revised".to_string());

    let changes = changes_between(&edited, &base);
    assert_eq!(changes.len(), 1);
    let mut target = MappedLattice::from_bytes(b"alpha\x17beta\x18gamma");
    let report = merge(&mut target, &changes);
    assert!(report.is_clean());
    assert_eq!(target.to_phext_bytes(), edited.to_phext_bytes());
}
