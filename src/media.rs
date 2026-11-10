//! Media scrolls: images stored inside a phext as canonical binary scrolls.
//!
//! The format of record is libphext's binary scroll (libphext-rs 0.5.0,
//! `libphext::binary`): a scroll whose stored form is the header line
//! `phext-binary:1:base64`, a newline, and the payload as RFC 4648 base64
//! (padding required, no line wrapping). A header-prefixed scroll is binary
//! by definition; a well-formed one decodes strictly; a malformed one is
//! fetched verbatim as its own bytes (see `MappedLattice::fetch_binary`).
//! This module adds the editor's media conventions on top of that format —
//! it does not redefine any of it.
//!
//! Coordinate convention (per phext file): media lives under the reserved
//! root 9.9.9/9.9.9. For slot n >= 1 the image payload (one binary scroll)
//! sits at 9.9.9/9.9.9/1.1.n and its metadata — a small JSON text scroll
//! carrying the file name, content type, and payload size — at
//! 9.9.9/9.9.9/1.2.n. Allocation takes the smallest slot whose two
//! coordinates are both unpopulated, so it is deterministic for a given
//! file state and cannot collide with an existing scroll.

use libphext::phext::{Coordinate, XCoordinate, YCoordinate, ZCoordinate};

fn media_coord(section: usize, slot: usize) -> Coordinate {
    Coordinate {
        z: ZCoordinate { library: 9, shelf: 9, series: 9 },
        y: YCoordinate { collection: 9, volume: 9, book: 9 },
        x: XCoordinate { chapter: 1, section, scroll: slot },
    }
}

/// Where slot `slot`'s image payload (a binary scroll) lives:
/// 9.9.9/9.9.9/1.1.n.
pub fn media_payload_coord(slot: usize) -> Coordinate {
    media_coord(1, slot)
}

/// Where slot `slot`'s metadata scroll (JSON text) lives:
/// 9.9.9/9.9.9/1.2.n.
pub fn media_metadata_coord(slot: usize) -> Coordinate {
    media_coord(2, slot)
}

/// The media slot a coordinate belongs to, if it is a media payload
/// coordinate (9.9.9/9.9.9/1.1.n).
pub fn media_slot_of(coord: &Coordinate) -> Option<usize> {
    if coord.z.library == 9
        && coord.z.shelf == 9
        && coord.z.series == 9
        && coord.y.collection == 9
        && coord.y.volume == 9
        && coord.y.book == 9
        && coord.x.chapter == 1
        && coord.x.section == 1
        && coord.x.scroll >= 1
    {
        Some(coord.x.scroll)
    } else {
        None
    }
}

/// True when `stored` is a well-formed binary scroll: it carries the
/// canonical header and its body is strict base64 (an empty payload —
/// header plus newline, nothing after — is well-formed). A header-prefixed
/// scroll whose body is not strict base64 is malformed: by the format's
/// fetch rule it is text, fetched verbatim, and the text write path may
/// still edit it. Only well-formed binary scrolls are protected from text
/// writes.
pub fn is_well_formed_binary_scroll(stored: &str) -> bool {
    libphext::binary::decode_scroll(stored).is_some()
}

/// A raster image format the editor accepts, identified by magic bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageKind {
    /// The Content-Type the payload is served with.
    pub content_type: &'static str,
    /// The file extension stored names are normalized to.
    pub extension: &'static str,
}

pub const PNG: ImageKind = ImageKind { content_type: "image/png", extension: "png" };
pub const JPEG: ImageKind = ImageKind { content_type: "image/jpeg", extension: "jpg" };
pub const GIF: ImageKind = ImageKind { content_type: "image/gif", extension: "gif" };
pub const WEBP: ImageKind = ImageKind { content_type: "image/webp", extension: "webp" };
pub const AVIF: ImageKind = ImageKind { content_type: "image/avif", extension: "avif" };

/// Identify an image payload by its magic bytes — never by a file name or
/// a client-claimed type. SVG is deliberately absent: it is a scriptable
/// document format, not a raster, and is never accepted or served by the
/// image path, so there is no stored-XSS surface to make inert.
pub fn sniff_image(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(PNG);
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(JPEG);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(GIF);
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(WEBP);
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && matches!(&bytes[8..12], b"avif" | b"avis") {
        return Some(AVIF);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_coordinates_follow_the_convention() {
        assert_eq!(format!("{}", media_payload_coord(1)), "9.9.9/9.9.9/1.1.1");
        assert_eq!(format!("{}", media_metadata_coord(1)), "9.9.9/9.9.9/1.2.1");
        assert_eq!(format!("{}", media_payload_coord(7)), "9.9.9/9.9.9/1.1.7");
        assert_eq!(media_slot_of(&media_payload_coord(7)), Some(7));
        assert_eq!(media_slot_of(&media_metadata_coord(7)), None);
        let origin = libphext::phext::default_coordinate();
        assert_eq!(media_slot_of(&origin), None);
    }

    #[test]
    fn sniffing_reads_magic_bytes_only() {
        assert_eq!(sniff_image(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00]), Some(PNG));
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]), Some(JPEG));
        assert_eq!(sniff_image(b"GIF89a...."), Some(GIF));
        assert_eq!(sniff_image(b"GIF87a...."), Some(GIF));
        assert_eq!(sniff_image(b"RIFF\x00\x00\x00\x00WEBP"), Some(WEBP));
        assert_eq!(sniff_image(b"\x00\x00\x00\x18ftypavif"), Some(AVIF));
        assert_eq!(sniff_image(b"\x00\x00\x00\x18ftypavis"), Some(AVIF));
        // Not images, however they are named: text, empty, and SVG.
        assert_eq!(sniff_image(b"plain text"), None);
        assert_eq!(sniff_image(b""), None);
        assert_eq!(sniff_image(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>"), None);
        // Truncated magics do not count.
        assert_eq!(sniff_image(&[0x89, b'P', b'N']), None);
        assert_eq!(sniff_image(b"RIFF\x00\x00\x00\x00AVI "), None);
    }

    #[test]
    fn well_formed_means_header_plus_strict_base64() {
        // The 1x1 GIF: header line, newline, strict base64.
        assert!(is_well_formed_binary_scroll(
            "phext-binary:1:base64\nR0lGODlhAQABAIAAAP///wAAACH5BAEAAAAALAAAAAABAAEAAAICRAEAOw=="
        ));
        // Header + newline alone is the empty payload: well-formed.
        assert!(is_well_formed_binary_scroll("phext-binary:1:base64\n"));
        // A bare header with no newline is not a binary scroll at all.
        assert!(!is_well_formed_binary_scroll("phext-binary:1:base64"));
        // Header-prefixed but not strict base64: malformed, i.e. text.
        assert!(!is_well_formed_binary_scroll("phext-binary:1:base64\n!!!not-base64!!!"));
        assert!(!is_well_formed_binary_scroll("phext-binary:1:base64\nR0lGODlhAQA"));
        // Ordinary text is not binary.
        assert!(!is_well_formed_binary_scroll("just a scroll of words"));
    }
}
