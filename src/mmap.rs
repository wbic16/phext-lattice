/// Memory-mapped phext file access.
///
/// The editor's I/O foundation. Instead of loading entire phext files into
/// a String (which libphext-rs does everywhere), we memory-map the file and
/// build a LatticeIndex over the raw bytes. Scroll content is only read
/// when navigated to.
///
/// For mutation, we use a copy-on-write model: the mmap is read-only,
/// and edits go into an overlay HashMap. On save, we reconstruct the
/// phext from the overlay merged with the original spans.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::io::{self, Write};
use memmap2::Mmap;
use tempfile::NamedTempFile;
use libphext::phext::Coordinate;
use crate::index::LatticeIndex;

/// A memory-mapped phext document with copy-on-write editing.
pub struct MappedLattice {
    /// The underlying memory-mapped file (read-only).
    mmap: Mmap,
    /// Path to the backing file.
    path: PathBuf,
    /// The coordinate → byte-span index built over the mmap.
    index: LatticeIndex,
    /// Copy-on-write overlay: edited scrolls stored as owned Strings.
    /// If a coordinate is in the overlay, it supersedes the mmap span.
    overlay: HashMap<Coordinate, ScrollOverlay>,
    /// Temp file handle kept alive for from_bytes mmap backing.
    _temp_file: Option<PathBuf>,
}

/// An overlay entry for a scroll that has been modified.
#[derive(Debug, Clone)]
enum ScrollOverlay {
    /// Modified content.
    Modified(String),
    /// Scroll has been deleted (exists in mmap but should be omitted on save).
    Deleted,
    /// New scroll inserted (no corresponding mmap span).
    Inserted(String),
}

impl MappedLattice {
    /// Open a phext file and build the lattice index.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<MappedLattice> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::File::open(&path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        let index = LatticeIndex::build(&mmap);

        Ok(MappedLattice {
            mmap,
            path,
            index,
            overlay: HashMap::new(),
            _temp_file: None,
        })
    }

    /// Create a MappedLattice from raw bytes (for testing or in-memory use).
    pub fn from_bytes(bytes: &[u8]) -> MappedLattice {
        // For non-file-backed use, we still build the index.
        // We create a temporary file-backed mmap for API consistency,
        // but for testing we'll use a different approach.
        let index = LatticeIndex::build(bytes);
        // We need a valid Mmap — create a temp file to back it.
        // The file must remain on disk while the mmap is alive.
        let tmp = tempfile_from_bytes(bytes);
        let file = std::fs::File::open(&tmp).unwrap();
        let mmap = unsafe { Mmap::map(&file).unwrap() };

        MappedLattice {
            mmap,
            path: PathBuf::new(),
            index,
            overlay: HashMap::new(),
            _temp_file: Some(tmp),
        }
    }

    /// Read the content of a scroll at the given coordinate.
    /// Returns the overlay version if edited, otherwise reads from mmap.
    pub fn read_scroll(&self, coord: &Coordinate) -> Option<String> {
        // Check overlay first
        if let Some(entry) = self.overlay.get(coord) {
            return match entry {
                ScrollOverlay::Modified(s) | ScrollOverlay::Inserted(s) => Some(s.clone()),
                ScrollOverlay::Deleted => None,
            };
        }

        // Fall back to mmap
        if let Some(span) = self.index.get(coord) {
            let bytes = &self.mmap[span.start..span.end];
            // Phext content is UTF-8
            Some(String::from_utf8_lossy(bytes).into_owned())
        } else {
            None
        }
    }

    /// Write content to a scroll at the given coordinate.
    /// If the coordinate already exists (in mmap or overlay), it's modified.
    /// If it's new, it's inserted.
    pub fn write_scroll(&mut self, coord: Coordinate, content: String) {
        if self.index.contains(&coord) {
            self.overlay.insert(coord, ScrollOverlay::Modified(content));
        } else {
            self.overlay.insert(coord, ScrollOverlay::Inserted(content));
        }
    }

    /// Delete a scroll at the given coordinate.
    pub fn delete_scroll(&mut self, coord: Coordinate) {
        if self.index.contains(&coord) {
            self.overlay.insert(coord, ScrollOverlay::Deleted);
        } else {
            // If it was only in the overlay, just remove it
            self.overlay.remove(&coord);
        }
    }

    /// Check if a coordinate has content (considering overlay).
    pub fn has_scroll(&self, coord: &Coordinate) -> bool {
        if let Some(entry) = self.overlay.get(coord) {
            return !matches!(entry, ScrollOverlay::Deleted);
        }
        self.index.contains(coord)
    }

    /// Get all populated coordinates, including overlay insertions,
    /// excluding overlay deletions. Returned in sorted order.
    pub fn populated_coordinates(&self) -> Vec<Coordinate> {
        let mut coords: Vec<Coordinate> = self.index.coordinates()
            .iter()
            .copied()
            .filter(|c| !matches!(self.overlay.get(c), Some(ScrollOverlay::Deleted)))
            .collect();

        // Add inserted coordinates from overlay
        for (coord, entry) in &self.overlay {
            if matches!(entry, ScrollOverlay::Inserted(_)) {
                coords.push(*coord);
            }
        }

        coords.sort();
        coords.dedup();
        coords
    }

    /// Number of populated scrolls (considering overlay).
    pub fn scroll_count(&self) -> usize {
        self.populated_coordinates().len()
    }

    /// Check if there are unsaved modifications.
    pub fn is_dirty(&self) -> bool {
        !self.overlay.is_empty()
    }

    /// Number of modified/inserted/deleted scrolls.
    pub fn pending_changes(&self) -> usize {
        self.overlay.len()
    }

    /// Save to the backing file. Reconstructs the phext from mmap + overlay.
    ///
    /// Uses an atomic write: content is written to a temp file in the same
    /// directory, fsync'd, then renamed into place. On POSIX, rename(2) is
    /// atomic — readers never see a partial write. (Aetheris, P4 patch)
    pub fn save(&mut self) -> io::Result<()> {
        if self.path.as_os_str().is_empty() {
            return Err(io::Error::new(io::ErrorKind::Other, "No backing file path"));
        }
        let content = self.to_phext_bytes();

        // Write to a temp file in the same directory so rename stays on one fs
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = NamedTempFile::new_in(parent)?;
        tmp.write_all(&content)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.path)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        // Re-map the file and rebuild index
        let file = std::fs::File::open(&self.path)?;
        self.mmap = unsafe { Mmap::map(&file)? };
        self.index = LatticeIndex::build(&self.mmap);
        self.overlay.clear();

        Ok(())
    }

    /// Save to a specific path.
    pub fn save_as<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        self.path = path.as_ref().to_path_buf();
        self.save()
    }

    /// Reconstruct the full phext as bytes from mmap + overlay.
    pub fn to_phext_bytes(&self) -> Vec<u8> {
        use libphext::phext::{
            SCROLL_BREAK, SECTION_BREAK, CHAPTER_BREAK, BOOK_BREAK,
            VOLUME_BREAK, COLLECTION_BREAK, SERIES_BREAK, SHELF_BREAK, LIBRARY_BREAK,
            default_coordinate,
        };

        let coords = self.populated_coordinates();
        let mut output: Vec<u8> = Vec::new();
        let mut current = default_coordinate();

        for coord in coords {
            // Emit delimiters to advance from `current` to `coord`
            while current.z.library < coord.z.library {
                output.push(LIBRARY_BREAK as u8);
                current.library_break();
            }
            while current.z.shelf < coord.z.shelf {
                output.push(SHELF_BREAK as u8);
                current.shelf_break();
            }
            while current.z.series < coord.z.series {
                output.push(SERIES_BREAK as u8);
                current.series_break();
            }
            while current.y.collection < coord.y.collection {
                output.push(COLLECTION_BREAK as u8);
                current.collection_break();
            }
            while current.y.volume < coord.y.volume {
                output.push(VOLUME_BREAK as u8);
                current.volume_break();
            }
            while current.y.book < coord.y.book {
                output.push(BOOK_BREAK as u8);
                current.book_break();
            }
            while current.x.chapter < coord.x.chapter {
                output.push(CHAPTER_BREAK as u8);
                current.chapter_break();
            }
            while current.x.section < coord.x.section {
                output.push(SECTION_BREAK as u8);
                current.section_break();
            }
            while current.x.scroll < coord.x.scroll {
                output.push(SCROLL_BREAK as u8);
                current.scroll_break();
            }

            // Write scroll content
            if let Some(content) = self.read_scroll(&coord) {
                output.extend_from_slice(content.as_bytes());
            }

            current = coord;
        }

        output
    }

    /// Access the raw mmap buffer for search operations.
    /// Returns None if overlay has modifications (use to_phext_bytes instead).
    pub fn raw_buffer(&self) -> Option<&[u8]> {
        if self.overlay.is_empty() {
            Some(&self.mmap)
        } else {
            None
        }
    }

    /// Access the underlying index for navigation queries.
    pub fn index(&self) -> &LatticeIndex {
        &self.index
    }

    /// Get the file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Discard all unsaved changes, reverting to the mmap state.
    pub fn revert(&mut self) {
        self.overlay.clear();
    }
}

impl Drop for MappedLattice {
    fn drop(&mut self) {
        if let Some(ref tmp) = self._temp_file {
            std::fs::remove_file(tmp).ok();
        }
    }
}

/// Helper: write bytes to a temp file and return the path.
fn tempfile_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "phext-lattice-{}-{}", std::process::id(), id
    ));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(bytes).unwrap();
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use libphext::phext::to_coordinate;

    fn test_phext() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"scroll one");
        buf.push(0x17); // scroll break
        buf.extend_from_slice(b"scroll two");
        buf.push(0x18); // section break
        buf.extend_from_slice(b"section two");
        buf
    }

    #[test]
    fn open_and_read() {
        let lattice = MappedLattice::from_bytes(&test_phext());
        let c1 = to_coordinate("1.1.1/1.1.1/1.1.1");
        assert_eq!(lattice.read_scroll(&c1).unwrap(), "scroll one");

        let c2 = to_coordinate("1.1.1/1.1.1/1.1.2");
        assert_eq!(lattice.read_scroll(&c2).unwrap(), "scroll two");

        let c3 = to_coordinate("1.1.1/1.1.1/1.2.1");
        assert_eq!(lattice.read_scroll(&c3).unwrap(), "section two");
    }

    #[test]
    fn edit_overlay() {
        let mut lattice = MappedLattice::from_bytes(&test_phext());
        let c1 = to_coordinate("1.1.1/1.1.1/1.1.1");

        assert!(!lattice.is_dirty());
        lattice.write_scroll(c1, "modified".to_string());
        assert!(lattice.is_dirty());
        assert_eq!(lattice.read_scroll(&c1).unwrap(), "modified");
        assert_eq!(lattice.pending_changes(), 1);
    }

    #[test]
    fn insert_new_scroll() {
        let mut lattice = MappedLattice::from_bytes(&test_phext());
        let new_coord = to_coordinate("2.2.2/2.2.2/2.2.2");

        assert!(!lattice.has_scroll(&new_coord));
        lattice.write_scroll(new_coord, "new content".to_string());
        assert!(lattice.has_scroll(&new_coord));
        assert_eq!(lattice.read_scroll(&new_coord).unwrap(), "new content");
    }

    #[test]
    fn delete_scroll() {
        let mut lattice = MappedLattice::from_bytes(&test_phext());
        let c1 = to_coordinate("1.1.1/1.1.1/1.1.1");

        lattice.delete_scroll(c1);
        assert!(!lattice.has_scroll(&c1));
        assert!(lattice.read_scroll(&c1).is_none());
    }

    #[test]
    fn revert_discards_changes() {
        let mut lattice = MappedLattice::from_bytes(&test_phext());
        let c1 = to_coordinate("1.1.1/1.1.1/1.1.1");

        lattice.write_scroll(c1, "modified".to_string());
        lattice.revert();
        assert!(!lattice.is_dirty());
        assert_eq!(lattice.read_scroll(&c1).unwrap(), "scroll one");
    }

    #[test]
    fn roundtrip_to_phext() {
        let original = test_phext();
        let lattice = MappedLattice::from_bytes(&original);
        let reconstructed = lattice.to_phext_bytes();
        assert_eq!(original, reconstructed);
    }

    #[test]
    fn populated_coordinates_reflects_overlay() {
        let mut lattice = MappedLattice::from_bytes(&test_phext());
        let original_count = lattice.scroll_count();

        // Insert a new scroll
        let new_coord = to_coordinate("5.5.5/5.5.5/5.5.5");
        lattice.write_scroll(new_coord, "far away".to_string());
        assert_eq!(lattice.scroll_count(), original_count + 1);

        // Delete an existing scroll
        let c1 = to_coordinate("1.1.1/1.1.1/1.1.1");
        lattice.delete_scroll(c1);
        assert_eq!(lattice.scroll_count(), original_count); // +1 -1 = same
    }
}
