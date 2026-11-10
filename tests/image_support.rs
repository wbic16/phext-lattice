//! Image support at the lattice layer: canonical binary scroll storage,
//! the byte-ingestion rule, fetch semantics, and the text-write guard.
//! The format under test is libphext 0.5.0's binary scroll
//! (`phext-binary:1:base64` header + strict base64), wired into
//! MappedLattice — these tests pin the semantics the web editor relies on.

use libphext::phext::{Coordinate, XCoordinate, YCoordinate, ZCoordinate};
use phext_lattice::media;
use phext_lattice::MappedLattice;

fn coord(x: usize) -> Coordinate {
    Coordinate {
        z: ZCoordinate { library: 1, shelf: 1, series: 1 },
        y: YCoordinate { collection: 1, volume: 1, book: 1 },
        x: XCoordinate { chapter: 1, section: 1, scroll: x },
    }
}

/// A PNG signature plus payload bytes (0x89 makes the whole thing
/// non-UTF-8, like any real image).
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR-fake-payload\x00\xff";

#[test]
fn binary_scrolls_round_trip_byte_identical() {
    let mut lattice = MappedLattice::from_bytes(b"hello\n");
    let at = media::media_payload_coord(1);
    lattice.write_binary(at, PNG_BYTES);

    // The stored form is the canonical text: header line + base64.
    let stored = lattice.read_scroll(&at).unwrap();
    assert!(stored.starts_with("phext-binary:1:base64\n"), "{stored}");
    assert!(media::is_well_formed_binary_scroll(&stored));

    // Fetch decodes back to the exact bytes.
    assert_eq!(lattice.fetch_binary(&at), PNG_BYTES);

    // And the form survives serialization into a fresh lattice.
    let bytes = lattice.to_phext_bytes();
    let reopened = MappedLattice::from_bytes(&bytes);
    assert_eq!(reopened.read_scroll(&at).unwrap(), stored);
    assert_eq!(reopened.fetch_binary(&at), PNG_BYTES);
}

#[test]
fn ingestion_never_rejects_and_keeps_utf8_as_text() {
    let mut lattice = MappedLattice::from_bytes(b"hello\n");
    // Non-UTF-8 payload: stored as a binary scroll of the exact bytes.
    lattice.write_bytes(coord(2), PNG_BYTES);
    assert!(media::is_well_formed_binary_scroll(&lattice.read_scroll(&coord(2)).unwrap()));
    assert_eq!(lattice.fetch_binary(&coord(2)), PNG_BYTES);
    // UTF-8 payload: stored as ordinary text, editable as text.
    lattice.write_bytes(coord(3), b"plain words");
    assert_eq!(lattice.read_scroll(&coord(3)).unwrap(), "plain words");
    assert!(lattice.write_scroll(coord(3), "edited words".to_string()));
    assert_eq!(lattice.read_scroll(&coord(3)).unwrap(), "edited words");
}

#[test]
fn fetch_binary_is_verbatim_for_text_and_malformed_scrolls() {
    let mut lattice = MappedLattice::from_bytes(b"hello");
    // A plain text scroll fetches as its own bytes.
    assert_eq!(lattice.fetch_binary(&coord(1)), b"hello");
    // A malformed binary scroll (header prefix, non-base64 body) fetches
    // verbatim — it is text by the format's rule.
    lattice.write_scroll_raw(coord(2), "phext-binary:1:base64\n!!!not-base64!!!".to_string());
    assert_eq!(
        lattice.fetch_binary(&coord(2)),
        b"phext-binary:1:base64\n!!!not-base64!!!"
    );
    // An unoccupied coordinate fetches empty.
    assert_eq!(lattice.fetch_binary(&coord(9)), Vec::<u8>::new());
    // A bare header with no newline is not a binary scroll.
    lattice.write_scroll_raw(coord(4), "phext-binary:1:base64".to_string());
    assert!(!media::is_well_formed_binary_scroll(
        &lattice.read_scroll(&coord(4)).unwrap()
    ));
    assert_eq!(lattice.fetch_binary(&coord(4)), b"phext-binary:1:base64");
}

#[test]
fn text_writes_cannot_corrupt_a_binary_scroll() {
    let mut lattice = MappedLattice::from_bytes(b"hello\n");
    let at = media::media_payload_coord(1);
    lattice.write_binary(at, PNG_BYTES);
    let stored = lattice.read_scroll(&at).unwrap();

    // A text write over the binary scroll is refused and changes nothing.
    assert!(!lattice.write_scroll(at, "just some words".to_string()));
    assert_eq!(lattice.read_scroll(&at).unwrap(), stored);
    assert_eq!(lattice.fetch_binary(&at), PNG_BYTES);

    // Writing the identical stored form back is a no-op success.
    assert!(lattice.write_scroll(at, stored.clone()));
    assert_eq!(lattice.fetch_binary(&at), PNG_BYTES);

    // The byte path can still replace it deliberately.
    lattice.write_binary(at, b"GIF89a-replacement");
    assert_eq!(lattice.fetch_binary(&at), b"GIF89a-replacement");

    // A malformed header-prefixed scroll is text: the guard stays out.
    lattice.write_scroll_raw(coord(5), "phext-binary:1:base64\n!!!".to_string());
    assert!(lattice.write_scroll(coord(5), "fixed by hand".to_string()));
    assert_eq!(lattice.read_scroll(&coord(5)).unwrap(), "fixed by hand");
}
