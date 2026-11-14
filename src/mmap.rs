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
use std::io;
use memmap2::Mmap;
use libphext::phext::Coordinate;
use crate::index::{LatticeIndex, ScrollSpan};

/// One member phext (a "universe") of a chain file: its content byte
/// range within the whole buffer, chain separators excluded.
#[derive(Debug, Clone, Copy)]
struct MemberSpan {
    start: usize,
    end: usize,
}

/// Split a phext stream into member ranges, byte-exactly as libphext's
/// `parse_chains` splits text: CHAIN_CLOSE (0x03) separates chains,
/// CHAIN_BREAK (0x02) separates the member phexts inside a chain, and a
/// trailing CHAIN_CLOSE does not start a new chain. Returns the member
/// ranges in stream order (members of successive chains flattened), the
/// chain count, and whether the stream ended closed. A plain phext
/// yields exactly one member covering the whole buffer, one chain,
/// unclosed — every pre-chain phext reads this way.
fn split_chain(bytes: &[u8]) -> (Vec<MemberSpan>, usize, bool) {
    use libphext::phext::{CHAIN_BREAK_U8, CHAIN_CLOSE_U8};
    let ends_closed = bytes.last() == Some(&CHAIN_CLOSE_U8);
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == CHAIN_CLOSE_U8 {
            segments.push((start, i));
            start = i + 1;
        }
    }
    if !ends_closed {
        segments.push((start, bytes.len()));
    }
    if segments.is_empty() {
        segments.push((0, 0));
    }
    let chains = segments.len();
    let mut members = Vec::new();
    for &(s, e) in &segments {
        let mut member_start = s;
        for i in s..e {
            if bytes[i] == CHAIN_BREAK_U8 {
                members.push(MemberSpan { start: member_start, end: i });
                member_start = i + 1;
            }
        }
        members.push(MemberSpan { start: member_start, end: e });
    }
    (members, chains, ends_closed)
}

/// A file is chain-class when its stream structure is anything other
/// than the plain-phext shape (one chain, one member, unclosed).
fn is_chain_layout(members: &[MemberSpan], chains: usize, closed: bool) -> bool {
    !(chains == 1 && members.len() == 1 && !closed)
}

/// Build one member's index over the whole buffer: the member's slice is
/// scanned on its own (coordinates restart at each member, exactly as
/// libphext reads a member phext), then its spans are shifted to
/// absolute offsets into the buffer.
fn member_index(bytes: &[u8], span: MemberSpan) -> LatticeIndex {
    let raw = LatticeIndex::build(&bytes[span.start..span.end]);
    if span.start == 0 {
        return raw;
    }
    let entries: Vec<(Coordinate, ScrollSpan)> = raw
        .coordinates()
        .iter()
        .copied()
        .zip(raw.spans().iter().map(|s| ScrollSpan {
            start: s.start + span.start,
            end: s.end + span.start,
        }))
        .collect();
    LatticeIndex::from_entries(entries, bytes.len())
}

/// The chain layout of a buffer: member ranges, one index per member,
/// and the chain-class flag.
fn chain_layout(bytes: &[u8]) -> (Vec<MemberSpan>, Vec<LatticeIndex>, bool) {
    let (members, chains, closed) = split_chain(bytes);
    let chain_class = is_chain_layout(&members, chains, closed);
    let indexes = members.iter().map(|m| member_index(bytes, *m)).collect();
    (members, indexes, chain_class)
}

/// A memory-mapped phext document with copy-on-write editing.
///
/// The document may be a chain (a multiverse): several member phexts —
/// universes — in one file. All read/write methods operate on the
/// *active* universe (see `universe` / `set_universe`); a plain phext
/// is a chain of one universe and behaves exactly as it always has.
pub struct MappedLattice {
    /// The underlying memory-mapped file (read-only).
    ///
    /// `None` only while `save` has dropped the map so the file can be
    /// replaced. Windows refuses that rename while a view is still mapped.
    mmap: Option<Mmap>,
    /// Path to the backing file.
    path: PathBuf,
    /// The active universe's coordinate → byte-span index.
    /// Mirrors `member_indexes[active_member]`.
    index: LatticeIndex,
    /// Copy-on-write overlay: edited scrolls stored as owned Strings.
    /// If a coordinate is in the overlay, it supersedes the mmap span.
    /// The overlay belongs to the active universe; switching universes
    /// is refused while it is non-empty.
    overlay: HashMap<Coordinate, ScrollOverlay>,
    /// Temp file handle kept alive for from_bytes mmap backing.
    _temp_file: Option<PathBuf>,
    /// Member ranges of the chain this file holds; exactly one entry
    /// covering the whole buffer for a plain (chain-free) phext.
    members: Vec<MemberSpan>,
    /// Every member's index, parallel to `members`.
    member_indexes: Vec<LatticeIndex>,
    /// The active member (0-based); universes are numbered from 1.
    active_member: usize,
    /// True when the file's stream structure is a real chain (see
    /// `is_chain_layout`). Chain files are read-only through `save`.
    chain_class: bool,
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

/// Attach the operation and full path to an I/O failure.
///
/// `err`'s Display (for example `The system cannot find the file specified. (os error 2)`)
/// is kept, and the path is never dropped.
fn io_at(op: &str, path: &Path, err: io::Error) -> io::Error {
    io::Error::new(err.kind(), format!("{op} {}: {err}", path.display()))
}

impl MappedLattice {
    /// Open a phext file and build the lattice index.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<MappedLattice> {
        let path = path.as_ref().to_path_buf();
        let mmap = map_file(&path)?;
        let (members, member_indexes, chain_class) = chain_layout(&mmap);
        let index = member_indexes[0].clone();

        Ok(MappedLattice {
            mmap: Some(mmap),
            path,
            index,
            overlay: HashMap::new(),
            _temp_file: None,
            members,
            member_indexes,
            active_member: 0,
            chain_class,
        })
    }

    /// Open a phext file with an index built elsewhere (e.g. loaded from a
    /// `sidecar`), skipping the O(n) scan. Fails if the index was built for a
    /// buffer of a different length. A chain-class file ignores the
    /// supplied index — a persisted span table describes one universe,
    /// not a chain — and is opened with a fresh scan instead.
    pub fn open_with_index<P: AsRef<Path>>(path: P, index: LatticeIndex) -> io::Result<MappedLattice> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::File::open(&path).map_err(|err| io_at("open", &path, err))?;
        let mmap = unsafe { Mmap::map(&file) }.map_err(|err| io_at("memory-map", &path, err))?;
        if mmap.len() != index.buffer_len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!(
                "index covers {} bytes but {} has {}", index.buffer_len(), path.display(), mmap.len())));
        }
        let (members, chains, closed) = split_chain(&mmap);
        if is_chain_layout(&members, chains, closed) {
            drop(mmap);
            return MappedLattice::open(&path);
        }
        Ok(MappedLattice {
            mmap: Some(mmap),
            path,
            index: index.clone(),
            overlay: HashMap::new(),
            _temp_file: None,
            members,
            member_indexes: vec![index],
            active_member: 0,
            chain_class: false,
        })
    }

    /// Create a MappedLattice from raw bytes (for testing or in-memory use).
    ///
    /// The bytes are written under `std::env::temp_dir()` (never a hard-coded
    /// `/tmp`) and mapped from that same file handle. The temp directory is
    /// created if it is missing.
    pub fn from_bytes(bytes: &[u8]) -> MappedLattice {
        match mapped_from_bytes(bytes) {
            Ok(lattice) => lattice,
            Err(err) => panic!("MappedLattice::from_bytes: {err}"),
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
            let bytes = self.bytes();
            let bytes = &bytes[span.start..span.end];
            // Phext content is UTF-8
            Some(String::from_utf8_lossy(bytes).into_owned())
        } else {
            None
        }
    }

    /// The scroll's exact stored bytes: the overlay version if edited,
    /// otherwise the mmap span, uninterpreted. This is the raw layer the
    /// binary-scroll support is built on — a binary scroll's stored form
    /// is text (header + base64), but payloads are recovered from these
    /// bytes without any lossy conversion.
    pub fn read_scroll_bytes(&self, coord: &Coordinate) -> Option<Vec<u8>> {
        if let Some(entry) = self.overlay.get(coord) {
            return match entry {
                ScrollOverlay::Modified(s) | ScrollOverlay::Inserted(s) => {
                    Some(s.clone().into_bytes())
                }
                ScrollOverlay::Deleted => None,
            };
        }
        self.index
            .get(coord)
            .map(|span| self.bytes()[span.start..span.end].to_vec())
    }

    /// The payload a fetch of this scroll returns, mirroring
    /// `libphext::binary::fetch_binary` exactly: a well-formed binary
    /// scroll decodes to its payload; any other scroll (plain text,
    /// malformed binary, unoccupied) yields its own stored bytes
    /// verbatim — empty when the coordinate holds no scroll. Total:
    /// never fails, never panics.
    pub fn fetch_binary(&self, coord: &Coordinate) -> Vec<u8> {
        let stored = self.read_scroll_bytes(coord).unwrap_or_default();
        if let Ok(text) = std::str::from_utf8(&stored) {
            if let Some(payload) = libphext::binary::decode_scroll(text) {
                return payload;
            }
        }
        stored
    }

    // ── Universes (chain members) ────────────────────────────────────

    /// True when this file is a chain: more structure than a plain
    /// phext's single unclosed member.
    pub fn is_chain(&self) -> bool {
        self.chain_class
    }

    /// How many universes (member phexts) this file holds. A plain
    /// phext holds exactly one.
    pub fn universe_count(&self) -> usize {
        self.members.len()
    }

    /// The active universe, numbered from 1.
    pub fn universe(&self) -> usize {
        self.active_member + 1
    }

    /// Per-universe (scroll count, content bytes), in universe order.
    pub fn member_stats(&self) -> Vec<(usize, usize)> {
        self.members
            .iter()
            .zip(self.member_indexes.iter())
            .map(|(m, idx)| (idx.scroll_count(), m.end - m.start))
            .collect()
    }

    /// Make `universe` (numbered from 1) the active universe. Refused
    /// while the overlay holds unsaved edits — an edit never straddles
    /// a switch — and when the universe is out of range.
    pub fn set_universe(&mut self, universe: usize) -> io::Result<()> {
        if universe < 1 || universe > self.members.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "universe ${} out of range: {} holds {} universe(s)",
                    universe,
                    self.path.display(),
                    self.members.len()
                ),
            ));
        }
        if !self.overlay.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsaved changes; save or discard them before switching universes",
            ));
        }
        self.active_member = universe - 1;
        self.index = self.member_indexes[self.active_member].clone();
        Ok(())
    }

    /// A scroll's exact stored bytes in `universe` (numbered from 1),
    /// without switching the active universe. The overlay applies only
    /// when `universe` is the active one.
    pub fn read_scroll_bytes_in(&self, universe: usize, coord: &Coordinate) -> Option<Vec<u8>> {
        if universe == self.universe() {
            return self.read_scroll_bytes(coord);
        }
        let idx = self.member_indexes.get(universe.checked_sub(1)?)?;
        idx.get(coord)
            .map(|span| self.bytes()[span.start..span.end].to_vec())
    }

    /// A scroll's content in `universe` (numbered from 1), without
    /// switching the active universe.
    pub fn read_scroll_in(&self, universe: usize, coord: &Coordinate) -> Option<String> {
        self.read_scroll_bytes_in(universe, coord)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Write content to a scroll at the given coordinate.
    /// If the coordinate already exists (in mmap or overlay), it's modified.
    /// If it's new, it's inserted.
    ///
    /// Binary-scroll guard: a well-formed binary scroll (an image, say)
    /// cannot be replaced through the text path — its stored form is a
    /// header plus base64, and a text edit would corrupt the payload
    /// silently. Returns false when the write is refused for that reason;
    /// the scroll is left exactly as it was. Writing the identical stored
    /// form back is a no-op success. Byte payloads go through
    /// `write_binary` / `write_bytes` instead.
    pub fn write_scroll(&mut self, coord: Coordinate, content: String) -> bool {
        if let Some(existing) = self.read_scroll(&coord) {
            if crate::media::is_well_formed_binary_scroll(&existing) && existing != content {
                return false;
            }
        }
        self.write_scroll_raw(coord, content);
        true
    }

    /// Store a scroll's stored form verbatim, with no binary guard. This
    /// is the byte path: callers pass either canonical binary scrolls
    /// (from `libphext::binary::encode_scroll`) or stored forms they have
    /// already validated. Prefer `write_scroll` for text and
    /// `write_binary` / `write_bytes` for payloads.
    pub fn write_scroll_raw(&mut self, coord: Coordinate, content: String) {
        if self.index.contains(&coord) {
            self.overlay.insert(coord, ScrollOverlay::Modified(content));
        } else {
            self.overlay.insert(coord, ScrollOverlay::Inserted(content));
        }
    }

    /// Store `payload` at `coord` as a canonical binary scroll (header +
    /// base64 of the exact bytes), whatever the payload holds.
    pub fn write_binary(&mut self, coord: Coordinate, payload: &[u8]) {
        self.write_scroll_raw(coord, libphext::binary::encode_scroll(payload));
    }

    /// Store `payload` at `coord` under the format's ingestion rule
    /// (libphext's `stored_form`, behind `insert_bytes`/`replace_bytes`):
    /// valid UTF-8 is stored as ordinary text; anything else becomes a
    /// binary scroll holding the exact bytes. Ingestion never rejects a
    /// payload.
    pub fn write_bytes(&mut self, coord: Coordinate, payload: &[u8]) {
        match std::str::from_utf8(payload) {
            Ok(text) => self.write_scroll_raw(coord, text.to_string()),
            Err(_) => self.write_binary(coord, payload),
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

    /// Coordinates with unsaved edits (modified, inserted, or deleted), sorted.
    pub fn pending_coordinates(&self) -> Vec<Coordinate> {
        let mut coords: Vec<Coordinate> = self.overlay.keys().copied().collect();
        coords.sort();
        coords
    }

    /// Save to the backing file. Reconstructs the phext from mmap + overlay.
    /// Uses atomic rename via NamedTempFile to prevent partial-write corruption.
    pub fn save(&mut self) -> io::Result<()> {
        if self.chain_class {
            // The editor's save is a whole-file rewrite of one universe;
            // applied to a chain it would silently flatten every member
            // into a single phext. Multiversal writes follow the WAL
            // append path (floor coordinate, appends above it, prior
            // scrolls immutable), which this save is not — so chain
            // files are read-only here, with the reason named.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "chain files are read-only in the editor: {} holds {} universes, and a rewrite save would flatten them into one phext",
                    self.path.display(),
                    self.members.len()
                ),
            ));
        }
        if self.path.as_os_str().is_empty() {
            return Err(io::Error::new(io::ErrorKind::Other, "save: no backing file path"));
        }
        let dest = self.path.clone();
        let (content, spans, clean) = self.serialize();

        // Write to a temp file in the same directory, then atomically rename.
        // This ensures no partial-write state is ever visible on disk.
        let parent = dest.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(parent)
            .map_err(|err| io_at("create temp file in", parent, err))?;
        {
            use std::io::Write;
            let write_result = tmp.write_all(&content);
            if let Err(err) = write_result {
                return Err(io_at("write", tmp.path(), err));
            }
        }
        tmp.as_file().sync_all().map_err(|err| io_at("sync", tmp.path(), err))?;

        // Drop the map before the rename. On Windows the destination still
        // has a mapped view, and MoveFileEx fails with ERROR_USER_MAPPED_FILE
        // (os error 1224): "The requested operation cannot be performed on a
        // file with a user-mapped section open." memmap2 also holds a
        // duplicated file handle until the map is dropped. A zero-length file
        // is not mapped, so the failure shows up once the phext has bytes —
        // edit, insert a scroll, save. Linux rename succeeds either way.
        drop(self.mmap.take());
        let file = match tmp.persist(&dest) {
            Ok(file) => file,
            Err(err) => {
                let replace_err = io_at("replace", &dest, err.error);
                if let Err(reopen) = self.remap(&dest) {
                    return Err(io::Error::new(
                        replace_err.kind(),
                        format!("{replace_err}; reopening the original file failed: {reopen}"),
                    ));
                }
                return Err(replace_err);
            }
        };
        // Map the handle persist just renamed. If that fails, reopen by path.
        self.mmap = Some(match unsafe { Mmap::map(&file) } {
            Ok(mmap) => mmap,
            Err(err) => {
                drop(file);
                map_file(&dest).map_err(|_| io_at("memory-map", &dest, err))?
            }
        });

        // The spans recorded while serializing are the index, unless an edit
        // smuggled in delimiter bytes; then rescan.
        let index = if clean {
            LatticeIndex::from_entries(spans, self.bytes().len())
        } else {
            LatticeIndex::build(self.bytes())
        };
        self.index = index;
        // Single-member files only reach here (save refuses chains), so
        // the active index is the one and only member index.
        self.member_indexes[0] = self.index.clone();
        self.overlay.clear();

        Ok(())
    }

    /// Map `path` into `self.mmap`, replacing whatever was there.
    fn remap(&mut self, path: &Path) -> io::Result<()> {
        self.mmap = Some(map_file(path)?);
        Ok(())
    }

    fn bytes(&self) -> &[u8] {
        self.mmap.as_deref().expect("phext file is not mapped")
    }

    /// Save to a specific path.
    pub fn save_as<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        self.path = path.as_ref().to_path_buf();
        self.save()
    }

    /// Reconstruct the full phext as bytes from mmap + overlay.
    pub fn to_phext_bytes(&self) -> Vec<u8> {
        self.serialize().0
    }

    /// Serialize mmap + overlay, recording each non-empty scroll's span as it is
    /// written. The flag is false if an overlay scroll contains delimiter bytes,
    /// in which case the recorded spans don't describe the output.
    fn serialize(&self) -> (Vec<u8>, Vec<(Coordinate, ScrollSpan)>, bool) {
        use libphext::phext::{
            SCROLL_BREAK, SECTION_BREAK, CHAPTER_BREAK, BOOK_BREAK,
            VOLUME_BREAK, COLLECTION_BREAK, SERIES_BREAK, SHELF_BREAK, LIBRARY_BREAK,
            default_coordinate,
        };

        let coords = self.populated_coordinates();
        let mut output: Vec<u8> = Vec::new();
        let mut spans: Vec<(Coordinate, ScrollSpan)> = Vec::with_capacity(coords.len());
        let mut clean = true;
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
                if self.overlay.contains_key(&coord) && content.bytes().any(crate::sidecar::is_break) {
                    clean = false;
                }
                let start = output.len();
                output.extend_from_slice(content.as_bytes());
                if output.len() > start {
                    spans.push((coord, ScrollSpan { start, end: output.len() }));
                }
            }

            current = coord;
        }

        (output, spans, clean)
    }

    /// Access the raw mmap buffer for search operations.
    /// Returns None if overlay has modifications (use to_phext_bytes instead).
    pub fn raw_buffer(&self) -> Option<&[u8]> {
        if self.overlay.is_empty() {
            self.mmap.as_deref()
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

fn mapped_from_bytes(bytes: &[u8]) -> io::Result<MappedLattice> {
    let (members, member_indexes, chain_class) = chain_layout(bytes);
    let index = member_indexes[0].clone();
    // Map the handle we just wrote. Re-opening by path is a separate lookup
    // and, on Windows, fails with "The system cannot find the file specified.
    // (os error 2)" when the temp path does not resolve.
    let (tmp, file) = tempfile_from_bytes(bytes)?;
    let mmap = unsafe { Mmap::map(&file) }.map_err(|err| io_at("memory-map", &tmp, err))?;
    Ok(MappedLattice {
        mmap: Some(mmap),
        path: PathBuf::new(),
        index,
        overlay: HashMap::new(),
        _temp_file: Some(tmp),
        members,
        member_indexes,
        active_member: 0,
        chain_class,
    })
}

fn map_file(path: &Path) -> io::Result<Mmap> {
    let file = std::fs::File::open(path).map_err(|err| io_at("open", path, err))?;
    unsafe { Mmap::map(&file) }.map_err(|err| io_at("memory-map", path, err))
}

/// Write bytes under `std::env::temp_dir` and return the path plus the open file.
fn tempfile_from_bytes(bytes: &[u8]) -> io::Result<(PathBuf, std::fs::File)> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir();
    std::fs::create_dir_all(&dir).map_err(|err| io_at("create directory", &dir, err))?;
    let path = dir.join(format!("phext-lattice-{}-{id}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|err| io_at("create", &path, err))?;
    file.write_all(bytes).map_err(|err| io_at("write", &path, err))?;
    file.flush().map_err(|err| io_at("flush", &path, err))?;
    file.sync_all().map_err(|err| io_at("sync", &path, err))?;
    Ok((path, file))
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

    #[test]
    fn open_missing_file_names_the_operation_and_full_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no such file.phext");
        let err = match MappedLattice::open(&path) {
            Err(err) => err,
            Ok(_) => panic!("opened missing file {}", path.display()),
        };
        let msg = err.to_string();
        let full = path.display().to_string();
        assert!(msg.contains("open "), "{msg}");
        assert!(msg.contains(&full), "{msg} missing {full}");
        assert!(msg.contains("os error") || msg.contains("No such file") || msg.contains("cannot find"), "{msg}");
    }

    #[test]
    fn from_bytes_temp_file_is_under_std_temp_dir() {
        let lattice = MappedLattice::from_bytes(b"temp backed");
        let tmp = lattice._temp_file.as_ref().expect("temp path recorded");
        let temp_root = std::env::temp_dir();
        assert!(
            tmp.starts_with(&temp_root),
            "{} is not under {}",
            tmp.display(),
            temp_root.display()
        );
        assert!(tmp.is_file(), "missing {}", tmp.display());
        // The backing file is created with Path::join, not a hard-coded "/tmp/..." string.
        assert_eq!(tmp.parent().unwrap(), temp_root.as_path());
    }

    #[test]
    fn save_records_the_same_index_a_scan_would_build() {
        let dir = std::env::temp_dir().join(format!("phext-lattice-save-index-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doc.phext");
        std::fs::write(&path, test_phext()).unwrap();
        let mut lat = MappedLattice::open(&path).unwrap();
        lat.write_scroll(to_coordinate("1.1.1/1.1.1/1.1.2"), "longer scroll two".into());
        lat.write_scroll(to_coordinate("1.1.1/1.1.2/1.1.1"), "new book".into());
        lat.write_scroll(to_coordinate("2.1.1/1.1.1/1.1.1"), "".into());
        lat.delete_scroll(to_coordinate("1.1.1/1.1.1/1.2.1"));
        lat.save().unwrap();
        let scanned = LatticeIndex::build(&std::fs::read(&path).unwrap());
        assert_eq!(lat.index().coordinates(), scanned.coordinates());
        assert_eq!(lat.index().spans(), scanned.spans());

        // an edit containing a delimiter falls back to a rescan
        lat.write_scroll(to_coordinate("1.1.1/1.1.1/1.1.1"), "split\x17here".into());
        lat.save().unwrap();
        let scanned = LatticeIndex::build(&std::fs::read(&path).unwrap());
        assert_eq!(lat.index().coordinates(), scanned.coordinates());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Edit an existing scroll, insert one that was not in the file, save,
    /// and read both back from a fresh open. This is the phext-edit flow
    /// (POST /api/update, then POST /api/save) that failed on Windows with
    /// `replace <path>` while the destination was still mapped.
    #[test]
    fn save_after_edit_and_insert_scroll_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.phext");
        std::fs::write(&path, test_phext()).unwrap();

        let edited = to_coordinate("1.1.1/1.1.1/1.1.1");
        let kept = to_coordinate("1.1.1/1.1.1/1.1.2");
        let inserted = to_coordinate("1.1.1/1.1.1/1.1.3");
        let section = to_coordinate("1.1.1/1.1.1/1.2.1");

        let mut lat = MappedLattice::open(&path).unwrap();
        lat.write_scroll(edited, "edited scroll one".into());
        lat.write_scroll(inserted, "brand new scroll".into());
        lat.save().unwrap_or_else(|err| panic!("save failed: {err}"));
        assert!(!lat.is_dirty());
        assert_eq!(lat.read_scroll(&edited).as_deref(), Some("edited scroll one"));
        assert_eq!(lat.read_scroll(&inserted).as_deref(), Some("brand new scroll"));

        let reloaded = MappedLattice::open(&path).unwrap();
        assert_eq!(reloaded.read_scroll(&edited).as_deref(), Some("edited scroll one"));
        assert_eq!(reloaded.read_scroll(&kept).as_deref(), Some("scroll two"));
        assert_eq!(reloaded.read_scroll(&inserted).as_deref(), Some("brand new scroll"));
        assert_eq!(reloaded.read_scroll(&section).as_deref(), Some("section two"));
        let scanned = LatticeIndex::build(&std::fs::read(&path).unwrap());
        assert_eq!(reloaded.index().coordinates(), scanned.coordinates());
        assert_eq!(reloaded.index().spans(), scanned.spans());
    }

    /// A failed replace still names the operation and the full path. The UI
    /// shows that string as `disk save failed: save failed: replace <path>: …`.
    #[test]
    fn failed_replace_names_the_operation_and_full_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.phext");
        std::fs::write(&path, test_phext()).unwrap();
        let mut lat = MappedLattice::open(&path).unwrap();
        let inserted = to_coordinate("1.1.1/1.1.1/1.1.3");
        lat.write_scroll(inserted, "brand new scroll".into());

        // A directory cannot be the target of a file rename, so persist fails
        // on the same step Windows hits while the phext is still mapped.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let err = lat.save().expect_err("replace onto a directory");
        let msg = err.to_string();
        let full = path.display().to_string();
        assert!(msg.starts_with("replace "), "{msg}");
        assert!(msg.contains(&full), "{msg} missing {full}");
        // The insert is still in the overlay; the disk replace did not land.
        assert!(lat.is_dirty());
        assert_eq!(lat.read_scroll(&inserted).as_deref(), Some("brand new scroll"));
    }

    /// A three-universe closed chain, built with libphext's own Chain
    /// serialization — the same bytes teixml-to-mutable writes.
    fn chain_bytes() -> Vec<u8> {
        let chain = libphext::phext::Chain {
            members: vec![
                "index scroll\x17original one".to_string(),
                "universe two root".to_string(),
                "universe three root\x17universe three scroll two".to_string(),
            ],
            closed: true,
        };
        chain.serialize().into_bytes()
    }

    #[test]
    fn split_chain_matches_libphext_parse_chains() {
        let bytes = chain_bytes();
        let text = String::from_utf8(bytes.clone()).unwrap();
        let parsed = libphext::phext::parse_chains(&text);
        assert_eq!(parsed.len(), 1);
        let (members, chains, closed) = split_chain(&bytes);
        assert_eq!(chains, 1);
        assert!(closed);
        assert_eq!(members.len(), parsed[0].members.len());
        for (span, member) in members.iter().zip(parsed[0].members.iter()) {
            assert_eq!(&bytes[span.start..span.end], member.as_bytes());
        }
        // A plain phext: one member, whole buffer, unclosed.
        let (members, chains, closed) = split_chain(&test_phext());
        assert_eq!((members.len(), chains, closed), (1, 1, false));
        assert_eq!((members[0].start, members[0].end), (0, test_phext().len()));
    }

    #[test]
    fn chain_opens_with_universes_and_reads_each() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chain.phext");
        std::fs::write(&path, chain_bytes()).unwrap();
        let mut lat = MappedLattice::open(&path).unwrap();
        assert!(lat.is_chain());
        assert_eq!(lat.universe_count(), 3);
        assert_eq!(lat.universe(), 1);
        let base = to_coordinate("1.1.1/1.1.1/1.1.1");
        let second = to_coordinate("1.1.1/1.1.1/1.1.2");
        // $1: two scrolls; addresses restart in every universe.
        assert_eq!(lat.read_scroll(&base).as_deref(), Some("index scroll"));
        assert_eq!(lat.read_scroll(&second).as_deref(), Some("original one"));
        // Read-through into another universe without switching.
        assert_eq!(lat.read_scroll_in(2, &base).as_deref(), Some("universe two root"));
        assert_eq!(lat.read_scroll_in(3, &second).as_deref(), Some("universe three scroll two"));
        assert!(lat.read_scroll_in(4, &base).is_none());
        // Switch and read through the ordinary path.
        lat.set_universe(3).unwrap();
        assert_eq!(lat.universe(), 3);
        assert_eq!(lat.read_scroll(&base).as_deref(), Some("universe three root"));
        assert_eq!(lat.scroll_count(), 2);
        let stats = lat.member_stats();
        assert_eq!(stats.len(), 3);
        assert_eq!(stats[0].0, 2);
        assert_eq!(stats[1].0, 1);
    }

    #[test]
    fn plain_phext_is_not_a_chain_and_stays_editable() {
        let lat = MappedLattice::from_bytes(&test_phext());
        assert!(!lat.is_chain());
        assert_eq!(lat.universe_count(), 1);
        assert_eq!(lat.universe(), 1);
    }

    #[test]
    fn set_universe_refuses_with_unsaved_edits_and_out_of_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chain.phext");
        std::fs::write(&path, chain_bytes()).unwrap();
        let mut lat = MappedLattice::open(&path).unwrap();
        assert!(lat.set_universe(4).is_err());
        assert!(lat.set_universe(0).is_err());
        lat.write_scroll(to_coordinate("1.1.1/1.1.1/1.1.1"), "edited".into());
        let err = lat.set_universe(2).unwrap_err();
        assert!(err.to_string().contains("unsaved changes"), "{err}");
        assert_eq!(lat.universe(), 1);
    }

    #[test]
    fn save_refuses_chain_files_and_names_why() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chain.phext");
        std::fs::write(&path, chain_bytes()).unwrap();
        let mut lat = MappedLattice::open(&path).unwrap();
        lat.write_scroll(to_coordinate("1.1.1/1.1.1/1.1.1"), "edited".into());
        let err = lat.save().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("read-only"), "{msg}");
        assert!(msg.contains("3 universes"), "{msg}");
        // The file on disk is byte-identical: nothing was flattened.
        assert_eq!(std::fs::read(&path).unwrap(), chain_bytes());
    }
}
