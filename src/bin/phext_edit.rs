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
use std::sync::{Arc, Mutex};

use serde::Serialize;

use phext_lattice::{
    CoordinateNav, Dimension, LatticeOverview,
    MappedLattice, Navigator, Sentron,
    DimensionDensity,
};
use phext_lattice::oauth::{status_text, Gate};

// ── API Types ──────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ApiStatus {
    scrolls: usize,
    bytes: usize,
    file: String,
    dirty: bool,
}

#[derive(Serialize)]
struct ApiPosition {
    coordinate: String,
    dimension: String,
    dimension_index: u8,
    has_scroll: bool,
    total_scrolls: usize,
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

        AppState { lattice, nav, overview, file_path, dirty: false, phext_dir: None, home_file: None, home_coord: None }
    }

    fn rebuild_overview(&mut self) {
        let buf = self.lattice.to_phext_bytes();
        self.overview = LatticeOverview::build(&buf, self.lattice.index());
    }

    fn nav_json(&self) -> ApiNav {
        nav_view(&self.lattice, &self.nav, self.overview.total_scrolls)
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
                if path.extension().map(|e| e == "phext").unwrap_or(false) {
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

    fn open_phext(&mut self, path: &str) -> Result<(), String> {
        // Validate path is within allowed directory
        if let Some(dir) = &self.phext_dir {
            let canonical = std::fs::canonicalize(path).map_err(|e| format!("{}", e))?;
            let dir_canonical = std::fs::canonicalize(dir).map_err(|e| format!("{}", e))?;
            if !canonical.starts_with(&dir_canonical) {
                return Err("path outside allowed directory".into());
            }
        }
        let lattice = MappedLattice::open(path).map_err(|e| format!("{}", e))?;
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
fn nav_view(lattice: &MappedLattice, nav: &Navigator, total_scrolls: usize) -> ApiNav {
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
    ("2.1.1/1.1.1/1.1.2", "Your own files.\n\nThis sample is read-only. Edits stay in this browser tab and are thrown away when you leave. Signing in with Google or GitHub opens the real editor: you can change scrolls and save them into the phext files on the server. There is no private format and no lock-in. A phext is plain text, and you can take the file with you.\n\nFollow 1.1.1/1.1.1/1.1.1 to return to the welcome scroll."),
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
        nav_view(&self.lattice, &nav, self.scrolls)
    }

    fn tree(&self) -> ApiTree {
        tree_view(&self.lattice)
    }

    fn status(&self) -> ApiStatus {
        ApiStatus {
            scrolls: self.scrolls,
            bytes: self.bytes,
            file: "demo.phext".into(),
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

fn serve_demo(stream: &mut std::net::TcpStream, method: &str, path: &str, demo: &DemoCatalog) {
    if method != "GET" && method != "HEAD" {
        send(stream, 405, "text/plain; charset=utf-8", b"demo is read-only");
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
            send(stream, 404, "text/plain; charset=utf-8", b"not found");
            return;
        }
    };
    if method == "HEAD" {
        send_head(stream, 200, "application/json", body.len());
    } else {
        send(stream, 200, "application/json", body.as_bytes());
    }
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
/// and a server with no auth configured, get the editor shell directly.
fn show_signed_out_gate(gate: &Gate, headers: &HashMap<String, String>) -> bool {
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

/// Status line and headers only. `no-store` keeps browsers from holding on to
/// an old copy of the page or a stale API response.
fn send_head(stream: &mut std::net::TcpStream, status: u16, content_type: &str, len: usize) {
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        status, status_text(status), content_type, len
    );
    let _ = stream.write_all(header.as_bytes());
}

fn send(stream: &mut std::net::TcpStream, status: u16, content_type: &str, body: &[u8]) {
    send_head(stream, status, content_type, body.len());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn json_response(stream: &mut std::net::TcpStream, value: &impl Serialize) {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    send(stream, 200, "application/json", body.as_bytes());
}

fn handle_request(
    stream: &mut std::net::TcpStream,
    state: &Arc<Mutex<AppState>>,
    gate: &Gate,
    demo: &DemoCatalog,
) {
    let cloned = match stream.try_clone() { Ok(s) => s, Err(_) => return };
    let mut reader = BufReader::new(cloned);
    let (method, path, headers) = match parse_request(&mut reader) { Some(v) => v, None => return };

    if method == "OPTIONS" {
        let h = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST\r\nAccess-Control-Allow-Headers: Content-Type\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(h.as_bytes());
        return;
    }

    if path == "/favicon.ico" {
        send(stream, 200, "image/svg+xml", FAVICON_SVG.as_bytes());
        return;
    }

    // Public pages: readable without signing in (OAuth app homepage + privacy policy)
    let public = match path.as_str() {
        "/about" => Some(ABOUT_HTML),
        "/privacy" => Some(PRIVACY_HTML),
        _ => None,
    };
    if let Some(page) = public {
        match method.as_str() {
            "GET" => send(stream, 200, "text/html; charset=utf-8", page.as_bytes()),
            "HEAD" => send_head(stream, 200, "text/html; charset=utf-8", page.len()),
            _ => send(stream, 405, "text/plain", b"method not allowed"),
        }
        return;
    }

    // Sign-in endpoints, then the public shell and the read-only demo.
    // Demo routes are answered here, before admit(), so a signed-out visitor
    // never receives a 401 and the real phext files are not in reach.
    if let Some(reply) = gate.route(&method, &path, &headers) {
        reply.write_to(stream);
        return;
    }
    let route = path.split('?').next().unwrap_or(path.as_str());
    if (method == "GET" || method == "HEAD") && (route == "/" || route == "/index.html") {
        let page = index_page(gate, &headers);
        if method == "HEAD" {
            send_head(stream, 200, "text/html; charset=utf-8", page.len());
        } else {
            send(stream, 200, "text/html; charset=utf-8", page.as_bytes());
        }
        return;
    }
    if route.starts_with("/api/demo") {
        serve_demo(stream, &method, &path, demo);
        return;
    }
    if let Err(reply) = gate.admit(&method, &path, &headers) {
        reply.write_to(stream);
        return;
    }

    let mut state = state.lock().unwrap();

    match (method.as_str(), path.as_str()) {
        // ── Phext file management ──
        ("GET", "/api/phexts") => {
            json_response(stream, &state.list_phexts());
        }
        ("POST", "/api/open") => {
            let body = read_body(&mut reader, &headers);
            let path_str = body.trim().to_string();
            match state.open_phext(&path_str) {
                Ok(()) => json_response(stream, &ApiOk { ok: true, message: format!("opened {}", state.file_path) }),
                Err(e) => json_response(stream, &ApiOk { ok: false, message: e }),
            }
        }

        ("GET", "/api/nav") => { json_response(stream, &state.nav_json()); }
        ("GET", "/api/status") => {
            let buf = state.lattice.to_phext_bytes();
            json_response(stream, &ApiStatus {
                scrolls: state.overview.total_scrolls, bytes: buf.len(),
                file: state.file_path.clone(), dirty: state.dirty,
            });
        }
        ("GET", "/api/tree") => { json_response(stream, &state.tree_json()); }

        // Navigation
        ("POST", "/api/dim") => {
            let body = read_body(&mut reader, &headers);
            if let Ok(d) = body.trim().parse::<u8>() {
                if (1..=9).contains(&d) { state.nav.select_dimension(d); }
            }
            json_response(stream, &state.nav_json());
        }
        ("POST", "/api/forward") => { state.nav.move_forward(); json_response(stream, &state.nav_json()); }
        ("POST", "/api/backward") => { state.nav.move_backward(); json_response(stream, &state.nav_json()); }
        ("POST", "/api/next") => {
            let idx = state.lattice.index().clone();
            state.nav.next_populated(&idx);
            json_response(stream, &state.nav_json());
        }
        ("POST", "/api/prev") => {
            let idx = state.lattice.index().clone();
            state.nav.prev_populated(&idx);
            json_response(stream, &state.nav_json());
        }
        ("POST", "/api/goto") => {
            let body = read_body(&mut reader, &headers);
            if let Some(coord) = parse_coordinate(body.trim()) { state.nav.goto(coord); }
            json_response(stream, &state.nav_json());
        }
        ("POST", "/api/home") => {
            if let Err(e) = state.go_home() { eprintln!("home: {}", e); }
            json_response(stream, &state.nav_json());
        }
        ("POST", "/api/base") => {
            state.nav.goto(libphext::phext::default_coordinate());
            json_response(stream, &state.nav_json());
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
                    json_response(stream, &ApiOk { ok: true, message: "scroll updated".into() });
                } else {
                    json_response(stream, &ApiOk { ok: false, message: "invalid coordinate".into() });
                }
            } else {
                json_response(stream, &ApiOk { ok: false, message: "invalid json".into() });
            }
        }
        ("POST", "/api/save") => {
            match state.lattice.save() {
                Ok(()) => { state.dirty = false; json_response(stream, &ApiOk { ok: true, message: "saved".into() }); }
                Err(e) => { json_response(stream, &ApiOk { ok: false, message: format!("save failed: {}", e) }); }
            }
        }

        // Search
        ("POST", "/api/search") => {
            let body = read_body(&mut reader, &headers);
            let query = body.trim();
            if query.is_empty() {
                json_response(stream, &Vec::<ApiSearchHit>::new());
            } else {
                let buf = state.lattice.to_phext_bytes();
                let hits = phext_lattice::search_lattice_auto(&buf, state.lattice.index(), query, false, 50);
                let api_hits: Vec<ApiSearchHit> = hits.into_iter().map(|h| ApiSearchHit {
                    coordinate: format!("{}", h.coordinate), context: h.context, offset: h.offset,
                }).collect();
                json_response(stream, &api_hits);
            }
        }

        // Zoom tiers
        ("GET", "/api/zoom/z") => { json_response(stream, &state.zoom_z()); }
        ("GET", p) if p.starts_with("/api/zoom/y/") => {
            json_response(stream, &state.zoom_y(&p["/api/zoom/y/".len()..]));
        }
        ("GET", p) if p.starts_with("/api/zoom/x/") => {
            let rest = &p["/api/zoom/x/".len()..];
            let parts: Vec<&str> = rest.splitn(2, '/').collect();
            if parts.len() == 2 { json_response(stream, &state.zoom_x(parts[0], parts[1])); }
            else { send(stream, 400, "text/plain", b"expected /api/zoom/x/z.z.z/y.y.y"); }
        }

        // Scroll read
        ("GET", p) if p.starts_with("/api/scroll/") => {
            let coord_str = &p["/api/scroll/".len()..];
            if let Some(coord) = parse_coordinate(coord_str) {
                let content = state.lattice.read_scroll(&coord).unwrap_or_default();
                let bytes = content.len();
                json_response(stream, &ApiScroll { coordinate: format!("{}", coord), content, bytes });
            } else {
                send(stream, 400, "text/plain", b"invalid coordinate");
            }
        }

        _ => { send(stream, 404, "text/plain", b"not found"); }
    }
}

fn parse_coordinate(s: &str) -> Option<libphext::phext::Coordinate> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 3 { return None; }
    let z: Vec<usize> = parts[0].split('.').filter_map(|n| n.parse().ok()).collect();
    let y: Vec<usize> = parts[1].split('.').filter_map(|n| n.parse().ok()).collect();
    let x: Vec<usize> = parts[2].split('.').filter_map(|n| n.parse().ok()).collect();
    if z.len() != 3 || y.len() != 3 || x.len() != 3 { return None; }
    Some(libphext::phext::Coordinate {
        z: libphext::phext::ZCoordinate { library: z[0], shelf: z[1], series: z[2] },
        y: libphext::phext::YCoordinate { collection: y[0], volume: y[1], book: y[2] },
        x: libphext::phext::XCoordinate { chapter: x[0], section: x[1], scroll: x[2] },
    })
}

fn get_flag(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("phext-edit — web-served phext editor");
        eprintln!();
        eprintln!("Usage: phext-edit <file.phext> [options]");
        eprintln!("       phext-edit --dir <directory> [options]");
        eprintln!();
        eprintln!("Options:");
        eprintln!("  --port <port>     HTTP port (default: 8080)");
        eprintln!("  --host <addr>     Listen address (default: 0.0.0.0; use 127.0.0.1 behind nginx)");
        eprintln!("  --dir <path>      Serve all .phext files in directory");
        eprintln!("  --token <token>   Require auth token for API access");
        eprintln!();
        eprintln!("GitHub/Google sign-in (environment):");
        eprintln!("  PHEXT_PUBLIC_URL, PHEXT_SESSION_SECRET, PHEXT_EDITORS, PHEXT_COOKIE_SECURE=1");
        eprintln!("  GITHUB_CLIENT_ID/SECRET, GOOGLE_CLIENT_ID/SECRET");
        std::process::exit(1);
    }

    let port: u16 = get_flag(&args, "--port").and_then(|p| p.parse().ok()).unwrap_or(8080);
    let host = get_flag(&args, "--host").unwrap_or_else(|| "0.0.0.0".to_string());
    let token = get_flag(&args, "--token");
    let dir = get_flag(&args, "--dir");
    let home_file = get_flag(&args, "--home")
        .or_else(|| std::env::var("PHEXT_HOME").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .map(|h| match (&dir, std::path::Path::new(&h).is_absolute()) {
            (Some(d), false) => std::path::Path::new(d).join(&h).to_string_lossy().to_string(),
            _ => h,
        })
        .filter(|h| {
            let ok = std::path::Path::new(h).is_file();
            if !ok { eprintln!("Home file {} not found; ignoring --home", h); }
            ok
        });
    let home_coord = get_flag(&args, "--home-coord")
        .or_else(|| std::env::var("PHEXT_HOME_COORD").ok())
        .and_then(|c| {
            let parsed = parse_coordinate(c.trim());
            if parsed.is_none() { eprintln!("Invalid --home-coord {}; ignoring", c); }
            parsed
        });

    // Determine initial file: the home file if set, else the first .phext by name
    let file_path = if let Some(ref h) = home_file {
        h.clone()
    } else if let Some(ref d) = dir {
        let mut found: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(d) {
            for entry in entries.flatten() {
                if entry.path().extension().map(|e| e == "phext").unwrap_or(false) {
                    found.push(entry.path().to_string_lossy().to_string());
                }
            }
        }
        found.sort();
        found.into_iter().next().unwrap_or_else(|| {
            eprintln!("No .phext files found in {}", d);
            std::process::exit(1);
        })
    } else {
        args[1].clone()
    };

    let lattice = MappedLattice::open(&file_path).unwrap_or_else(|e| {
        eprintln!("Failed to open {}: {}", file_path, e);
        std::process::exit(1);
    });

    let mut app_state = AppState::new(lattice, file_path.clone());
    app_state.phext_dir = dir.clone();
    app_state.home_file = home_file.clone();
    app_state.home_coord = home_coord;
    if let Err(e) = app_state.go_home() { eprintln!("home: {}", e); }

    let state = Arc::new(Mutex::new(app_state));
    let gate = Arc::new(Gate::from_env(token));
    let demo = Arc::new(DemoCatalog::load());
    let listener = TcpListener::bind(format!("{}:{}", host, port)).unwrap_or_else(|e| {
        eprintln!("Failed to bind {}:{}: {}", host, port, e);
        std::process::exit(1);
    });

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

const FAVICON_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><text y=".9em" font-size="90">💎</text></svg>"#;

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
</style>"# } }

const ABOUT_HTML: &str = concat!(r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="description" content="Phext Edit is a web editor for phext files: plain text arranged in 11 dimensions.">
<title>Phext Edit: about</title>
<link rel="icon" type="image/svg+xml" href="favicon.ico">
"#, public_style!(), r#"
</head>
<body>
<main>
<nav><a href="./">Open the editor</a> · <a href="privacy">Privacy Policy</a></nav>
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

<h2>Why it asks you to sign in</h2>
<p>The files on this server can be changed, so only approved editors may open them. You can
sign in with your Google or GitHub account. Sign-in is used for one thing: to check who you
are, so the server can tell whether your account is on its list of editors.</p>
<ul>
  <li><b>Google</b> sign-in asks for your basic profile and email address
    (<code>openid</code>, <code>email</code>, <code>profile</code>).</li>
  <li><b>GitHub</b> sign-in asks for read-only access to your public profile
    (<code>read:user</code>).</li>
</ul>
<p>Phext Edit does not read your Gmail, Drive, contacts, calendar or repositories. It does
not post anything for you, and it does not share or sell your information. The
<a href="privacy">Privacy Policy</a> gives the details.</p>

<footer>Phext Edit · <a href="privacy">Privacy Policy</a> ·
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a></footer>
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
<link rel="icon" type="image/svg+xml" href="favicon.ico">
"#, public_style!(), r#"
</head>
<body>
<main>
<nav><a href="about">About Phext Edit</a> · <a href="./">Open the editor</a></nav>
<h1>Privacy Policy</h1>
<p class="dim">Effective 30 October 2026</p>

<p>This policy covers Phext Edit, the web editor for phext files. It explains what Phext Edit
learns when you sign in, how that information is used, and what happens to it.</p>

<h2>What we receive</h2>
<p>When you sign in with <b>Google</b>, Phext Edit asks for the <code>openid</code>,
<code>email</code> and <code>profile</code> scopes. Google sends your account ID, your name, and
your email address with whether Google has verified it.</p>
<p>When you sign in with <b>GitHub</b>, Phext Edit asks for the <code>read:user</code> scope.
GitHub sends your account ID, username, display name, and your public email address if you
have one.</p>
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
  <li><b>Provider tokens.</b> The access token Google or GitHub returns is used once to read
    your profile, then thrown away. It is never saved, and Phext Edit never acts on your account.</li>
  <li><b>Your edits.</b> Text you save is written into the phext files on the server. Those
    files belong to the server's owner and are not tied to your account.</li>
  <li><b>Server logs.</b> The web server keeps standard access logs (IP address, time, page
    requested and browser) to keep the service running and secure. These logs do not include
    your account details.</li>
</ul>
<p>Phext Edit uses no third-party analytics, advertising or tracking scripts.</p>

<h2>Google user data</h2>
<p>Phext Edit's use and transfer of information received from Google APIs adheres to the
<a href="https://developers.google.com/terms/api-services-user-data-policy">Google API Services
User Data Policy</a>, including the Limited Use requirements.</p>

<h2>Your choices</h2>
<ul>
  <li>Sign out at any time from the status bar. This deletes the sign-in cookie.</li>
  <li>Remove Phext Edit's access from your
    <a href="https://myaccount.google.com/connections">Google account</a> or your
    <a href="https://github.com/settings/applications">GitHub settings</a>.</li>
  <li>Email questions or requests about your data to
    <a href="mailto:will+support@phext.io">will+support@phext.io</a>.</li>
</ul>

<h2>Changes</h2>
<p>If this policy changes, the new version will be posted on this page with a new effective date.</p>

<footer>Phext Edit · <a href="about">About</a> ·
Contact: <a href="mailto:will+support@phext.io">will+support@phext.io</a></footer>
</main>
</body>
</html>
"#);

// ── Embedded Frontend ──────────────────────────────────────────────────

const INDEX_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="description" content="phext-edit: browse and edit phext files, plain text arranged in 11 dimensions, from your browser.">
<title>💎 phext-edit</title>
<link rel="icon" type="image/svg+xml" href="favicon.ico">
<script>
(function () {
  try {
    if (localStorage.getItem('phext-token') && !new URLSearchParams(location.search).has('demo'))
      document.documentElement.classList.add('editor-ready');
  } catch (e) {}
})();
</script>
<style>
:root {
  --bg: #171717; --panel: #131313; --bar: #1c1c1c; --border: #2e2e2e;
  --text: #dedede; --dim: #808080; --coord: #d4a855;
  --z-color: #4dc9c9; --y-color: #c964c9; --x-color: #6bc96b;
  --active: #e8c547; --danger: #e05555;
  --trans-fast: 120ms ease; --trans-med: 200ms ease;
}
* { margin: 0; padding: 0; box-sizing: border-box; }
body {
  background: var(--bg); color: var(--text);
  font-family: 'JetBrains Mono', 'Fira Code', 'Cascadia Code', monospace;
  font-size: 13px; height: 100vh; display: flex; flex-direction: column; overflow: hidden;
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
  margin-left: auto; padding: 8px 16px; border: 1px solid var(--coord); border-radius: 6px;
  background: #2a2312; color: var(--coord); cursor: pointer; white-space: nowrap;
  transition: background var(--trans-fast), color var(--trans-fast);
}
#status-bar .signout:hover { background: var(--coord); color: #000; }

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
#search-results .hit {
  padding: 4px 8px; cursor: pointer; border-radius: 3px; display: flex; gap: 10px;
  transition: background var(--trans-fast);
}
#search-results .hit:hover { background: #ffffff10; }
#search-results .hit .hit-coord { color: var(--coord); min-width: 160px; }
#search-results .hit .hit-ctx { color: var(--dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.auth-btn { display:block; background:var(--panel); border:1px solid var(--border); color:var(--text);
  font-family:inherit; font-size:14px; padding:10px 14px; border-radius:6px; text-decoration:none; cursor:pointer; }
.auth-btn:hover { border-color:var(--coord); }
.auth-btn.primary { background:var(--coord); color:#1a1408; border-color:var(--coord); font-weight:600; margin-bottom:8px; }
html.editor-ready #auth-screen { display:none !important; }
.welcome { max-width:460px; margin:64px 16px; text-align:center; }
.welcome h1 { color:var(--coord); font-size:26px; font-weight:600; margin-bottom:10px; }
.welcome .lead { font-size:16px; line-height:1.4; margin-bottom:18px; }
.welcome .explain, .welcome .hint { color:var(--dim); font-size:13px; line-height:1.55; margin-top:16px; }
.welcome .links { margin-top:14px; font-size:12px; }
.welcome .links a { color:var(--coord); }
#auth-providers { display:flex; flex-direction:column; gap:8px; }
#token-box { margin-top:16px; color:var(--dim); font-size:12px; text-align:left; }
#token-box summary { cursor:pointer; text-align:center; }
#token-box p { margin-top:8px; line-height:1.5; }
#demo-banner {
  align-items:center; gap:14px; height:32px; padding:0 16px; flex-shrink:0;
  background:#241e10; color:var(--coord); font-size:12px; border-bottom:1px solid var(--border);
}
#demo-banner a { color:var(--coord); }
</style>
</head>
<body>

<div id="demo-banner" style="display:none">
  <span>Demo. Walk the 11 dimensions with j and k. Nothing is saved.</span>
  <span id="demo-signin">
    <a href="api/auth/google?next=%2F">Google</a>
    <a href="api/auth/github?next=%2F">GitHub</a>
  </span>
  <a id="demo-leave" href="/" style="display:none">Your files</a>
</div>

<div id="coord-bar">
  <span id="coord-text">loading...</span>
  <span class="scrolls" id="scroll-count"></span>
  <span class="dirty" id="dirty-flag"></span>
</div>

<div id="main">
  <div id="left-panel">
    <div id="panel-tabs">
      <div class="tab active" id="tab-sentron" onclick="showPanel('sentron')">◉ Sentron</div>
      <div class="tab" id="tab-navtree" onclick="showPanel('navtree')">🌳 Nav</div>
    </div>
    <div id="sentron-panel"></div>
    <div id="navtree-panel"></div>
  </div>
  <div id="content-area">
    <div id="content"></div>
    <div id="editor">
      <div class="editor-toolbar">
        <span class="coord-label" id="edit-coord"></span>
        <button class="save-btn" onclick="saveScroll()">Save (Ctrl+S)</button>
        <button onclick="cancelEdit()">Cancel (Esc)</button>
      </div>
      <textarea id="edit-textarea" spellcheck="false"></textarea>
    </div>
  </div>
</div>

<div id="status-bar">
  <span class="mode lattice" id="mode-label">LATTICE</span>
  <span id="active-file" style="color:var(--dim);cursor:pointer" onclick="openPhextPicker()" title="Click to switch phext"></span>
  <span class="help" id="help-text">1-9:dim  h/l:move  j/k:nav  J/K:×10  e:edit  t:tree  /:search  g:goto  Home:BASE</span>
  <span class="msg" id="status-msg"></span>
  <span id="signout" class="signout" style="display:none" onclick="signOut()" title="Sign out"></span>
</div>

<div class="overlay" id="search-overlay">
  <input id="search-input" placeholder="search scrolls..." autocomplete="off" />
  <div id="search-results"></div>
</div>
<div class="overlay" id="goto-overlay">
  <input id="goto-input" placeholder="z.z.z/y.y.y/x.x.x" autocomplete="off" />
</div>
<div class="overlay" id="phext-overlay" style="width:500px;max-height:500px">
  <div style="color:var(--dim);font-size:11px;margin-bottom:6px">Select a phext file:</div>
  <div id="phext-list" style="max-height:400px;overflow-y:auto"></div>
</div>
<div id="auth-screen" data-open="1" style="display:flex;position:fixed;inset:0;overflow:auto;background:var(--bg);z-index:100;justify-content:center">
  <div class="welcome">
    <h1>Phext Edit</h1>
    <p id="auth-prompt" class="lead">Write in 11 dimensions: plain text, no lock-in.</p>
    <a id="try-demo" class="auth-btn primary" href="/?demo">Try the demo</a>
    <div id="auth-providers">
      <a class="auth-btn" data-provider="google" href="api/auth/google?next=%2F">Sign in with Google</a>
      <a class="auth-btn" data-provider="github" href="api/auth/github?next=%2F">Sign in with GitHub</a>
    </div>
    <details id="token-box">
      <summary>use an access token</summary>
      <input id="auth-input" type="password" placeholder="token" autocomplete="off"
        style="width:100%;background:var(--panel);border:1px solid var(--border);color:var(--text);
        font-family:inherit;font-size:14px;padding:10px 14px;border-radius:6px;outline:none;text-align:center" />
      <p>For scripts and admin access. Press Enter to use it.</p>
    </details>
    <div id="auth-error" style="color:var(--danger);margin-top:8px;font-size:12px"></div>
    <p class="explain">Nine numbers name a scroll: library, shelf, series, collection, volume, book, chapter, section, scroll. The scroll is a page of lines and columns, which makes eleven. The file stays plain text.</p>
    <p class="hint">Sign in to save and edit your own files. The demo is a sample you can walk through; nothing in it is saved.</p>
    <p class="links"><a href="about">About</a> · <a href="privacy">Privacy Policy</a></p>
    <noscript>JavaScript opens the demo. Sign in with the Google or GitHub links above works without it.</noscript>
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
let authToken = localStorage.getItem('phext-token') || '';
let needsAuth = false;
let csrf = '';
let session = null;
let demoMode = false;
let demoAt = '1.1.1/1.1.1/1.1.1';
let demoDim = 1;
const demoEdits = new Map();
const wantDemo = new URLSearchParams(location.search).has('demo');
if (document.documentElement.classList.contains('editor-ready')) $('auth-screen').style.display = 'none';

function showMsg(msg) {
  $('status-msg').textContent = msg;
  clearTimeout(statusTimeout);
  statusTimeout = setTimeout(() => $('status-msg').textContent = '', 2000);
}

function setMode(m) {
  mode = m;
  const ml = $('mode-label');
  ml.textContent = m.toUpperCase();
  ml.className = 'mode ' + m;
  $('content').style.display = (m === 'lattice') ? '' : 'none';
  $('editor').style.display = (m === 'edit') ? 'flex' : 'none';
  const help = {
    lattice: '1-9:dim  h/l:move  j/k:nav  J/K:×10  e:edit  t:tree  /:search  g:goto  Home:BASE  Ctrl+S:save',
    edit: 'Ctrl+S:save  Esc:cancel',
    search: 'Enter:search  Esc:close',
    goto: 'Enter:go  Esc:close',
  };
  $('help-text').textContent = help[m] || '';
}

function showPanel(name) {
  activePanel = name;
  $('sentron-panel').style.display = (name === 'sentron') ? '' : 'none';
  $('navtree-panel').style.display = (name === 'navtree') ? '' : 'none';
  $('tab-sentron').className = 'tab' + (name === 'sentron' ? ' active' : '');
  $('tab-navtree').className = 'tab' + (name === 'navtree' ? ' active' : '');
  if (name === 'navtree' && !treeData) loadTree();
}

async function api(method, path, body) {
  if (demoMode) return demoApi(method, path, body);
  const opts = { method, headers: {} };
  if (authToken) opts.headers['Authorization'] = 'Bearer ' + authToken;
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

// ── Render ──
let suppressHashChange = false;

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

  if (!skipHash) {
    suppressHashChange = true;
    location.hash = p.coordinate;
    suppressHashChange = false;
  }

  renderSentron(nav);
  renderContent(nav);
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
    html += `<div class="axon spatial ${active}" onclick="selectDim(${a.dimension})">
      <span class="marker">${marker}</span><span class="dim-num">${a.dimension}</span>
      <span class="dim-name">${a.name}</span><span class="dim-val">${a.value}</span>
      <span class="counts">−${a.backward} +${a.forward}</span></div>`;
  }
  html += '<div class="group-label">── temporal ──</div>';
  for (const a of s.axons.filter(a => a.group === 'temporal')) {
    const active = a.dimension === p.dimension_index ? 'active' : '';
    const marker = a.dimension === p.dimension_index ? '▸' : ' ';
    html += `<div class="axon temporal ${active}" onclick="selectDim(${a.dimension})">
      <span class="marker">${marker}</span><span class="dim-num">${a.dimension}</span>
      <span class="dim-name">${a.name}</span><span class="dim-val">${a.value}</span>
      <span class="counts">−${a.backward} +${a.forward}</span></div>`;
  }
  for (const a of s.axons.filter(a => a.group === 'neuron')) {
    const active = a.dimension === p.dimension_index ? 'active' : '';
    const marker = a.dimension === p.dimension_index ? '▸' : ' ';
    html += `<div class="axon neuron ${active}" onclick="selectDim(${a.dimension})">
      <span class="marker">${marker}</span><span class="dim-num">${a.dimension}</span>
      <span class="dim-name">${a.name}</span><span class="dim-val">${a.value}</span>
      <span class="counts">← neuron</span></div>`;
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

    html += `<div class="nt-group${zCurrent}" onclick="toggleZ('${zk}')">
      <span class="nt-arrow${zExpanded ? ' open' : ''}">▶</span>
      <span class="nt-label">${zk}</span>
      <span class="nt-count">${zCount}</span></div>`;
    html += `<div class="nt-children${zExpanded ? '' : ' collapsed'}" id="nt-z-${zk.replace(/\./g,'-')}">`;

    if (zExpanded) {
      html += `<div class="subspace-header d7"><span class="sh-dim">7D</span><span class="sh-desc">Z=${zk} · ${yKeys.length} subspaces</span></div>`;
      for (const yk of yKeys) {
        const scrolls = yGroups[yk];
        const zyKey = zk + '/' + yk;
        const yExpanded = !!treeExpanded[zyKey];
        const yCurrent = curCoord.startsWith(zyKey + '/') ? ' current' : '';

        html += `<div class="nt-group${yCurrent}" onclick="toggleY('${escAttr(zk)}','${escAttr(yk)}')">
          <span class="nt-arrow${yExpanded ? ' open' : ''}">▶</span>
          <span class="nt-label" style="color:var(--y-color)">${yk}</span>
          <span class="nt-count">${scrolls.length}</span></div>`;
        html += `<div class="nt-children${yExpanded ? '' : ' collapsed'}" id="nt-y-${zyKey.replace(/[\/.]/g,'-')}">`;

        if (yExpanded) {
          html += `<div class="subspace-header d4"><span class="sh-dim">4D</span><span class="sh-desc">Y=${yk} · ${scrolls.length} scrolls</span></div>`;
          for (const s of scrolls) {
            const isCur = s.full === curCoord ? ' current' : '';
            html += `<div class="nt-scroll${isCur}" onclick="treeNav('${escAttr(s.full)}')">
              <span class="nt-coord">${s.x}</span></div>`;
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
  setMode('edit');
  $('edit-coord').textContent = lastNav.position.coordinate;
  $('edit-textarea').value = lastNav.scroll.content || '';
  $('edit-textarea').focus();
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
function cancelEdit() { setMode('lattice'); }

// ── Search ──
async function doSearch(query) {
  const hits = await api('POST', '/api/search', query);
  const el = $('search-results');
  if (!hits.length) { el.innerHTML = '<div style="color:var(--dim);padding:8px">no results</div>'; return; }
  el.innerHTML = hits.map(h =>
    `<div class="hit" onclick="jumpToHit('${escAttr(h.coordinate)}')">
      <span class="hit-coord">${escHtml(h.coordinate)}</span>
      <span class="hit-ctx">${escHtml(h.context)}</span></div>`
  ).join('');
}
function jumpToHit(coord) { closeOverlays(); gotoCoord(coord); }

// ── Overlays ──
function openSearch() {
  mode = 'search'; $('search-overlay').className = 'overlay visible';
  $('search-input').value = ''; $('search-results').innerHTML = ''; $('search-input').focus();
}
function openGoto() {
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

window.addEventListener('hashchange', async () => {
  if (suppressHashChange) return;
  if (!demoMode && $('auth-screen').style.display !== 'none') return;
  const hash = location.hash.replace(/^#/, '');
  if (hash && /^\d+\.\d+\.\d+\/\d+\.\d+\.\d+\/\d+\.\d+\.\d+$/.test(hash))
    render(await api('POST', '/api/goto', hash), true);
});

// ── Auth ──
// Sign-in state lives in an HttpOnly cookie; /api/session reports it and the CSRF token.
async function loadSession() {
  try { session = await (await fetch('api/session')).json(); } catch { session = null; }
  csrf = (session && session.csrf) || '';
  const so = $('signout');
  if (session && session.signed_in) {
    so.textContent = '⏏ sign out' + (session.handle ? ' ' + session.handle : '');
    so.title = 'Sign out';
    so.style.display = '';
  } else if (authToken) {
    so.textContent = '⏏ forget token';
    so.title = 'Forget the saved access token';
    so.style.display = '';
  } else {
    so.style.display = 'none';
  }
}
function paintProviders() {
  if (!session) return;
  const known = session.providers || [];
  document.querySelectorAll('#auth-providers [data-provider]').forEach(el => {
    el.style.display = known.indexOf(el.getAttribute('data-provider')) >= 0 ? '' : 'none';
  });
  const box = $('token-box');
  if (!box) return;
  // The token is for scripts and admin. Sign-in stays the path on the page.
  box.style.display = session.token ? '' : 'none';
  if (session.token && !session.ready) box.open = true;
}
async function showAuthScreen(message, sessionFresh) {
  if (!sessionFresh) await loadSession();
  const s = session || {};
  const tokenOk = !session || s.token;
  if (s.signed_in) {
    $('auth-providers').innerHTML =
      `<div style="color:var(--text)">Signed in as ${escHtml(s.name || s.handle)} <span style="color:var(--dim)">(${escHtml(s.who)})</span></div>` +
      `<button class="auth-btn" onclick="signOut()">Sign out</button>`;
  } else {
    paintProviders();
  }
  $('auth-prompt').textContent =
    s.signed_in && !s.editor ? 'This account is not on the editor list.'
    : s.ready ? 'Sign in to continue.'
    : 'Use an access token to continue.';
  const box = $('token-box');
  if (box) {
    box.style.display = tokenOk ? '' : 'none';
    if (!s.ready && tokenOk) box.open = true;
  }
  $('auth-error').textContent = message || '';
  $('auth-screen').style.display = 'flex';
}
async function signOut() {
  localStorage.removeItem('phext-token');
  if (session && session.signed_in) {
    try { await fetch('api/auth/logout', { method: 'POST', headers: { 'x-csrf-token': csrf } }); } catch {}
  }
  location.reload();
}
$('auth-input').addEventListener('keydown', async (e) => {
  if (e.key === 'Enter') {
    authToken = $('auth-input').value.trim();
    try {
      await api('GET', '/api/status');
      // Success — hide auth screen
      localStorage.setItem('phext-token', authToken);
      needsAuth = false;
      $('auth-screen').style.display = 'none';
      $('auth-error').textContent = '';
      if (!/^\d+\.\d+\.\d+\/\d+\.\d+\.\d+\/\d+\.\d+\.\d+$/.test(location.hash.replace(/^#/, '')))
        render(await api('POST', '/api/home'));
      initApp();
    } catch {
      $('auth-error').textContent = 'Invalid token';
      $('auth-input').value = '';
      $('auth-input').focus();
    }
  }
});

// ── Phext picker ──
async function openPhextPicker() {
  try {
    const files = await api('GET', '/api/phexts');
    let html = '';
    for (const f of files) {
      const kb = (f.bytes / 1024).toFixed(1);
      const isCurrent = lastNav && f.path === lastNav.position?.file;
      html += `<div class="hit" onclick="switchPhext('${escAttr(f.path)}')">
        <span class="hit-coord">${escHtml(f.name)}</span>
        <span class="hit-ctx">${kb} KB</span></div>`;
    }
    $('phext-list').innerHTML = html || '<div style="color:var(--dim);padding:8px">no phext files</div>';
    mode = 'phext-pick';
    $('phext-overlay').className = 'overlay visible';
  } catch(e) { showMsg('failed to list phexts'); }
}
async function switchPhext(path) {
  try {
    const res = await api('POST', '/api/open', path);
    if (res.ok) {
      closeOverlays();
      treeData = null; // force tree reload
      treeExpanded = {};
      const nav = await api('GET', '/api/nav');
      render(nav);
      if (activePanel === 'navtree') loadTree();
      showMsg('📂 ' + res.message);
    } else { showMsg(res.message); }
  } catch(e) { showMsg('failed to open'); }
}

async function initApp() {
  try {
    // Show active file name
    const status = await api('GET', '/api/status');
    $('active-file').textContent = '📂 ' + status.file.split('/').pop();
  } catch { /* auth will catch */ }
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
  if ((session && session.editor) || authToken) {
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
    const signedIn = !!authToken || (session && (session.editor || !session.locked));
    if (signedIn) { await bootEditor(); return; }
    if (session && session.signed_in && !session.editor) {
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
mod tests {
    use super::*;
    use phext_lattice::oauth::{Editors, Oauth};
    use std::io::{Read, Write};
    use std::time::Duration;

    fn locked_gate() -> Gate {
        Gate::new(Some("s3cret".into()), Oauth::test_ready(), Editors::parse("github:100"))
    }

    fn editor_headers() -> HashMap<String, String> {
        let cookie = format!("phext_session={}", Oauth::test_session_cookie());
        HashMap::from([("cookie".into(), cookie)])
    }

    #[test]
    fn signed_out_shell_leads_with_the_demo_and_tucks_the_token() {
        let page = index_page(&locked_gate(), &HashMap::new());
        assert!(page.contains(GATE_OPEN), "signed-out page should show the intro");
        assert!(page.contains("Phext Edit"));
        assert!(page.contains("Write in 11 dimensions: plain text, no lock-in."));
        assert!(page.contains("Try the demo"));
        assert!(page.contains("href=\"/?demo\""));
        assert!(page.contains("Sign in with Google"));
        assert!(page.contains("Sign in with GitHub"));
        assert!(page.contains("<noscript>"));
        assert!(!page.contains("Enter your access token to continue"));
        let details = page.find("<details id=\"token-box\">").expect("token disclosure");
        let input = page.find("id=\"auth-input\"").expect("token input");
        let close = page[details..].find("</details>").expect("disclosure end") + details;
        assert!(details < input && input < close, "token input stays inside the disclosure");
        assert!(page[details..close].contains("use an access token"));
    }

    #[test]
    fn a_signed_in_editor_does_not_see_the_intro() {
        let page = index_page(&locked_gate(), &editor_headers());
        assert!(page.contains(GATE_SHUT));
        assert!(!page.contains(GATE_OPEN));
    }

    #[test]
    fn an_open_server_skips_the_intro() {
        let gate = Gate::new(None, Oauth::closed(), Editors::default());
        let page = index_page(&gate, &HashMap::new());
        assert!(page.contains(GATE_SHUT));
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
            for mut stream in listener.incoming().take(9).flatten() {
                handle_request(&mut stream, &state, &gate, &demo);
            }
        });

        let (status, body) = exchange(port, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        assert_eq!(status, 200);
        assert!(body.contains("Try the demo"));
        assert!(body.contains("use an access token"));
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
