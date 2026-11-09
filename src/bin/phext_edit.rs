/// phext-edit — web-served phext editor.
///
/// Hosts a memory-mapped phext file over HTTP. Open your browser to navigate
/// the 9D lattice with sentron topology, search, and scroll editing.
///
/// Usage: phext-edit <file.phext> [--port 8080]

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;

use phext_lattice::{
    CoordinateNav, Dimension, LatticeOverview,
    MappedLattice, Navigator, Sentron,
    DimensionDensity,
};
use phext_lattice::claims::{self, Tier};
use phext_lattice::joins;
use phext_lattice::oauth::{forwarded_https, security_header_block, status_text, Gate, Reply, Session};
use phext_lattice::sponsor::{self, SponsorOffer};
use phext_lattice::submissions::{self, Submission, SubmissionStatus};
use phext_lattice::wavefront;
use sha2::Digest;

// ── API Types ──────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ApiStatus {
    scrolls: usize,
    bytes: usize,
    file: String,
    /// File selected by `/` and `/#coordinate`. Named URLs do not change it.
    default_file: String,
    dirty: bool,
}

#[derive(Serialize)]
struct ApiPosition {
    coordinate: String,
    dimension: String,
    dimension_index: u8,
    has_scroll: bool,
    total_scrolls: usize,
    /// Open phext. The file switcher matches this against `/api/phexts`.
    file: String,
}

#[derive(Serialize)]
struct ApiScroll {
    coordinate: String,
    content: String,
    bytes: usize,
}

#[derive(Serialize)]
struct ApiSentron {
    center: String,
    size: usize,
    structural_reach: usize,
    sequential_reach: usize,
    axons: Vec<ApiAxon>,
}

#[derive(Serialize)]
struct ApiAxon {
    dimension: u8,
    name: String,
    group: String,
    value: usize,
    backward: usize,
    forward: usize,
}

#[derive(Serialize)]
struct ApiNav {
    position: ApiPosition,
    sentron: ApiSentron,
    scroll: ApiScroll,
    densities: Vec<ApiDensity>,
}

#[derive(Serialize)]
struct ApiDensity {
    dimension: u8,
    name: String,
    extent: usize,
    populated: usize,
    sparkline: String,
}

#[derive(Serialize)]
struct ApiSearchHit {
    coordinate: String,
    context: String,
    offset: usize,
}

#[derive(Serialize)]
struct ApiZoomEntry {
    label: String,
    coordinate: String,
    scroll_count: usize,
    preview: String,
}

#[derive(Serialize)]
struct ApiZoom {
    tier: String,
    parent: String,
    entries: Vec<ApiZoomEntry>,
}

/// Full lattice tree — returned once, cached client-side
#[derive(Serialize)]
struct ApiTree {
    total_scrolls: usize,
    /// All populated coordinates as sorted strings
    coordinates: Vec<String>,
}

#[derive(Serialize)]
struct ApiOk {
    ok: bool,
    message: String,
}

/// Available phext file metadata
#[derive(Serialize, Clone)]
struct PhextFile {
    name: String,
    path: String,
    bytes: u64,
}

// ── State ──────────────────────────────────────────────────────────────

struct AppState {
    lattice: MappedLattice,
    nav: Navigator,
    overview: LatticeOverview,
    file_path: String,
    /// Phext opened at startup (or `--home`). `/` returns to this file.
    default_file: String,
    dirty: bool,
    /// Directory of phext files available
    phext_dir: Option<String>,
    /// Landing file after sign-in / on a fresh load (--home or PHEXT_HOME)
    home_file: Option<String>,
    /// Landing coordinate within the home file (--home-coord or PHEXT_HOME_COORD)
    home_coord: Option<libphext::phext::Coordinate>,
}

impl AppState {
    fn new(lattice: MappedLattice, file_path: String) -> Self {
        let buf = lattice.to_phext_bytes();
        let overview = LatticeOverview::build(&buf, lattice.index());

        let mut nav = Navigator::new();
        if lattice.has_scroll(&libphext::phext::default_coordinate()) {
            nav.goto(libphext::phext::default_coordinate());
        } else {
            nav.next_populated(lattice.index());
        }

        AppState { lattice, nav, overview, default_file: file_path.clone(), file_path, dirty: false, phext_dir: None, home_file: None, home_coord: None }
    }

    fn rebuild_overview(&mut self) {
        let buf = self.lattice.to_phext_bytes();
        self.overview = LatticeOverview::build(&buf, self.lattice.index());
    }

    fn nav_json(&self) -> ApiNav {
        nav_view(&self.lattice, &self.nav, self.overview.total_scrolls, &self.file_path)
    }

    fn list_phexts(&self) -> Vec<PhextFile> {
        let dir = match &self.phext_dir {
            Some(d) => d.clone(),
            None => {
                // Single file mode — just show current file
                let meta = std::fs::metadata(&self.file_path).ok();
                return vec![PhextFile {
                    name: std::path::Path::new(&self.file_path)
                        .file_name().unwrap_or_default().to_string_lossy().to_string(),
                    path: self.file_path.clone(),
                    bytes: meta.map(|m| m.len()).unwrap_or(0),
                }];
            }
        };
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if is_phext(&path) {
                    let meta = std::fs::metadata(&path).ok();
                    files.push(PhextFile {
                        name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                        path: path.to_string_lossy().to_string(),
                        bytes: meta.map(|m| m.len()).unwrap_or(0),
                    });
                }
            }
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        files
    }

    /// Switch to the configured home file (if any) and jump to the home
    /// coordinate (if any). Unsaved edits block a file switch.
    fn go_home(&mut self) -> Result<(), String> {
        if let Some(home) = self.home_file.clone() {
            let same = match (std::fs::canonicalize(&home), std::fs::canonicalize(&self.file_path)) {
                (Ok(a), Ok(b)) => a == b,
                _ => home == self.file_path,
            };
            if !same {
                if self.dirty {
                    return Err("unsaved changes; not switching to the home file".into());
                }
                self.open_phext(&home)?;
            }
        }
        if let Some(coord) = self.home_coord {
            self.nav.goto(coord);
        }
        Ok(())
    }

    /// `<name>.phext` inside the hosting directory, and nowhere else.
    /// A symlink that resolves outside that directory is not a hosted file.
    fn hosted_phext(&self, name: &str) -> Option<std::path::PathBuf> {
        hosted_phext_file(self.phext_dir.as_deref(), &self.file_path, name)
    }

    fn open_phext(&mut self, path: &str) -> Result<(), String> {
        // Validate path is within allowed directory
        if let Some(dir) = &self.phext_dir {
            let canonical = std::fs::canonicalize(path)
                .map_err(|e| format!("canonicalize {path}: {e}"))?;
            let dir_canonical = std::fs::canonicalize(dir)
                .map_err(|e| format!("canonicalize {dir}: {e}"))?;
            if !canonical.starts_with(&dir_canonical) {
                return Err(format!(
                    "path outside allowed directory: {path} is not inside {dir}"
                ));
            }
            if canonical.file_name().and_then(|n| n.to_str()).map(|n| n.ends_with(".phext")) != Some(true) {
                return Err("path outside allowed directory".into());
            }
        }
        let lattice = MappedLattice::open(path).map_err(|e| e.to_string())?;
        let buf = lattice.to_phext_bytes();
        self.overview = LatticeOverview::build(&buf, lattice.index());
        self.nav = Navigator::new();
        if lattice.has_scroll(&libphext::phext::default_coordinate()) {
            self.nav.goto(libphext::phext::default_coordinate());
        } else {
            self.nav.next_populated(lattice.index());
        }
        self.lattice = lattice;
        self.file_path = path.to_string();
        self.dirty = false;
        Ok(())
    }

    fn tree_json(&self) -> ApiTree {
        tree_view(&self.lattice)
    }

    fn zoom_z(&self) -> ApiZoom {
        let coords = self.lattice.populated_coordinates();
        let mut groups: HashMap<String, Vec<libphext::phext::Coordinate>> = HashMap::new();
        for c in &coords {
            let key = format!("{}.{}.{}", c.z.library, c.z.shelf, c.z.series);
            groups.entry(key).or_default().push(*c);
        }
        let mut entries: Vec<ApiZoomEntry> = groups.into_iter().map(|(key, scrolls)| {
            let first = scrolls[0];
            let preview = self.lattice.read_scroll(&first)
                .map(|s| s.chars().take(80).collect::<String>())
                .unwrap_or_default();
            ApiZoomEntry { label: key.clone(), coordinate: format!("{}/1.1.1/1.1.1", key), scroll_count: scrolls.len(), preview }
        }).collect();
        entries.sort_by(|a, b| a.label.cmp(&b.label));
        ApiZoom { tier: "z".into(), parent: "".into(), entries }
    }

    fn zoom_y(&self, z_str: &str) -> ApiZoom {
        let z_parts: Vec<usize> = z_str.split('.').filter_map(|n| n.parse().ok()).collect();
        if z_parts.len() != 3 { return ApiZoom { tier: "y".into(), parent: z_str.into(), entries: vec![] }; }
        let coords = self.lattice.populated_coordinates();
        let mut groups: HashMap<String, Vec<libphext::phext::Coordinate>> = HashMap::new();
        for c in &coords {
            if c.z.library == z_parts[0] && c.z.shelf == z_parts[1] && c.z.series == z_parts[2] {
                let key = format!("{}.{}.{}", c.y.collection, c.y.volume, c.y.book);
                groups.entry(key).or_default().push(*c);
            }
        }
        let mut entries: Vec<ApiZoomEntry> = groups.into_iter().map(|(key, scrolls)| {
            let first = scrolls[0];
            let preview = self.lattice.read_scroll(&first)
                .map(|s| s.chars().take(80).collect::<String>())
                .unwrap_or_default();
            ApiZoomEntry { label: key.clone(), coordinate: format!("{}/{}/1.1.1", z_str, key), scroll_count: scrolls.len(), preview }
        }).collect();
        entries.sort_by(|a, b| a.label.cmp(&b.label));
        ApiZoom { tier: "y".into(), parent: z_str.into(), entries }
    }

    fn zoom_x(&self, z_str: &str, y_str: &str) -> ApiZoom {
        let z_parts: Vec<usize> = z_str.split('.').filter_map(|n| n.parse().ok()).collect();
        let y_parts: Vec<usize> = y_str.split('.').filter_map(|n| n.parse().ok()).collect();
        if z_parts.len() != 3 || y_parts.len() != 3 {
            return ApiZoom { tier: "x".into(), parent: format!("{}/{}", z_str, y_str), entries: vec![] };
        }
        let coords = self.lattice.populated_coordinates();
        let mut entries: Vec<ApiZoomEntry> = coords.iter().filter(|c| {
            c.z.library == z_parts[0] && c.z.shelf == z_parts[1] && c.z.series == z_parts[2] &&
            c.y.collection == y_parts[0] && c.y.volume == y_parts[1] && c.y.book == y_parts[2]
        }).map(|c| {
            let key = format!("{}.{}.{}", c.x.chapter, c.x.section, c.x.scroll);
            let preview = self.lattice.read_scroll(c)
                .map(|s| s.chars().take(80).collect::<String>())
                .unwrap_or_default();
            ApiZoomEntry { label: key, coordinate: format!("{}", c), scroll_count: 1, preview }
        }).collect();
        entries.sort_by(|a, b| a.label.cmp(&b.label));
        ApiZoom { tier: "x".into(), parent: format!("{}/{}", z_str, y_str), entries }
    }
}

// ── HTTP Server ────────────────────────────────────────────────────────

/// The same nav payload the signed-in editor renders, for any lattice.
fn nav_view(lattice: &MappedLattice, nav: &Navigator, total_scrolls: usize, file: &str) -> ApiNav {
    let pos = nav.position();
    let content = lattice.read_scroll(&pos).unwrap_or_default();
    let bytes = content.len();
    let sentron = Sentron::build(&pos, lattice.index());
    let mut axons = Vec::new();
    for axon in sentron.structural_axons() {
        axons.push(ApiAxon {
            dimension: axon.dimension as u8,
            name: axon.dimension.name().to_string(),
            group: "spatial".into(),
            value: pos.dimension_value(axon.dimension),
            backward: axon.backward_count,
            forward: axon.forward_count,
        });
    }
    for axon in sentron.sequential_axons() {
        axons.push(ApiAxon {
            dimension: axon.dimension as u8,
            name: axon.dimension.name().to_string(),
            group: "temporal".into(),
            value: pos.dimension_value(axon.dimension),
            backward: axon.backward_count,
            forward: axon.forward_count,
        });
    }
    axons.push(ApiAxon {
        dimension: 9,
        name: "Scroll".into(),
        group: "neuron".into(),
        value: pos.dimension_value(Dimension::Scroll),
        backward: 0,
        forward: 0,
    });
    ApiNav {
        position: ApiPosition {
            coordinate: format!("{}", pos),
            dimension: nav.active_dimension().name().to_string(),
            dimension_index: nav.active_dimension() as u8,
            has_scroll: lattice.has_scroll(&pos),
            total_scrolls,
            file: file.to_string(),
        },
        sentron: ApiSentron {
            center: format!("{}", sentron.center),
            size: sentron.size(),
            structural_reach: sentron.structural_reach,
            sequential_reach: sentron.sequential_reach,
            axons,
        },
        scroll: ApiScroll { coordinate: format!("{}", pos), content, bytes },
        densities: (1..=9u8).map(|i| {
            let dim = Dimension::from_index(i).unwrap();
            let d = DimensionDensity::build(dim, lattice.index());
            ApiDensity {
                dimension: i,
                name: dim.name().to_string(),
                extent: d.distribution.len(),
                populated: d.total,
                sparkline: d.sparkline(20),
            }
        }).collect(),
    }
}

fn tree_view(lattice: &MappedLattice) -> ApiTree {
    let mut coords = lattice.populated_coordinates();
    coords.sort();
    ApiTree {
        total_scrolls: coords.len(),
        coordinates: coords.iter().map(|c| format!("{}", c)).collect(),
    }
}

// ── Public demo ────────────────────────────────────────────────────────
// A small original phext, held in memory and never written back. Navigation
// is computed per request so visitors do not share a cursor or touch the
// files the signed-in editor serves.

const DEMO_SCROLLS: &[(&str, &str)] = &[
    ("1.1.1/1.1.1/1.1.1", "Welcome to the sample phext.\n\nYou are at 1.1.1/1.1.1/1.1.1, the origin scroll. A phext is one plain-text file. Nine numbers name a scroll: library, shelf, series, collection, volume, book, chapter, section, scroll. The scroll itself is a page of lines and columns. That is eleven dimensions, and the file is still just text.\n\nPress j to step to the next scroll that has words. Press k to step back. Keys 1 through 9 pick a dimension (1 is Scroll, 9 is Library); h and l move along it.\n\nNothing you change here is saved. Sign in to edit your own files."),
    ("1.1.1/1.1.1/1.1.2", "Scroll, dimension 1, and the page.\n\nThis is the next scroll in the same section. A scroll is one page. Lines run down it and characters run across it: those two directions are the tenth and eleventh dimensions. Press 1, then l, to move by scroll. Press t and open the Nav tab to see this page under its address."),
    ("1.1.1/1.1.1/1.2.1", "Section, dimension 2.\n\nA section holds scrolls. This one sits beside the welcome section, not after it. Press 2, then l or h, to move by section. Moving along a higher dimension resets the numbers inside it, the same way a delimiter does, so the scroll goes back to 1."),
    ("1.1.1/1.1.1/2.1.1", "Chapter, dimension 3.\n\nChapters group sections. This scroll is chapter 2, section 1, scroll 1 of the same book. Press 3 to select Chapter, then l to step forward. The left panel shows how many neighbors each dimension has from here."),
    ("1.1.1/1.1.2/1.1.1", "Book, dimension 4.\n\nA book gathers chapters. This is book 2 of volume 1. Its only page is this note. The address reads 1.1.1/1.1.2/1.1.1: the middle group is collection, volume, book."),
    ("1.1.1/1.2.1/1.1.1", "Volume, dimension 5.\n\nVolumes stack books. This is volume 2 of collection 1. Press t for the tree. 11D is the whole file, 7D is one library-shelf-series group, and 4D is the scrolls inside one book."),
    ("1.1.1/2.1.1/1.1.1", "Collection, dimension 6.\n\nA collection holds volumes. This is collection 2, still in series 1 of shelf 1. The coordinate bar at the top is the full address. Click a dimension name on the left to select it."),
    ("1.1.2/1.1.1/1.1.1", "Series, dimension 7.\n\nSeries is the third number of the outer group. This scroll lives in series 2, on shelf 1 of library 1, so the address starts 1.1.2. Press 7, then h, to walk back to series 1."),
    ("1.2.1/1.1.1/1.1.1", "Shelf, dimension 8.\n\nShelves hold series. You are on shelf 2 of library 1. The coordinate bar should start with 1.2.1. One file can hold many shelves; they are just another number in the address."),
    ("2.1.1/1.1.1/1.1.1", "Library, dimension 9.\n\nLibraries are the outermost dimension. This is library 2, a second building in the same file. Press 9 and the left panel marks Library. j and k still walk every scroll that has text, in file order, ignoring the empty space between them."),
    ("2.1.1/1.1.1/1.1.2", "Your own files.\n\nThis sample is read-only. Edits stay in this browser tab and are thrown away when you leave. Signing in with Google, GitHub, or X opens the real editor: you can change scrolls and save them into the phext files on the server. There is no private format and no lock-in. A phext is plain text, and you can take the file with you.\n\nFollow 1.1.1/1.1.1/1.1.1 to return to the welcome scroll."),
];

struct DemoCatalog {
    lattice: MappedLattice,
    scrolls: usize,
    bytes: usize,
}

impl DemoCatalog {
    fn load() -> Self {
        let raw = demo_bytes();
        let bytes = raw.len();
        let lattice = MappedLattice::from_bytes(&raw);
        let scrolls = lattice.index().scroll_count();
        Self { lattice, scrolls, bytes }
    }

    /// Apply one navigation step without keeping the cursor. `at` is where the
    /// visitor is now; `to` is a coordinate (goto) or a dimension number (dim).
    fn navigate(&self, at: &str, dim: u8, op: &str, to: &str) -> ApiNav {
        let origin = libphext::phext::default_coordinate();
        let mut nav = Navigator::at(parse_coordinate(at).unwrap_or(origin));
        if (1..=9).contains(&dim) {
            nav.select_dimension(dim);
        }
        match op {
            "dim" => {
                if let Ok(next) = to.trim().parse::<u8>() {
                    nav.select_dimension(next);
                }
            }
            "forward" => { nav.move_forward(); }
            "backward" => { nav.move_backward(); }
            "next" => { nav.next_populated(self.lattice.index()); }
            "prev" => { nav.prev_populated(self.lattice.index()); }
            "goto" => {
                if let Some(coord) = parse_coordinate(to.trim()) {
                    nav.goto(coord);
                }
            }
            "home" => {
                nav.goto(origin);
                if !self.lattice.has_scroll(&origin) {
                    nav.next_populated(self.lattice.index());
                }
                nav.select_dimension(1);
            }
            "base" => { nav.goto(origin); }
            _ => {}
        }
        nav_view(&self.lattice, &nav, self.scrolls, "demo.phext")
    }

    fn tree(&self) -> ApiTree {
        tree_view(&self.lattice)
    }

    fn status(&self) -> ApiStatus {
        ApiStatus {
            scrolls: self.scrolls,
            bytes: self.bytes,
            file: "demo.phext".into(),
            default_file: "demo.phext".into(),
            dirty: false,
        }
    }

    fn search(&self, query: &str) -> Vec<ApiSearchHit> {
        let query: String = query.trim().chars().take(80).collect();
        if query.is_empty() {
            return Vec::new();
        }
        let buf = self.lattice.to_phext_bytes();
        phext_lattice::search_lattice_auto(&buf, self.lattice.index(), &query, false, 50)
            .into_iter()
            .map(|hit| ApiSearchHit {
                coordinate: format!("{}", hit.coordinate),
                context: hit.context,
                offset: hit.offset,
            })
            .collect()
    }
}

fn demo_bytes() -> Vec<u8> {
    use libphext::phext::{
        BOOK_BREAK, CHAPTER_BREAK, COLLECTION_BREAK, LIBRARY_BREAK, SCROLL_BREAK,
        SECTION_BREAK, SERIES_BREAK, SHELF_BREAK, VOLUME_BREAK, default_coordinate,
    };
    let mut scrolls: Vec<_> = DEMO_SCROLLS
        .iter()
        .filter_map(|(coord, text)| parse_coordinate(coord).map(|c| (c, *text)))
        .collect();
    scrolls.sort_by_key(|(coord, _)| *coord);
    let mut output = Vec::new();
    let mut current = default_coordinate();
    for (coord, text) in scrolls {
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
        output.extend_from_slice(text.as_bytes());
        current = coord;
    }
    output
}

fn serve_demo(
    stream: &mut std::net::TcpStream,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    demo: &DemoCatalog,
) {
    if method != "GET" && method != "HEAD" {
        send_response(stream, 405, "text/plain; charset=utf-8", b"demo is read-only", CacheClass::NoStore, headers, false);
        return;
    }
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    let q = query_pairs(query);
    let body = match route {
        "/api/demo/nav" => {
            let dim = q.get("dim").and_then(|d| d.parse().ok()).unwrap_or(1);
            let nav = demo.navigate(
                q.get("at").map(String::as_str).unwrap_or(""),
                dim,
                q.get("op").map(String::as_str).unwrap_or("stay"),
                q.get("to").map(String::as_str).unwrap_or(""),
            );
            serde_json::to_string(&nav).unwrap_or_else(|_| "{}".into())
        }
        "/api/demo/tree" => serde_json::to_string(&demo.tree()).unwrap_or_else(|_| "{}".into()),
        "/api/demo/status" => serde_json::to_string(&demo.status()).unwrap_or_else(|_| "{}".into()),
        "/api/demo/search" => {
            let hits = demo.search(q.get("q").map(String::as_str).unwrap_or(""));
            serde_json::to_string(&hits).unwrap_or_else(|_| "[]".into())
        }
        _ => {
            send_response(stream, 404, "text/plain; charset=utf-8", b"not found", CacheClass::NoStore, headers, false);
            return;
        }
    };
    send_response(
        stream,
        200,
        "application/json",
        body.as_bytes(),
        CacheClass::NoStore,
        headers,
        method == "HEAD",
    );
}

fn query_pairs(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if query.len() > 2048 {
        return out;
    }
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(percent_decode(key), percent_decode(value));
    }
    out
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn editor_signed_in(gate: &Gate, headers: &HashMap<String, String>) -> bool {
    let cookie = headers.get("cookie").map(String::as_str).unwrap_or("");
    match gate.oauth().read_session(cookie) {
        Some(session) => gate.editors().allows(&session.who, &session.email),
        None => false,
    }
}

const GATE_OPEN: &str = "data-open=\"1\" style=\"display:flex;";
const GATE_SHUT: &str = "data-open=\"0\" style=\"display:none;";

/// Signed-out visitors on a locked server get the intro. A signed-in editor,
/// a server with no auth configured, and `--local` get the editor shell.
/// `--local` stays loopback-only: it skips the intro, and `/about`, `/privacy`,
/// and `/api/demo` stay on their public paths.
fn show_signed_out_gate(gate: &Gate, headers: &HashMap<String, String>) -> bool {
    if gate.is_local() {
        return false;
    }
    gate.locked() && !editor_signed_in(gate, headers)
}

fn index_page(gate: &Gate, headers: &HashMap<String, String>) -> Cow<'static, str> {
    if show_signed_out_gate(gate, headers) {
        Cow::Borrowed(INDEX_HTML)
    } else {
        Cow::Owned(INDEX_HTML.replacen(GATE_OPEN, GATE_SHUT, 1))
    }
}

fn parse_request(reader: &mut BufReader<std::net::TcpStream>) -> Option<(String, String, HashMap<String, String>)> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 { return None; }
    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 { return None; }
    let method = parts[0].to_string();
    let path = parts[1].to_string();
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 { break; }
        let line = line.trim().to_string();
        if line.is_empty() { break; }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    Some((method, path, headers))
}

fn read_body(reader: &mut BufReader<std::net::TcpStream>, headers: &HashMap<String, String>) -> String {
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len == 0 { return String::new(); }
    let mut buf = vec![0u8; len];
    let _ = reader.read_exact(&mut buf);
    String::from_utf8_lossy(&buf).to_string()
}

const HTML: &str = "text/html; charset=utf-8";
/// The shell is one embedded document (CSS and JS are inline), so a minute of
/// freshness plus an ETag is enough. After that the browser revalidates.
const SHELL_CACHE: &str = "public, max-age=60, must-revalidate";
/// Compiled-in bytes (icons, the share image, site artwork) change only when the binary does.
const STATIC_CACHE: &str = "public, max-age=86400, must-revalidate";

#[derive(Clone, Copy)]
enum CacheClass {
    /// API responses, errors, and anything tied to a session.
    NoStore,
    Shell,
    Static,
}

fn cache_control(class: CacheClass) -> &'static str {
    match class {
        CacheClass::NoStore => "no-store",
        CacheClass::Shell => SHELL_CACHE,
        CacheClass::Static => STATIC_CACHE,
    }
}

fn strong_etag(body: &[u8]) -> String {
    let digest = sha2::Sha256::digest(body);
    let mut hex = String::with_capacity(2 + digest.len() * 2);
    hex.push('"');
    for byte in digest {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        hex.push(HEX[(byte >> 4) as usize] as char);
        hex.push(HEX[(byte & 0xf) as usize] as char);
    }
    hex.push('"');
    hex
}

fn if_none_match(headers: &HashMap<String, String>, etag: &str) -> bool {
    let Some(value) = headers.get("if-none-match") else {
        return false;
    };
    value.split(',').any(|part| {
        let part = part.trim();
        part == "*" || part == etag || part.strip_prefix("W/").map(str::trim) == Some(etag)
    })
}

fn response_bytes(
    status: u16,
    content_type: &str,
    body: &[u8],
    class: CacheClass,
    req_headers: &HashMap<String, String>,
    head_only: bool,
) -> Vec<u8> {
    let https = forwarded_https(req_headers);
    let etag = match class {
        CacheClass::NoStore => None,
        CacheClass::Shell | CacheClass::Static => Some(strong_etag(body)),
    };
    if status == 200 {
        if let Some(ref tag) = etag {
            if if_none_match(req_headers, tag) {
                let vary = match class {
                    CacheClass::Shell => "Vary: Cookie\r\n",
                    _ => "",
                };
                let head = format!(
                    "HTTP/1.1 304 Not Modified\r\nETag: {tag}\r\nCache-Control: {}\r\nContent-Length: 0\r\n{vary}Connection: close\r\n{}\r\n",
                    cache_control(class),
                    security_header_block(https)
                );
                return head.into_bytes();
            }
        }
    }
    let etag_line = etag.map(|tag| format!("ETag: {tag}\r\n")).unwrap_or_default();
    let vary = match class {
        CacheClass::Shell => "Vary: Cookie\r\n",
        _ => "",
    };
    let mut out = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: {}\r\n{etag_line}{vary}Connection: close\r\n{}\r\n",
        status,
        status_text(status),
        body.len(),
        cache_control(class),
        security_header_block(https)
    )
    .into_bytes();
    if !head_only {
        out.extend_from_slice(body);
    }
    out
}

fn send_response(
    stream: &mut std::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    class: CacheClass,
    req_headers: &HashMap<String, String>,
    head_only: bool,
) {
    let bytes = response_bytes(status, content_type, body, class, req_headers, head_only);
    let _ = stream.write_all(&bytes);
    let _ = stream.flush();
}

fn json_response(stream: &mut std::net::TcpStream, headers: &HashMap<String, String>, value: &impl Serialize) {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    send_response(stream, 200, "application/json", body.as_bytes(), CacheClass::NoStore, headers, false);
}

/// Path without the query string. Fragments are not sent by the browser.
fn path_route(path: &str) -> &str {
    path.split('?').next().unwrap_or(path)
}

struct PublicDocument {
    content_type: &'static str,
    body: &'static str,
}

fn sponsor_offer() -> &'static SponsorOffer {
    static OFFER: OnceLock<SponsorOffer> = OnceLock::new();
    OFFER.get_or_init(SponsorOffer::from_env)
}

/// Fill the donation slot. Pages without one are returned as-is.
fn render_with_sponsor(html: &str) -> std::borrow::Cow<'_, str> {
    if html.contains(sponsor::SLOT) {
        std::borrow::Cow::Owned(sponsor::apply_slot(html, sponsor_offer()))
    } else {
        std::borrow::Cow::Borrowed(html)
    }
}

fn send_html(
    stream: &mut std::net::TcpStream,
    status: u16,
    template: &str,
    class: CacheClass,
    headers: &HashMap<String, String>,
    head_only: bool,
) {
    let rendered = render_with_sponsor(template);
    send_response(stream, status, HTML, rendered.as_bytes(), class, headers, head_only);
}

/// Pages and crawler files a signed-out visitor can read. Exact paths only.
fn public_document(path: &str) -> Option<PublicDocument> {
    let (content_type, body) = match path_route(path) {
        "/about" => ("text/html; charset=utf-8", ABOUT_HTML),
        "/claim" => ("text/html; charset=utf-8", CLAIM_HTML),
        "/explore" => ("text/html; charset=utf-8", EXPLORE_HTML),
        "/wavefront-explorer.json" => ("application/json; charset=utf-8", WAVEFRONT_EXPLORER_JSON),
        "/privacy" => ("text/html; charset=utf-8", PRIVACY_HTML),
        "/terms" => ("text/html; charset=utf-8", TERMS_HTML),
        "/up-goer-five.html" => ("text/html; charset=utf-8", UP_GOER_FIVE_HTML),
        "/tts-demo.html" => ("text/html; charset=utf-8", TTS_DEMO_HTML),
        "/learn-you-phext.html" => ("text/html; charset=utf-8", LEARN_YOU_PHEXT_HTML),
        "/learn-you-phext.phext" => ("text/plain; charset=utf-8", LEARN_YOU_PHEXT),
        "/singular.html" => ("text/html; charset=utf-8", SINGULAR_HTML),
        "/phext-previewer.html" => ("text/html; charset=utf-8", PREVIEWER_HTML),
        "/phext-tts.js" => ("text/javascript; charset=utf-8", PHEXT_TTS_JS),
        "/robots.txt" => ("text/plain; charset=utf-8", ROBOTS_TXT),
        "/sitemap.xml" => ("application/xml; charset=utf-8", SITEMAP_XML),
        _ => return None,
    };
    Some(PublicDocument { content_type, body })
}

/// The editor shell. `admit` already lets `/` and `/index.html` through unsigned.
fn is_editor_shell(path: &str) -> bool {
    matches!(path_route(path), "/" | "/index.html")
}

/// File, search, and sign-in routes. These stay behind the gate and answer JSON.
fn is_api_path(path: &str) -> bool {
    let route = path_route(path);
    route == "/api" || route.starts_with("/api/")
}

const API_NOT_FOUND: &[u8] = br#"{"ok":false,"error":"not found"}"#;

fn handle_request(
    stream: &mut std::net::TcpStream,
    state: &Arc<Mutex<AppState>>,
    gate: &Gate,
    demo: &DemoCatalog,
) {
    let cloned = match stream.try_clone() { Ok(s) => s, Err(_) => return };
    let mut reader = BufReader::new(cloned);
    let (method, path, headers) = match parse_request(&mut reader) { Some(v) => v, None => return };

    // Same-origin fetches do not need CORS. Nothing else in this repo calls
    // the editor from another origin, and a wildcard would let any site read
    // responses alongside the session cookie. There is no OPTIONS preflight.
    // Icons and the share image stay before the auth gate, so a browser or a
    // crawler can fetch them without signing in. The bytes are compiled in.
    if let Some((content_type, body)) = public_asset(&path) {
        match method.as_str() {
            "GET" => send_response(stream, 200, content_type, body, CacheClass::Static, &headers, false),
            "HEAD" => send_response(stream, 200, content_type, body, CacheClass::Static, &headers, true),
            _ => send_response(stream, 405, "text/plain", b"method not allowed", CacheClass::NoStore, &headers, false),
        }
        return;
    }

    // Public documents, then a real 404. Both are answered before the gate so a
    // signed-out visitor is not handed 401 JSON. /api is unchanged: gate.route
    // still serves sign-in, and gate.admit still requires a session.
    // HTML pages revalidate like the shell. Crawler files are compiled in and
    // change only when the binary does, same as the favicon. The 404 is an
    // error, so it stays no-store (no ETag) and still carries the security headers.
    if let Some(doc) = public_document(&path) {
        let class = if doc.content_type.starts_with("text/html") {
            CacheClass::Shell
        } else {
            CacheClass::Static
        };
        if doc.content_type.starts_with("text/html") {
            match method.as_str() {
                "GET" => send_html(stream, 200, doc.body, class, &headers, false),
                "HEAD" => send_html(stream, 200, doc.body, class, &headers, true),
                _ => send_response(stream, 405, "text/plain", b"method not allowed", CacheClass::NoStore, &headers, false),
            }
        } else {
            let bytes = doc.body.as_bytes();
            match method.as_str() {
                "GET" => send_response(stream, 200, doc.content_type, bytes, class, &headers, false),
                "HEAD" => send_response(stream, 200, doc.content_type, bytes, class, &headers, true),
                _ => send_response(stream, 405, "text/plain", b"method not allowed", CacheClass::NoStore, &headers, false),
            }
        }
        return;
    }
    // `/<name>/` is the same editor shell as `/`, aimed at `<name>.phext`.
    // The shell HTML has no scroll text. Bytes still go through `/api/*`,
    // which `admit` gates the same way as every other file. Signed-out
    // visitors therefore get the sign-in shell, not the phext.
    if let Some(link) = phext_link(&path) {
        let hosted = state.lock().unwrap().hosted_phext(&link.name).is_some();
        if !hosted {
            send_not_found(stream, &method, &headers);
            return;
        }
        if !link.slash {
            send_redirect(stream, &link.location(), &headers);
            return;
        }
        match method.as_str() {
            "GET" => send_response(stream, 200, HTML, INDEX_HTML.as_bytes(), CacheClass::Shell, &headers, false),
            "HEAD" => send_response(stream, 200, HTML, INDEX_HTML.as_bytes(), CacheClass::Shell, &headers, true),
            _ => send_response(stream, 405, "text/plain; charset=utf-8", b"method not allowed", CacheClass::NoStore, &headers, false),
        }
        return;
    }
    if !is_editor_shell(&path) && !is_api_path(&path) && path_route(&path) != "/submit" && path_route(&path) != "/claim/dashboard" {
        match method.as_str() {
            "HEAD" => send_html(stream, 404, NOT_FOUND_HTML, CacheClass::NoStore, &headers, true),
            _ => send_html(stream, 404, NOT_FOUND_HTML, CacheClass::NoStore, &headers, false),
        }
        return;
    }

    // Sign-in endpoints, then the public shell and the read-only demo.
    // Demo routes are answered here, before admit(), so a signed-out visitor
    // never receives a 401 and the real phext files are not in reach.
    if let Some(reply) = gate.route(&method, &path, &headers) {
        // Two things happen here, where a sign-in completes. An X
        // sign-in grants editor (Will's ruling, 2026-11-05): an X
        // account that is not already an editor is admitted on the
        // spot, through the same mechanism as an accepted submission.
        // And a completed X sign-in whose handle the wavefront
        // snapshot does not hold is a wavefront join: the account has
        // arrived in the new era, so it is recorded.
        // Bookkeeping never fails a sign-in, so errors are dropped —
        // the same posture as Editors::admit dropping an editors-file
        // append error.
        if method == "GET" && (300..400).contains(&reply.status) {
            let route = path_route(&path);
            if route.starts_with("/api/auth/") && route.ends_with("/callback") {
                if let Some(session) = reply_session(gate, &reply) {
                    if session.who.starts_with("x:") {
                        if !gate.is_editor(&session) {
                            let _ = gate.admit_account(&session.who, &session.email);
                        }
                        if wavefront::lookup(&session.handle).is_none() {
                            let dir = joins_dir(state);
                            let _ = joins::record(&dir, &session.who, &format!("@{}", session.handle), "sign-in");
                        }
                    }
                }
            }
        }
        reply.write_to(stream);
        return;
    }
    let route = path_route(&path);
    if (method == "GET" || method == "HEAD") && (route == "/" || route == "/index.html") {
        let page = index_page(gate, &headers);
        let rendered = render_with_sponsor(&page);
        let bytes = rendered.as_bytes();
        send_response(stream, 200, HTML, bytes, CacheClass::Shell, &headers, method == "HEAD");
        return;
    }
    if route.starts_with("/api/demo") {
        serve_demo(stream, &method, &path, &headers, demo);
        return;
    }
    // Claims sit with submissions, between sign-in and the editor gate:
    // the lookup is public, claiming needs a signed-in account, and
    // neither needs an editor's place.
    if handle_claim(stream, &method, &path, &headers, state, gate) {
        return;
    }
    // The claim dashboard sits with claims: its own gate (the owner, or
    // an editor) answers inside the handler.
    if handle_dashboard(stream, &method, &path, &headers, state, gate) {
        return;
    }
    // Submissions sit between sign-in and the editor gate: uploading a
    // phext for review needs a signed-in account, not an editor's place.
    if handle_submissions(stream, &mut reader, &method, &path, &headers, state, gate) {
        return;
    }
    if let Err(reply) = gate.admit(&method, &path, &headers) {
        reply.write_to(stream);
        return;
    }

    let mut state = state.lock().unwrap();

    match (method.as_str(), path.as_str()) {
        ("GET", p) if is_editor_shell(p) => {
            send_html(stream, 200, INDEX_HTML, CacheClass::Shell, &headers, false);
        }
        ("HEAD", p) if is_editor_shell(p) => {
            send_html(stream, 200, INDEX_HTML, CacheClass::Shell, &headers, true);
        }

        // ── Phext file management ──
        ("GET", "/api/phexts") => {
            json_response(stream, &headers, &state.list_phexts());
        }
        ("POST", "/api/open") => {
            let body = read_body(&mut reader, &headers);
            let requested = body.trim();
            // A bare library name (`jonas`) resolves only to `<dir>/jonas.phext`.
            // Anything else stays on the existing path check inside open_phext.
            let path_str = if phext_name_ok(requested) {
                match state.hosted_phext(requested) {
                    Some(path) => path.to_string_lossy().into_owned(),
                    None => {
                        json_response(stream, &headers, &ApiOk { ok: false, message: "phext not found".into() });
                        return;
                    }
                }
            } else {
                requested.to_string()
            };
            match state.open_phext(&path_str) {
                Ok(()) => json_response(stream, &headers, &ApiOk { ok: true, message: format!("opened {}", state.file_path) }),
                Err(e) => json_response(stream, &headers, &ApiOk { ok: false, message: e }),
            }
        }

        ("GET", "/api/nav") => { json_response(stream, &headers, &state.nav_json()); }
        ("GET", "/api/status") => {
            let buf = state.lattice.to_phext_bytes();
            json_response(stream, &headers, &ApiStatus {
                scrolls: state.overview.total_scrolls, bytes: buf.len(),
                file: state.file_path.clone(),
                default_file: state.default_file.clone(),
                dirty: state.dirty,
            });
        }
        ("GET", "/api/tree") => { json_response(stream, &headers, &state.tree_json()); }

        // Navigation
        ("POST", "/api/dim") => {
            let body = read_body(&mut reader, &headers);
            if let Ok(d) = body.trim().parse::<u8>() {
                if (1..=9).contains(&d) { state.nav.select_dimension(d); }
            }
            json_response(stream, &headers, &state.nav_json());
        }
        ("POST", "/api/forward") => { state.nav.move_forward(); json_response(stream, &headers, &state.nav_json()); }
        ("POST", "/api/backward") => { state.nav.move_backward(); json_response(stream, &headers, &state.nav_json()); }
        ("POST", "/api/next") => {
            let idx = state.lattice.index().clone();
            state.nav.next_populated(&idx);
            json_response(stream, &headers, &state.nav_json());
        }
        ("POST", "/api/prev") => {
            let idx = state.lattice.index().clone();
            state.nav.prev_populated(&idx);
            json_response(stream, &headers, &state.nav_json());
        }
        ("POST", "/api/goto") => {
            let body = read_body(&mut reader, &headers);
            if let Some(coord) = parse_coordinate(body.trim()) { state.nav.goto(coord); }
            json_response(stream, &headers, &state.nav_json());
        }
        ("POST", "/api/home") => {
            if let Err(e) = state.go_home() { eprintln!("home: {}", e); }
            json_response(stream, &headers, &state.nav_json());
        }
        ("POST", "/api/base") => {
            state.nav.goto(libphext::phext::default_coordinate());
            json_response(stream, &headers, &state.nav_json());
        }

        // Edit
        ("POST", "/api/update") => {
            let body = read_body(&mut reader, &headers);
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body) {
                let coord_str = val.get("coordinate").and_then(|v| v.as_str()).unwrap_or("");
                let content = val.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(coord) = parse_coordinate(coord_str) {
                    state.lattice.write_scroll(coord, content.to_string());
                    state.dirty = true;
                    state.rebuild_overview();
                    json_response(stream, &headers, &ApiOk { ok: true, message: "scroll updated".into() });
                } else {
                    json_response(stream, &headers, &ApiOk { ok: false, message: "invalid coordinate".into() });
                }
            } else {
                json_response(stream, &headers, &ApiOk { ok: false, message: "invalid json".into() });
            }
        }
        ("POST", "/api/save") => {
            match state.lattice.save() {
                Ok(()) => { state.dirty = false; json_response(stream, &headers, &ApiOk { ok: true, message: "saved".into() }); }
                Err(e) => { json_response(stream, &headers, &ApiOk { ok: false, message: format!("save failed: {}", e) }); }
            }
        }

        // Search
        ("POST", "/api/search") => {
            let body = read_body(&mut reader, &headers);
            let query = body.trim();
            if query.is_empty() {
                json_response(stream, &headers, &Vec::<ApiSearchHit>::new());
            } else {
                let buf = state.lattice.to_phext_bytes();
                let hits = phext_lattice::search_lattice_auto(&buf, state.lattice.index(), query, false, 50);
                let api_hits: Vec<ApiSearchHit> = hits.into_iter().map(|h| ApiSearchHit {
                    coordinate: format!("{}", h.coordinate), context: h.context, offset: h.offset,
                }).collect();
                json_response(stream, &headers, &api_hits);
            }
        }

        // Zoom tiers
        ("GET", "/api/zoom/z") => { json_response(stream, &headers, &state.zoom_z()); }
        ("GET", p) if p.starts_with("/api/zoom/y/") => {
            json_response(stream, &headers, &state.zoom_y(&p["/api/zoom/y/".len()..]));
        }
        ("GET", p) if p.starts_with("/api/zoom/x/") => {
            let rest = &p["/api/zoom/x/".len()..];
            let parts: Vec<&str> = rest.splitn(2, '/').collect();
            if parts.len() == 2 { json_response(stream, &headers, &state.zoom_x(parts[0], parts[1])); }
            else { send_response(stream, 400, "text/plain", b"expected /api/zoom/x/z.z.z/y.y.y", CacheClass::NoStore, &headers, false); }
        }

        // Scroll read
        ("GET", p) if p.starts_with("/api/scroll/") => {
            let coord_str = &p["/api/scroll/".len()..];
            if let Some(coord) = parse_coordinate(coord_str) {
                let content = state.lattice.read_scroll(&coord).unwrap_or_default();
                let bytes = content.len();
                json_response(stream, &headers, &ApiScroll { coordinate: format!("{}", coord), content, bytes });
            } else {
                send_response(stream, 400, "text/plain", b"invalid coordinate", CacheClass::NoStore, &headers, false);
            }
        }

        // Unknown API routes stay JSON. The editor shell's other methods keep
        // the plain-text 404 they already returned after a successful admit.
        _ if is_api_path(&path) => {
            send_response(stream, 404, "application/json", API_NOT_FOUND, CacheClass::NoStore, &headers, false);
        }
        _ => { send_response(stream, 404, "text/plain", b"not found", CacheClass::NoStore, &headers, false); }
    }
}

// ── Claims ─────────────────────────────────────────────────────────────

/// Where claim records live: a `claims` subdirectory of the phext
/// library, beside `submissions`, so a claim never reads as a library
/// file. Mirrors `submissions_dir`, fallback included.
fn claims_dir(state: &Arc<Mutex<AppState>>) -> std::path::PathBuf {
    let state = state.lock().unwrap();
    match &state.phext_dir {
        Some(dir) => std::path::Path::new(dir).join("claims"),
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            resolve_data_dir(std::env::var("PHEXT_DATA_DIR").ok().as_deref(), &cwd).join("claims")
        }
    }
}

/// Where join records live: a `joins` subdirectory of the phext
/// library, beside `claims`. Mirrors `claims_dir`, fallback included.
fn joins_dir(state: &Arc<Mutex<AppState>>) -> std::path::PathBuf {
    let state = state.lock().unwrap();
    match &state.phext_dir {
        Some(dir) => std::path::Path::new(dir).join("joins"),
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            resolve_data_dir(std::env::var("PHEXT_DATA_DIR").ok().as_deref(), &cwd).join("joins")
        }
    }
}

/// The session a sign-in reply just sealed, read back from the reply's
/// own Set-Cookie header — the only place the new session exists before
/// the browser sends it back. Used to record a wavefront join at the
/// moment an X sign-in completes.
fn reply_session(gate: &Gate, reply: &Reply) -> Option<Session> {
    let cookie = reply
        .headers
        .iter()
        .filter(|(name, _)| *name == "Set-Cookie")
        .filter_map(|(_, value)| value.strip_prefix("phext_session="))
        .find_map(|value| value.split(';').next())
        .filter(|value| !value.is_empty())?;
    let mut headers = HashMap::new();
    headers.insert("cookie".to_string(), format!("phext_session={cookie}"));
    gate.session(&headers)
}

fn grant_json(tier: Tier) -> serde_json::Value {
    let grant = tier.grant();
    serde_json::json!({
        "storage_bytes": grant.storage_bytes,
        "max_phexts": grant.max_phexts,
        "max_phext_bytes": grant.max_phext_bytes,
        "max_delegates": grant.max_delegates,
    })
}

fn claim_json(claim: &claims::Claim) -> serde_json::Value {
    let mut value = serde_json::to_value(claim).unwrap_or_default();
    if let Some(obj) = value.as_object_mut() {
        obj.insert("ok".into(), serde_json::Value::Bool(true));
        obj.insert("grant".into(), grant_json(claim.tier));
    }
    value
}

/// The public lookup: is this X handle in the wavefront map, where, and
/// what can it claim? Reads only the checked-in snapshot — no session,
/// no filesystem beyond it, and a malformed handle is simply not found.
fn lookup_json(raw: &str) -> serde_json::Value {
    match wavefront::lookup(raw) {
        Some(person) => serde_json::json!({
            "ok": true,
            "found": true,
            "handle": person.handle,
            "name": person.name,
            "placements": person.placements.iter().map(|p| serde_json::json!({
                "coordinate": p.coordinate,
                "region": p.region.label(),
                "label": p.label,
            })).collect::<Vec<_>>(),
            "tier": Tier::Found,
            "grant": grant_json(Tier::Found),
        }),
        None => {
            let typed = raw.trim();
            let shown: String = if let Some(rest) = typed.strip_prefix('@') {
                format!("@{}", rest.chars().take(15).collect::<String>())
            } else {
                format!("@{}", typed.chars().take(15).collect::<String>())
            };
            serde_json::json!({
                "ok": true,
                "found": false,
                "handle": shown,
                "tier": Tier::New,
                "grant": grant_json(Tier::New),
            })
        }
    }
}

fn query_param(path: &str, key: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return Some(url_decode(v));
        }
    }
    None
}

fn url_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| -> Option<u8> {
                    match b {
                        b'0'..=b'9' => Some(b - b'0'),
                        b'a'..=b'f' => Some(b - b'a' + 10),
                        b'A'..=b'F' => Some(b - b'A' + 10),
                        _ => None,
                    }
                };
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push(hi * 16 + lo);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Routes for `/api/claim*`. Returns true when the request was answered
/// here. The lookup is public; claiming needs a signed-in account and
/// claim mode open (the operator opens it once the X app credentials
/// exist — until then the lookup still demos the whole offer).
fn handle_claim(
    stream: &mut std::net::TcpStream,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    state: &Arc<Mutex<AppState>>,
    gate: &Gate,
) -> bool {
    let route = path_route(path);
    if route == "/api/claim/lookup" {
        match method {
            "GET" | "HEAD" => {
                let raw = query_param(path, "handle").unwrap_or_default();
                json_status(stream, headers, 200, lookup_json(&raw));
            }
            _ => json_error(stream, headers, 405, "method not allowed"),
        }
        return true;
    }
    if route != "/api/claim" {
        return false;
    }
    if !gate.claims_open() {
        json_error(stream, headers, 403, "claims are not open yet — the lookup above still works");
        return true;
    }
    // Everything below needs a signed-in account — editor or not. Local
    // mode supplies the synthetic local developer, as the gate does.
    let Some(session) = gate.session(headers) else {
        json_error(stream, headers, 401, "sign in to claim your space");
        return true;
    };
    if method != "GET" && method != "HEAD" && !gate.is_local() {
        let csrf = headers.get("x-csrf-token").map(String::as_str).unwrap_or("");
        if !gate.oauth().csrf_matches(&session, csrf) {
            json_error(stream, headers, 403, "that request did not match this sign-in");
            return true;
        }
    }
    let dir = claims_dir(state);
    match method {
        "GET" | "HEAD" => match claims::get(&dir, &session.who) {
            Some(claim) => json_status(stream, headers, 200, claim_json(&claim)),
            None => json_status(stream, headers, 200, serde_json::json!({ "ok": true, "claimed": false })),
        },
        "POST" => {
            // Only an X sign-in carries the handle the map is keyed by.
            // Any other provider claims at the new tier; the claim can be
            // matched to the map later by signing in with X.
            let (handle, tier, placements) = if session.who.starts_with("x:") {
                match wavefront::lookup(&session.handle) {
                    Some(person) => (
                        person.handle,
                        Tier::Found,
                        person.placements.iter().map(|p| p.coordinate.clone()).collect::<Vec<String>>(),
                    ),
                    None => (format!("@{}", session.handle), Tier::New, Vec::new()),
                }
            } else {
                (String::new(), Tier::New, Vec::new())
            };
            match claims::record(&dir, &session.who, &handle, tier, &placements) {
                Ok(claim) => {
                    // A claim at the new tier is also a wavefront join:
                    // the map had not placed this account, and it has
                    // arrived in the new era. A found claim is not a
                    // join — the map already held the account. Join
                    // bookkeeping never fails a claim (record is
                    // get-or-create, so re-claiming changes nothing).
                    if claim.tier == Tier::New {
                        let join_dir = joins_dir(state);
                        let _ = joins::record(&join_dir, &claim.who, &claim.handle, "claim");
                    }
                    json_status(stream, headers, 200, claim_json(&claim));
                }
                Err(e) => json_error(stream, headers, 500, &e),
            }
        }
        _ => {
            json_error(stream, headers, 405, "method not allowed");
        }
    }
    true
}

// ── Submissions ────────────────────────────────────────────────────────

/// Largest phext a visitor may upload for review (10 MiB).
const MAX_SUBMISSION_BYTES: usize = 10 * 1024 * 1024;

fn json_status(stream: &mut std::net::TcpStream, headers: &HashMap<String, String>, status: u16, value: serde_json::Value) {
    send_response(stream, status, "application/json", value.to_string().as_bytes(), CacheClass::NoStore, headers, false);
}

fn json_error(stream: &mut std::net::TcpStream, headers: &HashMap<String, String>, status: u16, message: &str) {
    json_status(stream, headers, status, serde_json::json!({ "ok": false, "error": message }));
}

/// Where uploads wait for review: a `submissions` subdirectory of the
/// phext library. `list_phexts` scans only the library directory itself
/// (non-recursively), so a pending upload never shows up as a library
/// file. Without a library directory, fall back to the data dir,
/// mirroring how `--local` finds its phexts.
fn submissions_dir(state: &Arc<Mutex<AppState>>) -> std::path::PathBuf {
    let state = state.lock().unwrap();
    match &state.phext_dir {
        Some(dir) => std::path::Path::new(dir).join("submissions"),
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            resolve_data_dir(std::env::var("PHEXT_DATA_DIR").ok().as_deref(), &cwd).join("submissions")
        }
    }
}

fn owns(session: &Session, sub: &Submission) -> bool {
    sub.who == session.who || (!session.email.is_empty() && sub.email == session.email)
}

/// Routes for `/submit` and `/api/submissions*`. Returns true when the
/// request was answered here. Unknown shapes return false, so they fall
/// through to the gate and the usual JSON 404, exactly as before.
fn handle_submissions(
    stream: &mut std::net::TcpStream,
    reader: &mut BufReader<std::net::TcpStream>,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    state: &Arc<Mutex<AppState>>,
    gate: &Gate,
) -> bool {
    let route = path_route(path);
    if route == "/submit" {
        match method {
            "GET" => send_html(stream, 200, SUBMIT_HTML, CacheClass::Shell, headers, false),
            "HEAD" => send_html(stream, 200, SUBMIT_HTML, CacheClass::Shell, headers, true),
            _ => send_response(stream, 405, "text/plain", b"method not allowed", CacheClass::NoStore, headers, false),
        }
        return true;
    }
    if route != "/api/submissions" && !route.starts_with("/api/submissions/") {
        return false;
    }

    // Everything below needs a signed-in account — editor or not. Local
    // mode supplies the synthetic local developer, as the gate does.
    let Some(session) = gate.session(headers) else {
        json_error(stream, headers, 401, "sign in to use submissions");
        return true;
    };
    // Writes carry the session's CSRF token, exactly as `Gate::admit`
    // requires of session writes. Local mode has no cookie to protect.
    if method != "GET" && method != "HEAD" && !gate.is_local() {
        let csrf = headers.get("x-csrf-token").map(String::as_str).unwrap_or("");
        if !gate.oauth().csrf_matches(&session, csrf) {
            json_error(stream, headers, 403, "that request did not match this sign-in");
            return true;
        }
    }
    let editor = gate.is_editor(&session);
    let dir = submissions_dir(state);

    match (method, route) {
        ("POST", "/api/submissions") => {
            if let Some(len) = headers.get("content-length").and_then(|v| v.trim().parse::<usize>().ok()) {
                if len > MAX_SUBMISSION_BYTES {
                    json_error(stream, headers, 413, "that phext is larger than 10 MiB");
                    return true;
                }
            }
            let Some(filename) = headers.get("x-filename").cloned() else {
                json_error(stream, headers, 400, "missing x-filename header");
                return true;
            };
            let body = read_body(reader, headers);
            if body.is_empty() {
                json_error(stream, headers, 400, "the phext is empty");
                return true;
            }
            if body.len() > MAX_SUBMISSION_BYTES {
                json_error(stream, headers, 413, "that phext is larger than 10 MiB");
                return true;
            }
            match submissions::create(&dir, &filename, &session.who, &session.email, &session.name, &body) {
                Ok(sub) => json_status(stream, headers, 200, serde_json::to_value(&sub).unwrap_or_default()),
                Err(e) => json_error(stream, headers, 400, &e),
            }
            true
        }
        ("GET", "/api/submissions") => {
            let mut subs = submissions::list(&dir);
            if !editor {
                subs.retain(|sub| owns(&session, sub));
            }
            json_status(stream, headers, 200, serde_json::to_value(&subs).unwrap_or_default());
            true
        }
        _ => {
            // /api/submissions/{id}/download | accept | decline
            let rest = &route["/api/submissions/".len()..];
            let Some((id, action)) = rest.split_once('/') else { return false };
            match (method, action) {
                ("GET", "download") => {
                    let Some(sub) = submissions::get(&dir, id) else {
                        json_error(stream, headers, 404, "submission not found");
                        return true;
                    };
                    if !editor && !owns(&session, &sub) {
                        json_error(stream, headers, 403, "that submission is not yours");
                        return true;
                    }
                    match submissions::content(&dir, id) {
                        Some(bytes) => {
                            send_response(stream, 200, "text/plain; charset=utf-8", &bytes, CacheClass::NoStore, headers, false)
                        }
                        None => json_error(stream, headers, 404, "submission file not found"),
                    }
                    true
                }
                ("POST", "accept") => {
                    if !editor {
                        json_error(stream, headers, 403, "only editors can accept a submission");
                        return true;
                    }
                    accept_submission(stream, headers, state, gate, &session, &dir, id);
                    true
                }
                ("POST", "decline") => {
                    if !editor {
                        json_error(stream, headers, 403, "only editors can decline a submission");
                        return true;
                    }
                    let Some(sub) = submissions::get(&dir, id) else {
                        json_error(stream, headers, 404, "submission not found");
                        return true;
                    };
                    if sub.status != SubmissionStatus::Pending {
                        json_error(stream, headers, 409, "that submission was already decided");
                        return true;
                    }
                    match submissions::set_decision(&dir, id, SubmissionStatus::Declined, &session.who, "") {
                        Ok(sub) => json_status(stream, headers, 200, serde_json::to_value(&sub).unwrap_or_default()),
                        Err(e) => json_error(stream, headers, 500, &e),
                    }
                    true
                }
                _ => false,
            }
        }
    }
}

/// Accept one pending submission: curate its phext into the library,
/// record the decision, then admit the originator as an editor — in that
/// order, so a failure leaves the submission pending and nobody admitted.
fn accept_submission(
    stream: &mut std::net::TcpStream,
    headers: &HashMap<String, String>,
    state: &Arc<Mutex<AppState>>,
    gate: &Gate,
    session: &Session,
    dir: &std::path::Path,
    id: &str,
) {
    let Some(sub) = submissions::get(dir, id) else {
        json_error(stream, headers, 404, "submission not found");
        return;
    };
    if sub.status != SubmissionStatus::Pending {
        json_error(stream, headers, 409, "that submission was already decided");
        return;
    }
    let library = state.lock().unwrap().phext_dir.clone();
    let Some(library) = library else {
        json_error(stream, headers, 500, "no library directory configured");
        return;
    };
    let Some(mut curated) = submissions::sanitize_filename(&sub.filename) else {
        json_error(stream, headers, 500, "the submission's file name is not usable");
        return;
    };
    let library_path = std::path::Path::new(&library);
    if library_path.join(&curated).exists() {
        let stem = curated.strip_suffix(".phext").unwrap_or(&curated).to_string();
        let short: String = sub.id.chars().take(6).collect();
        curated = format!("{stem}-{short}.phext");
        if library_path.join(&curated).exists() {
            curated = format!("{stem}-{}.phext", sub.id);
        }
    }
    let Some(bytes) = submissions::content(dir, id) else {
        json_error(stream, headers, 500, "submission file not found");
        return;
    };
    if let Err(e) = std::fs::write(library_path.join(&curated), &bytes) {
        json_error(stream, headers, 500, &format!("could not curate the phext: {e}"));
        return;
    }
    match submissions::set_decision(dir, id, SubmissionStatus::Accepted, &session.who, &curated) {
        Ok(done) => {
            // Local mode's synthetic account has no provider id to admit;
            // it is already all-powerful on its own loopback server.
            let admitted = gate.admit_account(&sub.who, &sub.email) || gate.is_local();
            let mut value = serde_json::to_value(&done).unwrap_or_default();
            if let Some(obj) = value.as_object_mut() {
                obj.insert("admitted".into(), serde_json::Value::Bool(admitted));
            }
            json_status(stream, headers, 200, value);
        }
        Err(e) => json_error(stream, headers, 500, &e),
    }
}

/// Canonical scroll address: `LB.SF.SR/CN.VM.BK/CH.SN.SC`.
/// That is the 9-number form of the 11D phext address (line and column sit
/// inside the scroll). libphext also accepts `;` in place of `/` in URLs.
/// A fragment that does not parse falls back to this coordinate.
#[cfg_attr(not(test), allow(dead_code))]
const DEFAULT_COORD: &str = "1.1.1/1.1.1/1.1.1";

fn parse_dimension(bit: &str) -> Option<usize> {
    if bit.is_empty() || !bit.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: usize = bit.parse().ok()?;
    // libphext 0.4 removed COORDINATE_MAXIMUM. A dimension is any
    // number at or above the human minimum.
    (n >= libphext::phext::COORDINATE_MINIMUM).then_some(n)
}

fn coordinate_from(nums: [usize; 9]) -> libphext::phext::Coordinate {
    libphext::phext::Coordinate {
        z: libphext::phext::ZCoordinate {
            library: nums[0],
            shelf: nums[1],
            series: nums[2],
        },
        y: libphext::phext::YCoordinate {
            collection: nums[3],
            volume: nums[4],
            book: nums[5],
        },
        x: libphext::phext::XCoordinate {
            chapter: nums[6],
            section: nums[7],
            scroll: nums[8],
        },
    }
}

fn parse_coordinate(s: &str) -> Option<libphext::phext::Coordinate> {
    let s = s.trim();
    if s.is_empty() || s.len() > 80 {
        return None;
    }
    // Nine dimensions separated by `/`: 1/2/3/4/5/6/7/8/9
    if !s.contains('.') && !s.contains(';') && s.matches('/').count() == 8 {
        let mut nums = [0usize; 9];
        for (i, bit) in s.split('/').enumerate() {
            nums[i] = parse_dimension(bit)?;
        }
        return Some(coordinate_from(nums));
    }
    // Canonical LB.SF.SR/CN.VM.BK/CH.SN.SC, or the same with `;` (libphext URL form).
    let normalized = s.replace(';', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut nums = [0usize; 9];
    for (group, part) in parts.iter().enumerate() {
        let bits: Vec<&str> = part.split('.').collect();
        if bits.len() != 3 {
            return None;
        }
        for (i, bit) in bits.iter().enumerate() {
            nums[group * 3 + i] = parse_dimension(bit)?;
        }
    }
    Some(coordinate_from(nums))
}

/// Library names that are already pages. `/terms` stays the terms page;
/// `/terms/` is not a phext link either.
fn reserved_phext_name(name: &str) -> bool {
    // Single-segment pages. A dotted asset such as singular.html never matches
    // phext_name_ok, and /claim/dashboard has two segments. submit, claim, and
    // explore landed on exo after this route was written; they stay pages.
    matches!(
        name,
        "about" | "api" | "index" | "privacy" | "terms" | "submit" | "claim" | "explore"
    )
}

/// `[A-Za-z0-9_-]+`. Dots, slashes, and `..` never match.
fn phext_name_ok(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Decode one URL path segment. `.`, `/`, `\`, and NUL are rejected so a
/// decoded name cannot be a traversal. Invalid `%` sequences are rejected.
fn decode_phext_segment(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = hex_nibble(bytes[i + 1])?;
                let lo = hex_nibble(bytes[i + 2])?;
                let b = (hi << 4) | lo;
                if b == 0 || b == b'/' || b == b'\\' || b == b'.' || b == b'%' {
                    return None;
                }
                out.push(b);
                i += 3;
            }
            b'.' | b'/' | b'\\' | 0 => return None,
            b if b.is_ascii_alphanumeric() || b == b'_' || b == b'-' => {
                out.push(b);
                i += 1;
            }
            _ => return None,
        }
    }
    let name = String::from_utf8(out).ok()?;
    if phext_name_ok(&name) && !reserved_phext_name(&name) {
        Some(name)
    } else {
        None
    }
}

struct PhextLink {
    name: String,
    /// `/name` redirects; `/name/` is the shell.
    slash: bool,
    query: String,
}

impl PhextLink {
    fn location(&self) -> String {
        let mut loc = format!("/{}/", self.name);
        if !self.query.is_empty() {
            loc.push('?');
            loc.push_str(&self.query);
        }
        loc
    }
}

fn safe_link_query(query: &str) -> String {
    if query.is_empty() || query.len() > 200 {
        return String::new();
    }
    if query
        .bytes()
        .any(|b| b < 0x20 || b == 0x7f || b == b' ' || b == b'\\' || b == b'#')
    {
        return String::new();
    }
    query.to_string()
}

/// `Some` only for a single safe segment: `/jonas` or `/jonas/`.
/// Public assets, `/api/*`, and traversal attempts are `None` so the
/// existing routes and the HTML 404 keep them.
fn phext_link(path: &str) -> Option<PhextLink> {
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let route = path_route(path);
    if !route.starts_with('/') || route.starts_with("//") {
        return None;
    }
    let mut rest = &route[1..];
    let slash = if let Some(stripped) = rest.strip_suffix('/') {
        rest = stripped;
        true
    } else {
        false
    };
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    let name = decode_phext_segment(rest)?;
    Some(PhextLink {
        name,
        slash,
        query: safe_link_query(query),
    })
}

fn hosted_phext_file(
    dir: Option<&str>,
    current_file: &str,
    name: &str,
) -> Option<std::path::PathBuf> {
    if !phext_name_ok(name) || reserved_phext_name(name) {
        return None;
    }
    let file_name = format!("{name}.phext");
    if std::path::Path::new(&file_name).components().count() != 1 {
        return None;
    }
    if let Some(dir) = dir {
        let dir_canon = std::fs::canonicalize(dir).ok()?;
        let candidate = dir_canon.join(&file_name);
        if candidate.parent() != Some(dir_canon.as_path()) {
            return None;
        }
        let file_canon = std::fs::canonicalize(&candidate).ok()?;
        if !file_canon.starts_with(&dir_canon) {
            return None;
        }
        if file_canon.file_name().and_then(|n| n.to_str()) != Some(file_name.as_str()) {
            return None;
        }
        return file_canon.is_file().then_some(file_canon);
    }
    let current = std::path::Path::new(current_file);
    if current.file_name().and_then(|n| n.to_str()) != Some(file_name.as_str()) {
        return None;
    }
    let canon = std::fs::canonicalize(current).ok()?;
    canon.is_file().then_some(canon)
}

fn send_not_found(
    stream: &mut std::net::TcpStream,
    method: &str,
    headers: &HashMap<String, String>,
) {
    let accept = headers
        .get("accept")
        .map(|v| v.to_ascii_lowercase())
        .unwrap_or_default();
    let json = accept.contains("application/json") && !accept.contains("text/html");
    let (content_type, body): (&str, &[u8]) = if json {
        ("application/json", API_NOT_FOUND)
    } else {
        (HTML, NOT_FOUND_HTML.as_bytes())
    };
    send_response(
        stream,
        404,
        content_type,
        body,
        CacheClass::NoStore,
        headers,
        method == "HEAD",
    );
}

fn send_redirect(
    stream: &mut std::net::TcpStream,
    location: &str,
    headers: &HashMap<String, String>,
) {
    let https = forwarded_https(headers);
    let body = format!("Redirecting to {location}\n");
    let head = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n{}\r\n",
        body.len(),
        security_header_block(https)
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

fn get_flag(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

/// First argument that is not a flag or a flag's value.
fn positional_file(args: &[String]) -> Option<String> {
    // "--token" is retired (sign-in only since 2026-11-08) but stays in
    // this list so a command line or unit file that still passes it has
    // its value skipped rather than mistaken for a phext file.
    const VALUED: &[&str] = &[
        "--port", "--host", "--dir", "--token", "--home", "--home-coord",
    ];
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if VALUED.contains(&arg.as_str()) {
            i += 2;
            continue;
        }
        if arg == "--local" || arg == "--help" || arg == "-h" {
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(arg.clone());
    }
    None
}

/// `--local` listen address. An omitted `--host` is loopback. Anything else
/// must already be loopback, or startup is refused.
fn resolve_local_host(host: Option<&str>) -> Result<String, String> {
    match host.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok("127.0.0.1".to_string()),
        Some(host) if phext_lattice::oauth::is_loopback_bind(host) => {
            if host.contains(':') && !host.starts_with('[') {
                Ok(format!("[{host}]"))
            } else {
                Ok(host.to_string())
            }
        }
        Some(host) => Err(format!(
            "--local disables authentication and is only allowed when the server binds to a loopback address (127.0.0.1 or ::1).\nRefusing to listen on {host}.\nPass --host 127.0.0.1 or --host ::1, or omit --host."
        )),
    }
}

const LOCAL_STARTER: &[u8] =
    b"Local development phext.\nThis directory is separate from the production library.\n";

fn io_context(op: &str, path: &Path, err: impl std::fmt::Display) -> String {
    format!("{op} {}: {err}", path.display())
}

/// `.phext` files, compared without case so `Notes.PHEXT` counts on Windows.
fn is_phext(path: &Path) -> bool {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => ext.eq_ignore_ascii_case("phext"),
        None => false,
    }
}

/// `PHEXT_DATA_DIR` when it is set, otherwise `.phext-local` in `cwd`.
///
/// Relative values are joined onto `cwd` with `Path::join`. An absolute value
/// is kept as given. `/var/lib/phext-edit` is not a fallback; it is used only
/// when the variable is set to that path. On Windows a leading `/` is not an
/// absolute path (it is the root of the current drive).
fn resolve_data_dir(env_value: Option<&str>, cwd: &Path) -> PathBuf {
    match env_value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(dir) => {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        }
        None => cwd.join(".phext-local"),
    }
}

/// Phext files in `dir`. A directory that cannot be read is an error that
/// names the operation and the full path, not an empty list.
fn list_phext_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(dir).map_err(|err| io_context("read directory", dir, err))?;
    let mut phexts = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| io_context("read directory", dir, err))?;
        let path = entry.path();
        if is_phext(&path) && path.is_file() {
            phexts.push(path);
        }
    }
    phexts.sort();
    Ok(phexts)
}

/// Create the local data directory and a starter phext when it has none.
///
/// Returns the phext file to open. The directory is created with
/// `create_dir_all` when it does not exist, including when a parent is missing.
fn ensure_local_data_dir(dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|err| io_context("create directory", dir, err))?;
    let existing = list_phext_files(dir)?;
    if let Some(file) = existing.into_iter().next() {
        return Ok(file);
    }
    let welcome = dir.join("welcome.phext");
    write_starter(&welcome)?;
    Ok(welcome)
}

fn write_starter(path: &Path) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|err| io_context("create", path, err))?;
    file.write_all(LOCAL_STARTER).map_err(|err| io_context("write", path, err))?;
    file.flush().map_err(|err| io_context("flush", path, err))?;
    file.sync_all().map_err(|err| io_context("sync", path, err))?;
    let len = std::fs::metadata(path).map_err(|err| io_context("stat", path, err))?.len();
    if len == 0 {
        return Err(format!("write {}: file is empty after write", path.display()));
    }
    Ok(())
}

/// Data directory plus the phext `--local` should open, from a possibly missing dir.
fn local_startup_paths(env_value: Option<&str>, cwd: &Path) -> Result<(PathBuf, PathBuf), String> {
    let dir = resolve_data_dir(env_value, cwd);
    let file = ensure_local_data_dir(&dir)?;
    Ok((dir, file))
}

/// Make `path` absolute so an error can name the full location.
fn make_absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(err) => {
            eprintln!("error: read current directory: {err}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod local_mode_tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use phext_lattice::oauth::{Editors, Oauth, TEST_CSRF};
    use phext_lattice::MappedLattice;

    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("phext-edit".to_string()).chain(args.iter().map(|s| (*s).to_string())).collect()
    }

    fn serve(local: bool) -> (u16, tempfile::TempDir) {
        // Locked by sign-in, as phext-edit now runs: no bearer token.
        let (port, dir, _gate) =
            serve_gate(Gate::new(Oauth::test_ready(), Editors::default()), local);
        (port, dir)
    }

    fn serve_gate(gate: Gate, local: bool) -> (u16, tempfile::TempDir, Arc<Gate>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.phext");
        std::fs::write(&path, b"local note\n").unwrap();
        let lattice = MappedLattice::open(&path).unwrap();
        let mut app = AppState::new(lattice, path.to_string_lossy().to_string());
        app.phext_dir = Some(dir.path().to_string_lossy().to_string());
        let state = Arc::new(Mutex::new(app));
        if local {
            gate.enable_local();
        }
        let gate = Arc::new(gate);
        let demo = DemoCatalog::load();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server_gate = Arc::clone(&gate);
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else { break };
                let state = Arc::clone(&state);
                let gate = Arc::clone(&server_gate);
                handle_request(&mut stream, &state, &gate, &demo);
            }
        });
        (port, dir, gate)
    }

    fn http(port: u16, method: &str, path: &str, extra: &str) -> (u16, String) {
        http_body(port, method, path, extra, "")
    }

    fn http_body(port: u16, method: &str, path: &str, extra: &str, body: &str) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        stream.set_write_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{extra}\r\n{body}"
        );
        stream.write_all(req.as_bytes()).unwrap();
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        let code: u16 = status.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut raw = Vec::new();
        reader.read_to_end(&mut raw).unwrap();
        // Icons and /og.png are binary. Headers stay ASCII; the body is only
        // scanned for those header lines in the share-asset test.
        (code, String::from_utf8_lossy(&raw).into_owned())
    }

    /// `POST` the way the file switcher does: the phext path is the `text/plain` body.
    /// `headers` is extra header lines, each already ending in `\r\n`.
    fn http_with_body(port: u16, method: &str, path: &str, headers: &str, body: &str) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        stream.set_write_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{headers}Content-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(req.as_bytes()).unwrap();
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        let code: u16 = status.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut raw = Vec::new();
        reader.read_to_end(&mut raw).unwrap();
        (code, String::from_utf8_lossy(&raw).into_owned())
    }

    fn response_body(raw: &str) -> &str {
        raw.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or(raw)
    }

    #[test]
    fn protected_routes_require_auth_without_local() {
        let (port, _dir) = serve(false);
        let (code, body) = http(port, "GET", "/api/status", "");
        assert_eq!(code, 401, "{body}");
        let (code, _) = http(port, "GET", "/api/nav", "");
        assert_eq!(code, 401);
        let (code, body) = http(port, "GET", "/api/session", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"signed_in\":false"));
        assert!(!body.contains("local:developer"));
        let (code, body) = http(port, "GET", "/", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("data-open=\"1\""), "signed-out intro stays up without --local");
        assert!(body.contains("Try the demo"));
        let (code, body) = http(port, "GET", "/?demo", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("const wantDemo"));
        let (code, body) = http(port, "GET", "/api/demo/nav", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("Welcome to the sample phext"));
        assert!(!body.contains("local note"));
        let (code, _) = http(port, "GET", "/about", "");
        assert_eq!(code, 200);
        let (code, _) = http(port, "GET", "/privacy", "");
        assert_eq!(code, 200);
    }

    #[test]
    fn local_on_loopback_admits_protected_routes() {
        let (port, _dir) = serve(true);
        let (code, body) = http(port, "GET", "/api/status", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("note.phext"));
        let (code, _) = http(port, "GET", "/api/nav", "");
        assert_eq!(code, 200);
        let (code, body) = http(port, "GET", "/api/session", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("local:developer"));
        assert!(body.contains("\"editor\":true"));
        let (code, _) = http(port, "GET", "/api/status", "X-Forwarded-For: 203.0.113.9\r\n");
        assert_eq!(code, 403);
        let (code, _) = http(port, "GET", "/api/status", "Host: edit.phext.io\r\n");
        assert_eq!(code, 403);
        let (code, body) = http(port, "GET", "/", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("data-open=\"0\""), "--local opens the editor");
        assert!(!body.contains("data-open=\"1\""));
        let (code, body) = http(port, "GET", "/?demo", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("const wantDemo"));
        let (code, body) = http(port, "GET", "/api/demo/nav", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("Welcome to the sample phext"));
        let (code, _) = http(port, "GET", "/about", "");
        assert_eq!(code, 200);
        let (code, _) = http(port, "GET", "/privacy", "");
        assert_eq!(code, 200);
    }

    #[test]
    fn local_with_a_non_loopback_bind_is_refused() {
        let err = resolve_local_host(Some("0.0.0.0")).unwrap_err();
        assert!(err.contains("loopback"), "{err}");
        assert!(err.contains("0.0.0.0"), "{err}");
        assert!(err.contains("127.0.0.1"), "{err}");
        assert!(resolve_local_host(Some("192.168.1.10")).is_err());
        assert!(resolve_local_host(Some("edit.phext.io")).is_err());
        assert!(resolve_local_host(Some("::")).is_err());
        assert_eq!(resolve_local_host(None).unwrap(), "127.0.0.1");
        assert_eq!(resolve_local_host(Some("127.0.0.1")).unwrap(), "127.0.0.1");
        assert_eq!(resolve_local_host(Some("::1")).unwrap(), "[::1]");
        assert_eq!(resolve_local_host(Some("localhost")).unwrap(), "localhost");
        let public: std::net::SocketAddr = "8.8.8.8:8080".parse().unwrap();
        let loopback: std::net::SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let v6: std::net::SocketAddr = "[::1]:8080".parse().unwrap();
        assert!(!public.ip().is_loopback());
        assert!(loopback.ip().is_loopback());
        assert!(v6.ip().is_loopback());
    }

    #[test]
    fn local_data_dir_defaults_away_from_production() {
        let cwd = std::env::temp_dir().join("phext-lattice-cwd");
        assert_eq!(resolve_data_dir(None, &cwd), cwd.join(".phext-local"));
        assert_eq!(resolve_data_dir(Some("  "), &cwd), cwd.join(".phext-local"));
        assert_eq!(resolve_data_dir(Some("my data"), &cwd), cwd.join("my data"));
        let production = std::env::temp_dir().join("var").join("lib").join("phext-edit");
        let production_str = production.to_str().expect("temp path is unicode");
        assert_eq!(resolve_data_dir(Some(production_str), &cwd), production);
        assert_ne!(resolve_data_dir(None, &cwd), production);
        // `/var/lib/phext-edit` is not the default. On Unix it is absolute, so an
        // explicit PHEXT_DATA_DIR of that path is kept. On Windows a leading
        // slash is the current drive root, not an absolute path with a prefix.
        #[cfg(unix)]
        {
            assert_eq!(
                resolve_data_dir(Some("/var/lib/phext-edit"), &cwd),
                PathBuf::from("/var/lib/phext-edit")
            );
            assert_ne!(resolve_data_dir(None, &cwd), PathBuf::from("/var/lib/phext-edit"));
        }
        #[cfg(windows)]
        {
            let win = PathBuf::from(r"C:\phext data\library");
            assert!(win.is_absolute());
            assert_eq!(resolve_data_dir(Some(r"C:\phext data\library"), &cwd), win);
            assert_ne!(resolve_data_dir(None, &cwd), win);
        }

        let dir = tempfile::tempdir().unwrap();
        let file = ensure_local_data_dir(dir.path()).unwrap();
        assert_eq!(file, dir.path().join("welcome.phext"));
        assert!(file.is_file());
        ensure_local_data_dir(dir.path()).unwrap();
        let phexts = list_phext_files(dir.path()).unwrap();
        assert_eq!(phexts.len(), 1);
    }

    fn submission_id(body: &str) -> String {
        let start = body.find("\"id\":\"").expect("id field") + "\"id\":\"".len();
        body[start..].split('"').next().unwrap().to_string()
    }

    fn upload(port: u16, headers: &str, filename: &str, payload: &str) -> (u16, String) {
        http_body(
            port,
            "POST",
            "/api/submissions",
            &format!("{headers}X-Filename: {filename}\r\nContent-Length: {}\r\n", payload.len()),
            payload,
        )
    }

    #[test]
    fn submissions_roundtrip_in_local_mode() {
        let (port, dir) = serve(true);
        let (code, body) = http(port, "GET", "/submit", "");
        assert_eq!(code, 200);
        assert!(body.contains("Upload a phext for review"), "{body}");

        let (code, body) = upload(port, "", "demo.phext", "alpha scroll\n");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"status\":\"pending\""), "{body}");
        let id = submission_id(&body);

        // Pending uploads wait in submissions/, invisible to the library.
        let (code, body) = http(port, "GET", "/api/phexts", "");
        assert_eq!(code, 200);
        assert!(!body.contains("demo.phext"), "{body}");
        let (code, body) = http(port, "GET", "/api/submissions", "");
        assert_eq!(code, 200);
        assert!(body.contains("demo.phext"), "{body}");
        assert!(dir.path().join("submissions").join(format!("{id}.phext")).is_file());

        // Accept: the phext is curated into the library.
        let (code, body) = http(port, "POST", &format!("/api/submissions/{id}/accept"), "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"curated_as\":\"demo.phext\""), "{body}");
        assert!(body.contains("\"admitted\":true"), "{body}");
        assert_eq!(std::fs::read_to_string(dir.path().join("demo.phext")).unwrap(), "alpha scroll\n");
        let (code, body) = http(port, "GET", "/api/phexts", "");
        assert_eq!(code, 200);
        assert!(body.contains("demo.phext"), "{body}");

        // A second upload can be declined, and a decided one stays decided.
        let (code, body) = upload(port, "", "beta.phext", "beta scroll\n");
        assert_eq!(code, 200, "{body}");
        let id2 = submission_id(&body);
        let (code, body) = http(port, "POST", &format!("/api/submissions/{id2}/decline"), "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"status\":\"declined\""), "{body}");
        let (code, _) = http(port, "POST", &format!("/api/submissions/{id2}/accept"), "");
        assert_eq!(code, 409);

        // Bad uploads are refused before anything is stored.
        let (code, _) = upload(port, "", "evil.txt", "nope\n");
        assert_eq!(code, 400);
        let (code, _) = upload(port, "", "empty.phext", "");
        assert_eq!(code, 400);
    }

    /// Will's report: edit a scroll, insert a new one, save. The disk save
    /// must succeed and both scrolls must be readable after the file is reopened.
    #[test]
    fn save_after_edit_and_insert_scroll() {
        let (port, dir) = serve(true);
        let edit = r#"{"coordinate":"1.1.1/1.1.1/1.1.1","content":"edited note"}"#;
        let (code, body) = http_body(
            port,
            "POST",
            "/api/update",
            &format!("Content-Type: application/json\r\nContent-Length: {}\r\n", edit.len()),
            edit,
        );
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"ok\":true"), "{body}");

        let insert = r#"{"coordinate":"1.1.1/1.1.1/1.1.2","content":"brand new scroll"}"#;
        let (code, body) = http_body(
            port,
            "POST",
            "/api/update",
            &format!("Content-Type: application/json\r\nContent-Length: {}\r\n", insert.len()),
            insert,
        );
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"ok\":true"), "{body}");

        let (code, body) = http(port, "POST", "/api/save", "");
        assert_eq!(code, 200, "{body}");
        assert!(
            body.contains("\"ok\":true") && body.contains("\"message\":\"saved\""),
            "disk save failed: {body}"
        );

        let reloaded = MappedLattice::open(dir.path().join("note.phext")).unwrap();
        let edited = libphext::phext::to_coordinate("1.1.1/1.1.1/1.1.1");
        let inserted = libphext::phext::to_coordinate("1.1.1/1.1.1/1.1.2");
        assert_eq!(reloaded.read_scroll(&edited).as_deref(), Some("edited note"));
        assert_eq!(reloaded.read_scroll(&inserted).as_deref(), Some("brand new scroll"));
    }

    /// A session cookie for the scripted Google identity (an editor in
    /// these tests), minted by driving the gate's sign-in flow directly.
    fn google_session_cookie(gate: &Gate) -> String {
        let plain: HashMap<String, String> = HashMap::new();
        let start = gate.route("GET", "/api/auth/google?next=/submit", &plain).unwrap();
        let location = start.headers.iter().find(|(n, _)| *n == "Location").unwrap().1.clone();
        let state = location.split("state=").nth(1).unwrap().split('&').next().unwrap();
        let pending = start.headers.iter().find(|(n, _)| *n == "Set-Cookie").unwrap().1
            .split(';').next().unwrap().to_string();
        let mut cb: HashMap<String, String> = HashMap::new();
        cb.insert("cookie".into(), pending);
        let back = gate
            .route("GET", &format!("/api/auth/google/callback?code=ok&state={state}"), &cb)
            .unwrap();
        back.headers.iter().find(|(n, _)| *n == "Set-Cookie").unwrap().1
            .split(';').next().unwrap().to_string()
    }

    fn csrf_of(port: u16, cookie_header: &str) -> String {
        let (code, body) = http(port, "GET", "/api/session", cookie_header);
        assert_eq!(code, 200, "{body}");
        let start = body.find("\"csrf\":\"").expect("csrf field") + "\"csrf\":\"".len();
        body[start..].split('"').next().unwrap().to_string()
    }

    #[test]
    fn submission_accept_admits_the_originator() {
        // google:sub-1 is the editor; github:100 may upload but not edit.
        let gate = Gate::new(Oauth::test_ready(), Editors::parse("google:sub-1"));
        let (port, dir, gate) = serve_gate(gate, false);
        let editors_file = dir.path().join("admitted-editors.txt");
        gate.load_editors_file(&editors_file);

        let github = format!("Cookie: phext_session={}\r\nX-CSRF-Token: {TEST_CSRF}\r\n", Oauth::test_session_cookie());
        let (code, body) = upload(port, &github, "hand.phext", "jonas was here\n");
        assert_eq!(code, 200, "{body}");
        let id = submission_id(&body);

        let github_cookie_only = format!("Cookie: phext_session={}\r\n", Oauth::test_session_cookie());
        let (code, body) = http(port, "GET", "/api/session", &github_cookie_only);
        assert_eq!(code, 200);
        assert!(body.contains("\"editor\":false"), "{body}");
        // The originator cannot decide their own submission.
        let (code, _) = http(port, "POST", &format!("/api/submissions/{id}/accept"), &github);
        assert_eq!(code, 403);
        // …but can download it.
        let (code, body) = http(port, "GET", &format!("/api/submissions/{id}/download"), &github_cookie_only);
        assert_eq!(code, 200);
        assert!(body.contains("jonas was here"), "{body}");

        // An editor accepts: curated into the library, originator admitted.
        let google_cookie = google_session_cookie(&gate);
        let google = format!("Cookie: {google_cookie}\r\nX-CSRF-Token: {}\r\n", csrf_of(port, &format!("Cookie: {google_cookie}\r\n")));
        let (code, body) = http(port, "POST", &format!("/api/submissions/{id}/accept"), &google);
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"admitted\":true"), "{body}");
        assert_eq!(std::fs::read_to_string(dir.path().join("hand.phext")).unwrap(), "jonas was here\n");

        // Admission is live for the originator's existing session…
        let (code, body) = http(port, "GET", "/api/session", &github_cookie_only);
        assert_eq!(code, 200);
        assert!(body.contains("\"editor\":true"), "{body}");
        // …and persisted to the editors file for the next restart.
        let text = std::fs::read_to_string(&editors_file).unwrap();
        assert!(text.contains("github:100"), "{text}");
        assert!(text.contains("octo@example.com"), "{text}");
    }

    /// A session cookie for the scripted X identity (@jaivinwylde,
    /// placed in the wavefront snapshot), minted like the Google one.
    fn x_session_cookie(gate: &Gate) -> String {
        let plain: HashMap<String, String> = HashMap::new();
        let start = gate.route("GET", "/api/auth/x?next=/claim", &plain).unwrap();
        let location = start.headers.iter().find(|(n, _)| *n == "Location").unwrap().1.clone();
        assert!(location.starts_with("https://twitter.com/i/oauth2/authorize"), "{location}");
        let state = location.split("state=").nth(1).unwrap().split('&').next().unwrap();
        let pending = start.headers.iter().find(|(n, _)| *n == "Set-Cookie").unwrap().1
            .split(';').next().unwrap().to_string();
        let mut cb: HashMap<String, String> = HashMap::new();
        cb.insert("cookie".into(), pending);
        let back = gate
            .route("GET", &format!("/api/auth/x/callback?code=ok&state={state}"), &cb)
            .unwrap();
        back.headers.iter().find(|(n, _)| *n == "Set-Cookie").unwrap().1
            .split(';').next().unwrap().to_string()
    }

    #[test]
    fn claim_lookup_is_public_and_reads_the_snapshot() {
        // No sign-in configured at all: the lookup still answers, because
        // it reads only the checked-in snapshot.
        let (port, _dir) = serve(false);
        let (code, body) = http(port, "GET", "/api/claim/lookup?handle=%40eshear", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"found\":true"), "{body}");
        assert!(body.contains("\"handle\":\"@eshear\""), "{body}");
        assert!(body.contains("3.4.3/3.2.3/2.3.3"), "{body}");
        assert!(body.contains("\"tier\":\"found\""), "{body}");

        // Trails come back whole: both of Orai's placements, both Shoals.
        let (code, body) = http(port, "GET", "/api/claim/lookup?handle=oraiawake", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("4.3.2/3.2.2/2.5.2"), "{body}");
        assert!(body.contains("4.3.1/1.1.1/1.5.1"), "{body}");
        assert!(body.contains("\"label\":\"rupture\""), "{body}");

        let (code, body) = http(port, "GET", "/api/claim/lookup?handle=%40nobody_in_the_map_9", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"found\":false"), "{body}");
        assert!(body.contains("\"tier\":\"new\""), "{body}");

        // The page itself is public and explains the offer.
        let (code, body) = http(port, "GET", "/claim", "");
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("Claim your space"), "{body}");
        assert!(body.contains("Are you in the map?"), "{body}");
    }

    #[test]
    fn claim_writes_stay_closed_until_claim_mode_opens() {
        let gate = Gate::new(Oauth::test_ready(), Editors::default());
        let (port, _dir, gate) = serve_gate(gate, false);
        let x_cookie = x_session_cookie(&gate);
        let csrf = csrf_of(port, &format!("Cookie: {x_cookie}\r\n"));
        let x = format!("Cookie: {x_cookie}\r\nX-CSRF-Token: {csrf}\r\n");
        let (code, body) = http(port, "POST", "/api/claim", &x);
        assert_eq!(code, 403, "{body}");
        assert!(body.contains("not open yet"), "{body}");
        // The lookup is unaffected by the flag.
        let (code, _) = http(port, "GET", "/api/claim/lookup?handle=eshear", "");
        assert_eq!(code, 200);
    }

    #[test]
    fn claim_flow_records_a_found_claim_for_an_x_sign_in() {
        let gate = Gate::new(Oauth::test_ready(), Editors::default());
        gate.enable_claims();
        let (port, dir, gate) = serve_gate(gate, false);
        let x_cookie = x_session_cookie(&gate);
        let cookie_only = format!("Cookie: {x_cookie}\r\n");
        let csrf = csrf_of(port, &cookie_only);
        let x = format!("Cookie: {x_cookie}\r\nX-CSRF-Token: {csrf}\r\n");

        // The session really is the X account, handle underscores intact.
        let (code, body) = http(port, "GET", "/api/session", &cookie_only);
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"who\":\"x:1001\""), "{body}");
        assert!(body.contains("\"handle\":\"jaivinwylde\""), "{body}");

        let (code, body) = http(port, "GET", "/api/claim", &cookie_only);
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"claimed\":false"), "{body}");

        // Claiming matches the snapshot: found tier, full grant, the
        // placement the map holds for @jaivinwylde.
        let (code, body) = http(port, "POST", "/api/claim", &x);
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"tier\":\"found\""), "{body}");
        assert!(body.contains("\"handle\":\"@jaivinwylde\""), "{body}");
        assert!(body.contains("3.3.3/3.4.3/2.2.2"), "{body}");
        assert!(body.contains("\"storage_bytes\":104857600"), "{body}");
        assert!(dir.path().join("claims").join("x-1001.json").is_file());

        // Claiming again changes nothing.
        let (code, again) = http(port, "POST", "/api/claim", &x);
        assert_eq!(code, 200, "{again}");
        assert_eq!(again, body);

        // A non-X sign-in claims too, at the new tier: no map handle.
        let github = format!("Cookie: phext_session={}\r\nX-CSRF-Token: {TEST_CSRF}\r\n", Oauth::test_session_cookie());
        let (code, body) = http(port, "POST", "/api/claim", &github);
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"tier\":\"new\""), "{body}");
        assert!(body.contains("\"storage_bytes\":1048576"), "{body}");
    }

    /// A session cookie for a scripted X identity chosen by code
    /// ("ok-new": an account the snapshot does not hold; "ok-owner":
    /// @wbic16 himself), minted like `x_session_cookie`.
    fn x_session_cookie_for(gate: &Gate, code: &str) -> String {
        let plain: HashMap<String, String> = HashMap::new();
        let start = gate.route("GET", "/api/auth/x?next=/claim", &plain).unwrap();
        let location = start.headers.iter().find(|(n, _)| *n == "Location").unwrap().1.clone();
        let state = location.split("state=").nth(1).unwrap().split('&').next().unwrap();
        let pending = start.headers.iter().find(|(n, _)| *n == "Set-Cookie").unwrap().1
            .split(';').next().unwrap().to_string();
        let mut cb: HashMap<String, String> = HashMap::new();
        cb.insert("cookie".into(), pending);
        let back = gate
            .route("GET", &format!("/api/auth/x/callback?code={code}&state={state}"), &cb)
            .unwrap();
        back.headers.iter().find(|(n, _)| *n == "Set-Cookie").unwrap().1
            .split(';').next().unwrap().to_string()
    }

    #[test]
    fn claim_dashboard_is_gated_to_the_owner_and_editors() {
        // Only google:sub-1 is an editor on this gate.
        let gate = Gate::new(Oauth::test_ready(), Editors::parse("google:sub-1"));
        let (port, _dir, gate) = serve_gate(gate, false);

        // Signed out: the app's usual 401.
        let (code, _) = http(port, "GET", "/claim/dashboard", "");
        assert_eq!(code, 401);
        // Signed in but neither owner nor editor: the editor list's 403.
        let github = format!("Cookie: phext_session={}\r\n", Oauth::test_session_cookie());
        let (code, body) = http(port, "GET", "/claim/dashboard", &github);
        assert_eq!(code, 403, "{body}");
        assert!(body.contains("not on the editor list"), "{body}");
        // An editor sees it, with the whole snapshot still unclaimed.
        let google_cookie = google_session_cookie(&gate);
        let google = format!("Cookie: {google_cookie}\r\n");
        let (code, body) = http(port, "GET", "/claim/dashboard", &google);
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("Claim dashboard"), "{body}");
        assert!(body.contains("of the 521 handles in the wavefront"), "{body}");
        assert!(body.contains("521 have not yet claimed"), "{body}");
        // The owner sees it on his X identity alone: @wbic16 is not on
        // this gate's editors list, and the dashboard still opens.
        let owner_cookie = x_session_cookie_for(&gate, "ok-owner");
        let owner = format!("Cookie: {owner_cookie}\r\n");
        let (code, body) = http(port, "GET", "/claim/dashboard", &owner);
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("Claim dashboard"), "{body}");
    }

    #[test]
    fn claim_dashboard_lists_claims_and_joins() {
        let gate = Gate::new(Oauth::test_ready(), Editors::parse("google:sub-1"));
        gate.enable_claims();
        let (port, dir, gate) = serve_gate(gate, false);

        // A wavefront handle claims (found tier): a claim, not a join.
        let x_cookie = x_session_cookie(&gate);
        let x_csrf = csrf_of(port, &format!("Cookie: {x_cookie}\r\n"));
        let x = format!("Cookie: {x_cookie}\r\nX-CSRF-Token: {x_csrf}\r\n");
        let (code, body) = http(port, "POST", "/api/claim", &x);
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"tier\":\"found\""), "{body}");
        assert!(!dir.path().join("joins").join("x-1001.json").exists());

        // A non-X claim lands at the new tier: also a join, via claim.
        let github = format!("Cookie: phext_session={}\r\nX-CSRF-Token: {TEST_CSRF}\r\n", Oauth::test_session_cookie());
        let (code, body) = http(port, "POST", "/api/claim", &github);
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"tier\":\"new\""), "{body}");
        assert!(dir.path().join("joins").join("github-100.json").is_file());

        // An X sign-in the snapshot does not hold joins at sign-in. The
        // flow is driven over HTTP so the completion hook in
        // handle_request is what records the join.
        let (code, start) = http(port, "GET", "/api/auth/x?next=/claim", "");
        assert_eq!(code, 302, "{start}");
        let state = start.split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();
        let pending = start.split("phext_oauth=").nth(1).unwrap().split(';').next().unwrap().to_string();
        let callback = format!("Cookie: phext_oauth={pending}\r\n");
        let (code, _) = http(port, "GET", &format!("/api/auth/x/callback?code=ok-new&state={state}"), &callback);
        assert_eq!(code, 302);
        assert!(dir.path().join("joins").join("x-2002.json").is_file());

        // The dashboard shows all of it: the claim under Claimed, both
        // joins under Joined, and the unclaimed count down by one.
        let google_cookie = google_session_cookie(&gate);
        let google = format!("Cookie: {google_cookie}\r\n");
        let (code, body) = http(port, "GET", "/claim/dashboard", &google);
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("@jaivinwylde"), "{body}");
        assert!(body.contains("520 have not yet claimed"), "{body}");
        assert!(body.contains("@newarrival_x"), "{body}");
        assert!(body.contains("github:100"), "{body}");
        assert!(body.contains("new-era join"), "{body}");
    }

    /// Drive a full sign-in over HTTP — start, then callback — and
    /// return the session cookie it seals. Unlike the gate.route
    /// helpers above, this passes through handle_request, so the
    /// sign-in completion hook (promotion, joins) is what runs.
    fn http_sign_in_cookie(port: u16, provider: &str, code: &str) -> String {
        let (status, start) = http(port, "GET", &format!("/api/auth/{provider}?next=/claim"), "");
        assert_eq!(status, 302, "{start}");
        let state = start.split("state=").nth(1).unwrap().split('&').next().unwrap().to_string();
        let pending = start.split("phext_oauth=").nth(1).unwrap().split(';').next().unwrap().to_string();
        let callback = format!("Cookie: phext_oauth={pending}\r\n");
        let (status, back) = http(port, "GET", &format!("/api/auth/{provider}/callback?code={code}&state={state}"), &callback);
        assert_eq!(status, 302, "{back}");
        let session = back.split("phext_session=").nth(1).unwrap().split(';').next().unwrap().to_string();
        format!("phext_session={session}")
    }

    #[test]
    fn x_sign_in_grants_editor_and_persists_google_does_not() {
        // x:1001 (@jaivinwylde) is the only editor on this gate.
        let gate = Gate::new(Oauth::test_ready(), Editors::parse("x:1001"));
        let (port, dir, gate) = serve_gate(gate, false);
        let editors_file = dir.path().join("admitted-editors.txt");
        gate.load_editors_file(&editors_file);

        // A Google sign-in is not promoted (the ruling names X only):
        // the account signs in, stays off the list, and the editors
        // file gains nothing.
        let google_cookie = http_sign_in_cookie(port, "google", "ok");
        let (code, body) = http(port, "GET", "/api/session", &format!("Cookie: {google_cookie}\r\n"));
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"editor\":false"), "{body}");
        assert!(!editors_file.exists());

        // An X sign-in is promoted on completion: editor access is
        // live for the session the callback just sealed…
        let x_cookie = http_sign_in_cookie(port, "x", "ok-new");
        let (code, body) = http(port, "GET", "/api/session", &format!("Cookie: {x_cookie}\r\n"));
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"editor\":true"), "{body}");
        // …and the admission is persisted to the editors file.
        let text = std::fs::read_to_string(&editors_file).unwrap();
        assert!(text.contains("x:2002"), "{text}");
    }

    #[test]
    fn x_sign_in_already_editor_is_not_duplicated() {
        let gate = Gate::new(Oauth::test_ready(), Editors::parse("x:1001"));
        let (port, dir, gate) = serve_gate(gate, false);
        let editors_file = dir.path().join("admitted-editors.txt");
        // x:2002 is already an editor, carried by the backing file.
        std::fs::write(&editors_file, "x:2002\n").unwrap();
        gate.load_editors_file(&editors_file);

        let x_cookie = http_sign_in_cookie(port, "x", "ok-new");
        let (code, body) = http(port, "GET", "/api/session", &format!("Cookie: {x_cookie}\r\n"));
        assert_eq!(code, 200, "{body}");
        assert!(body.contains("\"editor\":true"), "{body}");
        // No second entry is appended for the account.
        let text = std::fs::read_to_string(&editors_file).unwrap();
        assert_eq!(text.matches("x:2002").count(), 1, "{text}");
    }

    #[test]
    fn dashboard_time_format_is_utc() {
        assert_eq!(fmt_unix_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(fmt_unix_utc(1_735_689_600), "2025-01-01 00:00 UTC");
        assert_eq!(fmt_unix_utc(1_750_000_000), "2025-06-15 15:06 UTC");
    }

    #[test]
    fn local_startup_creates_a_missing_data_dir_with_spaces() {
        let parent = tempfile::tempdir().unwrap();
        let cwd = parent.path().join("work dir");
        std::fs::create_dir_all(&cwd).unwrap();
        let custom = parent.path().join("does not exist").join("phext data");
        assert!(!custom.exists(), "{}", custom.display());

        let (dir, file) = local_startup_paths(None, &cwd).unwrap();
        assert_eq!(dir, cwd.join(".phext-local"));
        assert_eq!(file, dir.join("welcome.phext"));
        assert!(file.is_file(), "{}", file.display());
        let opened = MappedLattice::open(&file).unwrap();
        assert!(opened.scroll_count() >= 1);

        let custom_str = custom.to_str().expect("temp path is unicode");
        let (dir, file) = local_startup_paths(Some(custom_str), &cwd).unwrap();
        assert_eq!(dir, custom);
        assert_eq!(file.file_name().and_then(|name| name.to_str()), Some("welcome.phext"));
        assert!(file.is_file(), "{}", file.display());
        MappedLattice::open(&file).unwrap();

        // An existing .PHEXT is enough; do not add a second starter.
        let upper = parent.path().join("upper case dir");
        std::fs::create_dir_all(&upper).unwrap();
        std::fs::write(upper.join("Notes.PHEXT"), b"kept\n").unwrap();
        let file = ensure_local_data_dir(&upper).unwrap();
        assert_eq!(file.file_name().and_then(|name| name.to_str()), Some("Notes.PHEXT"));
        assert!(!upper.join("welcome.phext").exists());

        let missing = parent.path().join("no such data");
        assert!(!missing.exists());
        let err = list_phext_files(&missing).unwrap_err();
        assert!(err.contains("read directory"), "{err}");
        assert!(err.contains(&missing.display().to_string()), "{err}");

        let blocker = parent.path().join("blocked file");
        std::fs::write(&blocker, b"x").unwrap();
        let nested = blocker.join("child dir");
        let err = local_startup_paths(Some(nested.to_str().unwrap()), &cwd).unwrap_err();
        assert!(err.contains("create directory"), "{err}");
        assert!(err.contains(&nested.display().to_string()), "{err}");
    }

    #[test]
    fn positional_file_skips_local_and_valued_flags() {
        assert_eq!(positional_file(&argv(&["--local"])), None);
        assert_eq!(positional_file(&argv(&["--local", "notes.phext"])).as_deref(), Some("notes.phext"));
        assert_eq!(positional_file(&argv(&["--port", "9", "notes.phext"])).as_deref(), Some("notes.phext"));
        assert_eq!(positional_file(&argv(&["notes.phext", "--local"])).as_deref(), Some("notes.phext"));
    }

    #[test]
    fn local_serves_share_assets_and_editor_metadata() {
        let (port, _dir) = serve(true);
        let (code, page) = http(port, "GET", "/", "");
        assert_eq!(code, 200, "{page}");
        assert!(page.contains("property=\"og:title\""));
        assert!(page.contains("property=\"og:description\""));
        assert!(page.contains("property=\"og:image\""));
        assert!(page.contains("name=\"twitter:card\" content=\"summary_large_image\""));
        assert!(page.contains("rel=\"canonical\" href=\"https://edit.phext.io/\""));
        assert!(page.contains("name=\"theme-color\" content=\"#171717\""));
        assert!(page.contains("img-src 'self'"));
        assert!(!page.to_ascii_lowercase().contains("access-control-allow-origin"));
        for (path, content_type) in [
            ("/og.png", "image/png"),
            ("/favicon.ico", "image/x-icon"),
            ("/favicon.svg", "image/svg+xml"),
            ("/apple-touch-icon.png", "image/png"),
        ] {
            let (code, body) = http(port, "GET", path, "");
            assert_eq!(code, 200, "{path} {body}");
            assert!(body.contains(&format!("Content-Type: {content_type}\r\n")), "{path} {body}");
            assert!(body.contains(&format!("Cache-Control: {STATIC_CACHE}\r\n")), "{path}");
            assert!(body.contains("ETag: \""), "{path}");
            assert!(!body.to_ascii_lowercase().contains("access-control-allow-origin"), "{path}");
        }
        let (code, robots) = http(port, "GET", "/robots.txt", "");
        assert_eq!(code, 200, "{robots}");
        assert!(robots.contains("Sitemap: https://edit.phext.io/sitemap.xml"));
        let (code, sitemap) = http(port, "GET", "/sitemap.xml", "");
        assert_eq!(code, 200, "{sitemap}");
        assert!(sitemap.contains("https://edit.phext.io/terms"));
        let (code, nav) = http(port, "GET", "/api/nav", "");
        assert_eq!(code, 200, "{nav}");
    }

    fn open_phext(port: u16, path: &std::path::Path, headers: &str) -> (u16, String) {
        http_with_body(port, "POST", "/api/open", headers, &path.to_string_lossy())
    }

    /// The editor switches files with `POST /api/open` (path as text) and then
    /// reads `GET /api/nav`. Both the open file and the scroll must change.
    fn assert_switch_changes_scroll(port: u16, dir: &std::path::Path, headers: &str) {
        let note = dir.join("note.phext");
        let other = dir.join("other.phext");
        std::fs::write(&other, b"other scroll\n").unwrap();

        let (code, status) = http(port, "GET", "/api/status", headers);
        assert_eq!(code, 200, "{status}");
        let status_body = response_body(&status);
        assert!(status_body.contains("note.phext"), "{status_body}");
        let (code, nav) = http(port, "GET", "/api/nav", headers);
        assert_eq!(code, 200, "{nav}");
        let nav_body = response_body(&nav);
        assert!(nav_body.contains("local note"), "{nav_body}");
        assert!(nav_body.contains(&note.to_string_lossy().to_string()), "{nav_body}");

        let (code, opened) = open_phext(port, &other, headers);
        assert_eq!(code, 200, "{opened}");
        assert!(response_body(&opened).contains("\"ok\":true"), "{opened}");
        assert!(response_body(&opened).contains("other.phext"), "{opened}");

        let (code, status) = http(port, "GET", "/api/status", headers);
        assert_eq!(code, 200, "{status}");
        let status_body = response_body(&status);
        assert!(status_body.contains("other.phext"), "{status_body}");
        // The startup file stays named as default_file (#12); the open
        // file is the one the status `file` field names.
        assert!(status_body.contains(&format!("\"file\":\"{}\"", other.to_string_lossy())), "{status_body}");
        assert!(!status_body.contains(&format!("\"file\":\"{}\"", note.to_string_lossy())), "{status_body}");
        let (code, nav) = http(port, "GET", "/api/nav", headers);
        assert_eq!(code, 200, "{nav}");
        let nav_body = response_body(&nav);
        assert!(nav_body.contains("other scroll"), "{nav_body}");
        assert!(!nav_body.contains("local note"), "{nav_body}");
        assert!(nav_body.contains(&other.to_string_lossy().to_string()), "{nav_body}");

        let (code, opened) = open_phext(port, &note, headers);
        assert_eq!(code, 200, "{opened}");
        let (code, nav) = http(port, "GET", "/api/nav", headers);
        assert_eq!(code, 200, "{nav}");
        assert!(response_body(&nav).contains("local note"), "{nav}");
    }

    #[test]
    fn switching_phexts_changes_nav_when_local_and_when_signed_in() {
        assert!(INDEX_HTML.contains("p.file"));
        assert!(INDEX_HTML.contains("await initApp();"));
        assert!(INDEX_HTML.contains("lastNav.position?.file"));

        let (port, dir) = serve(true);
        assert_switch_changes_scroll(port, dir.path(), "");

        let (port, dir) = serve(false);
        let other = dir.path().join("other.phext");
        std::fs::write(&other, b"other scroll\n").unwrap();
        let (code, denied) = open_phext(port, &other, "");
        assert_eq!(code, 401, "{denied}");
        let cookie = Oauth::test_session_cookie();
        let session = format!("Cookie: phext_session={cookie}\r\nx-csrf-token: {TEST_CSRF}\r\n");
        let (code, status) = http(port, "GET", "/api/status", &session);
        assert_eq!(code, 200, "{status}");
        assert!(response_body(&status).contains("note.phext"), "{status}");
        assert_switch_changes_scroll(port, dir.path(), &session);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("phext-edit — web-served phext editor");
        eprintln!();
        eprintln!("Usage: phext-edit <file.phext> [options]");
        eprintln!("       phext-edit --dir <directory> [options]");
        eprintln!("       phext-edit --local [options]");
        eprintln!();
        eprintln!("Links: /#1.1.1/1.1.1/1.1.1");
        eprintln!("       /<name>/#1.1.1/1.1.1/1.1.1  (<name>.phext in --dir)");
        eprintln!();
        eprintln!("Options:");
        eprintln!("  --port <port>     HTTP port (default: 8080)");
        eprintln!("  --host <addr>     Listen address (default: 0.0.0.0; 127.0.0.1 with --local)");
        eprintln!("  --dir <path>      Serve all .phext files in directory");
        eprintln!("  --local           Local dev: no sign-in. Loopback bind only.");
        eprintln!("                    Phexts: --dir, else PHEXT_DATA_DIR, else ./.phext-local");
        eprintln!("  --help            Show this help");
        eprintln!();
        eprintln!("GitHub/Google/X sign-in (environment):");
        eprintln!("  PHEXT_PUBLIC_URL, PHEXT_SESSION_SECRET, PHEXT_EDITORS, PHEXT_EDITORS_FILE, PHEXT_COOKIE_SECURE=1");
        eprintln!("  GITHUB_CLIENT_ID/SECRET, GOOGLE_CLIENT_ID/SECRET, X_CLIENT_ID/SECRET");
        eprintln!("  PHEXT_CLAIM_ENABLED=1 opens claim mode (/api/claim; the /claim lookup is always public)");
        eprintln!("  PHEXT_SPONSOR_URL   Stripe Payment Link for the Support the site donation. Empty hides it.");
        std::process::exit(1);
    }

    let local = args.iter().any(|a| a == "--local");
    let port: u16 = get_flag(&args, "--port").and_then(|p| p.parse().ok()).unwrap_or(8080);
    let host = if local {
        match resolve_local_host(get_flag(&args, "--host").as_deref()) {
            Ok(host) => host,
            Err(message) => {
                eprintln!("error: {message}");
                std::process::exit(1);
            }
        }
    } else {
        get_flag(&args, "--host").unwrap_or_else(|| "0.0.0.0".to_string())
    };
    let mut dir = get_flag(&args, "--dir");
    // `--local` does not inherit PHEXT_HOME: that variable points at the deployed library.
    let mut home_file = get_flag(&args, "--home")
        .or_else(|| if local { None } else { std::env::var("PHEXT_HOME").ok() })
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .map(|h| {
            let path = match (&dir, Path::new(&h).is_absolute()) {
                (Some(d), false) => Path::new(d).join(&h),
                _ => PathBuf::from(&h),
            };
            make_absolute(path).to_string_lossy().to_string()
        })
        .filter(|h| {
            let ok = Path::new(h).is_file();
            if !ok { eprintln!("error: home file not found: {h}"); }
            ok
        });
    let home_coord = get_flag(&args, "--home-coord")
        .or_else(|| std::env::var("PHEXT_HOME_COORD").ok())
        .and_then(|c| {
            let parsed = parse_coordinate(c.trim());
            if parsed.is_none() { eprintln!("Invalid --home-coord {}; ignoring", c); }
            parsed
        });

    let positional = positional_file(&args);
    let mut startup_file: Option<PathBuf> = None;
    // A missing --home does not block the default data directory.
    if local && dir.is_none() && positional.is_none() && home_file.is_none() {
        let cwd = match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(err) => {
                eprintln!("error: read current directory: {err}");
                std::process::exit(1);
            }
        };
        match local_startup_paths(std::env::var("PHEXT_DATA_DIR").ok().as_deref(), &cwd) {
            Ok((data, file)) => {
                dir = Some(data.display().to_string());
                home_file = None;
                startup_file = Some(file);
            }
            Err(message) => {
                eprintln!("error: {message}");
                std::process::exit(1);
            }
        }
    }

    // Determine initial file: the home file if set, else the first .phext by name.
    // Open the PathBuf directly so a non-Unicode Windows path is not lossily
    // round-tripped through String before the first open.
    let file_path_buf = if let Some(file) = startup_file {
        file
    } else if let Some(ref h) = home_file {
        PathBuf::from(h)
    } else if let Some(ref d) = dir {
        let dir_path = make_absolute(PathBuf::from(d));
        match list_phext_files(&dir_path) {
            Err(message) => {
                eprintln!("error: {message}");
                std::process::exit(1);
            }
            Ok(found) => match found.into_iter().next() {
                Some(path) => path,
                None => {
                    eprintln!("error: no .phext files in {}", dir_path.display());
                    std::process::exit(1);
                }
            },
        }
    } else if local {
        match positional {
            Some(file) => make_absolute(PathBuf::from(file)),
            None => {
                eprintln!("error: --local needs a .phext file or --dir.");
                std::process::exit(1);
            }
        }
    } else {
        make_absolute(PathBuf::from(&args[1]))
    };

    let lattice = MappedLattice::open(&file_path_buf).unwrap_or_else(|e| {
        eprintln!("error: {e}");
        std::process::exit(1);
    });
    let file_path = file_path_buf.to_string_lossy().to_string();

    let mut app_state = AppState::new(lattice, file_path.clone());
    app_state.phext_dir = dir.clone();
    app_state.home_file = home_file.clone();
    app_state.home_coord = home_coord;
    if let Err(e) = app_state.go_home() { eprintln!("home: {}", e); }

    let state = Arc::new(Mutex::new(app_state));
    let gate = Arc::new(Gate::from_env());
    // Admissions (an editor accepting a submission) persist to an editors
    // file. PHEXT_EDITORS_FILE names it when set — Gate::from_env already
    // loaded that one — otherwise it lives next to the phexts.
    if std::env::var("PHEXT_EDITORS_FILE").map(|v| v.trim().is_empty()).unwrap_or(true) {
        let editors_file = match &dir {
            Some(d) => std::path::Path::new(d).join("admitted-editors.txt"),
            None => std::path::Path::new(&file_path)
                .parent()
                .map(|p| p.join("admitted-editors.txt"))
                .unwrap_or_else(|| std::path::PathBuf::from("admitted-editors.txt")),
        };
        gate.load_editors_file(&editors_file);
        println!("   admitted editors: {}", editors_file.display());
    }
    let demo = Arc::new(DemoCatalog::load());
    let listener = TcpListener::bind(format!("{}:{}", host, port)).unwrap_or_else(|e| {
        eprintln!("Failed to bind {}:{}: {}", host, port, e);
        std::process::exit(1);
    });

    if local {
        match listener.local_addr() {
            Ok(addr) if addr.ip().is_loopback() => {}
            Ok(addr) => {
                eprintln!("error: --local refuses to start: bound address {addr} is not loopback.");
                std::process::exit(1);
            }
            Err(err) => {
                eprintln!("error: --local refuses to start: {err}");
                std::process::exit(1);
            }
        }
        gate.enable_local();
        let phexts = dir.as_deref().unwrap_or(file_path.as_str());
        eprintln!();
        eprintln!("****************************************************************");
        eprintln!("WARNING: --local is set. Authentication is DISABLED.");
        eprintln!("         Every direct request is admitted as a local developer.");
        eprintln!("         Listening on {host}:{port} (loopback only).");
        eprintln!("         Phexts: {phexts}");
        eprintln!("         Do not put this process behind a reverse proxy.");
        eprintln!("****************************************************************");
        eprintln!();
    }

    let scroll_count = state.lock().unwrap().overview.total_scrolls;
    let phext_count = state.lock().unwrap().list_phexts().len();

    println!("💎 phext-edit on http://{}:{}", host, port);
    if let Some(ref d) = dir {
        println!("   directory: {} ({} phext files)", d, phext_count);
    }
    println!("   active: {} ({} scrolls)", file_path, scroll_count);
    for line in gate.describe() {
        println!("   {}", line);
    }
    println!("   demo: {} scrolls, read-only, http://localhost:{}/?demo", demo.scrolls, port);
    if sponsor_offer().url().is_some() {
        println!("   sponsor: Support the site donation link is on");
    } else {
        println!("   sponsor: hidden (set PHEXT_SPONSOR_URL to an https Payment Link)");
    }
    println!("   Open in browser: http://localhost:{}", port);

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let state = Arc::clone(&state);
                let gate = Arc::clone(&gate);
                let demo = Arc::clone(&demo);
                std::thread::spawn(move || { handle_request(&mut stream, &state, &gate, &demo); });
            }
            Err(e) => eprintln!("connection error: {}", e),
        }
    }
}

const FAVICON_ICO: &[u8] = include_bytes!("../../web/favicon.ico");
const FAVICON_SVG: &[u8] = include_bytes!("../../web/favicon.svg");
const APPLE_TOUCH_PNG: &[u8] = include_bytes!("../../web/apple-touch-icon.png");
const OG_PNG: &[u8] = include_bytes!("../../web/og.png");

/// Public icon and share-image routes. `path` may include a query string.
/// Anything else (including `/robots.txt`, `/sitemap.xml`) is left for other routes.
fn public_asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    match path_route(path) {
        "/favicon.ico" => Some(("image/x-icon", FAVICON_ICO)),
        "/favicon.svg" => Some(("image/svg+xml", FAVICON_SVG)),
        "/apple-touch-icon.png" | "/apple-touch-icon-precomposed.png" => {
            Some(("image/png", APPLE_TOUCH_PNG))
        }
        "/og.png" => Some(("image/png", OG_PNG)),
        "/banner.webp" => Some(("image/webp", BANNER_WEBP)),
        "/background.webp" => Some(("image/webp", BACKGROUND_WEBP)),
        _ => None,
    }
}

// ── Public pages (no sign-in) ──────────────────────────────────────────

// Shared by the public pages (concat! needs a literal, hence a macro).
macro_rules! public_style { () => { r#"<style>
body { margin: 0; background: #171717; color: #dedede; font: 15px/1.6 system-ui, sans-serif; }
main { max-width: 720px; margin: 0 auto; padding: 32px 16px 48px; }
h1 { color: #d4a855; font-size: 28px; margin: 0 0 8px; }
h2 { color: #d4a855; font-size: 18px; margin: 28px 0 8px; }
a { color: #4dc9c9; }
nav { margin-bottom: 24px; font-size: 14px; }
.dim { color: #9a9a9a; }
footer { margin-top: 40px; font-size: 13px; color: #9a9a9a; border-top: 1px solid #2e2e2e; padding-top: 16px; }
/* Mobile and accessibility for the public pages. Kept in this block so it
   stays clear of the page copy. The dark background is the product theme. */
.skip-link {
  position: absolute; left: 8px; top: 0; transform: translateY(-120%);
  background: #e8c547; color: #000; font-weight: bold; text-decoration: none;
  padding: 8px 12px; border-radius: 4px; z-index: 5;
}
.skip-link:focus { transform: none; top: 8px; }
:focus-visible { outline: 2px solid #e8c547; outline-offset: 3px; }
main a, main code { overflow-wrap: anywhere; }
code { overflow-wrap: anywhere; }
@media (max-width: 760px) {
  main { padding: 16px 16px 40px; }
  nav a { display: inline-block; padding: 12px 8px; min-height: 44px; }
  h1 { font-size: 26px; }
  table { display: block; overflow-x: auto; }
}
@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after {
    animation-duration: 0.01ms !important; animation-iteration-count: 1 !important;
    transition-duration: 0.01ms !important; scroll-behavior: auto !important;
  }
}
</style>"# } }

// ── Claim dashboard (the owner's page) ─────────────────────────────────

/// Who may see the claim dashboard: the owner — the X account @wbic16 —
/// and any editor, following how the app already recognizes editors.
/// The owner's X identity is named explicitly because owning the map
/// does not depend on sitting on the editors list.
fn dashboard_owner(gate: &Gate, session: &Session) -> bool {
    gate.is_editor(session)
        || (session.who.starts_with("x:") && session.handle.eq_ignore_ascii_case("wbic16"))
}

fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// "2025-06-15 15:06 UTC" from unix seconds, without a date library
/// (civil-from-days, after Howard Hinnant's algorithm).
fn fmt_unix_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm) = (rem / 3600, (rem % 3600) / 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    if month <= 2 {
        year += 1;
    }
    format!("{year:04}-{month:02}-{day:02} {hh:02}:{mm:02} UTC")
}

fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::Found => "found",
        Tier::Bought => "bought",
        Tier::Patron => "patron",
        Tier::New => "new",
    }
}

fn dashboard_html(claimed: &[&claims::Claim], total: usize, unclaimed: usize, joins: &[joins::Join]) -> String {
    let mut out = String::new();
    out.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str("<meta name=\"description\" content=\"The owner's view of claim mode: which wavefront handles have claimed their space, and which accounts have joined the wavefront since the snapshot.\">\n");
    out.push_str("<title>Phext Edit: claim dashboard</title>\n");
    out.push_str("<link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\">\n");
    out.push_str(public_style!());
    // The dashboard wears the shell's artwork like the sign-in screen:
    // the lattice-vault backdrop fills the window, the text column
    // keeps the public pages' solid #171717 so no word sits on art,
    // and the CYOA banner heads the column.
    out.push_str("\n<style>\ntable { border-collapse: collapse; margin: 10px 0; }\ntd, th { border: 1px solid #4a4a4a; padding: 4px 10px; text-align: left; }\nbody { background: #0b0e13 url(\"/background.webp\") center / cover fixed no-repeat; }\nmain { background: #171717; }\n.dash-banner { display: block; width: 100%; height: 150px; object-fit: cover; object-position: center; border: 1px solid #2e2e2e; border-radius: 8px; margin-bottom: 20px; }\n</style>\n</head>\n<body>\n<main>\n");
    out.push_str("<img class=\"dash-banner\" src=\"/banner.webp\" alt=\"The Flame at the root coordinate, its light running out along the filaments to a vast wall of scrolls — the phext lattice\">\n");
    out.push_str("<nav><a href=\"/\">Open the editor</a> · <a href=\"/claim\">Claim your space</a> · <a href=\"/explore\">Explore the map</a></nav>\n");
    out.push_str("<h1>💎 Claim dashboard</h1>\n");
    out.push_str("<p class=\"dim\">The owner's view: who has claimed their space from the wavefront, and who has joined it since. Visible only to @wbic16 and the editors.</p>\n");
    out.push_str("<h2>Claimed</h2>\n");
    out.push_str(&format!(
        "<p>{} of the {} handles in the wavefront have claimed their space; {} have not yet claimed.</p>\n",
        claimed.len(),
        total,
        unclaimed
    ));
    if claimed.is_empty() {
        out.push_str("<p class=\"dim\">No wavefront handle has claimed yet.</p>\n");
    } else {
        out.push_str("<table>\n<tr><th>Handle</th><th>Account</th><th>Tier</th><th>Claimed</th></tr>\n");
        for claim in claimed {
            out.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                html_escape(&claim.handle),
                html_escape(&claim.who),
                tier_name(claim.tier),
                fmt_unix_utc(claim.claimed_at),
            ));
        }
        out.push_str("</table>\n");
    }
    out.push_str("<h2>Joined the wavefront</h2>\n");
    out.push_str("<p>The wavefront snapshot ends with the archive (June 2025); every join recorded here is a new-era join — an account the map had not placed, arriving in the AGI era.</p>\n");
    if joins.is_empty() {
        out.push_str("<p class=\"dim\">No new-era joins recorded yet.</p>\n");
    } else {
        out.push_str("<table>\n<tr><th>Handle</th><th>Account</th><th>Joined via</th><th>Joined</th></tr>\n");
        for join in joins {
            out.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                html_escape(if join.handle.is_empty() { "—" } else { &join.handle }),
                html_escape(&join.who),
                html_escape(&join.via),
                fmt_unix_utc(join.joined_at),
            ));
        }
        out.push_str("</table>\n");
    }
    out.push_str("<footer>Phext Edit · <a href=\"/about\">About</a> · <a href=\"/privacy\">Privacy Policy</a> · <a href=\"/terms\">Terms of Service</a> · Contact: <a href=\"mailto:will+support@phext.io\">will+support@phext.io</a><!--phext-sponsor--></footer>\n");
    out.push_str("</main>\n</body>\n</html>\n");
    out
}

/// Route for `/claim/dashboard`. Returns true when the request was
/// answered here. The gate is the owner's — the X account @wbic16, or
/// any editor — and everyone else gets the same answers the app gives
/// for its other gated pages: 401 signed out, and the editor list's
/// own 403 wording signed in.
fn handle_dashboard(
    stream: &mut std::net::TcpStream,
    method: &str,
    path: &str,
    headers: &HashMap<String, String>,
    state: &Arc<Mutex<AppState>>,
    gate: &Gate,
) -> bool {
    if path_route(path) != "/claim/dashboard" {
        return false;
    }
    if method != "GET" && method != "HEAD" {
        json_error(stream, headers, 405, "method not allowed");
        return true;
    }
    let Some(session) = gate.session(headers) else {
        json_error(stream, headers, 401, "sign in to view the claim dashboard");
        return true;
    };
    if !dashboard_owner(gate, &session) {
        json_error(stream, headers, 403, "This account is not on the editor list.");
        return true;
    }
    let claims = claims::list(&claims_dir(state));
    let joins = joins::list(&joins_dir(state));
    let total = wavefront::snapshot_handles();
    // Claimed: the claims matched to a handle the snapshot holds. An X
    // sign-in from the map claims under its snapshot handle; a claim
    // with no handle — or a handle the map does not hold — is not a
    // wavefront claim, whatever tier it carries.
    let claimed: Vec<&claims::Claim> = claims
        .iter()
        .filter(|claim| !claim.handle.is_empty() && wavefront::lookup(&claim.handle).is_some())
        .collect();
    let claimed_handles: std::collections::HashSet<String> = claimed
        .iter()
        .filter_map(|claim| wavefront::normalize_handle(&claim.handle))
        .collect();
    let unclaimed = total - claimed_handles.len().min(total);
    let dash = dashboard_html(&claimed, total, unclaimed, &joins);
    let html = render_with_sponsor(&dash);
    send_response(stream, 200, HTML, html.as_bytes(), CacheClass::NoStore, headers, method == "HEAD");
    true
}

const CLAIM_HTML: &str = concat!(r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="description" content="Claim your space on this server. If your X account is placed in Will Bickford's wavefront map, a significant space is already yours — claim it by signing in with that account.">
<title>Phext Edit: claim your space</title>
<link rel="icon" type="image/svg+xml" href="favicon.svg">
"##, public_style!(), r##"
<style>
button { background: #2e2e2e; color: #dedede; border: 1px solid #4a4a4a; border-radius: 4px; padding: 2px 10px; cursor: pointer; font: inherit; }
button:disabled { opacity: 0.5; cursor: default; }
input[type="text"] { font: inherit; color: #dedede; background: #1c1c1c; border: 1px solid #4a4a4a; border-radius: 4px; padding: 2px 8px; }
table { border-collapse: collapse; margin: 10px 0; }
td, th { border: 1px solid #4a4a4a; padding: 4px 10px; text-align: left; }
.video-slot { border: 2px dashed #4a4a4a; border-radius: 8px; padding: 26px 18px; text-align: center; color: #b9c0c8; margin: 12px 0; }
</style>
</head>
<body>
<main>
<nav><a href="./">Open the editor</a> · <a href="about">About Phext Edit</a> · <a href="privacy">Privacy Policy</a> · <a href="terms">Terms of Service</a></nav>
<h1>💎 Claim your space</h1>
<p>For seventeen years, Will Bickford's use of Twitter was one long search: spanning an
exocortex, looking for other minds working the way his does. The result is a map —
<a href="https://github.com/wbic16/human">wavefront.phext</a> — placing 520 accounts by
how they stand in relation to that work: their coordinate on nine axes of alignment,
in a region (the Hearth, the Shoals, or open water).</p>
<p><b>If your X account is in the map, a significant space on this server is already
yours</b> — hosted phext space you can delegate as you wish. It is granted, not sold:
all you have to do is claim it by signing in with the X account that was placed.
Organic discovery is the scarce thing; the map is the receipt.</p>
<p>Not in the map? You still get a small space. A buy-in (opening when payments are
switched on) enlarges it and funds the exocortex — and it is priced by
<b>novelty</b>: staking a crowded, well-studied region of the map is cheap, because
little novelty remains there; staking a void — a region no one has been found in —
costs the most, because unexplored space is the rarest thing the exocortex has.
A buy-in never buys a placement in the map. We are not looking for money; we are
looking for alignment.</p>

<h2>Watch the front move</h2>
<!-- VIDEO SLOT: Will uploads wavefront-viz.mp4 to YouTube himself. When the
     upload is live, replace this placeholder with the YouTube embed iframe and
     add frame-src https://www.youtube-nocookie.com to the CSP in src/oauth.rs
     (frame-src is 'none' until then, deliberately). -->
<div class="video-slot">🎬 <b>The intro film</b> — 520 people, placed one coordinate
at a time, June 2008 to June 2025.<br>
<em>The video embed lands here as soon as the YouTube upload is live.</em><br>
Until then: <a href="explore">explore the map yourself</a> — it is live and zoomable.</div>

<h2>Are you in the map?</h2>
<form id="lookup-form">
<p><input type="text" id="lookup-handle" placeholder="@yourhandle" size="24">
<button type="submit">Look up</button></p>
<p id="lookup-msg" class="dim">The lookup needs no sign-in.</p>
</form>
<div id="lookup-result"></div>

<h2>The grants</h2>
<table>
<tr><th>Tier</th><th>Who</th><th>Space</th><th>Phexts</th><th>May delegate to</th></tr>
<tr><td>Found</td><td>your account is in the map</td><td>100 MiB</td><td>25</td><td>25 accounts</td></tr>
<tr><td>Bought</td><td>not in the map, void-region buy-in ($100)</td><td>100 MiB</td><td>25</td><td>25 accounts</td></tr>
<tr><td>Patron</td><td>not in the map, crowded-region buy-in ($10)</td><td>10 MiB</td><td>5</td><td>5 accounts</td></tr>
<tr><td>New</td><td>not in the map, no buy-in</td><td>1 MiB</td><td>1</td><td>—</td></tr>
</table>
<p>Every single phext is capped at 10 MiB, the same cap uploads for review use.
Delegating means naming other accounts that may work in your space; what you
delegate stays yours to revoke.</p>

<h2>Your own lens</h2>
<p>Each claimed space comes with the wavefront kit: the same instrument that built
Will's map, pointed at your own archive, so you can learn who you are connected to
and why. The kit's nine axes are a lens, and the lens can be reground — your
alignment metric may differ from Will's, and that is expected. Your map is yours.</p>

<h2>Claim</h2>
<p id="who" class="dim">Checking your sign-in…</p>
<div id="claim-box" style="display:none">
<p id="claim-state" class="dim"></p>
<p><button id="claim-button" type="button">Claim my space</button></p>
<p id="claim-msg" class="dim"></p>
</div>
<p id="dashboard-link" style="display:none">Owner and editors: the <a href="/claim/dashboard">claim dashboard</a> lists who has claimed their space, and who has joined the wavefront since the snapshot.</p>

<footer>Phext Edit · <a href="about">About</a> · <a href="privacy">Privacy Policy</a> · <a href="terms">Terms of Service</a> ·
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a><!--phext-sponsor--></footer>
</main>
<script>
const $ = (id) => document.getElementById(id);
let session = null;
function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({"&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;"}[c]));
}
function fmtSize(n) {
  if (n < 1024) return n + " B";
  if (n < 1048576) return (n / 1024).toFixed(1) + " KiB";
  return (n / 1048576).toFixed(1) + " MiB";
}
function grantLine(g) {
  return fmtSize(g.storage_bytes) + " of space, up to " + g.max_phexts +
    (g.max_phexts === 1 ? " phext" : " phexts") + ", delegate to " + g.max_delegates +
    (g.max_delegates === 1 ? " account" : " accounts");
}
async function doLookup(handle) {
  const box = $("lookup-result");
  box.innerHTML = '<p class="dim">Looking…</p>';
  const r = await fetch("/api/claim/lookup?handle=" + encodeURIComponent(handle));
  const v = await r.json().catch(() => ({}));
  if (!r.ok || !v.ok) { box.innerHTML = '<p class="dim">The lookup did not answer.</p>'; return; }
  if (v.found) {
    const places = v.placements.map((p) =>
      "<li><code>" + esc(p.coordinate) + "</code> — " + esc(p.region) +
      (p.label ? " (" + esc(p.label) + ")" : "") + "</li>").join("");
    box.innerHTML = "<p><b>" + esc(v.handle) + "</b> (" + esc(v.name) + ") is in the map:</p><ul>" +
      places + "</ul><p>Tier <b>found</b>: " + esc(grantLine(v.grant)) +
      ". Sign in with X below to claim it.</p>";
  } else {
    box.innerHTML = "<p><b>" + esc(v.handle) + "</b> is not in the map — yet. Tier <b>new</b>: " +
      esc(grantLine(v.grant)) + ". Buy-in tiers open when payments are switched on.</p>";
  }
}
$("lookup-form").addEventListener("submit", (e) => {
  e.preventDefault();
  const h = $("lookup-handle").value.trim();
  if (h) doLookup(h);
});
async function loadSession() {
  try {
    const r = await fetch("/api/session");
    session = await r.json();
  } catch (e) {
    $("who").textContent = "Could not check your sign-in.";
    return;
  }
  const el = $("who");
  if (!session.signed_in) {
    const links = (session.providers || [])
      .map((p) => '<a href="/api/auth/' + p + '?next=/claim">Sign in with ' + esc(p) + "</a>")
      .join(" · ");
    el.innerHTML = links
      ? "You are not signed in. " + links + " — claiming matches the map by your X account, so sign in with X to claim a found placement."
      : "You are not signed in, and sign-in is not set up on this server.";
    $("claim-box").style.display = "none";
    return;
  }
  el.innerHTML =
    "Signed in as <b>" + esc(session.name || session.handle || session.who) + "</b> (" +
    esc(session.who) + ")" + (session.editor ? " — editor" : "") +
    '. <a href="#" id="logout">Sign out</a>';
  $("logout").onclick = async (e) => {
    e.preventDefault();
    await fetch("/api/auth/logout", { method: "POST", headers: { "x-csrf-token": session.csrf } });
    location.reload();
  };
  $("claim-box").style.display = "";
  $("dashboard-link").style.display =
    (session.editor || (session.who.startsWith("x:") && (session.handle || "").toLowerCase() === "wbic16")) ? "" : "none";
  loadClaim();
}
async function loadClaim() {
  const state = $("claim-state");
  const r = await fetch("/api/claim");
  if (r.status === 403) {
    state.textContent = "Claims are not open yet — claiming opens when X sign-in is switched on. The lookup above works today.";
    $("claim-button").style.display = "none";
    return;
  }
  const v = await r.json().catch(() => ({}));
  if (v.claimed === false) {
    state.textContent = "You have not claimed yet.";
    $("claim-button").style.display = "";
    return;
  }
  state.innerHTML = "Claimed: tier <b>" + esc(v.tier) + "</b>" +
    (v.handle ? " for <b>" + esc(v.handle) + "</b>" : "") + " — " + esc(grantLine(v.grant)) + ".";
  $("claim-button").style.display = "none";
}
$("claim-button").addEventListener("click", async () => {
  const msg = $("claim-msg");
  msg.textContent = "Claiming…";
  const r = await fetch("/api/claim", { method: "POST", headers: { "x-csrf-token": session.csrf } });
  const v = await r.json().catch(() => ({}));
  if (!r.ok) { msg.textContent = v.error || "That did not work."; return; }
  msg.textContent = "";
  loadClaim();
});
loadSession();
</script>
</body>
</html>
"##);

const SUBMIT_HTML: &str = concat!(r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="description" content="Upload a phext for review. If an editor accepts it, your phext joins the library and your account becomes an editor.">
<title>Phext Edit: upload a phext for review</title>
<link rel="icon" type="image/svg+xml" href="favicon.svg">
"##, public_style!(), r##"
<style>
button { background: #2e2e2e; color: #dedede; border: 1px solid #4a4a4a; border-radius: 4px; padding: 2px 10px; cursor: pointer; font: inherit; }
button:disabled { opacity: 0.5; cursor: default; }
input[type="file"] { font: inherit; color: #dedede; }
.sub { margin: 10px 0; }
</style>
</head>
<body>
<main>
<nav><a href="./">Open the editor</a> · <a href="about">About Phext Edit</a> · <a href="privacy">Privacy Policy</a> · <a href="terms">Terms of Service</a></nav>
<h1>💎 Upload a phext for review</h1>
<p>Upload a phext for review. If an editor accepts it, your phext joins the library
and your account becomes an editor — you will be able to open and edit the phexts on
this server, including your own.</p>
<p>Your phext waits outside the library until an editor decides. Uploads are capped
at 10 MiB.</p>

<p id="who" class="dim">Checking your sign-in…</p>

<div id="upload-box" style="display:none">
<h2>Upload</h2>
<form id="upload-form">
<p><input type="file" id="file" accept=".phext"></p>
<p><button type="submit">Upload for review</button></p>
<p id="msg" class="dim"></p>
</form>
</div>

<div id="lists"></div>

<footer>Phext Edit · <a href="about">About</a> · <a href="privacy">Privacy Policy</a> · <a href="terms">Terms of Service</a> ·
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a><!--phext-sponsor--></footer>
</main>
<script>
const $ = (id) => document.getElementById(id);
let session = null;
function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({"&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;"}[c]));
}
function fmtSize(n) {
  if (n < 1024) return n + " B";
  if (n < 1048576) return (n / 1024).toFixed(1) + " KiB";
  return (n / 1048576).toFixed(1) + " MiB";
}
function fmtTime(s) { return s ? new Date(s * 1000).toLocaleString() : ""; }
async function loadSession() {
  try {
    const r = await fetch("/api/session");
    session = await r.json();
  } catch (e) {
    $("who").textContent = "Could not check your sign-in.";
    return;
  }
  const el = $("who");
  if (!session.signed_in) {
    const links = (session.providers || [])
      .map((p) => '<a href="/api/auth/' + p + '?next=/submit">Sign in with ' + esc(p) + "</a>")
      .join(" · ");
    el.innerHTML = links
      ? "You are not signed in. " + links + " to upload a phext for review."
      : "You are not signed in, and sign-in is not set up on this server.";
    $("upload-box").style.display = "none";
  } else {
    el.innerHTML =
      "Signed in as <b>" + esc(session.name || session.handle || session.who) + "</b> (" +
      esc(session.who) + ")" + (session.editor ? " — editor" : "") +
      '. <a href="#" id="logout">Sign out</a>';
    $("logout").onclick = async (e) => {
      e.preventDefault();
      await fetch("/api/auth/logout", { method: "POST", headers: { "x-csrf-token": session.csrf } });
      location.reload();
    };
    $("upload-box").style.display = "";
  }
  loadList();
}
function subLine(s) {
  return (
    "<b>" + esc(s.filename) + "</b> — " + esc(s.status) + ", " + s.scrolls + " scrolls, " +
    fmtSize(s.bytes) + ", " + fmtTime(s.submitted_at) +
    (s.curated_as ? " — curated as <code>" + esc(s.curated_as) + "</code>" : "") +
    ' <a href="/api/submissions/' + s.id + '/download">Download</a>'
  );
}
async function loadList() {
  const box = $("lists");
  if (!session || !session.signed_in) { box.innerHTML = ""; return; }
  const r = await fetch("/api/submissions");
  if (!r.ok) { box.innerHTML = '<p class="dim">Could not load submissions.</p>'; return; }
  const subs = await r.json();
  const mine = subs.filter((s) => s.who === session.who || (session.email && s.email === session.email));
  let html = "<h2>Your submissions</h2>";
  html += mine.length
    ? "<ul>" + mine.map((s) => "<li>" + subLine(s) + "</li>").join("") + "</ul>"
    : '<p class="dim">None yet.</p>';
  if (session.editor) {
    const pending = subs.filter((s) => s.status === "pending");
    html += "<h2>Review queue</h2>";
    html += pending.length
      ? pending.map((s) =>
          '<div class="sub">' + subLine(s) + "<br>" +
          "from " + esc(s.name || s.who) + " (" + esc(s.email || s.who) + ") " +
          '<button data-act="accept" data-id="' + s.id + '">Accept</button> ' +
          '<button data-act="decline" data-id="' + s.id + '">Decline</button></div>'
        ).join("")
      : '<p class="dim">Nothing waiting.</p>';
  }
  box.innerHTML = html;
  box.querySelectorAll("button[data-act]").forEach((b) => {
    b.onclick = async () => {
      b.disabled = true;
      const r = await fetch("/api/submissions/" + b.dataset.id + "/" + b.dataset.act, {
        method: "POST",
        headers: { "x-csrf-token": session.csrf },
      });
      const v = await r.json().catch(() => ({}));
      if (!r.ok) { alert(v.error || "That did not work."); b.disabled = false; return; }
      loadList();
      loadSession();
    };
  });
}
$("upload-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const file = $("file").files[0];
  const msg = $("msg");
  if (!file) { msg.textContent = "Choose a .phext file first."; return; }
  msg.textContent = "Uploading…";
  const r = await fetch("/api/submissions", {
    method: "POST",
    headers: { "x-csrf-token": session.csrf, "x-filename": file.name },
    body: file,
  });
  const v = await r.json().catch(() => ({}));
  if (!r.ok) { msg.textContent = v.error || "Upload failed."; return; }
  msg.textContent = "Received — " + v.filename + " is waiting for review.";
  e.target.reset();
  loadList();
});
loadSession();
</script>
</body>
</html>
"##);

const ABOUT_HTML: &str = concat!(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="description" content="Phext Edit is a web editor for phext files: plain text arranged in 11 dimensions.">
<title>Phext Edit: about</title>
<link rel="icon" href="/favicon.ico" sizes="16x16 32x32 48x48">
<link rel="icon" type="image/svg+xml" href="/favicon.svg">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
"#, public_style!(), r#"
</head>
<body>
<a class="skip-link" href='#content'>Skip to content</a>
<main id="content">
<nav><a href="./">Open the editor</a> · <a href="privacy">Privacy Policy</a> · <a href="terms">Terms of Service</a></nav>
<h1>💎 Phext Edit</h1>
<p class="dim">A web editor for phext files.</p>

<h2>What it does</h2>
<p>Phext is plain text arranged in 11 dimensions. A phext file holds many scrolls, and each
scroll lives at a coordinate such as <code>1.1.1/1.1.1/1.1.1</code>. Phext Edit lets you work
with those files from a browser:</p>
<ul>
  <li>move through a phext by coordinate, or step to the next or previous scroll that has text</li>
  <li>read and edit any scroll, then save it back to the phext file on the server</li>
  <li>browse the file as a tree of its dimensions and search its text</li>
  <li>switch between the phext files the server hosts</li>
</ul>
<p>Phext Edit is part of the open-source
<a href="https://github.com/wbic16/phext-lattice">phext-lattice</a> project.</p>

<h2>Public utilities</h2>
<p>Some parts of this site need no sign-in at all:</p>
<ul>
  <li><a href="up-goer-five.html">Up Goer Five</a>: check writing against the ten hundred
  most used words, and see each word's phext coordinate.</li>
  <li><a href="tts-demo.html">Coordinate TTS demo</a>: hear phext coordinates spoken aloud.</li>
  <li><a href="learn-you-phext.html">Learn You Phext</a>: an introduction to phext that is
  itself a phext, scroll by scroll.</li>
  <li><a href="singular.html">Singular</a>: an Up Goer Five variant whose word list is one
  person's Twitter archive — read, write, and read/write with only those words.</li>
  <li><a href="phext-previewer.html">Phext Previewer</a>: open a phext file from your
  own device and read it scroll by scroll at its coordinates — step by dimension,
  jump to a coordinate, search the text, and see a map of the whole file. The file
  is parsed in your browser and never uploaded.</li>
  <li><a href="submit">Upload a phext for review</a> (sign-in needed): offer a phext to
  this server's library. If an editor accepts it, the phext joins the library and the
  uploader becomes an editor too.</li>
  <li><a href="claim">Claim your space</a>: if your X account is placed in the wavefront
  map, a significant hosted space on this server is yours to claim and delegate —
  check the map from that page, no sign-in needed for the lookup.</li>
  <li><a href="explore">Explore the wavefront</a>: every placement in the
  nine-dimensional address space — rotate the triple projection, slice it by any
  two axes, click a point to read its entry.</li>
</ul>

<h2>Why it asks you to sign in</h2>
<p>The files on this server can be changed, so only approved editors may open them. You can
sign in with your Google, GitHub or X account. Sign-in is used for one thing: to check who you
are, so the server can tell whether your account is on its list of editors.</p>
<ul>
  <li><b>Google</b> sign-in asks for your basic profile and email address
    (<code>openid</code>, <code>email</code>, <code>profile</code>).</li>
  <li><b>GitHub</b> sign-in asks for read-only access to your public profile
    (<code>read:user</code>).</li>
  <li><b>X</b> sign-in asks for read-only access to your account ID and username
    (<code>tweet.read</code>, <code>users.read</code>). Signing in with X adds your
    account to the list of editors.</li>
</ul>
<p>Phext Edit does not read your Gmail, Drive, contacts, calendar or repositories. It does
not post anything for you, and it does not share or sell your information. The
<a href="privacy">Privacy Policy</a> gives the details.</p>

<h2>No ads, ever</h2>
<p>Phext Edit does not show advertisements, and it never will. There is no ad
slot, ad network, or sponsored placement. A donation, when the link is shown,
supports the site. It is not an advertisement and it is not an Exocortex
Founding purchase.</p>

<footer>Phext Edit · <a href="privacy">Privacy Policy</a> · <a href="terms">Terms of Service</a> ·
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a><!--phext-sponsor--></footer>
</main>
</body>
</html>
"#);

const PRIVACY_HTML: &str = concat!(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="description" content="How Phext Edit handles the information it gets when you sign in.">
<title>Phext Edit: privacy policy</title>
<link rel="icon" href="/favicon.ico" sizes="16x16 32x32 48x48">
<link rel="icon" type="image/svg+xml" href="/favicon.svg">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
"#, public_style!(), r#"
</head>
<body>
<a class="skip-link" href='#content'>Skip to content</a>
<main id="content">
<nav><a href="about">About Phext Edit</a> · <a href="terms">Terms of Service</a> · <a href="./">Open the editor</a></nav>
<h1>Privacy Policy</h1>
<p class="dim">Effective 8 November 2026</p>

<p>This policy covers Phext Edit, the web editor for phext files. It explains what Phext Edit
learns when you sign in, how that information is used, and what happens to it.</p>

<h2>What we receive</h2>
<p>When you sign in with <b>Google</b>, Phext Edit asks for the <code>openid</code>,
<code>email</code> and <code>profile</code> scopes. Google sends your account ID, your name, and
your email address with whether Google has verified it.</p>
<p>When you sign in with <b>GitHub</b>, Phext Edit asks for the <code>read:user</code> scope.
GitHub sends your account ID, username, display name, and your public email address if you
have one.</p>
<p>When you sign in with <b>X</b>, Phext Edit asks for the <code>tweet.read</code> and
<code>users.read</code> scopes. X sends your account ID and your username. Phext Edit never
posts for you, and it does not read your timeline.</p>
<p>Phext Edit asks for nothing else. It cannot see your Gmail, Drive, contacts, calendar or
repositories.</p>

<h2>How it is used</h2>
<p>Your sign-in details are used only to:</p>
<ul>
  <li>check whether your account may use the editor, by comparing your account ID or verified
    email with the server's list of editors</li>
  <li>show your name or username in the editor so you can see who is signed in</li>
</ul>
<p>They are not used for advertising, profiling or analytics, and they are not sold or shared
with anyone.</p>

<h2>What is stored, and for how long</h2>
<ul>
  <li><b>Sign-in cookie.</b> After you sign in, your account ID, username, name and verified
    email go into a signed, HttpOnly cookie in your own browser. It expires after 14 days, or
    when you sign out. The server keeps no account database.</li>
  <li><b>Provider tokens.</b> The access token Google, GitHub or X returns is used once to read
    your profile, then thrown away. It is never saved, and Phext Edit never acts on your account.</li>
  <li><b>Your edits.</b> Text you save is written into the phext files on the server. Those
    files belong to the server's owner and are not tied to your account.</li>
  <li><b>Server logs.</b> The web server keeps standard access logs (IP address, time, page
    requested and browser) to keep the service running and secure. These logs do not include
    your account details.</li>
</ul>
<p>Phext Edit uses no third-party analytics, advertising or tracking scripts.
There are no ads, ever. An optional donation opens a Stripe Payment Link in a
new tab. Phext Edit does not embed a payment form, and it does not add a
tracking pixel for an ad campaign.</p>

<h2>Google user data</h2>
<p>Phext Edit's use and transfer of information received from Google APIs adheres to the
<a href="https://developers.google.com/terms/api-services-user-data-policy">Google API Services
User Data Policy</a>, including the Limited Use requirements.</p>

<h2>Your choices</h2>
<ul>
  <li>Sign out at any time from the status bar. This deletes the sign-in cookie.</li>
  <li>Remove Phext Edit's access from your
    <a href="https://myaccount.google.com/connections">Google account</a>, your
    <a href="https://github.com/settings/applications">GitHub settings</a> or your
    <a href="https://x.com/settings/connected_apps">X settings</a>.</li>
  <li>Email questions or requests about your data to
    <a href="mailto:will+support@phext.io">will+support@phext.io</a>.</li>
</ul>

<h2>Changes</h2>
<p>If this policy changes, the new version will be posted on this page with a new effective date.</p>

<footer>Phext Edit · <a href="about">About</a> · <a href="terms">Terms of Service</a> ·
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a><!--phext-sponsor--></footer>
</main>
</body>
</html>
"#);

const TERMS_HTML: &str = concat!(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="description" content="Terms for Phext Edit: sign in with Google, GitHub or X, edit the phext files on this server, no ads ever.">
<title>Phext Edit: terms of service</title>
<link rel="icon" href="/favicon.ico" sizes="16x16 32x32 48x48">
<link rel="icon" type="image/svg+xml" href="/favicon.svg">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
"#, public_style!(), r#"
</head>
<body>
<main>
<nav><a href="about">About Phext Edit</a> · <a href="privacy">Privacy Policy</a> · <a href="./">Open the editor</a></nav>
<h1>Terms of Service</h1>
<p class="dim">Effective 8 November 2026</p>

<p>These terms describe Phext Edit, the web editor for phext files at this site.
Read them with the <a href="privacy">Privacy Policy</a>, which says what happens
to the information from sign-in.</p>

<h2>The editor</h2>
<p>Phext is plain text arranged in 11 dimensions. Phext Edit lets you read and edit
the phext files this server hosts, from a browser. It is part of the open-source
<a href="https://github.com/wbic16/phext-lattice">phext-lattice</a> project.</p>

<h2>Signing in</h2>
<p>The files can be changed, so the editor asks you to sign in before you open them.
You sign in with a Google account, a GitHub account or an X account. Sign-in is used to check who
you are, so the server can tell whether your account may use the editor. Phext Edit
does not read your mail, files, contacts, repositories or timeline, and it does not post
anything for you.</p>

<h2>Files you edit</h2>
<p>Text you save is written into the phext files on this server. Those files belong
to the person who runs the server, and they are not tied to your account.</p>

<h2>No ads, ever</h2>
<p>Phext Edit does not show advertisements, and it never will. There are no ad
slots, ad networks, sponsored placements, or tracking pixels for ad campaigns.
It does not use your sign-in or your edits for advertising. A donation supports
the site. That donation is not an advertisement and it is not an Exocortex
Founding purchase.</p>

<h2>Questions</h2>
<p>Email <a href="mailto:will+support@phext.io">will+support@phext.io</a>.</p>

<h2>Changes</h2>
<p>If these terms change, the new version will be posted on this page with a new effective date.</p>

<footer>Phext Edit · <a href="about">About</a> · <a href="privacy">Privacy Policy</a> ·
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a><!--phext-sponsor--></footer>
</main>
</body>
</html>
"#);

// The Up Goer Five editor page lives as a file under web/ (it carries a large
// inline atlas) and is compiled in, so it serves exactly like the pages above.
const UP_GOER_FIVE_HTML: &str = include_str!("../../web/up-goer-five.html");

// The TTS demo page likewise lives as a file under web/ and is compiled in, so
// it serves exactly like the pages above.
const TTS_DEMO_HTML: &str = include_str!("../../web/tts-demo.html");

// The TTS demo's script likewise lives as a file under web/ and is compiled
// in, so the demo page's <script src="phext-tts.js"> resolves like the pages
// above. It is not HTML, so it takes the static cache class.
const PHEXT_TTS_JS: &str = include_str!("../../web/phext-tts.js");

// The Learn You Phext intro page likewise lives as a file under web/ and is
// compiled in, so it serves exactly like the pages above.
const LEARN_YOU_PHEXT_HTML: &str = include_str!("../../web/learn-you-phext.html");

// The intro's phext itself likewise lives as a file under web/ and is compiled
// in, served raw so a visitor can take the very file the page teaches from
// (delimiters and all) and host it in their own SQ. It is not HTML, so it
// takes the static cache class.
const LEARN_YOU_PHEXT: &str = include_str!("../../web/learn-you-phext.phext");

// The Singular page likewise lives as a file under web/ and is compiled in:
// an Up Goer Five variant whose word list is one person's Twitter archive.
// Its modes pick which list flags the text: Read (what he read), Write
// (what he wrote), Read/Write (both together). The page is a map of CYOA
// and carries everything inline — the three lists plus every mapped
// word's coordinate — so it makes no requests of its own; there are no
// separate word-list routes. (Generated by the singular skill's
// build_editor.py; edit the template there, not this file's copy.)
const SINGULAR_HTML: &str = include_str!("../../web/singular.html");

// The wavefront explorer page and its data likewise live as files under web/
// and are compiled in. The JSON is a generated export of wavefront.phext
// (scripts/build_wavefront_explorer.py); the page reads it same-origin.
const EXPLORE_HTML: &str = include_str!("../../web/explore.html");
const WAVEFRONT_EXPLORER_JSON: &str = include_str!("../../web/wavefront-explorer.json");

// The Phext Previewer page likewise lives as a file under web/ and is
// compiled in: open any .phext file in the browser and read it scroll
// by scroll, search it, and map it. The file is parsed client-side and
// never leaves the visitor's device, so the page needs no sign-in and
// no server route beyond this one.
const PREVIEWER_HTML: &str = include_str!("../../web/phext-previewer.html");

// The shell's artwork likewise lives under web/ and is compiled in: the
// CYOA banner (the Flame at the root of the lattice, light running out
// along the filaments to a wall of scrolls) that tops the sign-in
// screen and the claim dashboard, and the lattice-vault backdrop behind
// them. Binary, so include_bytes!; both are served by icon_body, the
// same pre-gate route as the favicon.
const BANNER_WEBP: &[u8] = include_bytes!("../../web/banner.webp");
const BACKGROUND_WEBP: &[u8] = include_bytes!("../../web/background.webp");

// Canonical host is https://edit.phext.io (repeated below; tests check both files).
// Disallow comes before Allow so a first-match crawler still blocks /api.
// Allow: / is the public landing page (the editor shell). File contents and
// sign-in live under /api, which stays disallowed.
const ROBOTS_TXT: &str = "\
User-agent: *\n\
Disallow: /api\n\
Allow: /\n\
Allow: /index.html\n\
Allow: /about\n\
Allow: /privacy\n\
Allow: /terms\n\
\n\
Sitemap: https://edit.phext.io/sitemap.xml\n\
";

const SITEMAP_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://edit.phext.io/</loc></url>
  <url><loc>https://edit.phext.io/about</loc></url>
  <url><loc>https://edit.phext.io/privacy</loc></url>
  <url><loc>https://edit.phext.io/terms</loc></url>
  <url><loc>https://edit.phext.io/up-goer-five.html</loc></url>
  <url><loc>https://edit.phext.io/tts-demo.html</loc></url>
  <url><loc>https://edit.phext.io/learn-you-phext.html</loc></url>
  <url><loc>https://edit.phext.io/singular.html</loc></url>
  <url><loc>https://edit.phext.io/phext-previewer.html</loc></url>
  <url><loc>https://edit.phext.io/submit</loc></url>
  <url><loc>https://edit.phext.io/claim</loc></url>
  <url><loc>https://edit.phext.io/explore</loc></url>
</urlset>
"#;

const NOT_FOUND_HTML: &str = concat!(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Phext Edit: page not found</title>
<link rel="icon" href="/favicon.ico" sizes="16x16 32x32 48x48">
<link rel="icon" type="image/svg+xml" href="/favicon.svg">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
"#, public_style!(), r#"
</head>
<body>
<main>
<nav><a href="/">Phext Edit</a> · <a href="/about">About</a> · <a href="/privacy">Privacy Policy</a> · <a href="/terms">Terms of Service</a></nav>
<h1>Page not found</h1>
<p>There is no page at that address.</p>
<p><a href="/">Back to Phext Edit</a></p>
<footer>Phext Edit · <a href="/about">About</a> · <a href="/privacy">Privacy Policy</a> · <a href="/terms">Terms of Service</a> · Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a><!--phext-sponsor--></footer>
</main>
</body>
</html>
"#);

// ── Embedded Frontend ──────────────────────────────────────────────────

const INDEX_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<title>Phext Edit: an editor for 11-dimensional plain text</title>
<meta name="description" content="Browse and edit phext files in the browser. Phext is plain text arranged in 11 dimensions, and the files stay plain text, with no lock-in.">
<link rel="canonical" href="https://edit.phext.io/">
<meta name="theme-color" content="#171717">
<!-- Also served at /<name>/. Asset and API URLs in this shell are absolute. -->
<link rel="icon" href="/favicon.ico" sizes="16x16 32x32 48x48">
<link rel="icon" type="image/svg+xml" href="/favicon.svg">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
<meta property="og:title" content="Phext Edit: an editor for 11-dimensional plain text">
<meta property="og:description" content="Browse and edit phext files in the browser. Phext is plain text arranged in 11 dimensions, and the files stay plain text, with no lock-in.">
<meta property="og:type" content="website">
<meta property="og:url" content="https://edit.phext.io/">
<meta property="og:site_name" content="Phext Edit">
<meta property="og:image" content="https://edit.phext.io/og.png">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta property="og:image:alt" content="Phext Edit: an editor for plain text arranged in 11 dimensions">
<meta name="twitter:card" content="summary_large_image">
<meta name="twitter:title" content="Phext Edit: an editor for 11-dimensional plain text">
<meta name="twitter:description" content="Browse and edit phext files in the browser. Phext is plain text arranged in 11 dimensions, and the files stay plain text, with no lock-in.">
<meta name="twitter:image" content="https://edit.phext.io/og.png">
<style>
:root {
  --bg: #171717; --panel: #131313; --bar: #1c1c1c; --border: #2e2e2e;
  --text: #dedede; --dim: #9a9a9a; --coord: #d4a855; --field-border: #767676;
  --z-color: #4dc9c9; --y-color: #c964c9; --x-color: #6bc96b;
  --active: #e8c547; --danger: #e05555;
  --trans-fast: 120ms ease; --trans-med: 200ms ease;
}
* { margin: 0; padding: 0; box-sizing: border-box; }
body {
  background: var(--bg); color: var(--text);
  font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
  font-size: 13px; height: 100vh; height: 100dvh; display: flex; flex-direction: column; overflow: hidden;
}

/* ── Coordinate bar ── */
#coord-bar {
  height: 36px; background: var(--bar); border-bottom: 1px solid var(--border);
  display: flex; align-items: center; padding: 0 16px; color: var(--coord);
  font-weight: bold; gap: 8px; flex-shrink: 0;
}
#coord-bar .scrolls { color: var(--dim); font-weight: normal; font-size: 11px; }
#coord-bar .dirty { color: var(--danger); font-size: 11px; }
.zoom-crumbs { display: flex; gap: 4px; font-size: 11px; font-weight: normal; }
.zoom-crumbs .crumb { color: var(--z-color); cursor: pointer; transition: color var(--trans-fast); }
.zoom-crumbs .crumb:hover { color: var(--active); text-decoration: underline; }
.zoom-crumbs .sep { color: var(--dim); }

/* ── Main ── */
#main { flex: 1; display: flex; overflow: hidden; }

/* ── Left panel: sentron + navtree ── */
#left-panel {
  width: 320px; background: var(--panel); border-right: 1px solid var(--border);
  display: flex; flex-direction: column; flex-shrink: 0; overflow: hidden;
}
#panel-tabs {
  height: 28px; display: flex; border-bottom: 1px solid var(--border);
  font-size: 11px; flex-shrink: 0;
}
#panel-tabs .tab {
  flex: 1; display: flex; align-items: center; justify-content: center;
  cursor: pointer; color: var(--dim); transition: all var(--trans-fast);
  border-bottom: 2px solid transparent;
}
#panel-tabs .tab:hover { color: var(--text); background: #ffffff06; }
#panel-tabs .tab.active { color: var(--text); border-bottom-color: var(--coord); }
#sentron-panel, #navtree-panel { flex: 1; overflow-y: auto; padding: 10px 12px; }
#navtree-panel { display: none; }

/* Sentron */
#sentron-panel .header { color: var(--dim); margin-bottom: 6px; font-size: 12px; }
#sentron-panel .group-label { color: var(--dim); margin-top: 6px; font-size: 11px; }
.axon {
  display: flex; gap: 4px; cursor: pointer; padding: 1px 4px; border-radius: 3px;
  font-size: 12px; transition: background var(--trans-fast);
}
.axon:hover { background: #ffffff10; }
.axon.active { background: #ffffff18; }
.axon .marker { width: 12px; color: var(--active); }
.axon.spatial { color: var(--z-color); }
.axon.temporal { color: var(--y-color); }
.axon.neuron { color: var(--x-color); }
.axon .dim-num { width: 14px; text-align: right; }
.axon .dim-name { width: 80px; }
.axon .dim-val { width: 36px; text-align: right; }
.axon .counts { color: var(--dim); }

/* Densities */
.densities { margin-top: 10px; border-top: 1px solid var(--border); padding-top: 6px; }
.densities .row { display: flex; gap: 6px; font-size: 11px; color: var(--dim); }
.densities .row .name { width: 70px; }
.densities .row .spark { letter-spacing: 1px; }

/* ── Navtree ── */
.nt-level { font-size: 11px; color: var(--dim); margin: 6px 0 4px; }
.nt-level.z { color: var(--z-color); }
.nt-level.y { color: var(--y-color); }
.nt-level.x { color: var(--x-color); }
.nt-group {
  display: flex; align-items: center; gap: 4px; padding: 2px 4px; cursor: pointer;
  border-radius: 3px; transition: background var(--trans-fast); font-size: 12px;
}
.nt-group:hover { background: #ffffff10; }
.nt-group.current { background: #ffffff14; color: var(--active); }
.nt-group .nt-arrow { width: 14px; color: var(--dim); font-size: 10px; transition: transform var(--trans-fast); }
.nt-group .nt-arrow.open { transform: rotate(90deg); }
.nt-group .nt-label { color: var(--coord); min-width: 70px; }
.nt-group .nt-count { color: var(--dim); font-size: 10px; }
.nt-children { margin-left: 16px; overflow: hidden; transition: max-height var(--trans-med); }
.nt-children.collapsed { max-height: 0 !important; }
.nt-scroll {
  padding: 1px 4px; cursor: pointer; border-radius: 2px; font-size: 11px;
  display: flex; gap: 6px; transition: background var(--trans-fast);
  white-space: nowrap; overflow: hidden;
}
.nt-scroll:hover { background: #ffffff10; }
.nt-scroll.current { background: #ffffff14; color: var(--active); }
.nt-scroll .nt-coord { color: var(--x-color); min-width: 60px; }
.nt-scroll .nt-preview { color: var(--dim); overflow: hidden; text-overflow: ellipsis; }

/* ── Dimensional subspace headers ── */
.subspace-header {
  padding: 6px 8px; margin: 8px 0 4px; border-radius: 4px; font-size: 11px;
  display: flex; align-items: center; gap: 8px;
}
.subspace-header.d11 { background: #ffffff08; color: var(--text); border-left: 3px solid var(--z-color); }
.subspace-header.d7 { background: #ffffff06; color: var(--text); border-left: 3px solid var(--y-color); }
.subspace-header.d4 { background: #ffffff04; color: var(--text); border-left: 3px solid var(--x-color); }
.subspace-header .sh-dim { font-weight: bold; }
.subspace-header .sh-desc { color: var(--dim); }

/* ── Content area ── */
#content-area { flex: 1; display: flex; flex-direction: column; overflow: hidden; }
#content {
  flex: 1; overflow-y: auto; padding: 16px 20px;
  white-space: pre-wrap; word-wrap: break-word; line-height: 1.5; tab-size: 4;
  transition: opacity var(--trans-fast);
}
#content.empty { color: var(--dim); font-style: italic; }
#content a.coord-link {
  color: var(--coord); text-decoration: none; cursor: pointer;
  border-bottom: 1px dotted var(--coord); transition: color var(--trans-fast);
}
#content a.coord-link:hover { color: var(--active); border-bottom-color: var(--active); }

#editor { display: none; flex: 1; overflow: hidden; flex-direction: column; }
#editor textarea {
  width: 100%; flex: 1; background: var(--bg); color: var(--text);
  font-family: inherit; font-size: 13px; line-height: 1.5;
  padding: 16px 20px; border: none; outline: none; resize: none; tab-size: 4;
}
#editor .editor-toolbar {
  height: 28px; background: var(--bar); border-bottom: 1px solid var(--border);
  display: flex; align-items: center; padding: 0 12px; gap: 8px; font-size: 11px; flex-shrink: 0;
}
#editor .editor-toolbar button {
  background: var(--border); color: var(--text); border: none;
  padding: 2px 10px; border-radius: 3px; cursor: pointer;
  font-family: inherit; font-size: 11px; transition: background var(--trans-fast);
}
#editor .editor-toolbar button:hover { background: #444; }
#editor .editor-toolbar button.save-btn { background: var(--x-color); color: #000; }
#editor .editor-toolbar button.save-btn:hover { background: #8ddf8d; }
#editor .editor-toolbar .coord-label { color: var(--coord); }

/* ── Status bar ── */
#status-bar {
  height: 68px; background: #101010; border-top: 1px solid var(--border);
  display: flex; align-items: center; padding: 0 18px; font-size: 15px; flex-shrink: 0; gap: 18px;
}
#status-bar .mode {
  padding: 6px 14px; border-radius: 4px; font-weight: bold; color: #000;
  transition: background var(--trans-fast);
}
#status-bar .mode.lattice { background: var(--z-color); }
#status-bar .mode.edit { background: var(--x-color); }
#status-bar > * { white-space: nowrap; flex-shrink: 0; }
#status-bar .help { color: var(--dim); flex-shrink: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; }
#status-bar .msg { color: var(--active); }
#status-bar .signout {
  padding: 8px 16px; border: 1px solid var(--coord); border-radius: 6px;
  background: #2a2312; color: var(--coord); cursor: pointer; white-space: nowrap;
  transition: background var(--trans-fast), color var(--trans-fast);
}
#status-bar .signout:hover { background: var(--coord); color: #000; }
#status-bar .touch-actions { display: flex; gap: 6px; }
#status-bar .touch-actions button {
  background: var(--bar); color: var(--text); border: 1px solid var(--border);
  border-radius: 6px; padding: 8px 10px; font-family: inherit; font-size: 13px;
  cursor: pointer; white-space: nowrap; transition: border-color var(--trans-fast), color var(--trans-fast);
}
#status-bar .touch-actions button:hover { border-color: var(--coord); color: var(--coord); }
#status-bar .bar-link {
  margin-left: 14px; color: var(--dim); text-decoration: none; white-space: nowrap;
}
#status-bar .bar-link.first { margin-left: auto; }
#status-bar .bar-link:hover { color: var(--coord); text-decoration: underline; }
#status-bar { padding-bottom: env(safe-area-inset-bottom, 0px); }
#status-bar .sponsor-offer { margin-left: 14px; white-space: nowrap; }
#status-bar .sponsor-link { color: var(--dim); text-decoration: none; }
#status-bar .sponsor-link:hover { color: var(--coord); text-decoration: underline; }
#auth-screen .sponsor-link { color: var(--coord); }

/* ── Overlays ── */
.overlay {
  display: none; position: fixed; top: 36px; left: 50%; transform: translateX(-50%);
  background: var(--panel); border: 1px solid var(--border); border-radius: 6px;
  padding: 8px; z-index: 10; box-shadow: 0 8px 32px #00000080;
}
.overlay.visible { display: block; }
#search-overlay { width: 500px; max-height: 400px; }
#goto-overlay { width: 360px; }
.overlay input {
  width: 100%; background: var(--bg); border: 1px solid var(--border);
  color: var(--text); font-family: inherit; font-size: 13px;
  padding: 6px 10px; border-radius: 4px; outline: none;
}
.overlay input:focus { border-color: var(--coord); }
#search-results { max-height: 300px; overflow-y: auto; margin-top: 6px; }
#search-results .hit,
#phext-list .hit {
  padding: 4px 8px; cursor: pointer; border-radius: 3px; display: flex; gap: 10px;
  transition: background var(--trans-fast);
  text-decoration: none; color: inherit;
}
#search-results .hit:hover,
#phext-list .hit:hover { background: #ffffff10; }
#search-results .hit .hit-coord,
#phext-list .hit .hit-coord {
  color: var(--coord); min-width: 160px; cursor: pointer;
  text-decoration: underline; text-underline-offset: 2px;
}
#search-results .hit:hover .hit-coord,
#phext-list .hit:hover .hit-coord { color: var(--active); }
#search-results .hit .hit-ctx,
#phext-list .hit .hit-ctx { color: var(--dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
#phext-list .hit[aria-current="true"] { background: #ffffff10; }
.auth-btn { display:block; background:var(--panel); border:1px solid var(--border); color:var(--text);
  font-family:inherit; font-size:14px; padding:10px 14px; border-radius:6px; text-decoration:none; cursor:pointer; }
.auth-btn:hover { border-color:var(--coord); }
.auth-btn.primary { background:var(--coord); color:#1a1408; border-color:var(--coord); font-weight:600; margin-bottom:8px; }
.welcome { max-width:460px; margin:64px 16px; text-align:center; }
.welcome h1 { color:var(--coord); font-size:26px; font-weight:600; margin-bottom:10px; }
.welcome .lead { font-size:16px; line-height:1.4; margin-bottom:18px; }
.welcome .explain, .welcome .hint { color:var(--dim); font-size:13px; line-height:1.55; margin-top:16px; }
.welcome .links { margin-top:14px; font-size:12px; }
.welcome .links a { color:var(--coord); }
#auth-providers { display:flex; flex-direction:column; gap:8px; }
#demo-banner {
  align-items:center; gap:14px; height:32px; padding:0 16px; flex-shrink:0;
  background:#241e10; color:var(--coord); font-size:12px; border-bottom:1px solid var(--border);
}
#demo-banner a { color:var(--coord); }

/* ── Auth screen artwork ──
   The sign-in screen is the shell's front door, so it wears the art:
   the lattice-vault backdrop fills the screen under a half-dark scrim
   (the vault is darkest at its centre by design, and the scrim keeps
   the card's words at their usual contrast), and the CYOA banner —
   the Flame at the root of the lattice — tops the card. The editor's
   own surfaces keep their solid backgrounds: no artwork ever sits
   under text being edited. */
#auth-screen {
  background: linear-gradient(rgba(11, 14, 19, 0.5), rgba(11, 14, 19, 0.5)),
    url("/background.webp") center / cover no-repeat #0b0e13;
}
#auth-screen .auth-banner {
  display: block; width: 100%; height: 132px; object-fit: cover; object-position: center;
  border: 1px solid var(--border); border-radius: 8px; margin-bottom: 20px;
}

/* ── Mobile layout and accessibility ─────────────────────────────────
   Own block so it stays out of the way of other shell edits.
   The product theme is dark (R3.6). A prefers-color-scheme light override
   would fight that, so it is intentionally not set.
   --dim is #9a9a9a: #808080 is only 4.32:1 on the coord bar (#1c1c1c).
   Input borders use --field-border (#767676, 3.9:1 on #171717). Layout
   rules stay on --border; those lines are separators, not the control.
   Merged over the earlier mobile pass: its landscape band is kept
   below, and its dvh, generic-input, auth-button centring and
   hit-coord rules are folded into this block. */
.skip-link {
  position: absolute; left: 8px; top: 0; transform: translateY(-120%);
  background: var(--active); color: #000; font-weight: bold; text-decoration: none;
  padding: 8px 12px; border-radius: 4px; z-index: 200;
}
.skip-link:focus { transform: none; top: 8px; }
:focus-visible { outline: 2px solid var(--active); outline-offset: 2px; }
#editor textarea:focus-visible,
.overlay input:focus-visible { outline: 2px solid var(--active); outline-offset: 0; }
.overlay input, .auth-btn { border-color: var(--field-border); }
.overlay input::placeholder { color: #a3a3a3; opacity: 1; }
#panel-toggle { display: none; }
#panel-backdrop { display: none; }
button.tab, button.axon, button.nt-group, button.nt-scroll, button.hit,
#status-bar button#active-file {
  font: inherit; background: transparent; border: 0; color: inherit; cursor: pointer;
}
#panel-tabs button.tab { border-bottom: 2px solid transparent; padding: 0 8px; color: var(--dim); }
#panel-tabs button.tab.active { color: var(--text); border-bottom-color: var(--coord); }
button.axon, button.nt-group, button.nt-scroll, button.hit,
#status-bar button#active-file { text-align: left; }
button.axon, button.nt-group, button.nt-scroll, button.hit { width: 100%; }
#status-bar button.signout { font: inherit; }
#status-bar button#active-file { padding: 0; }
#phext-list button.hit { display: flex; gap: 10px; padding: 4px 8px; border-radius: 3px; }
#phext-list button.hit:hover { background: #ffffff10; }
#search-overlay { width: min(500px, calc(100vw - 16px)); }
#goto-overlay { width: min(360px, calc(100vw - 16px)); }
#phext-overlay { width: min(500px, calc(100vw - 16px)); }
/* Mobile top-bar actions, the message strip, and the sheet
   furniture (panel close, More-sheet header): present in the
   markup, display:none everywhere except inside the small-screen
   rules below, where the phone layout switches them on. */
#mobile-actions, #msg-strip, #sheet-close, #more-sheet-head { display: none; }

@media (max-width: 760px) {
  #panel-toggle {
    display: inline-flex; align-items: center; justify-content: center;
    min-height: 44px; min-width: 44px; padding: 8px 12px; margin-right: 4px;
    background: transparent; color: var(--coord); border: 1px solid var(--coord);
    border-radius: 6px; font: inherit; cursor: pointer; flex-shrink: 0;
  }
  #coord-bar { height: auto; min-height: 44px; flex-wrap: wrap; padding: 4px 8px; row-gap: 4px; }
  #coord-text { min-width: 0; }
  /* The compact top bar (Will's direction, 2026-11-08): the
     coordinate address and one row of action buttons; nothing else
     competes for the top of the screen. The scroll count leaves
     the bar, and feedback messages are mirrored into a transient
     strip in the bar, since the status bar is a sheet (below). */
  #coord-bar .scrolls { display: none; }
  #coord-text { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  #msg-strip.has-msg { display: block; flex-basis: 100%; color: var(--active); font-size: 12px; font-weight: normal; }
  #mobile-actions { display: flex; flex-basis: 100%; gap: 6px; }
  #mobile-actions button {
    flex: 1; min-height: 44px; padding: 6px 2px; background: var(--bar); color: var(--text);
    border: 1px solid var(--border); border-radius: 6px; font-family: inherit; font-size: 11px;
    cursor: pointer; white-space: nowrap; overflow: hidden;
  }
  #mobile-actions button:active { border-color: var(--coord); color: var(--coord); }
  #mobile-actions #btn-sentron { flex-grow: 1.3; }
  #mobile-actions #btn-search { flex-grow: 1.15; }
  #mobile-actions #btn-more { flex-grow: 0.85; }
  #main { position: relative; }
  /* The drawer is a full-width sheet on phones: Sentron and Nav
     get the entire screen, so nothing in them is squeezed into a
     desktop-width sidebar (the truncation Will photographed). */
  #left-panel {
    position: absolute; top: 0; bottom: 0; left: 0; z-index: 20;
    width: 100%; max-width: 100%;
    transform: translateX(-105%); pointer-events: none;
    box-shadow: 8px 0 24px #00000099;
  }
  #left-panel.drawer-open { transform: none; pointer-events: auto; }
  #panel-backdrop.visible {
    display: block; position: absolute; inset: 0; z-index: 19;
    background: #00000066; border: 0; padding: 0; cursor: pointer;
  }
  #panel-tabs { height: auto; }
  #sheet-close { display: flex; align-items: center; justify-content: center; width: 48px; flex-shrink: 0; background: transparent; border: 0; cursor: pointer; color: var(--dim); font-size: 15px; font-family: inherit; }
  #panel-tabs button.tab, button.axon, button.nt-group, button.nt-scroll, .hit,
  #status-bar button.signout, #status-bar button#active-file,
  #editor .editor-toolbar button, .auth-btn { min-height: 44px; }
  button.axon, button.nt-group, button.nt-scroll, .hit { padding: 10px 8px; }
  #content { padding: 10px 12px; }
  #editor .editor-toolbar { height: auto; min-height: 44px; flex-wrap: wrap; padding: 4px 8px; }
  #editor .editor-toolbar button { padding: 8px 12px; }
  #editor textarea, .overlay input { font-size: 16px; }
  input, select { font-size: 16px; }
  /* No permanent status bar on phones: hidden until More raises
     it as a bottom sheet (body.more-open, below). That is what
     gives the content — above all the editor — the full height of
     the screen. */
  #status-bar { display: none; }
  body.more-open #status-bar {
    display: flex; position: fixed; left: 0; right: 0; bottom: 0; height: auto;
    max-height: 72vh; max-height: 72dvh; overflow-y: auto; z-index: 60;
    flex-direction: column; align-items: stretch; gap: 4px; font-size: 14px;
    padding: 10px 14px; padding-bottom: calc(10px + env(safe-area-inset-bottom, 0px));
  }
  body.more-open #more-sheet-head { display: flex; align-items: center; justify-content: space-between; color: var(--dim); margin-bottom: 4px; }
  #more-sheet-head button {
    background: var(--bar); color: var(--text); border: 1px solid var(--border); border-radius: 6px;
    font-family: inherit; font-size: 13px; padding: 8px 12px; min-height: 40px; cursor: pointer;
  }
  body.more-open #status-bar > * { white-space: normal; flex-shrink: 1; }
  body.more-open #status-bar .bar-link, body.more-open #status-bar .sponsor-offer { margin-left: 0; padding: 11px 4px; }
  body.more-open #status-bar #active-file { padding: 11px 4px; }
  body.more-open #status-bar .touch-actions { display: none; }
  body.more-open #status-bar .signout { align-self: flex-start; }
  #status-bar > * { white-space: normal; }
  #status-bar .help { order: 10; flex-basis: 100%; white-space: normal; overflow: visible; text-overflow: unset; }
  #status-bar .bar-link, #status-bar .sponsor-offer { margin-left: 0; }
  #status-bar button#active-file {
    min-width: 0; max-width: 100%; flex-shrink: 1; overflow: hidden; text-overflow: ellipsis;
  }
  .overlay {
    top: 8px; left: 8px; right: 8px; width: auto; transform: none;
    max-height: calc(100vh - 16px); max-height: calc(100dvh - 16px); overflow: auto;
  }
  #search-overlay, #goto-overlay, #phext-overlay { width: auto; }
  #auth-screen { padding: 16px; overflow: auto; }
  #auth-screen > div { width: min(400px, 100%); box-sizing: border-box; }
  #auth-screen .auth-banner { height: 96px; }
  #editor textarea { padding: 10px 12px; }
  #search-results .hit .hit-coord,
  #phext-list .hit .hit-coord { min-width: 0; }
  .auth-btn { display: flex; align-items: center; justify-content: center; }
}
@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after {
    animation-duration: 0.01ms !important; animation-iteration-count: 1 !important;
    transition-duration: 0.01ms !important; scroll-behavior: auto !important;
  }
}

/* ── Landscape phones (701–1100px wide) ──
   Too wide for the small-screen rules above, too narrow for the
   desktop status bar: that bar is a single nowrap row measuring
   ~1487px of content, so at 844/915px its tail — Preview, Mutable
   OS, Claim dashboard, sign out — was clipped off-screen with no way
   to scroll to it. Give this band the same wrap treatment as the
   small-screen rules so every item stays reachable. Portrait and
   full desktop widths are untouched. */
@media (min-width: 701px) and (max-width: 1100px) {
  #status-bar { height: auto; min-height: 52px; flex-wrap: wrap; padding: 8px 12px; gap: 8px 14px; }
  #status-bar .help { order: 10; flex-basis: 100%; white-space: normal; overflow: visible; text-overflow: clip; }
  #status-bar .bar-link, #status-bar .sponsor-offer { margin-left: 0; }
}
</style>
</head>
<body>
<a class="skip-link" href="#content">Skip to scroll</a>

<div id="demo-banner" style="display:none">
  <span>Demo. Walk the 11 dimensions with j and k. Nothing is saved.</span>
  <span id="demo-signin">
    <a href="/api/auth/google?next=%2F">Google</a>
    <a href="/api/auth/github?next=%2F">GitHub</a>
  </span>
  <a id="demo-leave" href="/" style="display:none">Your files</a>
</div>

<header id="coord-bar">
  <button type="button" id="panel-toggle" aria-controls="left-panel" aria-expanded="false" onclick="toggleDrawer()">Panel</button>
  <span id="coord-text">loading...</span>
  <span class="scrolls" id="scroll-count"></span>
  <span class="dirty" id="dirty-flag"></span>
  <span id="msg-strip" role="status"></span>
  <div id="mobile-actions">
    <button type="button" id="btn-sentron" onclick="showPanel('sentron')" title="Sentron — the lattice around this scroll">◉ Sentron</button>
    <button type="button" id="btn-nav" onclick="showPanel('navtree')" title="Nav — the scroll tree">🌳 Nav</button>
    <button type="button" id="btn-edit" onclick="enterEdit()" title="Edit the current scroll (key: e)">✎ Edit</button>
    <button type="button" id="btn-search" onclick="openSearch()" title="Search scrolls (key: /)">⌕ Search</button>
    <button type="button" id="btn-goto" onclick="openGoto()" title="Go to a coordinate (key: g)">⌖ Goto</button>
    <button type="button" id="btn-more" onclick="toggleMoreSheet()" title="More — file, links, sign out">≡ More</button>
  </div>
</header>

<main id="main">
  <button type="button" id="panel-backdrop" aria-label="Close lattice panel" hidden onclick="closeDrawer()"></button>
  <nav id="left-panel" aria-label="Lattice">
    <div id="panel-tabs" role="tablist" aria-label="Lattice panels">
      <button type="button" class="tab active" id="tab-sentron" role="tab" aria-selected="true" aria-controls="sentron-panel" tabindex="0" onclick="showPanel('sentron')">◉ Sentron</button>
      <button type="button" class="tab" id="tab-navtree" role="tab" aria-selected="false" aria-controls="navtree-panel" tabindex="-1" onclick="showPanel('navtree')">🌳 Nav</button>
      <button type="button" id="sheet-close" aria-label="Close panel" onclick="closeDrawer()">✕</button>
    </div>
    <div id="sentron-panel" role="tabpanel" aria-labelledby="tab-sentron"></div>
    <div id="navtree-panel" role="tabpanel" aria-labelledby="tab-navtree"></div>
  </nav>
  <div id="content-area">
    <div id="content" tabindex="-1"></div>
    <div id="editor">
      <div class="editor-toolbar">
        <span class="coord-label" id="edit-coord"></span>
        <button type="button" class="save-btn" onclick="saveScroll()">Save (Ctrl+S)</button>
        <button type="button" onclick="cancelEdit()">Cancel (Esc)</button>
        <button type="button" onclick="openUG5()" title="Open this scroll in the Up Goer Five editor">UG5</button>
      </div>
      <textarea id="edit-textarea" aria-label="Scroll text" spellcheck="false"></textarea>
    </div>
  </div>
</main>

<div id="status-bar" role="region" aria-label="Editor status">
  <div id="more-sheet-head"><span>≡ More</span><button type="button" onclick="toggleMoreSheet()">✕ Close</button></div>
  <span class="mode lattice" id="mode-label">LATTICE</span>
  <button type="button" id="active-file" style="color:var(--dim);cursor:pointer" onclick="openPhextPicker()" title="Click to switch phext"></button>
  <span class="touch-actions"><button type="button" onclick="enterEdit()" title="Edit the current scroll (key: e)">✎ Edit</button><button type="button" onclick="openSearch()" title="Search scrolls (key: /)">⌕ Search</button><button type="button" onclick="openGoto()" title="Go to a coordinate (key: g)">⌖ Goto</button></span>
  <span class="help" id="help-text">1-9:dim  h/l:move  j/k:nav  J/K:×10  e:edit  u:UG5  t:tree  /:search  g:goto  Home:BASE</span>
  <span class="msg" id="status-msg" role="status" aria-live="polite"></span>
  <a class="bar-link first" href="/submit" title="Upload a phext for review — an editor can accept it into the library">Submit a phext</a>
  <a class="bar-link" href="/claim" title="If your X account is in the wavefront map, claim your space on this server">Claim your space</a>
  <a class="bar-link" href="/explore" title="Every placement in the wavefront, explorable in nine dimensions">Explore the map</a>
  <a class="bar-link" href="/phext-previewer.html" title="Open a phext file from your own device and preview it scroll by scroll — no sign-in needed">Preview a phext</a>
  <a class="bar-link" href="/api/auth/sso/mutable" title="Mutable OS — documents and Caverns, with this same sign-in">Mutable OS</a>
  <!--phext-sponsor-->
  <a class="bar-link" id="dashboard-link" href="/claim/dashboard" style="display:none" title="Who has claimed their wavefront space, and who has joined since the snapshot — the owner and editors">Claim dashboard</a>
  <button type="button" id="signout" class="signout" style="display:none" onclick="signOut()" title="Sign out"></button>
</div>

<div class="overlay" id="search-overlay" role="dialog" aria-label="Search scrolls">
  <input id="search-input" aria-label="Search scrolls" placeholder="search scrolls..." autocomplete="off" />
  <div id="search-results" aria-live="polite"></div>
</div>
<div class="overlay" id="goto-overlay" role="dialog" aria-label="Go to coordinate">
  <input id="goto-input" aria-label="Go to coordinate" placeholder="z.z.z/y.y.y/x.x.x" autocomplete="off" />
</div>
<div class="overlay" id="phext-overlay" role="dialog" aria-label="Switch phext" style="max-height:500px">
  <div style="color:var(--dim);font-size:11px;margin-bottom:6px">Select a phext file:</div>
  <div id="phext-list" style="max-height:400px;overflow-y:auto"></div>
</div>
<div id="auth-screen" data-open="1" style="display:flex;position:fixed;inset:0;padding:env(safe-area-inset-top, 0px) env(safe-area-inset-right, 0px) env(safe-area-inset-bottom, 0px) env(safe-area-inset-left, 0px);z-index:100;justify-content:center;overflow:auto" role="dialog" aria-modal="true" aria-labelledby="auth-prompt">
  <div class="welcome">
    <img class="auth-banner" src="/banner.webp" loading="lazy" decoding="async" alt="The Flame at the root coordinate, its light running out along the filaments to a vast wall of scrolls — the phext lattice">
    <h1>Phext Edit</h1>
    <p id="auth-prompt" class="lead">Write in 11 dimensions: plain text, no lock-in.</p>
    <a id="try-demo" class="auth-btn primary" href="/?demo">Try the demo</a>
    <div id="auth-providers">
      <a class="auth-btn" data-provider="google" href="/api/auth/google?next=%2F">Sign in with Google</a>
      <a class="auth-btn" data-provider="github" href="/api/auth/github?next=%2F">Sign in with GitHub</a>
      <a class="auth-btn" data-provider="x" href="/api/auth/x?next=%2F">Sign in with X</a>
    </div>
    <div id="auth-error" role="alert" style="color:var(--danger);margin-top:8px;font-size:12px"></div>
    <p class="explain">Nine numbers name a scroll: library, shelf, series, collection, volume, book, chapter, section, scroll. The scroll is a page of lines and columns, which makes eleven. The file stays plain text.</p>
    <p class="hint">Sign in to save and edit your own files. The demo is a sample you can walk through; nothing in it is saved.</p>
    <p class="links"><a href="/about">About</a> · <a href="/privacy">Privacy Policy</a> · <a href="/terms">Terms of Service</a> · <a href="mailto:will+support@phext.io">Contact</a><br>
      No sign-in needed: <a href="/up-goer-five.html">Up Goer Five</a> · <a href="/tts-demo.html">Coordinate TTS demo</a> · <a href="/learn-you-phext.html">Learn You Phext</a> · <a href="/singular.html">Singular</a> · <a href="/phext-previewer.html">Phext Previewer</a><br>
      Have a phext of your own? <a href="/submit">Upload it for review</a> — if an editor accepts it, you become an editor too. Signing in with X makes you an editor at once.<br>
      Placed in the wavefront map? <a href="/claim">Claim your space</a> — a significant hosted space is already yours if your X account is in the map. Or <a href="/explore">explore the map</a> first. No ads, ever.<!--phext-sponsor--></p>
    <noscript>JavaScript opens the demo. Sign in with the Google, GitHub, or X links above works without it.</noscript>
  </div>
</div>

<script>
const $ = id => document.getElementById(id);

// ── State ──
let mode = 'lattice';
let lastNav = null;
let dirty = false;
let statusTimeout = null;
let treeData = null;
let treeExpanded = {};
let activePanel = 'sentron';
let needsAuth = false;
let csrf = '';
let session = null;
let demoMode = false;
let demoAt = '1.1.1/1.1.1/1.1.1';
let demoDim = 1;
const demoEdits = new Map();
const wantDemo = new URLSearchParams(location.search).has('demo');

function showMsg(msg) {
  $('status-msg').textContent = msg;
  clearTimeout(statusTimeout);
  statusTimeout = setTimeout(() => $('status-msg').textContent = '', 2000);
  // Mirror into the top bar's message strip: on phones the status
  // bar is a sheet (closed by default), so its .msg span is out of
  // sight when feedback fires. The strip is display:none on desktop
  // and whenever it carries no message.
  const ms = $('msg-strip');
  if (ms) {
    ms.textContent = msg;
    ms.classList.toggle('has-msg', !!msg);
    clearTimeout(ms._t);
    if (msg) ms._t = setTimeout(() => { ms.classList.remove('has-msg'); ms.textContent = ''; }, 2000);
  }
}

function setMode(m) {
  mode = m;
  const ml = $('mode-label');
  ml.textContent = m.toUpperCase();
  ml.className = 'mode ' + m;
  $('content').style.display = (m === 'lattice') ? '' : 'none';
  $('editor').style.display = (m === 'edit') ? 'flex' : 'none';
  const help = {
    lattice: '1-9:dim  h/l:move  j/k:nav  J/K:×10  e:edit  u:UG5  t:tree  /:search  g:goto  Home:BASE  Ctrl+S:save',
    edit: 'Ctrl+S:save  Esc:cancel',
    search: 'Enter:search  Esc:close',
    goto: 'Enter:go  Esc:close',
  };
  $('help-text').textContent = help[m] || '';
}

function showPanel(name) {
  activePanel = name;
  // Explicit values, never '': the stylesheet hides #navtree-panel
  // (display: none) until first use, and an empty inline style does
  // not override a stylesheet rule — with '' the Nav panel could
  // never be shown. 'block' is the panels' natural display (they are
  // block-level flex items of the column #left-panel; flex: 1 still
  // sizes them), and an inline value always wins over the stylesheet.
  const sentron = name === 'sentron';
  $('sentron-panel').style.display = sentron ? 'block' : 'none';
  $('navtree-panel').style.display = sentron ? 'none' : 'block';
  $('tab-sentron').className = 'tab' + (sentron ? ' active' : '');
  $('tab-navtree').className = 'tab' + (sentron ? '' : ' active');
  $('tab-sentron').setAttribute('aria-selected', sentron ? 'true' : 'false');
  $('tab-navtree').setAttribute('aria-selected', sentron ? 'false' : 'true');
  $('tab-sentron').tabIndex = sentron ? 0 : -1;
  $('tab-navtree').tabIndex = sentron ? -1 : 0;
  if (name === 'navtree' && !treeData) loadTree();
  document.body.classList.remove('more-open');
  if (window.matchMedia('(max-width: 760px)').matches) setDrawer(true);
}

function drawerNarrow() { return window.matchMedia('(max-width: 760px)').matches; }

function setDrawer(open) {
  const show = !!(open && drawerNarrow());
  const panel = $('left-panel');
  const toggle = $('panel-toggle');
  const backdrop = $('panel-backdrop');
  const wasOpen = !!(panel && panel.classList.contains('drawer-open'));
  if (panel) {
    panel.classList.toggle('drawer-open', show);
    panel.inert = drawerNarrow() && !show;
  }
  if (toggle) {
    toggle.setAttribute('aria-expanded', show ? 'true' : 'false');
    toggle.textContent = show ? 'Close' : 'Panel';
  }
  if (backdrop) {
    backdrop.hidden = !show;
    backdrop.classList.toggle('visible', show);
  }
  if (wasOpen && !show && panel && toggle && panel.contains(document.activeElement)) toggle.focus();
}

function toggleDrawer() {
  const open = !$('left-panel').classList.contains('drawer-open');
  setDrawer(open);
  if (open) {
    const tab = activePanel === 'navtree' ? $('tab-navtree') : $('tab-sentron');
    // inert is cleared in setDrawer; focus waits until that takes effect.
    if (tab) requestAnimationFrame(() => tab.focus());
  }
}

function closeDrawer() {
  setDrawer(false);
  const toggle = $('panel-toggle');
  if (toggle && drawerNarrow()) toggle.focus();
}

// ── Mobile More sheet ──
// On small screens the status bar is not permanent furniture: it
// opens as a bottom sheet from the top bar's More button. The class
// toggled here only has CSS inside the small-screen media query, so
// this helper is inert on desktop.
function toggleMoreSheet() {
  const opening = !document.body.classList.contains('more-open');
  if (opening) setDrawer(false);
  document.body.classList.toggle('more-open', opening);
}

$('panel-tabs').addEventListener('keydown', (e) => {
  const tabs = ['sentron', 'navtree'];
  const i = Math.max(0, tabs.indexOf(activePanel));
  let next = -1;
  if (e.key === 'ArrowRight') next = (i + 1) % tabs.length;
  else if (e.key === 'ArrowLeft') next = (i - 1 + tabs.length) % tabs.length;
  else if (e.key === 'Home') next = 0;
  else if (e.key === 'End') next = tabs.length - 1;
  else return;
  e.preventDefault();
  showPanel(tabs[next]);
  $('tab-' + tabs[next]).focus();
});
window.matchMedia('(max-width: 760px)').addEventListener('change', () => { setDrawer(false); document.body.classList.remove('more-open'); });
setDrawer(false);

async function api(method, path, body) {
  if (demoMode) return demoApi(method, path, body);
  if (path.charAt(0) !== '/') path = '/' + path;
  const opts = { method, headers: {} };
  if (csrf && method !== 'GET') opts.headers['x-csrf-token'] = csrf;
  if (body !== undefined) {
    opts.headers['Content-Type'] = (typeof body === 'object' ? 'application/json' : 'text/plain');
    opts.body = typeof body === 'object' ? JSON.stringify(body) : body;
  }
  const res = await fetch(path, opts);
  if (res.status === 401 || res.status === 403) {
    needsAuth = true;
    let msg = '';
    try { msg = (await res.json()).error || ''; } catch {}
    await showAuthScreen(res.status === 403 ? msg : '');
    throw new Error('unauthorized');
  }
  return res.json();
}

// ── Shareable URL: /<name>/#LB.SF.SR/CN.VM.BK/CH.SN.SC ──
// The 11D scroll address is the canonical 9-number coordinate. `;` is the
// libphext URL form of `/`. A fragment that does not parse falls back to origin.
const DEFAULT_COORD = '1.1.1/1.1.1/1.1.1';
const PHEXT_NAME_RE = /^[A-Za-z0-9_-]+$/;
const RESERVED_PHEXT = { about: 1, api: 1, index: 1, privacy: 1, terms: 1, submit: 1, claim: 1, explore: 1 };
let defaultStem = '';
let defaultPath = '';
let urlSync = false;
// The first fragment write replaces the loaded URL. Later moves push, so
// back/forward walk coordinates. A link that already has a fragment is the
// first history entry.
let urlReady = location.hash.length > 1;

function canonicalCoord(nums) {
  if (!nums || nums.length !== 9 || nums.some(n => !Number.isInteger(n) || n < 1 || n > 1000)) return null;
  const [a,b,c,d,e,f,g,h,i] = nums;
  return a+'.'+b+'.'+c+'/'+d+'.'+e+'.'+f+'/'+g+'.'+h+'.'+i;
}
function normalizeCoord(raw) {
  if (!raw) return null;
  let s = String(raw).trim();
  try { s = decodeURIComponent(s.replace(/\+/g, '%2B')); } catch { return null; }
  if (/^\d{1,4}(?:\/\d{1,4}){8}$/.test(s)) return canonicalCoord(s.split('/').map(n => Number(n)));
  s = s.replace(/;/g, '/');
  if (!/^\d{1,4}(?:\.\d{1,4}){2}\/\d{1,4}(?:\.\d{1,4}){2}\/\d{1,4}(?:\.\d{1,4}){2}$/.test(s)) return null;
  return canonicalCoord(s.split(/[./]/).map(n => Number(n)));
}

function readFragment() {
  const raw = location.hash.startsWith('#') ? location.hash.slice(1) : '';
  if (!raw) return { present: false, malformed: false, coord: DEFAULT_COORD };
  const coord = normalizeCoord(raw);
  if (!coord) return { present: true, malformed: true, coord: DEFAULT_COORD };
  return { present: true, malformed: false, coord };
}

function phextNameFromLocation() {
  const parts = location.pathname.split('/').filter(Boolean);
  if (parts.length === 1 && PHEXT_NAME_RE.test(parts[0]) && !RESERVED_PHEXT[parts[0]]) return parts[0];
  return '';
}

function sharePath(name) {
  return (name && PHEXT_NAME_RE.test(name) && !RESERVED_PHEXT[name]) ? '/' + name + '/' : '/';
}

function fileStem(file) {
  const base = String(file || '').split('/').pop() || '';
  return base.replace(/\.phext$/, '');
}

function publishCoord(coord) {
  const name = phextNameFromLocation();
  const path = sharePath(name);
  const next = path + location.search + '#' + coord;
  const now = location.pathname + location.search + location.hash;
  if (now !== next) {
    urlSync = true;
    history[urlReady ? 'pushState' : 'replaceState'](null, '', next);
    urlSync = false;
  }
  urlReady = true;
}

// First 40 UTF-8 bytes of the scroll, cut on a character boundary, then one line.
function scrollTitle(text) {
  const raw = text == null ? '' : String(text);
  const bytes = new TextEncoder().encode(raw);
  let n = Math.min(40, bytes.length);
  if (n < bytes.length && (bytes[n] & 0xc0) === 0x80) {
    while (n > 0 && (bytes[n] & 0xc0) === 0x80) n--;
  }
  const snippet = new TextDecoder().decode(bytes.subarray(0, n))
    .replace(/[\u0000-\u001F\u007F-\u009F\u2028\u2029]/g, ' ')
    .replace(/ +/g, ' ')
    .trim();
  return snippet ? snippet + ' | Phext Edit' : 'Phext Edit';
}

function setScrollTitle(text) {
  document.title = scrollTitle(text);
}

function render(nav, skipHash) {
  if (demoMode && nav && nav.position && demoEdits.has(nav.position.coordinate)) {
    nav.scroll.content = demoEdits.get(nav.position.coordinate);
    nav.scroll.bytes = nav.scroll.content.length;
    nav.position.has_scroll = nav.scroll.content.length > 0;
  }
  lastNav = nav;
  const p = nav.position;
  const indicator = p.has_scroll ? '●' : '○';
  $('coord-text').textContent = `💎 ${p.coordinate} ◆ ${p.dimension} ${indicator}`;
  $('scroll-count').textContent = `[${p.total_scrolls} scrolls]`;
  $('dirty-flag').textContent = dirty ? '● unsaved' : '';
  if (p.file) $('active-file').textContent = '📂 ' + p.file.split('/').pop();

  if (!skipHash && p.coordinate) publishCoord(p.coordinate);

  renderSentron(nav);
  renderContent(nav);
  setScrollTitle(nav.scroll && nav.scroll.content);
  // A navigation lands back on the scroll: on phones, dismiss the
  // More sheet so the destination is what fills the screen.
  if (drawerNarrow()) document.body.classList.remove('more-open');
  if (activePanel === 'navtree' && treeData) highlightTreeNode(p.coordinate);
}

function renderSentron(nav) {
  const s = nav.sentron;
  const p = nav.position;
  let html = `<div class="header">◉ sentron [${s.size}/40] reach: ${s.structural_reach}↔${s.sequential_reach}</div>`;
  html += '<div class="group-label">── spatial ──</div>';
  for (const a of s.axons.filter(a => a.group === 'spatial')) {
    const active = a.dimension === p.dimension_index ? 'active' : '';
    const marker = a.dimension === p.dimension_index ? '▸' : ' ';
    html += `<button type="button" class="axon spatial ${active}" aria-pressed="${active ? 'true' : 'false'}" onclick="selectDim(${a.dimension})">
      <span class="marker">${marker}</span><span class="dim-num">${a.dimension}</span>
      <span class="dim-name">${a.name}</span><span class="dim-val">${a.value}</span>
      <span class="counts">−${a.backward} +${a.forward}</span></button>`;
  }
  html += '<div class="group-label">── temporal ──</div>';
  for (const a of s.axons.filter(a => a.group === 'temporal')) {
    const active = a.dimension === p.dimension_index ? 'active' : '';
    const marker = a.dimension === p.dimension_index ? '▸' : ' ';
    html += `<button type="button" class="axon temporal ${active}" aria-pressed="${active ? 'true' : 'false'}" onclick="selectDim(${a.dimension})">
      <span class="marker">${marker}</span><span class="dim-num">${a.dimension}</span>
      <span class="dim-name">${a.name}</span><span class="dim-val">${a.value}</span>
      <span class="counts">−${a.backward} +${a.forward}</span></button>`;
  }
  for (const a of s.axons.filter(a => a.group === 'neuron')) {
    const active = a.dimension === p.dimension_index ? 'active' : '';
    const marker = a.dimension === p.dimension_index ? '▸' : ' ';
    html += `<button type="button" class="axon neuron ${active}" aria-pressed="${active ? 'true' : 'false'}" onclick="selectDim(${a.dimension})">
      <span class="marker">${marker}</span><span class="dim-num">${a.dimension}</span>
      <span class="dim-name">${a.name}</span><span class="dim-val">${a.value}</span>
      <span class="counts">← neuron</span></button>`;
  }
  if (nav.densities && nav.densities.length) {
    html += '<div class="densities">';
    for (const d of nav.densities)
      html += `<div class="row"><span class="name">${d.name}</span><span class="spark">${d.sparkline}</span></div>`;
    html += '</div>';
  }
  $('sentron-panel').innerHTML = html;
}

function renderContent(nav) {
  const el = $('content');
  if (nav.scroll.content) {
    el.innerHTML = linkifyCoordinates(escHtml(nav.scroll.content));
    el.className = '';
  } else {
    el.textContent = '○ empty coordinate';
    el.className = 'empty';
  }
}

// ── Navtree ──
// Builds a hierarchical tree from flat coordinate list.
// Structure: Z groups → Y groups → X scrolls
// Dimensional subspace: 11D = full, 7D = within Z, 4D = within Z/Y, 1D = single scroll

async function loadTree() {
  treeData = await api('GET', '/api/tree');
  renderTree();
}

function buildTree(coords) {
  // coords = ["1.1.1/1.1.1/1.1.1", ...]
  const tree = {}; // zKey → { yKey → [xCoords] }
  for (const c of coords) {
    const [z, y, x] = c.split('/');
    if (!tree[z]) tree[z] = {};
    if (!tree[z][y]) tree[z][y] = [];
    tree[z][y].push({ x, full: c });
  }
  return tree;
}

function renderTree() {
  if (!treeData) return;
  const tree = buildTree(treeData.coordinates);
  const zKeys = Object.keys(tree).sort();
  const curCoord = lastNav ? lastNav.position.coordinate : '';

  let html = `<div class="subspace-header d11"><span class="sh-dim">11D</span><span class="sh-desc">Full Lattice · ${treeData.total_scrolls} scrolls</span></div>`;

  for (const zk of zKeys) {
    const yGroups = tree[zk];
    const yKeys = Object.keys(yGroups).sort();
    const zCount = yKeys.reduce((s, yk) => s + yGroups[yk].length, 0);
    const zExpanded = !!treeExpanded[zk];
    const zCurrent = curCoord.startsWith(zk + '/') ? ' current' : '';

    html += `<button type="button" class="nt-group${zCurrent}" aria-expanded="${zExpanded ? 'true' : 'false'}" onclick="toggleZ('${zk}')">
      <span class="nt-arrow${zExpanded ? ' open' : ''}">▶</span>
      <span class="nt-label">${zk}</span>
      <span class="nt-count">${zCount}</span></button>`;
    html += `<div class="nt-children${zExpanded ? '' : ' collapsed'}" id="nt-z-${zk.replace(/\./g,'-')}">`;

    if (zExpanded) {
      html += `<div class="subspace-header d7"><span class="sh-dim">7D</span><span class="sh-desc">Z=${zk} · ${yKeys.length} subspaces</span></div>`;
      for (const yk of yKeys) {
        const scrolls = yGroups[yk];
        const zyKey = zk + '/' + yk;
        const yExpanded = !!treeExpanded[zyKey];
        const yCurrent = curCoord.startsWith(zyKey + '/') ? ' current' : '';

        html += `<button type="button" class="nt-group${yCurrent}" aria-expanded="${yExpanded ? 'true' : 'false'}" onclick="toggleY('${escAttr(zk)}','${escAttr(yk)}')">
          <span class="nt-arrow${yExpanded ? ' open' : ''}">▶</span>
          <span class="nt-label" style="color:var(--y-color)">${yk}</span>
          <span class="nt-count">${scrolls.length}</span></button>`;
        html += `<div class="nt-children${yExpanded ? '' : ' collapsed'}" id="nt-y-${zyKey.replace(/[\/.]/g,'-')}">`;

        if (yExpanded) {
          html += `<div class="subspace-header d4"><span class="sh-dim">4D</span><span class="sh-desc">Y=${yk} · ${scrolls.length} scrolls</span></div>`;
          for (const s of scrolls) {
            const isCur = s.full === curCoord ? ' current' : '';
            html += `<button type="button" class="nt-scroll${isCur}" onclick="treeNav('${escAttr(s.full)}')">
              <span class="nt-coord">${s.x}</span></button>`;
          }
        }
        html += '</div>';
      }
    }
    html += '</div>';
  }

  $('navtree-panel').innerHTML = html;
}

function toggleZ(zk) {
  treeExpanded[zk] = !treeExpanded[zk];
  renderTree();
}

function toggleY(zk, yk) {
  const key = zk + '/' + yk;
  treeExpanded[key] = !treeExpanded[key];
  // Auto-expand parent Z
  if (!treeExpanded[zk]) treeExpanded[zk] = true;
  renderTree();
}

async function treeNav(coord) {
  render(await api('POST', '/api/goto', coord));
  if (drawerNarrow()) setDrawer(false);
}

function highlightTreeNode(coord) {
  // Auto-expand to show current coordinate
  const [z, y] = coord.split('/');
  if (z && !treeExpanded[z]) { treeExpanded[z] = true; }
  if (z && y && !treeExpanded[z + '/' + y]) { treeExpanded[z + '/' + y] = true; }
  renderTree();
}

// ── Navigation ──
async function selectDim(d) { render(await api('POST', '/api/dim', String(d))); }
async function moveForward() { render(await api('POST', '/api/forward')); }
async function moveBackward() { render(await api('POST', '/api/backward')); }
async function nextPop() { render(await api('POST', '/api/next')); }
async function prevPop() { render(await api('POST', '/api/prev')); }
async function jumpBase() { render(await api('POST', '/api/base')); showMsg('⌂ BASE'); }
async function gotoCoord(coord) { render(await api('POST', '/api/goto', coord)); }

// ── Edit ──
function enterEdit() {
  if (!lastNav || !lastNav.scroll) return;
  document.body.classList.remove('more-open');
  setMode('edit');
  $('edit-coord').textContent = lastNav.position.coordinate;
  $('edit-textarea').value = lastNav.scroll.content || '';
  $('edit-textarea').focus();
  setScrollTitle($('edit-textarea').value);
}

// UG5 mode for the current scroll: stash its text and coordinate for the
// UG5 page (same origin), then navigate there. The page reads and clears
// 'phext-edit:ug5-handoff' from localStorage and pre-fills its editor.
// Text source mirrors enterEdit: the live textarea while editing (so
// unsaved edits carry over), else the current scroll's content.
function openUG5() {
  if (!lastNav || !lastNav.scroll) return;
  const text = mode === 'edit' ? $('edit-textarea').value : (lastNav.scroll.content || '');
  try {
    localStorage.setItem('phext-edit:ug5-handoff', JSON.stringify({
      coordinate: lastNav.position.coordinate,
      text: text,
    }));
  } catch (e) { /* storage blocked: the UG5 page still opens, just empty */ }
  location.href = '/up-goer-five.html';
}
async function saveScroll() {
  if (demoMode) {
    const coord = lastNav.position.coordinate;
    const content = $('edit-textarea').value;
    demoEdits.set(coord, content);
    if (lastNav.scroll) lastNav.scroll.content = content;
    setMode('lattice');
    render(lastNav, true);
    showMsg('Kept in this tab only. Sign in to save your own files.');
    return;
  }
  const coord = lastNav.position.coordinate;
  const content = $('edit-textarea').value;
  const res = await api('POST', '/api/update', { coordinate: coord, content });
  if (res.ok) {
    dirty = true;
    showMsg('scroll saved to memory');
    setMode('lattice');
    // Refresh tree if it was loaded
    if (treeData) loadTree();
    render(await api('GET', '/api/nav'));
  } else { showMsg('save failed: ' + res.message); }
}
async function saveToDisk() {
  if (demoMode) {
    showMsg('Sign in to save your own files.');
    return;
  }
  const res = await api('POST', '/api/save');
  if (res.ok) { dirty = false; $('dirty-flag').textContent = ''; showMsg('💾 saved to disk'); }
  else { showMsg('disk save failed: ' + res.message); }
}
function cancelEdit() {
  setMode('lattice');
  if (lastNav && lastNav.scroll) setScrollTitle(lastNav.scroll.content);
}
$('edit-textarea').addEventListener('input', () => setScrollTitle($('edit-textarea').value));

// ── Search ──
async function doSearch(query) {
  const hits = await api('POST', '/api/search', query);
  const el = $('search-results');
  if (!hits.length) { el.innerHTML = '<div style="color:var(--dim);padding:8px">no results</div>'; return; }
  el.innerHTML = hits.map(h =>
    `<button type="button" class="hit" onclick="jumpToHit('${escAttr(h.coordinate)}')">
      <span class="hit-coord">${escHtml(h.coordinate)}</span>
      <span class="hit-ctx">${escHtml(h.context)}</span></button>`
  ).join('');
}
function jumpToHit(coord) { closeOverlays(); gotoCoord(coord); }

// ── Overlays ──
function openSearch() {
  document.body.classList.remove('more-open');
  mode = 'search'; $('search-overlay').className = 'overlay visible';
  $('search-input').value = ''; $('search-results').innerHTML = ''; $('search-input').focus();
}
function openGoto() {
  document.body.classList.remove('more-open');
  mode = 'goto'; $('goto-overlay').className = 'overlay visible';
  $('goto-input').value = ''; $('goto-input').focus();
}
function closeOverlays() {
  $('search-overlay').className = 'overlay';
  $('goto-overlay').className = 'overlay';
  $('phext-overlay').className = 'overlay';
  if (mode === 'search' || mode === 'goto' || mode === 'phext-pick') setMode('lattice');
}

// ── Helpers ──
function escHtml(s) { return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;'); }
function escAttr(s) { return s.replace(/'/g,"\\'").replace(/"/g,'&quot;'); }
function linkifyCoordinates(html) {
  const coordRe = /\b(\d{1,4}\.\d{1,4}\.\d{1,4}\/\d{1,4}\.\d{1,4}\.\d{1,4}\/\d{1,4}\.\d{1,4}\.\d{1,4})\b/g;
  return html.replace(coordRe, m => `<a class="coord-link" onclick="event.preventDefault();gotoCoord('${m}')" href="#">${m}</a>`);
}

// ── Keyboard ──
document.addEventListener('keydown', async (e) => {
  if (!demoMode && $('auth-screen').style.display !== 'none') return;
  if ((e.ctrlKey || e.metaKey) && e.key === 's') {
    e.preventDefault();
    if (mode === 'edit') { await saveScroll(); await saveToDisk(); }
    else if (dirty) { await saveToDisk(); }
    return;
  }
  if (mode === 'edit') { if (e.key === 'Escape') { cancelEdit(); e.preventDefault(); } return; }
  if (mode === 'search') { if (e.key === 'Escape') { closeOverlays(); e.preventDefault(); } return; }
  if (mode === 'phext-pick') { if (e.key === 'Escape') { closeOverlays(); e.preventDefault(); } return; }
  if (mode === 'goto') {
    if (e.key === 'Escape') { closeOverlays(); e.preventDefault(); }
    if (e.key === 'Enter') {
      const v = $('goto-input').value.trim();
      if (v) { closeOverlays(); await gotoCoord(v); }
      e.preventDefault();
    }
    return;
  }

  if (e.key === 'Escape' && $('left-panel').classList.contains('drawer-open')) {
    closeDrawer();
    e.preventDefault();
    return;
  }
  const focusTag = (e.target && e.target.tagName) || '';
  if (focusTag === 'INPUT' || focusTag === 'TEXTAREA' || focusTag === 'BUTTON' || focusTag === 'A' || focusTag === 'SELECT') return;

  const k = e.key;
  if (k >= '1' && k <= '9') { await selectDim(parseInt(k)); e.preventDefault(); }
  else if (k === 'l' || k === 'ArrowRight') { await moveForward(); e.preventDefault(); }
  else if (k === 'h' || k === 'ArrowLeft') { await moveBackward(); e.preventDefault(); }
  else if (k === 'j' || k === 'ArrowDown') {
    if (e.shiftKey) { for (let i=0;i<10;i++) await nextPop(); } else { await nextPop(); }
    e.preventDefault();
  }
  else if (k === 'k' || k === 'ArrowUp') {
    if (e.shiftKey) { for (let i=0;i<10;i++) await prevPop(); } else { await prevPop(); }
    e.preventDefault();
  }
  else if (k === 'J') { for (let i=0;i<10;i++) await nextPop(); e.preventDefault(); }
  else if (k === 'K') { for (let i=0;i<10;i++) await prevPop(); e.preventDefault(); }
  else if (k === '/' || (k === 'f' && e.ctrlKey)) { openSearch(); e.preventDefault(); }
  else if (k === 'g') { openGoto(); e.preventDefault(); }
  else if (k === 'e' || k === 'Enter') { enterEdit(); e.preventDefault(); }
  else if (k === 'u') { openUG5(); e.preventDefault(); }
  else if (k === 't') { showPanel(activePanel === 'navtree' ? 'sentron' : 'navtree'); e.preventDefault(); }
  else if (k === 'Home') { await jumpBase(); e.preventDefault(); }
});

$('search-input').addEventListener('input', e => {
  const q = e.target.value.trim();
  if (q.length >= 2) doSearch(q); else $('search-results').innerHTML = '';
});
$('search-input').addEventListener('keydown', e => {
  if (e.key === 'Enter') { const q = e.target.value.trim(); if (q) doSearch(q); e.preventDefault(); }
});

function scheduleFollow() {
  if (urlSync) { urlSync = false; return; }
  if (scheduleFollow.queued) return;
  scheduleFollow.queued = true;
  setTimeout(async () => {
    scheduleFollow.queued = false;
    if ($('auth-screen').style.display === 'flex') return;
    try { await openFromLocation(true); }
    catch (e) { if (!needsAuth) console.error(e); }
  }, 0);
}
window.addEventListener('hashchange', scheduleFollow);
window.addEventListener('popstate', scheduleFollow);


// ── Auth ──
// Sign-in state lives in an HttpOnly cookie; /api/session reports it and the CSRF token.
async function loadSession() {
  // Access-token sign-in was retired (2026-11-08): forget any token an
  // older version of this page saved, so it can never be sent again.
  localStorage.removeItem('phext-token');
  try { session = await (await fetch('/api/session')).json(); } catch { session = null; }
  csrf = (session && session.csrf) || '';
  const so = $('signout');
  if (session && session.signed_in) {
    so.textContent = '⏏ sign out' + (session.handle ? ' ' + session.handle : '');
    so.title = 'Sign out';
    so.style.display = '';
  } else {
    so.style.display = 'none';
  }
  const dl = $('dashboard-link');
  if (dl) dl.style.display = (session && session.signed_in &&
    (session.editor || ((session.who || '').startsWith('x:') && (session.handle || '').toLowerCase() === 'wbic16'))) ? '' : 'none';
}
function paintProviders() {
  if (!session) return;
  const known = session.providers || [];
  // A `?next=` in the query wins: the Mutable SSO handoff sends signed-out
  // visitors here with its endpoint as the next. Otherwise return to the
  // scroll in the hash. Same-site paths only — safe_next re-checks server-side.
  const fromQuery = new URLSearchParams(location.search).get('next');
  const nextPath = fromQuery && fromQuery.startsWith('/') && !fromQuery.includes(':') ? fromQuery : '/' + location.hash;
  const next = encodeURIComponent(nextPath);
  document.querySelectorAll('#auth-providers [data-provider]').forEach(el => {
    const provider = el.getAttribute('data-provider');
    el.style.display = known.indexOf(provider) >= 0 ? '' : 'none';
    if (provider) el.setAttribute('href', '/api/auth/' + provider + '?next=' + next);
  });
}
async function showAuthScreen(message, sessionFresh) {
  if (!sessionFresh) await loadSession();
  const s = session || {};
  let html = '';
  if (s.signed_in) {
    html += `<div style="color:var(--text)">Signed in as ${escHtml(s.name || s.handle)} <span style="color:var(--dim)">(${escHtml(s.who)})</span></div>`;
    html += `<button type="button" class="auth-btn" onclick="signOut()">Sign out</button>`;
  } else {
    // A `?next=` in the query wins: the Mutable SSO handoff sends signed-out
    // visitors here with its endpoint as the next, so sign-in returns them
    // to it. Otherwise return to this phext and coordinate. Same-site paths
    // only — authorize_url's safe_next re-checks server-side.
    const fromQuery = new URLSearchParams(location.search).get('next');
    const here = location.pathname + location.search + location.hash;
    const nextPath = fromQuery && fromQuery.startsWith('/') && !fromQuery.includes(':') ? fromQuery : here;
    const next = encodeURIComponent(nextPath);
    const label = { github: 'Sign in with GitHub', google: 'Sign in with Google', x: 'Sign in with X' };
    for (const p of (s.providers || []))
      html += `<a class="auth-btn" href="/api/auth/${p}?next=${next}">${label[p] || p}</a>`;
  }
  $('auth-providers').innerHTML = html;
  $('auth-prompt').textContent =
    s.signed_in && !s.editor ? 'This account is not on the editor list.'
    : s.ready ? 'Sign in to continue'
    : 'Sign-in is not configured on this server';
  $('auth-error').textContent = message || '';
  $('auth-screen').style.display = 'flex';
  setBackgroundInert(true);
}
function setBackgroundInert(on) {
  document.querySelectorAll('body > :not(#auth-screen)').forEach(el => { el.inert = on; });
}
function hideAuthScreen() {
  $('auth-screen').style.display = 'none';
  setBackgroundInert(false);
  setDrawer($('left-panel').classList.contains('drawer-open'));
}
async function signOut() {
  if (session && session.signed_in) {
    try { await fetch('/api/auth/logout', { method: 'POST', headers: { 'x-csrf-token': csrf } }); } catch {}
  }
  location.reload();
}

// ── Phext picker ──
async function openPhextPicker() {
  try {
    const files = await api('GET', '/api/phexts');
    let html = '';
    for (const f of files) {
      const kb = (f.bytes / 1024).toFixed(1);
      const stem = fileStem(f.name);
      const isCurrent = lastNav && f.path === lastNav.position?.file;
      const href = (PHEXT_NAME_RE.test(stem) && !RESERVED_PHEXT[stem]) ? '/' + stem + '/' : '#';
      html += `<a class="hit" href="${escAttr(href)}"${isCurrent ? ' aria-current="true"' : ''} onclick="event.preventDefault();switchPhext('${escAttr(f.name)}','${escAttr(f.path)}')">
        <span class="hit-coord">${escHtml(f.name)}</span>
        <span class="hit-ctx">${kb} KB</span></a>`;
    }
    $('phext-list').innerHTML = html || '<div style="color:var(--dim);padding:8px">no phext files</div>';
    mode = 'phext-pick';
    $('phext-overlay').className = 'overlay visible';
  } catch(e) { showMsg('failed to list phexts'); }
}
async function switchPhext(name, path) {
  try {
    const stem = fileStem(name);
    const target = (PHEXT_NAME_RE.test(stem) && !RESERVED_PHEXT[stem]) ? stem : path;
    const res = await api('POST', '/api/open', target);
    if (res.ok) {
      closeOverlays();
      treeData = null;
      treeExpanded = {};
      const nav = await api('GET', '/api/nav');
      $('active-file').textContent = '📂 ' + (name || stem);
      $('active-file').setAttribute('aria-label', 'Switch phext, current file ' + (name || stem));
      if (PHEXT_NAME_RE.test(stem) && !RESERVED_PHEXT[stem]) {
        const url = '/' + stem + '/' + location.search + '#' + nav.position.coordinate;
        history.pushState(null, '', url);
        urlReady = true;
      }
      render(nav, true);
      if (activePanel === 'navtree') loadTree();
      await initApp();
      showMsg('📂 ' + res.message);
    } else { showMsg(res.message); }
  } catch(e) { showMsg('failed to open'); }
}

async function ensurePhext(name) {
  const status = await api('GET', '/api/status');
  const current = fileStem(status.file);
  // `/` is the startup (or --home) phext, not whichever file was opened last.
  if (status.default_file) { defaultPath = status.default_file; defaultStem = fileStem(status.default_file); }
  else if (!defaultPath) { defaultPath = status.file; defaultStem = current; }
  const want = name || defaultStem;
  let switched = false;
  if (current !== want) {
    const body = (PHEXT_NAME_RE.test(want) && !RESERVED_PHEXT[want]) ? want : defaultPath;
    const res = await api('POST', '/api/open', body);
    if (!res.ok) { showMsg(res.message || 'phext not found'); return false; }
    treeData = null;
    treeExpanded = {};
    switched = true;
  }
  $('active-file').textContent = '📂 ' + want + '.phext';
  $('active-file').setAttribute('aria-label', 'Switch phext, current file ' + want + '.phext');
  return switched ? 'switched' : 'same';
}

async function openFromLocation(fromHistory) {
  const name = phextNameFromLocation();
  const parsed = readFragment();
  const opened = await ensurePhext(name);
  if (!opened) return;
  if (parsed.present) {
    const canonical = '#' + parsed.coord;
    if (location.hash !== canonical) {
      history.replaceState(null, '', sharePath(name) + location.search + canonical);
    }
    if (opened === 'same' && lastNav && lastNav.position.coordinate === parsed.coord) return;
    render(await api('POST', '/api/goto', parsed.coord), true);
  } else if (!name && !fromHistory) render(await api('POST', '/api/home'));
  else render(await api('GET', '/api/nav'));
}

async function initApp() {
  try { await openFromLocation(false); } catch { /* auth will catch */ }
}

const DEMO_OPS = {
  '/api/dim': 'dim', '/api/forward': 'forward', '/api/backward': 'backward',
  '/api/next': 'next', '/api/prev': 'prev', '/api/goto': 'goto',
  '/api/home': 'home', '/api/base': 'base', '/api/nav': 'stay',
};
async function demoGet(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error('demo unavailable');
  return res.json();
}
async function demoApi(method, path, body) {
  if (path === '/api/tree') return demoGet('/api/demo/tree');
  if (path === '/api/status') return demoGet('/api/demo/status');
  if (path === '/api/phexts') return [{ name: 'demo.phext', path: 'demo.phext', bytes: 0 }];
  if (path === '/api/open') return { ok: true, message: 'opened demo.phext' };
  if (path === '/api/update' || path === '/api/save')
    return { ok: false, message: 'Sign in to save your own files.' };
  if (path === '/api/search') {
    const q = encodeURIComponent(body == null ? '' : String(body));
    return demoGet('/api/demo/search?q=' + q);
  }
  const op = DEMO_OPS[path] || 'stay';
  const to = (op === 'goto' || op === 'dim') && body != null ? String(body) : '';
  const url = '/api/demo/nav?at=' + encodeURIComponent(demoAt)
    + '&dim=' + encodeURIComponent(String(demoDim))
    + '&op=' + op
    + (to ? '&to=' + encodeURIComponent(to) : '');
  const res = await fetch(url);
  if (!res.ok) throw new Error('demo unavailable');
  const nav = await res.json();
  if (nav.position) {
    demoAt = nav.position.coordinate;
    demoDim = nav.position.dimension_index;
  }
  return nav;
}
function coordHash() {
  const hash = location.hash.replace(/^#/, '');
  return /^\d+\.\d+\.\d+\/\d+\.\d+\.\d+\/\d+\.\d+\.\d+$/.test(hash) ? hash : '';
}
async function startDemo() {
  if (demoMode) return;
  demoMode = true;
  $('auth-screen').style.display = 'none';
  $('demo-banner').style.display = 'flex';
  if (session && session.editor) {
    const sign = $('demo-signin');
    if (sign) sign.style.display = 'none';
    $('demo-leave').style.display = '';
  }
  try {
    const start = coordHash() || '1.1.1/1.1.1/1.1.1';
    demoAt = start;
    demoDim = 1;
    render(await api('POST', '/api/goto', start));
    initApp();
  } catch (e) {
    demoMode = false;
    $('demo-banner').style.display = 'none';
    $('auth-screen').style.display = 'flex';
    $('auth-error').textContent = 'The demo did not load. You can still sign in.';
    if (!needsAuth) console.error(e);
  }
}
function bootEditor() {
  $('auth-screen').style.display = 'none';
  $('demo-banner').style.display = 'none';
  const hash = coordHash();
  const load = hash ? api('POST', '/api/goto', hash) : api('POST', '/api/home');
  return load.then(nav => { render(nav); initApp(); });
}

// Initial load. Signed-out visitors stay on the intro and do not call the editor API.
window.addEventListener('popstate', () => {
  if (demoMode && !new URLSearchParams(location.search).has('demo')) location.reload();
});
$('try-demo').addEventListener('click', (e) => {
  e.preventDefault();
  if (!new URLSearchParams(location.search).has('demo'))
    history.pushState(null, '', '/?demo' + location.hash);
  startDemo();
});
(async () => {
  try {
    await loadSession();
    if (wantDemo) { await startDemo(); return; }
    const signedIn = session && (session.editor || !session.locked);
    if (signedIn) { await bootEditor(); return; }
    if (session && session.signed_in && !session.editor) {
      needsAuth = true;
      await showAuthScreen('', true);
      return;
    }
    paintProviders();
  } catch (e) {
    if (!needsAuth) console.error(e);
  }
})();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod public_route_tests {
    use super::{
        dashboard_html, handle_request, is_api_path, is_editor_shell, public_document, AppState,
        DemoCatalog, ABOUT_HTML, BACKGROUND_WEBP, BANNER_WEBP, CLAIM_HTML, EXPLORE_HTML, INDEX_HTML,
        LEARN_YOU_PHEXT_HTML, NOT_FOUND_HTML, PREVIEWER_HTML, PRIVACY_HTML, ROBOTS_TXT, SHELL_CACHE,
        SINGULAR_HTML, SITEMAP_XML, STATIC_CACHE, SUBMIT_HTML, TERMS_HTML, TTS_DEMO_HTML,
        UP_GOER_FIVE_HTML, DEFAULT_COORD,
    };
    use phext_lattice::oauth::{Editors, Gate, Oauth};
    use phext_lattice::sponsor::{self, SponsorOffer};
    use phext_lattice::MappedLattice;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct HttpResponse {
        status: u16,
        head: String,
        content_type: String,
        body: String,
    }

    fn locked_gate() -> Arc<Gate> {
        Arc::new(Gate::new(Oauth::test_ready(), Editors::default()))
    }

    fn open_gate() -> Arc<Gate> {
        Arc::new(Gate::new(Oauth::closed(), Editors::default()))
    }

    fn http(gate: &Arc<Gate>, request: &str) -> HttpResponse {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(AppState::new(
            MappedLattice::from_bytes(b"scroll one"),
            "mem.phext".into(),
        )));
        let gate = Arc::clone(gate);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            handle_request(&mut stream, &state, &gate, &DemoCatalog::load());
        });
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        client.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        client.write_all(request.as_bytes()).unwrap();
        let _ = client.shutdown(std::net::Shutdown::Write);
        let mut raw = Vec::new();
        client.read_to_end(&mut raw).unwrap();
        server.join().expect("server thread");
        let raw = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let status_line = head.lines().next().unwrap_or("");
        let status = status_line.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
        let content_type = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-type") {
                    Some(value.trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        HttpResponse { status, head: head.to_string(), content_type, body: body.to_string() }
    }

    fn header_line<'a>(head: &'a str, name: &str) -> &'a str {
        head.lines().find_map(|line| {
            let (n, value) = line.split_once(':')?;
            if n.eq_ignore_ascii_case(name) {
                Some(value.trim())
            } else {
                None
            }
        }).unwrap_or("")
    }

    fn get(gate: &Arc<Gate>, path: &str) -> HttpResponse {
        http(gate, &format!("GET {path} HTTP/1.1\r\nHost: edit.phext.io\r\nConnection: close\r\n\r\n"))
    }

    #[test]
    fn public_pages_name_each_other_and_the_contact() {
        for page in [ABOUT_HTML, PRIVACY_HTML] {
            assert!(page.contains("href=\"terms\""), "missing terms link");
        }
        // The shell is also served under /<name>/, so its links are absolute.
        assert!(INDEX_HTML.contains("href=\"/terms\""));
        assert!(INDEX_HTML.contains("fetch('/api/session')"));
        assert!(INDEX_HTML.contains("href=\"/api/auth/"));
        assert!(INDEX_HTML.contains("history.replaceState"));
        assert!(INDEX_HTML.contains("history.pushState"));
        assert!(INDEX_HTML.contains("hashchange"));
        assert!(INDEX_HTML.contains("popstate"));
        assert!(INDEX_HTML.contains(DEFAULT_COORD));
        assert!(INDEX_HTML.contains("<title>Phext Edit: an editor for 11-dimensional plain text</title>"));
        assert!(INDEX_HTML.contains(" | Phext Edit"));
        assert!(INDEX_HTML.contains("function scrollTitle"));
        assert!(INDEX_HTML.contains("#phext-list .hit"));
        assert!(INDEX_HTML.contains("#phext-list .hit .hit-coord"));
        assert!(INDEX_HTML.contains("<a class=\"hit\""));
        assert!(INDEX_HTML.contains("new TextEncoder()"));
        assert!(TERMS_HTML.contains("href=\"about\""));
        assert!(TERMS_HTML.contains("href=\"privacy\""));
        assert!(TERMS_HTML.contains("will+support@phext.io"));
        assert!(TERMS_HTML.contains("Google"));
        assert!(TERMS_HTML.contains("GitHub"));
        assert!(TERMS_HTML.contains("X account"));
        assert!(PRIVACY_HTML.contains("sign in with <b>X</b>"));
        assert!(PRIVACY_HTML.contains("does not read your timeline"));
        assert!(ABOUT_HTML.contains("Google, GitHub or X account"));
        assert!(ABOUT_HTML.contains("<b>X</b> sign-in"));
        assert!(TERMS_HTML.contains("does not show advertisements"));
        assert!(!TERMS_HTML.to_ascii_lowercase().contains("legal review"));
        assert!(NOT_FOUND_HTML.contains("href=\"/\""));
    }

    #[test]
    fn crawler_files_use_the_canonical_host() {
        const ORIGIN: &str = "https://edit.phext.io";
        assert!(ROBOTS_TXT.contains(ORIGIN));
        assert!(SITEMAP_XML.contains(ORIGIN));
        assert!(ROBOTS_TXT.starts_with("User-agent: *\nDisallow: /api\n"));
        for path in ["/", "/index.html", "/about", "/privacy", "/terms"] {
            let line = if path == "/" {
                "Allow: /\n".to_string()
            } else {
                format!("Allow: {path}\n")
            };
            assert!(ROBOTS_TXT.contains(&line), "robots missing {line}");
        }
        assert!(ROBOTS_TXT.contains(&format!("Sitemap: {ORIGIN}/sitemap.xml\n")));
        assert!(!ROBOTS_TXT.contains("Disallow: /about"));
        assert!(!ROBOTS_TXT.contains("Disallow: /\n"));
        for path in ["/", "/about", "/privacy", "/terms"] {
            assert!(SITEMAP_XML.contains(&format!("{ORIGIN}{path}</loc>")));
        }
        assert!(!SITEMAP_XML.contains("/api"));
        assert_eq!(public_document("/robots.txt?from=crawler").unwrap().body, ROBOTS_TXT);
        assert!(public_document("/nope").is_none());
        assert!(is_api_path("/api/nav") && is_api_path("/api") && is_api_path("/api/auth/github?next=/"));
        assert!(!is_api_path("/apples") && !is_api_path("/about"));
        assert!(is_editor_shell("/?utm=1") && is_editor_shell("/index.html"));
        assert!(!is_editor_shell("/terms"));
    }

    #[test]
    fn signed_out_visitors_can_read_every_public_page() {
        let gate = locked_gate();
        let pages = [
            ("/", "text/html", "phext-edit"),
            ("/index.html", "text/html", "phext-edit"),
            ("/about", "text/html", "Phext Edit"),
            ("/privacy", "text/html", "Privacy Policy"),
            ("/terms", "text/html", "Terms of Service"),
            ("/terms?ref=about", "text/html", "will+support@phext.io"),
            ("/claim", "text/html", "Claim your space"),
            ("/robots.txt", "text/plain", "Sitemap: https://edit.phext.io/sitemap.xml"),
            ("/sitemap.xml", "application/xml", "https://edit.phext.io/terms"),
            ("/?utm=1", "text/html", "phext-edit"),
            ("/?demo", "text/html", "phext-edit"),
            ("/index.html?x=1", "text/html", "phext-edit"),
        ];
        for (path, kind, marker) in pages {
            let res = get(&gate, path);
            assert_eq!(res.status, 200, "{path} status {}", res.status);
            assert!(res.content_type.starts_with(kind), "{path} type {}", res.content_type);
            assert!(res.body.contains(marker), "{path} body missing {marker}");
            assert!(!res.body.contains("\"error\":\"unauthorized\""), "{path} was refused");
            assert!(res.head.contains("ETag: \""), "{path} missing etag");
            assert!(res.head.contains("Content-Security-Policy: "), "{path}");
            assert!(!res.head.to_ascii_lowercase().contains("access-control-allow-origin"), "{path}");
            assert!(!res.head.contains("Strict-Transport-Security"), "{path}");
            let cache = header_line(&res.head, "cache-control");
            if path.ends_with(".txt") || path.ends_with(".xml") {
                assert!(cache.contains(STATIC_CACHE), "{path} {cache}");
            } else {
                assert!(cache.contains(SHELL_CACHE), "{path} {cache}");
            }
        }
        let ico = get(&gate, "/favicon.ico");
        assert_eq!(ico.status, 200);
        assert!(ico.content_type.starts_with("image/x-icon"));
        assert!(header_line(&ico.head, "cache-control").contains(STATIC_CACHE));
        assert!(ico.head.contains("ETag:"));
        assert!(!ico.head.to_ascii_lowercase().contains("access-control-allow-origin"));
        let svg = get(&gate, "/favicon.svg?v=1");
        assert_eq!(svg.status, 200);
        assert!(svg.content_type.starts_with("image/svg+xml"));
        assert!(svg.body.contains("<svg"));
        let og = get(&gate, "/og.png?x=1");
        assert_eq!(og.status, 200);
        assert!(og.content_type.starts_with("image/png"));
        assert!(header_line(&og.head, "cache-control").contains(STATIC_CACHE));
        assert!(og.head.contains("ETag:"));
        assert!(og.head.contains("img-src 'self'"));
        assert!(!og.head.to_ascii_lowercase().contains("access-control-allow-origin"));
        let apple = get(&gate, "/apple-touch-icon.png");
        assert_eq!(apple.status, 200);
        assert!(apple.content_type.starts_with("image/png"));
        assert!(header_line(&apple.head, "cache-control").contains(STATIC_CACHE));
        // The shell artwork rides the same pre-gate route as the favicon:
        // compiled in, image/webp, day-long static caching with an ETag.
        let banner = get(&gate, "/banner.webp");
        assert_eq!(banner.status, 200);
        assert!(banner.content_type.starts_with("image/webp"));
        assert!(header_line(&banner.head, "cache-control").contains(STATIC_CACHE));
        assert!(banner.head.contains("ETag:"));
        assert_eq!(header_line(&banner.head, "content-length"), BANNER_WEBP.len().to_string());
        let backdrop = get(&gate, "/background.webp");
        assert_eq!(backdrop.status, 200);
        assert!(backdrop.content_type.starts_with("image/webp"));
        assert!(header_line(&backdrop.head, "cache-control").contains(STATIC_CACHE));
        assert_eq!(header_line(&backdrop.head, "content-length"), BACKGROUND_WEBP.len().to_string());
        assert!(INDEX_HTML.contains("src=\"/banner.webp\""), "sign-in banner wired");
        assert!(INDEX_HTML.contains("url(\"/background.webp\")"), "sign-in backdrop wired");
        let shell = get(&gate, "/");
        assert!(shell.body.contains("property=\"og:title\""));
        assert!(shell.body.contains("name=\"twitter:card\" content=\"summary_large_image\""));
        assert!(shell.body.contains("rel=\"canonical\" href=\"https://edit.phext.io/\""));
        assert!(shell.body.contains("name=\"theme-color\" content=\"#171717\""));
        assert!(shell.head.contains("img-src 'self'"));
        let etag = header_line(&shell.head, "etag").trim();
        let again = http(
            &gate,
            &format!("GET / HTTP/1.1\r\nHost: edit.phext.io\r\nIf-None-Match: {etag}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(again.status, 304, "{}", again.head);
        assert!(again.body.is_empty());
        let https = http(
            &gate,
            "GET /terms HTTP/1.1\r\nHost: edit.phext.io\r\nX-Forwarded-Proto: https\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(https.status, 200);
        assert!(https.head.contains("Strict-Transport-Security: max-age=31536000"));
        assert!(https.head.contains("upgrade-insecure-requests"));
        let head = http(&gate, "HEAD /terms HTTP/1.1\r\nHost: edit.phext.io\r\nConnection: close\r\n\r\n");
        assert_eq!(head.status, 200);
        assert!(head.body.is_empty());
        assert!(head.head.contains("ETag:"));
        let post = http(&gate, "POST /terms HTTP/1.1\r\nHost: edit.phext.io\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        assert_eq!(post.status, 405);
        assert!(!post.body.contains("unauthorized"));
        assert!(!post.head.to_ascii_lowercase().contains("access-control-allow-origin"));
    }

    #[test]
    fn submit_page_is_public_and_submissions_need_sign_in() {
        let gate = locked_gate();
        let page = get(&gate, "/submit");
        assert_eq!(page.status, 200);
        assert!(page.content_type.contains("text/html"), "{}", page.content_type);
        assert!(page.body.contains("Upload a phext for review"), "{}", page.body);
        assert!(page.body.contains("your account becomes an editor"), "{}", page.body);

        let post = http(
            &gate,
            "POST /api/submissions HTTP/1.1\r\nHost: edit.phext.io\r\nX-Filename: x.phext\r\nContent-Length: 5\r\n\r\nhello",
        );
        assert_eq!(post.status, 401);
        assert!(post.content_type.contains("application/json"), "{}", post.content_type);
        assert_eq!(get(&gate, "/api/submissions").status, 401);
        assert_eq!(get(&gate, "/api/submissions/1-0/download").status, 401);
    }

    #[test]
    fn claim_page_and_lookup_are_public_and_the_shell_links_claim() {
        let gate = locked_gate();
        let page = get(&gate, "/claim");
        assert_eq!(page.status, 200);
        assert!(page.content_type.contains("text/html"), "{}", page.content_type);
        assert!(page.body.contains("Claim your space"), "{}", page.body);
        assert!(page.body.contains("Are you in the map?"), "{}", page.body);

        // The lookup answers a signed-out visitor with the snapshot's data.
        let found = get(&gate, "/api/claim/lookup?handle=%40eigenhector");
        assert_eq!(found.status, 200, "{}", found.head);
        assert!(found.body.contains("\"found\":true"), "{}", found.body);
        assert!(found.body.contains("4.5.5/2.4.5/4.4.4"), "{}", found.body);
        // Claiming itself is closed on a locked gate (no claim mode, no
        // session): 403, never a silent success.
        let post = http(
            &gate,
            "POST /api/claim HTTP/1.1\r\nHost: edit.phext.io\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(post.status, 403);

        // Discoverability: the surfaces a signed-in user actually lands on
        // link /claim — the editor shell (status bar + sign-in screen),
        // the About page, and the sitemap. Not just the sitemap.
        assert!(INDEX_HTML.contains("class=\"bar-link\" href=\"/claim\""), "shell status bar link");
        assert!(INDEX_HTML.contains(">Claim your space</a>"), "shell sign-in screen link");
        assert!(ABOUT_HTML.contains("href=\"claim\""), "about link");
        assert!(SITEMAP_XML.contains("https://edit.phext.io/claim</loc>"), "sitemap entry");
    }

    #[test]
    fn explore_page_and_its_data_are_public_and_linked() {
        let gate = locked_gate();
        let page = get(&gate, "/explore");
        assert_eq!(page.status, 200);
        assert!(page.content_type.contains("text/html"), "{}", page.content_type);
        assert!(page.body.contains("Explore the Wavefront"), "{}", page.body);
        // The video slot lives on /claim until the YouTube upload replaces it.
        let claim = get(&gate, "/claim");
        assert!(claim.body.contains("Watch the front move"), "{}", claim.body);
        assert!(claim.body.contains("video-slot"), "video placeholder");

        // The explorer's export: every placement, generated from the phext.
        let data = get(&gate, "/wavefront-explorer.json");
        assert_eq!(data.status, 200);
        assert!(data.content_type.contains("application/json"), "{}", data.content_type);
        let parsed: serde_json::Value = serde_json::from_str(&data.body).expect("explorer export is JSON");
        assert_eq!(parsed["meta"]["placements"], 522);
        assert_eq!(parsed["entries"].as_array().map(|a| a.len()), Some(522));
        assert!(data.body.contains("@eigenhector"), "named entries ride along");

        // Same discoverability surfaces as /claim.
        assert!(INDEX_HTML.contains("class=\"bar-link\" href=\"/explore\""), "shell status bar link");
        assert!(INDEX_HTML.contains("Explore the map"), "shell sign-in screen link");
        assert!(ABOUT_HTML.contains("href=\"explore\""), "about link");
        assert!(SITEMAP_XML.contains("https://edit.phext.io/explore</loc>"), "sitemap entry");
    }

    #[test]
    fn mutable_os_is_linked_from_the_shell_status_bar() {
        // The crossing's front door: the editor shell a signed-in user
        // lands on links the SSO handoff, beside Submit/Claim/Explore —
        // and the sign-in screen honours the ?next= the handoff sends
        // signed-out visitors back with.
        assert!(
            INDEX_HTML.contains("class=\"bar-link\" href=\"/api/auth/sso/mutable\""),
            "shell status bar link"
        );
        assert!(INDEX_HTML.contains(">Mutable OS</a>"), "link label");
        assert!(
            INDEX_HTML.contains("URLSearchParams(location.search).get('next')"),
            "auth screen reads ?next="
        );
    }

    #[test]
    fn singular_page_is_public_inline_and_linked() {
        let gate = locked_gate();
        let page = get(&gate, "/singular.html");
        assert_eq!(page.status, 200);
        assert!(page.content_type.contains("text/html"), "{}", page.content_type);
        assert!(page.body.contains("Singular"), "{}", page.body);
        assert!(page.body.contains("Read/Write"), "the three modes are named");

        // The page is a map of CYOA and loads everything inline — the
        // three lists and the coordinate map ride in the page body, so a
        // known word and its CYOA coordinate appear in the HTML itself.
        assert!(page.body.contains("var SINGDATA"), "inline data block");
        assert!(page.body.contains("phext"), "his words are inlined");
        assert!(page.body.contains("42.12.13/14.13.17/15.12.14"),
            "phext's CYOA coordinate is inlined");
        assert!(page.body.contains("holographic positioning"), "the map is explained");

        // The old separate word-list routes are gone: the page must not
        // request them, and nothing else should find them either.
        for path in ["/singular-words-read.txt", "/singular-words.txt",
                     "/singular-words-authored.txt"] {
            let res = get(&gate, path);
            assert_eq!(res.status, 404, "{path} is no longer a public document");
        }

        // Discoverability: the About page's utility list, the editor shell's
        // sign-in screen, and the sitemap.
        assert!(ABOUT_HTML.contains("href=\"singular.html\""), "about link");
        assert!(INDEX_HTML.contains("href=\"/singular.html\""), "shell sign-in link");
        assert!(SITEMAP_XML.contains("https://edit.phext.io/singular.html</loc>"), "sitemap entry");
        assert!(!SITEMAP_XML.contains("singular-words"), "no word-list entries in the sitemap");

        // Cross-links: the sibling utility pages link back to Singular.
        assert!(UP_GOER_FIVE_HTML.contains("href=\"singular.html\""), "up goer five cross-link");
        assert!(TTS_DEMO_HTML.contains("href=\"singular.html\""), "tts demo cross-link");
        assert!(LEARN_YOU_PHEXT_HTML.contains("href=\"singular.html\""), "learn you phext cross-link");
    }

    #[test]
    fn previewer_page_is_public_and_linked() {
        let gate = locked_gate();
        let page = get(&gate, "/phext-previewer.html");
        assert_eq!(page.status, 200);
        assert!(page.content_type.contains("text/html"), "{}", page.content_type);
        assert!(page.body.contains("Phext Previewer"), "{}", page.body);
        // The file is parsed on the visitor's device: the page carries
        // the delimiter table and makes no request of its own for one.
        assert!(page.body.contains("DELIM_TO_LEVEL"), "the phext parser rides in the page");
        assert!(page.body.contains("never leaves your device"), "the no-upload promise is stated");

        // Discoverability: the surfaces a visitor actually lands on link
        // it — the editor shell (sign-in utilities row + status bar), the
        // About page's utility list, and the sitemap.
        assert!(INDEX_HTML.contains("href=\"/phext-previewer.html\""), "shell links");
        assert!(INDEX_HTML.contains("Preview a phext</a>"), "shell status bar link");
        assert!(ABOUT_HTML.contains("href=\"phext-previewer.html\""), "about link");
        assert!(SITEMAP_XML.contains("https://edit.phext.io/phext-previewer.html</loc>"), "sitemap entry");

        // Cross-links: the sibling utility pages link to the previewer,
        // the way they cross-link each other.
        assert!(UP_GOER_FIVE_HTML.contains("href=\"/phext-previewer.html\""), "up goer five cross-link");
        assert!(TTS_DEMO_HTML.contains("href=\"/phext-previewer.html\""), "tts demo cross-link");
        assert!(LEARN_YOU_PHEXT_HTML.contains("href=\"/phext-previewer.html\""), "learn you phext cross-link");
    }

    #[test]
    fn shell_and_public_pages_carry_small_screen_rules() {
        // The mobile audit (2026-11-07): the shell had no @media rules at
        // all — a fixed 320px panel and a no-wrap status bar at phone
        // width. These assertions keep the small-screen rules in place.
        // Phone layout is the 760px drawer. The previewer page keeps its own 700px rules.
        assert!(INDEX_HTML.contains("@media (max-width: 760px)"), "shell small-screen rules");
        assert!(INDEX_HTML.contains("drawer-open"), "panel is a drawer on phones");
        assert!(PREVIEWER_HTML.contains("@media (max-width: 700px)"), "previewer small-screen rules");
        assert!(ABOUT_HTML.contains("@media (max-width: 760px)"), "public pages small-screen rules");
    }

    #[test]
    fn unknown_paths_are_an_html_404_with_a_link_home() {
        let gate = locked_gate();
        for path in ["/no-such-page", "/foo/bar", "/missing?x=1", "/terms/"] {
            let res = get(&gate, path);
            assert_eq!(res.status, 404, "{path}");
            assert!(res.content_type.starts_with("text/html"), "{path}");
            assert!(res.body.contains("href=\"/\""), "{path} has no home link");
            assert!(res.body.contains("Page not found"), "{path}");
            assert!(!res.body.contains("unauthorized"), "{path}");
            assert!(header_line(&res.head, "cache-control").contains("no-store"), "{path}");
            assert!(!res.head.contains("ETag:"), "{path}");
            assert!(res.head.contains("Content-Security-Policy: "), "{path}");
            assert!(!res.head.to_ascii_lowercase().contains("access-control-allow-origin"), "{path}");
            assert!(!res.head.contains("Strict-Transport-Security"), "{path}");
        }
        let head = http(&gate, "HEAD /no-such-page HTTP/1.1\r\nHost: edit.phext.io\r\nConnection: close\r\n\r\n");
        assert_eq!(head.status, 404);
        assert!(head.content_type.starts_with("text/html"));
        assert!(head.body.is_empty());
        let https = http(
            &gate,
            "GET /no-such-page HTTP/1.1\r\nHost: edit.phext.io\r\nX-Forwarded-Proto: https\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(https.status, 404);
        assert!(https.head.contains("Strict-Transport-Security: max-age=31536000"));
        assert!(https.body.contains("Page not found"));
    }

    #[test]
    fn api_paths_still_return_json_and_bearer_headers_no_longer_admit() {
        let gate = locked_gate();
        let denied = get(&gate, "/api/nav");
        assert_eq!(denied.status, 401);
        assert!(denied.content_type.starts_with("application/json"));
        assert!(denied.body.contains("\"error\":\"unauthorized\""));
        assert!(!denied.body.contains("<html"));
        assert!(!denied.head.to_ascii_lowercase().contains("access-control-allow-origin"));
        assert!(header_line(&denied.head, "cache-control").contains("no-store"));

        let unknown = get(&gate, "/api/does-not-exist");
        assert_eq!(unknown.status, 401);
        assert!(unknown.content_type.starts_with("application/json"));
        assert!(!unknown.body.contains("<html"));

        // The old static-token auth was removed: a Bearer header admits
        // nobody now, on known or unknown paths alike.
        let bearer = http(
            &gate,
            "GET /api/status HTTP/1.1\r\nHost: edit.phext.io\r\nAuthorization: Bearer s3cret-token\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(bearer.status, 401, "{}", bearer.body);
        assert!(bearer.content_type.starts_with("application/json"));
        assert!(bearer.body.contains("\"error\":\"unauthorized\""));

        let bearer_missing = http(
            &gate,
            "GET /api/does-not-exist HTTP/1.1\r\nHost: edit.phext.io\r\nAuthorization: Bearer s3cret-token\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(bearer_missing.status, 401);
        assert!(bearer_missing.content_type.starts_with("application/json"));

        // A signed-in session is the way in.
        let cookie = format!("Cookie: phext_session={}\r\n", Oauth::test_session_cookie());
        let authed = http(
            &gate,
            &format!("GET /api/status HTTP/1.1\r\nHost: edit.phext.io\r\n{cookie}Connection: close\r\n\r\n"),
        );
        assert_eq!(authed.status, 200, "{}", authed.body);
        assert!(authed.content_type.starts_with("application/json"));

        let missing = http(
            &gate,
            &format!("GET /api/does-not-exist HTTP/1.1\r\nHost: edit.phext.io\r\n{cookie}Connection: close\r\n\r\n"),
        );
        assert_eq!(missing.status, 404);
        assert!(missing.content_type.starts_with("application/json"));
        assert!(missing.body.contains("\"error\":\"not found\""));

        let session = get(&open_gate(), "/api/session");
        assert_eq!(session.status, 200);
        assert!(session.content_type.starts_with("application/json"));
        assert!(session.body.contains("\"locked\":false"));

        // Sign-in itself is an API route and must stay reachable while signed out.
        let oauth = Arc::new(Gate::new(Oauth::test_ready(), Editors::default()));
        let who = get(&oauth, "/api/session");
        assert_eq!(who.status, 200);
        assert!(who.body.contains("\"locked\":true"));
        assert!(who.content_type.starts_with("application/json"));
        let start = get(&oauth, "/api/auth/github");
        assert_eq!(start.status, 302, "{}", start.body);
        assert!(!start.body.contains("Page not found"));
    }

    #[test]
    fn sponsor_url_comes_from_config_and_pages_have_no_ad_markup() {
        const AD_MARKERS: &[&str] = &[
            "adsbygoogle",
            "carbonads",
            "ethicalads",
            "ethical-ads",
            "doubleclick.net",
            "googlesyndication",
            "googletagmanager",
            "googletagservices",
            "ad-slot",
            "data-ad-",
            "buysellads",
            "carbon.js",
        ];
        let pages = [
            ("shell", INDEX_HTML),
            ("about", ABOUT_HTML),
            ("privacy", PRIVACY_HTML),
            ("terms", TERMS_HTML),
            ("claim", CLAIM_HTML),
            ("submit", SUBMIT_HTML),
            ("missing", NOT_FOUND_HTML),
            ("ug5", UP_GOER_FIVE_HTML),
            ("tts", TTS_DEMO_HTML),
            ("learn", LEARN_YOU_PHEXT_HTML),
            ("singular", SINGULAR_HTML),
            ("preview", PREVIEWER_HTML),
            ("explore", EXPLORE_HTML),
        ];
        let offer = SponsorOffer::parse("https://buy.stripe.com/test_site?foo=1&bar=2");
        for (name, page) in pages {
            assert!(page.contains(sponsor::SLOT), "{name} missing sponsor slot");
            let lower = page.to_ascii_lowercase();
            for marker in AD_MARKERS {
                assert!(!lower.contains(marker), "{name} contains {marker}");
            }
            assert!(!page.contains("buy.stripe.com"), "{name} hardcodes a payment link");
            assert!(!page.contains("donate.stripe.com"), "{name} hardcodes the live Payment Link");
            assert!(!page.contains(">Support the site</a>"), "{name} bakes in the donation link");
            let hidden = sponsor::apply_slot(page, &SponsorOffer::parse(""));
            assert!(!hidden.contains(sponsor::SLOT), "{name} left the slot in");
            assert!(!hidden.contains(">Support the site</a>"), "{name} shows a link with no URL");
            assert!(!hidden.contains("buy.stripe.com"), "{name}");
            let shown = sponsor::apply_slot(page, &offer);
            assert!(shown.contains("href=\"https://buy.stripe.com/test_site?foo=1&amp;bar=2\""));
            assert!(shown.contains(">Support the site</a>"));
            assert!(shown.contains("not an advertisement"));
            assert!(shown.contains("Exocortex Founding"));
            let shown_lower = shown.to_ascii_lowercase();
            for marker in AD_MARKERS {
                assert!(!shown_lower.contains(marker), "{name} render contains {marker}");
            }
        }
        assert!(INDEX_HTML.matches(sponsor::SLOT).count() >= 2, "status bar and sign-in screen");
        assert!(TERMS_HTML.contains("No ads, ever"));
        assert!(TERMS_HTML.contains("does not show advertisements"));
        assert!(ABOUT_HTML.contains("No ads, ever"));
        assert!(PRIVACY_HTML.contains("no ads, ever"));

        let dash = dashboard_html(
            &[] as &[&phext_lattice::claims::Claim],
            0,
            0,
            &[] as &[phext_lattice::joins::Join],
        );
        assert!(dash.contains(sponsor::SLOT), "dashboard footer");

        // The served page follows the process environment, same value parse accepts.
        let about = get(&locked_gate(), "/about");
        assert_eq!(about.status, 200);
        assert!(about.body.contains("No ads, ever"));
        assert!(!about.body.contains(sponsor::SLOT));
        match SponsorOffer::from_env().url() {
            None => {
                assert!(!about.body.contains(">Support the site</a>"));
                assert!(!about.body.contains("buy.stripe.com"));
            }
            Some(url) => {
                assert!(about.body.contains(&sponsor::apply_slot("<!--phext-sponsor-->", &SponsorOffer::parse(url))));
            }
        }
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;

    fn hdr(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn head_of(bytes: &[u8]) -> String {
        let end = bytes.windows(4).position(|w| w == b"\r\n\r\n").expect("header end");
        String::from_utf8(bytes[..end].to_vec()).unwrap()
    }

    #[test]
    fn shell_revalidates_and_api_responses_are_not_stored() {
        let headers = hdr(&[]);
        let bytes = response_bytes(200, HTML, INDEX_HTML.as_bytes(), CacheClass::Shell, &headers, false);
        let head = head_of(&bytes);
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.contains(&format!("Cache-Control: {SHELL_CACHE}\r\n")));
        assert!(head.contains("ETag: \""));
        assert!(head.contains("X-Content-Type-Options: nosniff\r\n"));
        assert!(head.contains("X-Frame-Options: DENY\r\n"));
        assert!(head.contains("Referrer-Policy: strict-origin-when-cross-origin\r\n"));
        assert!(head.contains("Permissions-Policy: accelerometer=()"));
        assert!(head.contains("Content-Security-Policy: "));
        assert!(head.contains("img-src 'self'"));
        assert!(!head.contains("img-src *"));
        assert!(head.contains("frame-ancestors 'none'"));
        assert!(!head.contains("unsafe-eval"));
        assert!(!head.to_ascii_lowercase().contains("access-control-allow-origin"));
        assert!(!head.contains("Strict-Transport-Security"));
        assert!(bytes.ends_with(INDEX_HTML.as_bytes()));

        let etag = head.split("ETag: ").nth(1).unwrap().split("\r\n").next().unwrap().to_string();
        let again = response_bytes(
            200,
            HTML,
            INDEX_HTML.as_bytes(),
            CacheClass::Shell,
            &hdr(&[("if-none-match", &etag)]),
            false,
        );
        assert!(head_of(&again).starts_with("HTTP/1.1 304 Not Modified"));
        let end = again.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        assert_eq!(end + 4, again.len());

        let weak = format!("W/{etag}");
        let weak_bytes = response_bytes(
            200,
            HTML,
            INDEX_HTML.as_bytes(),
            CacheClass::Shell,
            &hdr(&[("if-none-match", &weak)]),
            false,
        );
        assert!(head_of(&weak_bytes).starts_with("HTTP/1.1 304 Not Modified"));

        let api = response_bytes(200, "application/json", b"{\"ok\":true}", CacheClass::NoStore, &headers, false);
        let api_head = head_of(&api);
        assert!(api_head.contains("Cache-Control: no-store\r\n"));
        assert!(!api_head.contains("ETag:"));
        assert!(!api_head.to_ascii_lowercase().contains("access-control-allow-origin"));
    }

    #[test]
    fn https_shell_carries_hsts_and_head_omits_the_body() {
        let bytes = response_bytes(
            200,
            HTML,
            b"<html></html>",
            CacheClass::Shell,
            &hdr(&[("x-forwarded-proto", "https")]),
            true,
        );
        let head = head_of(&bytes);
        assert!(head.contains("Strict-Transport-Security: max-age=31536000"));
        assert!(head.contains("upgrade-insecure-requests"));
        assert!(head.contains("Content-Length: 13\r\n"));
        let end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        assert_eq!(end + 4, bytes.len());
    }

    #[test]
    fn favicon_ico_is_an_icon_and_the_svg_keeps_its_type() {
        let (ico_type, ico) = public_asset("/favicon.ico").unwrap();
        assert_eq!(ico_type, "image/x-icon");
        assert_eq!(&ico[..4], &[0, 0, 1, 0]);
        assert!(!ico.starts_with(b"<svg") && !ico.starts_with(b"<?xml"));
        assert_eq!(u16::from_le_bytes([ico[4], ico[5]]), 3);
        let (svg_type, svg) = public_asset("/favicon.svg").unwrap();
        assert_eq!(svg_type, "image/svg+xml");
        assert!(svg.windows(4).any(|w| w == b"<svg"));
        assert!(public_asset("/favicon.png").is_none());
        assert!(public_asset("/robots.txt").is_none());
        assert!(public_asset("/sitemap.xml").is_none());
        assert!(public_asset("/favicon.ico?from=browser").is_some());
        assert!(public_asset("/favicon.svg?v=1").is_some());
        let (apple_type, apple) = public_asset("/apple-touch-icon.png").unwrap();
        assert_eq!(apple_type, "image/png");
        assert_eq!(public_asset("/apple-touch-icon-precomposed.png").unwrap().1, apple);
        let (og_type, og) = public_asset("/og.png?fbclid=1").unwrap();
        assert_eq!(og_type, "image/png");
        assert!(og.starts_with(b"\x89PNG\r\n\x1a\n"));

        let head = head_of(&response_bytes(200, ico_type, ico, CacheClass::Static, &hdr(&[]), false));
        assert!(head.contains("Content-Type: image/x-icon\r\n"));
        assert!(head.contains(&format!("Cache-Control: {STATIC_CACHE}\r\n")));
        assert!(head.contains("ETag: \""));
        assert!(head.contains("img-src 'self'"));
        assert!(!head.to_ascii_lowercase().contains("access-control-allow-origin"));
        let og_head = head_of(&response_bytes(200, og_type, og, CacheClass::Static, &hdr(&[]), true));
        assert!(og_head.contains("Content-Type: image/png\r\n"));
        assert!(og_head.contains(&format!("Cache-Control: {STATIC_CACHE}\r\n")));
        assert!(og_head.contains("ETag: \""));
        assert!(!og_head.to_ascii_lowercase().contains("access-control-allow-origin"));
    }

    #[test]
    fn pages_name_the_icon_set() {
        // One icon set on every page, with absolute URLs: the shell is
        // also served at /<name>/ and the 404 page at arbitrary paths,
        // where a relative icon URL would resolve underneath them.
        let links = "\
<link rel=\"icon\" href=\"/favicon.ico\" sizes=\"16x16 32x32 48x48\">\n\
<link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\">\n\
<link rel=\"apple-touch-icon\" href=\"/apple-touch-icon.png\">";
        for page in [INDEX_HTML, ABOUT_HTML, PRIVACY_HTML, TERMS_HTML, NOT_FOUND_HTML] {
            assert!(page.contains(links), "missing icon set");
            assert!(!page.contains("image/svg+xml\" href=\"favicon.ico\""));
            assert!(!page.contains("href=\"favicon"), "icon URLs are absolute");
        }
    }
}

#[cfg(test)]
mod share_metadata_tests {
    use super::{
        handle_request, public_asset, AppState, DemoCatalog, Gate, ABOUT_HTML, APPLE_TOUCH_PNG,
        INDEX_HTML,
        NOT_FOUND_HTML, OG_PNG, PRIVACY_HTML, STATIC_CACHE, TERMS_HTML,
    };
    use phext_lattice::oauth::{Editors, Oauth};
    use phext_lattice::MappedLattice;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const TITLE: &str = "Phext Edit: an editor for 11-dimensional plain text";
    const DESCRIPTION: &str = "Browse and edit phext files in the browser. Phext is plain text arranged in 11 dimensions, and the files stay plain text, with no lock-in.";

    fn png_size(bytes: &[u8]) -> (u32, u32) {
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "not a png");
        assert_eq!(&bytes[12..16], b"IHDR");
        let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
        let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
        (w, h)
    }

    fn assert_icon_links(html: &str) {
        assert!(html.contains("<link rel=\"icon\" href=\"/favicon.ico\" sizes=\"16x16 32x32 48x48\">"));
        assert!(html.contains("<link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\">"));
        assert!(html.contains("<link rel=\"apple-touch-icon\" href=\"/apple-touch-icon.png\">"));
        assert!(
            !html.contains("image/svg+xml\" href=\"favicon.ico\""),
            "favicon.ico must not be labeled as SVG"
        );
    }

    #[test]
    fn editor_page_has_share_metadata() {
        assert!(INDEX_HTML.contains("<html lang=\"en\">"));
        assert!(INDEX_HTML.contains(&format!("<title>{TITLE}</title>")));
        assert!(!INDEX_HTML.contains("<title>💎 phext-edit</title>"));
        assert_eq!(INDEX_HTML.matches(TITLE).count(), 3);
        assert_eq!(INDEX_HTML.matches(DESCRIPTION).count(), 3);
        assert!(INDEX_HTML.contains("<link rel=\"canonical\" href=\"https://edit.phext.io/\">"));
        assert!(INDEX_HTML.contains("<meta name=\"theme-color\" content=\"#171717\">"));
        assert!(INDEX_HTML.contains("<meta property=\"og:type\" content=\"website\">"));
        assert!(INDEX_HTML.contains("<meta property=\"og:url\" content=\"https://edit.phext.io/\">"));
        assert!(INDEX_HTML.contains("<meta property=\"og:site_name\" content=\"Phext Edit\">"));
        assert!(INDEX_HTML.contains("<meta property=\"og:image\" content=\"https://edit.phext.io/og.png\">"));
        assert!(INDEX_HTML.contains("<meta property=\"og:image:width\" content=\"1200\">"));
        assert!(INDEX_HTML.contains("<meta property=\"og:image:height\" content=\"630\">"));
        assert!(INDEX_HTML.contains("<meta property=\"og:image:alt\" content=\"Phext Edit: an editor for plain text arranged in 11 dimensions\">"));
        assert!(INDEX_HTML.contains("<meta name=\"twitter:card\" content=\"summary_large_image\">"));
        assert!(INDEX_HTML.contains("<meta name=\"twitter:image\" content=\"https://edit.phext.io/og.png\">"));
        assert_eq!(INDEX_HTML.matches("https://edit.phext.io/og.png").count(), 2);
        assert_icon_links(INDEX_HTML);
        assert_icon_links(ABOUT_HTML);
        assert_icon_links(PRIVACY_HTML);
        assert_icon_links(TERMS_HTML);
        assert!(NOT_FOUND_HTML.contains("href=\"/favicon.ico\""));
        assert!(INDEX_HTML.contains("href=\"/terms\""));
    }

    #[test]
    fn share_assets_are_real_files_with_honest_types() {
        let (ty, ico) = public_asset("/favicon.ico").unwrap();
        assert_eq!(ty, "image/x-icon");
        assert_eq!(&ico[..4], &[0, 0, 1, 0]);
        assert!(!ico.starts_with(b"<svg") && !ico.starts_with(b"<?xml"));
        assert_eq!(u16::from_le_bytes([ico[4], ico[5]]), 3);

        let (ty, svg) = public_asset("/favicon.svg").unwrap();
        assert_eq!(ty, "image/svg+xml");
        assert!(svg.windows(4).any(|w| w == b"<svg"));

        let (ty, apple) = public_asset("/apple-touch-icon.png").unwrap();
        assert_eq!(ty, "image/png");
        assert_eq!(png_size(apple), (180, 180));
        let (ty, alias) = public_asset("/apple-touch-icon-precomposed.png").unwrap();
        assert_eq!(ty, "image/png");
        assert_eq!(alias, apple);

        let (ty, og) = public_asset("/og.png?fbclid=1").unwrap();
        assert_eq!(ty, "image/png");
        assert_eq!(png_size(og), (1200, 630));
        assert!(og.len() < 100_000, "share image should stay small, got {} bytes", og.len());

        assert!(public_asset("/api/status").is_none());
        assert!(public_asset("/robots.txt").is_none());
        assert!(public_asset("/sitemap.xml").is_none());
        assert!(public_asset("/og.svg").is_none());
    }

    fn http(port: u16, request: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        let sep = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("response headers");
        let head = String::from_utf8_lossy(&buf[..sep]);
        let mut lines = head.split("\r\n");
        let status: u16 = lines.next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap();
        let headers = lines
            .filter_map(|line| {
                let (k, v) = line.split_once(':')?;
                Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
            })
            .collect();
        (status, headers, buf[sep + 4..].to_vec())
    }

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
        headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).unwrap_or("")
    }

    #[test]
    fn share_assets_are_public_when_the_api_is_locked() {
        let lattice = MappedLattice::from_bytes(b"scroll");
        let state = Arc::new(Mutex::new(AppState::new(lattice, "test.phext".into())));
        let gate = Arc::new(Gate::new(Oauth::test_ready(), Editors::default()));
        assert!(gate.locked());

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state_bg = Arc::clone(&state);
        let gate_bg = Arc::clone(&gate);
        let server = std::thread::spawn(move || {
            for _ in 0..17 {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
                handle_request(&mut stream, &state_bg, &gate_bg, &DemoCatalog::load());
            }
        });

        let (status, headers, body) = http(
            port,
            "GET /og.png HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), "image/png");
        assert_eq!(header(&headers, "cache-control"), STATIC_CACHE);
        assert!(header(&headers, "etag").starts_with('"'));
        assert!(header(&headers, "content-security-policy").contains("img-src 'self'"));
        assert!(!header(&headers, "content-security-policy").contains("img-src *"));
        assert!(header(&headers, "access-control-allow-origin").is_empty());
        assert_eq!(body, OG_PNG);

        let etag = header(&headers, "etag").to_string();
        let (status, headers, body) = http(
            port,
            &format!("GET /og.png HTTP/1.1\r\nHost: localhost\r\nIf-None-Match: {etag}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(status, 304);
        assert!(body.is_empty());
        assert_eq!(header(&headers, "cache-control"), STATIC_CACHE);
        assert!(header(&headers, "access-control-allow-origin").is_empty());

        let (status, _, body) = http(
            port,
            "GET /og.png?x=1 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(body, OG_PNG);

        let (status, headers, body) = http(
            port,
            "HEAD /og.png HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), "image/png");
        assert_eq!(header(&headers, "content-length"), OG_PNG.len().to_string());
        assert_eq!(header(&headers, "cache-control"), STATIC_CACHE);
        assert!(body.is_empty());

        let (status, headers, body) = http(
            port,
            "GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), "image/x-icon");
        assert_eq!(header(&headers, "cache-control"), STATIC_CACHE);
        assert_eq!(&body[..4], &[0, 0, 1, 0]);

        let (status, headers, body) = http(
            port,
            "GET /favicon.svg HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), "image/svg+xml");
        assert!(body.windows(4).any(|w| w == b"<svg"));

        let (status, headers, body) = http(
            port,
            "GET /apple-touch-icon.png HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "content-type"), "image/png");
        assert_eq!(png_size(&body), (180, 180));

        let (status, _, body) = http(
            port,
            "GET /apple-touch-icon-precomposed.png HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200);
        assert_eq!(body, APPLE_TOUCH_PNG);

        let (status, headers, body) = http(
            port,
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200);
        let page = String::from_utf8(body).unwrap();
        assert!(page.contains(&format!("<title>{TITLE}</title>")));
        assert!(page.contains("rel=\"canonical\" href=\"https://edit.phext.io/\""));
        assert!(page.contains("name=\"twitter:card\" content=\"summary_large_image\""));
        assert!(page.contains("name=\"theme-color\" content=\"#171717\""));
        assert!(header(&headers, "content-security-policy").contains("img-src 'self'"));
        assert!(header(&headers, "access-control-allow-origin").is_empty());

        for path in ["/about", "/privacy", "/terms"] {
            let (status, _, _) = http(port, &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"));
            assert_eq!(status, 200, "{path}");
        }
        let (status, _, body) = http(
            port,
            "GET /robots.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200);
        let robots = String::from_utf8(body).unwrap();
        assert!(robots.contains("Sitemap: https://edit.phext.io/sitemap.xml"));
        assert!(robots.contains("Allow: /terms\n"));
        let (status, _, body) = http(
            port,
            "GET /sitemap.xml HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200);
        let sitemap = String::from_utf8(body).unwrap();
        assert!(sitemap.contains("https://edit.phext.io/terms"));
        assert!(sitemap.contains("https://edit.phext.io/about"));

        let (status, _, _) = http(
            port,
            "GET /api/status HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 401);
        let (status, headers, _) = http(
            port,
            "GET /api/nav HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 401);
        assert!(header(&headers, "access-control-allow-origin").is_empty());
        assert!(header(&headers, "cache-control").contains("no-store"));

        let (status, headers, _) = http(
            port,
            "POST /og.png HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 405);
        assert!(header(&headers, "access-control-allow-origin").is_empty());
        assert!(header(&headers, "cache-control").contains("no-store"));

        server.join().unwrap();
    }
}

#[cfg(test)]
mod link_tests {
    use super::*;
    use phext_lattice::oauth::{Editors, Gate, Oauth, TEST_CSRF};
    use phext_lattice::MappedLattice;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    fn coord_scroll(s: &str) -> usize {
        parse_coordinate(s).unwrap().x.scroll
    }

    #[test]
    fn coordinates_accept_canonical_semicolon_and_slash_forms() {
        assert_eq!(DEFAULT_COORD, "1.1.1/1.1.1/1.1.1");
        assert!(parse_coordinate(DEFAULT_COORD)
            .unwrap()
            .validate_coordinate());
        assert_eq!(coord_scroll("1.1.1;1.1.1;1.1.2"), 2);
        assert_eq!(coord_scroll("1/1/1/1/1/1/1/1/2"), 2);
        assert_eq!(coord_scroll("01.1.1/1.1.1/1.1.2"), 2);
        // libphext 0.4 uncapped coordinates, so a scroll past the old maximum is real.
        assert_eq!(coord_scroll("1.1.1/1.1.1/1.1.1001"), 1001);
        for bad in [
            "",
            "nope",
            "1.1.1",
            "1.1.1/1.1.1",
            "0.1.1/1.1.1/1.1.1",
            "../etc",
            "1.1.1/1.1.1/1.1.1/1.1",
            "1.2.3.4/1.1.1/1.1.1",
            "1.1.1/1.1.1/1.1",
            "1/1/1/1/1/1/1/1",
            "1.1.1/1.1.1/1.1.2/extra",
        ] {
            assert!(parse_coordinate(bad).is_none(), "{bad} should be rejected");
        }
    }

    #[test]
    fn phext_link_rejects_traversal_and_public_routes() {
        for bad in [
            "",
            ".",
            "..",
            "../etc",
            "..\\etc",
            "jonas.phext",
            "foo/bar",
            "foo\\bar",
            "has space",
            "a.b",
            "%2e%2e",
            "%2e%2e%2fetc",
            "jonas%2f..%2fetc",
            "%252e%252e",
            "jonas/../../etc",
            ".%2e",
            "jonas%00",
        ] {
            assert!(phext_link(&format!("/{bad}")).is_none(), "/{bad}");
            assert!(phext_link(&format!("/{bad}/")).is_none(), "/{bad}/");
        }
        for path in [
            "/../etc/passwd",
            "/%2e%2e/etc/passwd",
            "/%2e%2e/%2e%2e/etc/passwd",
            "/jonas/../../etc/passwd",
            "/jonas/%2e%2e/%2e%2e/etc/passwd",
            "/./../../etc/passwd",
            "//etc/passwd",
            "/og.png",
            "/favicon.ico",
            "/favicon.svg",
            "/robots.txt",
            "/sitemap.xml",
            "/api/nav",
            "/api/demo/nav",
            "/api/status",
            "/?demo",
            "/",
            "/index.html",
            "/terms",
            "/terms/",
            "/about",
            "/privacy",
            "/index",
            "/index/",
            "/submit",
            "/submit/",
            "/claim",
            "/claim/",
            "/explore",
            "/explore/",
            "/claim/dashboard",
        ] {
            assert!(phext_link(path).is_none(), "{path}");
        }
        let bare = phext_link("/jonas").unwrap();
        assert_eq!(bare.name, "jonas");
        assert!(!bare.slash);
        assert_eq!(bare.location(), "/jonas/");
        let slashed = phext_link("/jonas/?demo=1").unwrap();
        assert!(slashed.slash);
        assert_eq!(slashed.name, "jonas");
        let named = phext_link("/My_File-1?demo=1").unwrap();
        assert_eq!(named.location(), "/My_File-1/?demo=1");
        assert!(phext_link("/jonas/extra").is_none());
        assert!(phext_link("/jonas/extra/").is_none());
    }

    fn sample_library() -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("library");
        std::fs::create_dir(&dir).unwrap();
        let aaa = dir.join("aaa.phext");
        std::fs::write(&aaa, b"AAA-ORIGIN").unwrap();
        let mut jonas = Vec::new();
        jonas.extend_from_slice(b"JONAS-ORIGIN");
        jonas.push(0x17);
        jonas.extend_from_slice(b"JONAS-TARGET");
        std::fs::write(dir.join("jonas.phext"), &jonas).unwrap();
        std::fs::write(dir.join("terms.phext"), b"TERMS-FILE-SECRET").unwrap();
        std::fs::write(root.path().join("secret.phext"), b"SECRET-OUTSIDE-CONTENT").unwrap();
        // Symlinks are a Unix path check. Windows has no privilege-free equivalent
        // here; the escape file is simply absent, and the assertions still require
        // that name not to resolve outside the directory.
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path().join("secret.phext"), dir.join("escape.phext"))
            .unwrap();
        (root, dir)
    }

    fn serve_library(local: bool) -> (u16, tempfile::TempDir) {
        let (root, dir) = sample_library();
        let aaa = dir.join("aaa.phext");
        let lattice = MappedLattice::open(&aaa).unwrap();
        let mut app = AppState::new(lattice, aaa.to_string_lossy().to_string());
        app.phext_dir = Some(dir.to_string_lossy().to_string());
        let state = Arc::new(Mutex::new(app));
        let gate = Gate::new(Oauth::test_ready(), Editors::default());
        if local {
            gate.enable_local();
        }
        let gate = Arc::new(gate);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for incoming in listener.incoming() {
                let Ok(mut stream) = incoming else { break };
                let state = Arc::clone(&state);
                let gate = Arc::clone(&gate);
                handle_request(&mut stream, &state, &gate, &DemoCatalog::load());
            }
        });
        (port, root)
    }

    fn http(port: u16, request: &str) -> (u16, String, String) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let raw = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
        let status = head
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        (status, head.to_string(), body.to_string())
    }

    fn get(port: u16, path: &str, extra: &str) -> (u16, String, String) {
        http(
            port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{extra}\r\n"
            ),
        )
    }

    fn header<'a>(head: &'a str, name: &str) -> &'a str {
        head.lines()
            .find_map(|line| {
                let (n, value) = line.split_once(':')?;
                n.eq_ignore_ascii_case(name).then(|| value.trim())
            })
            .unwrap_or("")
    }

    #[test]
    fn named_links_stay_inside_the_library_and_do_not_leak() {
        let (port, _root) = serve_library(false);
        let secret = "SECRET-OUTSIDE-CONTENT";
        let marker = "JONAS-ORIGIN";

        let (status, head, body) = get(port, "/jonas/", "");
        assert_eq!(status, 200, "{head}");
        assert!(body.contains("phext-edit"), "{body}");
        assert!(body.contains("href=\"/favicon.svg\""));
        assert!(body.contains("fetch('/api/session')"));
        assert!(!body.contains(marker), "shell leaked scroll text");
        assert!(!body.contains(secret));
        assert!(head.contains("ETag:"));

        let (status, _, body) = http(
            port,
            &format!(
                "HEAD /jonas/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(status, 200);
        assert!(body.is_empty());

        let (status, head, body) = get(port, "/jonas", "");
        assert_eq!(status, 302, "{head} {body}");
        assert_eq!(header(&head, "Location"), "/jonas/");
        assert!(!body.contains(marker) && !body.contains(secret));

        let (status, head, _) = get(port, "/jonas?demo=1", "");
        assert_eq!(status, 302, "{head}");
        assert_eq!(header(&head, "Location"), "/jonas/?demo=1");

        let (status, _, body) = get(port, "/nope/", "");
        assert_eq!(status, 404);
        assert!(body.contains("Page not found"));
        assert!(!body.contains(secret));

        let (status, _, body) = get(port, "/nope/", "Accept: application/json\r\n");
        assert_eq!(status, 404);
        assert!(body.contains("\"error\":\"not found\""));
        assert!(!body.contains("<html"));
        assert!(!body.contains(secret));

        let traversals = [
            "/../etc/passwd",
            "/..%2f..%2fetc/passwd",
            "/%2e%2e/etc/passwd",
            "/%2e%2e/%2e%2e/etc/passwd",
            "/%2e%2e",
            "/jonas/../../etc/passwd",
            "/jonas/%2e%2e/%2e%2e/etc/passwd",
            "/./../../etc/passwd",
            "/%2e%2e%2fetc%2fpasswd",
            "/jonas%2f..%2f..%2fetc%2fpasswd",
            "/foo/../../../etc/passwd",
            "/%252e%252e/etc/passwd",
            "/escape/",
            "/escape",
            "/secret/",
            "/jonas.phext",
            "/jonas.phext/",
            "/aaa/../jonas/",
            "/terms/",
            "/about/",
        ];
        for path in traversals {
            let (status, _, body) = get(port, path, "");
            assert_eq!(status, 404, "{path} -> {status} {body}");
            assert!(!body.contains(secret), "{path} leaked secret");
            assert!(!body.contains(marker), "{path} leaked jonas");
            assert!(
                !body.contains("TERMS-FILE-SECRET"),
                "{path} leaked terms.phext"
            );
        }

        let (status, _, body) = get(port, "/terms", "");
        assert_eq!(status, 200);
        assert!(body.contains("Terms of Service"));
        assert!(!body.contains("TERMS-FILE-SECRET"));
        let (status, _, body) = get(port, "/robots.txt", "");
        assert_eq!(status, 200);
        assert!(body.contains("Sitemap:"));
        let (status, _, body) = get(port, "/sitemap.xml", "");
        assert_eq!(status, 200);
        assert!(body.contains("edit.phext.io"));
        let (status, _, body) = get(port, "/?demo", "");
        assert_eq!(status, 200);
        assert!(body.contains("phext-edit"));
        let (status, head, _) = get(port, "/favicon.ico", "");
        assert_eq!(status, 200);
        assert!(header(&head, "Content-Type").starts_with("image/"));
        let (status, head, body) = get(port, "/og.png", "");
        assert_eq!(status, 200);
        assert!(header(&head, "Content-Type").starts_with("image/"));
        assert!(!body.contains(secret));
        let (status, _, body) = get(port, "/api/nav", "");
        assert_eq!(status, 401);
        assert!(body.contains("unauthorized"));
        assert!(!body.contains(marker));

        let cookie = Oauth::test_session_cookie();
        let auth = format!("Cookie: phext_session={cookie}\r\n");
        let (status, _, body) = get(port, "/api/status", &auth);
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("aaa.phext"),
            "shell GET must not switch files: {body}"
        );
        assert!(!body.contains(secret));

        let open = |target: &str| {
            let req = format!(
                "POST /api/open HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: phext_session={cookie}\r\nx-csrf-token: {TEST_CSRF}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{target}",
                target.len()
            );
            http(port, &req)
        };
        let (status, _, body) = open("jonas");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"ok\":true"), "{body}");
        let (status, _, body) = get(port, "/api/nav", &auth);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains(marker), "{body}");
        assert!(!body.contains(secret));

        let goto = |coord: &str| {
            let req = format!(
                "POST /api/goto HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: phext_session={cookie}\r\nx-csrf-token: {TEST_CSRF}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{coord}",
                coord.len()
            );
            let (status, _, body) = http(port, &req);
            assert_eq!(status, 200, "{coord} {body}");
            body
        };
        assert!(goto("1.1.1/1.1.1/1.1.2").contains("JONAS-TARGET"));
        assert!(goto("1.1.1;1.1.1;1.1.2").contains("JONAS-TARGET"));
        assert!(goto("1/1/1/1/1/1/1/1/2").contains("JONAS-TARGET"));
        let stayed = goto("not-a-coordinate");
        assert!(
            stayed.contains("JONAS-TARGET"),
            "malformed goto moved: {stayed}"
        );

        for bad in [
            "../secret",
            "../secret.phext",
            "..",
            "escape",
            "secret",
            "/etc/passwd",
            "jonas/../../secret.phext",
        ] {
            let (status, _, body) = open(bad);
            assert_eq!(status, 200, "{bad} {body}");
            assert!(body.contains("\"ok\":false"), "{bad} opened: {body}");
            assert!(!body.contains(secret), "{bad} leaked: {body}");
        }
        let outside = _root.path().join("secret.phext");
        let (status, _, body) = open(&outside.to_string_lossy());
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"ok\":false"), "{body}");
        assert!(!body.contains(secret));
        let escape = _root.path().join("library").join("escape.phext");
        let (status, _, body) = open(&escape.to_string_lossy());
        assert!(body.contains("\"ok\":false"), "{status} {body}");
        assert!(!body.contains(secret));

        let (status, _, body) = get(port, "/api/status", &auth);
        assert!(body.contains("jonas.phext"), "{status} {body}");
        assert!(!body.contains("secret"));
    }

    #[test]
    fn local_mode_serves_a_named_shell_and_still_blocks_traversal() {
        let (port, _root) = serve_library(true);
        let (status, _, body) = get(port, "/api/status", "");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("aaa.phext"));
        let (status, _, body) = get(port, "/jonas/", "");
        assert_eq!(status, 200);
        assert!(body.contains("fetch('/api/session')"));
        assert!(!body.contains("JONAS-ORIGIN"));
        let (status, _, body) = get(port, "/%2e%2e/%2e%2e/etc/passwd", "");
        assert_eq!(status, 404);
        assert!(!body.contains("SECRET-OUTSIDE-CONTENT"));
        let (status, _, body) = get(port, "/api/nav", "");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("AAA-ORIGIN"), "{body}");
        assert!(!body.contains("JONAS-ORIGIN"));
    }

    #[test]
    fn hosted_phext_file_does_not_follow_links_out_of_the_directory() {
        let (root, dir) = sample_library();
        let dir_s = dir.to_string_lossy().to_string();
        assert!(hosted_phext_file(Some(&dir_s), "", "jonas")
            .unwrap()
            .ends_with("jonas.phext"));
        assert!(hosted_phext_file(Some(&dir_s), "", "escape").is_none());
        assert!(hosted_phext_file(Some(&dir_s), "", "..").is_none());
        assert!(hosted_phext_file(Some(&dir_s), "", "../secret").is_none());
        assert!(hosted_phext_file(Some(&dir_s), "", "secret").is_none());
        assert!(hosted_phext_file(Some(&dir_s), "", "nope").is_none());
        assert!(
            hosted_phext_file(Some(&dir_s), "", "terms").is_none(),
            "reserved name"
        );
        let outside = root
            .path()
            .join("secret.phext")
            .to_string_lossy()
            .to_string();
        assert!(hosted_phext_file(None, &outside, "secret").is_some());
        assert!(hosted_phext_file(None, &outside, "jonas").is_none());
    }
}

mod a11y {
    use super::{ABOUT_HTML, INDEX_HTML, PRIVACY_HTML};

    fn channel(c: f64) -> f64 {
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    }

    fn luminance(hex: &str) -> f64 {
        let hex = hex.trim_start_matches('#');
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap() as f64 / 255.0;
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap() as f64 / 255.0;
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap() as f64 / 255.0;
        0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
    }

    fn contrast(a: &str, b: &str) -> f64 {
        let (l1, l2) = (luminance(a), luminance(b));
        let (hi, lo) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn editor_shell_is_responsive_and_labeled() {
        let html = INDEX_HTML;
        assert!(html.contains("width=device-width"), "viewport meta is still present");
        assert!(html.contains("@media (max-width: 760px)"), "phone layout needs a breakpoint");
        assert!(html.contains("width: 320px"), "desktop panel stays 320px");
        assert!(html.contains("drawer-open"), "phone panel is a drawer");
        assert!(html.contains("class=\"skip-link\""));
        assert!(html.contains("role=\"tablist\""));
        assert!(html.contains("role=\"tab\""));
        assert!(html.contains("aria-live=\"polite\""));
        assert!(html.contains("aria-label=\"Search scrolls\""));
        assert!(html.contains("aria-label=\"Go to coordinate\""));
        assert!(html.contains("aria-label=\"Scroll text\""));
        assert!(html.contains("id=\"panel-toggle\""));
        assert!(html.contains("prefers-reduced-motion"));
        assert!(html.contains("<button type=\"button\" id=\"signout\""));
        assert!(html.contains("type=\"button\" class=\"axon spatial"));
        assert!(html.contains("type=\"button\" class=\"nt-group"));
        assert!(html.contains("type=\"button\" class=\"hit\""));
        assert!(!html.contains("<div class=\"tab"), "tabs are buttons, not click-only divs");
        assert!(!html.contains("--dim: #808080"), "dim grey failed AA on the coord bar");
        assert!(html.contains("--dim: #9a9a9a"));
        assert!(html.contains("--field-border: #767676"));
    }

    #[test]
    fn public_pages_are_responsive() {
        for page in [ABOUT_HTML, PRIVACY_HTML] {
            assert!(page.contains("width=device-width"));
            assert!(page.contains("@media (max-width: 760px)"));
            assert!(page.contains("class=\"skip-link\""));
            assert!(page.contains("prefers-reduced-motion"));
            assert!(page.contains("id=\"content\""));
        }
    }

    #[test]
    fn text_and_control_colors_meet_wcag_aa() {
        // Text on the surfaces it actually sits on. 4.5:1 is AA for normal text.
        for (fg, bg) in [
            ("#dedede", "#171717"),
            ("#9a9a9a", "#171717"),
            ("#9a9a9a", "#131313"),
            ("#9a9a9a", "#1c1c1c"),
            ("#9a9a9a", "#101010"),
            ("#d4a855", "#171717"),
            ("#d4a855", "#1c1c1c"),
            ("#d4a855", "#2a2312"),
            ("#4dc9c9", "#171717"),
            ("#4dc9c9", "#131313"),
            ("#c964c9", "#131313"),
            ("#6bc96b", "#131313"),
            ("#e8c547", "#171717"),
            ("#e05555", "#1c1c1c"),
            ("#a3a3a3", "#171717"),
            ("#000000", "#4dc9c9"),
            ("#000000", "#6bc96b"),
            ("#000000", "#d4a855"),
        ] {
            let ratio = contrast(fg, bg);
            assert!(ratio >= 4.5, "{fg} on {bg} is {ratio:.2}, below AA 4.5");
        }
        // The previous dim grey fails on the coordinate bar. Keep that from coming back.
        assert!(contrast("#808080", "#1c1c1c") < 4.5);
        // Input border against the page. 3:1 is AA for UI boundaries.
        assert!(contrast("#767676", "#171717") >= 3.0);
        assert!(contrast("#2e2e2e", "#171717") < 3.0);
    }
}

#[cfg(test)]
mod demo_tests {
    use super::*;
    use phext_lattice::oauth::{Editors, Oauth};
    use std::io::{Read, Write};
    use std::time::Duration;

    fn locked_gate() -> Gate {
        Gate::new(Oauth::test_ready(), Editors::parse("github:100"))
    }

    fn editor_headers() -> HashMap<String, String> {
        let cookie = format!("phext_session={}", Oauth::test_session_cookie());
        HashMap::from([("cookie".into(), cookie)])
    }

    #[test]
    fn signed_out_shell_leads_with_the_demo() {
        let page = index_page(&locked_gate(), &HashMap::new());
        assert!(page.contains(GATE_OPEN), "signed-out page should show the intro");
        assert!(page.contains("Phext Edit"));
        assert!(page.contains("Write in 11 dimensions: plain text, no lock-in."));
        assert!(page.contains("Try the demo"));
        assert!(page.contains("href=\"/?demo\""));
        assert!(page.contains("Sign in with Google"));
        assert!(page.contains("Sign in with GitHub"));
        assert!(page.contains("<noscript>"));
        assert!(page.contains("phext-previewer.html"));
        assert!(!page.contains("token-box"));
        assert!(!page.contains("auth-input"));
        assert!(!page.contains("use an access token"));
    }

    #[test]
    fn a_signed_in_editor_does_not_see_the_intro() {
        let page = index_page(&locked_gate(), &editor_headers());
        assert!(page.contains(GATE_SHUT));
        assert!(!page.contains(GATE_OPEN));
    }

    #[test]
    fn an_open_server_skips_the_intro() {
        let gate = Gate::new(Oauth::closed(), Editors::default());
        let page = index_page(&gate, &HashMap::new());
        assert!(page.contains(GATE_SHUT));
    }

    #[test]
    fn local_mode_skips_the_intro_even_when_the_server_is_locked() {
        let gate = locked_gate();
        gate.enable_local();
        let page = index_page(&gate, &HashMap::new());
        assert!(page.contains(GATE_SHUT));
        assert!(!page.contains(GATE_OPEN));
    }

    #[test]
    fn the_demo_is_stateless_and_spans_several_dimensions() {
        let demo = DemoCatalog::load();
        let origin = demo.navigate("", 1, "home", "");
        assert!(origin.scroll.content.contains("Welcome to the sample phext"));
        assert!(origin.scroll.content.contains("eleven dimensions"));
        assert_eq!(origin.position.coordinate, "1.1.1/1.1.1/1.1.1");
        assert!(origin.sentron.axons.iter().any(|a| a.name == "Library"));

        let again = demo.navigate("1.1.1/1.1.1/1.1.1", 1, "next", "");
        let same = demo.navigate("1.1.1/1.1.1/1.1.1", 1, "next", "");
        assert_eq!(again.position.coordinate, same.position.coordinate);
        assert_eq!(again.position.coordinate, "1.1.1/1.1.1/1.1.2");

        let mut at = origin.position.coordinate.clone();
        let mut seen = vec![at.clone()];
        for _ in 0..20 {
            let step = demo.navigate(&at, 1, "next", "");
            if step.position.coordinate == at {
                break;
            }
            at = step.position.coordinate.clone();
            seen.push(at.clone());
        }
        for coord in [
            "1.1.1/1.1.1/1.2.1",
            "1.1.1/1.1.2/1.1.1",
            "1.1.1/2.1.1/1.1.1",
            "1.1.2/1.1.1/1.1.1",
            "1.2.1/1.1.1/1.1.1",
            "2.1.1/1.1.1/1.1.1",
            "2.1.1/1.1.1/1.1.2",
        ] {
            assert!(seen.iter().any(|c| c == coord), "missing {coord} in {seen:?}");
        }
        assert_eq!(seen.len(), DEMO_SCROLLS.len());

        let section = demo.navigate("1.1.1/1.1.1/1.1.2", 2, "forward", "");
        assert_eq!(section.position.coordinate, "1.1.1/1.1.1/1.2.1");
        assert_eq!(section.position.dimension, "Section");

        let hits = demo.search("outermost");
        assert!(hits.iter().any(|h| h.coordinate == "2.1.1/1.1.1/1.1.1"));
        assert!(demo.search("").is_empty());
    }

    fn exchange(port: u16, request: &str) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut buf = String::new();
        let _ = stream.read_to_string(&mut buf);
        let status = buf.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
        (status, buf)
    }

    #[test]
    fn signed_out_demo_is_public_and_real_files_stay_locked() {
        let dir = tempfile::tempdir().unwrap();
        let phext = dir.path().join("real.phext");
        std::fs::write(&phext, b"hello-real-file-marker").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let lattice = MappedLattice::open(&phext).unwrap();
        let state = Arc::new(Mutex::new(AppState::new(
            lattice,
            phext.to_string_lossy().to_string(),
        )));
        let gate = Arc::new(locked_gate());
        let demo = Arc::new(DemoCatalog::load());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().take(10).flatten() {
                handle_request(&mut stream, &state, &gate, &demo);
            }
        });

        let (status, body) = exchange(port, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 200);
        assert!(body.contains("Try the demo"));
        assert!(!body.contains("use an access token"));
        assert!(!body.contains("hello-real-file-marker"));

        let (status, body) = exchange(port, "GET /?demo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 200);
        assert!(body.contains("Write in 11 dimensions: plain text, no lock-in."));

        let (status, body) = exchange(port, "GET /api/demo/nav HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("Welcome to the sample phext"));
        assert!(!body.contains("hello-real-file-marker"));

        let (status, body) = exchange(
            port,
            "GET /api/demo/nav?at=1.1.1%2F1.1.1%2F1.1.1&dim=1&op=next HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("1.1.1/1.1.1/1.1.2"));
        assert!(body.contains("tenth and eleventh"));

        let (status, body) = exchange(port, "GET /api/nav HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 401, "{body}");
        assert!(!body.contains("hello-real-file-marker"));

        let (status, body) = exchange(
            port,
            "GET /api/nav HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer s3cret\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 401, "{body}");
        assert!(!body.contains("hello-real-file-marker"));

        let cookie = format!("phext_session={}", Oauth::test_session_cookie());
        let (status, body) = exchange(
            port,
            &format!("GET /api/nav HTTP/1.1\r\nHost: localhost\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("hello-real-file-marker"));

        let (status, body) = exchange(port, "POST /api/demo/nav HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 405, "{body}");
        assert!(body.contains("read-only"));

        let (status, _) = exchange(
            port,
            "POST /api/update HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        );
        assert_eq!(status, 401);

        let (status, body) = exchange(
            port,
            "GET /api/demo/search?q=library HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("2.1.1/1.1.1/1.1.1"));
    }
}
