/// Sidecar — persisted lattice, name, and term indexes kept beside a phext.
///
/// Opening a phext normally costs a full scan (span index) plus a pass over the
/// name scrolls, and ranked search needs the whole corpus tokenized. The sidecar
/// stores all three so `IndexedLattice::open` reads a span table and mmaps the
/// rest: name lookups and BM25 queries binary-search sorted text tables in place.
///
/// For `wiki.phext` the sidecar is the directory `wiki.phext.idx/`. Everything in it
/// is phext, and the per-scroll files are **parallel** to the source: the scroll at
/// coordinate C describes source scroll C.
///
/// **Base** — written by a full build, and again whenever the delta is compacted:
///
/// - `parallel.phext`: at every source coordinate C, what the index knows about
///   scroll C, one `key value` per line:
///
///   ```text
///   h 8c1f0e9a2b3d4c5e        xxh3 of the source scroll
///   d 42                      document id (term-indexed scrolls only)
///   l 187                     token count
///   n Ferrel cell             a name this scroll contributes (repeatable)
///   t glacier 3               term frequency (repeatable)
///   ```
///
/// - `tables.phext`: query-ready tables, one per scroll, sorted so they can be
///   binary-searched straight from the mmap:
///
///   | scroll | contents | line format |
///   |---|---|---|
///   | 1 | header | `key value`; first line gives byte offsets of scrolls 2–8 |
///   | 2 | source spans | `<coord> <start:12> <len:10>`, fixed width, coordinate order |
///   | 3 | names | `<name>\t<source coord>\t<target coord>`, sorted |
///   | 4 | folded names | same, keyed by `names::fold(name)` |
///   | 5 | documents | `<coord> <tokens:10> <members:6>`, fixed width, line n = doc id n |
///   | 6 | postings | `<term>\t<df>\t<id[:tf],…>`, ids delta-coded base 36, sorted by term |
///   | 7 | parallel spans | spans of `parallel.phext`, same format as scroll 2 |
///   | 8 | document keys | `<coord> <id:10>`, fixed width, sorted by coordinate |
///
/// **Delta** — rewritten by every `save()`, so a save costs O(scrolls changed since
/// the base), not O(corpus):
///
/// - `delta.phext`: at each coordinate C changed since the base, C's current entry,
///   its base entry (lines prefixed `-`), its new span (`s start len`, or `x` if
///   deleted), and `r shift`: how far unchanged scrolls after C (up to the next
///   delta coordinate) moved in the source.
/// - `delta.stamp`: binds the delta to its base, the source, and its own bytes. It is
///   written last; if it doesn't match, the delta is ignored and the base resynced.
///
/// Opening loads the base tables and the delta and corrects the span table while
/// parsing it; queries merge base rows with the delta and return exactly what a
/// full rebuild would. Once the delta passes `compact_threshold` entries, `save()`
/// folds it into a new base.
///
/// Coordinates are written zero-padded (`001.001.001/001.001.001/001.002.003`) so
/// byte order is coordinate order. If the source changed behind the sidecar's back
/// (length or mtime differ), `open` rescans it, compares each scroll's hash with its
/// base entry, and records only the scrolls that differ as the delta.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use libphext::phext::{self, Coordinate};
use memmap2::Mmap;
use rayon::prelude::*;
use xxhash_rust::xxh3::xxh3_64;

use crate::index::{LatticeIndex, ScrollSpan};
use crate::mmap::MappedLattice;
use crate::names::fold;
use crate::term_index::{tokenize, Bm25Params, TermHit};

const FORMAT: &str = "phext-lattice-index/2";
const SECTIONS: usize = 7;
const CKEY: usize = 35;
const SPAN_LINE: usize = CKEY + 1 + 12 + 1 + 10 + 1;
const DOC_LINE: usize = CKEY + 1 + 10 + 1 + 6 + 1;
const DOCKEY_LINE: usize = CKEY + 1 + 10 + 1;

/// Whether `b` is a phext dimension break.
pub fn is_break(b: u8) -> bool {
    matches!(b, 0x01 | 0x17..=0x1A | 0x1C..=0x1F)
}

/// Maps a source scroll to the coordinate an index entry should point at, or
/// `None` to leave the scroll out of that index.
pub type Selector = Box<dyn Fn(&Coordinate) -> Option<Coordinate> + Send + Sync>;

/// Which scrolls feed which index. The selectors can't be persisted, so the
/// `label` stands in for them: change the selectors, change the label, and the
/// next open rebuilds instead of trusting a sidecar built another way.
pub struct IndexSpec {
    pub label: String,
    /// Every line of a selected scroll is a name for the returned coordinate.
    pub names: Selector,
    /// Selected scrolls are tokenized into the document keyed by the returned coordinate.
    pub terms: Selector,
}

impl IndexSpec {
    pub fn new(label: &str, names: Selector, terms: Selector) -> IndexSpec {
        IndexSpec { label: label.to_string(), names, terms }
    }

    /// Every scroll is its own term document; no names.
    pub fn scrolls() -> IndexSpec {
        IndexSpec::new("scrolls", Box::new(|_| None), Box::new(|c| Some(*c)))
    }
}

/// How `IndexedLattice::open` got its indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// The sidecar matched the source and was loaded without scanning it;
    /// `delta` scrolls were changed since the base was written.
    Loaded { delta: usize },
    /// The source changed behind the sidecar; this many scrolls were re-indexed.
    Resynced { changed: usize },
    /// There was no usable sidecar; one was built from scratch.
    Built,
}

/// What a `save()` or sync did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Source scrolls whose index entries were rewritten (edited, added, or deleted).
    pub changed: usize,
    /// Scrolls changed since the base, i.e. entries now in the delta.
    pub delta: usize,
    /// Whether the delta was folded into a new base.
    pub compacted: bool,
}

/// The sidecar directory for a phext path: `wiki.phext` → `wiki.phext.idx`.
pub fn sidecar_dir(source: &Path) -> PathBuf {
    let mut name = source.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".idx");
    source.with_file_name(name)
}

// ------------------------------------------------------------------------------------------------
// encoding helpers
// ------------------------------------------------------------------------------------------------

fn dims(c: &Coordinate) -> [usize; 9] {
    [c.z.library, c.z.shelf, c.z.series, c.y.collection, c.y.volume, c.y.book,
     c.x.chapter, c.x.section, c.x.scroll]
}

fn push_ckey(out: &mut Vec<u8>, c: &Coordinate) -> io::Result<()> {
    for (i, d) in dims(c).into_iter().enumerate() {
        if d == 0 || d > 999 {
            return Err(invalid(format!("coordinate out of range for the sidecar: {:?}", dims(c))));
        }
        if i > 0 {
            out.push(if i % 3 == 0 { b'/' } else { b'.' });
        }
        out.extend_from_slice(&[b'0' + (d / 100) as u8, b'0' + (d / 10 % 10) as u8, b'0' + (d % 10) as u8]);
    }
    Ok(())
}

fn ckey(c: &Coordinate) -> io::Result<String> {
    let mut v = Vec::with_capacity(CKEY);
    push_ckey(&mut v, c)?;
    Ok(String::from_utf8(v).unwrap())
}

fn push_padded(out: &mut Vec<u8>, mut n: u64, width: usize) {
    let at = out.len();
    out.resize(at + width, b'0');
    for i in (at..at + width).rev() {
        out[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
}

fn parse_ckey(b: &[u8]) -> Option<Coordinate> {
    if b.len() < CKEY {
        return None;
    }
    let d = |k: usize| -> Option<usize> {
        let s = &b[k * 4..k * 4 + 3];
        s.iter().all(u8::is_ascii_digit)
            .then(|| s.iter().fold(0, |a, &x| a * 10 + (x - b'0') as usize))
    };
    let mut c = phext::default_coordinate();
    c.z.library = d(0)?; c.z.shelf = d(1)?; c.z.series = d(2)?;
    c.y.collection = d(3)?; c.y.volume = d(4)?; c.y.book = d(5)?;
    c.x.chapter = d(6)?; c.x.section = d(7)?; c.x.scroll = d(8)?;
    Some(c)
}

fn parse_num(b: &[u8]) -> Option<u64> {
    let b = b.trim_ascii();
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(b.iter().fold(0u64, |a, &x| a * 10 + (x - b'0') as u64))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

const B36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

fn push_b36(out: &mut Vec<u8>, mut n: u64) {
    let mut buf = [0u8; 13];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = B36[(n % 36) as usize];
        n /= 36;
        if n == 0 { break; }
    }
    out.extend_from_slice(&buf[i..]);
}

fn parse_b36(b: &[u8]) -> Option<u64> {
    let mut n = 0u64;
    for &x in b {
        let v = match x {
            b'0'..=b'9' => x - b'0',
            b'a'..=b'z' => x - b'a' + 10,
            _ => return None,
        };
        n = n * 36 + v as u64;
    }
    (!b.is_empty()).then_some(n)
}

fn encode_postings(list: &[(u32, u32)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(list.len() * 4);
    let mut prev = 0u32;
    for (i, &(doc, tf)) in list.iter().enumerate() {
        if i > 0 { out.push(b','); }
        push_b36(&mut out, (doc - prev) as u64);
        if tf != 1 {
            out.push(b':');
            push_b36(&mut out, tf as u64);
        }
        prev = doc;
    }
    out
}

fn decode_postings(b: &[u8]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut doc = 0u32;
    for item in b.split(|&x| x == b',') {
        if item.is_empty() { continue; }
        let (d, tf) = match item.iter().position(|&x| x == b':') {
            Some(p) => (&item[..p], parse_b36(&item[p + 1..]).unwrap_or(1)),
            None => (item, 1),
        };
        doc += parse_b36(d).unwrap_or(0) as u32;
        out.push((doc, tf as u32));
    }
    out
}

/// A name as stored: trimmed, control characters (tabs included) turned into spaces.
fn clean_name(line: &str) -> Option<String> {
    let s: String = line.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Serialize (coordinate, bytes) pairs in document order as a phext, returning
/// the bytes and each non-empty scroll's span.
fn write_phext<'a>(entries: impl Iterator<Item = (Coordinate, &'a [u8])>) -> (Vec<u8>, Vec<(Coordinate, ScrollSpan)>) {
    let mut out = Vec::new();
    let mut spans = Vec::new();
    let mut cur = phext::default_coordinate();
    for (to, bytes) in entries {
        macro_rules! step {
            ($a:expr, $b:expr, $brk:expr, $f:ident) => {
                while $a < $b { out.push($brk as u8); cur.$f(); }
            };
        }
        step!(cur.z.library, to.z.library, phext::LIBRARY_BREAK, library_break);
        step!(cur.z.shelf, to.z.shelf, phext::SHELF_BREAK, shelf_break);
        step!(cur.z.series, to.z.series, phext::SERIES_BREAK, series_break);
        step!(cur.y.collection, to.y.collection, phext::COLLECTION_BREAK, collection_break);
        step!(cur.y.volume, to.y.volume, phext::VOLUME_BREAK, volume_break);
        step!(cur.y.book, to.y.book, phext::BOOK_BREAK, book_break);
        step!(cur.x.chapter, to.x.chapter, phext::CHAPTER_BREAK, chapter_break);
        step!(cur.x.section, to.x.section, phext::SECTION_BREAK, section_break);
        step!(cur.x.scroll, to.x.scroll, phext::SCROLL_BREAK, scroll_break);
        let start = out.len();
        out.extend_from_slice(bytes);
        if !bytes.is_empty() {
            spans.push((to, ScrollSpan { start, end: out.len() }));
        }
    }
    (out, spans)
}

fn span_rows(spans: impl Iterator<Item = (Coordinate, ScrollSpan)>) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for (c, s) in spans {
        push_ckey(&mut out, &c)?;
        out.push(b' ');
        push_padded(&mut out, s.start as u64, 12);
        out.push(b' ');
        push_padded(&mut out, s.len() as u64, 10);
        out.push(b'\n');
    }
    Ok(out)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)
}

fn source_stamp(path: &Path) -> io::Result<(u64, u128)> {
    let m = fs::metadata(path)?;
    let mtime = m.modified()?.duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    Ok((m.len(), mtime))
}

// ------------------------------------------------------------------------------------------------
// sorted line tables
// ------------------------------------------------------------------------------------------------

fn line_key(line: &[u8]) -> &[u8] {
    match line.iter().position(|&b| b == b'\t') {
        Some(p) => &line[..p],
        None => line,
    }
}

fn line_at(data: &[u8], start: usize) -> &[u8] {
    let end = data[start..].iter().position(|&b| b == b'\n').map(|p| start + p).unwrap_or(data.len());
    &data[start..end]
}

/// Byte offset of the first line whose key (text before the first tab) is >= `key`.
/// Lines must be sorted by key; `data` is a whole number of '\n'-terminated lines.
fn lower_bound(data: &[u8], key: &[u8]) -> usize {
    let (mut lo, mut hi) = (0, data.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let start = data[lo..mid].iter().rposition(|&b| b == b'\n').map(|p| lo + p + 1).unwrap_or(lo);
        let line = line_at(data, start);
        if line_key(line) < key {
            lo = start + line.len() + 1;
        } else {
            hi = start;
        }
    }
    lo
}

/// Lines from the first one whose key equals `key`, while the key matches.
fn lines_with_key<'a>(data: &'a [u8], key: &'a [u8]) -> impl Iterator<Item = &'a [u8]> + 'a {
    let at = lower_bound(data, key).min(data.len());
    data[at..].split(|&b| b == b'\n').take_while(move |l| !l.is_empty() && line_key(l) == key)
}

/// The first line whose key equals `key`.
fn find_line<'a>(data: &'a [u8], key: &'a [u8]) -> Option<&'a [u8]> {
    lines_with_key(data, key).next()
}

/// Row index of the first fixed-width row whose leading coordinate is >= `c`.
fn row_lower_bound(rows: &[u8], width: usize, c: &[u8]) -> usize {
    let (mut lo, mut hi) = (0, rows.len() / width);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if &rows[mid * width..mid * width + CKEY] < c { lo = mid + 1; } else { hi = mid; }
    }
    lo
}

fn lines(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.split(|&b| b == b'\n').filter(|l| !l.is_empty())
}

fn span_row(row: &[u8]) -> Option<(Coordinate, ScrollSpan)> {
    let c = parse_ckey(row)?;
    let start = parse_num(&row[CKEY + 1..CKEY + 13])? as usize;
    let len = parse_num(&row[CKEY + 14..CKEY + 24])? as usize;
    Some((c, ScrollSpan { start, end: start + len }))
}

// ------------------------------------------------------------------------------------------------
// tables.phext
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Header {
    label: String,
    base: u64,
    source_len: u64,
    source_mtime_ns: u128,
    live_docs: u64,
    total_len: u64,
}

struct Tables {
    mmap: Option<Mmap>,
    header: Header,
    /// spans, names, folded, docs, postings, parallel spans, doc keys
    sections: [(usize, usize); SECTIONS],
}

impl Tables {
    fn empty() -> Tables {
        Tables { mmap: None, header: Header::default(), sections: [(0, 0); SECTIONS] }
    }

    fn section(&self, k: usize) -> &[u8] {
        let (start, len) = self.sections[k];
        self.mmap.as_deref().map(|m| &m[start..start + len]).unwrap_or(&[])
    }

    fn spans(&self) -> &[u8] { self.section(0) }
    fn names(&self) -> &[u8] { self.section(1) }
    fn folded(&self) -> &[u8] { self.section(2) }
    fn docs(&self) -> &[u8] { self.section(3) }
    fn postings(&self) -> &[u8] { self.section(4) }
    fn parallel_spans(&self) -> &[u8] { self.section(5) }
    fn doc_keys(&self) -> &[u8] { self.section(6) }

    fn load(path: &Path) -> io::Result<Tables> {
        let file = fs::File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        let head_end = mmap.iter().position(|&b| b == phext::SCROLL_BREAK as u8)
            .ok_or_else(|| invalid("sidecar tables: no header".into()))?;
        let mut header = Header::default();
        let mut sections = [(0, 0); SECTIONS];
        let mut format_ok = false;
        for line in lines(&mmap[..head_end]) {
            let line = std::str::from_utf8(line).map_err(|e| invalid(e.to_string()))?;
            let (k, v) = line.split_once(' ').unwrap_or((line, ""));
            let num = || v.trim().parse::<u64>().map_err(|e| invalid(format!("sidecar header {k}: {e}")));
            match k {
                "format" => format_ok = v == FORMAT,
                "label" => header.label = v.to_string(),
                "base" => header.base = num()?,
                "source_len" => header.source_len = num()?,
                "source_mtime_ns" => header.source_mtime_ns = v.trim().parse().map_err(|_| invalid("mtime".into()))?,
                "live_docs" => header.live_docs = num()?,
                "total_len" => header.total_len = num()?,
                "sections" => {
                    let n: Vec<usize> = v.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                    if n.len() != SECTIONS * 2 {
                        return Err(invalid("sidecar header: bad sections".into()));
                    }
                    for k in 0..SECTIONS {
                        sections[k] = (n[2 * k], n[2 * k + 1]);
                    }
                }
                _ => {}
            }
        }
        if !format_ok {
            return Err(invalid(format!("sidecar tables: not {FORMAT}")));
        }
        if sections.iter().any(|&(s, l)| s + l > mmap.len()) {
            return Err(invalid("sidecar tables: truncated".into()));
        }
        Ok(Tables { mmap: Some(mmap), header, sections })
    }

    fn write(path: &Path, header: &Header, sections: [Vec<u8>; SECTIONS]) -> io::Result<()> {
        let rest = format!("format {FORMAT}\nlabel {}\nbase {}\nsource_len {}\nsource_mtime_ns {}\nlive_docs {}\ntotal_len {}\n",
                           header.label, header.base, header.source_len, header.source_mtime_ns,
                           header.live_docs, header.total_len);
        let sections_line_len = "sections".len() + SECTIONS * 2 * 17 + 1;
        let mut at = sections_line_len + rest.len() + 1;
        let mut line = String::from("sections");
        for s in &sections {
            line += &format!(" {:016} {:016}", at, s.len());
            at += s.len() + 1;
        }
        line.push('\n');
        debug_assert_eq!(line.len(), sections_line_len);
        let mut out = Vec::with_capacity(at);
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(rest.as_bytes());
        for s in &sections {
            out.push(phext::SCROLL_BREAK as u8);
            out.extend_from_slice(s);
        }
        write_atomic(path, &out)
    }

    fn rows(section: &[u8]) -> io::Result<impl Iterator<Item = (Coordinate, ScrollSpan)> + '_> {
        if section.len() % SPAN_LINE != 0 {
            return Err(invalid("sidecar spans: ragged table".into()));
        }
        Ok(section.chunks_exact(SPAN_LINE).filter_map(span_row))
    }

    fn parallel_index(&self, buffer_len: usize) -> io::Result<LatticeIndex> {
        Ok(LatticeIndex::from_entries(Self::rows(self.parallel_spans())?.collect(), buffer_len))
    }

    fn doc_count(&self) -> u32 {
        (self.docs().len() / DOC_LINE) as u32
    }

    /// (key, tokens, member scrolls) of a base document.
    fn doc(&self, id: u32) -> Option<(Coordinate, u64, u64)> {
        let start = id as usize * DOC_LINE;
        let row = self.docs().get(start..start + DOC_LINE)?;
        Some((parse_ckey(row)?, parse_num(&row[CKEY + 1..CKEY + 11])?, parse_num(&row[CKEY + 12..CKEY + 18])?))
    }

    fn doc_id(&self, key: &Coordinate) -> Option<u32> {
        let k = ckey(key).ok()?;
        let rows = self.doc_keys();
        let i = row_lower_bound(rows, DOCKEY_LINE, k.as_bytes());
        let row = rows.get(i * DOCKEY_LINE..(i + 1) * DOCKEY_LINE)?;
        (&row[..CKEY] == k.as_bytes()).then(|| parse_num(&row[CKEY + 1..CKEY + 11]).map(|x| x as u32)).flatten()
    }
}

// ------------------------------------------------------------------------------------------------
// entries
// ------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
struct Entry {
    hash: u64,
    doc: Option<u32>,
    len: u32,
    names: Vec<String>,
    terms: Vec<(String, u32)>,
}

impl Entry {
    fn parse(text: &str) -> Entry {
        let mut e = Entry::default();
        for line in text.lines() {
            e.parse_line(line);
        }
        e
    }

    fn parse_line(&mut self, line: &str) {
        let (Some(k), Some(v)) = (line.get(..2), line.get(2..)) else { return };
        match k {
            "h " => self.hash = u64::from_str_radix(v, 16).unwrap_or(0),
            "d " => self.doc = v.parse().ok(),
            "l " => self.len = v.parse().unwrap_or(0),
            "n " => self.names.push(v.to_string()),
            "t " => if let Some((t, tf)) = v.rsplit_once(' ') {
                self.terms.push((t.to_string(), tf.parse().unwrap_or(0)));
            },
            _ => {}
        }
    }

    fn hash_only(text: &[u8]) -> u64 {
        text.strip_prefix(b"h ").and_then(|h| h.get(..16))
            .and_then(|h| u64::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok()).unwrap_or(0)
    }

    fn render(&self, prefix: &str) -> String {
        let mut s = format!("{prefix}h {:016x}\n", self.hash);
        if let Some(d) = self.doc { s += &format!("{prefix}d {d}\n"); }
        if self.len > 0 { s += &format!("{prefix}l {}\n", self.len); }
        for n in &self.names { s += &format!("{prefix}n {n}\n"); }
        for (t, tf) in &self.terms { s += &format!("{prefix}t {t} {tf}\n"); }
        s
    }
}

/// Index entry for a source scroll; the document is still a key, resolved to an id later.
fn analyze(spec: &IndexSpec, c: &Coordinate, text: &str) -> (Entry, Option<Coordinate>) {
    let mut e = Entry { hash: xxh3_64(text.as_bytes()), ..Entry::default() };
    if spec.names.as_ref()(c).is_some() {
        let mut seen = HashSet::new();
        for line in text.lines() {
            if let Some(n) = clean_name(line) {
                if seen.insert(n.clone()) { e.names.push(n); }
            }
        }
    }
    let doc = spec.terms.as_ref()(c);
    if doc.is_some() {
        let mut tf: HashMap<String, u32> = HashMap::new();
        for t in tokenize(text) {
            *tf.entry(t).or_default() += 1;
            e.len += 1;
        }
        e.terms = tf.into_iter().collect();
        e.terms.sort();
    }
    (e, doc)
}

/// A scroll changed since the base.
#[derive(Debug, Clone, Default, PartialEq)]
struct DeltaEntry {
    /// Current entry; `None` if the scroll was deleted.
    cur: Option<Entry>,
    /// Entry in the base; `None` if the scroll is new since the base.
    base: Option<Entry>,
    /// Current span in the source.
    span: Option<ScrollSpan>,
    /// Byte shift of unchanged base scrolls after this one, up to the next delta entry.
    shift: i64,
}

impl DeltaEntry {
    fn render(&self) -> String {
        let mut s = match self.span {
            Some(sp) => format!("s {} {}\n", sp.start, sp.len()),
            None => "x\n".to_string(),
        };
        s += &format!("r {}\n", self.shift);
        if let Some(e) = &self.cur { s += &e.render(""); }
        if let Some(e) = &self.base { s += &e.render("-"); }
        s
    }

    fn parse(text: &str) -> DeltaEntry {
        let mut d = DeltaEntry::default();
        let (mut cur, mut base) = (Entry::default(), Entry::default());
        let (mut has_cur, mut has_base) = (false, false);
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix('-') {
                has_base |= rest.starts_with("h ");
                base.parse_line(rest);
            } else if let Some(rest) = line.strip_prefix("s ") {
                let mut it = rest.split(' ').filter_map(|x| x.parse::<usize>().ok());
                if let (Some(a), Some(l)) = (it.next(), it.next()) {
                    d.span = Some(ScrollSpan { start: a, end: a + l });
                }
            } else if let Some(rest) = line.strip_prefix("r ") {
                d.shift = rest.parse().unwrap_or(0);
            } else if line != "x" {
                has_cur |= line.starts_with("h ");
                cur.parse_line(line);
            }
        }
        d.cur = has_cur.then_some(cur);
        d.base = has_base.then_some(base);
        d
    }
}

/// The delta and what it implies for queries, relative to the base tables.
#[derive(Default)]
struct Delta {
    entries: BTreeMap<Coordinate, DeltaEntry>,
    /// name → (source, target) contributed by current entries
    names: HashMap<String, Vec<(Coordinate, Coordinate)>>,
    folded: HashMap<String, Vec<(Coordinate, Coordinate)>>,
    tf: HashMap<String, HashMap<u32, i64>>,
    doc_len: HashMap<u32, i64>,
    doc_members: HashMap<u32, i64>,
    /// documents created since the base: id → key
    new_docs: BTreeMap<u32, Coordinate>,
    new_doc_ids: HashMap<Coordinate, u32>,
    live_docs: u64,
    total_len: u64,
}

impl Delta {
    fn derive(&mut self, tables: &Tables, spec: &IndexSpec) {
        let entries = std::mem::take(&mut self.entries);
        *self = Delta::default();
        let base_docs = tables.doc_count();
        for (c, e) in &entries {
            if let Some(cur) = &e.cur {
                if let Some(d) = cur.doc {
                    if d >= base_docs {
                        if let Some(key) = spec.terms.as_ref()(c) {
                            self.new_docs.insert(d, key);
                            self.new_doc_ids.insert(key, d);
                        }
                    }
                    *self.doc_len.entry(d).or_default() += cur.len as i64;
                    *self.doc_members.entry(d).or_default() += 1;
                    for (t, tf) in &cur.terms {
                        *self.tf.entry(t.clone()).or_default().entry(d).or_default() += *tf as i64;
                    }
                }
                if let Some(target) = spec.names.as_ref()(c) {
                    for n in &cur.names {
                        self.names.entry(n.clone()).or_default().push((*c, target));
                        self.folded.entry(fold(n)).or_default().push((*c, target));
                    }
                }
            }
            if let Some(base) = &e.base {
                if let Some(d) = base.doc {
                    *self.doc_len.entry(d).or_default() -= base.len as i64;
                    *self.doc_members.entry(d).or_default() -= 1;
                    for (t, tf) in &base.terms {
                        *self.tf.entry(t.clone()).or_default().entry(d).or_default() -= *tf as i64;
                    }
                }
            }
        }
        let (mut live, mut total) = (tables.header.live_docs as i64, tables.header.total_len as i64);
        let docs: HashSet<u32> = self.doc_len.keys().chain(self.doc_members.keys()).copied().collect();
        for d in docs {
            let (bl, bm) = tables.doc(d).map(|x| (x.1 as i64, x.2 as i64)).unwrap_or((0, 0));
            let nl = bl + self.doc_len.get(&d).copied().unwrap_or(0);
            let nm = bm + self.doc_members.get(&d).copied().unwrap_or(0);
            live += (nm > 0) as i64 - (bm > 0) as i64;
            total += if nm > 0 { nl } else { 0 } - if bm > 0 { bl } else { 0 };
        }
        self.live_docs = live.max(0) as u64;
        self.total_len = total.max(0) as u64;
        self.entries = entries;
    }
}

// ------------------------------------------------------------------------------------------------
// IndexedLattice
// ------------------------------------------------------------------------------------------------

/// A `MappedLattice` whose span, name, and term indexes persist in a sidecar.
///
/// Queries reflect the last `save()` (or open); unsaved overlay edits are visible
/// through `lattice()` reads but not through `name`/`resolve`/`search` until saved.
pub struct IndexedLattice {
    lattice: MappedLattice,
    spec: IndexSpec,
    dir: PathBuf,
    tables: Tables,
    delta: Delta,
    params: Bm25Params,
    compact_threshold: usize,
}

impl IndexedLattice {
    /// Open `path` with indexes from its sidecar: load them if they match the
    /// source, patch them if the source changed, or build them if absent.
    pub fn open<P: AsRef<Path>>(path: P, spec: IndexSpec) -> io::Result<(IndexedLattice, OpenMode)> {
        let path = path.as_ref();
        let dir = sidecar_dir(path);
        let stamp = source_stamp(path)?;
        let new = |lattice, spec, tables| IndexedLattice {
            lattice, spec, dir: dir.clone(), tables, delta: Delta::default(),
            params: Bm25Params::default(), compact_threshold: 0,
        };
        if let Ok(tables) = Tables::load(&dir.join("tables.phext")) {
            if tables.header.label == spec.label {
                let base_matches = (tables.header.source_len, tables.header.source_mtime_ns) == stamp;
                let delta = match read_stamp(&dir) {
                    Some(s) if s.base == tables.header.base && (s.source_len, s.source_mtime_ns) == stamp =>
                        read_delta(&dir, s.delta_hash),
                    Some(_) => None,
                    None if base_matches => Some(BTreeMap::new()),
                    None => None,
                };
                if let Some(entries) = delta {
                    if let Ok(index) = merged_span_index(&tables, &entries, stamp.0 as usize) {
                        if let Ok(lattice) = MappedLattice::open_with_index(path, index) {
                            let mut me = new(lattice, spec, tables);
                            me.delta.entries = entries;
                            me.delta.derive(&me.tables, &me.spec);
                            me.compact_threshold = default_threshold(me.lattice.scroll_count());
                            let n = me.delta.entries.len();
                            return Ok((me, OpenMode::Loaded { delta: n }));
                        }
                    }
                }
                if dir.join("parallel.phext").exists() {
                    let lattice = MappedLattice::open(path)?;
                    let mut me = new(lattice, spec, tables);
                    me.compact_threshold = default_threshold(me.lattice.scroll_count());
                    let report = me.resync()?;
                    return Ok((me, OpenMode::Resynced { changed: report.changed }));
                }
            }
        }
        let lattice = MappedLattice::open(path)?;
        let mut me = new(lattice, spec, Tables::empty());
        me.compact_threshold = default_threshold(me.lattice.scroll_count());
        me.rebuild()?;
        Ok((me, OpenMode::Built))
    }

    /// Discard the sidecar and index every scroll again.
    pub fn rebuild(&mut self) -> io::Result<SyncReport> {
        let _ = fs::remove_file(self.dir.join("parallel.phext"));
        self.tables = Tables::empty();
        self.delta = Delta::default();
        self.sync(None)
    }

    pub fn lattice(&self) -> &MappedLattice {
        &self.lattice
    }

    /// Edit through the lattice, then `save()` to persist and re-sync the indexes.
    pub fn lattice_mut(&mut self) -> &mut MappedLattice {
        &mut self.lattice
    }

    /// The sidecar directory.
    pub fn sidecar(&self) -> &Path {
        &self.dir
    }

    /// Scrolls changed since the base (entries in the delta).
    pub fn delta_len(&self) -> usize {
        self.delta.entries.len()
    }

    /// Fold the delta into the base once it holds more than `n` scrolls.
    /// Defaults to 64 + 1% of the source's scrolls.
    pub fn set_compact_threshold(&mut self, n: usize) {
        self.compact_threshold = n;
    }

    /// Save pending edits to the source, then record exactly the edited scrolls in
    /// the delta (or compact, past the threshold).
    pub fn save(&mut self) -> io::Result<SyncReport> {
        let pending = self.lattice.pending_coordinates();
        if pending.is_empty() {
            return Ok(SyncReport { delta: self.delta.entries.len(), ..SyncReport::default() });
        }
        self.lattice.save()?;
        self.record(pending)
    }

    /// Fold the delta into a new base now.
    pub fn compact(&mut self) -> io::Result<SyncReport> {
        let changed: Vec<Coordinate> = self.delta.entries.keys().copied().collect();
        let mut report = self.sync(Some(changed))?;
        report.compacted = true;
        Ok(report)
    }

    /// Exact name lookup (first owner in document order wins).
    pub fn name(&self, name: &str) -> Option<Coordinate> {
        let key = clean_name(name)?;
        self.lookup(self.tables.names(), &self.delta.names, &key)
    }

    /// Exact lookup, then case-folded.
    pub fn resolve(&self, name: &str) -> Option<Coordinate> {
        let key = clean_name(name)?;
        self.lookup(self.tables.names(), &self.delta.names, &key)
            .or_else(|| self.lookup(self.tables.folded(), &self.delta.folded, &fold(&key)))
    }

    fn lookup(&self, table: &[u8], delta: &HashMap<String, Vec<(Coordinate, Coordinate)>>, key: &str) -> Option<Coordinate> {
        // base rows whose source scroll changed since the base are superseded by the delta
        let base = lines_with_key(table, key.as_bytes()).find_map(|line| {
            let mut f = line.split(|&b| b == b'\t').skip(1);
            let src = parse_ckey(f.next()?)?;
            if self.delta.entries.contains_key(&src) { return None; }
            Some((src, parse_ckey(f.next()?)?))
        });
        let fresh = delta.get(key).and_then(|v| v.iter().min().copied());
        match (base, fresh) {
            (Some(a), Some(b)) => Some(a.min(b).1),
            (a, b) => a.or(b).map(|x| x.1),
        }
    }

    /// (key, tokens, members) of a document, base plus delta.
    fn doc(&self, id: u32) -> Option<(Coordinate, u64, u64)> {
        let (key, len, members) = self.tables.doc(id)
            .or_else(|| self.delta.new_docs.get(&id).map(|k| (*k, 0, 0)))?;
        let len = len as i64 + self.delta.doc_len.get(&id).copied().unwrap_or(0);
        let members = members as i64 + self.delta.doc_members.get(&id).copied().unwrap_or(0);
        Some((key, len.max(0) as u64, members.max(0) as u64))
    }

    /// BM25-ranked documents for a free-text query (terms OR'ed), best first.
    pub fn search(&self, query: &str, limit: usize) -> Vec<TermHit> {
        if self.delta.live_docs == 0 || limit == 0 {
            return Vec::new();
        }
        let n = self.delta.live_docs as f64;
        let avg = (self.delta.total_len as f64 / n).max(1.0);
        let Bm25Params { k1, b } = self.params;
        let mut terms: Vec<String> = tokenize(query).collect();
        terms.sort();
        terms.dedup();
        let mut lens: HashMap<u32, f64> = HashMap::new();
        let mut scores: HashMap<u32, f64> = HashMap::new();
        for t in terms {
            let base = find_line(self.tables.postings(), t.as_bytes())
                .map(|line| decode_postings(line.splitn(3, |&b| b == b'\t').nth(2).unwrap_or(&[])))
                .unwrap_or_default();
            let list: Vec<(u32, u32)> = match self.delta.tf.get(&t) {
                None => base,
                Some(d) => {
                    let mut m: BTreeMap<u32, i64> = base.into_iter().map(|(a, b)| (a, b as i64)).collect();
                    for (doc, v) in d { *m.entry(*doc).or_default() += v; }
                    m.into_iter().filter(|x| x.1 > 0).map(|(a, b)| (a, b as u32)).collect()
                }
            };
            if list.is_empty() { continue; }
            let df = list.len() as f64;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for (doc, tf) in list {
                let dl = *lens.entry(doc).or_insert_with(|| self.doc(doc).map(|d| d.1 as f64).unwrap_or(avg));
                let tf = tf as f64;
                let norm = 1.0 - b + b * dl / avg;
                *scores.entry(doc).or_default() += idf * tf * (k1 + 1.0) / (tf + k1 * norm);
            }
        }
        let mut ranked: Vec<(u32, f64)> = scores.into_iter().collect();
        let by_score = |a: &(u32, f64), b: &(u32, f64)| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0));
        if ranked.len() > limit {
            ranked.select_nth_unstable_by(limit - 1, by_score);
            ranked.truncate(limit);
        }
        ranked.sort_by(by_score);
        ranked.into_iter()
            .filter_map(|(d, score)| Some(TermHit { coordinate: self.doc(d)?.0, score }))
            .collect()
    }

    /// Live term documents.
    pub fn doc_count(&self) -> usize {
        self.delta.live_docs as usize
    }

    /// The base entry of a source scroll, read through the parallel span table.
    fn base_entry(&self, par: Option<&(Mmap, LatticeIndex)>, c: &Coordinate) -> Option<Entry> {
        let (m, ix) = par?;
        let s = ix.get(c)?;
        Some(Entry::parse(&String::from_utf8_lossy(&m[s.start..s.end])))
    }

    fn open_parallel(&self) -> io::Result<Option<(Mmap, LatticeIndex)>> {
        if self.tables.mmap.is_none() {
            return Ok(None);
        }
        let f = match fs::File::open(self.dir.join("parallel.phext")) {
            Ok(f) => f,
            Err(_) => return Ok(None),
        };
        let m = unsafe { Mmap::map(&f)? };
        let ix = self.tables.parallel_index(m.len())?;
        Ok(Some((m, ix)))
    }

    /// The source changed behind the sidecar: find scrolls whose hash differs from
    /// their base entry and record them as the delta (any old delta is discarded).
    fn resync(&mut self) -> io::Result<SyncReport> {
        let par = match self.open_parallel()? {
            Some(p) => p,
            None => return self.rebuild(),
        };
        let (m, pix) = &par;
        let src = &self.lattice;
        let mut all: Vec<Coordinate> = src.index().coordinates().to_vec();
        all.extend_from_slice(pix.coordinates());
        all.sort();
        all.dedup();
        let changed: Vec<Coordinate> = all.par_iter().copied().filter(|c| {
            let base = pix.get(c).map(|s| Entry::hash_only(&m[s.start..s.end]));
            let now = src.index().get(c).map(|s| xxh3_64(src.raw_buffer().map(|b| &b[s.start..s.end]).unwrap_or(&[])));
            base != now
        }).collect();
        drop(par);
        self.delta = Delta::default();
        self.delta.derive(&self.tables, &self.spec);
        self.record(changed)
    }

    /// Record edited scrolls (already saved to the source) in the delta.
    fn record(&mut self, pending: Vec<Coordinate>) -> io::Result<SyncReport> {
        let par = self.open_parallel()?;
        let src = &self.lattice;
        let spec = &self.spec;
        let analyzed: Vec<(Coordinate, Option<Entry>, Option<(Entry, Option<Coordinate>)>)> = pending
            .par_iter()
            .map(|c| {
                let base = match self.delta.entries.get(c) {
                    Some(e) => e.base.clone(),
                    None => self.base_entry(par.as_ref(), c),
                };
                (*c, base, src.read_scroll(c).map(|t| analyze(spec, c, &t)))
            })
            .collect();

        let mut next_id = self.delta.new_docs.keys().next_back().map(|d| d + 1).unwrap_or(0).max(self.tables.doc_count());
        let mut changed = 0;
        for (c, base, cur) in analyzed {
            changed += 1;
            let cur = cur.map(|(mut e, key)| {
                if let Some(key) = key {
                    let id = self.tables.doc_id(&key).or_else(|| self.delta.new_doc_ids.get(&key).copied())
                        .unwrap_or_else(|| {
                            self.delta.new_doc_ids.insert(key, next_id);
                            next_id += 1;
                            next_id - 1
                        });
                    e.doc = Some(id);
                }
                e
            });
            let back_to_base = match (&cur, &base) {
                (Some(a), Some(b)) => a.hash == b.hash,
                (None, None) => true,
                _ => false,
            };
            if back_to_base {
                self.delta.entries.remove(&c);
            } else {
                self.delta.entries.insert(c, DeltaEntry { cur, base, span: None, shift: 0 });
            }
        }

        if self.delta.entries.len() > self.compact_threshold {
            drop(par);
            let mut report = self.compact()?;
            report.changed = changed;
            return Ok(report);
        }

        // every delta entry's span and shift, against the base span table
        let spans = self.tables.spans();
        let index = self.lattice.index();
        let keys: Vec<Coordinate> = self.delta.entries.keys().copied().collect();
        for c in &keys {
            let mut row = row_lower_bound(spans, SPAN_LINE, ckey(c)?.as_bytes());
            let mut shift = 0;
            while let Some(r) = spans.get(row * SPAN_LINE..(row + 1) * SPAN_LINE) {
                let (u, base_span) = span_row(r).ok_or_else(|| invalid("sidecar spans: bad row".into()))?;
                if u != *c && !self.delta.entries.contains_key(&u) {
                    if let Some(now) = index.get(&u) {
                        shift = now.start as i64 - base_span.start as i64;
                    }
                    break;
                }
                row += 1;
            }
            let e = self.delta.entries.get_mut(c).unwrap();
            e.span = index.get(c).copied();
            e.shift = shift;
        }

        let rendered: Vec<(Coordinate, String)> = self.delta.entries.iter().map(|(c, e)| (*c, e.render())).collect();
        let (bytes, _) = write_phext(rendered.iter().map(|(c, s)| (*c, s.as_bytes())));
        write_atomic(&self.dir.join("delta.phext"), &bytes)?;
        let (source_len, source_mtime_ns) = source_stamp(self.lattice.path())?;
        write_stamp(&self.dir, &Stamp { base: self.tables.header.base, source_len, source_mtime_ns, delta_hash: xxh3_64(&bytes) })?;
        self.delta.derive(&self.tables, &self.spec);
        Ok(SyncReport { changed, delta: self.delta.entries.len(), compacted: false })
    }

    /// Write a new base: re-index `candidates` (or every scroll whose hash differs
    /// from its parallel entry), copy all other parallel entries, regenerate tables,
    /// and drop the delta.
    fn sync(&mut self, candidates: Option<Vec<Coordinate>>) -> io::Result<SyncReport> {
        fs::create_dir_all(&self.dir)?;
        let old_par = self.open_parallel()?;
        let old_entry = |c: &Coordinate| -> Option<&[u8]> {
            let (m, ix) = old_par.as_ref()?;
            ix.get(c).map(|s| &m[s.start..s.end])
        };

        // 1. which scrolls changed
        let src = &self.lattice;
        let cands: Vec<Coordinate> = match candidates {
            Some(v) => v,
            None => {
                let mut all: Vec<Coordinate> = src.index().coordinates().to_vec();
                if let Some((_, ix)) = &old_par { all.extend_from_slice(ix.coordinates()); }
                all.sort();
                all.dedup();
                all
            }
        };
        let spec = &self.spec;
        let mut changed: Vec<(Coordinate, Option<Entry>, Option<(Entry, Option<Coordinate>)>)> = cands
            .par_iter()
            .filter_map(|c| {
                let new_text = src.read_scroll(c);
                let old = old_entry(c);
                if let (Some(o), Some(t)) = (old, &new_text) {
                    if Entry::hash_only(o) == xxh3_64(t.as_bytes()) { return None; }
                }
                if old.is_none() && new_text.is_none() { return None; }
                Some((*c, old.map(|o| Entry::parse(&String::from_utf8_lossy(o))),
                      new_text.as_deref().map(|t| analyze(spec, c, t))))
            })
            .collect();
        changed.sort_by(|a, b| a.0.cmp(&b.0));

        // 2. documents, name edits, and term deltas
        let mut docs: Vec<(Coordinate, u64, u64)> = (0..self.tables.doc_count())
            .map(|i| self.tables.doc(i).ok_or_else(|| invalid("sidecar docs: bad row".into())))
            .collect::<io::Result<_>>()?;
        let mut doc_ids: HashMap<Coordinate, u32> = docs.iter().enumerate().map(|(i, d)| (d.0, i as u32)).collect();
        let mut delta: HashMap<String, Vec<(u32, i64)>> = HashMap::new();
        let mut name_drop: HashSet<(String, String)> = HashSet::new();
        let mut name_add: Vec<(String, String, String)> = Vec::new();
        let mut new_entries: Vec<(Coordinate, Option<String>)> = Vec::with_capacity(changed.len());

        for (c, old, new) in changed {
            let src_key = ckey(&c)?;
            if let Some(old) = old {
                if let Some(id) = old.doc {
                    if let Some(d) = docs.get_mut(id as usize) {
                        d.1 = d.1.saturating_sub(old.len as u64);
                        d.2 = d.2.saturating_sub(1);
                    }
                    for (t, tf) in old.terms {
                        delta.entry(t).or_default().push((id, -(tf as i64)));
                    }
                }
                for n in old.names {
                    name_drop.insert((n, src_key.clone()));
                }
            }
            let entry = match new {
                Some((mut e, doc_key)) => {
                    if let Some(key) = doc_key {
                        let id = *doc_ids.entry(key).or_insert_with(|| {
                            docs.push((key, 0, 0));
                            (docs.len() - 1) as u32
                        });
                        e.doc = Some(id);
                        let d = &mut docs[id as usize];
                        d.1 += e.len as u64;
                        d.2 += 1;
                    }
                    if !e.names.is_empty() {
                        let target = spec.names.as_ref()(&c).map(|t| ckey(&t)).transpose()?.unwrap_or_default();
                        for n in &e.names {
                            name_add.push((n.clone(), src_key.clone(), target.clone()));
                        }
                    }
                    let text = e.render("");
                    if let Some(id) = e.doc {
                        for (t, tf) in e.terms {
                            delta.entry(t).or_default().push((id, tf as i64));
                        }
                    }
                    Some(text)
                }
                None => None,
            };
            new_entries.push((c, entry));
        }

        // 3. parallel.phext: unchanged entries copied, changed ones rewritten
        let touched: HashSet<Coordinate> = new_entries.iter().map(|(c, _)| *c).collect();
        let mut merged: Vec<(Coordinate, &[u8])> = Vec::new();
        if let Some((m, ix)) = &old_par {
            for (c, s) in ix.coordinates().iter().zip(ix.spans()) {
                if !touched.contains(c) {
                    merged.push((*c, &m[s.start..s.end]));
                }
            }
        }
        merged.extend(new_entries.iter().filter_map(|(c, e)| e.as_ref().map(|s| (*c, s.as_bytes()))));
        merged.sort_by(|a, b| a.0.cmp(&b.0));
        let (parallel, parallel_spans) = write_phext(merged.into_iter());

        // 4. tables
        let index = src.index();
        let spans = span_rows(index.coordinates().iter().copied().zip(index.spans().iter().copied()))?;
        let parallel_spans = span_rows(parallel_spans.into_iter())?;
        let names = merge_names(self.tables.names(), &name_drop, name_add.iter().map(|(n, s, t)| (n.clone(), s, t)));
        let folded_drop: HashSet<(String, String)> = name_drop.iter().map(|(n, s)| (fold(n), s.clone())).collect();
        let folded = merge_names(self.tables.folded(), &folded_drop, name_add.iter().map(|(n, s, t)| (fold(n), s, t)));
        let mut doc_rows = Vec::with_capacity(docs.len() * DOC_LINE);
        let (mut live, mut total) = (0u64, 0u64);
        for (c, len, members) in &docs {
            push_ckey(&mut doc_rows, c)?;
            doc_rows.push(b' ');
            push_padded(&mut doc_rows, *len, 10);
            doc_rows.push(b' ');
            push_padded(&mut doc_rows, *members, 6);
            doc_rows.push(b'\n');
            if *members > 0 {
                live += 1;
                total += len;
            }
        }
        let mut keyed: Vec<(Coordinate, u32)> = doc_ids.into_iter().collect();
        keyed.sort();
        let mut doc_keys = Vec::with_capacity(keyed.len() * DOCKEY_LINE);
        for (c, id) in keyed {
            push_ckey(&mut doc_keys, &c)?;
            doc_keys.push(b' ');
            push_padded(&mut doc_keys, id as u64, 10);
            doc_keys.push(b'\n');
        }
        let postings = merge_postings(self.tables.postings(), delta);

        let (source_len, source_mtime_ns) = source_stamp(self.lattice.path())?;
        let base = xxh3_64(format!("{source_len} {source_mtime_ns} {:?}", SystemTime::now()).as_bytes());
        let header = Header { label: spec.label.clone(), base, source_len, source_mtime_ns, live_docs: live, total_len: total };
        drop(old_par);
        // the stamp goes first: a crash below leaves no delta claiming the new base
        let _ = fs::remove_file(self.dir.join("delta.stamp"));
        write_atomic(&self.dir.join("parallel.phext"), &parallel)?;
        let tables_path = self.dir.join("tables.phext");
        self.tables = Tables::empty();
        Tables::write(&tables_path, &header, [spans, names, folded, doc_rows, postings, parallel_spans, doc_keys])?;
        let _ = fs::remove_file(self.dir.join("delta.phext"));
        self.tables = Tables::load(&tables_path)?;
        self.delta = Delta::default();
        self.delta.derive(&self.tables, &self.spec);
        Ok(SyncReport { changed: new_entries.len(), delta: 0, compacted: false })
    }
}

fn default_threshold(scrolls: usize) -> usize {
    64 + scrolls / 100
}

/// The source span table with the delta applied: changed scrolls take their new
/// spans (or vanish), and unchanged ones shift by the preceding entry's `r`.
fn merged_span_index(tables: &Tables, delta: &BTreeMap<Coordinate, DeltaEntry>, buffer_len: usize) -> io::Result<LatticeIndex> {
    let mut out: Vec<(Coordinate, ScrollSpan)> = Vec::with_capacity(tables.spans().len() / SPAN_LINE + delta.len());
    let mut pending = delta.iter().peekable();
    let mut shift = 0i64;
    for (u, s) in Tables::rows(tables.spans())? {
        while let Some((c, e)) = pending.peek() {
            if **c > u { break; }
            if let Some(sp) = e.span { out.push((**c, sp)); }
            shift = e.shift;
            pending.next();
        }
        if delta.contains_key(&u) { continue; }
        let start = (s.start as i64 + shift) as usize;
        out.push((u, ScrollSpan { start, end: start + s.len() }));
    }
    for (c, e) in pending {
        if let Some(sp) = e.span { out.push((*c, sp)); }
    }
    if out.last().is_some_and(|(_, s)| s.end > buffer_len) {
        return Err(invalid("sidecar delta: span past end of source".into()));
    }
    Ok(LatticeIndex::from_entries(out, buffer_len))
}

struct Stamp {
    base: u64,
    source_len: u64,
    source_mtime_ns: u128,
    delta_hash: u64,
}

fn write_stamp(dir: &Path, s: &Stamp) -> io::Result<()> {
    write_atomic(&dir.join("delta.stamp"), format!("base {}\nsource_len {}\nsource_mtime_ns {}\ndelta {:016x}\n",
                                                   s.base, s.source_len, s.source_mtime_ns, s.delta_hash).as_bytes())
}

fn read_stamp(dir: &Path) -> Option<Stamp> {
    let text = fs::read_to_string(dir.join("delta.stamp")).ok()?;
    let mut s = Stamp { base: 0, source_len: 0, source_mtime_ns: 0, delta_hash: 0 };
    for line in text.lines() {
        let (k, v) = line.split_once(' ')?;
        match k {
            "base" => s.base = v.parse().ok()?,
            "source_len" => s.source_len = v.parse().ok()?,
            "source_mtime_ns" => s.source_mtime_ns = v.parse().ok()?,
            "delta" => s.delta_hash = u64::from_str_radix(v, 16).ok()?,
            _ => {}
        }
    }
    Some(s)
}

fn read_delta(dir: &Path, expect: u64) -> Option<BTreeMap<Coordinate, DeltaEntry>> {
    let bytes = fs::read(dir.join("delta.phext")).ok()?;
    if xxh3_64(&bytes) != expect {
        return None;
    }
    let ix = LatticeIndex::build(&bytes);
    Some(ix.coordinates().iter().zip(ix.spans())
        .map(|(c, s)| (*c, DeltaEntry::parse(&String::from_utf8_lossy(&bytes[s.start..s.end]))))
        .collect())
}

/// Rewrite a sorted `name \t source \t target` table: drop (name, source) pairs and
/// merge new rows in order. Untouched rows are copied as-is.
fn merge_names<'a>(old: &[u8], drop: &HashSet<(String, String)>,
                   add: impl Iterator<Item = (String, &'a String, &'a String)>) -> Vec<u8> {
    let mut add: Vec<(String, &String, &String)> = add.collect();
    add.sort();
    add.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    let mut out = Vec::with_capacity(old.len() + add.len() * 64);
    let mut next = add.into_iter().peekable();
    for line in lines(old) {
        let mut f = line.splitn(3, |&b| b == b'\t');
        let (name, src) = (f.next().unwrap_or(&[]), f.next().unwrap_or(&[]));
        while let Some(a) = next.peek() {
            if (a.0.as_bytes(), a.1.as_bytes()) < (name, src) {
                out.extend_from_slice(format!("{}\t{}\t{}\n", a.0, a.1, a.2).as_bytes());
                next.next();
            } else {
                break;
            }
        }
        if !drop.is_empty() {
            if let (Ok(n), Ok(s)) = (std::str::from_utf8(name), std::str::from_utf8(src)) {
                if drop.contains(&(n.to_string(), s.to_string())) { continue; }
            }
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    for a in next {
        out.extend_from_slice(format!("{}\t{}\t{}\n", a.0, a.1, a.2).as_bytes());
    }
    out
}

/// Apply per-(term, doc) tf deltas to a sorted postings table; untouched terms are
/// copied as-is, so the cost is a memcpy plus decoding the touched terms.
fn merge_postings(old: &[u8], delta: HashMap<String, Vec<(u32, i64)>>) -> Vec<u8> {
    let mut delta: Vec<(String, Vec<(u32, i64)>)> = delta.into_iter().collect();
    delta.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::with_capacity(old.len() + delta.len() * 16);
    let emit = |out: &mut Vec<u8>, term: &[u8], list: &[(u32, u32)]| {
        if list.is_empty() { return; }
        out.extend_from_slice(term);
        out.push(b'\t');
        out.extend_from_slice(list.len().to_string().as_bytes());
        out.push(b'\t');
        out.extend_from_slice(&encode_postings(list));
        out.push(b'\n');
    };
    // both inputs sorted by doc after the sort below: a linear merge
    let apply = |base: Vec<(u32, u32)>, mut d: Vec<(u32, i64)>| -> Vec<(u32, u32)> {
        d.sort_unstable_by_key(|x| x.0);
        let mut out = Vec::with_capacity(base.len() + d.len());
        let (mut i, mut j) = (0, 0);
        while i < base.len() || j < d.len() {
            let doc = match (base.get(i), d.get(j)) {
                (Some(a), Some(b)) => a.0.min(b.0),
                (Some(a), None) => a.0,
                (None, Some(b)) => b.0,
                (None, None) => unreachable!(),
            };
            let mut tf = 0i64;
            while i < base.len() && base[i].0 == doc { tf += base[i].1 as i64; i += 1; }
            while j < d.len() && d[j].0 == doc { tf += d[j].1; j += 1; }
            if tf > 0 { out.push((doc, tf as u32)); }
        }
        out
    };
    let mut next = delta.into_iter().peekable();
    for line in lines(old) {
        let term = line_key(line);
        while next.peek().is_some_and(|d| d.0.as_bytes() < term) {
            let (t, d) = next.next().unwrap();
            emit(&mut out, t.as_bytes(), &apply(Vec::new(), d));
        }
        if next.peek().is_some_and(|d| d.0.as_bytes() == term) {
            let (_, d) = next.next().unwrap();
            let list = line.splitn(3, |&b| b == b'\t').nth(2).unwrap_or(&[]);
            emit(&mut out, term, &apply(decode_postings(list), d));
        } else {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
    }
    for (t, d) in next {
        emit(&mut out, t.as_bytes(), &apply(Vec::new(), d));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NameIndex, TermIndex};
    use libphext::phext::to_coordinate;

    fn c(s: &str) -> Coordinate {
        to_coordinate(s)
    }

    /// record per section: scroll 1 title, 2 body, 3 aliases
    fn wiki_spec() -> IndexSpec {
        IndexSpec::new("wiki-v1",
            Box::new(|s| (s.x.scroll == 1 || s.x.scroll == 3).then(|| { let mut t = *s; t.x.scroll = 1; t })),
            Box::new(|s| (s.x.scroll <= 2).then(|| { let mut t = *s; t.x.scroll = 1; t })))
    }

    const WIKI: &str = "Glacier\x17A glacier is a persistent body of dense ice.\x17Glaciers\nIce river\x18\
                        Ferrel cell\x17The Ferrel cell is an atmospheric circulation cell.\x18\
                        Sea level rise\x17Melting glaciers and ice sheets raise the sea level.\x17SLR";

    fn tmp(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("phext-sidecar-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("wiki.phext");
        fs::write(&p, body).unwrap();
        p
    }

    fn reference(path: &Path) -> (NameIndex, TermIndex) {
        let lat = MappedLattice::open(path).unwrap();
        let spec = wiki_spec();
        (NameIndex::from_scrolls(&lat, |s| spec.names.as_ref()(s)),
         TermIndex::build_grouped(&lat, |s| spec.terms.as_ref()(s)))
    }

    /// Indexes must answer exactly like a fresh in-memory build over the current file.
    fn assert_matches_reference(ix: &IndexedLattice, queries: &[&str], names: &[&str]) {
        let path = ix.lattice().path().to_path_buf();
        let scanned = LatticeIndex::build(&fs::read(&path).unwrap());
        assert_eq!(ix.lattice().index().coordinates(), scanned.coordinates());
        assert_eq!(ix.lattice().index().spans(), scanned.spans());
        let (rn, rt) = reference(&path);
        assert_eq!(ix.doc_count(), rt.doc_count());
        for q in queries {
            let a = ix.search(q, 10);
            let b = rt.search(q, 10);
            let a: Vec<(Coordinate, String)> = a.iter().map(|h| (h.coordinate, format!("{:.9}", h.score))).collect();
            let b: Vec<(Coordinate, String)> = b.iter().map(|h| (h.coordinate, format!("{:.9}", h.score))).collect();
            assert_eq!(a, b, "query {q}");
        }
        for n in names {
            assert_eq!(ix.name(n), rn.get(n), "name {n}");
            assert_eq!(ix.resolve(n), rn.resolve(n), "resolve {n}");
        }
    }

    const QUERIES: &[&str] = &["glacier ice", "sea level", "circulation", "dense body", "melting new",
                               "hadley polar", "permafrost", "atmospheric", "frozen ground"];
    const NAMES: &[&str] = &["Glacier", "glaciers", "ICE RIVER", "Ferrel cell", "slr", "Brand new",
                             "permafrost", "missing"];

    fn edit(ix: &mut IndexedLattice) {
        let lat = ix.lattice_mut();
        lat.write_scroll(c("1.1.1/1.1.1/1.2.2"), "Ferrel cells sit between Hadley and polar cells.".into());
        lat.write_scroll(c("1.1.1/1.1.1/1.1.3"), "Ice river".into()); // dropped alias "Glaciers"
        lat.write_scroll(c("1.1.1/1.1.1/1.4.1"), "Brand new".into());
        lat.write_scroll(c("1.1.1/1.1.1/1.4.2"), "A new record about melting permafrost.".into());
        lat.delete_scroll(c("1.1.1/1.1.1/1.3.3"));
    }

    #[test]
    fn postings_roundtrip() {
        let list = vec![(0, 1), (5, 3), (40, 1), (1_000_000, 70)];
        assert_eq!(decode_postings(&encode_postings(&list)), list);
    }

    #[test]
    fn lower_bound_finds_first_of_duplicates() {
        let t = b"apple\tx\nbanana\t1\nbanana\t2\ncherry\tz\n";
        assert_eq!(lines_with_key(t, b"banana").collect::<Vec<_>>(), vec![&b"banana\t1"[..], &b"banana\t2"[..]]);
        assert_eq!(find_line(t, b"apple"), Some(&b"apple\tx"[..]));
        assert_eq!(find_line(t, b"cherry"), Some(&b"cherry\tz"[..]));
        assert_eq!(find_line(t, b"blueberry"), None);
        assert_eq!(find_line(t, b"zzz"), None);
        assert_eq!(find_line(b"", b"a"), None);
    }

    #[test]
    fn ckey_is_fixed_width_and_ordered() {
        let a = ckey(&c("1.1.1/1.1.1/1.2.3")).unwrap();
        assert_eq!(a, "001.001.001/001.001.001/001.002.003");
        assert_eq!(parse_ckey(a.as_bytes()), Some(c("1.1.1/1.1.1/1.2.3")));
        assert!(ckey(&c("1.1.1/1.1.1/1.10.1")).unwrap() > ckey(&c("1.1.1/1.1.1/1.9.999")).unwrap());
    }

    #[test]
    fn build_then_load_without_scanning() {
        let p = tmp("load", WIKI);
        let (ix, mode) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        assert_eq!(mode, OpenMode::Built);
        assert_matches_reference(&ix, QUERIES, NAMES);
        drop(ix);

        let (ix, mode) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        assert_eq!(mode, OpenMode::Loaded { delta: 0 });
        assert_eq!(ix.lattice().read_scroll(&c("1.1.1/1.1.1/1.2.2")).unwrap(),
                   "The Ferrel cell is an atmospheric circulation cell.");
        assert_eq!(ix.resolve("ice river"), Some(c("1.1.1/1.1.1/1.1.1")));
        assert_matches_reference(&ix, QUERIES, NAMES);
    }

    #[test]
    fn save_writes_a_delta_and_reopens_from_it() {
        let p = tmp("delta", WIKI);
        let (mut ix, _) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        let tables_before = fs::read(ix.sidecar().join("tables.phext")).unwrap();
        edit(&mut ix);
        let report = ix.save().unwrap();
        assert_eq!(report, SyncReport { changed: 5, delta: 5, compacted: false });
        // the base is untouched; only the delta was written
        assert_eq!(fs::read(ix.sidecar().join("tables.phext")).unwrap(), tables_before);
        assert!(ix.sidecar().join("delta.phext").exists());
        assert_eq!(ix.name("Glaciers"), None);
        assert_eq!(ix.resolve("slr"), None);
        assert_eq!(ix.resolve("brand NEW"), Some(c("1.1.1/1.1.1/1.4.1")));
        assert_eq!(ix.search("hadley", 1)[0].coordinate, c("1.1.1/1.1.1/1.2.1"));
        assert!(ix.search("atmospheric", 10).is_empty());
        assert_matches_reference(&ix, QUERIES, NAMES);
        drop(ix);

        let (ix, mode) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        assert_eq!(mode, OpenMode::Loaded { delta: 5 });
        assert_matches_reference(&ix, QUERIES, NAMES);
    }

    #[test]
    fn deltas_accumulate_and_revert() {
        let p = tmp("accumulate", WIKI);
        let (mut ix, _) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        let body = c("1.1.1/1.1.1/1.1.2");
        let original = ix.lattice().read_scroll(&body).unwrap();
        ix.lattice_mut().write_scroll(body, "Glaciers flow downhill under their own weight.".into());
        assert_eq!(ix.save().unwrap().delta, 1);
        ix.lattice_mut().write_scroll(c("1.1.1/1.1.1/1.3.2"), "Seas rise as oceans warm.".into());
        assert_eq!(ix.save().unwrap().delta, 2);
        assert_matches_reference(&ix, &["flow downhill", "oceans warm", "dense ice", "melting"], NAMES);
        ix.lattice_mut().write_scroll(body, original);
        assert_eq!(ix.save().unwrap().delta, 1, "reverting a scroll drops it from the delta");
        assert_matches_reference(&ix, &["flow downhill", "oceans warm", "dense ice"], NAMES);
        drop(ix);
        let (ix, mode) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        assert_eq!(mode, OpenMode::Loaded { delta: 1 });
        assert_matches_reference(&ix, &["oceans warm", "dense ice"], NAMES);
    }

    #[test]
    fn compaction_folds_the_delta_into_the_base() {
        let p = tmp("compact", WIKI);
        let (mut ix, _) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        ix.set_compact_threshold(2);
        edit(&mut ix);
        let report = ix.save().unwrap();
        assert!(report.compacted);
        assert_eq!(report.delta, 0);
        assert!(!ix.sidecar().join("delta.phext").exists());
        assert_matches_reference(&ix, QUERIES, NAMES);
        drop(ix);
        let (ix, mode) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        assert_eq!(mode, OpenMode::Loaded { delta: 0 });
        assert_matches_reference(&ix, QUERIES, NAMES);
    }

    #[test]
    fn external_edit_is_resynced() {
        let p = tmp("external", WIKI);
        let (mut ix, _) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        ix.lattice_mut().write_scroll(c("1.1.1/1.1.1/1.2.2"), "Hadley and polar cells.".into());
        ix.save().unwrap();
        drop(ix);
        // another tool appends a record at EOF
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"\x18Permafrost\x17Ground that stays frozen for two or more years.").unwrap();
        drop(f);
        let (ix, mode) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        assert_eq!(mode, OpenMode::Resynced { changed: 3 }, "the delta's edit plus the two appended scrolls");
        assert_eq!(ix.delta_len(), 3, "resync records into the delta; the base is untouched");
        assert_eq!(ix.resolve("permafrost"), Some(c("1.1.1/1.1.1/1.4.1")));
        assert_matches_reference(&ix, QUERIES, NAMES);
        drop(ix);
        let (ix, mode) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        assert_eq!(mode, OpenMode::Loaded { delta: 3 });
        assert_matches_reference(&ix, QUERIES, NAMES);
    }

    #[test]
    fn torn_delta_is_ignored() {
        let p = tmp("torn", WIKI);
        let (mut ix, _) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        ix.lattice_mut().write_scroll(c("1.1.1/1.1.1/1.2.2"), "Hadley and polar cells.".into());
        ix.save().unwrap();
        let dir = ix.sidecar().to_path_buf();
        drop(ix);
        fs::write(dir.join("delta.phext"), b"garbage").unwrap();
        let (ix, mode) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        assert_eq!(mode, OpenMode::Resynced { changed: 1 });
        assert_matches_reference(&ix, QUERIES, NAMES);
    }

    #[test]
    fn label_change_rebuilds() {
        let p = tmp("label", WIKI);
        drop(IndexedLattice::open(&p, wiki_spec()).unwrap());
        let (ix, mode) = IndexedLattice::open(&p, IndexSpec::scrolls()).unwrap();
        assert_eq!(mode, OpenMode::Built);
        assert_eq!(ix.doc_count(), 8);
        assert_eq!(ix.name("Glacier"), None);
    }

    #[test]
    fn parallel_files_mirror_source_coordinates() {
        let p = tmp("parallel", WIKI);
        let (mut ix, _) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        let par = MappedLattice::open(ix.sidecar().join("parallel.phext")).unwrap();
        assert_eq!(par.index().coordinates(), ix.lattice().index().coordinates());
        let e = Entry::parse(&par.read_scroll(&c("1.1.1/1.1.1/1.1.3")).unwrap());
        assert_eq!(e.names, vec!["Glaciers", "Ice river"]);
        assert_eq!(e.doc, None);
        let e = Entry::parse(&par.read_scroll(&c("1.1.1/1.1.1/1.1.2")).unwrap());
        assert_eq!(e.doc, Some(0));
        assert!(e.terms.contains(&("ice".to_string(), 1)));

        edit(&mut ix);
        ix.save().unwrap();
        let delta = MappedLattice::open(ix.sidecar().join("delta.phext")).unwrap();
        let expected = ["1.1.1/1.1.1/1.1.3", "1.1.1/1.1.1/1.2.2", "1.1.1/1.1.1/1.3.3",
                        "1.1.1/1.1.1/1.4.1", "1.1.1/1.1.1/1.4.2"].map(c);
        assert_eq!(delta.index().coordinates(), &expected[..]);
        let d = DeltaEntry::parse(&delta.read_scroll(&c("1.1.1/1.1.1/1.3.3")).unwrap());
        assert_eq!(d.cur, None);
        assert_eq!(d.base.unwrap().names, vec!["SLR"]);
    }

    #[test]
    fn many_saves_match_reference() {
        // a larger lattice with edits scattered across it, saved one at a time
        let mut body = String::new();
        for i in 0..300 {
            if i > 0 { body.push('\x18'); }
            body += &format!("Title {i}\x17Body text {i} about topic{} and topic{}.", i % 7, i % 11);
        }
        let p = tmp("many", &body);
        let (mut ix, _) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        let mut queries = vec!["topic3".to_string(), "topic5 topic10".to_string()];
        for k in 0..25usize {
            let sec = (k * 37) % 300 + 1;
            let mut at = c("1.1.1/1.1.1/1.1.2");
            at.x.section = sec;
            if k % 5 == 4 {
                ix.lattice_mut().delete_scroll(at);
            } else {
                ix.lattice_mut().write_scroll(at, format!("Rewritten {k} mentions zeta{k} and topic{}.", k % 3));
            }
            ix.save().unwrap();
            queries.push(format!("zeta{k}"));
        }
        let q: Vec<&str> = queries.iter().map(|s| s.as_str()).collect();
        assert_matches_reference(&ix, &q, &["Title 5", "title 37"]);
        drop(ix);
        let (ix, mode) = IndexedLattice::open(&p, wiki_spec()).unwrap();
        assert!(matches!(mode, OpenMode::Loaded { delta } if delta > 0));
        assert_matches_reference(&ix, &q, &["Title 5", "title 37"]);
    }
}
